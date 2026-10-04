import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { utf8 } from '../src/bytes.js';
import { fullGrants, isCode, member, type Session, type UnlockedFs, zw } from '../src/index.js';
import { testUser } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

let w: World;
let ufs: UnlockedFs;
let recoveryKey: Uint8Array;
let bob: Session;
let bobFs: UnlockedFs;

const text = (b: Uint8Array) => new TextDecoder().decode(b);

async function all<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const x of it) out.push(x);
  return out;
}

beforeAll(async () => {
  // Small pages, so reads page through `more`; short claims for per_key.
  w = await world({ limits: { max_range_items: 3, claim_ttl_ms: 400 } });
  ufs = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    recoveryKey = r.recoveryKey;
    return [r.slot];
  });
  // A second member on another device (leases are held per device).
  const b = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(b.identity, [b.cert]));
    doc.grants.push(...fullGrants(1, b.fp));
  });
  bob = await w.client.signInDevice(b);
  bobFs = await bob.fs(1).unlock({ recoveryKey });
});
afterAll(() => w?.server.stop());

describe('log', () => {
  it('appends and reads, decrypted, paging, by key', async () => {
    const t = ufs.topic('orders');
    const offsets: Uint8Array[] = [];
    for (let i = 0; i < 7; i++) {
      offsets.push(await t.append(utf8(`e${i}`), { key: i % 2 ? 'odd' : 'even' }));
    }
    const evs = await all(t.read());
    expect(evs.map((e) => text(e.payload))).toEqual(['e0', 'e1', 'e2', 'e3', 'e4', 'e5', 'e6']);
    expect(evs.map((e) => e.offset)).toEqual(offsets);
    expect(evs[0]!.sender).toEqual(w.session.deviceFp);
    expect(evs[0]!.keyToken).toEqual(t.keyToken('even'));
    for (let i = 1; i < evs.length; i++) expect(evs[i]!.hlc > evs[i - 1]!.hlc).toBe(true);

    const odd = await all(t.read({ key: 'odd' }));
    expect(odd.map((e) => text(e.payload))).toEqual(['e1', 'e3', 'e5']);
    const tail = await all(t.read({ after: offsets[4], limit: 1 }));
    expect(tail.map((e) => text(e.payload))).toEqual(['e5']);

    // The same path in another session derives the same topic and opens it.
    const other = bobFs.topic('orders');
    expect(other.id).toEqual(t.id);
    expect((await all(other.read({ limit: 2 }))).map((e) => text(e.payload))).toEqual(['e0', 'e1']);
    // A different topic's key doesn't open these envelopes.
    expect(ufs.topic('other').id).not.toEqual(t.id);
  });

  it('carries causation', async () => {
    const t = ufs.topic('caused');
    const first = await t.append(utf8('a'));
    await t.append(utf8('b'), { causation: first });
    const [a, b] = await all(t.read());
    expect(a!.causation).toHaveLength(0);
    expect(b!.causation).toEqual(first);
  });

  it('appends atomically with a transaction', async () => {
    const t = ufs.topic('txn');
    await ufs.transaction(async (tx) => {
      tx.set(['balance'], utf8('10'));
      t.appendIn(tx, utf8('credited'));
    });
    expect(text((await ufs.kv.get(['balance']))!)).toBe('10');
    expect((await all(t.read())).map((e) => text(e.payload))).toEqual(['credited']);

    await expect(
      ufs.transaction(async (tx) => {
        tx.set(['balance'], utf8('20'));
        t.appendIn(tx, utf8('lost'));
        throw new Error('abort');
      }),
    ).rejects.toThrow('abort');
    expect(text((await ufs.kv.get(['balance']))!)).toBe('10');
    expect(await all(t.read())).toHaveLength(1);
  });
});

