// @zen/db against a spawned zen-serve: rows, unique and fast indexes,
// queries, transactions, migrations (zendb.md §2–8).
import { bytes, isCode, type UnlockedFs, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { Db, DbError, type DbOptions, type Fields } from '../src/index.js';
import { rowBucket } from '../src/row.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
/** A fresh database name per test. */
const ns = () => `db${++n}`;
const quiet = { warn: () => {} };

beforeAll(async () => {
  w = await world({
    limits: { max_value_bytes: 16384, max_commit_ops: 400, max_range_items: 50 },
  });
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

interface User extends Fields {
  id: string;
  email?: string | null;
  team?: string;
  age?: number;
  blob?: Uint8Array;
}

/** A database with `users(id)`, a unique email and a fast team index. */
function usersDb(name: string, extra: DbOptions = {}): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('users', { pk: ['id'] });
        await m.createIndex('users', 'by_email', {
          fields: [['email', 'text']],
          unique: true,
          kind: 'none',
        });
        await m.createIndex('users', 'by_team', { fields: [['team', 'text']], kind: 'fast' });
      },
    },
    ...extra,
  });
}

/** Resolves once `n` callers have arrived. */
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

describe('opening (§3.1)', () => {
  it('creates the database at schema 0, then migrates', async () => {
    const name = ns();
    const db0 = await Db.open(fs, name, quiet);
    expect(db0.schema).toBe(0);
    expect(db0.tableNames()).toEqual([]);
    const db1 = await usersDb(name);
    expect(db1.tableNames()).toEqual(['users']);
    expect(await code(Db.open(fs, name, quiet))).toBe('schema_newer');
    expect(await code(Db.open(fs, name, { ...quiet, schema: 2 }))).toBe('bad_value');
  });
});

describe('rows (§4)', () => {
  let db: Db;
  beforeAll(async () => {
    db = await usersDb(ns());
  });

  it('inserts, gets, puts, updates and deletes', async () => {
    const users = db.table<User>('users');
    await users.insert({ id: 'u1', email: 'a@x', age: 30 });
    expect(await users.get('u1')).toEqual({ id: 'u1', email: 'a@x', age: 30 });
    expect(await code(users.insert({ id: 'u1' }))).toBe('exists');
    await users.put({ id: 'u1', email: 'a@x', age: 31 });
    const r = await users.update('u1', (u) => ({ ...u, age: (u.age ?? 0) + 1 }));
    expect(r.age).toBe(32);
    expect((await users.get('u1'))?.age).toBe(32);
    expect(await code(users.update('nope', (u) => u))).toBe('not_found');
    expect(await code(users.update('u1', (u) => ({ ...u, id: 'u2' })))).toBe('bad_value');
    expect(await users.delete('u1')).toBe(true);
    expect(await users.delete('u1')).toBe(false);
    expect(await users.get('u1')).toBeUndefined();
    expect(await code(users.insert({ email: 'x' } as User))).toBe('bad_value');
    expect(await code(db.table('nope').get('x'))).toBe('not_found');
  });

  it('keeps every value type, big integers included', async () => {
    const t = db.table('users');
    const row = {
      id: 'types',
      n: null,
      b: true,
      i: -7,
      big: 2n ** 63n,
      f: 1.5,
      s: 'zen ✓',
      bytes: new Uint8Array([1, 2]),
      arr: [1, 'a', [2]],
      map: { k: 'v' },
    };
    await t.put(row);
    expect(await t.get('types')).toEqual(row);
  });

  it('stores a large row in parts, and drops parts it no longer needs (§4.2)', async () => {
    const users = db.table<User>('users');
    const blob = new Uint8Array(40_000).map((_, i) => i % 251);
    await users.put({ id: 'big', blob });
    expect((await users.get('big'))?.blob).toEqual(blob);
    const t = (await db.transaction((tx) => tx.tableRecord('users'))).id;
    const parts = async () => {
      let k = 0;
      for await (const _ of fs.kv.rangeStored(...db.keys.range('o', t))) k++;
      return k;
    };
    expect(await parts()).toBe(3);
    await users.put({ id: 'big', blob: blob.subarray(0, 20_000) });
    expect((await users.get('big'))?.blob).toEqual(blob.subarray(0, 20_000));
    expect(await parts()).toBe(2);
    await users.put({ id: 'big' });
    expect(await parts()).toBe(0);
    await users.put({ id: 'big', blob });
    await users.delete('big');
    expect(await parts()).toBe(0);
  });

  it('pads rows of a padded table to size buckets (§4.3)', async () => {
    const pdb = await Db.open(fs, ns(), {
      ...quiet,
      schema: 1,
      migrations: { 1: (m) => m.createTable('p', { pk: ['id'], pad: true }) },
    });
    const p = pdb.table('p');
    const sizes = [0, 100, 300, 2000, 9000, 30_000];
    for (const s of sizes) await p.put({ id: `r${s}`, blob: new Uint8Array(s) });
    const t = (await pdb.transaction((tx) => tx.tableRecord('p'))).id;
    for await (const e of fs.kv.rangeStored(...pdb.keys.rows(t))) {
      // The plaintext Row fills its bucket (one or two bytes short at a CBOR head boundary).
      const len = e.value.length;
      expect(rowBucket(len) - len).toBeLessThanOrEqual(2);
    }
    for (const s of sizes) expect((await p.get(`r${s}`))?.blob).toEqual(new Uint8Array(s));
  });

  it('works in long mode', async () => {
    await db.transaction(
      async (tx) => {
        const u = tx.table<User>('users');
        await u.insert({ id: 'long1', email: 'long@x' });
        expect(await u.get('long1')).toEqual({ id: 'long1', email: 'long@x' });
      },
      { mode: 'long' },
    );
    expect(await db.table('users').get('long1')).toBeTruthy();
  });

  it('refuses a value of the wrong type for an index (bad_type)', async () => {
    expect(await code(db.table('users').put({ id: 'bt', email: 5 }))).toBe('bad_type');
  });

  it('fails early when a commit breaks the server limits (§7.5)', async () => {
    const e = await code(
      db.transaction(async (tx) => {
        for (let i = 0; i < 250; i++) await tx.table('users').put({ id: `many${i}` });
      }),
    );
    expect(e).toBe('too_large');
  });
});

