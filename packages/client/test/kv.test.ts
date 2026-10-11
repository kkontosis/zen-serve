import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { bytes, fullGrants, isCode, member, type UnlockedFs, ZenError, zw } from '../src/index.js';
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

describe('stored keys, limited ranges, getAll, clearRange', () => {
  it('gets, sets and deletes by stored key; getAll reads several and records them', async () => {
    const a = fs.kv.key(['sk', 'a']);
    const b = fs.kv.key(['sk', 'b']);
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      tx.set(a, u('A'));
      expect(s(await tx.get(a))).toBe('A');
      const [x, y] = await tx.getAll([a, ['sk', 'b']]);
      expect(s(x)).toBe('A');
      expect(s(y)).toBe(runs === 1 ? undefined : 'B');
      if (runs === 1) await fs.kv.set(['sk', 'b'], u('B'));
    });
    // The concurrent write of b conflicted with the getAll: a second run.
    expect(runs).toBe(2);
    await fs.transaction(async (tx) => tx.delete(b));
    expect(await fs.kv.get(['sk', 'b'])).toBeUndefined();
  });

  it('a limited range records only the part it read', async () => {
    await fs.transaction(async (tx) => {
      for (let i = 0; i < 8; i++) tx.set(['lr', `k${i}`], u(`v${i}`));
    });
    const sorted: Uint8Array[] = [];
    for await (const e of fs.kv.range(['lr'])) sorted.push(e.key);
    const [begin, end] = [fs.kv.key(['lr']), bytes.prefixEnd(fs.kv.key(['lr']))];
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      const page = await tx.rangeStored(begin, end, { limit: 3 });
      expect(page.map((e) => e.key)).toEqual(sorted.slice(0, 3));
      // A write past the page does not conflict.
      if (runs === 1) {
        await fs.transaction(async (t2) => t2.set(sorted[6]!, u('changed')));
      }
      tx.set(['lr', 'seen'], u(String(page.length)));
    });
    expect(runs).toBe(1);
    // One inside the page does.
    runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      await tx.rangeStored(begin, end, { limit: 3 });
      if (runs === 1) {
        await fs.transaction(async (t2) => t2.set(sorted[1]!, u('changed')));
      }
      tx.set(['lr', 'seen'], u('x'));
    });
    expect(runs).toBe(2);
  });

  it('clearRange deletes a stored-key range, and reads see it', async () => {
    await fs.transaction(async (tx) => {
      for (let i = 0; i < 4; i++) tx.set(['cr', `k${i}`], u(`v${i}`));
    });
    const keys: Uint8Array[] = [];
    for await (const e of fs.kv.range(['cr'])) keys.push(e.key);
    await fs.transaction(async (tx) => {
      tx.clearRange(keys[1]!, keys[3]!);
      expect(await tx.get(keys[1]!)).toBeUndefined();
      expect(s(await tx.get(keys[3]!))).toBeDefined();
    });
    const left: Uint8Array[] = [];
    for await (const e of fs.kv.range(['cr'])) left.push(e.key);
    expect(left).toEqual([keys[0], keys[3]]);
  });

  it('a retryable error thrown by fn re-runs it', async () => {
    let runs = 0;
    const out = await fs.transaction(async () => {
      if (++runs < 3) throw new ZenError(409, 'conflict', 'test');
      return runs;
    });
    expect(out).toBe(3);
    await expect(
      fs.transaction(async () => {
        throw new ZenError(400, 'bad_request', 'test');
      }),
    ).rejects.toThrow('bad_request');
  });
});

