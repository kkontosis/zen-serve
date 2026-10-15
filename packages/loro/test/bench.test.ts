// Measured costs of @zen/loro against a spawned zen-serve (docs/STATS.md,
// "zen-db costs (measured)"). Opt-in, never in CI:
//
//   ZEN_BENCH=1 npx vitest run packages/loro/test/bench.test.ts
import { type UnlockedFs, zw } from '@zen/client';
import { Db } from '@zen/db';
import { afterAll, beforeAll, describe, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openLoro } from '../src/index.js';

const quiet = { warn: () => {} };
let w: World;
let fs: UnlockedFs;
const lines: string[] = [];

beforeAll(async () => {
  w = await world();
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(async () => {
  console.log(['', '| What | ms (median) |', '|---|---|', ...lines].join('\n'));
  await w?.server.stop();
});

const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)]!;
const record = (what: string, ms: number[]) => lines.push(`| ${what} | ${median(ms).toFixed(1)} |`);

describe.skipIf(!process.env.ZEN_BENCH)('zen-loro costs', () => {
  it('an edit across installations, and a load from a snapshot plus a tail', async () => {
    const other = await w.client.signInDevice(w.admin);
    const fs2 = await other.fs(1).unlock({ device: w.admin.device });
    const open = (f: UnlockedFs) => Db.open(f, 'bench', { ...quiet, locks: null });
    const da = await open(fs);
    const db = await open(fs2);
    const a = await openLoro(da, 'doc', { snapshotEvery: 1_000_000, snapshotBytes: 1 << 30 });
    const b = await openLoro(db, 'doc');
    await Promise.all([a.ready, b.ready]);
    const lat: number[] = [];
    for (let i = 0; i < 20; i++) {
      const seen = new Promise<void>((r) => {
        const off = b.onChange(() => {
          off();
          r();
        });
      });
      const t0 = performance.now();
      a.doc.getText('t').insert(0, `edit ${i} `);
      a.doc.commit();
      await seen;
      lat.push(performance.now() - t0);
    }
    record('Loro: a local edit until another installation imports it', lat);
    await a.compact();
    for (let i = 0; i < 100; i++) {
      a.doc.getText('t').insert(0, 'x');
      a.doc.commit();
      await a.flush();
    }
    await b.close();
    await db.close();
    const loads: number[] = [];
    for (let i = 0; i < 5; i++) {
      const d = await open(fs2);
      const t0 = performance.now();
      const h = await openLoro(d, 'doc');
      await h.ready;
      loads.push(performance.now() - t0);
      await h.close();
      await d.close();
    }
    record('Loro: open from a snapshot plus a 100-event tail', loads);
    await a.close();
    await da.close();
    fs2.close();
  });
});
