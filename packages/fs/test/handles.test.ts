// File handles of the replica (fs.md §11.1): bounded memory, flushes as
// edits that seal only what changed, the queue across a crash, close with
// wait, the index records, and zeros.
import { createHash } from 'node:crypto';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setFlagsFromString } from 'node:v8';
import { runInNewContext } from 'node:vm';
import {
  bytes,
  chunkHash,
  chunkId,
  connect,
  memoryLocalStore,
  ROOT,
  type SealedStore,
  sealedStore,
  Tree,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import type { CdcParams } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type Replica, type ReplicaOptions } from '../src/index.js';
import { decodeJson, encodeJson } from '../src/queue.js';
import { type Net, watch } from './helpers.js';

const CDC: CdcParams = { min: 256, avgBits: 10, max: 8128 };
const { concat, equal } = bytes;

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

const solo = (local: SealedStore, more: Partial<ReplicaOptions> = {}): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 200,
  ...more,
});

/** Another installation of the admin on fs `fs`: its own session, watched. */
async function installation(w: World, fs: number): Promise<{ ufs: UnlockedFs; net: Net }> {
  const client = await connect(w.server.url, { origin: w.client.origin });
  const session = await client.signInDevice(w.admin);
  const net = watch(session);
  const ufs = await session.fs(fs).unlock({ device: w.admin.device });
  return { ufs, net };
}

/** What the commits since `net.reset()` put, and the chunks fetched. */
function traffic(net: Net) {
  let chunks = 0;
  let pages = 0;
  let fetched = 0;
  for (const c of net.calls) {
    const req = c.req as { chunks?: unknown[]; pages?: unknown[]; ids?: unknown[] };
    if (c.path === '/v1/commit') {
      chunks += req.chunks?.length ?? 0;
      pages += req.pages?.length ?? 0;
    }
    if (c.path === '/v1/fs/chunks/get') fetched += req.ids?.length ?? 0;
  }
  return { chunks, pages, fetched };
}

describe('the memory bound', () => {
  let w: World;
  let dir: string;
  beforeAll(async () => {
    w = await world();
    (
      await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)])
    ).close();
    dir = await mkdtemp(join(tmpdir(), 'zen-handles-'));
  });
  afterAll(async () => {
    await w?.server.stop();
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it('128 MiB through one handle keeps at most the bound in memory', async () => {
    const id = Tree.newId();
    // Not watched: the calls kept would hold every chunk sent.
    const client = await connect(w.server.url, { origin: w.client.origin });
    const ufs = await (await client.signInDevice(w.admin)).fs(1).unlock({ device: w.admin.device });
    const r = await openReplica(
      ufs,
      id,
      solo(sealedStore(nodeDiskLocalStore(dir)), { cacheBytes: 16 << 20, dirtyBytes: 4 << 20 }),
    );
    await r.sync();
    // A collection before each sample, so garbage doesn't count as growth.
    setFlagsFromString('--expose-gc');
    const gc = runInNewContext('gc') as () => void;
    gc();
    const rss0 = process.memoryUsage().rss;
    let peak = 0;
    let dirty = 0;
    let spills = 0;
    const samples: number[] = [];
    const h = await r.open('/big.bin', 'w');
    const sum = createHash('sha256');
    const W = 128 << 10;
    const piece = random(W);
    for (let i = 0; i < 1024; i++) {
      // Each write differs (its first bytes), so nothing is reused.
      new DataView(piece.buffer).setUint32(0, i);
      await h.write(piece, i * W);
      sum.update(piece);
      dirty = Math.max(dirty, h.dirtyBytes);
      if (h.dirtyBytes === 0) spills++;
      if (i % 32) continue;
      gc();
      const rss = process.memoryUsage().rss - rss0;
      peak = Math.max(peak, rss);
      samples.push(rss);
    }
    // Fixed costs (wasm memory, caches, the HTTP client) come first; what
    // grows with the file shows as a trend: the last quarter's samples
    // against the second quarter's, by their medians (one noisy sample on
    // either side moves neither).
    const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[xs.length >> 1]!;
    const q = samples.length / 4;
    const late = median(samples.slice(3 * q)) - median(samples.slice(q, 2 * q));
    await h.close({ wait: true });
    await r.flush();
    expect(dirty).toBeLessThanOrEqual((4 << 20) + W);
    const mib = (x: number) => (x / 2 ** 20).toFixed(1);
    console.log(
      `handles: RSS growth ${mib(peak)} MiB, ${mib(late)} from the second quarter to the last; ${spills} spills`,
    );
    expect(peak).toBeLessThan(160 << 20);
    // Past the middle a few bounded costs still grow: the queued entry's
    // local version lists the inline file's chunks so far, and the page memo
    // fills toward its bound. With the allocator's noise that measured 2–21
    // MiB here and 34–38 MiB on CI runners; with the dirty bound off, 64.
    expect(late).toBeLessThan(48 << 20);
    // Each spill queued a write of its own.
    expect(spills).toBeGreaterThanOrEqual(30);
    expect((await r.tree.versions((await r.nodeAt('/big.bin'))!.id)).length).toBe(1);
    await r.close();

    // A fresh replica reads it back.
    const other = await installation(w, 1);
    const view = await openReplica(other.ufs, id, solo(sealedStore(memoryLocalStore())));
    await view.sync();
    const back = createHash('sha256');
    let size = 0;
    const rd = (await view.createReadStream('/big.bin')).getReader();
    for (let x = await rd.read(); !x.done; x = await rd.read()) {
      back.update(x.value);
      size += x.value.length;
    }
    expect(size).toBe(1024 * W);
    expect(back.digest('hex')).toBe(sum.digest('hex'));
    await view.close();
  }, 300_000);
});