describe('unique and fast indexes (§5.2, §5.3)', () => {
  let db: Db;
  beforeAll(async () => {
    db = await usersDb(ns());
  });

  it('refuses a duplicate unique value, frees changed values, ignores nulls', async () => {
    const users = db.table<User>('users');
    await users.insert({ id: 'a', email: 'same@x' });
    expect(await code(users.insert({ id: 'b', email: 'same@x' }))).toBe('unique_violation');
    await users.put({ id: 'a', email: 'other@x' });
    await users.insert({ id: 'b', email: 'same@x' });
    await users.insert({ id: 'c' });
    await users.insert({ id: 'd', email: null });
    const q = await users.query().where('email', '=', 'same@x').all();
    expect(q.map((u) => u.id)).toEqual(['b']);
    await users.delete('b');
    await users.insert({ id: 'e', email: 'same@x' });
  });

  for (const mode of ['short', 'long'] as const) {
    it(`two clients inserting the same value: one wins (${mode})`, async () => {
      const name = ns();
      const a = await usersDb(name);
      const b = await usersDb(name);
      const arrive = barrier(2);
      const insert = (db: Db, id: string) =>
        code(
          db.transaction(
            async (tx) => {
              await tx.table('users').insert({ id, email: 'race@x' });
              await arrive();
            },
            { mode },
          ),
        );
      const results = await Promise.all([insert(a, 'x'), insert(b, 'y')]);
      expect(results.sort()).toEqual(['ok', 'unique_violation']);
    });
  }

  it('looks up a fast index, with paging', async () => {
    const users = db.table<User>('users');
    for (let i = 0; i < 7; i++) await users.put({ id: `t${i}`, team: i < 5 ? 'red' : 'blue' });
    const all = await users.query().where('team', '=', 'red').all();
    expect(all.map((u) => u.id).sort()).toEqual(['t0', 't1', 't2', 't3', 't4']);
    const seen: string[] = [];
    let cursor: Awaited<ReturnType<ReturnType<typeof users.query>['page']>>['cursor'];
    for (;;) {
      const p = await users.query().where('team', '=', 'red').limit(2).after(cursor).page();
      seen.push(...p.rows.map((u) => u.id));
      if (!p.cursor) break;
      cursor = p.cursor;
    }
    expect(seen.sort()).toEqual(['t0', 't1', 't2', 't3', 't4']);
    // Other predicates filter on the client.
    await users.put({ id: 't0', team: 'red', age: 9 });
    const old = await users.query().where('team', '=', 'red').where('age', '>', 5).all();
    expect(old.map((u) => u.id)).toEqual(['t0']);
  });
});

