// Content-defined chunks (version-3 manifests, form-2 pages), edits that
// cost the pages they touch, pipelined uploads and upload sessions
// (formats.md §11.3, §11.6, §11.7; fs.md §4.1; api.md §12.1). The server's
// max_value_bytes is 8,192 and the chunker's average 1 KiB, so a page holds
// 409 chunks and a file of a few MB spans several pages.
import type { CdcParams, Info, ManifestV3, PageRefV3 } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  bytes,
  ROOT,
  type Session,
  Tree,
  type UnlockedFs,
  type UploadState,
  ZenError,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

const { equal, concat } = bytes;
const CDC: CdcParams = { min: 256, avgBits: 10, max: 8128 };
const PS = 409; // pageCapacity2(8192)

let w: World;
let ufs: UnlockedFs;
beforeAll(async () => {
  w = await world({ limits: { max_value_bytes: 8192, max_commit_bytes: 200_000 } });
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const tree = (id = Tree.newId(), fs = ufs) => fs.tree(id, { cdc: CDC });
let names = 0;
const name = () => `f${names++}`;

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

async function* pieces(data: Uint8Array, size: number) {
  for (let i = 0; i < data.length; i += size) yield data.subarray(i, i + size);
}

function streamOf(data: Uint8Array): ReadableStream<Uint8Array> {
  let i = 0;
  return new ReadableStream({
    pull(ctl) {
      if (i >= data.length) ctl.close();
      else {
        ctl.enqueue(data.slice(i, i + 50_000));
        i += 50_000;
      }
    },
  });
}

async function collect(s: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const parts: Uint8Array[] = [];
  const r = s.getReader();
  for (let x = await r.read(); !x.done; x = await r.read()) parts.push(x.value);
  return concat(...parts);
}

/** Count the calls to each path while `fn` runs; `edit` may change a request's outcome. */
async function calls<T>(
  s: Session,
  fn: () => Promise<T>,
  edit?: (path: string, n: number, r: () => Promise<unknown>) => Promise<unknown>,
): Promise<{ result: T; count: Map<string, number> }> {
  const orig = s.call.bind(s);
  const count = new Map<string, number>();
  s.call = (async (path: string, enc: never, dec: never, req: never) => {
    const n = (count.get(path) ?? 0) + 1;
    count.set(path, n);
    const go = () => orig(path, enc, dec, req);
    return edit ? edit(path, n, go) : go();
  }) as Session['call'];
  try {
    return { result: await fn(), count };
  } finally {
    s.call = orig;
  }
}

const v3 = (m: { version: number }) => m as ManifestV3;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Every short page but the last is followed by one with which it holds more than a page. */
function filled(m: ManifestV3) {
  const counts = m.pages!.map((p) => p.count);
  for (let i = 0; i + 1 < counts.length; i++) {
    if (2 * counts[i]! < PS) expect(counts[i]! + counts[i + 1]!).toBeGreaterThan(PS);
  }
  expect(counts.length).toBeLessThanOrEqual(Math.ceil((2 * m.n) / PS) + 1);
}

describe('content-defined chunks (version 3)', () => {
  it('round-trips inline and paged from bytes, a generator and a web stream', async () => {
    const t = tree();
    for (const n of [5_000, 1_200_000]) {
      const data = random(n);
      for (const src of [data, pieces(data, 7777), streamOf(data)]) {
        const f = await t.create(ROOT, name());
        const r = await t.writeFile(f, src);
        const m = v3(r.manifest);
        expect(m.version).toBe(3);
        expect(m.cdc).toEqual(CDC);
        expect(m.size).toBe(n);
        const lens = r.index!.lens!;
        expect(lens.length).toBe(m.n);
        lens.slice(0, -1).forEach((l) => {
          expect(l).toBeGreaterThanOrEqual(CDC.min);
          expect(l).toBeLessThanOrEqual(CDC.max);
        });
        if (n === 5_000) expect(m.chunks!.length).toBe(m.n);
        else expect(m.pages!.length).toBeGreaterThanOrEqual(2);
        expect(equal(await tree(t.id).readFile(f), data)).toBe(true);
      }
    }
    // The server keeps form-2 pages: ids and lengths, as the manifest says.
    const f = await t.create(ROOT, name());
    const r = await t.writeFile(f, random(1_200_000));
    const ref = v3(r.manifest).pages![0]!;
    const page = (
      await w.session.call('/v1/fs/pages/get', zw.encodePagesGet, zw.decodePages, {
        fs: 1,
        ids: [ref.id],
      })
    ).pages[0]!.data!;
    const d = zw.decodeAnyPage(page);
    expect(d.ids.length).toBe(ref.count);
    expect(d.lens!.reduce((a, b) => a + b, 0)).toBe(ref.bytes);
  });

  it('an insertion re-uploads only the chunks it changes, and at most 2 pages', async () => {
    const t = tree();
    const f = await t.create(ROOT, name());
    const data = random(1_200_000);
    await t.writeFile(f, data);
    // The chunks of some content, as the chunker cuts them (by their bytes).
    const cuts = (d: Uint8Array) => {
      const ch = ufs.sealKeys().chunker(CDC);
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
    for (const at of [600_000, 10, 1_199_000]) {
      const prev = await t.versions(f);
      const v = prev[0]!;
      const idx = await t.chunkIndex(f, v);
      const before = await t.readFile(f);
      const next = concat(before.subarray(0, at), random(100), before.subarray(at));
      const old = new Set(cuts(before));
      const changed = cuts(next).filter((c) => !old.has(c)).length;
      // The cuts resync within a few chunks (one in most cases).
      expect(changed).toBeGreaterThanOrEqual(1);
      expect(changed).toBeLessThanOrEqual(12);
      const w2 = await t.writeFile(f, next, { previous: idx, replaces: [v.dot] });
      expect(w2.uploaded).toBe(changed);
      expect(w2.pagesUploaded).toBeLessThanOrEqual(2);
      expect(equal(await tree(t.id).readFile(f), next)).toBe(true);
    }
  });

  it('an append fetches only the last page and its last chunk (v3 and v2)', async () => {
    for (const chunking of ['cdc', 'fixed'] as const) {
      const t = tree();
      const f = await t.create(ROOT, name());
      const data = random(chunking === 'cdc' ? 1_200_000 : 5_000_000);
      const w1 = await t.writeFile(f, data, { chunking });
      expect(w1.manifest.pages!.length).toBeGreaterThanOrEqual(2);
      const more = random(5_000);
      const fresh = tree(t.id);
      const r = await calls(w.session, () => fresh.append(f, more));
      expect(r.count.get('/v1/fs/pages/get')).toBe(1);
      expect(r.count.get('/v1/fs/chunks/get')).toBe(1);
      expect(r.result.manifest.version).toBe(chunking === 'cdc' ? 3 : 2);
      expect(r.result.uploaded).toBeLessThanOrEqual(chunking === 'cdc' ? 1 + 5_000 / CDC.min : 2);
      expect(r.result.pagesUploaded).toBe(1);
      expect(r.result.index).toBeUndefined();
      expect(equal(await tree(t.id).readFile(f), concat(data, more))).toBe(true);
    }
  });

  it('an edit fetches and uploads only the pages it touches', async () => {
    const t = tree();
    const f = await t.create(ROOT, name());
    let model = random(2_000_000);
    await t.writeFile(f, model);
    const apply = (offset: number, remove: number, insert: Uint8Array) => {
      model = concat(model.subarray(0, offset), insert, model.subarray(offset + remove));
    };
    for (const [offset, remove, insert] of [
      [900_000, 100, random(100)], // overwrite
      [500_000, 0, random(3_000)], // insertion
      [1_500_000, 4_000, new Uint8Array(0)], // deletion
    ] as const) {
      const fresh = tree(t.id);
      const r = await calls(w.session, () =>
        fresh.edit(f, undefined, [{ offset, remove, insert }]),
      );
      apply(offset, remove, insert);
      // The touched page, and the chunks until the cuts resync (a few, at
      // most about 12 in a probe of 300 insertions): never the whole file.
      expect(r.count.get('/v1/fs/pages/get')).toBeLessThanOrEqual(2);
      expect(r.count.get('/v1/fs/chunks/get') ?? 0).toBeLessThanOrEqual(14);
      expect(r.result.pagesUploaded).toBeLessThanOrEqual(2);
      expect(r.result.uploaded).toBeLessThanOrEqual(14 + Math.ceil(insert.length / CDC.min));
      expect(equal(await tree(t.id).readFile(f), model)).toBe(true);
    }
    const m = v3((await t.versions(f))[0]!.manifest);
    expect(m.pages!.some((p) => p.count < PS)).toBe(true);
    filled(m);

    // Many edits at random places, several per write, keep the pages filled.
    for (let round = 0; round < 6; round++) {
      const edits = [];
      let at = 0;
      for (let k = 0; k < 4; k++) {
        const offset = at + Math.floor(Math.random() * 200_000);
        if (offset + 2_000 > model.length) break;
        const remove = Math.floor(Math.random() * 2_000);
        edits.push({ offset, remove, insert: random(Math.floor(Math.random() * 2_000)) });
        at = offset + remove;
      }
      await t.edit(f, undefined, edits);
      for (const e of [...edits].reverse()) apply(e.offset, e.remove, e.insert);
      expect(equal(await tree(t.id).readFile(f), model)).toBe(true);
      filled(v3((await t.versions(f))[0]!.manifest));
    }

    // Truncating, and an edit of a file with no content.
    await t.edit(f, undefined, [{ offset: 300_000, remove: model.length - 300_000 }]);
    expect(equal(await t.readFile(f), model.subarray(0, 300_000))).toBe(true);
    const g = await t.create(ROOT, name());
    await t.append(g, model.subarray(0, 1000));
    expect(equal(await t.readFile(g), model.subarray(0, 1000))).toBe(true);
    await expect(
      t.edit(f, undefined, [{ offset: 10, remove: 5 }, { offset: 12 }]),
    ).rejects.toMatchObject({ code: 'bad_value' });
  });

  it('the fill rule merges a short page with a short neighbour', async () => {
    const t = tree();
    let f = await t.create(ROOT, name());
    let model = random(2_000_000);
    await t.writeFile(f, model);
    const starts = async () => {
      const m = v3((await t.versions(f))[0]!.manifest);
      const out = [0];
      for (const p of m.pages!) out.push(out.at(-1)! + p.bytes);
      return { m, out };
    };
    const cut = async (page: number) => {
      // Keep about a third of the page's chunks, whole: it is left short, and
      // the next page falls on a cut.
      const { out } = await starts();
      const v = (await t.versions(f))[0]!;
      const lens = (await t.chunkIndex(f, v, undefined, { hashes: false })).lens!;
      let from = 0;
      for (const l of lens) {
        if (from >= out[page]! + (out[page + 1]! - out[page]!) / 3) break;
        from += l;
      }
      const remove = out[page + 1]! - from;
      await t.edit(f, undefined, [{ offset: from, remove }]);
      model = concat(model.subarray(0, from), model.subarray(from + remove));
    };
    await cut(1);
    const once = (await starts()).m;
    expect(once.pages![1]!.count).toBeLessThan(PS / 2);
    // Page 0 left short too: it merges with the short page 1, unread so far.
    await cut(0);
    const { m } = await starts();
    expect(m.pages!.length).toBe(once.pages!.length - 1);
    expect(m.pages![0]!.count).toBeGreaterThanOrEqual(PS / 2);
    filled(m);
    expect(equal(await tree(t.id).readFile(f), model)).toBe(true);

    // A short page of the base whose next page an edit shrinks: the two
    // become one, though the short one isn't touched.
    f = await t.create(ROOT, name());
    model = random(2_000_000);
    await t.writeFile(f, model);
    await cut(1);
    const first = (await starts()).m;
    expect(first.pages![1]!.count).toBeLessThan(PS / 2);
    await cut(2);
    const again = (await starts()).m;
    expect(again.pages!.length).toBe(first.pages!.length - 1);
    filled(again);
    expect(equal(await tree(t.id).readFile(f), model)).toBe(true);
  });

  it('random reads and streams match the content, versions 1 to 3, and after a repack', async () => {
    const t = tree();
    const keys = ufs.sealKeys();
    // Version 1 (an old writer), version 2 (paged) and version 3 (paged).
    const one = await t.create(ROOT, name());
    const d1 = random(5 * 8128 + 7);
    const ids: Uint8Array[] = [];
    const puts = [];
    for (let i = 0; i < d1.length; i += 8128) {
      const id = crypto.getRandomValues(new Uint8Array(16));
      ids.push(id);
      puts.push({ fs: 1, id, data: keys.sealChunk(id, d1.subarray(i, i + 8128)) });
    }
    const manifest = keys.sealManifest(t.id, one, {
      size: d1.length,
      chunkSize: 8128,
      chunks: ids,
    });
    await t.send([{ op: 'write', node: one, replaces: [], chunks: ids, manifest }], puts);
    const two = await t.create(ROOT, name());
    const d2 = random(5_000_000);
    await t.writeFile(two, d2, { chunking: 'fixed' });
    const three = await t.create(ROOT, name());
    const d3 = random(2_000_000);
    await t.writeFile(three, d3);
    const files: [Uint8Array, Uint8Array, number][] = [
      [one, d1, 1],
      [two, d2, 2],
      [three, d3, 3],
    ];
    const check = async (f: Uint8Array, data: Uint8Array) => {
      const r = tree(t.id);
      const v = (await r.versions(f))[0]!;
      for (let i = 0; i < 100; i++) {
        const offset = Math.floor(Math.random() * (data.length + 100));
        const length = Math.floor(Math.random() * 30_000);
        const got = await r.read(f, v, offset, length);
        expect(equal(got, data.subarray(offset, offset + length))).toBe(true);
      }
      for (const readAhead of [0, 3]) {
        const offset = Math.floor(Math.random() * data.length);
        const length = Math.floor(Math.random() * 6_000_000);
        const got = await collect(r.readStream(f, { version: v, offset, length, readAhead }));
        expect(equal(got, data.subarray(offset, offset + length))).toBe(true);
      }
    };
    for (const [f, data, version] of files) {
      expect((await t.versions(f))[0]!.manifest.version).toBe(version);
      await check(f, data);
    }
    // A repack re-cuts every page full and reads back the same.
    await t.edit(three, undefined, [{ offset: 1000, remove: 10, insert: random(10) }]);
    const v = (await t.versions(three))[0]!;
    const cut = await t.read(three, v, 0, v.size);
    await t.edit(three, v, [], { repack: true });
    const m = v3((await t.versions(three))[0]!.manifest);
    expect(m.pages!.slice(0, -1).every((p) => p.count === PS)).toBe(true);
    await check(three, cut);
  });

  it('refuses pages that disagree with the manifest, and known items not of the base', async () => {
    const t = tree();
    const f = await t.create(ROOT, name());
    const data = random(1_200_000);
    const r = await t.writeFile(f, data);
    const m = v3(r.manifest);
    const keys = ufs.sealKeys();
    // A manifest whose first page claims one more byte, or one chunk fewer.
    for (const change of [
      (p: PageRefV3) => ({ ...p, bytes: p.bytes + 1 }),
      (p: PageRefV3) => ({ ...p, count: p.count - 1 }),
    ]) {
      const page = change(m.pages![0]!);
      const bad: ManifestV3 = {
        ...m,
        size: m.size + page.bytes - m.pages![0]!.bytes,
        n: m.n + page.count - m.pages![0]!.count,
        pages: [page, ...m.pages!.slice(1)],
      };
      const g = await t.create(ROOT, name());
      await t.send([
        {
          op: 'write',
          node: g,
          replaces: [],
          chunks: [],
          pages: m.pages!.map((p) => p.id),
          manifest: keys.sealManifestV3(t.id, g, bad),
        },
      ]);
      await expect(tree(t.id).read(g, undefined, 0, 10)).rejects.toMatchObject({ code: 'format' });
    }
    // A page swapped for another is refused.
    const swapped = await calls(
      w.session,
      () => tree(t.id).read(f, undefined, 0, 10),
      async (path, _n, go) => {
        const res = (await go()) as { pages: { data?: Uint8Array }[] };
        if (path === '/v1/fs/pages/get')
          res.pages[0]!.data = zw.encodePage2([{ id: m.pages![1]!.id, len: 5 }]);
        return res;
      },
    ).catch((e) => e);
    expect(swapped).toBeInstanceOf(ZenError);
    expect((swapped as ZenError).code).toBe('format');
    // Known items: a page not of the base, a chunk without a base.
    const v = (await t.versions(f))[0]!;
    const g = await t.create(ROOT, name());
    const other = await t.writeFile(g, random(1_200_000));
    await expect(
      t.writeFile(f, [{ page: v3(other.manifest).pages![0]! }], { base: v }),
    ).rejects.toMatchObject({ code: 'bad_value' });
    await expect(t.writeFile(f, [{ id: Tree.newId(), size: 10 }])).rejects.toMatchObject({
      code: 'bad_value',
    });
  });
});

describe('pipelined uploads', () => {
  it('keeps commit order: progress in order, a failed commit stops it, the state resumes', async () => {
    const t = tree();
    const f = await t.create(ROOT, name());
    const data = random(2_000_000);
    const states: UploadState[] = [];
    let failAt = 4;
    const r = await calls(
      w.session,
      () =>
        t.upload(f, data, {
          onProgress: (s, c) => {
            expect(c.chunks.length + c.pages.length).toBeGreaterThan(0);
            states.push(structuredClone(s));
          },
        }),
      async (path, n, go) => {
        if (path !== '/v1/commit') return go();
        // Later commits finish first, now and then.
        await sleep(n % 3 === 1 ? 40 : 1);
        if (n === failAt) throw new ZenError(400, 'bad_request', 'injected');
        return go();
      },
    ).catch((e) => e);
    expect(r).toBeInstanceOf(ZenError);
    expect(states.map((s) => s.commits)).toEqual([1, 2, 3]);
    for (let i = 1; i < states.length; i++) {
      expect(states[i]!.size).toBeGreaterThanOrEqual(states[i - 1]!.size);
    }
    const last = states.at(-1)!;
    expect(last.tail.length).toBeLessThanOrEqual(PS);
    expect('chunks' in last).toBe(false);
    failAt = -1;
    const up = await t.upload(f, data.subarray(last.size), { resume: last });
    await t.send([await t.writeOp(up)]);
    expect(equal(await t.readFile(f), data)).toBe(true);

    // Sequential (parallel: 1) gives the same content.
    const g = await t.create(ROOT, name());
    await t.writeFile(g, data, { parallel: 1 });
    expect(equal(await t.readFile(g), data)).toBe(true);
  });
});

describe('the policy and the fallbacks', () => {
  let small: World;
  let sfs: UnlockedFs;
  beforeAll(async () => {
    small = await world({ limits: { max_value_bytes: 2048 } });
    sfs = await small.session
      .fs(1)
      .init((k) => [zw.createDeviceSlot(k, small.admin.device.devicePublic)]);
  });
  afterAll(() => small?.server.stop());
  const SMALL: CdcParams = { min: 64, avgBits: 8, max: 1984 };

  it('writes version 2 under chunking: fixed, and version 3 again under cdc', async () => {
    const fs2 = await w.session
      .fs(2)
      .init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    const t = tree(Tree.newId(), fs2);
    const f = await t.create(ROOT, name());
    await fs2.setPolicy({ chunking: 'fixed' });
    expect((await t.writeFile(f, random(50_000))).manifest.version).toBe(2);
    await fs2.setPolicy({ chunking: 'cdc' });
    const g = await t.create(ROOT, name());
    expect((await t.writeFile(g, random(50_000))).manifest.version).toBe(3);
    // An explicit option wins, and an edit keeps the base's form.
    expect((await t.writeFile(g, random(50_000), { chunking: 'fixed' })).manifest.version).toBe(2);
    expect((await t.append(g, random(10))).manifest.version).toBe(2);
  });

  it('a size past the CDC capacity picks fixed; an unknown one fails too_large', async () => {
    const t = sfs.tree(Tree.newId(), { cdc: SMALL });
    const f = await t.create(ROOT, name());
    expect((await t.writeFile(f, random(1000))).manifest.version).toBe(3);
    expect((await t.writeFile(f, random(1000), { size: 5_000_000 })).manifest.version).toBe(2);
    const big = random(1_500_000);
    expect((await t.writeFile(f, big)).manifest.version).toBe(2);
    const e = await t.writeFile(f, pieces(big, 10_000)).catch((x) => x);
    expect(e).toMatchObject({ code: 'too_large' });
    expect((e as Error).message).toContain("chunking: 'fixed'");
    // No parameters for this max_value_bytes, or no fs_cdc: fixed.
    expect((await sfs.tree(t.id).writeFile(f, random(1000))).manifest.version).toBe(2);
    const info = await small.session.client.info();
    const noCdc = async (): Promise<Info> => ({
      ...info,
      features: info.features.filter((x) => x !== 'fs_cdc'),
    });
    const old = sfs.tree(t.id, { cdc: SMALL, info: noCdc });
    expect((await old.writeFile(f, random(1000))).manifest.version).toBe(2);
  });
});

describe('upload sessions (api.md §12.1)', () => {
  let gw: World;
  let gfs: UnlockedFs;
  beforeAll(async () => {
    gw = await world({
      limits: {
        max_value_bytes: 8192,
        max_commit_bytes: 60_000,
        chunk_grace_secs: 1,
        sweep_interval_secs: 1,
      },
    });
    gfs = await gw.session
      .fs(1)
      .init((k) => [zw.createDeviceSlot(k, gw.admin.device.devicePublic)]);
  });
  afterAll(() => gw?.server.stop());

  /** An upload that fails after `at` bytes; the last state it reported. */
  async function failed(t: Tree, f: Uint8Array, data: Uint8Array, opts = {}) {
    let last: UploadState | undefined;
    async function* failing() {
      for (let i = 0; i < data.length; i += 10_000) {
        if (i > data.length / 2) throw new Error('connection lost');
        yield data.subarray(i, i + 10_000);
      }
    }
    await expect(
      t.upload(f, failing(), {
        ...opts,
        onProgress: (s) => {
          last = structuredClone(s);
        },
      }),
    ).rejects.toThrow('connection lost');
    return last!;
  }

  it('a resume after the grace period works with a session, and fails without one', async () => {
    const t = gfs.tree(Tree.newId(), { cdc: CDC });
    const data = random(1_500_000);
    // With a session.
    const f = await t.create(ROOT, name());
    const last = await failed(t, f, data, { session: true });
    expect(last.session).toBeDefined();
    expect(last.pages.length).toBeGreaterThan(0);
    expect(last.tail.length).toBeLessThanOrEqual(PS);
    expect((await t.uploads()).map((s) => bytes.hex(s.session))).toContain(
      bytes.hex(last.session!),
    );
    await sleep(3500);
    const up = await t.upload(f, data.subarray(last.size), { resume: last });
    expect(up.session).toBeDefined();
    await t.send([await t.writeOp(up)]);
    await t.closeUpload(up.session!);
    expect((await t.uploads()).length).toBe(0);
    await sleep(2500);
    expect(equal(await gfs.tree(t.id).readFile(f), data)).toBe(true);

    // Without one, what was uploaded is collected after the grace.
    const g = await t.create(ROOT, name());
    const plain = await failed(t, g, data);
    expect(plain.session).toBeUndefined();
    await sleep(3500);
    const res = await t
      .upload(g, data.subarray(plain.size), { resume: plain })
      .then(async (u) => t.send([await t.writeOp(u)]))
      .catch((e) => e);
    expect(res).toMatchObject({ status: 400 });
  });

  it('an expired session refuses the resume (412 upload_gone); writeFile closes its own', async () => {
    const t = gfs.tree(Tree.newId(), { cdc: CDC });
    const data = random(600_000);
    const f = await t.create(ROOT, name());
    const last = await failed(t, f, data, { session: true, sessionTtlSecs: 1 });
    await sleep(2500);
    await expect(t.upload(f, data.subarray(last.size), { resume: last })).rejects.toMatchObject({
      status: 412,
      code: 'upload_gone',
    });
    const before = (await t.uploads()).length;
    const g = await t.create(ROOT, name());
    await t.writeFile(g, data, { session: true });
    expect((await t.uploads()).length).toBe(before);
    expect(equal(await t.readFile(g), data)).toBe(true);
  });
});
