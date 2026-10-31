// Measured costs of integrity in @zen/client (docs/STATS.md §6.1, §6.2).
// Opt-in, never in CI:
//
//   ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve \
//     npx vitest run packages/client/test/integritybench.test.ts
//
// with `cargo build --release -p zen-server --features test-utils` and
// `scripts/build-wasm.sh` (release WASM). Each line is the median of its runs.
// Ten writer devices of one user: the admin's own (in the ACL) and nine
// enrolled ones; a verify-only reader syncs them.
import { afterAll, beforeAll, describe, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import {
  type Fs,
  type Integrity,
  type Session,
  type Signer,
  type Topic,
  type UnlockedFs,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

const RUNS = 20;
const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)]!;
};
const fmt = (x: number) => (x >= 100 ? x.toFixed(0) : x >= 10 ? x.toFixed(1) : x.toFixed(2));
const lines: string[] = [];
const record = (what: string, ms: number[], bytes: number | string = '–') =>
  lines.push(`| ${what} | ${fmt(median(ms))} | ${bytes} |`);

let w: World;
/** Each fs's recovery key. */
const recoveryKey = new Map<number, Uint8Array>();

/** The bytes of each call to `path` while `fn` runs (the encoded request). */
async function sent<R>(s: Session, path: string, fn: () => Promise<R>): Promise<number[]> {
  const call = s.call;
  const sizes: number[] = [];
  s.call = ((p: string, enc: (r: unknown) => Uint8Array, dec: unknown, req: unknown) => {
    if (p === path) sizes.push(enc(req).length);
    return (call as (...a: unknown[]) => unknown).call(s, p, enc, dec, req);
  }) as typeof s.call;
  try {
    await fn();
    return sizes;
  } finally {
    s.call = call;
  }
}

interface Side {
  fs: Fs;
  integ: Integrity;
  u: UnlockedFs;
}

/** fs `id` with integrity attached (`device` signs; none: verify only), no cadence. */
async function open(id: number, device?: Signer): Promise<Side> {
  const fs = w.session.fs(id);
  const integ = await fs.integrity({
    ...(device ? { device } : {}),
    sigEvery: 1_000_000,
    delayMs: 3_600_000,
  });
  const u = await fs.unlock({ recoveryKey: recoveryKey.get(id)! });
  return { fs, integ, u };
}

async function all<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const x of it) out.push(x);
  return out;
}

const opened: Side[] = [];

beforeAll(async () => {
  if (!process.env.ZEN_BENCH) return;
  w = await world();
  for (const id of [1, 2]) {
    const u = await w.session.fs(id).init((k) => {
      const r = zw.createRecoverySlot(k);
      recoveryKey.set(id, r.recoveryKey);
      return [r.slot];
    });
    u.close();
  }
});
afterAll(async () => {
  for (const s of opened) await s.integ.close();
  if (lines.length) {
    console.log(['', '| What | ms (median) | bytes |', '|---|---|---|', ...lines].join('\n'));
  }
  await w?.server.stop();
});

describe.skipIf(!process.env.ZEN_BENCH)('integrity costs', () => {
  it('checkpoints and syncs', async () => {
    // Ten writers: the admin's device and nine enrolled ones.
    const signers: Signer[] = [{ device: w.admin.device, cert: w.admin.cert }];
    for (let i = 0; i < 9; i++) {
      const { device } = await w.session.enrollDevice(w.admin.identity);
      signers.push({ device: device.device, cert: device.cert });
    }
    const writers: Side[] = [];
    for (const s of signers) writers.push(await open(1, s));
    opened.push(...writers);
    const [a] = writers as [Side];

    // A checkpoint carrying n entries (one per topic), after one more change.
    for (const n of [10, 100]) {
      const ts: Topic[] = Array.from({ length: n }, (_, i) => a.u.topic(`n${n}-${i}`));
      for (const t of ts) await t.append(utf8('x'));
      await a.integ.checkpoint();
      const ms: number[] = [];
      const bytes = await sent(w.session, '/v1/checkpoint/put', async () => {
        for (let i = 0; i < RUNS; i++) {
          await ts[i % n]!.append(utf8('y'));
          const t0 = performance.now();
          await a.integ.checkpoint();
          ms.push(performance.now() - t0);
        }
      });
      record(`checkpoint(), ${n} + entries (sign, seal, put)`, ms, median(bytes));
    }

    // A verify-only reader syncs after 1 and after 10 devices moved.
    const v = await open(1);
    opened.push(v);
    for (const s of writers) {
      await s.u.topic('sync').append(utf8('s'));
      await s.integ.checkpoint();
    }
    await v.integ.sync();
    for (const k of [1, 10]) {
      const ms: number[] = [];
      for (let i = 0; i < RUNS; i++) {
        for (const s of writers.slice(0, k)) {
          await s.u.topic('sync').append(utf8('s'));
          await s.integ.checkpoint();
        }
        const t0 = performance.now();
        await v.integ.sync();
        ms.push(performance.now() - t0);
      }
      record(`sync(), ${k} of 10 devices moved`, ms);
    }
    if (v.integ.alarms.length) throw new Error(`alarms: ${v.integ.alarms.map((x) => x.detail)}`);
    record('exportPins(), 10 devices', [0], v.integ.exportPins().length);
  });

  it('event chains', async () => {
    const a = await open(1, { device: w.admin.device, cert: w.admin.cert });
    opened.push(a);
    const plainFs = await w.session.fs(2).unlock({ recoveryKey: recoveryKey.get(2)! });
    const topics: [string, Topic][] = [
      ['without integrity (v1 bodies)', plainFs.topic('chain')],
      ['with integrity (v2 bodies, chained)', a.u.topic('chain')],
    ];
    for (const [what, t] of topics) {
      await t.append(utf8('warm'));
      const ms: number[] = [];
      const bytes = await sent(w.session, '/v1/log/append', async () => {
        for (let i = 0; i < 50; i++) {
          const t0 = performance.now();
          await t.append(utf8(`event ${i} with a short payload`));
          ms.push(performance.now() - t0);
        }
      });
      record(`append, ${what}`, ms, median(bytes));
    }
    for (const [what, t] of topics) {
      const t1 = t.fs === plainFs ? plainFs.topic('read') : a.u.topic('read');
      for (let i = 0; i < 1000; i++) await t1.append(utf8(`e${i}`));
      const ms: number[] = [];
      for (let i = 0; i < 5; i++) {
        const t0 = performance.now();
        const n = (await all(t1.read())).length;
        ms.push(performance.now() - t0);
        if (n !== 1000) throw new Error(`read ${n}`);
      }
      record(`read 1,000 events, ${what}`, ms);
    }
    plainFs.close();
  });
});
