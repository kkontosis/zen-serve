// Large files through @zen/client against a spawned zen-serve (docs/STATS.md
// §7): writes, reads, random reads, appends, edits, a resume past the grace
// period, pipelining, many writers at once, and the chunker. Opt-in, never
// in CI's default runs:
//
//   ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve \
//     npx vitest run packages/client/test/largebench.test.ts
//
// with `cargo build --release -p zen-server` and `scripts/build-wasm.sh`.
// `ZEN_BENCH_GIB` sets the file size (default 4). FoundationDB:
// `ZEN_BENCH_BACKEND=fdb ZEN_BENCH_CLUSTER_FILE=/etc/foundationdb/fdb.cluster`
// with a server built `--features fdb` (CDC only). Memory is the peak
// resident set (`VmHWM`) of this process and of the server, per step.
import type { CdcParams } from '@zen/wasm';
import { afterAll, describe, expect, it } from 'vitest';
import {
  bytes,
  connect,
  ROOT,
  Tree,
  TreeClock,
  type UnlockedFs,
  type UploadState,
  zw,
} from '../src/index.js';
import {
  benchBytes,
  checkDisk,
  chunksOf,
  counts,
  digest,
  du,
  edited,
  GiB,
  keystream,
  MiB,
  mib,
  type Part,
  Peaks,
  partsOf,
  peakRss,
  prng,
  quantile,
  rate,
  resetPeak,
  sleep,
  traffic,
} from './benchkit.js';
import { type World, world } from './helpers.js';

const N = benchBytes();
const FDB = process.env.ZEN_BENCH_BACKEND === 'fdb';
const BACKEND = FDB ? 'fdb' : 'embedded';
const GRACE = 5;
const HOURS = 4 * 3600_000;
const lines: string[] = [];
const row = (form: string, what: string, figure: string) =>
  lines.push(`| ${BACKEND}, ${form} | ${what} | ${figure} |`);

async function openWorld(): Promise<{ w: World; ufs: UnlockedFs }> {
  const cluster = process.env.ZEN_BENCH_CLUSTER_FILE;
  const w = await world({
    limits: { chunk_grace_secs: GRACE, sweep_interval_secs: 1 },
    ...(FDB
      ? { extra: `[storage]\nbackend = "fdb"\ncluster_file = ${JSON.stringify(cluster)}\n` }
      : {}),
  });
  const ufs = await w.session
    .fs(1)
    .init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  return { w, ufs };
}

/** An upload state's size as JSON, ids in hex (as a client would keep it). */
const stateBytes = (s: UploadState) =>
  JSON.stringify(s, (_, v) => (v instanceof Uint8Array ? bytes.hex(v) : v)).length;

/** A source that fails once `at` bytes of it are read. */
function* failing(seed: number, from: number, to: number, at: number): Generator<Uint8Array> {
  let pos = from;
  for (const b of keystream(seed, from, to)) {
    yield b;
    pos += b.length;
    if (pos >= at) throw new Error('source lost');
  }
}

/** Peaks of this process and the server while `fn` runs. */
async function measured<T>(w: World, fn: () => Promise<T>) {
  resetPeak();
  resetPeak(w.server.pid);
  const p = new Peaks();
  const t0 = performance.now();
  const result = await fn();
  const ms = performance.now() - t0;
  const most = p.stop();
  return {
    result,
    ms,
    mem:
      `client ${mib(peakRss())} MiB (heap ${mib(most.heapUsed!)}, buffers ${mib(most.arrayBuffers!)}), ` +
      `server ${mib(peakRss(w.server.pid))} MiB`,
  };
}

/** Write a file under a session (upload, write, close), its progress kept. */
async function write(t: Tree, node: Uint8Array, src: Iterable<Uint8Array>, parallel?: number) {
  let most = 0;
  const up = await t.upload(node, src, {
    session: true,
    ...(parallel ? { parallel } : {}),
    onProgress: (s) => {
      most = Math.max(most, stateBytes(s));
    },
  });
  await t.send([await t.writeOp(up)]);
  await t.closeUpload(up.session!);
  return { up, stateMost: most };
}

/** The whole file read back as a stream, hashed. */
async function readBack(t: Tree, node: Uint8Array, readAhead?: number) {
  return digest(chunksOf(t.readStream(node, readAhead === undefined ? {} : { readAhead })));
}

/** Drop a file's content: a version of nothing, the old content collected after the grace. */
async function drop(t: Tree, node: Uint8Array) {
  await t.writeFile(node, new Uint8Array(0));
}

