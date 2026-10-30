// The authenticated tier (zendb.md §14) against a spawned zen-serve with
// the test adversary: every read is checked against the NsRoot's state
// tree, whose freshness device checkpoints keep. Honest runs report
// nothing; a rolled-back row, index root or NsRoot, a withheld row and a
// fork are detected. Each attack test first shows the attack works on a
// basic database (or in the window), then that it is caught.
import {
  bytes,
  type Client,
  connect,
  fullGrants,
  type Integrity,
  IntegrityError,
  member,
  memoryLocalStore,
  type SealedStore,
  type Session,
  sealedStore,
  Transaction,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { afterEach, describe, expect, it } from 'vitest';
import { type Lie, lie, truth } from '../../client/src/testing/adversary.js';
import type { ServerOptions, TestServer, TestUser } from '../../client/src/testing/index.js';
import { testUser } from '../../client/src/testing/index.js';
import { type World, world } from '../../client/test/helpers.js';
import { decodeNsRoot, STATE_INDEX } from '../src/auth.js';
import { decodeRoot } from '../src/index/node.js';
import { Db, DbError, type DbOptions, type Fields } from '../src/index.js';

interface Env {
  w: World;
  bob: TestUser;
  recoveryKey: Uint8Array;
  servers: TestServer[];
  closers: (() => Promise<unknown> | unknown)[];
}

let env: Env | undefined;

/** A server with the adversary, fs 1 created by the admin (a recovery slot), and bob. */
async function setup(opts: ServerOptions = {}): Promise<Env> {
  const w = await world({ adversary: true, ...opts });
  const bob = testUser(2);
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(bob.identity, [bob.cert]));
    doc.grants.push(...fullGrants(1, bob.fp));
  });
  let recoveryKey: Uint8Array = new Uint8Array(0);
  const u = await w.session.fs(1).init((k) => {
    const r = zw.createRecoverySlot(k);
    recoveryKey = r.recoveryKey;
    return [r.slot];
  });
  u.close();
  env = { w, bob, recoveryKey, servers: [w.server], closers: [] };
  return env;
}

afterEach(async () => {
  if (!env) return;
  for (const c of env.closers.reverse()) await Promise.resolve(c()).catch(() => {});
  for (const s of env.servers) await s.stop();
  env = undefined;
});

const pins = (): SealedStore => sealedStore(memoryLocalStore(), { device: false, plaintext: true });

interface Side {
  client: Client;
  session: Session;
  integ: Integrity;
  ufs: UnlockedFs;
}

/** Sign `user` in on `url` and unlock fs 1 with integrity (`signer`: the user's device signs). */
async function side(
  e: Env,
  user: TestUser,
  o: { url?: string; signer?: boolean; local?: SealedStore; integrity?: boolean } = {},
): Promise<Side> {
  const client = await connect(o.url ?? e.w.server.url, { origin: e.w.client.origin });
  const session = await client.signInDevice(user);
  const fs = session.fs(1);
  const integ =
    o.integrity === false
      ? (undefined as unknown as Integrity)
      : await fs.integrity({
          ...(o.signer === false ? {} : { device: { device: user.device, cert: user.cert } }),
          ...(o.local ? { local: o.local } : {}),
          sigEvery: 1000,
        });
  const ufs = await fs.unlock({ recoveryKey: e.recoveryKey });
  e.closers.push(() => ufs.close());
  return { client, session, integ, ufs };
}

interface Acct extends Fields {
  id: string;
  bal: number;
  owner?: string;
  email?: string;
  tag?: string;
  note?: string;
}

/** A database with `acct(id)` and an index of every kind (private, unique, fast, sealed). */
async function open(e: Env, s: Side, ns: string, opts: DbOptions = {}): Promise<Db> {
  const db = await Db.open(s.ufs, ns, {
    warn: () => {},
    locks: null,
    channel: null,
    scheduler: false,
    integrity: 'authenticated',
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('acct', { pk: ['id'] });
        await m.createIndex('acct', 'by_owner', {
          fields: [['owner', 'text']],
          kind: 'private',
          fanout: 4,
        });
        await m.createIndex('acct', 'by_email', {
          fields: [['email', 'text']],
          kind: 'none',
          unique: true,
        });
        await m.createIndex('acct', 'by_tag', { fields: [['tag', 'text']], kind: 'fast' });
        await m.createIndex('acct', 'by_bal', { fields: [['bal', 'int']], kind: 'sealed' });
      },
    },
    ...opts,
  });
  e.closers.push(() => db.close());
  return db;
}

