// A large file through the @zen/fs replica, over a disk store as zen-mount
// keeps it, at the default bounds (docs/STATS.md §7): written through a
// handle in 1 MiB writes, edited by a byte, read cold on a second replica,
// and pinned on a third, with the peaks of memory and of the bounds.
// Opt-in, never in CI's default runs:
//
//   ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve \
//     npx vitest run packages/fs/test/bench.test.ts
//
// `ZEN_BENCH_GIB` sets the size (default 4).
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { connect, ROOT, sealedStore, Tree, type UnlockedFs, zw } from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { afterAll, describe, expect, it } from 'vitest';
import {
  benchBytes,
  checkDisk,
  counts,
  digest,
  diskPeak,
  edited,
  keystream,
  MiB,
  mib,
  type Part,
  Peaks,
  partsOf,
  peakRss,
  rate,
  resetPeak,
  traffic,
} from '../../client/test/benchkit.js';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type Replica } from '../src/index.js';

const N = benchBytes();
const lines: string[] = [];
const row = (what: string, figure: string) => lines.push(`| ${what} | ${figure} |`);
const dirs: string[] = [];

/** A session of its own (not watched), unlocked. */
async function unlocked(w: World): Promise<UnlockedFs> {
  const client = await connect(w.server.url, { origin: w.client.origin });
  const session = await client.signInDevice(w.admin);
  return session.fs(1).unlock({ device: w.admin.device });
}

/** A replica over a fresh cache dir, at the defaults, caught up. */
async function replica(
  w: World,
  id: Uint8Array,
): Promise<{ r: Replica; u: UnlockedFs; dir: string }> {
  const dir = await mkdtemp(join(tmpdir(), 'zen-bench-replica-'));
  dirs.push(dir);
  const u = await unlocked(w);
  const r = await openReplica(u, id, {
    local: sealedStore(nodeDiskLocalStore(dir)),
    locks: null,
    channel: null,
    waitMs: 200,
  });
  await r.sync();
  return { r, u, dir };
}

const mem = (w: World, most: Record<string, number>) =>
  `RSS ${mib(peakRss())} MiB (heap ${mib(most.heapUsed!)}, buffers ${mib(most.arrayBuffers!)}), ` +
  `server ${mib(peakRss(w.server.pid))} MiB`;

describe.skipIf(!process.env.ZEN_BENCH)('a large file through the replica', () => {
  let w: World | undefined;
  afterAll(async () => {
    console.log(['', `| Replica, ${mib(N)} MiB | Figure |`, '|---|---|', ...lines].join('\n'));
    await w?.server.stop();
    for (const d of dirs) await rm(d, { recursive: true, force: true });
  });

  it(
    'write through a handle, a 1-byte edit, a cold read, a pin',
    async () => {
      checkDisk(3.5 * N, 'the replica bench');
      w = await world({ limits: { chunk_grace_secs: 5, sweep_interval_secs: 1 } });
      const admin = await w.session
        .fs(1)
        .init((k) => [zw.createDeviceSlot(k, w!.admin.device.devicePublic)]);
      const id = Tree.newId();
      await admin.tree(id).mkdir(ROOT, '.keep');

      // 1. Write N bytes through one handle, in 1 MiB writes; then the drain.
      const a = await replica(w, id);
      const h = await a.r.open('/big', 'w');
      resetPeak();
      resetPeak(w.server.pid);
      const peaks = new Peaks(
        { dirtyBytes: () => h.dirtyBytes, pendingBytes: () => a.r.pendingBytes },
        50,
      );
      const disk = diskPeak(a.dir);
      const t0 = performance.now();
      for (const b of keystream(1, 0, N)) await h.write(b);
      await h.close();
      const queued = performance.now() - t0;
      await a.r.flush();
      const sent = performance.now() - t0;
      const most = peaks.stop();
      const cacheMost = await disk.stop();
      row(
        `write ${mib(N)} MiB through a handle (1 MiB writes)`,
        `${rate(N, queued)} MiB/s until queued, ${rate(N, sent)} MiB/s until on the server; ` +
          mem(w, most),
      );
      row(
        'the bounds while writing',
        `dirtyBytes at most ${mib(most.dirtyBytes!)} MiB (bound 64), pendingBytes at most ` +
          `${mib(most.pendingBytes!)} MiB (bound 32); the cache dir at most ${mib(cacheMost)} MiB`,
      );
      let parts: Part[] = [{ seed: 1, from: 0, to: N }];

      // 2. A 1-byte overwrite at the middle, through a handle.
      const mid = Math.floor(N / 2);
      const one = new Uint8Array([0x5a]);
      const e = await traffic(a.u.session, async () => {
        const t1 = performance.now();
        const g = await a.r.open('/big', 'r+');
        await g.write(one, mid);
        await g.close({ wait: true });
        return performance.now() - t1;
      });
      parts = edited(parts, mid, 1, one);
      row(
        'a 1-byte overwrite at the middle, close with wait',
        `${e[0].toFixed(0)} ms; ${counts(e[1])}`,
      );
      await a.r.close();
      a.u.close();
      const want = await digest(partsOf(parts));

      // 3. A cold read on a second replica (a fresh cache dir), in 1 MiB reads.
      const b = await replica(w, id);
      resetPeak();
      resetPeak(w.server.pid);
      const rp = new Peaks({ pendingBytes: () => b.r.pendingBytes }, 50);
      const t2 = performance.now();
      const got = await digest(
        (async function* () {
          for (let i = 0; i < N; i += MiB) yield await b.r.read('/big', i, Math.min(MiB, N - i));
        })(),
      );
      const readMs = performance.now() - t2;
      const rmost = rp.stop();
      expect(got).toEqual(want);
      const kept = (await b.r.status()).cacheBytes;
      row(
        'a cold read on another replica (1 MiB reads)',
        `${rate(N, readMs)} MiB/s; ${mem(w, rmost)}; pendingBytes at most ${mib(rmost.pendingBytes!)} MiB; ` +
          `${mib(kept)} MiB kept in its cache (cacheBytes 1 GiB)`,
      );
      await b.r.close();
      b.u.close();

      // 4. A pin on a third replica: kept whole.
      const c = await replica(w, id);
      resetPeak();
      const pp = new Peaks({ pendingBytes: () => c.r.pendingBytes }, 50);
      const t3 = performance.now();
      await c.r.pin('/big');
      await c.r.idle();
      await c.r.flush();
      const pinMs = performance.now() - t3;
      const pmost = pp.stop();
      const pinKept = (await c.r.status()).cacheBytes;
      expect(pinKept).toBeGreaterThanOrEqual(N);
      row(
        'a pin on a third replica',
        `${rate(N, pinMs)} MiB/s; ${mem(w, pmost)}; pendingBytes at most ${mib(pmost.pendingBytes!)} MiB; ` +
          `${mib(pinKept)} MiB kept`,
      );
      await c.r.close();
      c.u.close();
      admin.close();
    },
    4 * 3600_000,
  );
});