describe('snapshot reads, expectKey and commit hooks', () => {
  it('a snapshot read records nothing: a concurrent write does not conflict', async () => {
    await fs.kv.set(['snap', 'a'], u('1'));
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      expect(tx.readVersion).toBeUndefined();
      const [e, missing] = await tx.snapshotGet([
        ['snap', 'a'],
        ['snap', 'none'],
      ]);
      expect(tx.readVersion).toBeTypeOf('bigint');
      expect(missing).toBeUndefined();
      if (runs === 1) await fs.kv.set(['snap', 'a'], u('2'));
      // Still at the read version: the value before the concurrent write.
      expect(s((await tx.snapshotGet([['snap', 'a']]))[0]?.value)).toBe('1');
      tx.set(['snap', 'copy'], e!.value);
    });
    expect(runs).toBe(1);
    expect(s(await fs.kv.get(['snap', 'copy']))).toBe('1');
  });

  it("a snapshot range pages, reverses, and sees the transaction's own writes", async () => {
    await fs.transaction(async (tx) => {
      for (let i = 0; i < 8; i++) tx.set(['srange', `k${i}`], u(`v${i}`));
    });
    await fs
      .transaction(async (tx) => {
        tx.delete(['srange', 'k1']);
        tx.set(['srange', 'k2'], u('mine'));
        tx.set(['srange', 'new'], u('n'));
        const got = async (opts?: { limit?: number; reverse?: boolean }) => {
          const out: (string | undefined)[] = [];
          for await (const e of tx.snapshotRange(['srange'], opts)) out.push(s(e.value));
          return out;
        };
        // Stored keys are PRF tokens: compare as sets, and check the order
        // against the stored keys.
        const all = await got();
        expect([...all].sort()).toEqual(['mine', 'n', 'v0', 'v3', 'v4', 'v5', 'v6', 'v7']);
        const keys: Uint8Array[] = [];
        for await (const e of tx.snapshotRange(['srange'])) keys.push(e.key);
        const sorted = [...keys].sort(bytes.compare);
        expect(keys).toEqual(sorted);
        expect(await got({ reverse: true })).toEqual([...all].reverse());
        expect(await got({ limit: 3 })).toEqual(all.slice(0, 3));
        tx.clearPrefix(['srange']);
        expect(await got()).toEqual([]);
        // Abort: the clear is longer than this server's max_range_items.
        throw new Error('abort');
      })
      .catch((e) => expect((e as Error).message).toBe('abort'));
  });

  it('expectKey: a stale version conflicts and re-runs, a current one commits', async () => {
    await fs.kv.set(['cached'], u('c1'));
    const [cached] = await fs.kv.getEntries([['cached']]);
    await fs.transaction(async (tx) => {
      tx.expectKey(['cached'], cached!.version);
      tx.set(['derived'], u('from c1'));
    });
    await fs.kv.set(['cached'], u('c2'));
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      const v = runs === 1 ? cached! : (await fs.kv.getEntries([['cached']]))[0]!;
      tx.expectKey(['cached'], v.version);
      tx.set(['derived'], u(`from ${s(v.value)}`));
    });
    expect(runs).toBe(2);
    expect(s(await fs.kv.get(['derived']))).toBe('from c2');
  });

  it('expectKey: an absent key conflicts once written', async () => {
    await expect(
      fs.transaction(
        async (tx) => {
          tx.expectKey(['not-yet'], undefined);
          await fs.kv.set(['not-yet'], u('x'));
          tx.set(['after-not-yet'], u('y'));
        },
        { attempts: 1 },
      ),
    ).rejects.toSatisfy((e) => isCode(e, 'conflict'));
  });

  it('adds CRDT operations to the commit; the result has their set dots', async () => {
    const object = bytes.randomBytes(16);
    const field = bytes.randomBytes(16);
    let committed: Uint8Array[] | undefined;
    let errors = 0;
    await fs.transaction(async (tx) => {
      tx.addCrdtOps(
        [1, 2].map((i) => ({
          op: 'add' as const,
          fs: fs.id,
          object,
          field,
          elem: bytes.randomBytes(16),
          value: u(`e${i}`),
        })),
      );
      tx.set(['with-crdt'], u('1'));
      tx.onCommit((r) => {
        committed = r.set_dots;
      });
      tx.onError(async () => {
        errors++;
        return false;
      });
    });
    expect(committed).toHaveLength(2);
    expect(committed![0]!.length).toBe(12);
    expect(errors).toBe(0);
  });

  it('an onError hook can ask for a re-run, even after an error that is not retryable', async () => {
    let runs = 0;
    await fs.transaction(async (tx) => {
      runs++;
      // A malformed object (not a multiple of 16 bytes) on the first run: 400.
      const object = bytes.randomBytes(runs === 1 ? 5 : 16);
      tx.addCrdtOps([
        { op: 'row', fs: fs.id, object, hlc: zw.hlc(Date.now(), 0), alive: true, value: u('r') },
      ]);
      tx.onError(async (e) => isCode(e, 'bad_request'));
    });
    expect(runs).toBe(2);
  });
});
