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
