// The incremental prolly tree (zendb.md §5.4) in memory, against zen-core's
// reference builder (`IndexKeys.build`): after every batch of changes the
// stored nodes and the root must be exactly those of a fresh build.
import { readFileSync } from 'node:fs';
import { bytes, initWasm, zw } from '@zen/client';
import type { IndexKeys } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { decodeNode, nodeBucket } from '../src/index/node.js';
import { type Change, lookup, rewrite, scan, treeKeys } from '../src/index/prolly.js';
import { sortKey } from '../src/sortkey.js';
import { MemStore } from './memstore.js';

const hex = bytes.hex;
// biome-ignore lint/suspicious/noExplicitAny: vector JSON
const K: any = JSON.parse(
  readFileSync(new URL('../../../spec/test-vectors/keys.json', import.meta.url), 'utf8'),
);

let fsKeys: zw.FsKeys;
let dbKeys: zw.DbKeys;
let ik: IndexKeys;

beforeAll(async () => {
  await initWasm();
  fsKeys = zw.FsKeys.fromBundle(bytes.fromHex(K.fs_epoch0.bundle));
  dbKeys = fsKeys.db('app');
  ik = dbKeys.index(new Uint8Array(16).fill(0x22));
});
afterAll(() => {
  ik.free();
  dbKeys.free();
  fsKeys.free();
});

/** The reference tree: node id → bytes, and the root record. */
function reference(keys: Uint8Array[], fanout: number) {
  const out = ik.build(keys, fanout);
  const nodes = new Map<string, string>();
  for (let i = 0; i + 1 < out.length; i += 2) nodes.set(hex(out[i]!), hex(out[i + 1]!));
  return { nodes, root: out.length % 2 ? hex(out.at(-1)!) : undefined };
}

function xorshift(seed: number) {
  let x = seed >>> 0 || 1;
  return () => {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    return x / 2 ** 32;
  };
}

const key = (i: number, pad = 0) =>
  sortKey([{ value: i, type: 'int', desc: false }], [`r${i}${'x'.repeat(pad)}`]);

async function check(store: MemStore, model: Map<string, Uint8Array>, fanout: number) {
  const ref = reference([...model.values()], fanout);
  expect(store.root ? hex(store.root) : undefined).toBe(ref.root);
  expect(store.m.size).toBe(ref.nodes.size);
  for (const [id, b] of store.m) expect(hex(b), id).toBe(ref.nodes.get(id));
}

describe('incremental rewrites equal a fresh build (§5.4.4)', () => {
  for (const [seed, fanout] of [
    [1, 4],
    [2, 4],
    [3, 64],
    [4, 2],
  ] as const) {
    it(`seed ${seed}, fanout ${fanout}`, async () => {
      const rnd = xorshift(seed);
      const tk = treeKeys(ik, fanout);
      const store = new MemStore();
      const model = new Map<string, Uint8Array>();
      for (let round = 0; round < 40; round++) {
        const changes: Change[] = [];
        const n = 1 + Math.floor(rnd() * (round % 5 === 0 ? 120 : 12));
        for (let j = 0; j < n; j++) {
          const k = key(Math.floor(rnd() * 400));
          const add = rnd() < (round < 20 ? 0.7 : 0.35);
          changes.push({ key: k, add });
        }
        // Repeats: re-adding a present key, removing an absent one.
        const present = [...model.values()];
        if (present.length) changes.push({ key: present[0]!, add: true });
        changes.push({ key: key(10_000 + round), add: false });
        for (const c of changes) {
          if (c.add) model.set(hex(c.key), c.key);
          else model.delete(hex(c.key));
        }
        const r = await rewrite(tk, store, store.rootRecord(), changes);
        store.apply(r);
        await check(store, model, fanout);
      }
      // Remove everything.
      const r = await rewrite(
        tk,
        store,
        store.rootRecord(),
        [...model.values()].map((k) => ({ key: k, add: false })),
      );
      store.apply(r);
      expect(store.root).toBeUndefined();
      expect(store.m.size).toBe(0);
    });
  }

  it('one change at a time', async () => {
    const rnd = xorshift(9);
    const tk = treeKeys(ik, 4);
    const store = new MemStore();
    const model = new Map<string, Uint8Array>();
    for (let i = 0; i < 300; i++) {
      const k = key(Math.floor(rnd() * 150));
      const add = rnd() < 0.6;
      if (add) model.set(hex(k), k);
      else model.delete(hex(k));
      store.apply(await rewrite(tk, store, store.rootRecord(), [{ key: k, add }]));
      await check(store, model, 4);
    }
  });

  it('hits the entry cap (a huge fanout)', async () => {
    const fanout = 2 ** 31;
    const tk = treeKeys(ik, fanout);
    const store = new MemStore();
    const model = new Map<string, Uint8Array>();
    const add = (from: number, to: number) => {
      const cs: Change[] = [];
      for (let i = from; i < to; i++) {
        const k = key(i);
        model.set(hex(k), k);
        cs.push({ key: k, add: true });
      }
      return cs;
    };
    store.apply(await rewrite(tk, store, undefined, add(0, 2100)));
    await check(store, model, fanout);
    expect(store.m.size).toBeGreaterThan(2);
    store.apply(await rewrite(tk, store, store.rootRecord(), add(5000, 5010)));
    const rm = [7, 1500, 2099].map((i) => ({ key: key(i), add: false }));
    for (const c of rm) model.delete(hex(c.key));
    store.apply(await rewrite(tk, store, store.rootRecord(), rm));
    await check(store, model, fanout);
  });

  it('hits the byte cap (large keys)', async () => {
    const fanout = 2 ** 31;
    const tk = treeKeys(ik, fanout);
    const store = new MemStore();
    const model = new Map<string, Uint8Array>();
    const cs: Change[] = [];
    for (let i = 0; i < 40; i++) {
      const k = key(i, 4000);
      model.set(hex(k), k);
      cs.push({ key: k, add: true });
    }
    store.apply(await rewrite(tk, store, undefined, cs));
    await check(store, model, fanout);
    expect(store.m.size).toBeGreaterThan(2);
    const k = key(3, 4000);
    model.delete(hex(k));
    store.apply(await rewrite(tk, store, store.rootRecord(), [{ key: k, add: false }]));
    await check(store, model, fanout);
  });
});

