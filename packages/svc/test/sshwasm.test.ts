// The SSH engine's WASM binding (@zen/svc-wasm) against OpenSSH's `ssh`: a
// Node TCP server drives the engine, saving and restoring it at every read,
// as handlers do. Needs OpenSSH (`ssh`, `ssh-keygen`).
import { createServer, type Server, type Socket } from 'node:net';
import type { SshConfig, SshConn, SshProgress } from '@zen/svc-wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { runSsh, type TestSshKeys, testSshKeys } from '../../client/src/testing/index.js';
import { loadSvcWasm, type SvcWasm } from '../src/wasm.js';

let sw: SvcWasm;
let keys: TestSshKeys;

beforeAll(async () => {
  sw = await loadSvcWasm();
  keys = testSshKeys();
});
afterAll(() => keys?.cleanup());

const enc = new TextEncoder();
const dec = new TextDecoder();
const now = () => Math.floor(Date.now() / 1000);

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

/** What the server saw. */
interface Stats {
  reads: number;
  kexes: number;
  user: string | null;
  asked: [string, boolean][];
  errors: NonNullable<SshProgress['error']>[];
  ciphers: string[] | undefined;
}

/**
 * A server of one connection at a time: `echo <text>`, `cat` (stdin to
 * stdout until EOF), `exit <n>` (with `bye` on stderr). The engine is
 * restored from its state at every read.
 */
async function server(
  config: SshConfig,
  authorized: string[],
): Promise<{ port: number; stats: Stats[]; close(): void }> {
  const stats: Stats[] = [];
  const allowed = authorized.map((l) => sw.sshPublicKey(l).fingerprint);
  const srv: Server = createServer((sock: Socket) => {
    const st: Stats = { reads: 0, kexes: 0, user: null, asked: [], errors: [], ciphers: undefined };
    stats.push(st);
    let state: Uint8Array | undefined;
    let pending: Uint8Array = new Uint8Array(0);
    let eof = false;
    /** Output not yet taken per channel (the window was full). */
    const backlog = new Map<number, Uint8Array>();
    const commands = new Map<number, string>();
    const step = () => {
      st.reads++;
      const c: SshConn = state
        ? sw.SshConn.restore(config, state)
        : sw.SshConn.start(config, now());
      try {
        const out: Uint8Array[] = [];
        const send = (b: Uint8Array) => {
          if (b.length) out.push(b);
        };
        for (let again = true; again; ) {
          again = false;
          const p = c.input(pending, eof, now());
          pending = pending.subarray(p.consumed);
          send(p.out);
          if (p.error) st.errors.push(p.error);
          if (c.ended) break;
          for (const e of p.events) {
            if (e.type === 'exec') commands.set(e.ch, dec.decode(e.command));
          }
          const q = c.pendingAuthorize();
          if (q) {
            st.asked.push([q.user, q.signed]);
            send(c.authorize(allowed.includes(q.key.fingerprint)));
            again = p.held;
          }
          for (const ch of c.channels()) {
            if (ch.closed) continue;
            const cmd = commands.get(ch.ch);
            if (cmd === undefined) continue;
            const write = (d: Uint8Array) => {
              const w = c.write(ch.ch, d);
              send(w.out);
              return w.taken;
            };
            const rest = backlog.get(ch.ch);
            if (rest) {
              const n = write(rest);
              if (n < rest.length) backlog.set(ch.ch, rest.subarray(n));
              else backlog.delete(ch.ch);
              if (n < rest.length) continue;
            }
            if (cmd === 'cat') {
              const d = c.channelData(ch.ch);
              if (d.length) {
                const n = write(d);
                send(c.consume(ch.ch, n));
                if (n > 0 && p.held) again = true;
              }
              if (ch.eof && c.channelData(ch.ch).length === 0) {
                send(c.exitStatus(ch.ch, 0));
                send(c.close(ch.ch));
                commands.delete(ch.ch);
              }
            } else if (cmd.startsWith('echo ')) {
              write(enc.encode(`${cmd.slice(5)}\n`));
              send(c.exitStatus(ch.ch, 0));
              send(c.close(ch.ch));
              commands.delete(ch.ch);
            } else if (cmd.startsWith('exit ')) {
              const w = c.writeStderr(ch.ch, enc.encode('bye\n'));
              send(w.out);
              send(c.exitStatus(ch.ch, Number(cmd.slice(5))));
              send(c.close(ch.ch));
              commands.delete(ch.ch);
            } else {
              send(c.exitStatus(ch.ch, 127));
              send(c.close(ch.ch));
              commands.delete(ch.ch);
            }
          }
        }
        st.user = c.user ?? null;
        st.kexes = c.kexes;
        st.ciphers = c.ciphers;
        for (const b of out) sock.write(b);
        if (c.ended) {
          sock.end();
          state = undefined;
        } else {
          state = c.state();
        }
      } finally {
        c.free();
      }
    };
    sock.on('data', (d: Buffer) => {
      pending = concat(pending, d);
      step();
    });
    sock.on('end', () => {
      eof = true;
      if (state) step();
    });
    sock.on('error', () => {});
    // The server speaks first.
    step();
  });
  await new Promise<void>((r) => srv.listen(0, '127.0.0.1', r));
  const addr = srv.address();
  if (!addr || typeof addr === 'string') throw new Error('no address');
  return { port: addr.port, stats, close: () => srv.close() };
}

