// The version-2 file formats through wasm, against spec/test-vectors/fs.json
// (formats.md §11.3–11.6).
import { readFileSync } from 'node:fs';
import { beforeAll, describe, expect, it } from 'vitest';
import { fromHex, hex } from '../src/bytes.js';
import { initWasm, zw } from '../src/wasm.js';

// biome-ignore lint/suspicious/noExplicitAny: vector JSON
type J = any;
const root = new URL('../../../spec/test-vectors/', import.meta.url);
const V: J = JSON.parse(readFileSync(new URL('fs.json', root), 'utf8'));
const K: J = JSON.parse(readFileSync(new URL('keys.json', root), 'utf8'));

beforeAll(async () => {
  await initWasm();
});

describe('fs.json, version 2', () => {
  it('pages, manifests, a chunk under its file key, a paged write', () => {
    const keys = zw.FsKeys.fromBundle(fromHex(K.fs_epoch0.bundle));
    const tree = fromHex(V.tree);
    const node = fromHex(V.node);
    const pageIds: Uint8Array[] = [];
    for (const p of V.pages) {
      const page = fromHex(p.page);
      expect(hex(zw.pageDigest(page))).toBe(p.digest);
      expect(hex(zw.encodePage(zw.decodePage(page)))).toBe(p.page);
      pageIds.push(fromHex(p.id));
    }
    const fileKey = fromHex(V.file_key.key);
    for (const name of ['manifest_v2_inline', 'manifest_v2_paged']) {
      const m = keys.openAnyManifest(tree, node, fromHex(V[name].sealed));
      expect(m.version).toBe(2);
      if (m.version !== 2) continue;
      expect(hex(m.fileKey!)).toBe(V.file_key.key);
      expect(m.keyEpoch).toBe(0);
      // Sealing it again round-trips (fresh nonce, same plaintext).
      const again = keys.openAnyManifest(tree, node, keys.sealManifestV2(tree, node, m));
      expect(again).toEqual(m);
    }
    // key_mode 0: no file key, the chunks under the fs keys.
    const plain = keys.openAnyManifest(tree, node, fromHex(V.manifest_v2_fs_keys.sealed));
    expect(plain.version === 2 && plain.fileKey === undefined && plain.keyEpoch === undefined).toBe(
      true,
    );
    expect(
      keys.openAnyManifest(tree, node, keys.sealManifestV2(tree, node, plain as never)),
    ).toEqual(plain);
    const paged = keys.openAnyManifest(tree, node, fromHex(V.manifest_v2_paged.sealed));
    expect(paged.version === 2 && paged.pages?.map((p) => hex(p.id))).toEqual(pageIds.map(hex));
    const chunkId = fromHex(V.file_chunk.id);
    const opened = zw.openFileChunk(7, fileKey, 0, chunkId, fromHex(V.file_chunk.sealed));
    expect(hex(opened)).toBe(V.file_chunk.plaintext);
    expect(() =>
      zw.openFileChunk(7, zw.newFileKey(), 0, chunkId, fromHex(V.file_chunk.sealed)),
    ).toThrow();
    const sealed = zw.sealFileChunk(7, fileKey, 0, chunkId, opened);
    expect(hex(zw.openFileChunk(7, fileKey, 0, chunkId, sealed))).toBe(V.file_chunk.plaintext);
    const op = zw.writePagedOpBytes(
      7,
      tree,
      node,
      V.op_chain.replaces.map(fromHex),
      pageIds,
      fromHex(V.manifest_v2_paged.sealed),
    );
    expect(hex(op)).toBe(V.write_paged.op);
    expect(zw.pageCapacity(90_000)).toBe(5_624);
    // A version-1 manifest opens as version 1.
    const v1 = keys.openAnyManifest(tree, node, fromHex(V.manifest.sealed));
    expect(v1.version).toBe(1);
  });
});

/** `n` bytes of a xorshift32 stream: after each step, its low byte (fs.json `cdc`). */
function xorshift(seed: number, n: number): Uint8Array {
  let x = seed >>> 0;
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    out[i] = x & 0xff;
  }
  return out;
}

