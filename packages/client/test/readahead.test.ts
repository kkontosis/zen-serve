// readStream fetches exactly `readAhead` windows ahead of what has been
// read, and nothing before the first read (docs/CLIENT.md, OPS.md §6).
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { bytes, ROOT, Tree, type UnlockedFs, zw } from '../src/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
beforeAll(async () => {
  w = await world();
  ufs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const MiB = 1 << 20;
/** Windows of the file: readStream reads about 4 MiB (whole chunks) at a time. */
const WINDOWS = 7;

/** Let the stream's fetches start and settle. */
const settle = () => new Promise((r) => setTimeout(r, 150));

describe('read-ahead', () => {
  it('readStream holds exactly readAhead windows beyond what was read', async () => {
    const id = Tree.newId();
    const t = ufs.tree(id);
    const f = await t.create(ROOT, 'big.bin');
    const data = new Uint8Array(WINDOWS * 4 * MiB - 1000).map((_, i) => (i * 13) & 0xff);
    await t.writeFile(f, data);

    for (const ahead of [0, 1, 3]) {
      const r = ufs.tree(id);
      let windows = 0;
      const orig = r.fetchChunks.bind(r);
      r.fetchChunks = (...a: Parameters<typeof orig>) => {
        windows++;
        return orig(...a);
      };
      const s = r.readStream(f, { readAhead: ahead });
      await settle();
      expect(windows, `readAhead ${ahead}, before any read`).toBe(0);
      const reader = s.getReader();
      const parts: Uint8Array[] = [];
      // Windows fetched after each read.
      const seen: number[] = [];
      for (;;) {
        const x = await reader.read();
        if (x.done) break;
        parts.push(x.value);
        await settle();
        seen.push(windows);
      }
      const n = parts.length; // the file's windows
      expect(n).toBeGreaterThanOrEqual(WINDOWS - 1);
      expect(seen, `readAhead ${ahead}`).toEqual(seen.map((_, i) => Math.min(i + 1 + ahead, n)));
      expect(bytes.equal(bytes.concat(...parts), data)).toBe(true);
    }
  });
});
