// The replica's records are written behind (store.ts); what waits to be
// written is bounded by `pendingBytes`, so a local store slower than the
// writes (zen-mount's cache dir: a file and an fsync per record) makes the
// writes wait rather than fill memory (milestone 8 step 6b).
import {
  bytes,
  connect,
  type LocalStore,
  memoryLocalStore,
  ROOT,
  sealedStore,
  Tree,
  type UnlockedFs,
  zw,
} from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type ReplicaOptions } from '../src/index.js';

const MiB = 1 << 20;
let w: World;
let admin: UnlockedFs;
beforeAll(async () => {
  w = await world();
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

function random(n: number): Uint8Array {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, i + 65536));
  return out;
}

/** A local store whose writes take `ms` each, as one fsync per file does. */
function slowStore(ms: number): LocalStore {
  const raw = memoryLocalStore();
  return {
    ...raw,
    put: async (s, k, v) => {
      await new Promise((r) => setTimeout(r, ms));
      return raw.put(s, k, v);
    },
  };
}

/** A session of its own, not watched (a watched one keeps every request). */
async function unlocked(): Promise<UnlockedFs> {
  const client = await connect(w.server.url, { origin: w.client.origin });
  const session = await client.signInDevice(w.admin);
  return session.fs(1).unlock({ device: w.admin.device });
}

const opts = (pendingBytes: number): ReplicaOptions => ({
  local: sealedStore(slowStore(2)),
  locks: null,
  channel: null,
  waitMs: 200,
  dirtyBytes: 4 * MiB,
  pendingBytes,
});

/** The most `f()` reaches while `g` runs, sampled every 5 ms. */
async function peak<T>(f: () => number, g: () => Promise<T>): Promise<[T, number]> {
  let most = f();
  const t = setInterval(() => {
    most = Math.max(most, f());
  }, 5);
  try {
    return [await g(), Math.max(most, f())];
  } finally {
    clearInterval(t);
  }
}

/**
 * Write `size` bytes as one write (sealed whole before it is queued, as a
 * file handle's first flush of a large file is), then read them back in
 * 1 MiB reads on a second replica: the most bytes each replica had
 * waiting to be written meanwhile.
 */
async function run(pendingBytes: number, size: number) {
  const id = Tree.newId();
  await admin.tree(id).mkdir(ROOT, '.keep');
  const data = random(size);
  const r = await openReplica(await unlocked(), id, opts(pendingBytes));
  await r.sync();
  const [, writing] = await peak(
    () => r.pendingBytes,
    () => r.writeFile('/big', data),
  );
  await r.flush();
  await r.close();
  const view = await openReplica(await unlocked(), id, opts(pendingBytes));
  await view.sync();
  const got = new Uint8Array(size);
  const [, reading] = await peak(
    () => view.pendingBytes,
    async () => {
      for (let i = 0; i < size; i += MiB) {
        got.set(await view.read('/big', i, Math.min(MiB, size - i)), i);
      }
    },
  );
  // Fetched content past the bound is a cache dropped, not waited for.
  const viewKept = (await view.status()).cacheBytes;
  await view.close();
  // A pin is kept whole, waiting for the store window by window.
  const pinner = await openReplica(await unlocked(), id, opts(pendingBytes));
  await pinner.sync();
  const [, pinning] = await peak(
    () => pinner.pendingBytes,
    async () => {
      await pinner.pin('/big');
      await pinner.idle();
    },
  );
  await pinner.flush();
  const pinKept = (await pinner.status()).cacheBytes;
  await pinner.close();
  return { size, writing, reading, pinning, viewKept, pinKept, equal: bytes.equal(got, data) };
}

describe('records waiting to be written are bounded (pendingBytes)', () => {
  it('a store slower than the writes makes them wait; reads drop past it; pins are kept', async () => {
    const bound = 2 * MiB;
    const m = await run(bound, 48 * MiB);
    expect(m.equal).toBe(true);
    // The bound, plus what one sink call (one commit's content, at most
    // max_commit_bytes, 8 MB) or one read adds before it waits.
    expect(m.writing).toBeLessThanOrEqual(bound + 9 * MiB);
    expect(m.reading).toBeLessThanOrEqual(bound + 2 * MiB);
    expect(m.viewKept).toBeLessThan(m.size / 2);
    // A pinned file: whole, with at most one window (256 chunks) past the bound.
    expect(m.pinKept).toBeGreaterThanOrEqual(m.size);
    expect(m.pinning).toBeLessThanOrEqual(bound + 2 * 256 * 65_536);
  });
});