describe.skipIf(!process.env.ZEN_BENCH)('large files through @zen/client', () => {
  afterAll(() => {
    console.log(
      ['', `| ${mib(N)} MiB files | What | Figure |`, '|---|---|---|', ...lines].join('\n'),
    );
  });

  for (const form of FDB ? (['cdc'] as const) : (['cdc', 'fixed'] as const)) {
    it(
      `${form}: write, read, random reads, append, edits, resume, pipelining`,
      async () => {
        checkDisk(3 * N, 'the client bench');
        const { w, ufs } = await openWorld();
        try {
          if (form === 'fixed') await ufs.setPolicy({ chunking: 'fixed' });
          const id = Tree.newId();
          const t = ufs.tree(id);
          const s = w.session;

          // 1. Write N bytes under a session.
          const f = await t.create(ROOT, 'a');
          const before = du(w.server.dataDir);
          const wr = await measured(w, () => traffic(s, () => write(t, f, keystream(1, 0, N))));
          const [{ up, stateMost }, tw] = wr.result;
          const v = (await t.versions(f))[0]!;
          expect(v.size).toBe(N);
          expect(v.chunking).toBe(form);
          row(
            form,
            `write ${mib(N)} MiB (session)`,
            `${rate(N, wr.ms)} MiB/s; ${tw.commits} commits; ${wr.mem}`,
          );
          row(
            form,
            'the version',
            `${v.chunkCount} chunks (mean ${(N / v.chunkCount / 1024).toFixed(1)} KiB), ` +
              `${v.pages.length} pages, manifest ${v.sealedManifest.length} B; ` +
              `sent ${mib(tw.sent)} MiB; data dir +${mib(du(w.server.dataDir) - before)} MiB; ` +
              `largest upload state ${(stateMost / 1024).toFixed(1)} KiB; ${up.uploaded} chunks uploaded`,
          );
          let parts: Part[] = [{ seed: 1, from: 0, to: N }];
          const want = await digest(partsOf(parts));

          // 2. Cold reads of the whole file, 3 and 1 windows ahead.
          for (const ahead of [3, 1]) {
            const r = await measured(w, () => readBack(ufs.tree(id), f, ahead));
            expect(r.result).toEqual(want);
            row(form, `readStream, readAhead ${ahead}`, `${rate(N, r.ms)} MiB/s; ${r.mem}`);
          }

          // 3. 1,000 random reads of 4 KiB, on a fresh tree (cold pages), then again.
          const rnd = prng(42);
          const offs = Array.from({ length: 1000 }, () => Math.floor(rnd() * (N - 4096)));
          const rt = ufs.tree(id);
          for (const pass of ['cold', 'warm']) {
            const ms: number[] = [];
            for (const o of offs) {
              const t0 = performance.now();
              const got = await rt.read(f, v, o, 4096);
              ms.push(performance.now() - t0);
              const [exp] = keystream(1, o, o + 4096);
              if (!bytes.equal(got, exp!)) throw new Error(`random read at ${o} differs`);
            }
            row(
              form,
              `1,000 random 4 KiB reads (${pass})`,
              `median ${quantile(ms, 0.5).toFixed(2)} ms, p99 ${quantile(ms, 0.99).toFixed(2)} ms`,
            );
          }

          // 4. Append 1 MiB.
          const [more] = keystream(9, 0, MiB);
          const ap = await measured(w, () => traffic(s, () => ufs.tree(id).append(f, more!)));
          parts = [...parts, more!];
          row(form, 'append 1 MiB', `${ap.ms.toFixed(0)} ms; ${counts(ap.result[1])}`);

          // 5. A 4 KiB overwrite and a 100-byte insertion at the middle.
          const mid = Math.floor(N / 2);
          const [over] = keystream(10, 0, 4096);
          const ow = await measured(w, () =>
            traffic(s, () =>
              ufs.tree(id).edit(f, undefined, [{ offset: mid, remove: 4096, insert: over! }]),
            ),
          );
          parts = edited(parts, mid, 4096, over!);
          row(
            form,
            '4 KiB overwrite at the middle',
            `${ow.ms.toFixed(0)} ms; ${counts(ow.result[1])}`,
          );
          const [ins] = keystream(11, 0, 100);
          // Fixed: an insertion shifts every later block; measured on 1 GiB.
          const insSize = form === 'fixed' ? Math.min(N, GiB) : N;
          let g = f;
          if (form === 'fixed') {
            g = await t.create(ROOT, 'g');
            await write(t, g, keystream(3, 0, insSize));
          }
          const at = Math.floor(insSize / 2) + MiB;
          const ie = await measured(w, () =>
            traffic(s, () => ufs.tree(id).edit(g, undefined, [{ offset: at, insert: ins! }])),
          );
          row(
            form,
            `100-byte insertion at the middle of ${mib(insSize)} MiB`,
            `${ie.ms.toFixed(0)} ms; ${counts(ie.result[1])}; ${ie.mem}`,
          );
          if (form === 'fixed') {
            const got = await readBack(ufs.tree(id), g);
            expect(got).toEqual(
              await digest(partsOf(edited([{ seed: 3, from: 0, to: insSize }], at, 0, ins!))),
            );
            await drop(t, g);
          } else parts = edited(parts, at, 0, ins!);
          expect(await readBack(ufs.tree(id), f)).toEqual(await digest(partsOf(parts)));
          await drop(t, f);

          // 6. A resume past the grace period: the source fails at half.
          const b = await t.create(ROOT, 'b');
          let last: UploadState | undefined;
          await expect(
            t.upload(b, failing(2, 0, N, N / 2), {
              session: true,
              onProgress: (st) => {
                last = structuredClone(st);
              },
            }),
          ).rejects.toThrow('source lost');
          await sleep((2 * GRACE + 2) * 1000);
          const rs = await measured(w, () =>
            traffic(s, async () => {
              const u = await t.upload(b, keystream(2, last!.size, N), { resume: last });
              await t.send([await t.writeOp(u)]);
              await t.closeUpload(u.session!);
            }),
          );
          expect(await readBack(ufs.tree(id), b)).toEqual(await digest(keystream(2, 0, N)));
          row(
            form,
            `resume at ${mib(last!.size)} MiB, ${2 * GRACE + 2} s later (grace ${GRACE} s)`,
            `${rate(N - last!.size, rs.ms)} MiB/s; sent ${mib(rs.result[1].sent)} MiB for the ` +
              `${mib(N - last!.size)} MiB left; state ${(stateBytes(last!) / 1024).toFixed(1)} KiB`,
          );
          await drop(t, b);
          // Without a session, the first half is gone by then.
          const c = await t.create(ROOT, 'c');
          let plain: UploadState | undefined;
          await expect(
            t.upload(c, failing(4, 0, 64 * MiB, 32 * MiB), {
              onProgress: (st) => {
                plain = structuredClone(st);
              },
            }),
          ).rejects.toThrow('source lost');
          await sleep((2 * GRACE + 2) * 1000);
          const refused = await t
            .upload(c, keystream(4, plain!.size, 64 * MiB), { resume: plain })
            .then(async (u) => t.send([await t.writeOp(u)]))
            .then(
              () => 'written',
              (e: { status?: number; code?: string }) => `refused (${e.status} ${e.code})`,
            );
          expect(refused).toMatch(/^refused/);
          row(form, 'the same at 64 MiB without a session', refused);

          // 7. Pipelining: 1 GiB at 1 and 3 commits in flight.
          const p = await t.create(ROOT, 'p');
          const pn = Math.min(N, GiB);
          for (const parallel of [1, 3]) {
            const r = await measured(w, () =>
              write(t, p, keystream(5 + parallel, 0, pn), parallel),
            );
            row(
              form,
              `write ${mib(pn)} MiB, ${parallel} in flight`,
              `${rate(pn, r.ms)} MiB/s; ${r.mem}`,
            );
          }
          await drop(t, p);
          ufs.close();
        } finally {
          await w.server.stop();
        }
      },
      HOURS,
    );
  }

  it(
    'the server under 16 writers at once',
    async () => {
      checkDisk(3 * N, 'the client bench');
      const { w, ufs } = await openWorld();
      try {
        const id = Tree.newId();
        // One device, so one clock: each session stamping on its own would reuse timestamps.
        const clock = new TreeClock();
        const writers = await Promise.all(
          Array.from({ length: 16 }, async (_, i) => {
            const client = await connect(w.server.url, { origin: w.client.origin });
            const session = await client.signInDevice(w.admin);
            const u = await session.fs(1).unlock({ device: w.admin.device });
            const t = u.tree(id, { clock });
            return { u, t, node: await t.create(ROOT, `w${i}`) };
          }),
        );
        const each = Math.floor(N / 16);
        const r = await measured(w, () =>
          Promise.all(writers.map((x, i) => write(x.t, x.node, keystream(20 + i, 0, each)))),
        );
        row(
          'cdc',
          `16 writers of ${mib(each)} MiB at once`,
          `${rate(16 * each, r.ms)} MiB/s in all; ${r.mem}`,
        );
        const check = ufs.tree(id);
        for (const [i, x] of writers.entries()) {
          if (i % 5 === 0)
            expect(await readBack(check, x.node)).toEqual(await digest(keystream(20 + i, 0, each)));
          x.u.close();
        }
        ufs.close();
      } finally {
        await w.server.stop();
      }
    },
    HOURS,
  );

  it.skipIf(FDB)(
    'the chunker at the defaults: speed, mean chunk, capacity, resync',
    async () => {
      const { w, ufs } = await openWorld();
      try {
        const mvb = (await w.client.info()).limits.max_value_bytes;
        const params = zw.cdcDefaults(mvb) as CdcParams;
        const keys = ufs.sealKeys();
        // Speed and the mean chunk, over 1 GiB in 1 MiB pieces.
        const total = Math.min(N, GiB);
        const ch = keys.chunker(params);
        let ncuts = 0;
        const t0 = performance.now();
        for (const piece of keystream(30, 0, total)) ncuts += ch.push(piece).length;
        const ms = performance.now() - t0;
        ch.finish();
        ch.free();
        const mean = total / (ncuts + 1);
        const perPage = zw.pageCapacity2(mvb);
        // The writer's own bound on pages per version (writer.ts `forms`).
        const maxPages = Math.floor((mvb - emptyPagedV3(ufs, params)) / 72);
        row(
          'cdc',
          `the chunker (min ${params.min}, avg 2^${params.avgBits}, max ${params.max})`,
          `${rate(total, ms)} MiB/s; mean chunk ${(mean / 1024).toFixed(1)} KiB; ` +
            `${perPage} chunks a page, ${maxPages} pages a version: ` +
            `${((maxPages * perPage * mean) / GiB).toFixed(0)} GiB full, ` +
            `${((maxPages * perPage * mean) / 2 / GiB).toFixed(0)} GiB at the fill rule's worst`,
        );
        // Resync: 1,000 random insertions of 1-100 bytes into 4 MiB buffers.
        const rnd = prng(7);
        const hist = new Map<number, number>();
        // A chunk is unchanged when the old content has one at the same bytes:
        // the same offsets before the insertion, shifted by its length after.
        const cuts = (d: Uint8Array) => {
          const c = keys.chunker(params);
          const ends = [...c.push(d)];
          c.finish();
          c.free();
          if (ends.at(-1) !== d.length) ends.push(d.length);
          return ends;
        };
        for (let buf = 0; buf < 20; buf++) {
          const [data] = keystream(40 + buf, 0, 4 * MiB);
          const old = new Set<string>();
          let p = 0;
          for (const e of cuts(data!)) {
            old.add(`${p}:${e}`);
            p = e;
          }
          for (let k = 0; k < 50; k++) {
            const at = Math.floor(rnd() * data!.length);
            const n = 1 + Math.floor(rnd() * 100);
            const next = bytes.concat(
              data!.subarray(0, at),
              crypto.getRandomValues(new Uint8Array(n)),
              data!.subarray(at),
            );
            let changed = 0;
            let q = 0;
            for (const e of cuts(next)) {
              const same =
                e <= at ? old.has(`${q}:${e}`) : q >= at + n && old.has(`${q - n}:${e - n}`);
              if (!same) changed++;
              q = e;
            }
            hist.set(changed, (hist.get(changed) ?? 0) + 1);
          }
        }
        const keysSorted = [...hist.keys()].sort((a, b) => a - b);
        row(
          'cdc',
          '1,000 insertions of 1-100 B into 4 MiB: chunks changed',
          keysSorted.map((k) => `${k}: ${((hist.get(k)! / 1000) * 100).toFixed(1)} %`).join(', '),
        );
        ufs.close();
      } finally {
        await w.server.stop();
      }
    },
    HOURS,
  );
});

/** The sealed size of an empty paged v3 manifest (the writer's fixed overhead per version). */
function emptyPagedV3(ufs: UnlockedFs, cdc: CdcParams): number {
  return ufs.sealKeys().sealManifestV3(Tree.newId(), Tree.newId(), {
    version: 3,
    size: 0,
    n: 0,
    cdc,
    pageSize: 4499,
    pages: [],
  }).length;
}