describe('small values', () => {
  let w: World;
  let fs1: UnlockedFs;
  let fs2: UnlockedFs;
  beforeAll(async () => {
    // A page holds 409 chunks of about 1 KiB: a 2 MB file has 5 pages.
    w = await world({ limits: { max_value_bytes: 8192, max_commit_bytes: 200_000 } });
    fs1 = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    fs2 = await w.session.fs(2).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    await fs2.setPolicy({ chunking: 'fixed' });
  });
  afterAll(() => w?.server.stop());

  /** A fresh tree on fs `fs`, a replica of it on another installation, and the admin's view. */
  async function setup(fs = 1, more: Partial<ReplicaOptions> = {}) {
    const id = Tree.newId();
    const ufs = fs === 1 ? fs1 : fs2;
    const t = ufs.tree(id, { cdc: CDC });
    await t.mkdir(ROOT, '.keep');
    const inst = await installation(w, fs);
    const local = sealedStore(memoryLocalStore());
    const r = await openReplica(inst.ufs, id, solo(local, { cdc: CDC, ...more }));
    await r.sync();
    return { id, t, r, local, net: inst.net, ufs: inst.ufs };
  }

  const fileOf = async (r: Replica, path: string) => (await r.nodeAt(path))!.id;

  it.each([
    ['cdc', 1],
    ['fixed', 2],
  ])(
    'random operations match a model (%s)',
    async (form, fs) => {
      const { t, r } = await setup(fs, { dirtyBytes: 64 << 10 });
      let model = random(1_500_000);
      let h = await r.open('/m.bin', 'w');
      await h.write(model, 0);
      await h.flush({ wait: true });
      const node = await fileOf(r, '/m.bin');
      let seed = fs * 7919;
      const rnd = (n: number) => {
        seed = (seed * 1103515245 + 12345) >>> 0;
        return seed % Math.max(1, n);
      };
      let flushes = 0;
      for (let i = 0; i < 300; i++) {
        const k = rnd(100);
        if (k < 40) {
          const pos = rnd(model.length + 4096);
          const data = random(1 + rnd(40_000));
          if (pos > model.length) model = concat(model, new Uint8Array(pos - model.length));
          const next = model.length < pos + data.length ? new Uint8Array(pos + data.length) : model;
          if (next !== model) next.set(model);
          next.set(data, pos);
          model = next;
          await h.write(data, pos);
        } else if (k < 50 || model.length > 3_000_000) {
          const len = rnd(Math.floor(model.length * 1.2) + 10_000);
          model =
            len <= model.length
              ? model.slice(0, len)
              : concat(model, new Uint8Array(len - model.length));
          await h.truncate(len);
        } else if (k < 60) {
          const data = random(1 + rnd(20_000));
          await h.write(data, model.length);
          model = concat(model, data);
        } else if (k < 80) {
          const pos = rnd(model.length + 100);
          const len = rnd(50_000);
          expect(equal(await h.read(len, pos), model.slice(pos, pos + len))).toBe(true);
        } else if (k < 92) {
          // The handle's write, and any queued before it (a close without wait).
          await h.flush({ wait: true });
          await r.flush();
          flushes++;
          expect(equal(await t.readFile(node), model)).toBe(true);
        } else {
          await h.close();
          h = await r.open('/m.bin', 'r+');
          expect(h.size).toBe(model.length);
        }
        expect(h.size).toBe(model.length);
      }
      await h.close({ wait: true });
      await r.flush();
      expect(flushes).toBeGreaterThan(10);
      expect(equal(await t.readFile(node), model)).toBe(true);
      const [v] = await t.versions(node);
      expect(v!.manifest.version).toBe(form === 'cdc' ? 3 : 2);
      // A second replica reads the same.
      const other = await installation(w, fs);
      const view = await openReplica(other.ufs, r.id, solo(sealedStore(memoryLocalStore())));
      await view.sync();
      expect(equal(await view.readFile('/m.bin'), model)).toBe(true);
      await view.close();
      await r.close();
    },
    300_000,
  );

  /** The chunks of content as the fs's chunker cuts it (hex of their bytes). */
  const cuts = (d: Uint8Array) => {
    const ch = fs1.sealKeys().chunker(CDC);
    const out: string[] = [];
    let p = 0;
    for (const c of ch.push(d)) {
      out.push(bytes.hex(d.subarray(p, c)));
      p = c;
    }
    ch.finish();
    ch.free();
    if (p < d.length) out.push(bytes.hex(d.subarray(p)));
    return out;
  };
  const diff = (before: Uint8Array, after: Uint8Array) => {
    const a = cuts(before);
    const b = cuts(after);
    const old = new Set(a);
    const now = new Set(b);
    return {
      added: b.filter((c) => !old.has(c)).length,
      removed: a.filter((c) => !now.has(c)).length,
    };
  };

  it('an edit through a handle seals only the chunks it changes, in at most 2 pages', async () => {
    const { t, r, net } = await setup();
    const data = random(2_000_000);
    const f = await t.create(ROOT, 'e.bin');
    await t.writeFile(f, data);
    const [v0] = await t.versions(f);
    expect(v0!.manifest.version).toBe(3);
    expect(v0!.pages.length).toBeGreaterThanOrEqual(4);
    await r.sync();

    // A 1-byte overwrite mid-file.
    const at = 1_000_003;
    const next = data.slice();
    next[at]! ^= 0xff;
    let h = await r.open('/e.bin', 'r+');
    net.reset();
    await h.write(next.subarray(at, at + 1), at);
    await h.close({ wait: true });
    const one = traffic(net);
    const d1 = diff(data, next);
    expect(one.pages).toBeLessThanOrEqual(2);
    expect(one.chunks).toBe(d1.added);
    // Only the chunks the edit re-cuts are read.
    expect(one.fetched).toBe(d1.removed);
    expect(equal(await t.readFile(f), next)).toBe(true);

    // A 1-byte append: the last page and the re-cut tail.
    h = await r.open('/e.bin', 'r+');
    net.reset();
    await h.write(new Uint8Array([7]), next.length);
    await h.close({ wait: true });
    const after = concat(next, new Uint8Array([7]));
    const two = traffic(net);
    const d2 = diff(next, after);
    expect(two.pages).toBe(1);
    expect(two.chunks).toBe(d2.added);
    expect(two.fetched).toBe(d2.removed);
    expect(equal(await t.readFile(f), after)).toBe(true);
    const [v] = await t.versions(f);
    expect(v!.manifest.version).toBe(3);
    await r.close();
  });

  it('close({wait}) resolves with the write on the server; a handle flushed with wait goes on', async () => {
    const { t, r } = await setup();
    const h = await r.open('/w.txt', 'w');
    await h.write(bytes.utf8('one'), 0);
    await h.close({ wait: true });
    const node = await fileOf(r, '/w.txt');
    expect(bytes.fromUtf8(await t.readFile(node))).toBe('one');

    const g = await r.open('/w.txt', 'r+');
    await g.write(bytes.utf8('two'), 3);
    await g.flush({ wait: true });
    expect(bytes.fromUtf8(await t.readFile(node))).toBe('onetwo');
    await g.write(bytes.utf8('!'), 6);
    await g.flush({ wait: true });
    expect(bytes.fromUtf8(await t.readFile(node))).toBe('onetwo!');
    await g.close({ wait: true });
    expect((await t.versions(node)).length).toBe(1);
    await r.close();
  });

  it('index records: a rewrite reuses chunks without reading them; old records still serve', async () => {
    const { id, t, r, local, net } = await setup();
    const reads = vi.spyOn(r.tree, 'chunkIndex');
    const data = random(1_200_000);
    await r.writeFile('/p.bin', data, { wait: true });
    const node = await fileOf(r, '/p.bin');
    expect((await t.versions(node))[0]!.pages.length).toBeGreaterThanOrEqual(2);

    // The records of what the replica wrote: no read, few chunks sent.
    const edit = (d: Uint8Array, at: number) => {
      const out = d.slice();
      out[at]! ^= 0xff;
      return out;
    };
    let cur = edit(data, 600_000);
    net.reset();
    await r.writeFile('/p.bin', cur, { wait: true });
    expect(reads).toHaveBeenCalledTimes(0);
    expect(traffic(net).chunks).toBeLessThanOrEqual(12);

    // An edit through a handle writes no records: the next rewrite reads once…
    const h = await r.open('/p.bin', 'r+');
    cur = edit(cur, 10);
    await h.write(cur.subarray(10, 11), 10);
    await h.close({ wait: true });
    cur = edit(cur, 900_000);
    await r.writeFile('/p.bin', cur, { wait: true });
    expect(reads).toHaveBeenCalledTimes(1);
    // …and keeps the records of what it read.
    cur = edit(cur, 300_000);
    await r.writeFile('/p.bin', cur, { wait: true });
    expect(reads).toHaveBeenCalledTimes(1);
    expect(equal(await t.readFile(node), cur)).toBe(true);

    // A legacy record (the whole index as JSON, by node and dot) is used.
    const f = await t.create(ROOT, 'legacy.bin');
    const old = random(900_000);
    await t.writeFile(f, old);
    await r.sync();
    const [v] = await t.versions(f);
    const ix = await t.chunkIndex(f, v!);
    await local.put(`zfs/1/${bytes.hex(id)}/index`, concat(f, v!.dot), encodeJson(ix));
    net.reset();
    await r.writeFile('/legacy.bin', edit(old, 5), { wait: true });
    expect(reads).toHaveBeenCalledTimes(1);
    expect(traffic(net).fetched).toBe(0);
    await r.close();
  });

  it('a queue entry of the earlier format (with its whole index) still loads and serves', async () => {
    const { id, r, local, net, ufs } = await setup();
    const data = random(600_000);
    net.down = true;
    await r.writeFile('/q.bin', data);
    const node = await fileOf(r, '/q.bin');
    const v = (await r.shown(await r.lookup('/q.bin')))!;
    const ix = await r.tree.chunkIndex(node, v, data);
    // The entry as earlier replicas wrote it, and no index records.
    const qs = `zfs/1/${bytes.hex(id)}/queue`;
    let rewrote = 0;
    for await (const [k, raw] of local.list(qs)) {
      if (k.length !== 13) continue;
      const e = decodeJson<{ v: number; ops: { op: string; index?: unknown }[] }>(raw);
      // Earlier replicas kept the index in its array form.
      const { n, ids: _ids, lens, hashes: _h, ...rest } = ix;
      const legacy = {
        ...rest,
        chunks: Array.from({ length: n }, (_, i) => chunkId(ix, i).slice()),
        ...(lens ? { lens: [...lens] } : {}),
        hashes: Array.from({ length: n }, (_, i) => chunkHash(ix, i)!.slice()),
      };
      for (const o of e.ops) if (o.op === 'write') o.index = legacy;
      await local.put(qs, k, encodeJson(e));
      rewrote++;
    }
    expect(rewrote).toBe(1);
    await r.close();
    await local.clear(`zfs/1/${bytes.hex(id)}/index`);

    const again = await openReplica(ufs, id, solo(local, { cdc: CDC }));
    const reads = vi.spyOn(again.tree, 'chunkIndex');
    expect((await again.status()).queued).toBe(1);
    const lv = (await again.shown(await again.lookup('/q.bin')))!;
    expect(await again.indexOf(node, lv)).toEqual(ix);
    await again.writeFile('/q.bin', concat(data, new Uint8Array([1])));
    expect(reads).toHaveBeenCalledTimes(0);
    net.down = false;
    await again.flush();
    const t = again.fs.tree(id);
    expect(equal(await t.readFile(node), concat(data, new Uint8Array([1])))).toBe(true);
    await again.close();
  });

  it('zeros: a file extended by truncate holds no buffer for them', async () => {
    const { t, r } = await setup();
    const h = await r.open('/z.bin', 'w');
    await h.write(bytes.utf8('head'), 0);
    await h.truncate(64 << 20);
    expect(h.dirtyBytes).toBe(4);
    await h.write(bytes.utf8('tail'), (64 << 20) - 4);
    expect(h.dirtyBytes).toBe(8);
    await h.close({ wait: true });
    const node = await fileOf(r, '/z.bin');
    const back = await t.readFile(node);
    expect(back.length).toBe(64 << 20);
    expect(bytes.fromUtf8(back.subarray(0, 4))).toBe('head');
    expect(bytes.fromUtf8(back.subarray(back.length - 4))).toBe('tail');
    expect(back.subarray(4, back.length - 4).every((b) => b === 0)).toBe(true);

    // And a file grown by an edit: the zeros are streamed into it.
    const g = await r.open('/z.bin', 'r+');
    await g.truncate(80 << 20);
    expect(g.dirtyBytes).toBe(0);
    await g.close({ wait: true });
    const more = await t.readFile(node);
    expect(more.length).toBe(80 << 20);
    expect(more.subarray(64 << 20).every((b) => b === 0)).toBe(true);
    await r.close();
  }, 120_000);
});