describe('queries (§6)', () => {
  let db: Db;
  const warnings: string[] = [];
  beforeAll(async () => {
    db = await usersDb(ns(), { warn: (m) => warnings.push(m) });
    for (let i = 0; i < 12; i++) {
      await db.table('users').put({ id: `q${i}`, age: i, team: i % 2 ? 'odd' : 'even' });
    }
  });

  it('scans with a warning, filters, pages', async () => {
    const users = db.table<User>('users');
    const r = await users.query().where('age', '>=', 8).all();
    expect(r.map((u) => u.age).sort((a, b) => a! - b!)).toEqual([8, 9, 10, 11]);
    expect(warnings.length).toBeGreaterThan(0);
    const seen: number[] = [];
    let p = await users.query().where('age', '<', 7).limit(3).page();
    seen.push(...p.rows.map((u) => u.age!));
    while (p.cursor) {
      p = await users.query().where('age', '<', 7).limit(3).after(p.cursor).page();
      seen.push(...p.rows.map((u) => u.age!));
    }
    expect(seen.sort((a, b) => a - b)).toEqual([0, 1, 2, 3, 4, 5, 6]);
    const ins = await users.query().where('age', 'in', [1, 3, 100]).all();
    expect(ins.map((u) => u.age).sort()).toEqual([1, 3]);
    const pre = await users.query().where('id', 'prefix', 'q1').all();
    expect(pre.map((u) => u.id).sort()).toEqual(['q1', 'q10', 'q11']);
  });

  it('orders on the client, or refuses an order a limit would cut (needs_index)', async () => {
    const users = db.table<User>('users');
    const r = await users.query().where('team', '=', 'odd').orderBy('age', 'desc').all();
    expect(r.map((u) => u.age)).toEqual([11, 9, 7, 5, 3, 1]);
    expect(await code(users.query().orderBy('age').limit(3).page())).toBe('needs_index');
    const few = await users.query().where('age', '<', 2).orderBy('age').limit(3).all();
    expect(few.map((u) => u.age)).toEqual([0, 1]);
  });

  it('pk lookups', async () => {
    const r = await db.table<User>('users').query().where('id', '=', 'q3').all();
    expect(r.map((u) => u.age)).toEqual([3]);
  });
});