async function alarmOf(p: Promise<unknown>): Promise<IntegrityError> {
  try {
    await p;
  } catch (err) {
    if (err instanceof IntegrityError) return err;
    throw err;
  }
  throw new Error('no alarm');
}

async function code(p: Promise<unknown>): Promise<string> {
  try {
    await p;
  } catch (err) {
    if (err instanceof DbError || err instanceof ZenError) return err.code;
    throw err;
  }
  return 'ok';
}

/** The NsRoot as stored now (honest reads only). */
async function nsRoot(s: Side, db: Db) {
  const [e] = await new Transaction(s.ufs, 'short').snapshotGet([db.keys.nsRoot()]);
  return e ? decodeNsRoot(e.value) : undefined;
}

/** The namespace's floor on `integ`. */
const dbFloor = (integ: Integrity, db: Db) => integ.floor(`3${bytes.hex(db.keys.db.prefix)}`);

/** Every value under D with an entry (not nodes, not the NsRoot), read honestly. */
async function values(s: Side, db: Db): Promise<[Uint8Array, Uint8Array][]> {
  const [begin, end] = db.keys.range();
  const out: [Uint8Array, Uint8Array][] = [];
  for await (const e of s.ufs.kv.rangeStored(begin, end)) {
    if (db.nsAuth!.covers(e.key)) out.push([e.key, e.value]);
  }
  return out;
}

/** The NsRoot's tree equals the reference build over every value (§14.2). */
async function expectScratch(s: Side, db: Db): Promise<void> {
  const n = db.keys.db.prefix.length;
  const entries = (await values(s, db)).map(([k, v]) => zw.stateEntry(k.subarray(n), v));
  const ref = db.indexKeys(STATE_INDEX).build(entries, 64, 32);
  const r = await nsRoot(s, db);
  if (!entries.length) {
    expect(r?.tree).toBeUndefined();
    return;
  }
  // The reference ends with the RootRecord: the NsRoot names the same tree.
  expect(r?.tree).toEqual(decodeRoot(ref.at(-1)!, 32));
}

const sum = (rows: Acct[]) => rows.reduce((a, r) => a + r.bal, 0);

