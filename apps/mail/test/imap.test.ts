// IMAP (apps/mail) over implicit TLS: Python's imaplib for the commands,
// and a Node TLS client for IDLE, literals, a large FETCH and kills. Mail
// arrives through the SMTP handler, in-process. The IMAP handler runs
// in-process, and as Node workers that are SIGKILLed mid-FETCH and while
// a client idles: the successor continues the session.
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { connect as tcp } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { connect, type TLSSocket } from 'node:tls';
import type { Db } from '@zen/db';
import type { ServeHandle } from '@zen/svc';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { type TestCerts, testCerts } from '../../../packages/client/src/testing/index.js';
import {
  ROOT_DIR,
  spawnWorker,
  startWorld,
  until,
  type WorkerProcess,
  type World,
  workerEnv,
} from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  appPasswords,
  type Counter,
  createAppPassword,
  DB_OPTIONS,
  IMAP_SERVICE,
  type Message,
  messageBytes,
  messages,
  NS,
  type Part,
  putCert,
  putMailbox,
  revokeAppPassword,
  SMTP_SERVICE,
  serveImap,
  serveSmtp,
  wakeOnMail,
} from '../src/mail.js';
import { GRANTS } from './grants.js';

const CLIENT = join(ROOT_DIR, 'apps/mail/test/imapclient.py');

let w: World;
let alice: WorkerContext;
let db: Db;
let certs: TestCerts;
let dir: string;
let smtpH: ServeHandle;

