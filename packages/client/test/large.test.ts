// Large files: version-2 manifests, pages, streams, resumable uploads and
// the fs file-key policy (formats.md §11.3–11.6, §12; fs.md §4.1–4.2). The
// server's max_value_bytes is 2,048, so chunks are 1,984 bytes, a page
// holds 127 ids and an inline manifest about 60.
import type { ChunkPut, ManifestV2, PagePut } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  bytes,
  type ChunkIndex,
  connect,
  memoryLocalStore,
  ROOT,
  type Session,
  sealedStore,
  Tree,
  type UnlockedFs,
  type UploadState,
  ZenError,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

const { equal, hex } = bytes;
const CS = 1984;

let w: World;
let ufs: UnlockedFs;
beforeAll(async () => {
  w = await world({ limits: { max_value_bytes: 2048, max_commit_bytes: 60_000 } });
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

async function collect(s: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const parts: Uint8Array[] = [];
  const r = s.getReader();
  for (let x = await r.read(); !x.done; x = await r.read()) parts.push(x.value);
  return bytes.concat(...parts);
}

/** Count the calls to each path while `fn` runs; `edit` may change a response. */
async function calls<T>(
  s: Session,
  fn: () => Promise<T>,
  edit?: (path: string, r: unknown) => unknown,
): Promise<{ result: T; count: Map<string, number>; chunks: number }> {
  const orig = s.call.bind(s);
  const count = new Map<string, number>();
  let chunks = 0;
  s.call = (async (path: string, enc: never, dec: never, req: { chunks?: unknown[] }) => {
    count.set(path, (count.get(path) ?? 0) + 1);
    if (path === '/v1/commit') chunks += req.chunks?.length ?? 0;
    const r = await orig(path, enc, dec, req as never);
    return edit ? edit(path, r) : r;
  }) as Session['call'];
  try {
    return { result: await fn(), count, chunks };
  } finally {
    s.call = orig;
  }
}

describe('paged files', () => {
  it('round-trip from bytes, an async generator and a web stream', async () => {
    const t = ufs.tree(Tree.newId());
    const data = random(400 * CS - 100);
    async function* gen() {
      for (let i = 0; i < data.length; i += 5000) yield data.subarray(i, i + 5000);
    }
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        for (let i = 0; i < data.length; i += 30_000) c.enqueue(data.slice(i, i + 30_000));
        c.close();
      },
    });
    for (const src of [data, gen(), stream]) {
      const f = await t.create(ROOT, `f${Math.random()}`);
      const r = await t.writeFile(f, src);
      expect(r.manifest.version === 2 && r.manifest.chunkSize).toBe(CS);
      expect(r.manifest.pages?.length).toBe(4);
      expect(r.pagesUploaded).toBe(4);
      const [v] = await t.versions(f);
      expect(v!.pages.length).toBe(4);
      expect(v!.chunks).toEqual([]);
      expect(v!.chunkCount).toBe(400);
      expect(equal(await t.readFile(f), data)).toBe(true);
      expect(equal(await collect(t.readStream(f)), data)).toBe(true);
      const part = await collect(t.readStream(f, { offset: 300_000, length: 200_001 }));
      expect(equal(part, data.subarray(300_000, 500_001))).toBe(true);
    }
  });

  it('a range read fetches one page and its chunks; the page is kept', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'r');
    const data = random(400 * CS);
    await t.writeFile(f, data);
    const fresh = ufs.tree(t.id);
    const [v] = await fresh.versions(f);
    const at = 200 * CS + 5;
    const a = await calls(w.session, () => fresh.read(f, v, at, 100));
    expect(equal(a.result, data.subarray(at, at + 100))).toBe(true);
    expect(a.count.get('/v1/fs/pages/get')).toBe(1);
    expect(a.count.get('/v1/fs/chunks/get')).toBe(1);
    const b = await calls(w.session, () => fresh.read(f, v, at + CS, 100));
    expect(b.count.get('/v1/fs/pages/get')).toBeUndefined();
    // The page cache serves another tree object.
    const cache = new Map<string, Uint8Array>();
    const pageCache = {
      get: (id: Uint8Array) => cache.get(hex(id)),
      put: (id: Uint8Array, d: Uint8Array) => void cache.set(hex(id), d),
    };
    await ufs.tree(t.id, { pageCache }).read(f, v, at, 1);
    const c = await calls(w.session, () => ufs.tree(t.id, { pageCache }).read(f, v, at, 1));
    expect(c.count.get('/v1/fs/pages/get')).toBeUndefined();
  });

  it('an append reuses chunks and full pages', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'a');
    const data = random(300 * CS);
    const w1 = await t.writeFile(f, data);
    expect(w1.manifest.pages?.length).toBe(3);
    const more = bytes.concat(data, random(100 * CS));
    const w2 = await t.writeFile(f, more, { previous: w1.index });
    expect(w2.uploaded).toBe(100);
    expect(w2.pagesUploaded).toBe(2);
    expect(w2.manifest.pages!.slice(0, 2)).toEqual(w1.manifest.pages!.slice(0, 2));
    expect(equal(await t.readFile(f), more)).toBe(true);
    // An index built from the version read back reuses as well.
    const [v] = await t.versions(f);
    const idx = await t.chunkIndex(f, v!);
    expect(idx.pages).toEqual(w2.manifest.pages);
    const w3 = await t.writeFile(f, bytes.concat(more, random(10)), { previous: idx });
    expect([w3.uploaded, w3.pagesUploaded]).toEqual([1, 1]);
  });

  it('small files stay inline; past the inline limit they are paged', async () => {
    const t = ufs.tree(Tree.newId());
    const small = await t.create(ROOT, 'small');
    const r = await t.writeFile(small, random(10 * CS));
    expect(r.manifest.version).toBe(2);
    expect(r.manifest.chunks?.length).toBe(10);
    expect((await t.versions(small))[0]!.pages).toEqual([]);
    let n = 10;
    for (; n < 200; n += 5) {
      const f = await t.create(ROOT, `n${n}`);
      const x = await t.writeFile(f, random(n * CS));
      if (x.manifest.pages) break;
    }
    expect(n).toBeGreaterThan(40);
    expect(n).toBeLessThan(80);
  });

  it('a swapped or missing page is refused', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'p');
    await t.writeFile(f, random(300 * CS));
    const other = ufs.tree(Tree.newId());
    const g = await other.create(ROOT, 'q');
    await other.writeFile(g, random(300 * CS));
    const [vg] = await other.versions(g);
    const foreign = (
      await w.session.call('/v1/fs/pages/get', zw.encodePagesGet, zw.decodePages, {
        fs: 1,
        ids: [vg!.pages[0]!],
      })
    ).pages[0]!.data!;
    const swap = (data: Uint8Array | undefined) => (path: string, r: unknown) => {
      if (path !== '/v1/fs/pages/get') return r;
      const p = r as { pages: { id: Uint8Array; data?: Uint8Array }[] };
      return { pages: p.pages.map((x) => ({ id: x.id, ...(data ? { data } : {}) })) };
    };
    for (const [data, code] of [
      [foreign, 'format'],
      [undefined, 'not_found'],
    ] as const) {
      const fresh = ufs.tree(t.id);
      await expect(calls(w.session, () => fresh.readFile(f), swap(data))).rejects.toMatchObject({
        code,
      });
    }
  });

  it('reads version 1 and rewrites it as version 2', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'v1');
    const data = random(5 * CS + 7);
    const keys = ufs.sealKeys();
    const ids: Uint8Array[] = [];
    const puts = [];
    for (let i = 0; i < data.length; i += CS) {
      const id = crypto.getRandomValues(new Uint8Array(16));
      ids.push(id);
      puts.push({ fs: 1, id, data: keys.sealChunk(id, data.subarray(i, i + CS)) });
    }
    const manifest = keys.sealManifest(t.id, f, { size: data.length, chunkSize: CS, chunks: ids });
    await t.send([{ op: 'write', node: f, replaces: [], chunks: ids, manifest }], puts);
    const [v] = await t.versions(f);
    expect(v!.manifest.version).toBe(1);
    expect(equal(await t.readFile(f), data)).toBe(true);
    expect(equal(await t.read(f, v, CS - 3, 10), data.subarray(CS - 3, CS + 7))).toBe(true);
    expect(equal(await collect(t.readStream(f)), data)).toBe(true);
    // Under the default policy its chunks are reused (both under the fs keys).
    const idx = await t.chunkIndex(f, v!);
    const next = bytes.concat(data, random(5));
    const w2 = await t.writeFile(f, next, { previous: idx });
    expect(w2.manifest.version).toBe(2);
    expect(w2.manifest.fileKey).toBeUndefined();
    expect(w2.uploaded).toBe(1);
    expect(equal(await t.readFile(f), next)).toBe(true);
  });

  it('resumes an upload from its last state', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'resume');
    const data = random(400 * CS + 11);
    let last: UploadState | undefined;
    async function* failing() {
      for (let i = 0; i < data.length; i += 10_000) {
        if (i > 250 * CS) throw new Error('connection lost');
        yield data.subarray(i, i + 10_000);
      }
    }
    await expect(
      t.upload(f, failing(), {
        onProgress: (s) => {
          last = structuredClone(s);
        },
      }),
    ).rejects.toThrow('connection lost');
    expect(last!.size % CS).toBe(0);
    expect(last!.pages.length).toBeGreaterThan(0);
    const before = last!.uploaded;
    const r = await calls(w.session, () =>
      t.upload(f, data.subarray(last!.size), { resume: last! }),
    );
    expect(r.chunks).toBe(401 - before);
    expect(r.result.uploaded).toBe(401);
    await t.send([await t.writeOp(r.result)]);
    expect(equal(await t.readFile(f), data)).toBe(true);
    // A state of another node is refused.
    await expect(t.upload(ROOT, new Uint8Array(0), { resume: last! })).rejects.toMatchObject({
      code: 'bad_value',
    });
  });

  it('a server without fs_pages refuses a file past the inline limit (413)', async () => {
    const client = await connect(w.server.url, { origin: `http://localhost:${w.server.port}` });
    const info = client.info.bind(client);
    client.info = async () => {
      const i = await info();
      return { ...i, features: i.features.filter((f) => f !== 'fs_pages') };
    };
    const s = await client.signInDevice(w.admin);
    const u = await s.fs(1).unlock({ device: w.admin.device });
    const t = u.tree(Tree.newId());
    const f = await t.create(ROOT, 'x');
    await t.writeFile(f, random(10 * CS));
    await expect(t.writeFile(f, random(200 * CS))).rejects.toMatchObject({ status: 413 });
  });
});

