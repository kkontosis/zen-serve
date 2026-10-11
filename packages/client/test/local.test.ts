// The local store (zendb.md §9.3): the backends, and the sealed layers.
import { mkdtemp, readdir, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import type { UnlockedFs } from '../src/fs.js';
import { indexedDbLocalStore } from '../src/local/idb.js';
import { nodeDiskLocalStore } from '../src/local/node.js';
import { type SealedStoreOptions, sealedStore } from '../src/local/sealed.js';
import { type LocalStore, memoryLocalStore } from '../src/local/store.js';
import { initWasm, zw } from '../src/wasm.js';

let dir: string;
beforeAll(async () => {
  await initWasm();
  dir = await mkdtemp(join(tmpdir(), 'zen-local-'));
});
afterAll(() => rm(dir, { recursive: true, force: true }));

const k = (s: string) => utf8(s);
const all = async (it: AsyncIterable<[Uint8Array, Uint8Array]>) => {
  const out: [string, string][] = [];
  for await (const [a, b] of it)
    out.push([new TextDecoder().decode(a), new TextDecoder().decode(b)]);
  return out.sort();
};

let n = 0;
const backends: [string, () => LocalStore][] = [
  ['memory', () => memoryLocalStore()],
  ['node disk', () => nodeDiskLocalStore(join(dir, `d${n++}`))],
  [
    'IndexedDB',
    () => indexedDbLocalStore(`db${n++}`, { factory: new IDBFactory(), keyRange: IDBKeyRange }),
  ],
];

/** An fs whose keys are `keys` (the fs layer needs only these). */
const fakeFs = (keys: zw.FsKeys) => ({ keys, keysFor: () => keys }) as unknown as UnlockedFs;

describe.each(backends)('%s backend', (_, make) => {
  it('puts, gets, lists, deletes and clears', async () => {
    const s = make();
    expect(await s.get('a', k('x'))).toBeUndefined();
    await s.put('a', k('x'), k('1'));
    await s.put('a', k('y'), k('2'));
    await s.put('a', k('x'), k('3'));
    await s.put('a/b', k('x'), k('4'));
    expect(new TextDecoder().decode(await s.get('a', k('x')))).toBe('3');
    expect(await all(s.list('a'))).toEqual([
      ['x', '3'],
      ['y', '2'],
    ]);
    expect(await all(s.list('a/b'))).toEqual([['x', '4']]);
    expect(await all(s.list('none'))).toEqual([]);
    await s.delete('a', [k('y'), k('missing')]);
    expect(await all(s.list('a'))).toEqual([['x', '3']]);
    await s.clear('a');
    expect(await all(s.list('a'))).toEqual([]);
    expect(await all(s.list('a/b'))).toEqual([['x', '4']]);
    await s.clear();
    expect(await all(s.list('a/b'))).toEqual([]);
    s.close();
  });

  it('keeps one device key', async () => {
    const s = make();
    const a = await s.deviceKey();
    const b = await s.deviceKey();
    expect(a).toBe(b);
    expect(a.extractable).toBe(false);
  });
});

describe('the Node disk backend', () => {
  it('keeps the device key in a file only its owner reads, and reuses it', async () => {
    const d = join(dir, 'keyfile');
    const raw = nodeDiskLocalStore(d);
    const s = sealedStore(raw);
    await s.put('q', k('e1'), k('hello'));
    expect((await stat(join(d, 'device.key'))).mode & 0o777).toBe(0o600);
    // Another process opens the same directory: the same key.
    const again = sealedStore(nodeDiskLocalStore(d));
    expect(new TextDecoder().decode(await again.get('q', k('e1')))).toBe('hello');
    // No temporary files are left behind.
    const files = await readdir(join(d, 'stores', 'q'));
    expect(files.every((f) => !f.endsWith('.tmp'))).toBe(true);
  });

  it('a store without its device key reads nothing', async () => {
    const d = join(dir, 'lostkey');
    await sealedStore(nodeDiskLocalStore(d)).put('q', k('e1'), k('hello'));
    await rm(join(d, 'device.key'));
    const warned: string[] = [];
    const s = sealedStore(nodeDiskLocalStore(d), { warn: (m) => warned.push(m) });
    expect(await s.get('q', k('e1'))).toBeUndefined();
    expect(warned.length).toBe(1);
  });
});

describe('the sealed store', () => {
  const fsA = () => fakeFs(zw.FsKeys.generate(7));

  const layerSets = [
    { name: 'device', opts: () => ({}) },
    { name: 'fs', opts: () => ({ device: false as const, fs: fsA() }) },
    { name: 'device + fs', opts: () => ({ fs: fsA() }) },
    { name: 'plaintext', opts: () => ({ device: false as const, plaintext: true }) },
  ];

  it.each(layerSets)('round-trips with layers: $name', async ({ opts }) => {
    const raw = memoryLocalStore();
    const s = sealedStore(raw, opts());
    await s.put('queue/app', k('e1'), k('secret'));
    expect(new TextDecoder().decode(await s.get('queue/app', k('e1')))).toBe('secret');
    expect(await all(s.list('queue/app'))).toEqual([['e1', 'secret']]);
    const stored = (await raw.get('queue/app', k('e1')))!;
    expect(stored[0]).toBe(1);
    expect(stored[1]).toBe(s.layers);
    // Sealed records don't hold the plaintext.
    const clear = new TextDecoder().decode(stored).includes('secret');
    expect(clear).toBe(s.layers === 0);
  });

  it('refuses neither layer without plaintext: true', () => {
    expect(() => sealedStore(memoryLocalStore(), { device: false })).toThrow(/plaintext/);
  });

  it('reads records of another setting as absent, and drops them', async () => {
    const raw = memoryLocalStore();
    const fs = fsA();
    const settings = [
      sealedStore(raw, { warn: () => {} }),
      sealedStore(raw, { fs, warn: () => {} }),
      sealedStore(raw, { device: false, fs, warn: () => {} }),
      sealedStore(raw, { device: false, plaintext: true, warn: () => {} }),
    ];
    for (const [i, w] of settings.entries()) {
      for (const [j, r] of settings.entries()) {
        await w.put('s', k('x'), k('v'));
        const got = await r.get('s', k('x'));
        if (i === j) expect(got).toBeDefined();
        else {
          expect(got).toBeUndefined();
          expect(await raw.get('s', k('x'))).toBeUndefined();
        }
      }
    }
  });

  it.each(layerSets.slice(0, 3))(
    'a flipped byte, a moved record, or another fs reads as absent: $name',
    async ({ opts }) => {
      const raw = memoryLocalStore();
      const o: SealedStoreOptions = opts();
      const warned: string[] = [];
      const s = sealedStore(raw, { ...o, warn: (m) => warned.push(m) });
      await s.put('a', k('x'), k('v'));
      const rec = (await raw.get('a', k('x')))!;
      // Flipped.
      const bad = rec.slice();
      bad[bad.length - 1]! ^= 1;
      await raw.put('a', k('x'), bad);
      expect(await s.get('a', k('x'))).toBeUndefined();
      // Copied to another key, or another store.
      await raw.put('a', k('y'), rec);
      await raw.put('b', k('x'), rec);
      expect(await s.get('a', k('y'))).toBeUndefined();
      expect(await s.get('b', k('x'))).toBeUndefined();
      expect(warned.length).toBe(3);
      // Under another fs's keys (the same device key).
      if (o.fs) {
        const dk = await raw.deviceKey();
        const mine = sealedStore(raw, { ...o, device: o.device === false ? false : dk });
        await mine.put('a', k('z'), k('v'));
        const other = sealedStore(raw, {
          ...o,
          device: o.device === false ? false : dk,
          fs: fakeFs(zw.FsKeys.generate(8)),
          warn: () => {},
        });
        expect(await other.get('a', k('z'))).toBeUndefined();
      }
    },
  );

  it('an app-given device key is used instead of the backend one', async () => {
    const raw = memoryLocalStore();
    const key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, [
      'encrypt',
      'decrypt',
    ]);
    await sealedStore(raw, { device: key }).put('a', k('x'), k('v'));
    expect(await sealedStore(raw, { warn: () => {} }).get('a', k('x'))).toBeUndefined();
    await sealedStore(raw, { device: key }).put('a', k('x'), k('v'));
    expect(await sealedStore(raw, { device: key }).get('a', k('x'))).toBeDefined();
  });

  it('a plaintext record on disk is the value itself, after two header bytes', async () => {
    const d = join(dir, 'plain');
    const s = sealedStore(nodeDiskLocalStore(d), { device: false, plaintext: true });
    await s.put('a', k('x'), k('hello'));
    const f = await readFile(join(d, 'stores', 'a', '78'));
    expect([...f.subarray(0, 2)]).toEqual([1, 0]);
    expect(f.subarray(2).toString()).toBe('hello');
  });
});
