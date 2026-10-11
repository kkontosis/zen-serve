// Indexes on CRDT tables (zendb.md §19.6): the indexer, its lag, restarts,
// backfills and drops, against a spawned zen-serve.
import { bytes, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import type { IndexDef } from '../src/catalog.js';
import type { Value } from '../src/cbor.js';
import { encode } from '../src/cbor.js';
import { sortKeyOf } from '../src/index/private.js';
import { readSealed } from '../src/index/sealed.js';
import { Db, DbError, type DbOptions, type Fields, type Migrator } from '../src/index.js';
import { expectCanonical } from './helpers.js';

let w: World;
let fs: UnlockedFs;
let fs2: UnlockedFs;
let n = 0;
const ns = () => `ci${++n}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  const s = await w.client.signInDevice(w.admin);
  fs2 = await s.fs(1).unlock({ device: w.admin.device });
});
afterAll(() => w?.server.stop());

const code = async (p: Promise<unknown>) => {
  try {
    await p;
  } catch (e) {
    if (e instanceof DbError || e instanceof ZenError) return e.code;
    throw e;
  }
  return 'ok';
};

async function until(cond: () => boolean | Promise<boolean>, ms = 15_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(50);
  }
}

interface Card extends Fields {
  id: string;
  status?: string;
  rank?: Value;
  title?: string;
  votes?: number;
}

const table = (m: Migrator) =>
  m.createTable('cards', {
    pk: ['id'],
    merge: 'crdt',
    fields: { votes: 'counter', labels: 'set' },
    changes: {},
  });

const indexes = async (m: Migrator) => {
  await m.createIndex('cards', 'by_status', { kind: 'fast', fields: [['status', 'text']] });
  await m.createIndex('cards', 'by_rank', {
    fields: [
      ['status', 'text'],
      ['rank', 'int'],
    ],
    fanout: 4,
  });
  await m.createIndex('cards', 'by_title', { kind: 'sealed', fields: [['title', 'text']] });
  await m.createIndex('cards', 'by_votes', { fields: [['votes', 'int', 'desc']], fanout: 4 });
};

const migrations: DbOptions['migrations'] = {
  1: async (m) => {
    await table(m);
    await indexes(m);
  },
};

const open = (f: UnlockedFs, name: string, extra: DbOptions = {}) =>
  Db.open(f, name, {
    warn: () => {},
    // Online: writes commit at once, reads ask the server.
    crdtQueue: false,
    crdtRead: 'server',
    locks: null,
    scheduler: false,
    schema: 1,
    migrations,
    ...extra,
  });

const index = async (db: Db, name: string): Promise<IndexDef> => {
  const t = await db.transaction((tx) => tx.tableRecord('cards'));
  return t.indexes.find((i) => i.name === name)!;
};

/** Every live row, merged (a scan). */
const allRows = (db: Db) => db.table<Card>('cards').query().all();

const ids = (rows: Fields[]) => rows.map((r) => r.id);

/** The ids of the rows with this status, through the fast index (sorted). */
const byStatus = async (db: Db, s: string) =>
  ids(await db.table<Card>('cards').query().where('status', '=', s).all()).sort();

/** The `$ixrows` rows of a table: index name → values, by pk. */
async function ixRows(db: Db): Promise<Map<Value, Fields>> {
  const t = await db.transaction((tx) => tx.tableRecord('cards'));
  const rows = await db.table('$ixrows').query().all();
  const out = new Map<Value, Fields>();
  for (const r of rows) {
    if (!bytes.equal(r.table_id as Uint8Array, t.id)) continue;
    const named: Fields = {};
    for (const [k, v] of Object.entries(r.values as Fields)) {
      named[t.indexes.find((i) => bytes.hex(i.id) === k)?.name ?? '?'] = v;
    }
    out.set(r.pk!, named);
  }
  return out;
}

/** A sealed index's list against the rows (§5.7). */
async function expectSealed(db: Db, ix: IndexDef, rows: Fields[]) {
  const want = rows
    .map((r) =>
      sortKeyOf(
        ix,
        ix.fields.map(([f]) => r[f] ?? null),
        [r.id as Value],
      ),
    )
    .sort(bytes.compare)
    .map(bytes.hex);
  const got = await db.transaction(
    async (tx) => (await readSealed(tx, ix.id, () => undefined)).keys,
  );
  expect(got.map(bytes.hex)).toEqual(want);
}

/** Whether every index holds exactly `rows` (the indexer caught up). */
async function indexed(db: Db, rows: Card[]): Promise<boolean> {
  const ix = await ixRows(db);
  if (ix.size !== rows.length) return false;
  return rows.every((r) => {
    const v = ix.get(r.id);
    return (
      !!v && bytes.equal(encode(v.by_rank ?? null), encode([r.status ?? null, r.rank ?? null]))
    );
  });
}

async function seed(db: Db, rows: Card[]) {
  await db.transaction(async (tx) => {
    for (const r of rows) await tx.table<Card>('cards').insert(r);
  });
}

const cards = (k: number): Card[] =>
  Array.from({ length: k }, (_, i) => ({
    id: `c${String(i).padStart(2, '0')}`,
    status: i % 3 ? 'todo' : 'done',
    rank: (i * 7) % 11,
    title: `t${(i * 5) % 13}`,
  }));

describe('indexes on CRDT tables (§19.6)', () => {
  it('refuses unique indexes, and indexes without change events', async () => {
    const step = (fn: (m: Migrator) => Promise<void>) => open(fs, ns(), { migrations: { 1: fn } });
    expect(
      await code(
        step(async (m) => {
          await m.createTable('x', { pk: ['id'], merge: 'crdt' });
          await m.createIndex('x', 'i', { fields: [['a', 'text']] });
        }),
      ),
    ).toBe('bad_value');
    for (const o of [{ unique: true }, { unique: true, kind: 'none' as const }]) {
      expect(
        await code(
          step(async (m) => {
            await table(m);
            await m.createIndex('cards', 'i', { fields: [['status', 'text']], ...o });
          }),
        ),
      ).toBe('bad_value');
    }
  });

  it('fast, private and sealed queries once the indexer caught up', async () => {
    const db = await open(fs, ns());
    const rows = cards(12);
    await seed(db, rows);
    await until(() => indexed(db, rows));
    const t = db.table<Card>('cards');

    expect(await byStatus(db, 'done')).toEqual(['c00', 'c03', 'c06', 'c09']);
    // A private range, both ways, in pages of 3.
    const todo = rows.filter((r) => r.status === 'todo');
    for (const dir of ['asc', 'desc'] as const) {
      const want = [...todo]
        .sort((a, b) => ((a.rank as number) - (b.rank as number)) * (dir === 'asc' ? 1 : -1))
        .map((r) => r.id);
      const got: string[] = [];
      let p = await t.query().where('status', '=', 'todo').orderBy('rank', dir).limit(3).page();
      for (;;) {
        got.push(...p.rows.map((r) => r.id));
        if (!p.cursor) break;
        p = await t
          .query()
          .where('status', '=', 'todo')
          .orderBy('rank', dir)
          .limit(3)
          .after(p.cursor)
          .page();
      }
      expect(got).toEqual(want);
    }
    // Without an order, the fast index is the path: the range is re-checked.
    expect(
      ids(await t.query().where('status', '=', 'todo').where('rank', '>=', 5).all()).sort(),
    ).toEqual(todo.filter((r) => (r.rank as number) >= 5).map((r) => r.id));
    // A sealed range.
    expect(ids(await t.query().where('title', 'prefix', 't1').orderBy('title').all())).toEqual(
      rows
        .filter((r) => r.title!.startsWith('t1'))
        .sort((a, b) => (a.title! < b.title! ? -1 : a.title! > b.title! ? 1 : a.id < b.id ? -1 : 1))
        .map((r) => r.id),
    );
    await expectCanonical(db, await index(db, 'by_rank'), rows);
    await expectSealed(db, await index(db, 'by_title'), rows);
    db.close();
  });

  it('indexes merged counters from two installations', async () => {
    const name = ns();
    const a = await open(fs, name);
    const b = await open(fs2, name, { indexer: false });
    await seed(a, [{ id: 'x' }, { id: 'y' }, { id: 'z' }]);
    await Promise.all([
      a.table('cards').incr('x', 'votes', 2),
      b.table('cards').incr('x', 'votes', 3),
      b.table('cards').incr('y', 'votes', 4),
      a.table('cards').incr('z', 'votes', 1),
    ]);
    const top = () =>
      a.table<Card>('cards').query().where('votes', '>=', 0).orderBy('votes', 'desc').all();
    await until(async () => ids(await top()).join() === 'x,y,z');
    expect((await top()).map((r) => r.votes)).toEqual([5, 4, 1]);
    a.close();
    b.close();
  });

  it('lags without an indexer, re-checks what it returns, and catches up', async () => {
    const name = ns();
    const first = await open(fs, name);
    const rows = cards(6);
    await seed(first, rows);
    await until(() => indexed(first, rows));
    first.close();

    const wr = await open(fs, name, { indexer: false });
    await wr.table<Card>('cards').patch('c01', { status: 'done' });
    // The entry for `todo` is stale: the re-check drops the row; `done` misses it.
    expect(await byStatus(wr, 'todo')).toEqual(['c02', 'c04', 'c05']);
    expect(await byStatus(wr, 'done')).toEqual(['c00', 'c03']);
    const late = await wr
      .table<Card>('cards')
      .query()
      .where('status', '=', 'done')
      .orderBy('rank')
      .all();
    expect(ids(late)).toEqual(['c00', 'c03']);

    const ixer = await open(fs, name);
    await until(async () => (await byStatus(wr, 'done')).join() === 'c00,c01,c03');
    rows[1]!.status = 'done';
    await until(() => indexed(wr, rows));
    await expectCanonical(wr, await index(wr, 'by_rank'), rows);
    wr.close();
    ixer.close();
  });

  it('resumes after the indexer stops mid-stream', async () => {
    const name = ns();
    const wr = await open(fs, name, { indexer: false });
    const ix1 = await open(fs, name);
    const rows = cards(20);
    const writes = (async () => {
      for (const r of rows) {
        await wr.table<Card>('cards').insert(r);
        await sleep(5);
      }
    })();
    await sleep(60);
    ix1.close();
    await writes;
    for (const r of rows.slice(0, 5)) {
      r.rank = (r.rank as number) + 100;
      await wr.table<Card>('cards').patch(r.id, { rank: r.rank });
    }
    const ix2 = await open(fs, name);
    await until(() => indexed(wr, rows), 20_000);
    await expectCanonical(wr, await index(wr, 'by_rank'), rows);
    await expectSealed(wr, await index(wr, 'by_title'), rows);
    expect(await allRows(wr)).toHaveLength(20);
    wr.close();
    ix2.close();
  });

  it('removes deleted rows; a value of the wrong type leaves that index only', async () => {
    const warnings: string[] = [];
    const db = await open(fs, ns(), { warn: (m) => warnings.push(m) });
    const rows = cards(6);
    await seed(db, rows);
    await until(() => indexed(db, rows));
    await db.table('cards').delete('c00');
    await db.table('cards').patch('c01', { rank: 'high' });
    const live = rows.filter((r) => r.id !== 'c00');
    await until(async () => (await ixRows(db)).get('c01')?.by_rank === undefined);
    await until(async () => (await ixRows(db)).size === 5);
    expect(await byStatus(db, 'done')).toEqual(['c03']);
    expect((await ixRows(db)).get('c01')?.by_status).toEqual(['todo']);
    expect(warnings.some((m) => m.includes("can't hold"))).toBe(true);
    await expectCanonical(
      db,
      await index(db, 'by_rank'),
      live.filter((r) => r.id !== 'c01'),
    );
    await expectSealed(db, await index(db, 'by_title'), live);
    db.close();
  });

  it('backfills existing rows', async () => {
    const name = ns();
    const plain = { 1: table };
    const a = await open(fs, name, { migrations: plain });
    const rows = cards(7);
    await seed(a, rows);
    a.close();
    const b = await open(fs, name, {
      schema: 2,
      migrations: { ...plain, 2: indexes },
      pageSize: 2,
      indexer: false,
    });
    await expectCanonical(b, await index(b, 'by_rank'), rows);
    await expectSealed(b, await index(b, 'by_title'), rows);
    expect(await byStatus(b, 'todo')).toEqual(
      rows.filter((r) => r.status === 'todo').map((r) => r.id),
    );
    expect(await indexed(b, rows)).toBe(true);
    b.close();
  });

  it('backfills while rows change, with the indexer running', async () => {
    const name = ns();
    const one = {
      1: async (m: Migrator) => {
        await table(m);
        await m.createIndex('cards', 'by_status', { kind: 'fast', fields: [['status', 'text']] });
      },
    };
    const a = await open(fs, name, { migrations: one });
    const rows = cards(16);
    await seed(a, rows);
    let stop = false;
    const writes = (async () => {
      for (let i = 0; !stop; i++) {
        const r = rows[i % rows.length]!;
        r.rank = ((r.rank as number) + 3) % 17;
        await a.table<Card>('cards').patch(r.id, { rank: r.rank });
      }
    })();
    const b = await open(fs, name, {
      schema: 2,
      migrations: {
        ...one,
        2: (m) =>
          m.createIndex('cards', 'by_rank', {
            fields: [
              ['status', 'text'],
              ['rank', 'int'],
            ],
            fanout: 4,
          }),
      },
      pageSize: 2,
      indexer: false,
    });
    stop = true;
    await writes;
    await until(() => indexed(b, rows), 20_000);
    await expectCanonical(b, await index(b, 'by_rank'), rows);
    a.close();
    b.close();
  });

  it('drops an index and a table with their entries', async () => {
    const name = ns();
    const a = await open(fs, name);
    const rows = cards(5);
    await seed(a, rows);
    await until(() => indexed(a, rows));
    const ixs = (await a.transaction((tx) => tx.tableRecord('cards'))).indexes;
    a.close();
    const keysIn = (db: Db, ix: IndexDef) =>
      db.transaction(async (tx) => {
        let k = 0;
        for (const p of ['f', 'n', 'r', 's']) {
          k += (await tx.raw.rangeStored(...db.keys.range(p, ix.id))).length;
        }
        return k;
      });

    const b = await open(fs, name, {
      schema: 2,
      migrations: { ...migrations, 2: (m) => m.dropIndex('cards', 'by_rank') },
      indexer: false,
    });
    const rank = ixs.find((i) => i.name === 'by_rank')!;
    expect(await keysIn(b, rank)).toBe(0);
    expect((await index(b, 'by_rank')) ?? null).toBeNull();
    expect(await keysIn(b, ixs.find((i) => i.name === 'by_status')!)).toBeGreaterThan(0);
    b.close();

    const c = await open(fs, name, {
      schema: 3,
      migrations: {
        ...migrations,
        2: (m) => m.dropIndex('cards', 'by_rank'),
        3: (m) => m.dropTable('cards'),
      },
      indexer: false,
      pageSize: 2,
    });
    for (const ix of ixs) expect(await keysIn(c, ix)).toBe(0);
    expect(await c.table('$ixrows').query().all()).toEqual([]);
    c.close();
  });
});
