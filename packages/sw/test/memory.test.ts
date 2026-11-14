// A response is streamed (spec/sw.md §6): a large file through handle()
// holds about the replica's read-ahead and its records waiting to be
// written, whatever the file's size (OPS.md §6). Live memory is measured
// with array buffers swept at each collection: by default V8 frees them on
// a concurrent sweeper, so right after gc() they still count, at 2 to 4
// times the live figure (milestone 9 step 3b). That handle adds at most one
// piece is handle.test.ts's "the body is pulled" test.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setFlagsFromString } from 'node:v8';
import { runInNewContext } from 'node:vm';
import { connect, ROOT, sealedStore, Tree, type UnlockedFs, zw } from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { openReplica } from '@zen/fs';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { handle, parseMounts } from '../src/index.js';

const MiB = 1 << 20;
const SIZE = 256 * MiB;
let w: World;
let admin: UnlockedFs;
let dir: string;
beforeAll(async () => {
  w = await world();
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  dir = await mkdtemp(join(tmpdir(), 'zen-sw-memory-'));
});
afterAll(async () => {
  w?.server.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

setFlagsFromString('--expose-gc');
setFlagsFromString('--no-concurrent-array-buffer-sweeping');
const gc = runInNewContext('gc') as () => void;

/** Live bytes: the heap and array buffers, after a full collection (and its sweep). */
function live(): number {
  gc();
  const m = process.memoryUsage();
  return m.heapUsed + m.arrayBuffers;
}

/** The byte at `p` of the file. */
const byteAt = (p: number) => (p * 7 + (p >>> 16)) & 0xff;

/** `n` bytes, generated a MiB at a time (never held whole). */
async function* source(n: number): AsyncGenerator<Uint8Array> {
  for (let i = 0; i < n; i += MiB) {
    const b = new Uint8Array(Math.min(MiB, n - i));
    for (let j = 0; j < b.length; j++) b[j] = byteAt(i + j);
    yield b;
  }
}

describe('memory (OPS.md §6)', () => {
  it('a 256 MiB file through handle holds less than 48 MiB live', async () => {
    const id = Tree.newId();
    const t = admin.tree(id);
    const f = await t.create(ROOT, 'big.bin');
    await t.writeFile(f, source(SIZE));

    const client = await connect(w.server.url, { origin: w.client.origin });
    const session = await client.signInDevice(w.admin);
    const ufs = await session.fs(1).unlock({ device: w.admin.device });
    const r = await openReplica(ufs, id, {
      local: sealedStore(nodeDiskLocalStore(dir)),
      locks: null,
      channel: null,
      waitMs: 200,
      pendingBytes: 4 * MiB,
    });
    await r.sync();
    const site = {
      mounts: parseMounts([{ name: 's', fs: 1, tree: id, base: '/' }]),
      replica: async () => r,
    };
    const base = live();
    let most = 0;
    const timer = setInterval(() => {
      most = Math.max(most, live() - base);
    }, 50);
    let got = 0;
    let wrong = 0;
    try {
      const res = await handle(new Request('https://view.test/s/big.bin'), site);
      expect(res.status).toBe(200);
      expect(res.headers.get('Content-Length')).toBe(String(SIZE));
      const reader = res.body!.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        for (let j = 0; j < value.length; j += 97) if (value[j] !== byteAt(got + j)) wrong++;
        got += value.length;
      }
    } finally {
      clearInterval(timer);
      most = Math.max(most, live() - base);
      await r.close();
    }
    expect(got).toBe(SIZE);
    expect(wrong).toBe(0);
    console.log(`handle: 256 MiB streamed, live memory +${(most / MiB).toFixed(1)} MiB at most`);
    // Measured 20.8–35.5 MiB over 14 runs: the window read and 3 ahead (4 MiB
    // each), the copies of those being fetched, and up to 4 MiB of records
    // waiting to be written plus one window's chunks put below that bound.
    expect(most).toBeLessThan(48 * MiB);
  }, 120_000);
});