describe('consumer groups', () => {
  it('sequential: next, ack, ack inside a transaction', async () => {
    const t = ufs.topic('seq');
    const o1 = await t.append(utf8('one'));
    const o2 = await t.append(utf8('two'));
    const c = await t.group('workers', { mode: 'sequential' });
    // Creating it again with the same definition is a no-op; a different one is refused.
    await t.group('workers', { mode: 'sequential' });
    await expect(t.group('workers', { mode: 'per_key' })).rejects.toSatisfy((e) =>
      isCode(e, 'group_exists'),
    );

    await expect(c.next()).rejects.toSatisfy((e) => isCode(e, 'no_lease'));
    const token = await c.lease({ ttlMs: 5000 });
    const [d1] = await c.next();
    expect(text(d1!.payload)).toBe('one');
    expect(d1!.token).toBe(token);
    // Not acknowledged: delivered again (the delivery gate).
    expect((await c.next())[0]!.offset).toEqual(o1);
    await c.ack(d1!);
    expect((await c.cursor()).cursor).toEqual(o1);

    const [d2] = await c.next();
    expect(d2!.offset).toEqual(o2);
    await ufs.transaction(async (tx) => {
      tx.set(['processed', 'two'], utf8('yes'));
      await c.ack(d2!, tx);
    });
    expect(text((await ufs.kv.get(['processed', 'two']))!)).toBe('yes');
    expect((await c.cursor()).cursor).toEqual(o2);

    // A second ack of the same event: the cursor moved, the commit fails whole.
    await expect(
      ufs.transaction(async (tx) => {
        tx.set(['processed', 'again'], utf8('no'));
        await c.ack(d2!, tx);
      }),
    ).rejects.toSatisfy((e) => isCode(e, 'cursor_moved'));
    expect(await ufs.kv.get(['processed', 'again'])).toBeUndefined();

    expect(await c.next({ waitMs: 50 })).toEqual([]);
    await c.release();
  });

  it('long-polls through the delivery iterator', async () => {
    const t = ufs.topic('poll');
    const c = await t.group('poller', { mode: 'sequential' });
    const it = c.deliveries({ waitMs: 2000, ttlMs: 3000 })[Symbol.asyncIterator]();
    const first = it.next();
    setTimeout(() => void t.append(utf8('late')), 200);
    const d = (await first).value!;
    expect(text(d.payload)).toBe('late');
    await c.ack(d);
    await it.return?.(undefined);
    await c.release();
  });

  it('nack until the DLQ, then retry and drop', async () => {
    const t = ufs.topic('poison');
    await t.append(utf8('bad'));
    await t.append(utf8('good'));
    const c = await t.group('poison-g', { mode: 'sequential', maxAttempts: 2 });
    await c.lease();
    const [d] = await c.next();
    expect(text(d!.payload)).toBe('bad');
    expect(await c.nack(d!)).toEqual({ attempts: 1, deadLettered: false });
    const [again] = await c.next();
    expect(again!.attempts).toBe(1);
    expect(await c.nack(again!)).toEqual({ attempts: 2, deadLettered: true });
    // The cursor moved past it.
    const [next] = await c.next();
    expect(text(next!.payload)).toBe('good');
    await c.ack(next!);

    const dlq = await c.dlq.list();
    expect(dlq).toHaveLength(1);
    expect(text(dlq[0]!.event!.payload)).toBe('bad');
    const retried = await c.dlq.retry(dlq[0]!.id);
    const [r] = await c.next();
    expect(r!.offset).toEqual(retried);
    expect(text(r!.payload)).toBe('bad');
    await c.ack(r!);
    await c.dlq.drop(dlq[0]!.id);
    expect(await c.dlq.list()).toEqual([]);
    await c.release();
  });

  it('per_key: keys in parallel, each in order', async () => {
    const t = ufs.topic('perkey');
    await t.append(utf8('a1'), { key: 'a' });
    await t.append(utf8('b1'), { key: 'b' });
    await t.append(utf8('a2'), { key: 'a' });
    await t.append(utf8('unkeyed'));
    const c = await t.group('perkey-g', { mode: 'per_key' });
    const ds = await c.next({ limit: 10 });
    expect(ds.map((d) => text(d.payload))).toEqual(['a1', 'b1']);
    // Both keys are claimed: nothing more until one is acknowledged.
    expect(await c.next({ limit: 10 })).toEqual([]);
    await c.ack(ds[0]!);
    const [a2] = await c.next({ limit: 10 });
    expect(text(a2!.payload)).toBe('a2');
    expect((await c.cursor({ key: 'a' })).cursor).toEqual(ds[0]!.offset);

    // A claim another worker lets lapse is handed out again; the stale token is refused.
    const bc = await bobFs.topic('perkey').group('perkey-g', { mode: 'per_key' });
    const deadline = Date.now() + 10_000;
    let b1: Awaited<ReturnType<typeof bc.next>> = [];
    while (!b1.some((d) => text(d.payload) === 'b1') && Date.now() < deadline) {
      b1 = await bc.next({ limit: 10, waitMs: 200 });
    }
    const theirs = b1.find((d) => text(d.payload) === 'b1')!;
    expect(theirs.token > ds[1]!.token).toBe(true);
    await expect(c.ack(ds[1]!)).rejects.toSatisfy((e) => isCode(e, 'claim_lost'));
    await bc.ack(theirs);
  });
});