/** Chunk lengths of `data`, fed to `c` in pieces of `piece` bytes. */
function lengths(c: zw.Chunker, data: Uint8Array, piece = data.length): number[] {
  const out: number[] = [];
  let last = 0;
  for (let off = 0; off < data.length; off += piece) {
    for (const cut of c.push(data.subarray(off, off + piece))) {
      out.push(off + cut - last);
      last = off + cut;
    }
  }
  const tail = c.finish();
  if (tail) out.push(tail);
  return out;
}

describe('fs.json, version 3', () => {
  it('manifests with chunk lengths, form-2 pages', () => {
    const keys = zw.FsKeys.fromBundle(fromHex(K.fs_epoch0.bundle));
    const tree = fromHex(V.tree);
    const node = fromHex(V.node);
    V.pages_v2.forEach((p: J, i: number) => {
      const page = fromHex(p.page);
      expect(hex(zw.pageDigest(page))).toBe(p.digest);
      const any = zw.decodeAnyPage(page);
      expect(any.lens).toEqual([[40_000, 30_000][i]]);
      const again = zw.encodePage2(any.ids.map((id, j) => ({ id, len: any.lens![j]! })));
      expect(hex(again)).toBe(p.page);
      expect(() => zw.decodePage(page)).toThrow();
    });
    // A form-1 page decodes without lengths.
    expect(zw.decodeAnyPage(fromHex(V.pages[0].page)).lens).toBeUndefined();
    for (const name of ['manifest_v3_inline', 'manifest_v3_paged']) {
      const m = keys.openAnyManifest(tree, node, fromHex(V[name].sealed));
      expect(m.version).toBe(3);
      if (m.version !== 3) continue;
      expect([m.size, m.n]).toEqual([70_000, 2]);
      expect(m.cdc).toEqual({
        min: V.cdc.params.min,
        avgBits: V.cdc.params.avg_bits,
        max: V.cdc.params.max,
      });
      const again = keys.openAnyManifest(tree, node, keys.sealManifestV3(tree, node, m));
      expect(again).toEqual(m);
    }
    const inline = keys.openAnyManifest(tree, node, fromHex(V.manifest_v3_inline.sealed));
    expect(inline.version === 3 && inline.chunks?.map((c) => c.len)).toEqual([40_000, 30_000]);
    // Sealing checks the manifest as a reader would: lengths must sum to the size.
    if (inline.version === 3) {
      expect(() => keys.sealManifestV3(tree, node, { ...inline, size: 70_001 })).toThrow();
    }
    expect(zw.pageCapacity2(90_000)).toBe(4_499);
  });

  it('the chunker: the vector cuts, in pieces, after an insertion, under a file key', () => {
    const keys = zw.FsKeys.fromBundle(fromHex(K.fs_epoch0.bundle));
    const c = V.cdc;
    const params = zw.cdcDefaults(90_000)!;
    expect(params).toEqual({ min: c.params.min, avgBits: c.params.avg_bits, max: c.params.max });
    expect(zw.cdcDefaults(1_000)).toBeUndefined();
    const input = xorshift(c.input.xorshift32_seed, c.input.len);
    for (const piece of [input.length, 1, 7, 65_536]) {
      expect(lengths(keys.chunker(params), input, piece)).toEqual(c.lengths);
    }
    const ins = xorshift(c.insert.xorshift32_seed, c.insert.len);
    const edited = new Uint8Array(input.length + ins.length);
    edited.set(input.subarray(0, c.insert.at));
    edited.set(ins, c.insert.at);
    edited.set(input.subarray(c.insert.at), c.insert.at + ins.length);
    expect(lengths(keys.chunker(params), edited)).toEqual(c.lengths_after_insert);
    const fileKey = fromHex(V.file_key.key);
    expect(lengths(zw.fileChunker(7, fileKey, params), input)).toEqual(c.lengths_file_key);
    const ch = keys.chunker(params);
    ch.push(input.subarray(0, 100));
    expect(ch.pending).toBe(100);
    ch.free();
  });
});
