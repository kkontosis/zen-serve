// A network failure while a consumer's handler commits (zendb.md §13.2),
// at the default 30 s claim TTL: the delivery runs again once the network
// is back. A nack would fail too, and the key's later events would wait
// for the claim to expire.
import { type Session, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { Db } from '../src/index.js';

let w: World;
let fs: UnlockedFs;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world();
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

/** An offline switch on a session: while on, every call fails `network`. */
function offline(s: Session): { on: boolean; restore(): void } {
  const orig = s.call.bind(s);
  const n = {
    on: false,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.on) throw new ZenError(0, 'network', 'offline (test)');
    return orig(path, enc, dec, req);
  }) as Session['call'];
  return n;
}

describe('a network failure in a handler', () => {
  it('per_key: the delivery runs again once the network is back; the key is not held until the claim expires', async () => {
    const db = await Db.open(fs, 'net1', {
      warn: () => {},
      locks: null,
      scheduler: false,
      schema: 1,
      migrations: { 1: (m) => m.createTable('log', { pk: ['id'] }) },
    });
    const t = db.topic('work', 'net1');
    await db.emit(t, 'job', 'a', { key: 'k' });
    await db.emit(t, 'job', 'b', { key: 'k' });
    const net = offline(db.fs.session);
    let cut = false;
    const t0 = Date.now();
    const h = db.consume(
      { group: 'g', topic: t, mode: 'per_key', onError: () => {} },
      async (m, tx) => {
        await tx.table('log').insert({ id: m.body as string });
        if (!cut) {
          // This commit fails, and so would a nack: the network is gone
          // for longer than the commit's own replays.
          cut = true;
          net.on = true;
          setTimeout(() => {
            net.on = false;
          }, 2000);
        }
      },
    );
    try {
      const rows = async () => (await db.table('log').query().limit(10).page()).rows;
      const end = Date.now() + 10_000;
      while ((await rows().catch(() => [])).length < 2) {
        if (Date.now() > end) throw new Error('timed out waiting for both events');
        await sleep(50);
      }
      expect(cut).toBe(true);
      expect((await rows()).map((r) => r.id).sort()).toEqual(['a', 'b']);
      // Well under the 30 s claim TTL.
      expect(Date.now() - t0).toBeLessThan(10_000);
    } finally {
      net.restore();
      await h.stop();
      db.close();
    }
  });
});
