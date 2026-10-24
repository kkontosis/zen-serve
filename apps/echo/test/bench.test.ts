// Measured costs of relayed services (docs/STATS.md §5.5) on the echo
// world: one embedded zen-serve and the handlers in this process, or in
// Node workers for the takeover. Opt-in, never in CI:
//
//   ZEN_BENCH=1 npx vitest run apps/echo/test/bench.test.ts
//
// STATS §5.5 was measured with release builds: ZEN_SERVE_BIN pointing at
// `cargo build --release -p zen-server`, and `scripts/build-wasm.sh`.
//
// Step latency is from the peer's write of a line to its reply (each a
// step that also inserts a row); throughput is a mirror's bytes back per
// second; the takeover pause is the longest wait for a reply when the
// handler holding the connection is SIGKILLed; an idle connection's cost
// is the server's RSS and the handler's heap per connection, and the
// steps run while they sit.
import { type ChildProcess, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { type Socket, connect as tcp } from 'node:net';
import { connect } from 'node:tls';
import type { Db } from '@zen/db';
import { type RawHandler, serve } from '@zen/svc';
import { afterAll, beforeAll, describe, it } from 'vitest';
import {
  type TestSshKeys,
  testCerts,
  testSshKeys,
} from '../../../packages/client/src/testing/index.js';
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
  AUTH_SERVICE,
  DB_OPTIONS,
  echo,
  MIRROR_SERVICE,
  mirror,
  NS,
  putCert,
  putHostKey,
  putSshKey,
  SERVICE,
  SSH_SERVICE,
  sshEcho,
  sshLayer,
  TLS_SERVICE,
  tlsLayer,
} from '../src/echo.js';
import { GRANTS } from './grants.js';

const NO_RESUME = ['svc', 'echo-noresume'];
const MIRROR_RAW = ['svc', 'mirror-raw'];
const IDLE = ['svc', 'echo-idle'];
const IDLE_CONNS = 1000;
const MB = 1 << 20;

const out: string[] = [];
const row = (...cells: (string | number)[]) => out.push(`| ${cells.join(' | ')} |`);

const sorted = (xs: number[]) => [...xs].sort((a, b) => a - b);
const median = (xs: number[]) => sorted(xs)[Math.floor(xs.length / 2)]!;
const p90 = (xs: number[]) => sorted(xs)[Math.floor(xs.length * 0.9)]!;
const ms = (x: number) => (x >= 100 ? x.toFixed(0) : x.toFixed(1));

/** A byte stream to a service: its writes, and a wait for what comes back. */
interface Peer {
  write(d: string | Uint8Array): void;
  /** Bytes received so far. */
  received(): number;
  /** Resolves once the text received includes `s`. */
  waitFor(s: string): Promise<void>;
  end(): void;
}

/** A peer over any source of chunks (a socket, an ssh child, a SvcSocket). */
function peerOf(write: Peer['write'], end: () => void): Peer & { feed(d: Uint8Array): void } {
  let text = '';
  let n = 0;
  let wait: { s: string; ok: () => void } | undefined;
  const check = () => {
    if (wait && text.includes(wait.s)) {
      const w = wait;
      wait = undefined;
      text = '';
      w.ok();
    }
  };
  return {
    write,
    end,
    received: () => n,
    feed(d) {
      n += d.length;
      if (wait || text.length < 4096) text += Buffer.from(d).toString('latin1');
      check();
    },
    waitFor: (s) =>
      new Promise<void>((ok) => {
        wait = { s, ok };
        check();
      }),
  };
}

async function tcpPeer(port: number): Promise<Peer> {
  const s = tcp(port, '127.0.0.1');
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  s.setNoDelay(true);
  const p = peerOf(
    (d) => s.write(d),
    () => s.destroy(),
  );
  s.on('data', (d: Buffer) => p.feed(d));
  s.on('error', () => {});
  return p;
}

async function tlsPeer(port: number, ca: string): Promise<Peer> {
  const s = connect({ host: '127.0.0.1', port, ca, servername: 'localhost' });
  await new Promise<void>((ok, fail) => s.once('secureConnect', ok).once('error', fail));
  s.setNoDelay(true);
  const p = peerOf(
    (d) => s.write(d),
    () => s.destroy(),
  );
  s.on('data', (d: Buffer) => p.feed(d));
  s.on('error', () => {});
  return p;
}

function sshPeer(args: string[]): Peer & { child: ChildProcess } {
  const c = spawn('ssh', args, { stdio: ['pipe', 'pipe', 'pipe'] });
  c.stdin.on('error', () => {});
  const p = peerOf(
    (d) => c.stdin.write(d),
    () => c.kill('SIGKILL'),
  );
  c.stdout.on('data', (d: Buffer) => p.feed(d));
  return Object.assign(p, { child: c });
}

