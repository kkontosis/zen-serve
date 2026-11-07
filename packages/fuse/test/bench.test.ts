// A large file through zen-mount (the CLI, as a child) at its defaults
// (docs/STATS.md §7): written into the mount in 1 MiB writes and fsynced,
// read back on a fresh cache, and edited by a byte, with zen-mount's peak
// memory. Opt-in, never in CI's default runs; needs FUSE and `npm run build`:
//
//   ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve \
//     npx vitest run packages/fuse/test/bench.test.ts
//
// `ZEN_BENCH_GIB` sets the size (default 4).
import { type ChildProcess, execFileSync, spawn } from 'node:child_process';
import { createReadStream, existsSync } from 'node:fs';
import { mkdtemp, open, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { bytes, ROOT, Tree, type UnlockedFs, zw } from '@zen/client';
import { afterAll, describe, expect, it } from 'vitest';
import {
  benchBytes,
  checkDisk,
  digest,
  diskPeak,
  keystream,
  MiB,
  mib,
  peakRss,
  rate,
  resetPeak,
  sleep,
} from '../../client/test/benchkit.js';
import { type World, world } from '../../client/test/helpers.js';

const canMount =
  process.platform === 'linux' &&
  existsSync('/dev/fuse') &&
  ['/bin/fusermount', '/usr/bin/fusermount'].some((p) => existsSync(p));

const N = benchBytes();
const root = resolve(import.meta.dirname, '../../..');
const lines: string[] = [];
const row = (what: string, figure: string) => lines.push(`| ${what} | ${figure} |`);

describe.skipIf(!process.env.ZEN_BENCH || !canMount)('a large file through zen-mount', () => {
  let w: World | undefined;
  let dir: string | undefined;
  const children: ChildProcess[] = [];
  afterAll(async () => {
    console.log(['', `| zen-mount, ${mib(N)} MiB | Figure |`, '|---|---|', ...lines].join('\n'));
    for (const c of children) c.kill('SIGKILL');
    if (dir) {
      try {
        execFileSync('fusermount', ['-uz', join(dir, 'mnt')], { stdio: 'ignore' });
      } catch {
        // not mounted
      }
      await rm(dir, { recursive: true, force: true });
    }
    await w?.server.stop();
  });

  /** zen-mount in the foreground on `cache`, mounted once it says so. */
  async function mount(id: Uint8Array, keyFile: string, cache: string) {
    const child = spawn(
      process.execPath,
      [
        join(root, 'packages/fuse/dist/cli.js'),
        w!.server.url,
        join(dir!, 'mnt'),
        '--origin',
        w!.client.origin,
        '--tree',
        bytes.hex(id),
        '--device-key',
        keyFile,
        '--cache-dir',
        cache,
        '--foreground',
      ],
      { stdio: ['ignore', 'pipe', 'pipe'] },
    );
    children.push(child);
    let err = '';
    child.stderr!.on('data', (d) => {
      err += d;
    });
    const exit = new Promise<number | null>((r) => child.once('exit', (c) => r(c)));
    for (let i = 0; !err.includes('mounted fs'); i++) {
      if (i > 600 || child.exitCode !== null) throw new Error(`zen-mount didn't mount:\n${err}`);
      await sleep(100);
    }
    return {
      pid: child.pid!,
      err: () => err,
      /** SIGTERM, then the ms until it exits (the drain included). */
      async stop() {
        const t0 = performance.now();
        child.kill('SIGTERM');
        const code = await exit;
        expect(code).toBe(0);
        return performance.now() - t0;
      },
    };
  }

  /** The newest version of `/big` on the server, if any. */
  async function onServer(u: UnlockedFs, id: Uint8Array) {
    const t = u.tree(id);
    const big = (await t.list(ROOT)).find((n) => n.name === 'big');
    return big ? { node: big.id, v: (await t.versions(big.id))[0] } : undefined;
  }

  it(
    'write and fsync, the drain, a cold read, a 1-byte edit',
    async () => {
      checkDisk(3 * N, 'the zen-mount bench');
      w = await world({ limits: { chunk_grace_secs: 5, sweep_interval_secs: 1 } });
      const u = await w.session
        .fs(1)
        .init((k) => [zw.createDeviceSlot(k, w!.admin.device.devicePublic)]);
      const id = Tree.newId();
      await u.tree(id).mkdir(ROOT, '.keep');
      dir = await mkdtemp(join(tmpdir(), 'zen-bench-mount-'));
      const keyFile = join(dir, 'device.json');
      await writeFile(
        keyFile,
        JSON.stringify({
          identity_seed: bytes.hex(new Uint8Array(32).fill(1)),
          device_seed: bytes.hex(new Uint8Array(32).fill(0x81)),
          cert: bytes.hex(w.admin.cert),
        }),
      );
      const file = join(dir, 'mnt', 'big');

      // 1. Write N bytes in 1 MiB writes, fsync, close; then until the server has it.
      let m = await mount(id, keyFile, join(dir, 'cache1'));
      resetPeak(m.pid);
      resetPeak(w.server.pid);
      const disk = diskPeak(join(dir, 'cache1'));
      const t0 = performance.now();
      const fh = await open(file, 'w');
      for (const b of keystream(1, 0, N)) await fh.write(b);
      await fh.sync();
      await fh.close();
      const written = performance.now() - t0;
      for (;;) {
        const s = await onServer(u, id);
        if (s?.v?.size === N) break;
        await sleep(500);
      }
      const sent = performance.now() - t0;
      const mountPeak = peakRss(m.pid);
      const serverPeak = peakRss(w.server.pid);
      const stopMs = await m.stop();
      const cacheMost = await disk.stop();
      row(
        `write ${mib(N)} MiB (1 MiB writes), fsync, close`,
        `${rate(N, written)} MiB/s until closed, ${rate(N, sent)} MiB/s until on the server; ` +
          `zen-mount ${mib(mountPeak)} MiB, server ${mib(serverPeak)} MiB; ` +
          `the cache dir at most ${mib(cacheMost)} MiB; stop ${(stopMs / 1000).toFixed(1)} s`,
      );

      // 2. Read it back on a fresh cache dir.
      m = await mount(id, keyFile, join(dir, 'cache2'));
      resetPeak(m.pid);
      const t1 = performance.now();
      const got = await digest(createReadStream(file, { highWaterMark: MiB }));
      const readMs = performance.now() - t1;
      expect(got).toEqual(await digest(keystream(1, 0, N)));
      row(
        'read it back, cold (a fresh cache dir)',
        `${rate(N, readMs)} MiB/s; zen-mount ${mib(peakRss(m.pid))} MiB`,
      );

      // 3. A 1-byte write at the middle, and fsync.
      const mid = Math.floor(N / 2);
      const t2 = performance.now();
      const g = await open(file, 'r+');
      await g.write(new Uint8Array([0x5a]), 0, 1, mid);
      await g.sync();
      await g.close();
      const editMs = performance.now() - t2;
      const stop2 = await m.stop();
      const s = await onServer(u, id);
      expect(s!.v!.size).toBe(N);
      expect([...(await u.tree(id).read(s!.node, undefined, mid, 1))]).toEqual([0x5a]);
      row(
        'a 1-byte write at the middle, fsync, close',
        `${editMs.toFixed(0)} ms; stop (the edit sent) ${(stop2 / 1000).toFixed(1)} s`,
      );
      u.close();
    },
    4 * 3600_000,
  );
});
