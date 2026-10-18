// The echo app over SSH (`ssh()`, svc.md §7.2): users log in with a key of
// `ssh_keys`, and each session channel runs one command:
//
// * `cat`: what arrives goes back, until EOF.
// * `lines` (and a shell): each line `l` gets `<n>: l` and a row of
//   `lines`, committed with the reply; `bye` at EOF.
// * `exit <n>`: `bye` on standard error, then exit status n.
// * `git-upload-pack '<path>'`: a stub repository's ref advertisement
//   (protocol v0), so `git ls-remote` works; real git comes later.
// * anything else: exit status 127.
//
// The engine gives a command once, in its event, so each channel's
// command and progress live in the application state.
import { bytes } from '@zen/client';
import type { Db, DbTransaction } from '@zen/db';
import { type Layer, type SshChannel, type SshStep, ssh } from '@zen/svc';
import { type Cert, SSH_HOST_KEY, type SshKey } from './schema.js';

/** The echo service over SSH. */
export const SSH_SERVICE = ['svc', 'echo-ssh'];

/** The stub repository's one commit, and what `git ls-remote` shows. */
export const STUB_COMMIT = '5d1e8c6bd2f4e7a6c3b0a9f8e7d6c5b4a3928170';
export const STUB_REFS = ['HEAD', 'refs/heads/main'];

/** A channel's progress, in the application state. */
interface Chan {
  /** `cat`, `lines`, `exit`, `git` or `none`. */
  kind: string;
  /** The exit status, once decided. */
  exit?: number;
  /** Output not taken yet (the client's window was full), hex. */
  out?: string;
  /** `lines`: the lines so far; `exit`: the status asked for; `git`: 1 once advertised. */
  n?: number;
}

type Chans = Record<string, Chan>;

const text = new TextDecoder();

/** A `ssh()` layer with the stored host key. */
export async function sshLayer(db: Db): Promise<Layer<SshStep>> {
  const c = await db.table<Cert>('certs').get(SSH_HOST_KEY);
  if (!c) throw new Error(`no SSH host key in certs (${SSH_HOST_KEY})`);
  return ssh({ hostKey: c.key });
}

/** A pkt-line (git's protocol framing). */
function pkt(s: string): string {
  return (s.length + 4).toString(16).padStart(4, '0') + s;
}

/** The stub's ref advertisement (protocol v0), then a flush. */
function advertisement(): string {
  const caps = 'symref=HEAD:refs/heads/main agent=zen-echo';
  const [head, ...rest] = STUB_REFS;
  return (
    pkt(`${STUB_COMMIT} ${head}\0${caps}\n`) +
    rest.map((r) => pkt(`${STUB_COMMIT} ${r}\n`)).join('') +
    '0000'
  );
}

/** A channel running `command`. */
function chan(command: string): Chan {
  if (command === 'cat' || command === 'lines') return { kind: command };
  const exit = /^exit (\d{1,3})$/.exec(command);
  if (exit) return { kind: 'exit', n: Number(exit[1]) & 255 };
  if (/^git-upload-pack '[^']*'$/.test(command)) return { kind: 'git' };
  return { kind: 'none' };
}

/** Send what a channel has pending; whether it all went. */
function flush(ch: SshChannel, st: Chan): boolean {
  if (!st.out) return true;
  const b = bytes.fromHex(st.out);
  const n = ch.write(b);
  st.out = n < b.length ? bytes.hex(b.subarray(n)) : undefined;
  return st.out === undefined;
}

/** Send `s`, keeping what the client's window doesn't take. */
function say(ch: SshChannel, st: Chan, s: string): void {
  const b = bytes.utf8(s);
  const pending = st.out ? bytes.fromHex(st.out) : new Uint8Array(0);
  const all = new Uint8Array(pending.length + b.length);
  all.set(pending);
  all.set(b, pending.length);
  st.out = bytes.hex(all);
  flush(ch, st);
}

/** The handler. */
export async function sshEcho(c: SshStep, tx: DbTransaction): Promise<void> {
  const q = c.question;
  if (q) {
    const k = await tx.table<SshKey>('ssh_keys').get([q.user, q.key.fingerprint]);
    c.answer(k !== undefined);
  }
  const chans: Chans = c.state ? JSON.parse(text.decode(c.state)) : {};
  for (const e of c.events) {
    if (e.type === 'exec') chans[e.ch] = chan(e.command);
    else if (e.type === 'shell') chans[e.ch] = { kind: 'lines' };
    else if (e.type === 'subsystem') chans[e.ch] = { kind: 'none' };
  }
  for (const ch of c.channels) {
    const st = chans[ch.id];
    if (!st) continue;
    if (ch.closed) {
      delete chans[ch.id];
      continue;
    }
    if (st.exit === undefined && flush(ch, st)) await run(c, tx, ch, st);
    if (st.exit !== undefined && flush(ch, st)) {
      ch.exit(st.exit);
      ch.close();
      delete chans[ch.id];
    }
  }
  c.state = Object.keys(chans).length ? bytes.utf8(JSON.stringify(chans)) : null;
}

async function run(c: SshStep, tx: DbTransaction, ch: SshChannel, st: Chan): Promise<void> {
  switch (st.kind) {
    case 'cat': {
      // The unacked input is the backlog: the client's window holds it.
      const n = ch.write(ch.data);
      ch.ack(n);
      if (ch.eof && ch.data.length === 0) st.exit = 0;
      return;
    }
    case 'lines': {
      const data = ch.data;
      let at = 0;
      for (let nl = data.indexOf(10); nl >= 0 && !st.out; nl = data.indexOf(10, at)) {
        st.n = (st.n ?? 0) + 1;
        const l = text.decode(data.subarray(at, nl));
        say(ch, st, `${st.n}: ${l}\n`);
        await tx.table('lines').insert({ conn: `${bytes.hex(c.conn)}/${ch.id}`, n: st.n, line: l });
        at = nl + 1;
      }
      ch.ack(at);
      if (ch.eof && at === data.length && !st.out) {
        say(ch, st, 'bye\n');
        st.exit = 0;
      }
      return;
    }
    case 'exit':
      ch.stderr('bye\n');
      st.exit = st.n ?? 0;
      return;
    case 'git': {
      // Advertise once, then wait for the client's flush (no wants).
      if (st.n === undefined) {
        st.n = 1;
        say(ch, st, advertisement());
      }
      const data = ch.data;
      ch.ack(data.length);
      if (text.decode(data).includes('0000') || ch.eof) st.exit = 0;
      return;
    }
    default:
      ch.stderr('unknown command\n');
      st.exit = 127;
  }
}
