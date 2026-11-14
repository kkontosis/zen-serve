// The replica's write-behind records (store.ts): a record reads as written
// from `put` on, while it waits, while its store write runs, and after.
import { bytes, memoryLocalStore, type SealedStore, sealedStore } from '@zen/client';
import { describe, expect, it } from 'vitest';
import { Records } from '../src/store.js';

/** A sealed store whose writes wait until `release()` (or fail with `fail()`). */
function gated(): {
  store: SealedStore;
  held: () => number;
  release: () => void;
  fail: () => void;
} {
  const inner = sealedStore(memoryLocalStore(), { device: false, plaintext: true });
  let waiting: { ok: () => void; no: (e: unknown) => void }[] = [];
  const gate = () => new Promise<void>((ok, no) => waiting.push({ ok, no }));
  const settle = (f: (w: (typeof waiting)[number]) => void) => {
    const ws = waiting;
    waiting = [];
    for (const w of ws) f(w);
  };
  return {
    store: {
      ...inner,
      get: inner.get.bind(inner),
      list: inner.list.bind(inner),
      clear: inner.clear.bind(inner),
      put: async (s, k, v) => {
        await gate();
        await inner.put(s, k, v);
      },
      delete: async (s, ks) => {
        await gate();
        await inner.delete(s, ks);
      },
    },
    held: () => waiting.length,
    release: () => settle((w) => w.ok()),
    fail: () => settle((w) => w.no(new Error('disk full (test)'))),
  };
}

const key = bytes.utf8('k');
const v1 = bytes.utf8('one');
const v2 = bytes.utf8('two');
const str = (b: Uint8Array | undefined) => (b ? bytes.fromUtf8(b) : undefined);
const tick = () => new Promise((r) => setTimeout(r, 0));

/** Wait until the store holds `n` writes at the gate. */
async function heldAt(g: ReturnType<typeof gated>, n: number) {
  for (let i = 0; i < 100 && g.held() < n; i++) await tick();
  expect(g.held()).toBe(n);
}

describe('records being written stay readable (store.ts)', () => {
  it('a put reads as written while its store write runs, and after', async () => {
    const g = gated();
    const rec = new Records(g.store, 'zfs/1/t');
    rec.put('chunks', key, v1);
    expect(str(await rec.get('chunks', key))).toBe('one'); // pending
    const f = rec.flush();
    await heldAt(g, 1);
    // Taken by the flush, not in the store yet.
    expect(await g.store.get(rec.name('chunks'), key)).toBeUndefined();
    expect(str(await rec.get('chunks', key))).toBe('one');
    expect([...(await rec.list('chunks')).keys()]).toEqual([bytes.hex(key)]);
    expect(rec.pendingBytes).toBe(3);
    g.release();
    await f;
    expect(rec.pendingBytes).toBe(0);
    expect(str(await g.store.get(rec.name('chunks'), key))).toBe('one');
    expect(str(await rec.get('chunks', key))).toBe('one');
  });

  it('a delete reads as done while its store write runs', async () => {
    const g = gated();
    const rec = new Records(g.store, 'zfs/1/t');
    rec.put('versions', key, v1);
    const f1 = rec.flush();
    await heldAt(g, 1);
    g.release();
    await f1;
    rec.delete('versions', key);
    const f2 = rec.flush();
    await heldAt(g, 1);
    expect(str(await g.store.get(rec.name('versions'), key))).toBe('one');
    expect(await rec.get('versions', key)).toBeUndefined();
    expect((await rec.list('versions')).size).toBe(0);
    g.release();
    await f2;
    expect(await rec.get('versions', key)).toBeUndefined();
  });

  it('a newer value taken by a later flush wins over one still being written', async () => {
    const g = gated();
    const rec = new Records(g.store, 'zfs/1/t');
    rec.put('nodes', key, v1);
    const f1 = rec.flush();
    await heldAt(g, 1);
    rec.put('nodes', key, v2);
    expect(str(await rec.get('nodes', key))).toBe('two'); // pending
    const f2 = rec.flush(); // waits for the first batch
    expect(str(await rec.get('nodes', key))).toBe('two'); // being written, after `one`
    g.release(); // `one` lands; `two` is next at the gate
    await f1;
    await heldAt(g, 1);
    expect(str(await g.store.get(rec.name('nodes'), key))).toBe('one');
    expect(str(await rec.get('nodes', key))).toBe('two');
    expect(str((await rec.list('nodes')).get(bytes.hex(key)))).toBe('two');
    g.release();
    await f2;
    expect(str(await rec.get('nodes', key))).toBe('two');
    expect(str(await g.store.get(rec.name('nodes'), key))).toBe('two');
  });

  it('a failed write is no longer read as written', async () => {
    const g = gated();
    const rec = new Records(g.store, 'zfs/1/t');
    rec.put('pins', key, v1);
    const f = rec.flush();
    await heldAt(g, 1);
    expect(str(await rec.get('pins', key))).toBe('one');
    g.fail();
    await expect(f).rejects.toThrow('disk full');
    expect(await rec.get('pins', key)).toBeUndefined();
    expect(rec.pendingBytes).toBe(0);
  });
});