describe('the SSH binding against OpenSSH', () => {
  it('runs a command, with each cipher; the state restores at every read', async () => {
    const config = new sw.SshConfig(keys.hostKey);
    const s = await server(config, [keys.userLine]);
    try {
      for (const cipher of ['chacha20-poly1305@openssh.com', 'aes256-gcm@openssh.com']) {
        const r = await runSsh([
          ...keys.sshArgs(s.port, { options: [`Ciphers=${cipher}`] }),
          'echo hello there',
        ]);
        expect(r.stderr).toBe('');
        expect(r.code).toBe(0);
        expect(r.stdout.toString()).toBe('hello there\n');
        const st = s.stats.at(-1)!;
        expect(st.user).toBe('test');
        expect(st.ciphers).toEqual([cipher, cipher]);
        expect(st.asked).toEqual([
          ['test', false],
          ['test', true],
        ]);
        expect(st.reads).toBeGreaterThan(4);
      }
      const r = await runSsh([...keys.sshArgs(s.port), 'exit 3']);
      expect(r.code).toBe(3);
      expect(r.stderr).toContain('bye');
    } finally {
      s.close();
      config.free();
    }
  });

  it('cats 1 MB with rekeys every 256 KiB', async () => {
    const config = new sw.SshConfig(keys.hostKey, { kex: ['curve25519-sha256'] });
    const s = await server(config, [keys.userLine]);
    try {
      const data = new Uint8Array(1 << 20);
      for (let i = 0; i < data.length; i++) data[i] = (i * 7 + (i >> 9)) & 0xff;
      const r = await runSsh(
        [...keys.sshArgs(s.port, { options: ['RekeyLimit=256K'] }), 'cat'],
        data,
      );
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(Buffer.compare(r.stdout, Buffer.from(data))).toBe(0);
      expect(s.stats.at(-1)!.kexes).toBeGreaterThanOrEqual(5);
      // OpenSSH ends with DISCONNECT, "by application".
      expect(s.stats.at(-1)!.errors.map((e) => [e.code, e.reason])).toEqual([['disconnected', 11]]);
    } finally {
      s.close();
      config.free();
    }
  }, 60_000);

  it('refuses a key the server does not know', async () => {
    const config = new sw.SshConfig(keys.hostKey);
    const s = await server(config, [keys.userLine]);
    try {
      const r = await runSsh([...keys.sshArgs(s.port, { keyFile: keys.otherKeyFile }), 'echo no']);
      expect(r.code).toBe(255);
      expect(r.stderr).toContain('Permission denied (publickey)');
      expect(s.stats.at(-1)!.user).toBe(null);
      expect(s.stats.at(-1)!.asked).toEqual([['test', false]]);
    } finally {
      s.close();
      config.free();
    }
  });
});

describe('the SSH binding', () => {
  const failing = (f: () => unknown): { name: string; message: string } => {
    try {
      f();
    } catch (e) {
      return e as Error;
    }
    throw new Error('it did not throw');
  };

  it('reads keys as ssh-keygen writes them', () => {
    expect(sw.sshHostKeyLine(keys.hostKey)).toBe(keys.hostLine);
    const config = new sw.SshConfig(keys.hostKey);
    expect(config.hostKeyLine).toBe(keys.hostLine);
    config.free();
    const k = sw.sshPublicKey(`${keys.userLine} a comment`);
    expect(k.algorithm).toBe('ssh-ed25519');
    expect(k.fingerprint).toMatch(/^SHA256:[A-Za-z0-9+/]{43}$/);
    expect(k.blob.length).toBe(51);
  });

  it('refuses with stable codes', () => {
    const e = failing(() => new sw.SshConfig('not a key'));
    expect(e.name).toBe('ZenSshError');
    expect(e.message).toBe('key');
    expect(failing(() => sw.sshPublicKey('ssh-dss AAAA')).message).toBe('key');
    expect(failing(() => new sw.SshConfig(keys.hostKey, { window: 10 })).message).toBe('config');
    expect(failing(() => new sw.SshConfig(keys.hostKey, { kex: ['dh'] })).message).toBe('config');
    const config = new sw.SshConfig(keys.hostKey, {
      ciphers: ['aes256-gcm@openssh.com'],
      software: 'test',
    });
    expect(failing(() => sw.SshConn.restore(config, new Uint8Array([1, 2]))).message).toBe('state');
    const c = sw.SshConn.start(config, now());
    // The greeting comes out of the first input.
    const p = c.input(new Uint8Array(0), false, now());
    expect(dec.decode(p.out.subarray(0, 14))).toBe('SSH-2.0-test\r\n');
    expect(failing(() => c.authorize(true)).message).toBe('not_asked');
    expect(failing(() => c.write(0, enc.encode('x'))).message).toBe('no_channel');
    expect(c.pendingAuthorize()).toBeUndefined();
    expect(c.channels()).toEqual([]);
    expect(c.inKex).toBe(true);
    // Not SSH: the engine disconnects.
    const bad = c.input(enc.encode('SSH-1.5-old\r\n'), false, now());
    expect(bad.error?.code).toBe('protocol');
    expect(bad.error?.reason).toBe(8);
    expect(c.ended).toBe(true);
    expect(c.input(new Uint8Array(0), false, now()).error?.code).toBe('closed');
    c.free();
    config.free();
  });
});
