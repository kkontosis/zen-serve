import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { bytes, fullGrants, isCode, member, type UnlockedFs, zw } from '../src/index.js';
import { testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

const u = bytes.utf8;
const s = (b: Uint8Array | undefined) => (b ? bytes.fromUtf8(b) : undefined);

let w: World;
let fs: UnlockedFs;
beforeAll(async () => {
  w = await world({ limits: { max_range_items: 5 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

describe('kv', () => {
  it('sets, gets and deletes', async () => {
    expect(await fs.kv.get(['users', 'alice'])).toBeUndefined();
    await fs.kv.set(['users', 'alice'], u('A'));
    expect(s(await fs.kv.get(['users', 'alice']))).toBe('A');
    await fs.kv.delete(['users', 'alice']);
    expect(await fs.kv.get(['users', 'alice'])).toBeUndefined();
  });

  it('ranges over a prefix, paging past max_range_items (5 here)', async () => {
    await fs.transaction(async (tx) => {
      for (let i = 0; i < 12; i++) tx.set(['items', `k${i}`], u(`v${i}`));
      tx.set(['other'], u('x'));
    });
    const all = [];
    for await (const e of fs.kv.range(['items'])) all.push(s(e.value));
    expect(all.sort()).toEqual(Array.from({ length: 12 }, (_, i) => `v${i}`).sort());
    const three = [];
    for await (const e of fs.kv.range(['items'], { limit: 3 })) three.push(e);
    expect(three).toHaveLength(3);
  });

  it('a value from one fs does not open under another key', async () => {
    const other = await w.session
      .fs(2)
      .init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
    await fs.kv.set(['secret'], u('fs1'));
    const raw = await fs.session.call('/v1/kv/get', zw.encodeKvGet, zw.decodeKvItems, {
      fs: 1,
      keys: [fs.kv.key(['secret'])],
    });
    expect(() => other.kv.open(other.kv.key(['secret']), raw.items[0]!.value!)).toThrow(/decrypt/);
    other.close();
  });
});

describe('transactions', () => {
  it('short mode: concurrent increments serialize', async () => {
    await fs.kv.set(['counter'], u('0'));
    const inc = () =>
      fs.transaction(async (tx) => {
        const n = Number(s(await tx.get(['counter'])));
        tx.set(['counter'], u(String(n + 1)));
      });
    await Promise.all(Array.from({ length: 6 }, inc));
    expect(s(await fs.kv.get(['counter']))).toBe('6');
  });

  it('short mode: a range read conflicts with an insert into it (phantoms)', async () => {
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      const items = await tx.range(['phantom']);
      if (runs === 1) await fs.kv.set(['phantom', 'sneaky'], u('1'));
      tx.set(['phantom-count'], u(String(items.length)));
    });
    expect(runs).toBe(2);
    expect(s(await fs.kv.get(['phantom-count']))).toBe('1');
  });

  it('long mode: expect and range hashes, with no time limit', async () => {
    await fs.kv.set(['doc'], u('v1'));
    let runs = 0;
    await fs.transaction(
      async (tx) => {
        runs++;
        const v = await tx.get(['doc']);
        await tx.range(['doc-parts']);
        // Someone changes the doc after our read: the commit's expect fails.
        if (runs === 1) await fs.kv.set(['doc'], u('v2'));
        tx.set(['doc'], u(`${s(v)}+edit`));
      },
      { mode: 'long' },
    );
    expect(runs).toBe(2);
    expect(s(await fs.kv.get(['doc']))).toBe('v2+edit');
  });

  it('long mode: expecting an absent key', async () => {
    await expect(
      fs.transaction(
        async (tx) => {
          if (!(await tx.get(['once']))) tx.set(['once'], u('first'));
          await fs.kv.set(['once'], u('raced'));
        },
        { mode: 'long', attempts: 1 },
      ),
    ).rejects.toSatisfy((e) => isCode(e, 'conflict'));
  });

  it("reads see the transaction's own writes and clears", async () => {
    await fs.transaction(async (tx) => {
      tx.set(['small', 'x'], u('x'));
      tx.set(['small', 'y'], u('y'));
    });
    await fs.transaction(async (tx) => {
      tx.set(['own', 'a'], u('1'));
      expect(s(await tx.get(['own', 'a']))).toBe('1');
      tx.clearPrefix(['small']);
      expect(await tx.range(['small'])).toEqual([]);
      tx.set(['small', 'fresh'], u('f'));
      expect((await tx.range(['small'])).map((e) => s(e.value))).toEqual(['f']);
    });
    const left = [];
    for await (const e of fs.kv.range(['small'])) left.push(s(e.value));
    expect(left).toEqual(['f']);
  });

  it('a read-only transaction commits nothing', async () => {
    const v = await fs.transaction(async (tx) => s(await tx.get(['own', 'a'])));
    expect(v).toBe('1');
  });

  it('a member without write rights is refused', async () => {
    const carol = testUser(3);
    await w.session.acl.update(w.admin.identity, (doc) => {
      doc.members.push(member(carol.identity, [carol.cert]));
      doc.grants.push({ fs: 1, rights: ['read'], subject: carol.fp });
    });
    await fs.addDevice(carol.device.devicePublic);
    const sc = await w.client.signInDevice(carol);
    const cfs = await sc.fs(1).unlock({ device: carol.device });
    expect(s(await cfs.kv.get(['own', 'a']))).toBe('1');
    await expect(cfs.kv.set(['own', 'a'], u('2'))).rejects.toSatisfy((e) => isCode(e, 'forbidden'));
    cfs.close();
    // And fs 2 isn't hers at all.
    await expect(sc.fs(2).header()).rejects.toSatisfy((e) => isCode(e, 'forbidden'));
    void fullGrants;
  });
});
