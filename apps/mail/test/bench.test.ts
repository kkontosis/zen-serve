// Measured costs of IMAP over a relayed service (docs/STATS.md §5.5): a
// NOOP's round trip, and a 10 MB FETCH, with the handler in this process.
// Opt-in, never in CI:
//
//   ZEN_BENCH=1 npx vitest run apps/mail/test/bench.test.ts
import { connect as tcp } from 'node:net';
import { connect, type TLSSocket } from 'node:tls';
import type { Db } from '@zen/db';
import type { ServeHandle } from '@zen/svc';
import { afterAll, beforeAll, describe, it } from 'vitest';
import { testCerts } from '../../../packages/client/src/testing/index.js';
import { startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  createAppPassword,
  DB_OPTIONS,
  IMAP_SERVICE,
  NS,
  putCert,
  putMailbox,
  SMTP_SERVICE,
  serveImap,
  serveSmtp,
} from '../src/mail.js';
import { GRANTS } from './grants.js';

const sorted = (xs: number[]) => [...xs].sort((a, b) => a - b);
const median = (xs: number[]) => sorted(xs)[Math.floor(xs.length / 2)]!;
const p90 = (xs: number[]) => sorted(xs)[Math.floor(xs.length * 0.9)]!;
const ms = (x: number) => (x >= 100 ? x.toFixed(0) : x.toFixed(1));

/** A TLS IMAP client that resolves each command on its tagged reply. */
class Client {
  private text = '';
  private bytes = 0;
  private t = 0;
  private wait: { tag: string; ok: (n: number) => void } | undefined;
  constructor(readonly s: TLSSocket) {
    s.on('data', (d: Buffer) => {
      this.bytes += d.length;
      // Keep only the tail: a FETCH's literal is megabytes.
      this.text = (this.text + d.toString('latin1')).slice(-4096);
      this.check();
    });
    s.on('error', () => {});
  }

  private check(): void {
    const w = this.wait;
    if (w && new RegExp(`(^|\\r\\n)${w.tag} (OK|NO|BAD)[^\\r]*\\r\\n$`).test(this.text)) {
      this.wait = undefined;
      w.ok(this.bytes);
    }
  }

  /** Send a command; resolves with the bytes received by its tagged reply. */
  cmd(text: string): Promise<number> {
    const tag = `t${++this.t}`;
    const done = new Promise<number>((ok) => {
      this.wait = { tag, ok };
    });
    this.s.write(`${tag} ${text}\r\n`);
    return done;
  }

  received(): number {
    return this.bytes;
  }
}

describe.skipIf(!process.env.ZEN_BENCH)('IMAP costs', () => {
  let w: World;
  let alice: WorkerContext;
  let db: Db;
  let ca: string;
  const handles: ServeHandle[] = [];
  const out: string[] = [];
  const onError = (e: unknown) => console.error('handler error', e);

  beforeAll(async () => {
    w = await startWorld({
      grants: GRANTS,
      services: [
        { name: 'smtp', path: SMTP_SERVICE },
        { name: 'imaps', path: IMAP_SERVICE, more: { idle_secs: 1860 } },
      ],
    });
    alice = await signInWorker({
      url: w.url,
      device: w.user('alice').deviceFile,
      passphrase: w.passphrase,
    });
    db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
    const c = testCerts();
    ca = c.ca;
    await putCert(db, c.chain, c.key);
    handles.push(await serveSmtp(db, { onError, smtp: { host: 'mx.test', starttls: false } }));
    handles.push(await serveImap(db, { onError }));
  }, 120_000);

  afterAll(async () => {
    console.log(['', ...out].join('\n'));
    for (const h of handles) await h.stop();
    await alice?.close();
    await w?.stop();
  });

  /** Send one message through SMTP; resolves once it is stored. */
  async function send(to: string, body: string): Promise<void> {
    const s = tcp(w.servicePort('smtp'), '127.0.0.1');
    let buf = '';
    s.on('data', (d: Buffer) => {
      buf = (buf + d.toString('latin1')).slice(-4096);
    });
    await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
    try {
      await until(() => buf.includes('220 '), 10_000, 'the greeting');
      s.write(`EHLO t\r\nMAIL FROM:<x@remote.test>\r\nRCPT TO:<${to}>\r\nDATA\r\n`);
      await until(() => buf.includes('354 '), 10_000, `354 (${buf})`);
      s.write(`From: x@remote.test\r\nTo: ${to}\r\nSubject: big\r\n\r\n${body}.\r\n`);
      await until(() => /250 2\.0\.0 Ok: queued/.test(buf), 300_000, `the 250 (${buf})`);
      s.write('QUIT\r\n');
    } finally {
      s.end();
    }
  }

  it('a NOOP, and a 10 MB FETCH', async () => {
    await putMailbox(db, 'bench@example.test', 'bench');
    const { password } = await createAppPassword(db, 'bench', 'bench');
    // About 10 MB of 72-byte lines.
    const body = Array.from({ length: 140_000 }, (_, i) => `${i} ${'q'.repeat(64)}\r\n`).join('');
    await send('bench@example.test', body);

    const s = connect({
      host: '127.0.0.1',
      port: w.servicePort('imaps'),
      ca,
      servername: 'localhost',
    });
    await new Promise<void>((ok, fail) => s.once('secureConnect', ok).once('error', fail));
    const c = new Client(s);
    await until(() => c.received() > 0, 10_000, 'the greeting');
    await c.cmd(`LOGIN bench ${password}`);
    await c.cmd('SELECT INBOX');

    const noop: number[] = [];
    for (let i = 0; i < 55; i++) {
      const t = performance.now();
      await c.cmd('NOOP');
      if (i >= 5) noop.push(performance.now() - t);
    }

    const rates: number[] = [];
    let size = 0;
    for (let i = 0; i < 3; i++) {
      const before = c.received();
      const t = performance.now();
      const after = await c.cmd('UID FETCH 1 (BODY.PEEK[])');
      size = after - before;
      rates.push(size / (1 << 20) / ((performance.now() - t) / 1000));
    }
    s.destroy();
    out.push(
      '',
      '| IMAP (TLS 1.3, handler in-process) | |',
      '|---|---|',
      `| NOOP round trip | ${ms(median(noop))} ms median, ${ms(p90(noop))} ms p90 |`,
      `| FETCH \`BODY.PEEK[]\` of ${(size / (1 << 20)).toFixed(1)} MB | ${median(rates).toFixed(1)} MB/s (median of 3) |`,
    );
  }, 600_000);
});