describe('a crash mid-write', () => {
  let w: World;
  beforeAll(async () => {
    w = await world({ limits: { max_value_bytes: 8192, max_commit_bytes: 200_000 } });
    (
      await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)])
    ).close();
  });
  afterAll(() => w?.server.stop());

  it('what spilled past the bound is queued, and sent after a restart', async () => {
    const id = Tree.newId();
    const admin = await w.session.fs(1).unlock({ device: w.admin.device });
    await admin.tree(id).mkdir(ROOT, '.keep');
    const { ufs } = await installation(w, 1);
    const local = sealedStore(memoryLocalStore());
    const opts = solo(local, { cdc: CDC, dirtyBytes: 64 << 10 });
    const r = await openReplica(ufs, id, opts);
    await r.sync();
    await r.writeFile('/c.bin', new Uint8Array(0), { wait: true });
    const node = (await r.nodeAt('/c.bin'))!.id;

    await w.server.kill();
    const h = await r.open('/c.bin', 'r+');
    const data = random(16 << 10);
    let written = 0;
    let spilled = 0;
    let spills = 0;
    while (spills < 3) {
      new DataView(data.buffer).setUint32(0, written);
      await h.write(data, written);
      written += data.length;
      if (h.dirtyBytes === 0) {
        spills++;
        spilled = written;
      }
    }
    // One more write, kept in memory only.
    await h.write(data, written);
    expect(h.dirtyBytes).toBeGreaterThan(0);
    expect((await r.status()).queued).toBe(3);
    const expected = await h.read(spilled, 0);
    // A crash: the handle is never closed, its memory lost.
    await r.close();

    w.server = await w.server.restart();
    const again = await openReplica(ufs, id, opts);
    await again.sync();
    await again.flush();
    const t = admin.tree(id);
    expect((await t.versions(node)).length).toBe(1);
    expect(equal(await t.readFile(node), expected)).toBe(true);
    await again.close();
    admin.close();
  }, 120_000);
});