describe('authenticated databases', () => {
  it('honest: every index kind, migrations, parts, a reopen from the local store; no alarms', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const local = sealedStore(memoryLocalStore());
    const db = await open(e, a, 'app', { local, pageSize: 3 });
    expect(db.integrity).toBe('authenticated');
    const t = db.table<Acct>('acct');
    for (let i = 0; i < 12; i++) {
      await t.insert({
        id: `a${i}`,
        bal: i * 10,
        owner: `o${i % 3}`,
        email: `u${i}@x`,
        tag: i % 2 ? 'odd' : 'even',
      });
    }
    await t.update('a1', (r) => ({ ...r, bal: 999 }));
    await t.delete('a2');
    // A large row, in parts (§4.2).
    await t.put({ id: 'big', bal: 1, note: 'x'.repeat(3 * db.limits.max_value_bytes) });
    expect((await t.get('big'))?.note?.length).toBe(3 * db.limits.max_value_bytes);
    // Queries over every index kind, and a scan.
    const byOwner = await t.query().where('owner', '=', 'o1').orderBy('owner').all();
    expect(byOwner.map((r) => r.id).sort()).toEqual(['a1', 'a10', 'a4', 'a7']);
    expect((await t.query().where('email', '=', 'u5@x').all()).map((r) => r.id)).toEqual(['a5']);
    expect((await t.query().where('tag', '=', 'odd').all()).length).toBe(6);
    expect((await t.query().orderBy('bal', 'desc').limit(2).all()).map((r) => r.id)).toEqual([
      'a1',
      'a11',
    ]);
    expect((await t.query().all()).length).toBe(12);
    await expectScratch(a, db);
    await db.close();

    // A migration that backfills an index and drops a table, in pages (§8).
    const db2 = await open(e, a, 'app', {
      local,
      pageSize: 3,
      schema: 3,
      migrations: {
        1: async () => {},
        2: async (m) => {
          await m.createTable('tmp', { pk: ['id'] });
          await m.createIndex('acct', 'by_bal2', { fields: [['bal', 'int']], kind: 'private' });
        },
        3: async (m) => {
          await m.dropTable('tmp');
        },
      },
    });
    expect((await db2.table<Acct>('acct').query().orderBy('bal').limit(3).all()).length).toBe(3);
    await expectScratch(a, db2);
    // Kept rows come back from the local store with their VH and are used.
    await db2.table<Acct>('acct').get('a3');
    await db2.close();
    const db3 = await open(e, a, 'app', {
      local,
      schema: 3,
      migrations: { 1: async () => {}, 2: async () => {}, 3: async () => {} },
    });
    const hits = db3.stats.rowHits;
    expect((await db3.table<Acct>('acct').get('a3'))?.bal).toBe(30);
    expect(db3.stats.rowHits).toBe(hits + 1);

    // Bob verifies alice's checkpoint and reads the same database.
    await a.integ.checkpoint();
    const b = await side(e, e.bob);
    await b.integ.sync();
    const floor = dbFloor(b.integ, db3);
    expect(floor).toMatchObject({ type: 'db', seq: (await nsRoot(a, db3))!.seq });
    const dbB = await open(e, b, 'app', {
      schema: 3,
      migrations: { 1: async () => {}, 2: async () => {}, 3: async () => {} },
    });
    expect(sum(await dbB.table<Acct>('acct').query().all())).toBe(
      sum(await db3.table<Acct>('acct').query().all()),
    );
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });

  it('the scheduler, request parts and gc keep the tree exact', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const db = await open(e, a, 'sch', { scheduler: true, schedulerOptions: { pollMs: 50 } });
    const topic = db.topic('jobs');
    // A large body with a ttl: parts under ("m"), and a gc job (§10.3).
    await db.emit(topic, 'big', 'y'.repeat(2 * db.limits.max_value_bytes), { ttlMs: 50 });
    await db.schedule(topic, 'later', { n: 1 }, { delayMs: 50 });
    const deadline = Date.now() + 15_000;
    for (;;) {
      const left = await db.transaction((tx) => tx.table('$sched').query().all());
      if (!left.length) break;
      if (Date.now() > deadline) throw new Error('the scheduler never ran');
      await new Promise((r) => setTimeout(r, 100));
    }
    await expectScratch(a, db);
    expect(a.integ.alarms).toEqual([]);
  });

  it('state equals scratch over random puts, deletes and a paged drop', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const db = await open(e, a, 'prop', { pageSize: 7 });
    let x = 0x2545f491;
    const rnd = (n: number) => {
      x ^= x << 13;
      x ^= x >>> 17;
      x ^= x << 5;
      return (x >>> 0) % n;
    };
    const t = db.table<Acct>('acct');
    for (let step = 0; step < 30; step++) {
      await db.transaction(async (tx) => {
        const tt = tx.table<Acct>('acct');
        for (let k = 0; k < 10; k++) {
          const id = `r${rnd(60)}`;
          if (rnd(4) === 0) await tt.delete(id);
          else await tt.put({ id, bal: rnd(1000), owner: `o${rnd(5)}`, tag: `t${rnd(3)}` });
        }
      });
      if (step % 10 === 9) await expectScratch(a, db);
    }
    expect((await t.query().all()).length).toBeGreaterThan(0);
    await db.close();
    const db2 = await open(e, a, 'prop', {
      pageSize: 7,
      schema: 2,
      migrations: { 1: async () => {}, 2: (m) => m.dropTable('acct') },
    });
    await expectScratch(a, db2);
    expect(a.integ.alarms).toEqual([]);
  });

  it('concurrent writers on two devices conflict on the NsRoot and lose nothing', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    const dbA = await open(e, a, 'bank');
    for (let i = 0; i < 4; i++) await dbA.table<Acct>('acct').insert({ id: `k${i}`, bal: 100 });
    const dbB = await open(e, b, 'bank');
    let runs = 0;
    let moves = 0;
    const loop = async (db: Db, seed: number) => {
      for (let j = 0; j < 5; j++) {
        await db.transaction(async (tx) => {
          runs++;
          const tt = tx.table<Acct>('acct');
          const from = `k${(seed + j) % 4}`;
          const to = `k${(seed + j + 1) % 4}`;
          const f = (await tt.get(from))!;
          const g = (await tt.get(to))!;
          await tt.put({ ...f, bal: f.bal - 7 });
          await tt.put({ ...g, bal: g.bal + 7 });
        });
        moves++;
      }
    };
    await Promise.all([0, 1, 2].flatMap((s) => [loop(dbA, s), loop(dbB, s + 1)]));
    expect(moves).toBe(30);
    expect(runs).toBeGreaterThan(30); // some conflicted and ran again
    expect(sum(await dbA.table<Acct>('acct').query().all())).toBe(400);
    await a.integ.checkpoint();
    await b.integ.checkpoint();
    await a.integ.sync();
    await b.integ.sync();
    expect(sum(await dbB.table<Acct>('acct').query().all())).toBe(400);
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });

  it('a rolled-back row: served silently by a basic database, caught by an authenticated one', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    // The attack works on a basic database.
    const basicA = await open(e, a, 'plain', { integrity: 'basic' });
    const basicB = await open(e, b, 'plain', { integrity: 'basic' });
    await basicA.table<Acct>('acct').insert({ id: 'r', bal: 10 });
    await lie(e.w.session, [
      { kind: 'kv', fs: 1, key: basicA.keys.row(rowTable(basicA), pkOf('r')) },
    ]);
    await basicB.table<Acct>('acct').update('r', (x) => ({ ...x, bal: 20 }));
    expect((await fresh(basicA).get('r'))?.bal).toBe(10);
    await truth(e.w.session);

    const dbA = await open(e, a, 'auth');
    const dbB = await open(e, b, 'auth');
    await dbA.table<Acct>('acct').insert({ id: 'r', bal: 10, owner: 'x' });
    await lie(e.w.session, [{ kind: 'kv', fs: 1, key: dbA.keys.row(rowTable(dbA), pkOf('r')) }]);
    await dbB.table<Acct>('acct').update('r', (x) => ({ ...x, bal: 20 }));
    const err = await alarmOf(fresh(dbA).get('r'));
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
    expect(a.integ.trusted).toBe(false);
    // A query reading the row is caught too (bob's view, a fresh Integrity).
    const c = await side(e, e.bob, { signer: false });
    const dbC = await open(e, c, 'auth');
    const err2 = await alarmOf(dbC.table<Acct>('acct').query().where('owner', '=', 'x').all());
    expect(err2.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
  });

  it('a rolled-back private index root is caught by a query', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    const dbA = await open(e, a, 'ixr');
    const dbB = await open(e, b, 'ixr');
    await dbA.table<Acct>('acct').insert({ id: 'p', bal: 1, owner: 'm' });
    const ix = indexOf(dbA, 'by_owner');
    await lie(e.w.session, [{ kind: 'kv', fs: 1, key: dbA.keys.root(ix, 0) }]);
    await dbB.table<Acct>('acct').insert({ id: 'q', bal: 2, owner: 'n' });
    const err = await alarmOf(dbA.table<Acct>('acct').query().orderBy('owner').all());
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
  });

  it('a withheld row: a scan and a fast lookup are caught', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    const dbA = await open(e, a, 'wh');
    const dbB = await open(e, b, 'wh');
    await dbA.table<Acct>('acct').insert({ id: 'one', bal: 1, tag: 't' });
    // The server will keep the row absent from what it reads.
    await lie(e.w.session, [{ kind: 'kv', fs: 1, key: dbA.keys.row(rowTable(dbA), pkOf('two')) }]);
    await dbB.table<Acct>('acct').insert({ id: 'two', bal: 2, tag: 't' });
    const scan = await alarmOf(dbA.table<Acct>('acct').query().all());
    expect(scan.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
    expect(scan.alarm.detail).toContain('left out');
    const c = await side(e, e.bob, { signer: false });
    const dbC = await open(e, c, 'wh');
    const fast = await alarmOf(dbC.table<Acct>('acct').query().where('tag', '=', 't').all());
    expect(fast.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
  });

  it('a cached row whose entry changed is read again, with no alarm', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    const dbA = await open(e, a, 'cache');
    const dbB = await open(e, b, 'cache');
    await dbA.table<Acct>('acct').insert({ id: 'c', bal: 1 });
    expect((await dbA.table<Acct>('acct').get('c'))?.bal).toBe(1); // cached
    await dbB.table<Acct>('acct').update('c', (x) => ({ ...x, bal: 2 }));
    // Read-only: the cached copy is stale by the tree, so it is read again.
    expect((await dbA.table<Acct>('acct').get('c'))?.bal).toBe(2);
    await dbB.table<Acct>('acct').update('c', (x) => ({ ...x, bal: 3 }));
    // In a write transaction too.
    await dbA.table<Acct>('acct').update('c', (x) => ({ ...x, bal: x.bal * 10 }));
    expect((await dbB.table<Acct>('acct').get('c'))?.bal).toBe(30);
    expect([...a.integ.alarms, ...b.integ.alarms]).toEqual([]);
  });

  it('a rolled-back NsRoot: a consistent snapshot passes in the window, and is caught after a sync', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const b = await side(e, e.bob);
    const dbA = await open(e, a, 'nsr');
    await dbA.table<Acct>('acct').insert({ id: 'v', bal: 1 });
    const dbB = await open(e, b, 'nsr');
    // Freeze the database as it is: the NsRoot, its tree, and the row.
    const rowKey = dbA.keys.row(rowTable(dbA), pkOf('v'));
    await lie(e.w.session, await snapshot(a, dbA, [rowKey]));
    await dbA.table<Acct>('acct').update('v', (x) => ({ ...x, bal: 2 }));
    await a.integ.checkpoint();
    // B hasn't synced: the old snapshot is consistent, and passes (the window).
    const c = await side(e, e.bob, { signer: false });
    expect((await fresh(await open(e, c, 'nsr')).get('v'))?.bal).toBe(1);
    expect((await fresh(dbB).get('v'))?.bal).toBe(1);
    expect([...b.integ.alarms, ...c.integ.alarms]).toEqual([]);
    // Once B verifies A's checkpoint, the NsRoot is below the floor.
    await b.integ.sync();
    const err = await alarmOf(fresh(dbB).get('v'));
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
    expect(err.alarm.detail).toContain('went back');
    // A's own next read is below its own note.
    const errA = await alarmOf(fresh(dbA).get('v'));
    expect(errA.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
  });

  it('a rolled-back NsRoot whose tree is gone is caught without a checkpoint', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const dbA = await open(e, a, 'nsr2');
    await dbA.table<Acct>('acct').insert({ id: 'v', bal: 1 });
    // Freeze the NsRoot and the row, but not the tree: every commit replaces its root node.
    const rowKey = dbA.keys.row(rowTable(dbA), pkOf('v'));
    await lie(e.w.session, [
      { kind: 'kv', fs: 1, key: dbA.keys.nsRoot() },
      { kind: 'kv', fs: 1, key: rowKey },
    ]);
    await dbA.table<Acct>('acct').update('v', (x) => ({ ...x, bal: 2 }));
    const c = await side(e, e.bob, { signer: false });
    const err = await alarmOf(open(e, c, 'nsr2').then((d) => fresh(d).get('v')));
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
    expect(err.alarm.detail).toContain('missing');
  });

  it('a fork: two servers on one history; a reader that moves across sees two NsRoots at one seq', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const db0 = await open(e, a, 'fk');
    await db0.table<Acct>('acct').insert({ id: 'f', bal: 1 });
    await db0.close();
    a.ufs.close();

    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    const a1 = await side(e, e.w.admin, { url: self.url });
    const b2 = await side(e, e.bob, { url: copy.url });
    const d1 = await open(e, a1, 'fk');
    const d2 = await open(e, b2, 'fk');
    await d1.table<Acct>('acct').update('f', (x) => ({ ...x, bal: 11 }));
    await d2.table<Acct>('acct').update('f', (x) => ({ ...x, bal: 22 }));
    const r1 = (await nsRoot(a1, d1))!;
    const r2 = (await nsRoot(b2, d2))!;
    expect(r1.seq).toBe(r2.seq);
    await a1.integ.checkpoint();
    await b2.integ.checkpoint();
    await a1.integ.sync();
    await b2.integ.sync();
    expect([...a1.integ.alarms, ...b2.integ.alarms]).toEqual([]);

    // A third client verifies A's checkpoint on S1, then reads the database on S2.
    const lc = pins();
    const c1 = await side(e, e.w.admin, { url: self.url, signer: false, local: lc });
    await c1.integ.sync();
    expect(dbFloor(c1.integ, d1)?.type).toBe('db');
    c1.ufs.close();
    const c2 = await side(e, e.w.admin, { url: copy.url, signer: false, local: lc });
    const err = await alarmOf(open(e, c2, 'fk'));
    expect(err.alarm).toMatchObject({ kind: 'fork', object: 'db' });
  });

  it('opening: the level is kept, needs integrity, and an absent database with a floor is a rollback', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    await open(e, a, 'b1', { integrity: 'basic' });
    expect(await code(open(e, a, 'b1'))).toBe('format');
    const plain = await side(e, e.w.admin, { integrity: false });
    expect(await code(open(e, plain, 'new1'))).toBe('unsupported');
    await open(e, a, 'au1');
    const re = await open(e, a, 'au1', { integrity: undefined });
    expect(re.integrity).toBe('authenticated');
    expect(await code(open(e, plain, 'au1', { integrity: undefined }))).toBe('unsupported');

    // A copy of the store before the database exists; then it is created and checkpointed.
    a.ufs.close();
    const { self, copy } = await e.w.server.fork();
    e.servers = [self, copy];
    const a1 = await side(e, e.w.admin, { url: self.url });
    await open(e, a1, 'gone');
    await a1.integ.checkpoint();
    const lc = pins();
    const c1 = await side(e, e.w.admin, { url: self.url, signer: false, local: lc });
    await c1.integ.sync();
    c1.ufs.close();
    const c2 = await side(e, e.w.admin, { url: copy.url, signer: false, local: lc });
    const err = await alarmOf(open(e, c2, 'gone'));
    expect(err.alarm).toMatchObject({ kind: 'rollback', object: 'db' });
  });

  it('refuses CRDT tables', async () => {
    const e = await setup();
    const a = await side(e, e.w.admin);
    const r = open(e, a, 'cr', {
      schema: 1,
      migrations: { 1: (m) => m.createTable('c', { pk: ['id'], merge: 'crdt' }) },
    });
    expect(await code(r)).toBe('bad_value');
  });
});

