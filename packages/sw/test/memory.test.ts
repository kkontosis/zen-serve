// A response is streamed (spec/sw.md §6): a large file through handle()
// is never held whole (OPS.md §6). What the replica's read holds meanwhile
// (its read-ahead, the fetches and the records being written) was measured
// at 59–73 MiB live for 128 to 512 MiB, so the bound is half the file
// (TECH_DEBT TD-MEMORY). That handle adds at most one piece to it is
// handle.test.ts's "the body is pulled" test.
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
const gc = runInNewContext('gc') as () => void;

/** Live bytes: the heap and array buffers, after a full collection. */
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
  it('a 256 MiB file through handle is never held whole', async () => {
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
    expect(most).toBeLessThan(SIZE / 2);
  }, 120_000);
});