describe('scans (§5.4.3)', () => {
  it('forward and reverse from random bounds equal the sorted model', async () => {
    const rnd = xorshift(5);
    const store = new MemStore();
    const keys = Array.from({ length: 300 }, (_, i) => key(i * 3));
    store.apply(
      await rewrite(
        treeKeys(ik, 4),
        store,
        undefined,
        keys.map((k) => ({ key: k, add: true })),
      ),
    );
    const sorted = [...keys].sort(bytes.compare).map(hex);
    for (let t = 0; t < 30; t++) {
      const a = key(Math.floor(rnd() * 950) - 20);
      const b = key(Math.floor(rnd() * 950) - 20);
      const [lo, hi] = bytes.compare(a, b) <= 0 ? [a, b] : [b, a];
      const want = sorted.filter((k) => k >= hex(lo) && k < hex(hi));
      const fwd: string[] = [];
      for await (const k of scan(store, store.rootRecord(), lo, hi)) fwd.push(hex(k));
      expect(fwd).toEqual(want);
      const rev: string[] = [];
      for await (const k of scan(store, store.rootRecord(), lo, hi, true)) rev.push(hex(k));
      expect(rev).toEqual([...want].reverse());
    }
    const all: string[] = [];
    for await (const k of scan(store, store.rootRecord(), undefined, undefined)) all.push(hex(k));
    expect(all).toEqual(sorted);
  });
});

describe('decoys (§5.4.6)', () => {
  it('salt and pad every node, rewrite extra paths, keep the entries canonical', async () => {
    const rnd = xorshift(7);
    const tk = treeKeys(ik, 4);
    const store = new MemStore();
    const model = new Map<string, Uint8Array>();
    for (let round = 0; round < 15; round++) {
      const cs: Change[] = [];
      for (let j = 0; j < 6; j++) {
        const k = key(Math.floor(rnd() * 200));
        const add = rnd() < 0.7;
        cs.push({ key: k, add });
        if (add) model.set(hex(k), k);
        else model.delete(hex(k));
      }
      const r = await rewrite(tk, store, store.rootRecord(), cs, { decoys: 2, random: rnd });
      store.apply(r);
      for (const b of store.m.values()) {
        expect(b.length).toBeLessThanOrEqual(nodeBucket(b.length));
        expect(b.length).toBeGreaterThanOrEqual(nodeBucket(b.length) - 2);
        expect(decodeNode(b).salt?.length).toBe(16);
      }
      // Same entries and shape as the canonical tree, other ids.
      const got: string[] = [];
      for await (const k of scan(store, store.rootRecord(), undefined, undefined)) got.push(hex(k));
      expect(got).toEqual([...model.keys()].sort());
      const ref = reference([...model.values()], 4);
      expect(store.m.size).toBe(ref.nodes.size);
      expect(store.rootRecord()?.count ?? 0).toBe(model.size);
    }
  });
});

describe('lookup by prefix (the state tree, §14.2)', () => {
  it('finds the one entry starting with each prefix, with 32-byte ids, in one descent per level', async () => {
    const rnd = xorshift(77);
    const tk = treeKeys(ik, 4, 32);
    const store = new MemStore(32);
    // Entries `esc(suffix) ‖ 00 00 ‖ VH`, as the state tree holds them.
    const suffix = (i: number) => bytes.utf8(`k${i}${i % 3 ? '' : '\0z'}`);
    const entry = (i: number) => zw.stateEntry(suffix(i), bytes.utf8(`v${i}`));
    const prefix = (i: number) => {
      const e = entry(i);
      return e.subarray(0, e.length - 32);
    };
    const live = new Set<number>();
    for (let i = 0; i < 300; i++) if (rnd() < 0.6) live.add(i);
    const r = await rewrite(
      tk,
      store,
      undefined,
      [...live].map((i) => ({ key: entry(i), add: true })),
    );
    store.apply(r);
    let gets = 0;
    const counting = {
      get: (ids: Uint8Array[]) => {
        gets++;
        return store.get(ids);
      },
    };
    const asked = Array.from({ length: 300 }, (_, i) => i);
    const got = await lookup(counting, store.rootRecord(), asked.map(prefix));
    asked.forEach((i, j) => {
      expect(got[j] && hex(got[j])).toBe(live.has(i) ? hex(entry(i)) : undefined);
    });
    expect(gets).toBe(store.rootRecord()!.height);
    expect(await lookup(store, undefined, [prefix(1)])).toEqual([undefined]);
  });
});