describe('leaders', () => {
  async function race(name: string, ttlMs: number) {
    const lost: unknown[] = [];
    const la = await ufs
      .topic('jobs')
      .leader(name, { ttlMs, renewMs: 60_000, onLost: (e) => lost.push(e) });
    const lb = await bobFs.topic('jobs').leader(name, { ttlMs });
    const [a, b] = await Promise.all([la.acquire(), lb.acquire()]);
    expect(a !== b).toBe(true);
    return { la, lb, aWon: a, lost };
  }

  it('one wins, the other gets not_leader, and takes over after expiry', async () => {
    const jobs = ufs.topic('jobs');
    await jobs.append(utf8('job-1'));
    // `la` never renews on its own (renewMs is long), so its lease lapses.
    const { la, lb, aWon, lost } = await race('lead', 600);
    if (!aWon) {
      // Make `la` the stale leader in either case: let bob's lapse first.
      await lb.stop();
      expect(await la.acquire()).toBe(true);
    }
    expect(la.isLeader).toBe(true);
    const tokenA = la.token!;
    await expect(lb.consumer.lease()).rejects.toSatisfy((e) => isCode(e, 'not_leader'));
    const [d] = await la.next();
    expect(text(d!.payload)).toBe('job-1');

    // Bob campaigns until the lease expires (expiry is in versions: poll).
    const deadline = AbortSignal.timeout(15_000);
    await lb.campaign(deadline);
    expect(lb.token).toBe(tokenA + 1n);

    // The deposed leader's late commit is fenced off as a whole.
    await expect(
      ufs.transaction(async (tx) => {
        tx.set(['jobs', 'done'], utf8('by a stale leader'));
        await la.ack(d!, tx);
      }),
    ).rejects.toSatisfy((e) => isCode(e, 'not_leader'));
    expect(await ufs.kv.get(['jobs', 'done'])).toBeUndefined();
    expect(await la.renew()).toBe(false);
    expect(la.isLeader).toBe(false);
    expect(lost).toHaveLength(1);
    expect(isCode(lost[0], 'not_leader')).toBe(true);

    // The new leader processes it, fenced by its own token.
    const [d2] = await lb.next();
    await bobFs.transaction(async (tx) => {
      tx.set(['jobs', 'done'], utf8('by bob'));
      await lb.ack(d2!, tx);
    });
    expect(text((await ufs.kv.get(['jobs', 'done']))!)).toBe('by bob');
    await lb.stop();
    expect(lb.isLeader).toBe(false);
    // Released: the lease is free at once.
    expect(await la.acquire()).toBe(true);
    await la.stop();
  });

  it('renews while it runs', async () => {
    const t = ufs.topic('renewing');
    const la = await t.leader('renew-l', { ttlMs: 300 });
    const lb = await bobFs.topic('renewing').leader('renew-l', { ttlMs: 300 });
    expect(await la.acquire()).toBe(true);
    const end = Date.now() + 1500;
    while (Date.now() < end) {
      expect(await lb.acquire()).toBe(false);
      await new Promise((r) => setTimeout(r, 150));
    }
    expect(la.isLeader).toBe(true);
    await la.stop();
  });
});
