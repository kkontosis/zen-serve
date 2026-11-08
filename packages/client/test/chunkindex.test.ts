// The compact chunk index (milestone 8 step 8): its helpers, and that a
// written file's index costs about 52 bytes a chunk in memory, not the ~390
// of one small `Uint8Array` per id and per hash.
import { setFlagsFromString } from 'node:v8';
import { runInNewContext } from 'node:vm';
import type { CdcParams } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { Bytes, compactIndex, pack, U32s } from '../src/chunkindex.js';
import {
  bytes,
  type ChunkIndex,
  chunkHash,
  chunkId,
  ROOT,
  Tree,
  type UnlockedFs,
  zw,
} from '../src/index.js';
import { type World, world } from './helpers.js';

const CDC: CdcParams = { min: 256, avgBits: 10, max: 8128 };

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

describe('the compact chunk index', () => {
  it('Bytes and U32s grow by doubling and take exactly what was pushed', () => {
    const b = new Bytes(4);
    const u = new U32s();
    for (let i = 0; i < 1000; i++) {
      b.push(new Uint8Array([i & 0xff, (i >> 8) & 0xff, 7]));
      u.push(i * 3);
    }
    const got = b.take();
    expect(got.length).toBe(3000);
    expect([...got.subarray(2997)]).toEqual([999 & 0xff, 999 >> 8, 7]);
    expect(u.take()).toEqual(Uint32Array.from({ length: 1000 }, (_, i) => i * 3));
    b.clear();
    expect(b.take().length).toBe(0);
  });

  it('views, packing and the array form made compact', () => {
    const ids = [Tree.newId(), Tree.newId(), Tree.newId()];
    const hashes = ids.map(() => random(32));
    const ix = compactIndex({ cdc: CDC, chunks: ids, lens: [5, 6, 7], hashes });
    expect(ix.n).toBe(3);
    expect(ix.lens).toEqual(Uint32Array.from([5, 6, 7]));
    for (let i = 0; i < 3; i++) {
      expect(bytes.equal(chunkId(ix, i), ids[i]!)).toBe(true);
      expect(bytes.equal(chunkHash(ix, i)!, hashes[i]!)).toBe(true);
    }
    expect(compactIndex(ix)).toBe(ix);
    expect(() => pack([new Uint8Array(15)], 16)).toThrow(RangeError);
  });

  describe('in memory', () => {
    let w: World;
    let ufs: UnlockedFs;
    beforeAll(async () => {
      w = await world({ limits: { max_value_bytes: 8192, max_commit_bytes: 2_000_000 } });
      ufs = await w.session
        .fs(1)
        .init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    });
    afterAll(() => w?.server.stop());

    // About 40,000 chunks of about 1 KiB.
    const N = 40 << 20;

    /** Heap and buffers after collecting, in bytes. */
    function live(): number {
      setFlagsFromString('--expose-gc');
      const gc = runInNewContext('gc') as () => void;
      gc();
      gc();
      const m = process.memoryUsage();
      return m.heapUsed + m.arrayBuffers;
    }

    it('a written file of 40,000 chunks keeps an index of about 52 bytes a chunk', async () => {
      const t = ufs.tree(Tree.newId(), { cdc: CDC });
      const f = await t.create(ROOT, 'big');
      const data = random(N);
      const before = live();
      const up = await t.upload(f, data, {});
      const ix: ChunkIndex = up.index!;
      // The negative control: the same index as one array per id and hash.
      const held = process.env.ZEN_INDEX_CONTROL
        ? {
            chunks: Array.from({ length: ix.n }, (_, i) => chunkId(ix, i).slice()),
            hashes: Array.from({ length: ix.n }, (_, i) => chunkHash(ix, i)!.slice()),
          }
        : ix;
      const grew = live() - before;
      const perChunk = grew / ix.n;
      console.log(
        `chunkindex: ${ix.n} chunks, ${(grew / 2 ** 20).toFixed(1)} MiB kept, ${perChunk.toFixed(0)} B a chunk`,
      );
      expect(ix.n).toBeGreaterThan(30_000);
      expect(ix.ids.length).toBe(16 * ix.n);
      expect(ix.hashes!.length).toBe(32 * ix.n);
      expect(perChunk).toBeLessThan(120);
      expect(held).toBeDefined();
    }, 120_000);
  });
});
