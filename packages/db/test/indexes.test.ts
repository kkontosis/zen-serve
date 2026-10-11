// Private and sealed indexes and ordered queries against a spawned
// zen-serve (zendb.md §5.4–5.7, §6).
import { bytes, type UnlockedFs, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import type { IndexDef } from '../src/catalog.js';
import { encode, type Value } from '../src/cbor.js';
import { decodeNode, nodeBucket } from '../src/index/node.js';
import { sortKeyOf } from '../src/index/private.js';
import { decodeBlob, decodeHead } from '../src/index/sealed.js';
import { Db, DbError, type DbOptions, type Fields, type Query } from '../src/index.js';
import type { FieldSpec } from '../src/migrate.js';
import { type Cursor, compareValues } from '../src/query.js';
import { joinParts } from '../src/row.js';
import { expectCanonical } from './helpers.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `ix${++n}`;
const quiet = { warn: () => {} };

beforeAll(async () => {
  w = await world({ limits: { max_range_items: 100 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const code = async (p: Promise<unknown>) => {
  try {
    await p;
  } catch (e) {
    if (e instanceof DbError) return e.code;
    throw e;
  }
  return 'ok';
};

function barrier(n: number) {
  let left = n;
  let open: () => void;
  const p = new Promise<void>((r) => {
    open = r;
  });
  return () => {
    if (--left <= 0) open();
    return p;
  };
}

interface Task extends Fields {
  id: string;
  status?: string | null;
  created?: number | null;
  title?: string;
}

const FIELDS: FieldSpec[] = [
  ['status', 'text'],
  ['created', 'int', 'desc'],
];

/** A database with `tasks(id)` and one index of `kind` on (status, created desc). */
function tasksDb(
  kind: 'private' | 'sealed',
  opts: {
    unique?: boolean;
    shards?: number;
    decoys?: number;
    fanout?: number;
    maxBytes?: number;
  } = {},
  extra: DbOptions = {},
  name = ns(),
): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('tasks', { pk: ['id'] });
        await m.createIndex('tasks', 'by_status', { fields: FIELDS, kind, ...opts });
      },
    },
    ...extra,
  });
}

const indexOf = (db: Db, table: string, name: string): IndexDef =>
  db.catalogEntry(table)!.record!.indexes.find((i) => i.name === name)!;

/** Every page of a query, following cursors. */
async function allPages<T extends Fields>(q: () => Query<T>, limit: number): Promise<T[]> {
  const out: T[] = [];
  let cursor: Cursor | undefined;
  for (let i = 0; i < 1000; i++) {
    const p = await q().limit(limit).after(cursor).page();
    out.push(...p.rows);
    if (!p.cursor) return out;
    cursor = p.cursor;
  }
  throw new Error('too many pages');
}

/** Rows in the order of the index's sort keys. */
function bySortKey<T extends Fields>(ix: IndexDef, rows: T[], reverse = false): T[] {
  const k = (r: T) =>
    bytes.hex(
      sortKeyOf(
        ix,
        ix.fields.map(([f]) => r[f] ?? null),
        [r.id as Value],
      ),
    );
  const out = [...rows].sort((a, b) => (k(a) < k(b) ? -1 : k(a) > k(b) ? 1 : 0));
  return reverse ? out.reverse() : out;
}

const sample = (): Task[] =>
  Array.from({ length: 30 }, (_, i) => ({
    id: `t${i}`,
    status: i % 7 === 0 ? null : ['open', 'done', 'opening'][i % 3]!,
    created: i % 11 === 5 ? null : (i * 37) % 50,
    title: `task ${i}`,
  }));

async function fill(db: Db, rows: Task[]) {
  await db.transaction(async (tx) => {
    for (const r of rows) await tx.table<Task>('tasks').insert(r);
  });
}

const ids = (rows: Fields[]) => rows.map((r) => r.id);

for (const kind of ['private', 'sealed'] as const) {
  describe(`${kind} index queries (§6.1)`, () => {
    let db: Db;
    let ix: IndexDef;
    const rows = sample();
    beforeAll(async () => {
      db = await tasksDb(kind, kind === 'private' ? { fanout: 4 } : {});
      await fill(db, rows);
      ix = indexOf(db, 'tasks', 'by_status');
    });

    it('equality plus a range, ordered both ways', async () => {
      const t = db.table<Task>('tasks');
      const open = rows.filter((r) => r.status === 'open');
      const want = bySortKey(
        ix,
        open.filter((r) => compareValues(r.created ?? null, 10) >= 0),
      );
      // `created` is descending in the index: orderBy desc scans forward.
      const q = () => t.query().where('status', '=', 'open').where('created', '>=', 10);
      expect(ids(await q().orderBy('created', 'desc').all())).toEqual(ids(want));
      expect(ids(await q().orderBy('created', 'asc').all())).toEqual(ids([...want].reverse()));
      expect(ids(await q().all())).toEqual(ids(want));
      const lt = bySortKey(
        ix,
        open.filter((r) => compareValues(r.created ?? null, 20) < 0),
      );
      expect(
        ids(await t.query().where('status', '=', 'open').where('created', '<', 20).all()),
      ).toEqual(ids(lt));
      // Nulls sort first, so `<` includes them, as the row filter does.
      expect(lt.some((r) => r.created === null)).toBe(true);
    });

    it('prefix, and ordering by the leading field with nulls first', async () => {
      const t = db.table<Task>('tasks');
      const want = bySortKey(
        ix,
        rows.filter((r) => typeof r.status === 'string' && r.status.startsWith('open')),
      );
      expect(ids(await t.query().where('status', 'prefix', 'open').all())).toEqual(ids(want));
      const all = await t.query().orderBy('status').all();
      expect(all.map((r) => r.status ?? null)).toEqual(
        bySortKey(ix, rows).map((r) => r.status ?? null),
      );
      expect(all[0]!.status ?? null).toBe(null);
      const rev = await t.query().orderBy('status', 'desc').limit(5).page();
      expect(ids(rev.rows)).toEqual(ids(bySortKey(ix, rows, true).slice(0, 5)));
    });

    it('pages with cursors, either direction', async () => {
      const t = db.table<Task>('tasks');
      for (const dir of ['asc', 'desc'] as const) {
        const got = await allPages(() => t.query().orderBy('status', dir), 4);
        expect(ids(got)).toEqual(ids(bySortKey(ix, rows, dir === 'desc')));
      }
      const open = await allPages(
        () => t.query().where('status', '=', 'open').orderBy('created', 'asc'),
        3,
      );
      expect(ids(open)).toEqual(
        ids(
          bySortKey(
            ix,
            rows.filter((r) => r.status === 'open'),
            true,
          ),
        ),
      );
    });

    it('sees its own writes in the same transaction', async () => {
      await db
        .transaction(async (tx) => {
          const t = tx.table<Task>('tasks');
          await t.insert({ id: 'mine', status: 'open', created: 1000 });
          await t.delete('t1');
          await t.update('t4', (r) => ({ ...r, created: 999 }));
          const now = [
            ...rows
              .filter((r) => r.id !== 't1')
              .map((r) => (r.id === 't4' ? { ...r, created: 999 } : r)),
            { id: 'mine', status: 'open', created: 1000 },
          ];
          const got = await t
            .query()
            .where('status', '=', 'open')
            .orderBy('created', 'desc')
            .limit(3)
            .all();
          expect(ids(got)).toEqual(
            ids(
              bySortKey(
                ix,
                now.filter((r) => r.status === 'open'),
              ).slice(0, 3),
            ),
          );
          expect(ids(await t.query().orderBy('status').all())).toEqual(ids(bySortKey(ix, now)));
          throw new DbError('bad_value', 'roll back');
        })
        .catch((e) => {
          if (!(e instanceof DbError && e.message.endsWith('roll back'))) throw e;
        });
      expect(await db.table<Task>('tasks').get('mine')).toBeUndefined();
    });
  });
}

describe('unique private and sealed indexes', () => {
  for (const kind of ['private', 'sealed'] as const) {
    it(`refuses a duplicate (${kind}); nulls don't count`, async () => {
      const db = await tasksDb(kind, { unique: true });
      const t = db.table<Task>('tasks');
      await t.insert({ id: 'a', status: 'x', created: 1 });
      await t.insert({ id: 'b', status: 'x', created: 2 });
      expect(await code(t.insert({ id: 'c', status: 'x', created: 1 }))).toBe('unique_violation');
      await t.insert({ id: 'd', status: null, created: 1 });
      await t.insert({ id: 'e', status: null, created: 1 });
      await t.update('a', (r) => ({ ...r, created: 3 }));
      await t.insert({ id: 'c', status: 'x', created: 1 });
      expect(ids(await t.query().where('status', '=', 'x').where('created', '=', 1).all())).toEqual(
        ['c'],
      );
    });
  }

  it('a sealed index over max_bytes fails with too_large', async () => {
    const db = await tasksDb('sealed', { maxBytes: 600 });
    const t = db.table<Task>('tasks');
    let i = 0;
    for (; i < 40; i++) {
      const c = await code(t.insert({ id: `r${i}`, status: 'some status text', created: i }));
      if (c !== 'ok') {
        expect(c).toBe('too_large');
        break;
      }
    }
    expect(i).toBeGreaterThan(5);
    expect(i).toBeLessThan(40);
    expect(await t.get(`r${i}`)).toBeUndefined();
  });
});

describe('private index trees (§5.4)', () => {
  it('concurrent writers all land, and the tree stays canonical', async () => {
    const name = ns();
    const db = await tasksDb('private', { fanout: 4 }, {}, name);
    const rows = sample();
    await fill(db, rows.slice(0, 10));
    const others = await Promise.all([1, 2, 3].map(() => tasksDb('private', {}, {}, name)));
    const writers = [db, ...others];
    await Promise.all(
      rows.slice(10).map((r, i) => writers[i % writers.length]!.table<Task>('tasks').insert(r)),
    );
    const ix = indexOf(db, 'tasks', 'by_status');
    await expectCanonical(db, ix, rows);
    // Deletes and updates keep it canonical too.
    await Promise.all(
      rows.slice(0, 12).map((r, i) =>
        i % 2
          ? writers[i % 4]!.table<Task>('tasks').delete(r.id)
          : writers[i % 4]!.table<Task>('tasks').update(r.id, (x) => ({
              ...x,
              created: 100 + i,
            })),
      ),
    );
    const now = rows
      .slice(12)
      .concat(rows.slice(0, 12).flatMap((r, i) => (i % 2 ? [] : [{ ...r, created: 100 + i }])));
    await expectCanonical(db, ix, now);
    expect((await db.table<Task>('tasks').query().orderBy('status').all()).length).toBe(now.length);
  });

  it("writers of different shards don't conflict, and ranges merge the shards", async () => {
    const db = await tasksDb('private', { shards: 4, fanout: 4 });
    const ix = indexOf(db, 'tasks', 'by_status');
    const ik = db.indexKeys(ix.id);
    const byShard = new Map<number, string>();
    for (let i = 0; byShard.size < 4; i++) {
      const s = ik.shard(encode(`k${i}`), 4);
      if (!byShard.has(s)) byShard.set(s, `k${i}`);
    }
    const go = barrier(4);
    let runs = 0;
    await Promise.all(
      [...byShard.values()].map((id, i) =>
        db.transaction(async (tx) => {
          runs++;
          await tx.table<Task>('tasks').insert({ id, status: 'open', created: i });
          await go();
        }),
      ),
    );
    expect(runs).toBe(4);
    const rows = sample();
    await fill(db, rows);
    const all = [
      ...rows,
      ...[...byShard.values()].map((id, i) => ({ id, status: 'open', created: i })),
    ];
    const got = await allPages(() => db.table<Task>('tasks').query().orderBy('status'), 7);
    expect(ids(got)).toEqual(ids(bySortKey(ix, all)));
  });

  it('decoys: every node is salted and padded to a bucket, queries stay right', async () => {
    const db = await tasksDb('private', { decoys: 2, fanout: 4 });
    const ix = indexOf(db, 'tasks', 'by_status');
    const rows = sample();
    for (let i = 0; i < rows.length; i += 5) await fill(db, rows.slice(i, i + 5));
    await db.table<Task>('tasks').delete('t3');
    const left = rows.filter((r) => r.id !== 't3');
    const nodes = await db.transaction((tx) => tx.raw.rangeStored(...db.keys.range('n', ix.id)));
    // A root over leaves (boundaries are random per index: one is about 0.3% likely).
    expect(nodes.length).toBeGreaterThanOrEqual(3);
    for (const e of nodes) {
      expect(e.value.length).toBeLessThanOrEqual(nodeBucket(e.value.length));
      expect(e.value.length).toBeGreaterThanOrEqual(nodeBucket(e.value.length) - 2);
      expect(decodeNode(e.value).salt?.length).toBe(16);
    }
    const got = await db.table<Task>('tasks').query().orderBy('status').all();
    expect(ids(got)).toEqual(ids(bySortKey(ix, left)));
  });

  it('refuses decoys when a padded node would not fit one value', async () => {
    const small = await world({ limits: { max_value_bytes: 16384 } });
    try {
      const f = await small.session
        .fs(1)
        .init((k) => [zw.createDeviceSlot(k, small.admin.device.devicePublic)]);
      const e = await code(
        Db.open(f, 'd', {
          ...quiet,
          schema: 1,
          migrations: {
            1: async (m) => {
              await m.createTable('t', { pk: ['id'] });
              await m.createIndex('t', 'i', { fields: [['x', 'int']], decoys: 1 });
            },
          },
        }),
      );
      expect(e).toBe('bad_value');
    } finally {
      small.server.stop();
    }
  });

  it('a node gone mid-read re-runs the transaction (§5.4.3)', async () => {
    const name = ns();
    const db = await tasksDb('private', { fanout: 4 }, {}, name);
    // Enough leaves that the first page can't have cached them all.
    const rows = Array.from({ length: 150 }, (_, i) => ({
      id: `m${i}`,
      status: 'open',
      created: i,
    }));
    await fill(db, rows);
    const other = await tasksDb('private', {}, {}, name);
    const reader = await tasksDb('private', {}, {}, name); // an empty node cache
    let runs = 0;
    const got = await reader.transaction(
      async (tx) => {
        runs++;
        const q = () => tx.table<Task>('tasks').query().orderBy('status').limit(2);
        const first = await q().page();
        if (runs === 1) {
          // Another client replaces every entry: every old node is deleted.
          await other.transaction(async (o) => {
            for (const r of rows)
              await o.table<Task>('tasks').update(r.id, (x) => ({ ...x, status: 'moved' }));
          });
        }
        const rest = await allPages(() => tx.table<Task>('tasks').query().orderBy('status'), 5);
        return { first: first.rows, rest };
      },
      { mode: 'long' },
    );
    expect(runs).toBe(2);
    expect(got.rest.every((r) => r.status === 'moved')).toBe(true);
    expect(got.rest.length).toBe(rows.length);
  });
});

describe('building and dropping (§8.2, §8.3)', () => {
  it('backfills give the canonical tree and the sorted list; a drop clears the tree', async () => {
    const name = ns();
    const rows = sample();
    const migrations: DbOptions['migrations'] = {
      1: async (m) => {
        await m.createTable('tasks', { pk: ['id'] });
      },
      2: async (m) => {
        await m.createIndex('tasks', 'p', { fields: FIELDS, fanout: 4 });
        await m.createIndex('tasks', 's', { fields: FIELDS, kind: 'sealed' });
      },
      3: async (m) => {
        await m.dropIndex('tasks', 'p');
      },
    };
    const db1 = await Db.open(fs, name, { ...quiet, schema: 1, migrations });
    await fill(db1, rows);
    const db2 = await Db.open(fs, name, { ...quiet, schema: 2, migrations, pageSize: 2 });
    const p = indexOf(db2, 'tasks', 'p');
    const s = indexOf(db2, 'tasks', 's');
    expect([p.state, s.state]).toEqual(['active', 'active']);
    await expectCanonical(db2, p, rows);
    const list = await db2.transaction(async (tx) => {
      const head = await tx.raw.get(db2.keys.sealedHead(s.id));
      const parts = await tx.raw.rangeStored(...db2.keys.range('s', s.id));
      return { head, parts };
    });
    expect(list.head).toBeDefined();
    const want = rows.map((r) => sortKeyOf(s, [r.status ?? null, r.created ?? null], [r.id]));
    want.sort(bytes.compare);
    // The parts are found by their keys, not their order.
    const partVals = await db2.transaction(async (tx) => {
      const out: Uint8Array[] = [];
      for (let i = 0; ; i++) {
        const v = await tx.raw.get(db2.keys.sealedPart(s.id, i));
        if (!v) return out;
        out.push(v);
      }
    });
    expect(list.parts.length).toBe(partVals.length + 1);
    const blob = decodeBlob(joinParts(partVals, decodeHead(list.head!).digest));
    expect(blob.map(bytes.hex)).toEqual(want.map(bytes.hex));

    const db3 = await Db.open(fs, name, { ...quiet, schema: 3, migrations });
    expect(indexOf(db3, 'tasks', 'p')).toBeUndefined();
    const left = await db3.transaction(async (tx) => ({
      n: await tx.raw.rangeStored(...db3.keys.range('n', p.id)),
      r: await tx.raw.rangeStored(...db3.keys.range('r', p.id)),
    }));
    expect([left.n.length, left.r.length]).toEqual([0, 0]);
    // The sealed index still answers.
    expect(ids(await db3.table<Task>('tasks').query().orderBy('status').all())).toEqual(
      ids(bySortKey(s, rows)),
    );
  });
});

describe('query property: every path equals a naive model', () => {
  interface R extends Fields {
    id: string;
    g?: string | null;
    v?: number | null;
  }
  const G = ['a', 'ab', 'b', 'ba', null];

  function xorshift(seed: number) {
    let x = seed;
    return () => {
      x ^= x << 13;
      x >>>= 0;
      x ^= x >>> 17;
      x ^= x << 5;
      x >>>= 0;
      return x / 2 ** 32;
    };
  }

  it('random rows and queries over private, sealed, fast and scans', async () => {
    const rnd = xorshift(42);
    const pick = <T>(xs: T[]) => xs[Math.floor(rnd() * xs.length)]!;
    const spec: [string, FieldSpec[]][] = [
      [
        'by_gv',
        [
          ['g', 'text'],
          ['v', 'int', 'desc'],
        ],
      ],
    ];
    const db = await Db.open(fs, ns(), {
      ...quiet,
      schema: 1,
      migrations: {
        1: async (m) => {
          for (const t of ['tp', 'ts', 'tf', 'tn']) await m.createTable(t, { pk: ['id'] });
          await m.createIndex('tp', spec[0]![0], { fields: spec[0]![1], fanout: 4, shards: 2 });
          await m.createIndex('ts', spec[0]![0], { fields: spec[0]![1], kind: 'sealed' });
          await m.createIndex('tf', 'by_g', { fields: [['g', 'text']], kind: 'fast' });
        },
      },
    });
    const rows: R[] = Array.from({ length: 80 }, (_, i) => ({
      id: `r${i}`,
      g: pick(G),
      v: rnd() < 0.15 ? null : Math.floor(rnd() * 30) - 5,
    }));
    for (let i = 0; i < rows.length; i += 20) {
      await db.transaction(async (tx) => {
        for (const r of rows.slice(i, i + 20)) {
          for (const t of ['tp', 'ts', 'tf', 'tn']) await tx.table<R>(t).insert(r);
        }
      });
    }
    const ix = indexOf(db, 'tp', 'by_gv');
    type C = [string, '=' | '<' | '<=' | '>' | '>=' | 'prefix', Value];
    const holds = (r: R, [f, op, x]: C) => {
      const v = r[f] ?? null;
      if (op === '=') return compareValues(v, x) === 0 && (v === null) === (x === null);
      if (op === 'prefix') return typeof v === 'string' && v.startsWith(x as string);
      const c = compareValues(v, x);
      return op === '<' ? c < 0 : op === '<=' ? c <= 0 : op === '>' ? c > 0 : c >= 0;
    };
    for (let q = 0; q < 40; q++) {
      const conds: C[] = [];
      const shape = Math.floor(rnd() * 5);
      if (shape === 0) conds.push(['g', '=', pick(G.slice(0, 4))]);
      if (shape === 1)
        conds.push(
          ['g', '=', pick(G.slice(0, 4))],
          ['v', pick(['<', '<=', '>', '>='] as const), Math.floor(rnd() * 30) - 5],
        );
      if (shape === 2) conds.push(['g', 'prefix', pick(['a', 'b', ''])]);
      if (shape === 3) conds.push(['v', '>=', Math.floor(rnd() * 20)]);
      const order = rnd() < 0.6 ? (shape <= 1 ? 'v' : 'g') : undefined;
      const dir = rnd() < 0.5 ? 'asc' : 'desc';
      const match = rows.filter((r) => conds.every((c) => holds(r, c)));
      const label = JSON.stringify({ conds, order, dir });
      for (const t of ['tp', 'ts', 'tf', 'tn']) {
        const query = () => {
          let qq = db.table<R>(t).query();
          for (const c of conds) qq = qq.where(...c);
          return order ? qq.orderBy(order, dir) : qq;
        };
        const indexed = t === 'tp' || t === 'ts';
        const got =
          indexed && rnd() < 0.7
            ? await allPages(query, 1 + Math.floor(rnd() * 9))
            : await query().all();
        expect(new Set(ids(got)), `${t} ${label}`).toEqual(new Set(ids(match)));
        expect(got.length, `${t} ${label}`).toBe(match.length);
        if (order) {
          const want = [...match].sort(
            (a, b) => compareValues(a[order] ?? null, b[order] ?? null) * (dir === 'desc' ? -1 : 1),
          );
          expect(
            got.map((r) => r[order] ?? null),
            `${t} ${label}`,
          ).toEqual(want.map((r) => r[order] ?? null));
          if (indexed && (order === 'g' || conds.some((c) => c[0] === 'g' && c[1] === '='))) {
            // An index gives a total order: the sort keys.
            const keyed = bySortKey(ix, match, order === 'g' ? dir === 'desc' : dir === 'asc');
            expect(ids(got), `${t} ${label}`).toEqual(ids(keyed));
          }
        }
      }
    }
  });
});