/** `runs` lines through the echo, after `warm` unmeasured ones: each line's ms. */
async function lineTimes(p: Peer, runs = 50, warm = 5): Promise<number[]> {
  const times: number[] = [];
  for (let i = 0; i < warm + runs; i++) {
    const t = performance.now();
    const got = p.waitFor(`${i + 1}: x${i}\n`);
    p.write(`x${i}\n`);
    await got;
    if (i >= warm) times.push(performance.now() - t);
  }
  return times;
}

/** Send `n` random bytes and read them back: MB/s. */
async function mirrorRate(p: Peer, n: number): Promise<number> {
  const data = randomBytes(n);
  const t = performance.now();
  for (let at = 0; at < n; at += 1 << 16) p.write(data.subarray(at, at + (1 << 16)));
  await until(() => p.received() >= n, 300_000, `${n} bytes back (have ${p.received()})`);
  return n / MB / ((performance.now() - t) / 1000);
}

/** The resident set of a process, in KiB. */
function rssKiB(pid: number): number {
  const m = /VmRSS:\s+(\d+) kB/.exec(readFileSync(`/proc/${pid}/status`, 'utf8'));
  return Number(m?.[1] ?? Number.NaN);
}

describe.skipIf(!process.env.ZEN_BENCH)('relayed service costs', () => {
  let w: World;
  let alice: WorkerContext;
  let db: Db;
  let ca: string;
  let keys: TestSshKeys;
  const onError = (e: unknown) => console.error('handler error', e);

  beforeAll(async () => {
    w = await startWorld({
      grants: GRANTS,
      services: [
        { name: 'echo', path: SERVICE },
        { name: 'echo-noresume', path: NO_RESUME, resume: false },
        { name: 'echo-tls', path: TLS_SERVICE },
        { name: 'mirror-raw', path: MIRROR_RAW },
        { name: 'mirror-tls', path: MIRROR_SERVICE },
        { name: 'echo-ssh', path: SSH_SERVICE, more: { window: 1_048_576 } },
        { name: 'echo-auth', path: AUTH_SERVICE, mode: 'tcp-auth' },
        {
          name: 'echo-idle',
          path: IDLE,
          more: { max_conns: 2 * IDLE_CONNS, per_ip_rate: 0, accept_wait_ms: 60_000 },
        },
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
    keys = testSshKeys();
    await putHostKey(db, keys.hostKey, keys.hostLine);
    await putSshKey(db, 'test', keys.userLine);
  }, 120_000);

  afterAll(async () => {
    console.log(['', ...out].join('\n'));
    await alice?.close();
    await w?.stop();
    keys?.cleanup();
  });

  it('step latency: a line and its reply', async () => {
    out.push('', '| Step latency (line → reply) | median ms | p90 ms |', '|---|---|---|');
    const report = (what: string, t: number[]) => row(what, ms(median(t)), ms(p90(t)));
    const serving = async (h: { stop(): Promise<void> }, fn: () => Promise<void>) => {
      try {
        await fn();
      } finally {
        await h.stop();
      }
    };

    await serving(serve(db, { service: SERVICE, handler: echo, onError }), async () => {
      const p = await tcpPeer(w.servicePort('echo'));
      report('raw, `resume: true`', await lineTimes(p));
      p.end();
    });
    await serving(serve(db, { service: NO_RESUME, handler: echo, onError }), async () => {
      const p = await tcpPeer(w.servicePort('echo-noresume'));
      report('raw, `resume: false`', await lineTimes(p));
      p.end();
    });
    await serving(
      serve(db, { service: TLS_SERVICE, handler: echo, layer: await tlsLayer(db), onError }),
      async () => {
        const p = await tlsPeer(w.servicePort('echo-tls'), ca);
        report('TLS 1.3', await lineTimes(p));
        p.end();
      },
    );
    await serving(
      serve(db, { service: SSH_SERVICE, handler: sshEcho, layer: await sshLayer(db), onError }),
      async () => {
        const p = sshPeer([...keys.sshArgs(w.servicePort('echo-ssh')), 'lines']);
        report('SSH (`lines` on one channel)', await lineTimes(p));
        p.end();
      },
    );
    await serving(serve(db, { service: AUTH_SERVICE, handler: echo, onError }), async () => {
      let p!: Peer & { feed(d: Uint8Array): void };
      const s = await alice.session.svcConnect({
        fs: 1,
        svc: Buffer.from(w.serviceId('echo-auth'), 'hex'),
        onData: (d) => p.feed(d),
      });
      p = peerOf(
        (d) => s.write(typeof d === 'string' ? Buffer.from(d) : d),
        () => s.reset(),
      );
      report('`tcp-auth` (svcConnect)', await lineTimes(p));
      p.end();
    });
  }, 300_000);

  it('throughput: 20 MB through a mirror', async () => {
    out.push('', '| Throughput (20 MB each way) | MB/s |', '|---|---|');
    const N = 20 * MB;
    {
      const h = serve(db, { service: MIRROR_RAW, handler: mirror, onError });
      const p = await tcpPeer(w.servicePort('mirror-raw'));
      row('raw mirror', (await mirrorRate(p, N)).toFixed(1));
      p.end();
      await h.stop();
    }
    {
      const h = serve(db, {
        service: MIRROR_SERVICE,
        handler: mirror,
        layer: await tlsLayer(db),
        onError,
      });
      const p = await tlsPeer(w.servicePort('mirror-tls'), ca);
      row('TLS mirror', (await mirrorRate(p, N)).toFixed(1));
      p.end();
      await h.stop();
    }
    {
      const h = serve(db, {
        service: SSH_SERVICE,
        handler: sshEcho,
        layer: await sshLayer(db),
        onError,
      });
      const p = sshPeer([...keys.sshArgs(w.servicePort('echo-ssh')), 'cat']);
      row('SSH `cat`', (await mirrorRate(p, N)).toFixed(1));
      p.end();
      await h.stop();
    }
  }, 600_000);

  it('the takeover pause: the holder SIGKILLed, a successor waiting (lease TTL 1 s)', async () => {
    const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_LAYER: 'raw' });
    const started: WorkerProcess[] = [];
    const start = async () => {
      const k = spawnWorker('echo', 'handler', env);
      started.push(k);
      await k.waitFor('handler ready', 60_000);
      return k;
    };
    const pauses: number[] = [];
    try {
      for (let round = 0; round < 5; round++) {
        const holder = await start();
        const p = await tcpPeer(w.servicePort('echo'));
        // The holder has the connection before the successor exists.
        const first = p.waitFor('1: x0\n');
        p.write('x0\n');
        await first;
        const successor = await start();
        let worst = 0;
        let killed = false;
        for (let i = 1; i < 400; i++) {
          const t = performance.now();
          const got = p.waitFor(`${i + 1}: x${i}\n`);
          p.write(`x${i}\n`);
          await got;
          if (killed) worst = Math.max(worst, performance.now() - t);
          if (i === 100) {
            void holder.kill();
            killed = true;
          }
          await sleep(5);
        }
        pauses.push(worst);
        p.end();
        await successor.kill();
      }
    } finally {
      for (const k of started) await k.kill();
    }
    out.push(
      '',
      '| Takeover pause (5 rounds) | ms |',
      '|---|---|',
      `| median | ${ms(median(pauses))} |`,
      `| each | ${pauses.map(ms).join(', ')} |`,
    );
  }, 600_000);

  it(`${IDLE_CONNS} idle connections: memory, and the steps they cost`, async () => {
    let calls = 0;
    const counted: RawHandler = (c, tx) => {
      calls++;
      return echo(c, tx);
    };
    const h = serve(db, {
      service: IDLE,
      handler: counted,
      maxConns: 2 * IDLE_CONNS,
      onError,
    });
    const socks: Socket[] = [];
    const errs: string[] = [];
    let closedN = 0;
    try {
      await sleep(1000);
      const rss0 = rssKiB(w.server.pid);
      const heap0 = process.memoryUsage().heapUsed;
      for (let i = 0; i < IDLE_CONNS; i++) {
        const s = tcp(w.servicePort('echo-idle'), '127.0.0.1');
        s.on('error', (e) => errs.push(String(e)));
        s.on('close', () => closedN++);
        socks.push(s);
        if (i % 50 === 49) await sleep(20);
      }
      try {
        await until(() => h.conns === IDLE_CONNS, 120_000, `${IDLE_CONNS} connections held`);
      } catch (e) {
        // Are the missing ones queued (a second handler takes them) or gone?
        const log = (await w.server.log()).split('\n').slice(-40).join('\n');
        let other = 0;
        const h2 = serve(db, {
          service: IDLE,
          handler: (c, tx) => {
            other++;
            return echo(c, tx);
          },
          maxConns: 100,
          onError,
        });
        await sleep(5000);
        await h2.stop();
        throw new Error(
          `${e}: held ${h.conns}; closed ${closedN}; errors ${errs.slice(0, 5)}; a second handler took ${other}\n${log}`,
        );
      }
      // Every connection's first step (its state) has committed.
      await until(() => calls >= IDLE_CONNS, 120_000, 'the first steps');
      await sleep(2000);
      const rss1 = rssKiB(w.server.pid);
      const heap1 = process.memoryUsage().heapUsed;
      const before = calls;
      await sleep(10_000);
      const idleSteps = calls - before;
      out.push(
        '',
        `| ${IDLE_CONNS} idle raw connections | |`,
        '|---|---|',
        `| server RSS | ${(rss0 / 1024).toFixed(0)} → ${(rss1 / 1024).toFixed(0)} MiB (${((rss1 - rss0) / IDLE_CONNS).toFixed(1)} KiB each) |`,
        `| handler heap | ${((heap1 - heap0) / IDLE_CONNS / 1024).toFixed(1)} KiB each |`,
        `| handler calls in 10 s idle | ${idleSteps} |`,
      );
    } finally {
      for (const s of socks) s.destroy();
      await h.stop();
    }
  }, 600_000);
});