describe('the file-key policy (fs.md §4.2)', () => {
  let fs2: UnlockedFs;
  let t: Tree;
  beforeAll(async () => {
    fs2 = await w.session.fs(2).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    t = fs2.tree(Tree.newId());
  });

  const rotate = async () => {
    fs2 = await fs2.rotate((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    t = fs2.tree(t.id);
  };

  it('off by default: fs keys, and appends reuse chunks across a rotation', async () => {
    expect(fs2.policy).toEqual({ fileKeys: false, rekeyAfterRotate: 'rewrite', chunking: 'cdc' });
    expect(fs2.header.encode()[0]).toBe(1);
    const f = await t.create(ROOT, 'plain');
    const data = random(3 * CS);
    const w1 = await t.writeFile(f, data);
    expect(w1.manifest.fileKey).toBeUndefined();
    const [v] = await t.versions(f);
    const id = (v!.manifest as ManifestV2).chunks![0]!;
    const sealed = (
      await w.session.call('/v1/fs/chunks/get', zw.encodeChunksGet, zw.decodeChunks, {
        fs: 2,
        ids: [id],
      })
    ).chunks[0]!.data!;
    expect(equal(fs2.keysFor(sealed).openChunk(id, sealed), data.subarray(0, CS))).toBe(true);
    await rotate();
    const more = bytes.concat(data, random(CS));
    const w2 = await t.writeFile(f, more, { previous: w1.index });
    expect(w2.uploaded).toBe(1);
    expect(equal(await t.readFile(f), more)).toBe(true);
  });

  it('on: a key per file, kept while the epoch stays', async () => {
    await fs2.setPolicy({ fileKeys: true });
    expect(fs2.policy.fileKeys).toBe(true);
    expect(fs2.header.encode()[0]).toBe(2);
    const f = await t.create(ROOT, 'k1');
    const g = await t.create(ROOT, 'k2');
    const w1 = await t.writeFile(f, random(2 * CS));
    const w2 = await t.writeFile(g, random(2 * CS));
    expect(w1.manifest.fileKey).toBeDefined();
    expect(equal(w1.manifest.fileKey!, w2.manifest.fileKey!)).toBe(false);
    // f's chunks don't open under g's key.
    const id = w1.index!.chunks[0]!;
    const sealed = (
      await w.session.call('/v1/fs/chunks/get', zw.encodeChunksGet, zw.decodeChunks, {
        fs: 2,
        ids: [id],
      })
    ).chunks[0]!.data!;
    expect(() =>
      zw.openFileChunk(2, w2.manifest.fileKey!, w2.manifest.keyEpoch!, id, sealed),
    ).toThrow();
    // A rewrite without `previous` keeps the node's key.
    const w3 = await t.writeFile(f, random(CS));
    expect(equal(w3.manifest.fileKey!, w1.manifest.fileKey!)).toBe(true);
  });

  it('after a rotation: rewrite, eager and keep', async () => {
    const files = new Map<
      string,
      { node: Uint8Array; data: Uint8Array; key: Uint8Array; index: ChunkIndex }
    >();
    const setup = async (name: string) => {
      const node = await t.create(ROOT, name);
      const data = random(3 * CS);
      const r = await t.writeFile(node, data);
      files.set(name, { node, data, key: r.manifest.fileKey!, index: r.index! });
    };
    for (const n of ['rw-prev', 'rw-whole', 'eager', 'keep']) await setup(n);
    await rotate();
    const write = async (name: string, withPrevious: boolean) => {
      const x = files.get(name)!;
      const next = bytes.concat(x.data, random(10));
      const r = await t.writeFile(x.node, next, withPrevious ? { previous: x.index } : {});
      expect(equal(await t.readFile(x.node), next)).toBe(true);
      return r;
    };
    const old = (name: string) => files.get(name)!.key;

    const a = await write('rw-prev', true);
    expect(equal(a.manifest.fileKey!, old('rw-prev'))).toBe(true);
    expect(a.uploaded).toBe(1);
    const b = await write('rw-whole', false);
    expect(equal(b.manifest.fileKey!, old('rw-whole'))).toBe(false);
    expect(b.manifest.keyEpoch).toBe(fs2.keys.epoch);

    await fs2.setPolicy({ rekeyAfterRotate: 'eager' });
    const c = await write('eager', true);
    expect(equal(c.manifest.fileKey!, old('eager'))).toBe(false);
    expect(c.uploaded).toBe(4);

    await fs2.setPolicy({ rekeyAfterRotate: 'keep' });
    const d = await write('keep', true);
    expect(equal(d.manifest.fileKey!, old('keep'))).toBe(true);
    expect(d.uploaded).toBe(1);
    await fs2.setPolicy({ rekeyAfterRotate: 'rewrite' });
  });

  it('a history switching modes reads back at every step', async () => {
    const f = await t.create(ROOT, 'mixed');
    let prev: ChunkIndex | undefined;
    let data = random(2 * CS);
    for (const on of [false, true, false]) {
      await fs2.setPolicy({ fileKeys: on });
      data = bytes.concat(data, random(100));
      const r = await t.writeFile(f, data, prev ? { previous: prev } : {});
      expect(r.manifest.fileKey !== undefined).toBe(on);
      // A switch of key re-uploads everything.
      expect(r.uploaded).toBe(3);
      expect(equal(await t.readFile(f), data)).toBe(true);
      prev = r.index;
    }
  });

  it('chunking: set to fixed and back, kept in the header (formats.md §12)', async () => {
    await fs2.setPolicy({ fileKeys: false, rekeyAfterRotate: 'rewrite', chunking: 'cdc' });
    await fs2.setPolicy({ chunking: 'fixed' });
    expect(fs2.policy.chunking).toBe('fixed');
    expect(fs2.header.encode()[0]).toBe(2);
    const again = await w.session.fs(2).unlock({ device: w.admin.device });
    expect(again.policy.chunking).toBe('fixed');
    again.close();
    await fs2.setPolicy({ chunking: 'cdc' });
    // The default policy is written as a version-1 header again.
    expect(fs2.header.encode()[0]).toBe(1);
  });

  it('the policy survives an offline unlock (the kept header)', async () => {
    await fs2.setPolicy({ fileKeys: true, rekeyAfterRotate: 'keep' });
    const local = sealedStore(memoryLocalStore());
    const how = { device: w.admin.device };
    (await w.session.fs(2).unlock(how, { local })).close();
    const orig = w.session.call.bind(w.session);
    w.session.call = (async () => {
      throw new ZenError(0, 'network', 'offline (test)');
    }) as Session['call'];
    try {
      const u = await w.session.fs(2).unlock(how, { local });
      expect(u.policy).toEqual({ fileKeys: true, rekeyAfterRotate: 'keep', chunking: 'cdc' });
      u.close();
    } finally {
      w.session.call = orig;
    }
  });
});

describe('sealing without sending (Tree.seal), known chunks', () => {
  it('seals into a sink, sends nothing, and the sunk content writes later', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'later.bin');
    const data = random(200 * CS + 7); // paged: 2 pages
    const chunks: ChunkPut[] = [];
    const pages: PagePut[] = [];
    const { result: u, count } = await calls(w.session, () =>
      t.seal(f, data, {
        sink: async (cs, ps) => {
          chunks.push(...cs);
          pages.push(...ps);
        },
      }),
    );
    expect(count.get('/v1/commit') ?? 0).toBe(0);
    expect(u.commits).toBeGreaterThan(1);
    expect(chunks.length).toBe(201);
    expect(pages.length).toBe(2);
    expect(u.pages.length).toBe(2);
    // Upload the sunk content, then write it with explicit replaces.
    for (let i = 0; i < chunks.length; i += 20) await t.send([], chunks.slice(i, i + 20));
    await t.send([], [], pages);
    await t.send([t.writeOpOf(u, [])]);
    expect(equal(await t.readFile(f), data)).toBe(true);
  });

  it('takes known chunks over with no plaintext: one chunk and one page change', async () => {
    const t = ufs.tree(Tree.newId());
    const f = await t.create(ROOT, 'big.bin');
    const data = random(300 * CS);
    const w1 = await t.writeFile(f, data);
    expect(w1.manifest.pages!.length).toBe(3);
    const ix = w1.index!;
    const at = 150 * CS + 10; // chunk 150, page 1
    const next = data.slice();
    next[at]! ^= 0xff;
    async function* src() {
      for (let i = 0; i < ix.chunks.length; i++) {
        if (i === 150) yield next.subarray(i * CS, (i + 1) * CS);
        else yield { id: ix.chunks[i]!, hash: ix.hashes![i]!, size: CS };
      }
    }
    const w2 = await t.writeFile(f, src(), { previous: ix, replaces: [w1.dot] });
    expect(w2.uploaded).toBe(1);
    expect(w2.pagesUploaded).toBe(1);
    expect(equal(await t.readFile(f), next)).toBe(true);

    // A known chunk not of `previous`, or mid-block, is refused.
    const bad = async function* () {
      yield { id: Tree.newId(), hash: new Uint8Array(32), size: CS };
    };
    await expect(t.writeFile(f, bad(), { previous: w2.index })).rejects.toMatchObject({
      code: 'bad_value',
    });
    // A known chunk off a block boundary is fetched and cut again.
    const mid = async function* () {
      yield new Uint8Array(10);
      yield { id: ix.chunks[1]!, hash: ix.hashes![1]!, size: CS };
    };
    const r = await calls(w.session, () => t.writeFile(f, mid(), { previous: ix }));
    expect(r.count.get('/v1/fs/chunks/get')).toBe(1);
    expect(
      equal(await t.readFile(f), bytes.concat(new Uint8Array(10), data.subarray(CS, 2 * CS))),
    ).toBe(true);
  });
});
