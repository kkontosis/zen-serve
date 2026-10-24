// The echo service with a sealed inbox (spec/svc.md §9): the server seals
// each input chunk at rest, and handlers open them with the inbox seed,
// kept in a `certs` row. A `STARTTLS` line stops the sealing. Handlers
// in-process and as Node workers killed with SIGKILL.
import { type Socket, connect as tcp } from 'node:net';
import { bytes, initWasm, isCode, zw } from '@zen/client';
import type { Db } from '@zen/db';
import { serve } from '@zen/svc';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import {
  sleep,
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
  echo,
  INBOX_SERVICE,
  inboxSeed,
  lines,
  NS,
  putInboxSeed,
} from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;
let alice: WorkerContext;
let db: Db;
const seed = new Uint8Array(32);

beforeAll(async () => {
  await initWasm();
  seed.set(zw.randomBytes(32));
  w = await startWorld({
    grants: GRANTS,
    services: [
      {
        name: 'echo-inbox',
        path: INBOX_SERVICE,
        more: { inbox: bytes.hex(zw.inboxPublic(seed)) },
      },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
  await putInboxSeed(db, seed);
});
afterAll(async () => {
  await alice?.close();
  await w?.stop();
});

/** A peer of the service: what it received, and whether the server ended it. */
interface Peer {
  s: Socket;
  got(): string;
  /** Resolves when the server's FIN arrives. */
  ended: Promise<void>;
  /** Resolves when the socket closed (any reason). */
  closed: Promise<void>;
}

async function peer(): Promise<Peer> {
  const s = tcp(w.servicePort('echo-inbox'), '127.0.0.1');
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  let buf = '';
  s.on('data', (d) => {
    buf += d.toString();
  });
  s.on('error', () => {});
  return {
    s,
    got: () => buf,
    ended: new Promise<void>((ok) => s.once('end', ok)),
    closed: new Promise<void>((ok) => s.once('close', () => ok())),
  };
}

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
afterEach(() => {
  expect(errors).toEqual([]);
  errors.length = 0;
});

/** Count the sealed chunks this process opens while `fn` runs. */
async function counting<T>(fn: (opened: () => number) => Promise<T>): Promise<T> {
  const proto = zw.InboxKey.prototype;
  const open = proto.openChunk;
  let n = 0;
  proto.openChunk = function (this: zw.InboxKey, off: bigint, sealed: Uint8Array) {
    n++;
    return open.call(this, off, sealed);
  };
  try {
    return await fn(() => n);
  } finally {
    proto.openChunk = open;
  }
}

describe('echo with a sealed inbox', () => {
  it('the seed is kept in certs', async () => {
    expect(bytes.hex((await inboxSeed(db))!)).toBe(bytes.hex(seed));
  });

  it('sealed lines are echoed; after STARTTLS the input is stored plain; FIN gives bye', async () => {
    const h = serve(db, { service: INBOX_SERVICE, handler: echo, inbox: seed, onError });
    try {
      await counting(async (opened) => {
        const p = await peer();
        p.s.write('a\n');
        await until(() => p.got() === '1: a\n', 10_000, 'the first reply');
        expect(opened()).toBeGreaterThan(0);
        p.s.write('STARTTLS\n');
        await until(() => p.got() === '1: a\n2: STARTTLS\n', 10_000, 'the STARTTLS reply');
        // The step that sent this reply set `unseal`: what follows is plain.
        const sealed = opened();
        p.s.write('b\nc');
        await until(() => p.got().endsWith('3: b\n'), 10_000, 'the third reply');
        p.s.write('\n');
        await until(() => p.got().endsWith('4: c\n'), 10_000, 'the fourth reply');
        expect(opened()).toBe(sealed);
        p.s.end();
        await p.ended;
        expect(p.got()).toBe('1: a\n2: STARTTLS\n3: b\n4: c\nbye\n');
      });
      await until(() => h.conns === 0, 10_000, 'the connection to be forgotten');
    } finally {
      await h.stop();
    }
  });

  it('a worker SIGKILLed mid-stream: the next one opens the input too, every line once and in order', async () => {
    const N = 200;
    // Sealed until line 120, plain after it.
    const line = (i: number) => (i === 120 ? 'STARTTLS' : `l${i}`);
    const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_LAYER: 'inbox' });
    const started: WorkerProcess[] = [];
    const start = async () => {
      const k = spawnWorker('echo', 'handler', env);
      started.push(k);
      await k.waitFor('handler ready');
      return k;
    };
    const before = new Set((await lines(db)).map((r) => r.conn));
    try {
      let worker = await start();
      const p = await peer();
      const replies = () => p.got().split('\n').filter(Boolean).length;
      const sending = (async () => {
        for (let i = 1; i <= N; i++) {
          p.s.write(`${line(i)}\n`);
          await sleep(5);
        }
      })();
      await until(() => replies() >= 80, 60_000, '80 replies');
      await worker.kill();
      worker = await start();
      await sending;
      await until(() => replies() >= N, 60_000, `all ${N} replies`);
      const want = Array.from({ length: N }, (_, i) => `${i + 1}: ${line(i + 1)}\n`).join('');
      expect(p.got()).toBe(want);
      p.s.end();
      await p.ended;
      expect(p.got()).toBe(`${want}bye\n`);
      const mine = (await lines(db)).filter((r) => !before.has(r.conn)).sort((a, b) => a.n - b.n);
      expect(new Set(mine.map((r) => r.conn)).size).toBe(1);
      expect(mine.map((r) => [r.n, r.line])).toEqual(
        Array.from({ length: N }, (_, i) => [i + 1, line(i + 1)]),
      );
      for (const k of started) expect(k.output.filter((l) => l.startsWith('error'))).toEqual([]);
    } catch (e) {
      const logs = started.map((k, i) => `worker ${i}:\n${k.output.join('\n')}`).join('\n');
      throw new Error(`${String(e)}\n${logs}`);
    } finally {
      for (const k of started) await k.kill();
    }
  }, 120_000);

  it('a handler without the seed reports it and closes the connection', async () => {
    const mine: unknown[] = [];
    let steps = 0;
    const h = serve(db, {
      service: INBOX_SERVICE,
      handler: (c, tx) => {
        steps++;
        return echo(c, tx);
      },
      onError: (e) => mine.push(e),
    });
    try {
      const p = await peer();
      p.s.write('secret\n');
      await Promise.race([
        p.closed,
        sleep(15_000).then(() => {
          throw new Error('the connection stayed open');
        }),
      ]);
      expect(p.got()).toBe('');
      expect(steps).toBe(0);
      await until(() => mine.length > 0, 5_000, 'the error');
      expect(mine.some((e) => isCode(e, 'inbox'))).toBe(true);
    } finally {
      await h.stop();
    }
  });
});
