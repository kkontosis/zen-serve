// The echo service over TLS (`tls()`, svc.md §7.1) with Node `tls` peers:
// handlers in-process and as Node workers killed with SIGKILL, mid-stream
// (the connection continues) and mid-handshake (the server aborts it).
import { execFileSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { createServer, type Server, type Socket, connect as tcp } from 'node:net';
import { type ConnectionOptions, connect, type TLSSocket } from 'node:tls';
import type { Db } from '@zen/db';
import { type RawStep, serve, tls } from '@zen/svc';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { testCerts } from '../../../packages/client/src/testing/index.js';
import {
  ROOT_DIR,
  sleep,
  spawnWorker,
  startWorld,
  until,
  type WorkerProcess,
  type World,
  workerEnv,
} from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import { DB_OPTIONS, echo, lines, NS, putCert, TLS_SERVICE, tlsLayer } from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;
let alice: WorkerContext;
let db: Db;
let ca: string;

beforeAll(async () => {
  execFileSync(process.execPath, ['scripts/build-apps.mjs', 'echo'], {
    cwd: ROOT_DIR,
    stdio: 'inherit',
  });
  w = await startWorld({
    grants: GRANTS,
    services: [
      { name: 'echo-tls', path: ['svc', 'echo-tls'] },
      { name: 'mirror-tls', path: ['svc', 'mirror-tls'] },
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
  await putCert(db, { name: 'echo', chain: c.chain, key: c.key });
});
afterAll(async () => {
  await alice?.close();
  await w?.stop();
});

/** A TLS peer of a service: what it received, and how it ended. */
interface Peer {
  s: TLSSocket;
  got(): Buffer;
  text(): string;
  /** Resolves when the server's close_notify or FIN arrives. */
  ended: Promise<void>;
  /** Resolves when the socket closed (any reason). */
  closed: Promise<void>;
  errors: Error[];
}

async function peer(port: number, opts: ConnectionOptions = {}): Promise<Peer> {
  const s = connect({ host: '127.0.0.1', port, ca, servername: 'localhost', ...opts });
  const chunks: Buffer[] = [];
  const errors: Error[] = [];
  s.on('data', (d: Buffer) => chunks.push(d));
  s.on('error', (e) => errors.push(e));
  const ended = new Promise<void>((ok) => s.once('end', ok));
  const closed = new Promise<void>((ok) => s.once('close', () => ok()));
  await new Promise<void>((ok, fail) => {
    s.once('secureConnect', ok);
    s.once('error', fail);
  });
  const got = () => Buffer.concat(chunks);
  return { s, got, text: () => got().toString(), ended, closed, errors };
}

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
afterEach(() => {
  expect(errors).toEqual([]);
  errors.length = 0;
});

describe('echo over TLS', () => {
  for (const maxVersion of ['TLSv1.3', 'TLSv1.2'] as const) {
    it(`echoes lines in-process over ${maxVersion}; close_notify gives bye`, async () => {
      const seen: NonNullable<RawStep['tls']>[] = [];
      const h = serve(db, {
        service: TLS_SERVICE,
        layer: await tlsLayer(db),
        handler: (c, tx) => {
          if (c.tls) seen.push(c.tls);
          return echo(c, tx);
        },
        onError,
      });
      try {
        const p = await peer(w.servicePort('echo-tls'), { maxVersion });
        expect(p.s.getProtocol()).toBe(maxVersion);
        const suite = p.s.getCipher().standardName;
        p.s.write('a\nb');
        await until(() => p.text() === '1: a\n', 10_000, 'the first reply');
        p.s.write('c\n');
        await until(() => p.text() === '1: a\n2: bc\n', 10_000, 'the second reply');
        p.s.end();
        await p.ended;
        expect(p.text()).toBe('1: a\n2: bc\nbye\n');
        expect(p.errors).toEqual([]);
        const t = seen.at(-1)!;
        expect(t.version).toBe(maxVersion);
        expect(t.suite).toBe(suite);
        expect(t.sni).toBe('localhost');
        // The client's end() sent close_notify: not a truncation.
        expect(t.truncated).toBe(false);
        expect(seen.every((s) => !s.truncated)).toBe(true);
        await until(() => h.conns === 0, 10_000, 'the connection to be forgotten');
        const rows = (await lines(db)).filter((r) => r.line === 'a' || r.line === 'bc');
        expect(rows.length).toBeGreaterThanOrEqual(2);
      } finally {
        await h.stop();
      }
    });
  }

  it('a bad certificate is reported and nothing is claimed', async () => {
    const got: unknown[] = [];
    const h = serve(db, {
      service: TLS_SERVICE,
      layer: tls({ cert: 'not a certificate', key: 'not a key' }),
      handler: echo,
      onError: (e) => got.push(e),
    });
    try {
      await until(() => got.length > 0, 10_000, 'the error');
      expect((got[0] as Error).name).toBe('ZenTlsError');
      expect((got[0] as Error).message).toBe('config');
      expect(h.conns).toBe(0);
    } finally {
      await h.stop();
    }
  });

  it('a worker SIGKILLed mid-stream: the next one continues the TLS connection', async () => {
    const N = 200;
    const env = workerEnv(w, 'alice', {
      ECHO_LEASE_TTL_MS: '1000',
      ECHO_LAYER: 'tls',
      ECHO_SERVICE: 'echo-tls',
    });
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
      const p = await peer(w.servicePort('echo-tls'));
      const replies = () => p.text().split('\n').filter(Boolean).length;
      const sending = (async () => {
        for (let i = 1; i <= N; i++) {
          p.s.write(`l${i}\n`);
          await sleep(5);
        }
      })();
      await until(() => replies() >= 80, 60_000, '80 replies');
      await worker.kill();
      worker = await start();
      await sending;
      await until(() => replies() >= N, 60_000, `all ${N} replies`);
      const want = Array.from({ length: N }, (_, i) => `${i + 1}: l${i + 1}\n`).join('');
      expect(p.text()).toBe(want);
      p.s.end();
      await p.ended;
      expect(p.text()).toBe(`${want}bye\n`);
      expect(p.errors).toEqual([]);
      const mine = (await lines(db)).filter((r) => !before.has(r.conn)).sort((a, b) => a.n - b.n);
      expect(new Set(mine.map((r) => r.conn)).size).toBe(1);
      expect(mine.map((r) => [r.n, r.line])).toEqual(
        Array.from({ length: N }, (_, i) => [i + 1, `l${i + 1}`]),
      );
    } finally {
      for (const k of started) await k.kill();
    }
  });

  it('a worker SIGKILLed mid-handshake: the server aborts the connection; the next one works', async () => {
    const env = workerEnv(w, 'alice', {
      ECHO_LEASE_TTL_MS: '1000',
      ECHO_LAYER: 'tls',
      ECHO_SERVICE: 'echo-tls',
    });
    const first = spawnWorker('echo', 'handler', env);
    let second: WorkerProcess | undefined;
    const proxy = await holdingProxy(w.servicePort('echo-tls'));
    try {
      await first.waitFor('handler ready');
      const s = connect({ host: '127.0.0.1', port: proxy.port, ca, servername: 'localhost' });
      const closed = new Promise<void>((ok) => s.once('close', () => ok()));
      s.on('error', () => {});
      // The client's second flight is held: the handler has committed the
      // ClientHello and its own flight, and is waiting for the Finished.
      await until(() => proxy.held() > 0, 30_000, "the client's second flight");
      await sleep(300);
      expect(proxy.isServerClosed()).toBe(false);
      await first.kill();
      // The successor's lease calls reap the lapsed lease; the connection,
      // not resumable mid-handshake, is aborted.
      second = spawnWorker('echo', 'handler', env);
      await second.waitFor('handler ready');
      await Promise.race([
        proxy.serverClosed,
        sleep(20_000).then(() => {
          throw new Error('the server kept the connection');
        }),
      ]);
      proxy.release();
      await Promise.race([
        closed,
        sleep(10_000).then(() => {
          throw new Error('the client hung');
        }),
      ]);
      const p = await peer(w.servicePort('echo-tls'));
      p.s.write('after\n');
      await until(() => p.text() === '1: after\n', 30_000, 'the reply after the takeover');
      p.s.destroy();
    } finally {
      proxy.close();
      await first.kill();
      await second?.kill();
    }
  });

  it('20 MB to the mirror across two kills: the SHA-256 of what came back matches', async () => {
    const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_LAYER: 'mirror' });
    const started: WorkerProcess[] = [];
    const start = async () => {
      const k = spawnWorker('echo', 'handler', env);
      started.push(k);
      await k.waitFor('handler ready');
      return k;
    };
    try {
      let worker = await start();
      const p = await peer(w.servicePort('mirror-tls'));
      const N = 20 << 20;
      const data = randomBytes(N);
      const back = () => p.got().length;
      const sending = (async () => {
        for (let at = 0; at < N; at += 1 << 16) {
          if (!p.s.write(data.subarray(at, Math.min(at + (1 << 16), N)))) {
            await new Promise((r) => p.s.once('drain', r));
          }
        }
      })();
      for (const frac of [0.3, 0.65]) {
        await until(() => back() >= N * frac, 120_000, `${frac * 100}% back`);
        await worker.kill();
        worker = await start();
      }
      await sending;
      await until(() => back() >= N, 120_000, 'all 20 MB back');
      expect(back()).toBe(N);
      const sha = (b: Buffer) => createHash('sha256').update(b).digest('hex');
      expect(sha(p.got())).toBe(sha(data));
      p.s.end();
      await p.ended;
      expect(p.errors).toEqual([]);
    } finally {
      for (const k of started) await k.kill();
    }
  }, 300_000);
});

/**
 * A TCP proxy to `port` that forwards the client's first flight and the
 * server's replies, then holds what the client sends after the server
 * spoke, until `release()`.
 */
async function holdingProxy(port: number): Promise<{
  port: number;
  held(): number;
  release(): void;
  isServerClosed(): boolean;
  serverClosed: Promise<void>;
  close(): void;
}> {
  const held: Buffer[] = [];
  let up: Socket | undefined;
  let down: Socket | undefined;
  let spoke = false;
  let released = false;
  let isClosed = false;
  let serverClosed!: () => void;
  const closedP = new Promise<void>((r) => {
    serverClosed = () => {
      isClosed = true;
      r();
    };
  });
  const server: Server = createServer((c) => {
    down = c;
    up = tcp(port, '127.0.0.1');
    up.on('data', (d) => {
      spoke = true;
      c.write(d);
    });
    up.on('close', () => {
      serverClosed();
      c.destroy();
    });
    up.on('error', () => {});
    c.on('data', (d) => {
      if (spoke && !released) held.push(d);
      else up!.write(d);
    });
    c.on('error', () => {});
    c.on('close', () => up?.destroy());
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const addr = server.address();
  if (!addr || typeof addr === 'string') throw new Error('no address');
  return {
    port: addr.port,
    held: () => held.length,
    release: () => {
      released = true;
      for (const d of held) if (up && !up.destroyed) up.write(d);
    },
    isServerClosed: () => isClosed,
    serverClosed: closedP,
    close: () => {
      down?.destroy();
      up?.destroy();
      server.close();
    },
  };
}
