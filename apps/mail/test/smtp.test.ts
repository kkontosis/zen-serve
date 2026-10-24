// Inbound SMTP (apps/mail): Python's smtplib sends mail plain and over
// STARTTLS (TLS 1.2 and 1.3); a Node `net` client checks refusals,
// pipelining and the STARTTLS injection rule. The service has a sealed
// inbox (svc.md §9). Handlers run in-process and as Node workers, which
// are SIGKILLed between the end of DATA and the reply, and mid-DATA: every
// message is stored exactly once, with its exact bytes.
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { type Socket, connect as tcp } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { bytes, initWasm, zw } from '@zen/client';
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
  DB_OPTIONS,
  type Draft,
  decodeData,
  type Message,
  messageBytes,
  messages,
  NS,
  type Part,
  putCert,
  putInboxSeed,
  putMailbox,
  type Received,
  receivedTopic,
  SMTP_SERVICE,
  type SmtpOptions,
  serveSmtp,
  sweepDrafts,
} from '../src/mail.js';
import { GRANTS } from './grants.js';

const CLIENT = join(ROOT_DIR, 'apps/mail/test/client.py');

let w: World;
let alice: WorkerContext;
let db: Db;
let certs: TestCerts;
let dir: string;
const seed = new Uint8Array(32);
/** Every `mail/received` event, in order. */
const events: Received[] = [];

