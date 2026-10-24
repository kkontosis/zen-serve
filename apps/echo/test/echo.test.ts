// The echo service (a relayed line service, spec/svc.md) with Node `net`
// peers: handlers in-process and as Node workers killed with SIGKILL.
import { type Socket, connect as tcp } from 'node:net';
import { bytes } from '@zen/client';
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
import { DB_OPTIONS, echo, lines, NS, serveEcho } from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;
let alice: WorkerContext;
let db: Db;

beforeAll(async () => {
  w = await startWorld({
    grants: GRANTS,
    services: [
      { name: 'echo', path: ['svc', 'echo'] },
      { name: 'plain', path: ['svc', 'plain'], resume: false },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
});
afterAll(async () => {
  await alice?.close();
  await w?.stop();
});

/** A peer of a service: what it received, and whether the server ended it. */
interface Peer {
  s: Socket;
  got(): string;
  /** Resolves when the server's FIN arrives. */
  ended: Promise<void>;
  /** Resolves when the socket closed (any reason). */
  closed: Promise<void>;
}

async function peer(service: string): Promise<Peer> {
  const s = tcp(w.servicePort(service), '127.0.0.1');
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
// The in-process handlers report nothing.
afterEach(() => {
  expect(errors).toEqual([]);
});

describe('echo', () => {
  it('echoes lines in-process; a partial line waits; the rows match; FIN gives bye', async () => {
    const h = serveEcho(db, { onError });
    try {
      const p = await peer('echo');
      p.s.write('a\nb');
      await until(() => p.got() === '1: a\n', 10_000, 'the first reply');
      p.s.write('c\n');
      await until(() => p.got() === '1: a\n2: bc\n', 10_000, 'the second reply');
      p.s.end();
      await p.ended;
      expect(p.got()).toBe('1: a\n2: bc\nbye\n');
      const rows = (await lines(db))
        .filter((r) => r.line === 'a' || r.line === 'bc')
        .sort((a, b) => a.n - b.n);
      expect(rows.map((r) => [r.n, r.line])).toEqual([
        [1, 'a'],
        [2, 'bc'],
      ]);
      await until(() => h.conns === 0, 10_000, 'the connection to be forgotten');
    } finally {
      await h.stop();
    }
  });

  it('a worker SIGKILLed mid-stream: the next one continues, every line once and in order', async () => {
    await takeovers(1);
  });

  it('three kills at random points: every reply has its row and every row its reply', async () => {
    await takeovers(3);
  });

  it('two handlers share the connections', async () => {
    const bob = await signInWorker({
      url: w.url,
      device: w.user('bob').deviceFile,
      passphrase: w.passphrase,
    });
    const bobDb = await bob.open(NS, { ...DB_OPTIONS, warn: () => {} });
    const served = { alice: new Set<string>(), bob: new Set<string>() };
    const as =
      (who: 'alice' | 'bob') => (c: Parameters<typeof echo>[0], tx: Parameters<typeof echo>[1]) => {
        served[who].add(bytes.hex(c.conn));
        return echo(c, tx);
      };
    const ha = serve(db, { service: ['svc', 'echo'], handler: as('alice'), maxConns: 10, onError });
    const hb = serve(bobDb, {
      service: ['svc', 'echo'],
      handler: as('bob'),
      maxConns: 10,
      onError,
    });
    try {
      const peers = await Promise.all(Array.from({ length: 20 }, () => peer('echo')));
      peers.forEach((p, i) => {
        p.s.write(`hello ${i}\n`);
      });
      await until(
        () => peers.every((p, i) => p.got() === `1: hello ${i}\n`),
        30_000,
        'every peer echoed',
      );
      expect(served.alice.size).toBeGreaterThan(0);
      expect(served.bob.size).toBeGreaterThan(0);
      expect(served.alice.size + served.bob.size).toBe(20);
      for (const p of peers) p.s.destroy();
    } finally {
      await ha.stop();
      await hb.stop();
      await bob.close();
    }
  });

  it("resume: false: the echo works, and once the killed handler's lease lapses the connection closes", async () => {
    const k = spawnWorker(
      'echo',
      'handler',
      workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_SERVICE: 'plain' }),
    );
    let other: ReturnType<typeof serve> | undefined;
    try {
      await k.waitFor('handler ready');
      const p = await peer('plain');
      p.s.write('one\n');
      await until(() => p.got() === '1: one\n', 15_000, 'the reply');
      await k.kill();
      // Another handler's lease calls reap the lapsed lease: the
      // connection can't move to it, so it is aborted.
      let steps = 0;
      other = serve(db, {
        service: ['svc', 'plain'],
        handler: (c, tx) => {
          steps++;
          return echo(c, tx);
        },
        onError,
      });
      await Promise.race([
        p.closed,
        sleep(15_000).then(() => {
          throw new Error('the connection stayed open');
        }),
      ]);
      expect(p.got()).toBe('1: one\n');
      expect(steps).toBe(0);
    } finally {
      await other?.stop();
      await k.kill();
    }
  });
});

/**
 * A peer sends 200 lines, 5 ms apart, to handlers run as Node workers; the
 * worker serving it is SIGKILLed `kills` times at random points and a new
 * one started. The transcript holds each reply once and in order, and the
 * table exactly the connection's 200 lines.
 */
async function takeovers(kills: number): Promise<void> {
  const N = 200;
  const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000' });
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
    const p = await peer('echo');
    const replies = () => p.got().split('\n').filter(Boolean).length;
    const at = Array.from({ length: kills }, () => 20 + Math.floor(Math.random() * (N - 60))).sort(
      (a, b) => a - b,
    );
    const sending = (async () => {
      for (let i = 1; i <= N; i++) {
        p.s.write(`l${i}\n`);
        await sleep(5);
      }
    })();
    for (const n of at) {
      await until(() => replies() >= n, 60_000, `${n} replies`);
      await worker.kill();
      worker = await start();
    }
    await sending;
    await until(() => replies() >= N, 60_000, `all ${N} replies`);
    const want = Array.from({ length: N }, (_, i) => `${i + 1}: l${i + 1}\n`).join('');
    expect(p.got()).toBe(want);
    p.s.end();
    await p.ended;
    expect(p.got()).toBe(`${want}bye\n`);
    const mine = (await lines(db)).filter((r) => !before.has(r.conn)).sort((a, b) => a.n - b.n);
    expect(new Set(mine.map((r) => r.conn)).size).toBe(1);
    expect(mine.map((r) => [r.n, r.line])).toEqual(
      Array.from({ length: N }, (_, i) => [i + 1, `l${i + 1}`]),
    );
  } finally {
    for (const k of started) await k.kill();
  }
}