// ------------------------------------------------------------ helpers

/** Lies that freeze a database as it is now, consistently: its NsRoot, every state-tree node, and `keys`. */
async function snapshot(s: Side, db: Db, keys: Uint8Array[]): Promise<Lie[]> {
  const [begin, end] = db.keys.range('n', STATE_INDEX);
  const nodes: Uint8Array[] = [];
  for await (const e of s.ufs.kv.rangeStored(begin, end)) nodes.push(e.key);
  return [db.keys.nsRoot(), ...nodes, ...keys].map((key) => ({ kind: 'kv' as const, fs: 1, key }));
}

const pkOf = (id: string) => encodePk(id);

function encodePk(id: string): Uint8Array {
  // CBOR text: the pk element (§4.1).
  const b = bytes.utf8(id);
  return bytes.concat(new Uint8Array([0x60 + b.length]), b);
}

function rowTable(db: Db): Uint8Array {
  const t = db.catalogEntry('acct')?.record;
  if (!t) throw new Error('no acct table cached');
  return t.id;
}

function indexOf(db: Db, name: string): Uint8Array {
  const ix = db.catalogEntry('acct')?.record?.indexes.find((i) => i.name === name);
  if (!ix) throw new Error(`no index ${name}`);
  return ix.id;
}

/** The table, with its cached rows dropped first: the read goes to the server. */
function fresh(db: Db) {
  for (const k of [...(db as unknown as { rows: Map<string, unknown> }).rows.keys()]) db.dropRow(k);
  return db.table<Acct>('acct');
}