beforeAll(async () => {
  certs = testCerts();
  dir = mkdtempSync(join(tmpdir(), 'zen-imap-'));
  writeFileSync(join(dir, 'ca.pem'), certs.ca);
  w = await startWorld({
    grants: GRANTS,
    services: [
      { name: 'smtp', path: SMTP_SERVICE },
      // RFC 2177: a client re-idles within 29 minutes.
      { name: 'imaps', path: IMAP_SERVICE, more: { idle_secs: 1860 } },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
  await putCert(db, certs.chain, certs.key);
  smtpH = await serveSmtp(db, { onError, smtp: { host: 'mx.test', starttls: false } });
}, 120_000);
afterAll(async () => {
  await smtpH?.stop();
  await alice?.close();
  await w?.stop();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
afterEach(() => {
  expect(errors.splice(0)).toEqual([]);
});

// ---------------------------------------------------------------- mail

let n = 0;
/** A mail user of its own (an address and an app password): each test's mailbox starts empty. */
async function account(): Promise<{ user: string; addr: string; password: string }> {
  const user = `u${++n}${Date.now() % 100_000}`;
  const addr = `${user}@example.test`;
  await putMailbox(db, addr, user);
  const { password } = await createAppPassword(db, user, 'test');
  return { user, addr, password };
}

/** Send one message through SMTP; resolves once it is stored. */
async function send(to: string, subject: string, body = 'Hello.\r\n', from = 'x@remote.test') {
  const data = `From: ${from}\r\nTo: ${to}\r\nSubject: ${subject}\r\n\r\n${body}`;
  const s = tcp(w.servicePort('smtp'), '127.0.0.1');
  let buf = '';
  s.on('data', (d: Buffer) => {
    buf += d.toString('latin1');
  });
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  try {
    await until(() => buf.includes('220 '), 10_000, 'the greeting');
    s.write(`EHLO t\r\nMAIL FROM:<${from}>\r\nRCPT TO:<${to}>\r\nDATA\r\n`);
    await until(() => buf.includes('354 '), 10_000, `354 (${buf})`);
    const wire = data.endsWith('\r\n') ? data : `${data}\r\n`;
    s.write(`${wire.replace(/^\./gm, '..')}.\r\n`);
    await until(() => /250 2\.0\.0 Ok: queued/.test(buf), 30_000, `the 250 (${buf})`);
    s.write('QUIT\r\n');
  } finally {
    s.end();
  }
}

// ---------------------------------------------------------------- clients

type Py = { call: string; typ?: string; data?: unknown[]; error?: string };

/** Run the Python client; its results, one per call. */
async function py(calls: unknown[][], timeout = 30): Promise<Py[]> {
  const file = join(dir, `c${Math.random().toString(36).slice(2)}.json`);
  writeFileSync(file, JSON.stringify(calls));
  const args = [
    '-I',
    CLIENT,
    '--port',
    String(w.servicePort('imaps')),
    '--ca',
    join(dir, 'ca.pem'),
  ];
  args.push('--timeout', String(timeout), file);
  const stdout = await new Promise<string>((ok, fail) =>
    execFile('python3', args, { maxBuffer: 1 << 26 }, (e, out, err) => {
      if (e && !out) fail(new Error(`${e.message}\n${err}`));
      else ok(out);
    }),
  );
  rmSync(file);
  return stdout
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l) as Py);
}

/** imaplib's data, bytes decoded as latin1 text. */
function txt(x: unknown): unknown {
  if (Array.isArray(x)) return x.map(txt);
  if (x && typeof x === 'object' && 'b' in x) {
    return Buffer.from((x as { b: string }).b, 'base64').toString('latin1');
  }
  return x;
}

const result = (r: Py[], call: string, i = 0) => r.filter((x) => x.call === call)[i]!;

/** A response: its text (a literal shows as `{n}`) and its literals. */
interface Resp {
  text: string;
  lits: Buffer[];
}

/** A Node TLS IMAP client: commands with tags, and every response as it comes. */
class Client {
  private buf = Buffer.alloc(0);
  readonly resps: Resp[] = [];
  private cur: Resp | undefined;
  private t = 0;
  closed = false;
  private constructor(readonly s: TLSSocket) {
    s.on('data', (d: Buffer) => {
      this.buf = Buffer.concat([this.buf, d]);
      this.parse();
    });
    s.on('error', () => {});
    s.on('close', () => {
      this.closed = true;
    });
  }

  static async open(): Promise<Client> {
    const s = connect({
      host: '127.0.0.1',
      port: w.servicePort('imaps'),
      ca: certs.ca,
      servername: 'localhost',
    });
    await new Promise<void>((ok, fail) => s.once('secureConnect', ok).once('error', fail));
    const c = new Client(s);
    await c.wait((r) => r.text.startsWith('* OK'), 'the greeting');
    return c;
  }

  private parse(): void {
    for (;;) {
      const lf = this.buf.indexOf('\r\n');
      if (lf < 0) return;
      const line = this.buf.subarray(0, lf).toString('latin1');
      const lit = /\{(\d+)\}$/.exec(line);
      if (lit) {
        const len = Number(lit[1]);
        if (this.buf.length < lf + 2 + len) return;
        this.cur ??= { text: '', lits: [] };
        this.cur.text += line;
        this.cur.lits.push(Buffer.from(this.buf.subarray(lf + 2, lf + 2 + len)));
        this.buf = this.buf.subarray(lf + 2 + len);
        continue;
      }
      const r = this.cur ?? { text: '', lits: [] };
      r.text += line;
      this.cur = undefined;
      this.resps.push(r);
      this.buf = this.buf.subarray(lf + 2);
    }
  }

  write(raw: string | Buffer): void {
    this.s.write(raw);
  }

  /** Wait for a response, from the `from`th on; returns its index. */
  async wait(pred: (r: Resp) => boolean, what: string, ms = 10_000, from = 0): Promise<number> {
    let at = -1;
    await until(
      () => {
        at = this.resps.findIndex((r, i) => i >= from && pred(r));
        return at >= 0;
      },
      ms,
      `${what} (have ${JSON.stringify(this.resps.slice(-8).map((r) => r.text))})`,
    );
    return at;
  }

  /** Send a command; its responses through the tagged one. */
  async cmd(text: string, ms = 10_000): Promise<Resp[]> {
    const tag = `t${++this.t}`;
    const from = this.resps.length;
    this.write(`${tag} ${text}\r\n`);
    const end = await this.wait((r) => r.text.startsWith(`${tag} `), `${tag} ${text}`, ms, from);
    return this.resps.slice(from, end + 1);
  }

  texts = async (text: string) => (await this.cmd(text)).map((r) => r.text);

  end(): void {
    this.s.destroy();
  }
}

/** Log in and select INBOX. */
async function opened(user: string, password: string): Promise<Client> {
  const c = await Client.open();
  expect((await c.texts(`LOGIN ${user} ${password}`)).at(-1)).toMatch(/^t1 OK/);
  expect((await c.texts('SELECT INBOX')).at(-1)).toMatch(/OK \[READ-WRITE\]/);
  return c;
}

const sha = (b: Uint8Array) => createHash('sha256').update(b).digest('hex');

// ---------------------------------------------------------------- tests

describe('IMAP in process', () => {
  let h: ServeHandle;
  let unwake: () => Promise<void>;
  beforeAll(async () => {
    h = await serveImap(db, { onError });
    unwake = await wakeOnMail(db, h);
  });
  afterAll(async () => {
    await unwake?.();
    await h?.stop();
  });

  it('LOGIN: an app password, by name or address; failures; the third is BYE; one mailbox per user', async () => {
    const a = await account();
    const b = await account();
    await send(a.addr, 'for a');
    await send(b.addr, 'for b');
    const r = await py([
      ['login', a.user, a.password],
      ['select', 'INBOX'],
      ['fetch', '1:*', '(BODY.PEEK[HEADER.FIELDS (SUBJECT)])'],
      ['logout'],
    ]);
    expect(r.map((x) => x.error)).toEqual([undefined, undefined, undefined, undefined, undefined]);
    expect(result(r, 'connect').data![0]).toMatch(/CAPABILITY IMAP4rev1 LITERAL\+ IDLE/);
    expect(result(r, 'select').data).toEqual([{ b: Buffer.from('1').toString('base64') }]);
    expect(JSON.stringify(txt(result(r, 'fetch').data))).toContain('Subject: for a');
    // By address, with the password's dashes left out.
    const byAddr = await py([
      ['login', a.addr.toUpperCase(), a.password.replace(/-/g, '')],
      ['select', 'INBOX'],
    ]);
    expect(result(byAddr, 'login').typ).toBe('OK');
    // b's password doesn't open a's mailbox; three failures end the session.
    const bad = await py([
      ['login', a.user, b.password],
      ['login', a.user, 'nope'],
      ['login', b.user, 'nope'],
      ['login', b.user, b.password],
    ]);
    expect(bad.slice(1, 3).map((x) => x.error)).toEqual([
      expect.stringContaining('AUTHENTICATIONFAILED'),
      expect.stringContaining('AUTHENTICATIONFAILED'),
    ]);
    expect(bad[3]!.error).toBeDefined();
    expect(bad[4]!.error).toBeDefined();
    const c = await Client.open();
    for (let i = 0; i < 2; i++) {
      expect((await c.texts(`LOGIN ${a.user} wrong`)).at(-1)).toMatch(
        /NO \[AUTHENTICATIONFAILED\]/,
      );
    }
    const third = await c.texts(`LOGIN ${a.user} wrong`);
    expect(third.at(-1)).toMatch(/NO \[AUTHENTICATIONFAILED\]/);
    await c.wait((x) => x.text.startsWith('* BYE'), 'BYE');
    await until(() => c.closed, 10_000, 'the close');
    const bb = await py([
      ['login', b.user, b.password],
      ['select', 'INBOX'],
      ['fetch', '1:*', '(BODY.PEEK[HEADER.FIELDS (SUBJECT)])'],
    ]);
    const text = JSON.stringify(txt(result(bb, 'fetch').data));
    expect(text).toContain('Subject: for b');
    expect(text).not.toContain('for a');
  }, 60_000);

  it('SELECT, FETCH items and sections; BODY[] sets \\Seen, EXAMINE and PEEK do not', async () => {
    const a = await account();
    await send(a.addr, 'one', 'Body one.\r\n');
    await send(a.addr, 'two', 'Body two.\r\n');
    const [m1] = await messages(db, a.user);
    const whole = Buffer.from(await messageBytes(db, a.user, 1)).toString('latin1');
    const c = await Client.open();
    await c.cmd(`LOGIN ${a.user} ${a.password}`);
    const sel = (await c.texts('SELECT INBOX')).join('\n');
    const ctr = await db.table<Counter>('counters').get(a.user);
    expect(sel).toContain('* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)');
    expect(sel).toContain('* OK [PERMANENTFLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)]');
    expect(sel).toContain('* 2 EXISTS');
    expect(sel).toContain('* 0 RECENT');
    expect(sel).toContain('* OK [UNSEEN 1]');
    expect(sel).toContain(`* OK [UIDVALIDITY ${ctr!.validity}]`);
    expect(sel).toContain('* OK [UIDNEXT 3]');
    const fast = await c.texts('FETCH 1:* (UID FLAGS RFC822.SIZE INTERNALDATE)');
    expect(fast[0]).toMatch(
      new RegExp(
        `^\\* 1 FETCH \\(UID 1 FLAGS \\(\\) RFC822\\.SIZE ${m1!.size} INTERNALDATE "\\d{2}-\\w{3}-\\d{4} \\d{2}:\\d{2}:\\d{2} \\+0000"\\)$`,
      ),
    );
    expect(fast).toHaveLength(3);
    const peek = await c.cmd('FETCH 1 (BODY.PEEK[])');
    expect(peek[0]!.lits[0]!.toString('latin1')).toBe(whole);
    const hdrEnd = whole.indexOf('\r\n\r\n') + 4;
    const parts = await c.cmd(
      'FETCH 1 (BODY.PEEK[HEADER] BODY.PEEK[TEXT] BODY.PEEK[HEADER.FIELDS (SUBJECT FROM)] BODY.PEEK[]<5.10>)',
    );
    const lits = parts[0]!.lits.map((l) => l.toString('latin1'));
    expect(lits).toEqual([
      whole.slice(0, hdrEnd),
      whole.slice(hdrEnd),
      'From: x@remote.test\r\nSubject: one\r\n\r\n',
      whole.slice(5, 15),
    ]);
    expect(parts[0]!.text).toContain('BODY[HEADER.FIELDS (SUBJECT FROM)] {');
    expect(parts[0]!.text).toContain('BODY[]<5> {10}');
    expect(
      (
        await c.cmd('FETCH 1 (BODY.PEEK[HEADER.FIELDS.NOT (RECEIVED SUBJECT)])')
      )[0]!.lits[0]!.toString(),
    ).toBe(`From: x@remote.test\r\nTo: ${a.addr}\r\n\r\n`);
    expect((await c.texts('FETCH 1 FLAGS'))[0]).toBe('* 1 FETCH (FLAGS ())');
    // EXAMINE: BODY[] marks nothing.
    await c.cmd('EXAMINE INBOX');
    await c.cmd('FETCH 1 BODY[]');
    expect((await c.texts('FETCH 1 FLAGS'))[0]).toBe('* 1 FETCH (FLAGS ())');
    expect(await c.texts('STORE 1 +FLAGS (\\Seen)')).toEqual([
      expect.stringMatching(/NO \[READ-ONLY\]/),
    ]);
    // SELECT: BODY[] sets it, and the response tells.
    await c.cmd('SELECT INBOX');
    const read = await c.cmd('FETCH 2 BODY[TEXT]');
    expect(read[0]!.text).toMatch(/^\* 2 FETCH \(BODY\[TEXT\] \{11\} FLAGS \(\\Seen\)\)$/);
    expect(read[0]!.lits[0]!.toString()).toBe('Body two.\r\n');
    expect((await db.table<Message>('messages').get([a.user, 2]))!.flags).toEqual(['\\Seen']);
    // Refused: what isn't supported.
    for (const bad of [
      'FETCH 1 ENVELOPE',
      'FETCH 1 BODYSTRUCTURE',
      'FETCH 1 ALL',
      'FETCH 9 FLAGS',
    ]) {
      expect((await c.texts(bad)).at(-1)).toMatch(/^t\d+ BAD/);
    }
    c.end();
  }, 60_000);

  it('STORE and UID STORE: +FLAGS, -FLAGS, FLAGS and .SILENT', async () => {
    const a = await account();
    for (const s of ['a', 'b', 'c']) await send(a.addr, s);
    const r = await py([
      ['login', a.user, a.password],
      ['select', 'INBOX'],
      ['store', '1:2', '+FLAGS', '(\\Flagged \\Seen)'],
      ['store', '2', '-FLAGS', '(\\Seen)'],
      ['store', '3', '+FLAGS.SILENT', '(\\Answered)'],
      ['uid', 'STORE', '3', 'FLAGS', '(\\Draft)'],
      ['store', '1', '+FLAGS', '(\\Recent)'],
      ['store', '1', '+FLAGS', '($Junk)'],
    ]);
    expect(txt(r[3]!.data)).toEqual([
      '1 (FLAGS (\\Seen \\Flagged))',
      '2 (FLAGS (\\Seen \\Flagged))',
    ]);
    expect(txt(r[4]!.data)).toEqual(['2 (FLAGS (\\Flagged))']);
    expect(r[5]!.data).toEqual([null]);
    expect(txt(r[6]!.data)).toEqual(['3 (FLAGS (\\Draft) UID 3)']);
    expect(r[7]!.error ?? r[7]!.typ).toMatch(/BAD/);
    expect(r[8]!.error ?? r[8]!.typ).toMatch(/BAD/);
    const flags = (await messages(db, a.user)).map((m) => m.flags);
    expect(flags).toEqual([['\\Seen', '\\Flagged'], ['\\Flagged'], ['\\Draft']]);
  }, 60_000);

  it('SEARCH and UID SEARCH', async () => {
    const a = await account();
    await send(a.addr, 'Quarterly report', 'x'.repeat(3000), 'boss@corp.test');
    await send(a.addr, 'lunch?', 'Hi.\r\n', 'friend@home.test');
    await send(a.addr, 'Report draft', 'Hi.\r\n', 'boss@corp.test');
    const today = new Date();
    const d = `${today.getUTCDate()}-${['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'][today.getUTCMonth()]}-${today.getUTCFullYear()}`;
    const r = await py([
      ['login', a.user, a.password],
      ['select', 'INBOX'],
      ['store', '2', '+FLAGS', '(\\Seen \\Flagged)'],
      ['search', null, 'UNSEEN'],
      ['search', null, 'FLAGGED'],
      ['search', null, 'SUBJECT', 'report'],
      ['search', null, 'FROM', 'BOSS'],
      ['search', null, 'SINCE', d],
      ['search', null, 'BEFORE', d],
      ['search', null, 'OR', 'FLAGGED', 'LARGER', '2000'],
      ['search', null, 'NOT', 'FROM', 'boss', 'ALL'],
      ['search', null, '(SEEN', 'FROM', 'friend)'],
      ['search', null, '2:*', 'UNSEEN'],
      ['uid', 'SEARCH', 'UID', '2:3', 'FROM', 'boss'],
      ['search', 'UTF-8', 'SUBJECT', 'lunch'],
      ['search', 'KOI8-R', 'ALL'],
      ['search', null, 'BODY', 'x'],
      ['search', null, 'NEW'],
    ]);
    const got = r
      .slice(4)
      .map((x) => (x.error ? x.error : x.typ === 'OK' ? String(txt(x.data)) : x.typ));
    expect(got).toEqual([
      '1 3',
      '2',
      '1 3',
      '1 3',
      '1 2 3',
      '',
      '1 2',
      '2',
      '2',
      '3',
      '3',
      '2',
      'NO',
      'NO',
      '',
    ]);
    expect(String(txt(r[16]!.data))).toContain('[BADCHARSET (UTF-8 US-ASCII)]');
  }, 60_000);

  it('EXPUNGE: responses in order; a message sent to two users keeps its parts until both copies go', async () => {
    const a = await account();
    const b = await account();
    for (const s of ['1', '2', '3', '4']) await send(a.addr, `m${s}`);
    // One message to both.
    await new Promise<void>((ok, fail) => {
      const s = tcp(w.servicePort('smtp'), '127.0.0.1');
      let buf = '';
      s.on('data', (d: Buffer) => {
        buf += d.toString();
        if (buf.includes('354 ')) {
          s.write(`Subject: both\r\n\r\n${'z'.repeat(150_000)}\r\n.\r\nQUIT\r\n`);
          buf = '';
        }
        if (buf.includes('221 ')) ok();
      });
      s.once('error', fail);
      s.write(
        `EHLO t\r\nMAIL FROM:<x@y>\r\nRCPT TO:<${a.addr}>\r\nRCPT TO:<${b.addr}>\r\nDATA\r\n`,
      );
    });
    const draft = (await messages(db, a.user)).at(-1)!.draft;
    const parts = () => db.table<Part>('parts').query().where('draft', '=', draft).all();
    expect((await parts()).length).toBe(3);
    const c = await opened(a.user, a.password);
    await c.cmd('STORE 2,4:5 +FLAGS.SILENT (\\Deleted)');
    const ex = await c.texts('EXPUNGE');
    expect(ex).toEqual(['* 5 EXPUNGE', '* 4 EXPUNGE', '* 2 EXPUNGE', expect.stringMatching(/OK/)]);
    expect((await messages(db, a.user)).map((m) => m.uid)).toEqual([1, 3]);
    expect((await parts()).length).toBe(3);
    expect(await c.texts('FETCH 1:* UID')).toEqual([
      '* 1 FETCH (UID 1)',
      '* 2 FETCH (UID 3)',
      expect.stringMatching(/OK/),
    ]);
    const cb = await opened(b.user, b.password);
    await cb.cmd('STORE 1 +FLAGS (\\Deleted)');
    // CLOSE expunges silently.
    expect(await cb.texts('CLOSE')).toEqual([expect.stringMatching(/^t\d+ OK/)]);
    expect(await messages(db, b.user)).toEqual([]);
    expect(await parts()).toEqual([]);
    c.end();
    cb.end();
  }, 60_000);

  it('IDLE: new mail, flags and expunges from elsewhere arrive; DONE ends it', async () => {
    const a = await account();
    await send(a.addr, 'first');
    await send(a.addr, 'second');
    const c = await opened(a.user, a.password);
    const other = await opened(a.user, a.password);
    const from = c.resps.length;
    c.write('i1 IDLE\r\n');
    await c.wait((r) => r.text === '+ idling', '+ idling', 10_000, from);
    let t = Date.now();
    await send(a.addr, 'third');
    await c.wait((r) => r.text === '* 3 EXISTS', 'EXISTS', 10_000, from);
    const times = [Date.now() - t];
    t = Date.now();
    await other.cmd('STORE 1 +FLAGS (\\Flagged)');
    await c.wait((r) => r.text === '* 1 FETCH (UID 1 FLAGS (\\Flagged))', 'FETCH', 10_000, from);
    times.push(Date.now() - t);
    t = Date.now();
    await other.cmd('STORE 2 +FLAGS.SILENT (\\Deleted)');
    await other.cmd('EXPUNGE');
    await c.wait((r) => r.text === '* 2 EXPUNGE', 'EXPUNGE', 10_000, from);
    times.push(Date.now() - t);
    // The deleted flag came too, before the expunge.
    expect(c.resps.slice(from).map((r) => r.text)).toEqual([
      '+ idling',
      '* 3 EXISTS',
      '* 1 FETCH (UID 1 FLAGS (\\Flagged))',
      ...(c.resps.slice(from).some((r) => r.text === '* 2 FETCH (UID 2 FLAGS (\\Deleted))')
        ? ['* 2 FETCH (UID 2 FLAGS (\\Deleted))']
        : []),
      '* 2 EXPUNGE',
    ]);
    c.write('DONE\r\n');
    await c.wait((r) => r.text === 'i1 OK IDLE terminated', 'DONE', 10_000, from);
    expect(await c.texts('FETCH 1:* UID')).toEqual([
      '* 1 FETCH (UID 1)',
      '* 2 FETCH (UID 3)',
      expect.stringMatching(/OK/),
    ]);
    // The other session's own changes aren't told back to it.
    expect((await other.texts('NOOP')).filter((x) => x.includes('FLAGS'))).toEqual([]);
    // Each arrived within 5 s (locally, well under one), with room for loaded runners.
    expect(Math.max(...times)).toBeLessThan(5000);
    c.end();
    other.end();
  }, 60_000);

  it('literals: synchronizing with a continuation, LITERAL+ without; a command over the limit', async () => {
    const a = await account();
    const c = await Client.open();
    const from = c.resps.length;
    c.write(`t1 LOGIN {${a.user.length}}\r\n`);
    await c.wait((r) => r.text.startsWith('+ '), 'a continuation', 10_000, from);
    c.write(`${a.user} {${a.password.length}+}\r\n${a.password}\r\n`);
    await c.wait((r) => r.text.startsWith('t1 OK'), 'logged in', 10_000, from);
    // Only one continuation was sent: LITERAL+ needs none.
    expect(c.resps.slice(from).filter((r) => r.text.startsWith('+')).length).toBe(1);
    // A synchronizing literal over the limit is refused before it is sent.
    expect(await c.texts('SELECT {70000}')).toEqual([
      expect.stringMatching(/^t\d+ BAD .*too long/),
    ]);
    expect((await c.texts('NOOP')).at(-1)).toMatch(/OK/);
    // A LITERAL+ one can't be: BYE.
    c.write(`t9 SELECT {70000+}\r\n${'x'.repeat(70_000)}\r\n`);
    await c.wait((r) => r.text.startsWith('* BYE'), 'BYE');
    await until(() => c.closed, 10_000, 'the close');
  }, 60_000);

  it('a 3 MB BODY[] crosses steps and arrives whole', async () => {
    const a = await account();
    const body = Array.from({ length: 40_000 }, (_, i) => `${i} ${'q'.repeat(70)}\r\n`).join('');
    await send(a.addr, 'big', body);
    const whole = await messageBytes(db, a.user, 1);
    expect(whole.length).toBeGreaterThan(3_000_000);
    const c = await opened(a.user, a.password);
    const r = await c.cmd('UID FETCH 1 (BODY.PEEK[])', 60_000);
    expect(r[0]!.text).toMatch(/^\* 1 FETCH \(UID 1 BODY\[\] \{\d+\}\)$/);
    expect(sha(r[0]!.lits[0]!)).toBe(sha(whole));
    c.end();
  }, 120_000);

  it('a revoked app password: the next command gets BYE', async () => {
    const a = await account();
    const c = await opened(a.user, a.password);
    const id = (await appPasswords(db, a.user))[0]!.id;
    expect(await revokeAppPassword(db, a.user, id)).toBe(true);
    c.write('t9 NOOP\r\n');
    await c.wait((r) => r.text === '* BYE The app password was revoked', 'BYE');
    await until(() => c.closed, 10_000, 'the close');
  }, 60_000);
});

describe('takeovers', () => {
  const started: WorkerProcess[] = [];
  const start = async (extra: Record<string, string> = {}) => {
    const k = spawnWorker(
      'mail',
      'imap',
      workerEnv(w, 'alice', { MAIL_LEASE_TTL_MS: '1000', ...extra }),
    );
    started.push(k);
    await k.waitFor('handler ready');
    return k;
  };
  const logs = () => started.map((k, i) => `worker ${i}:\n${k.output.join('\n')}`).join('\n');
  afterEach(async () => {
    for (const k of started) {
      expect(k.output.filter((l) => l.startsWith('error'))).toEqual([]);
      await k.kill();
    }
    started.length = 0;
  });

  it('killed mid-FETCH of 10 MB: the successor continues the literal; \\Seen is set once', async () => {
    try {
      const a = await account();
      const body = Array.from({ length: 130_000 }, (_, i) => `${i} ${'r'.repeat(70)}\r\n`).join('');
      await send(a.addr, 'ten', body);
      const whole = await messageBytes(db, a.user, 1);
      expect(whole.length).toBeGreaterThan(10_000_000);
      const before = (await db.table<Message>('messages').get([a.user, 1]))!.modseq!;
      const k = await start({ MAIL_STALL: 'fetch' });
      const c = await opened(a.user, a.password);
      const from = c.resps.length;
      c.write('f1 UID FETCH 1 (BODY[])\r\n');
      await k.waitFor('stall', 30_000);
      await k.kill();
      await start();
      await c.wait((r) => r.text.startsWith('f1 '), 'the FETCH', 120_000, from);
      const r = c.resps.slice(from);
      expect(r.map((x) => x.text)).toEqual([
        expect.stringMatching(/^\* 1 FETCH \(UID 1 BODY\[\] \{\d+\} FLAGS \(\\Seen\)\)$/),
        'f1 OK UID FETCH completed',
      ]);
      expect(sha(r[0]!.lits[0]!)).toBe(sha(whole));
      const m = (await db.table<Message>('messages').get([a.user, 1]))!;
      expect(m.flags).toEqual(['\\Seen']);
      expect(m.modseq).toBe(before + 1);
      c.end();
    } catch (e) {
      throw new Error(`${String(e)}\n${logs()}`);
    }
  }, 240_000);

  it('killed while a client idles: the successor tells it of new mail', async () => {
    try {
      const a = await account();
      await send(a.addr, 'one');
      const k = await start();
      const c = await opened(a.user, a.password);
      const from = c.resps.length;
      c.write('i1 IDLE\r\n');
      await c.wait((r) => r.text === '+ idling', '+ idling', 10_000, from);
      await send(a.addr, 'two');
      await c.wait((r) => r.text === '* 2 EXISTS', 'EXISTS before the kill', 10_000, from);
      await k.kill();
      await start();
      await send(a.addr, 'three');
      await c.wait((r) => r.text === '* 3 EXISTS', 'EXISTS after the takeover', 30_000, from);
      c.write('DONE\r\n');
      await c.wait((r) => r.text === 'i1 OK IDLE terminated', 'DONE', 10_000, from);
      expect(c.closed).toBe(false);
      c.end();
    } catch (e) {
      throw new Error(`${String(e)}\n${logs()}`);
    }
  }, 120_000);
});