beforeAll(async () => {
  await initWasm();
  seed.set(zw.randomBytes(32));
  certs = testCerts();
  dir = mkdtempSync(join(tmpdir(), 'zen-mail-'));
  writeFileSync(join(dir, 'ca.pem'), certs.ca);
  w = await startWorld({
    grants: GRANTS,
    services: [
      { name: 'smtp', path: SMTP_SERVICE, more: { inbox: bytes.hex(zw.inboxPublic(seed)) } },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
  await putCert(db, certs.chain, certs.key);
  await putInboxSeed(db, seed);
  await putMailbox(db, 'alice@example.test', 'alice');
  await putMailbox(db, 'Bob@Example.test', 'bob');
  await putMailbox(db, 'postmaster@example.test', 'alice');
  await db.on<Received>(receivedTopic(db), (m) => void events.push(m.body), { start: 'earliest' });
}, 120_000);
afterAll(async () => {
  await alice?.close();
  await w?.stop();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
afterEach(() => {
  expect(errors).toEqual([]);
  errors.length = 0;
});

/** Serve in this process while `fn` runs. */
async function serving<T>(fn: () => Promise<T>, smtp: Partial<SmtpOptions> = {}): Promise<T> {
  const h: ServeHandle = await serveSmtp(db, {
    inbox: seed,
    onError,
    smtp: { host: 'mx.test', ...smtp },
  });
  try {
    return await fn();
  } finally {
    await h.stop();
  }
}

// ---------------------------------------------------------------- clients

interface Mail {
  from: string;
  to: string[];
  data: Uint8Array;
  mail_options?: string[];
}

interface Reply {
  cmd: string;
  code: number;
  msg: string;
  to?: string;
  version?: string;
  cipher?: string;
}

/** Run the Python client; its replies, one per line. */
async function py(
  mails: Mail[],
  opts: { tls?: '1.2' | '1.3'; timeout?: number } = {},
): Promise<Reply[]> {
  const file = join(dir, `m${Math.random().toString(36).slice(2)}.json`);
  writeFileSync(
    file,
    JSON.stringify(
      mails.map((m) => ({
        ...m,
        data: undefined,
        data_b64: Buffer.from(m.data).toString('base64'),
      })),
    ),
  );
  const args = ['-I', CLIENT, '--port', String(w.servicePort('smtp'))];
  if (opts.tls) args.push('--starttls', '--tls', opts.tls, '--ca', join(dir, 'ca.pem'));
  args.push('--timeout', String(opts.timeout ?? 30), file);
  const stdout = await new Promise<string>((ok, fail) =>
    execFile('python3', args, { maxBuffer: 1 << 24 }, (e, out, err) => {
      if (e && !out) fail(new Error(`${e.message}\n${err}`));
      else ok(out);
    }),
  );
  rmSync(file);
  return stdout
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l) as Reply);
}

let counter = 0;
/** A message with CRLF lines, a line that begins with a dot, and an 8-bit byte. */
function letter(
  subject = `test ${++counter}`,
  body = 'Hello.\r\n.hidden dot line\r\n',
): Uint8Array {
  const text =
    `From: sender@remote.test\r\nTo: alice@example.test\r\nSubject: ${subject}\r\n\r\n` +
    `${body}café\r\n`;
  return new TextEncoder().encode(text);
}

/** The stored copies of the message with this subject. */
async function copies(user: string, subject: string): Promise<Message[]> {
  const out: Message[] = [];
  for (const m of await messages(db, user)) {
    const b = new TextDecoder().decode(await messageBytes(db, user, m.uid));
    if (b.includes(`Subject: ${subject}\r\n`)) out.push(m);
  }
  return out;
}

/** The body as stored: the message less its `Received:` header. */
async function body(m: Message): Promise<Uint8Array> {
  return (await messageBytes(db, m.user, m.uid)).subarray(new TextEncoder().encode(m.head).length);
}

const partsOf = (draft: string) => db.table<Part>('parts').query().where('draft', '=', draft).all();

const subjectOf = (data: Uint8Array) =>
  /Subject: ([^\r]*)/.exec(new TextDecoder().decode(data))![1]!;

/** A raw SMTP client: send text, read whole replies. */
class Raw {
  buf = '';
  closed: Promise<void>;
  private constructor(readonly s: Socket) {
    s.on('data', (d: Buffer) => {
      this.buf += d.toString('latin1');
    });
    s.on('error', () => {});
    this.closed = new Promise((r) => s.once('close', () => r()));
  }

  static async open(): Promise<Raw> {
    const s = tcp(w.servicePort('smtp'), '127.0.0.1');
    await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
    const r = new Raw(s);
    expect(await r.replies(1)).toEqual([expect.stringMatching(/^220 mx\.test ESMTP/)]);
    return r;
  }

  /** The next `n` complete replies (a multi-line reply is one, its lines joined by \n). */
  async replies(n: number, ms = 10_000): Promise<string[]> {
    const out: string[] = [];
    await until(
      () => {
        for (;;) {
          const m = /^(?:\d{3}-[^\r\n]*\r\n)*\d{3}(?: [^\r\n]*)?\r\n/.exec(this.buf);
          if (!m) break;
          out.push(m[0].trimEnd().split('\r\n').join('\n'));
          this.buf = this.buf.slice(m[0].length);
        }
        return out.length >= n;
      },
      ms,
      `${n} replies (have ${JSON.stringify(out)}, buffered ${JSON.stringify(this.buf)})`,
    );
    return out;
  }

  /** Send lines and read one reply for each. */
  async cmd(...lines: string[]): Promise<string[]> {
    this.s.write(lines.map((l) => `${l}\r\n`).join(''));
    return this.replies(lines.length);
  }

  codes = async (...lines: string[]) =>
    (await this.cmd(...lines)).map((r) => Number(r.slice(0, 3)));
}

// ---------------------------------------------------------------- tests

describe('the DATA decoder', () => {
  const enc = new TextEncoder();
  const run = (chunks: string[]) => {
    let st = 0;
    let out = '';
    for (const c of chunks) {
      const r = decodeData(st, enc.encode(c));
      out += new TextDecoder().decode(r.out);
      st = r.state;
      if (r.end) return { out, used: r.used, end: true };
    }
    return { out, end: false };
  };

  it('dot-unstuffing and the end, in any split', () => {
    const wire = 'a\r\n..b\r\n.\r\nNEXT';
    for (let i = 0; i <= wire.length; i++) {
      for (let j = i; j <= wire.length; j++) {
        const r = run([wire.slice(0, i), wire.slice(i, j), wire.slice(j)]);
        expect(r.end).toBe(true);
        expect(r.out).toBe('a\r\n.b\r\n');
      }
    }
  });

  it('an empty message; bare LF never ends it (no smuggling)', () => {
    expect(run(['.\r\n'])).toMatchObject({ out: '', end: true, used: 3 });
    // A dot after a bare LF stays; one that starts a line is dropped, even before a bare LF.
    expect(run(['a\n.\nb\r\n.\n\r\nc'])).toMatchObject({ out: 'a\n.\nb\r\n\n\r\nc', end: false });
    expect(run(['a\r\n.\rb\r\n.\r\n'])).toMatchObject({ out: 'a\r\n\rb\r\n', end: true });
  });
});

describe('inbound SMTP in process (Python smtplib)', () => {
  it('plain, STARTTLS 1.3 and 1.2: each lands once per recipient, with its bytes and its event', async () => {
    await serving(async () => {
      for (const tls of [undefined, '1.3', '1.2'] as const) {
        const data = letter();
        const subject = subjectOf(data);
        const r = await py(
          [{ from: 'sender@remote.test', to: ['alice@example.test', 'bob@example.test'], data }],
          tls ? { tls } : {},
        );
        expect(r.find((x) => x.cmd === 'data')).toMatchObject({ code: 250 });
        expect(r.at(-1)).toMatchObject({ cmd: 'quit', code: 221 });
        const ehlo = r.filter((x) => x.cmd === 'ehlo');
        expect(ehlo[0]!.msg).toContain('SIZE 10485760');
        expect(ehlo[0]!.msg).toContain('STARTTLS');
        if (tls) {
          expect(r.find((x) => x.cmd === 'tls')!.version).toBe(`TLSv${tls}`);
          expect(ehlo[1]!.msg).not.toContain('STARTTLS');
        }
        for (const user of ['alice', 'bob']) {
          await until(async () => (await copies(user, subject)).length > 0, 10_000, 'the copy');
          const [m, ...more] = await copies(user, subject);
          expect(more).toEqual([]);
          expect(bytes.hex(await body(m!))).toBe(bytes.hex(data));
          expect(m!.from).toBe('sender@remote.test');
          expect(m!.head).toMatch(/^Received: from client\.test \(\[127\.0\.0\.1/);
          expect(m!.head).toContain(tls ? `with ESMTPS (TLSv${tls}:` : 'with ESMTP id');
          await until(
            () => events.some((e) => e.user === user && e.uid === m!.uid),
            10_000,
            'the event',
          );
          expect(events.filter((e) => e.user === user && e.uid === m!.uid)).toEqual([
            { user, uid: m!.uid, size: data.length },
          ]);
        }
      }
      // Recipients are matched without case; uids count up per user.
      const uids = (await messages(db, 'bob')).map((m) => m.uid);
      expect(uids).toEqual(Array.from({ length: uids.length }, (_, i) => i + 1));
      expect(await db.table<Draft>('drafts').query().all()).toEqual([]);
    });
  }, 60_000);
});

describe('refusals and the session rules', () => {
  it('order, unknown users, SIZE, RSET, unknown commands, and an over-long line', async () => {
    await serving(async () => {
      const c = await Raw.open();
      expect(await c.codes('MAIL FROM:<a@remote.test>')).toEqual([503]);
      const [ehlo] = await c.cmd('EHLO client.test');
      expect(ehlo).toContain('250-PIPELINING');
      expect(ehlo).toMatch(/250 STARTTLS$/);
      expect(await c.codes('RCPT TO:<alice@example.test>', 'DATA')).toEqual([503, 503]);
      expect(await c.codes('MAIL FROM:<a@remote.test> SIZE=10485761')).toEqual([552]);
      expect(await c.codes('MAIL FROM:<a@remote.test> SIZE=100 BODY=8BITMIME')).toEqual([250]);
      expect(await c.codes('MAIL FROM:<b@remote.test>')).toEqual([503]);
      expect(
        await c.codes('RCPT TO:<nobody@example.test>', 'RCPT TO:<ALICE@example.test>'),
      ).toEqual([550, 250]);
      expect(await c.codes('RSET', 'DATA', 'VRFY alice', 'NOOP', 'BDAT 10', 'FOO')).toEqual([
        250, 503, 252, 250, 502, 500,
      ]);
      // RSET kept the session: MAIL works without EHLO again.
      expect(await c.codes('MAIL FROM:<>', 'QUIT')).toEqual([250, 221]);
      await c.closed;

      const d = await Raw.open();
      d.s.write(`EHLO ${'x'.repeat(1100)}\r\n`);
      expect((await d.replies(1))[0]).toMatch(/^500 /);
      await d.closed;
    });
  });

  it('a body over the size limit gets 552, and leaves no parts', async () => {
    await serving(
      async () => {
        const r = await py([
          {
            from: 'a@remote.test',
            to: ['alice@example.test'],
            data: letter('big', 'x'.repeat(5000)),
          },
          { from: 'a@remote.test', to: ['alice@example.test'], data: letter('small') },
        ]);
        expect(r.filter((x) => x.cmd === 'data').map((x) => x.code)).toEqual([552, 250]);
        expect(await copies('alice', 'big')).toEqual([]);
        expect(await copies('alice', 'small')).toHaveLength(1);
        const drafts = new Set(
          (await db.table<Message>('messages').query().all()).map((m) => m.draft),
        );
        const parts = await db.table<Part>('parts').query().all();
        expect(parts.filter((p) => !drafts.has(p.draft))).toEqual([]);
        expect(await db.table<Draft>('drafts').query().all()).toEqual([]);
      },
      { maxSize: 4096 },
    );
  });
});

describe('pipelining and STARTTLS injection', () => {
  it('MAIL, RCPT and DATA in one write get three replies, in order', async () => {
    await serving(async () => {
      const c = await Raw.open();
      await c.cmd('EHLO client.test');
      const codes = await c.codes(
        'MAIL FROM:<p@remote.test>',
        'RCPT TO:<alice@example.test>',
        'DATA',
      );
      expect(codes).toEqual([250, 250, 354]);
      expect(await c.codes('Subject: pipelined\r\n\r\nhi\r\n.', 'QUIT')).toEqual([250, 221]);
      expect(await copies('alice', 'pipelined')).toHaveLength(1);
    });
  });

  it('plaintext after STARTTLS is never a command: NOOP is not answered, and the connection closes', async () => {
    const mine: unknown[] = [];
    const h = await serveSmtp(db, {
      inbox: seed,
      onError: (e) => mine.push(e),
      smtp: { host: 'mx.test' },
    });
    try {
      const c = await Raw.open();
      await c.cmd('EHLO client.test');
      c.s.write('STARTTLS\r\nNOOP\r\n');
      await c.closed;
      expect(c.buf.startsWith('220 2.0.0 Ready to start TLS\r\n')).toBe(true);
      expect(c.buf).not.toContain('250');
      await until(() => mine.length > 0, 5000, 'the TLS error');
      expect(mine.map((e) => (e as { code?: string }).code?.slice(0, 4))).toEqual(['tls_']);
    } finally {
      await h.stop();
    }
  });
});

describe('takeovers', () => {
  const started: WorkerProcess[] = [];
  const start = async (extra: Record<string, string> = {}) => {
    const k = spawnWorker(
      'mail',
      'smtp',
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

  for (const tls of [undefined, '1.3'] as const) {
    it(`killed between the end of DATA and the reply (${tls ? `TLS ${tls}` : 'plain, sealed'}): stored exactly once`, async () => {
      try {
        const k = await start({ MAIL_STALL: 'data-end' });
        const data = letter();
        const subject = subjectOf(data);
        const sent = py([{ from: 'k@remote.test', to: ['alice@example.test'], data }], {
          ...(tls ? { tls } : {}),
          timeout: 60,
        });
        await k.waitFor('stall');
        await k.kill();
        await start();
        const r = await sent;
        expect(r.find((x) => x.cmd === 'data')).toMatchObject({ code: 250 });
        const [m, ...more] = await copies('alice', subject);
        expect(more).toEqual([]);
        expect(bytes.hex(await body(m!))).toBe(bytes.hex(data));
        await until(
          () => events.some((e) => e.uid === m!.uid && e.user === 'alice'),
          10_000,
          'the event',
        );
        expect(events.filter((e) => e.uid === m!.uid && e.user === 'alice')).toHaveLength(1);
      } catch (e) {
        throw new Error(`${String(e)}\n${logs()}`);
      }
    }, 90_000);
  }

  it('killed mid-DATA (3 MB over STARTTLS 1.2): the successor continues; the SHA-256 matches', async () => {
    try {
      await start();
      const big = new Uint8Array(3 << 20);
      for (let i = 0; i < big.length; i++) big[i] = 97 + ((i * 7 + (i >> 11)) % 26);
      for (let i = 76; i < big.length; i += 78) {
        big[i] = 13;
        big[i + 1] = 10;
      }
      const head = new TextEncoder().encode('Subject: three megabytes\r\n\r\n');
      const data = new Uint8Array(head.length + big.length + 2);
      data.set(head);
      data.set(big, head.length);
      data.set([13, 10], head.length + big.length);
      const sent = py([{ from: 'k@remote.test', to: ['alice@example.test'], data }], {
        tls: '1.2',
        timeout: 120,
      });
      // Four full parts of the draft being received.
      await until(
        async () => {
          for (const d of await db.table<Draft>('drafts').query().all()) {
            if ((await partsOf(d.draft)).length >= 5) return true;
          }
          return false;
        },
        60_000,
        'some parts staged',
      );
      await started[0]!.kill();
      await start();
      const r = await sent;
      expect(r.find((x) => x.cmd === 'data')).toMatchObject({ code: 250 });
      const [m, ...more] = await copies('alice', 'three megabytes');
      expect(more).toEqual([]);
      const sha = (b: Uint8Array) => createHash('sha256').update(b).digest('hex');
      expect(sha(await body(m!))).toBe(sha(data));
      expect(await db.table<Draft>('drafts').query().all()).toEqual([]);
    } catch (e) {
      throw new Error(`${String(e)}\n${logs()}`);
    }
  }, 180_000);
});

describe('drafts', () => {
  /** Begin DATA on a raw connection and send a partial body; resolves once parts are staged. */
  async function midData(): Promise<{ c: Raw; draft: string }> {
    const c = await Raw.open();
    expect(
      await c.codes(
        'EHLO client.test',
        'MAIL FROM:<d@remote.test>',
        'RCPT TO:<alice@example.test>',
        'DATA',
      ),
    ).toEqual([250, 250, 250, 354]);
    c.s.write('Subject: never finished\r\n\r\npartial\r\n');
    let draft = '';
    await until(
      async () => {
        for (const d of await db.table<Draft>('drafts').query().all()) {
          const parts = await db.table<Part>('parts').query().where('draft', '=', d.draft).all();
          if (parts.length) draft = d.draft;
        }
        return draft !== '';
      },
      10_000,
      'a staged part',
    );
    return { c, draft };
  }

  it('a client that goes away mid-DATA: its draft is deleted at once', async () => {
    await serving(async () => {
      const { c, draft } = await midData();
      c.s.end();
      await c.closed;
      await until(
        async () => !(await db.table<Draft>('drafts').get(draft)),
        10_000,
        'the draft gone',
      );
      expect(await partsOf(draft)).toEqual([]);
    });
  });

  it('a connection reset mid-DATA leaves its draft; sweepDrafts removes it', async () => {
    await serving(async () => {
      const { c, draft } = await midData();
      c.s.resetAndDestroy();
      await c.closed;
      expect(await sweepDrafts(db, 60_000)).toBe(0);
      expect(await sweepDrafts(db, 0)).toBe(1);
      expect(await db.table<Draft>('drafts').get(draft)).toBeUndefined();
      expect(await partsOf(draft)).toEqual([]);
    });
  });
});
