// The caches on the sealed local store, and opening offline (zendb.md §3.1,
// §7.7, §9.3, §19.4). Offline is a wrapped `session.call` that fails with
// `network`; a "restart" is a new `Db` on the same local store.
import {
  bytes,
  type LocalStore,
  memoryLocalStore,
  type SealedStore,
  type Session,
  sealedStore,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { Db, DbError, type DbOptions, type Fields } from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `dbp${++n}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world();
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

/** Another session of this device: a process of its own. */
async function signIn(): Promise<Session> {
  return w.client.signInDevice(w.admin);
}

/** A session's network: calls by path, and a switch to go offline. */
function net(s: Session) {
  const orig = s.call.bind(s);
  const n = {
    offline: false,
    calls: [] as string[],
    count: (p: string) => n.calls.filter((c) => c === p).length,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.offline) throw new ZenError(0, 'network', 'offline (test)');
    n.calls.push(path);
    return orig(path, enc, dec, req);
  }) as Session['call'];
  return n;
}

interface Item extends Fields {
  id: string;
  status: string;
  n: number;
  tag: string;
}
interface Card extends Fields {
  id: string;
  title?: string;
  votes?: number;
}

const migrations: DbOptions['migrations'] = {
  1: async (m) => {
    await m.createTable('items', { pk: ['id'] });
    await m.createIndex('items', 'by_status', {
      fields: [
        ['status', 'text'],
        ['n', 'int'],
      ],
      fanout: 2,
    });
    await m.createIndex('items', 'by_tag', { fields: [['tag', 'text']], kind: 'sealed' });
    await m.createTable('cards', { pk: ['id'], merge: 'crdt', fields: { votes: 'counter' } });
  },
  2: async (m) => {
    await m.createTable('extra', { pk: ['id'] });
  },
};

const open = (f: UnlockedFs, name: string, extra: DbOptions = {}) =>
  Db.open(f, name, {
    warn: () => {},
    locks: null,
    scheduler: false,
    indexer: false,
    schema: 1,
    migrations,
    ...extra,
  });

const device = () => ({ device: w.admin.device });

/** A database with rows, seen once by a `Db` on `local` (its caches filled), then closed. */
async function seeded(local: SealedStore, name: string, f: UnlockedFs = fs) {
  const db = await open(f, name, { local });
  const items = db.table<Item>('items');
  for (let i = 0; i < 6; i++) {
    await items.insert({ id: `i${i}`, status: i % 2 ? 'b' : 'a', n: i, tag: i < 3 ? 'x' : 'y' });
  }
  await db.table<Card>('cards', { wait: true }).insert({ id: 'c1', title: 'one' });
  await db.table<Card>('cards', { wait: true }).incr('c1', 'votes', 2);
  // Read: the caches fill.
  for (let i = 0; i < 6; i++) await items.get(`i${i}`);
  await items.query().where('status', '=', 'a').orderBy('n').all();
  await items.query().where('tag', '=', 'x').all();
  await db.table<Card>('cards').get('c1', { read: 'server' });
  await db.close();
}

const code = async (p: Promise<unknown>) => {
  try {
    await p;
  } catch (e) {
    if (e instanceof DbError || e instanceof ZenError) return e.code;
    throw e;
  }
  return 'ok';
};

async function records(raw: LocalStore, store: string): Promise<number> {
  let k = 0;
  for await (const _ of raw.list(store)) k++;
  return k;
}

describe('opening offline (§3.1)', () => {
  it('a cold start offline: unlock, open, read the caches, queue writes', async () => {
    const name = ns();
    const s = await signIn();
    const local = sealedStore(memoryLocalStore());
    const f = await s.fs(1).unlock(device(), { local });
    await seeded(local, name, f);
    f.close();

    // A new process, offline: the fs and the database open from the store.
    const na = net(s);
    na.offline = true;
    const f2 = await s.fs(1).unlock(device(), { local });
    const db = await open(f2, name, { local });
    expect(db.offline).toBe(true);
    const cards = db.table<Card>('cards');
    expect(await cards.get('c1')).toMatchObject({ title: 'one', votes: 2 });
    expect(await cards.get('c1', { read: 'cache' })).toMatchObject({ title: 'one' });
    expect((await cards.query().read('cache').all()).map((c) => c.id)).toEqual(['c1']);
    // Transactional rows: only when asked for (`cache`), never by default.
    const items = db.table<Item>('items');
    expect(await items.get('i0', { read: 'cache' })).toMatchObject({ status: 'a', n: 0 });
    expect(await code(items.get('i0'))).toBe('network');
    expect(await code(items.get('nope', { read: 'cache' }))).toBe('offline');
    expect(await code(items.put({ id: 'i9', status: 'a', n: 9, tag: 'x' }))).toBe('network');
    // CRDT writes queue, and go once online.
    await cards.patch('c1', { title: 'offline edit' });
    expect((await cards.get('c1'))?.title).toBe('offline edit');
    na.restore();
    await db.crdt.flush();
    const check = await open(fs, name);
    expect((await check.table<Card>('cards').get('c1', { read: 'server' }))?.title).toBe(
      'offline edit',
    );
    await check.close();
    await db.close();
    f2.close();
  });

  it('a migration due offline is refused; online it runs', async () => {
    const name = ns();
    const s = await signIn();
    const local = sealedStore(memoryLocalStore());
    const f = await s.fs(1).unlock(device(), { local });
    await (await open(f, name, { local })).close();
    const na = net(s);
    na.offline = true;
    const err = await open(f, name, { local, schema: 2 }).catch((e) => e);
    expect(err).toBeInstanceOf(DbError);
    expect(err.code).toBe('offline');
    na.restore();
    const db = await open(f, name, { local, schema: 2 });
    expect(db.offline).toBe(false);
    expect(db.tableNames()).toContain('extra');
    await db.close();
    f.close();
  });

  it('nothing kept: the network error stands', async () => {
    const s = await signIn();
    const local = sealedStore(memoryLocalStore());
    const f = await s.fs(1).unlock(device(), { local });
    const na = net(s);
    na.offline = true;
    expect(await code(open(f, ns(), { local }))).toBe('network');
    na.restore();
    f.close();
  });
});

describe('caches that survive restarts (§9.3)', () => {
  it('a warm start reads rows, nodes and sealed blobs from the store', async () => {
    const name = ns();
    const local = sealedStore(memoryLocalStore());
    await seeded(local, name);
    // Calls of a query on a fresh instance, with and without the kept caches.
    const calls = async (opts: DbOptions) => {
      const s = await signIn();
      const f = await s.fs(1).unlock(device());
      const db = await open(f, name, opts);
      const nt = net(s);
      const items = db.table<Item>('items');
      const hits = db.stats.rowHits;
      await items.get('i1');
      const hit = db.stats.rowHits - hits;
      const a = await items.query().where('status', '=', 'a').orderBy('n').all();
      const x = await items.query().where('tag', '=', 'x').all();
      expect(a.map((r) => r.id)).toEqual(['i0', 'i2', 'i4']);
      expect(x.map((r) => r.id).sort()).toEqual(['i0', 'i1', 'i2']);
      nt.restore();
      await db.close();
      f.close();
      return { gets: nt.count('/v1/kv/get'), hit };
    };
    const warm = await calls({ local });
    const cold = await calls({ local, persist: false });
    expect(warm.hit).toBe(1);
    expect(cold.hit).toBe(0);
    // One row get, the node reads (one per tree level) and the sealed parts read.
    expect(cold.gets - warm.gets).toBeGreaterThanOrEqual(3);
  });

  it('a row changed while closed re-runs its transaction once, then reads the new value', async () => {
    const name = ns();
    const local = sealedStore(memoryLocalStore());
    await seeded(local, name);
    const other = await open(fs, name);
    await other.table<Item>('items').put({ id: 'i0', status: 'z', n: 0, tag: 'x' });
    await other.close();
    const db = await open(fs, name, { local });
    let runs = 0;
    const got = await db.transaction(async (tx) => {
      runs++;
      return tx.table<Item>('items').get('i0');
    });
    expect(got?.status).toBe('z');
    expect(runs).toBe(2);
    await db.close();
  });

  it('CRDT states: an old one costs a version check after a restart, not a get', async () => {
    const name = ns();
    const local = sealedStore(memoryLocalStore());
    await seeded(local, name);
    await sleep(150);
    const s = await signIn();
    const f = await s.fs(1).unlock(device());
    const db = await open(f, name, { local, crdtCacheMs: 100 });
    const nt = net(s);
    expect((await db.table<Card>('cards').get('c1'))?.title).toBe('one');
    expect([nt.count('/v1/crdt/versions'), nt.count('/v1/crdt/get')]).toEqual([1, 0]);
    nt.restore();
    await db.close();
    f.close();
  });

  it('a record tampered with is dropped with a warning and read again', async () => {
    const name = ns();
    const raw = memoryLocalStore();
    const warned: string[] = [];
    const local = sealedStore(raw, { warn: (m) => warned.push(m) });
    await seeded(local, name);
    for await (const [k, v] of raw.list(`rows/${name}`)) {
      const bad = v.slice();
      bad[bad.length - 1]! ^= 1;
      await raw.put(`rows/${name}`, k, bad);
    }
    const db = await open(fs, name, { local });
    expect(warned.length).toBeGreaterThan(0);
    const misses = db.stats.rowMisses;
    expect((await db.table<Item>('items').get('i0'))?.status).toBe('a');
    expect(db.stats.rowMisses).toBe(misses + 1);
    await db.close();
  });

  it('persist: false keeps the caches in memory; the queue stays kept', async () => {
    const name = ns();
    const raw = memoryLocalStore();
    const local = sealedStore(raw);
    const db = await open(fs, name, { local, persist: false });
    await db.table<Item>('items').insert({ id: 'i0', status: 'a', n: 0, tag: 'x' });
    await db.table<Item>('items').get('i0');
    await db.table<Card>('cards').insert({ id: 'c1' });
    await db.close();
    expect(await records(raw, `rows/${name}`)).toBe(0);
    expect(await records(raw, `db/${name}`)).toBe(0);
    const again = await open(fs, name, { local, persist: false });
    await again.crdt.flush();
    expect((await again.table<Card>('cards').get('c1', { read: 'server' }))?.id).toBe('c1');
    await again.close();
  });

  it('Db.wipe and Fs.forget leave nothing', async () => {
    const name = ns();
    const raw = memoryLocalStore();
    const local = sealedStore(raw);
    const s = await signIn();
    const f = await s.fs(1).unlock(device(), { local });
    await seeded(local, name, f);
    expect(await records(raw, `rows/${name}`)).toBeGreaterThan(0);
    await Db.wipe(local, name);
    await s.fs(1).forget(local);
    for (const st of ['queue', 'db', 'rows', 'crdt', 'nodes', 'sealed']) {
      expect(await records(raw, `${st}/${name}`)).toBe(0);
    }
    expect(await records(raw, 'fs/1')).toBe(0);
    expect(bytes.hex(new Uint8Array(0))).toBe('');
    f.close();
  });
});