describe('migrations (§8)', () => {
  it('renames, transforms, drops tables and indexes', async () => {
    const name = ns();
    const db1 = await usersDb(name, { pageSize: 4 });
    for (let i = 0; i < 10; i++) {
      await db1.table('users').put({ id: `m${i}`, email: `m${i}@x`, team: 'a', age: i });
    }
    const old = await db1.transaction((tx) => tx.tableRecord('users'));
    const db2 = await usersDb(name, {
      pageSize: 4,
      schema: 3,
      migrations: {
        2: async (m) => {
          await m.renameTable('users', 'people');
          await m.transform<User>('people', (u) =>
            u.age! >= 8 ? null : { ...u, age: u.age! * 10 },
          );
          await m.dropIndex('people', 'by_team');
        },
        3: async (m) => {
          await m.createTable('tmp', { pk: ['id'] });
          await m.dropTable('tmp');
        },
      },
    });
    expect(db2.tableNames().sort()).toEqual(['people']);
    const people = db2.table<User>('people');
    expect((await people.get('m3'))?.age).toBe(30);
    expect(await people.get('m9')).toBeUndefined();
    expect((await people.query().where('email', '=', 'm2@x').all()).map((u) => u.id)).toEqual([
      'm2',
    ]);
    expect(await people.query().where('email', '=', 'm9@x').all()).toEqual([]);
    // The fast index's entries are cleared, and it is gone from the catalog.
    const team = old.indexes.find((i) => i.name === 'by_team')!;
    let left = 0;
    for await (const _ of fs.kv.rangeStored(...db2.keys.range('f', team.id))) left++;
    expect(left).toBe(0);
    const t = await db2.transaction((tx) => tx.tableRecord('people'));
    expect(t.indexes.map((i) => i.name)).toEqual(['by_email']);
    expect(t.id).toEqual(old.id);
  });

  it('resumes a killed step where it stopped', async () => {
    const name = ns();
    const calls: string[] = [];
    let kill = true;
    const opts = (): DbOptions => ({
      ...quiet,
      pageSize: 3,
      schema: 1,
      migrations: {
        1: async (m) => {
          calls.push('step');
          await m.createTable('a', { pk: ['id'] });
          if (kill) throw new Error('killed');
          await m.createTable('b', { pk: ['id'] });
        },
      },
    });
    await expect(Db.open(fs, name, opts())).rejects.toThrow('killed');
    const half = await Db.open(fs, name, { ...quiet });
    expect(half.tableNames()).toEqual(['a']);
    const aId = (await half.transaction((tx) => tx.tableRecord('a'))).id;
    kill = false;
    const db = await Db.open(fs, name, opts());
    expect(db.tableNames().sort()).toEqual(['a', 'b']);
    // Table a was not created again.
    expect((await db.transaction((tx) => tx.tableRecord('a'))).id).toEqual(aId);
    expect(calls).toEqual(['step', 'step']);
  });

  it('a unique build over duplicates stops, and resumes once they are fixed (§8.2)', async () => {
    const name = ns();
    const base = {
      ...quiet,
      pageSize: 2,
      schema: 1,
      migrations: {
        1: (m: import('../src/index.js').Migrator) => m.createTable('u', { pk: ['id'] }),
      },
    } satisfies DbOptions;
    const db1 = await Db.open(fs, name, base);
    for (let i = 0; i < 7; i++)
      await db1.table('u').put({ id: `d${i}`, v: i === 5 ? 'v1' : `v${i}` });
    const v2 = {
      ...base,
      schema: 2,
      migrations: {
        ...base.migrations,
        2: (m: import('../src/index.js').Migrator) =>
          m.createIndex('u', 'by_v', { fields: [['v', 'text']], unique: true, kind: 'none' }),
      },
    };
    const err = await Db.open(fs, name, v2).catch((e) => e);
    expect(err).toBeInstanceOf(DbError);
    expect(err.code).toBe('building');
    expect(err.detail).toHaveLength(1);
    // The index stays building, and writers already maintain it.
    const t = await db1.transaction((tx) => tx.tableRecord('u'));
    expect(t.indexes[0]!.state).toBe('building');
    expect(await code(db1.table('u').put({ id: 'd9', v: 'v2' }))).toBe('unique_violation');
    await db1.table('u').delete('d5');
    const db2 = await Db.open(fs, name, v2);
    const t2 = await db2.transaction((tx) => tx.tableRecord('u'));
    expect(t2.indexes[0]!.state).toBe('active');
    expect((await db2.table('u').query().where('v', '=', 'v3').all()).map((r) => r.id)).toEqual([
      'd3',
    ]);
  });

  it('writers during a backfill keep the index complete', async () => {
    const name = ns();
    const base = {
      ...quiet,
      pageSize: 2,
      schema: 1,
      migrations: {
        1: (m: import('../src/index.js').Migrator) => m.createTable('w', { pk: ['id'] }),
      },
    } satisfies DbOptions;
    const db1 = await Db.open(fs, name, base);
    for (let i = 0; i < 12; i++) await db1.table('w').put({ id: `w${i}`, g: 'x' });
    const building = Db.open(fs, name, {
      ...base,
      schema: 2,
      migrations: {
        ...base.migrations,
        2: (m: import('../src/index.js').Migrator) =>
          m.createIndex('w', 'by_g', { fields: [['g', 'text']], kind: 'fast' }),
      },
    });
    const writes = (async () => {
      for (let i = 12; i < 24; i++) await db1.table('w').put({ id: `w${i}`, g: 'x' });
      for (let i = 0; i < 4; i++) await db1.table('w').put({ id: `w${i}`, g: 'y' });
    })();
    const [db2] = await Promise.all([building, writes]);
    const xs = await db2.table('w').query().where('g', '=', 'x').all();
    const ys = await db2.table('w').query().where('g', '=', 'y').all();
    expect(xs.length + ys.length).toBe(24);
    expect(ys.map((r) => r.id).sort()).toEqual(['w0', 'w1', 'w2', 'w3']);
  });

  it('a write on a cached schema retries after a concurrent schema change (§3, §7.7)', async () => {
    const name = ns();
    const base = {
      ...quiet,
      schema: 1,
      migrations: {
        1: (m: import('../src/index.js').Migrator) => m.createTable('s', { pk: ['id'] }),
      },
    } satisfies DbOptions;
    const a = await Db.open(fs, name, base);
    await a.table('s').put({ id: '1', email: 'e' });
    await Db.open(fs, name, {
      ...base,
      schema: 2,
      migrations: {
        ...base.migrations,
        2: (m: import('../src/index.js').Migrator) =>
          m.createIndex('s', 'by_email', {
            fields: [['email', 'text']],
            unique: true,
            kind: 'none',
          }),
      },
    });
    // `a` still caches the TableRecord without the index: its write fails the
    // `expect`, re-reads the record, and then sees the unique index.
    expect(await code(a.table('s').put({ id: '2', email: 'e' }))).toBe('unique_violation');
  });

  it('refuses index kinds this version does not build', async () => {
    const e = await code(
      Db.open(fs, ns(), {
        ...quiet,
        schema: 1,
        migrations: {
          1: async (m) => {
            await m.createTable('t', { pk: ['id'] });
            await m.createIndex('t', 'i', { fields: [['x', 'int']], kind: 'oblivious' });
          },
        },
      }),
    );
    expect(e).toBe('format');
  });
});

describe('raw access', () => {
  it('stored keys are opaque tokens under the database prefix', async () => {
    const db = await usersDb(ns());
    await db.table('users').put({ id: 'k' });
    const t = (await db.transaction((tx) => tx.tableRecord('users'))).id;
    const [begin] = db.keys.rows(t);
    expect(bytes.hex(begin).startsWith(bytes.hex(db.keys.db.prefix))).toBe(true);
    expect(isCode).toBeTypeOf('function');
  });
});
