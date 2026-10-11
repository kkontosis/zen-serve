// @zen/db transactions against a spawned zen-serve (zendb.md §7, §9.2):
// auto mode, manual transactions, the row cache, files in a transaction,
// bulk import, and the owner election.
import { bytes, ROOT, Tree, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import type { IndexDef } from '../src/catalog.js';
import { Db, DbError, type DbOptions, type Fields, type LockManagerLike } from '../src/index.js';
import { expectCanonical } from './helpers.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
/** A fresh database name per test. */
const ns = () => `tx${++n}`;
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { max_commit_ops: 400, max_range_items: 50 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
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

interface Item extends Fields {
  id: string;
  v?: number;
  email?: string;
  rank?: number;
}

/** A database with `items(id)`, a unique email and a private index on rank. */
function itemsDb(name: string, extra: DbOptions = {}): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    locks: null,
    // These tests count reads: no scheduler polling in the background.
    scheduler: false,
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('items', { pk: ['id'] });
        await m.createIndex('items', 'by_email', {
          fields: [['email', 'text']],
          unique: true,
          kind: 'none',
        });
        await m.createIndex('items', 'by_rank', { fields: [['rank', 'int']], kind: 'private' });
      },
    },
    ...extra,
  });
}

/** Count the session's `/v1/kv/get` calls while `fn` runs. */
async function getsDuring(fn: () => Promise<unknown>): Promise<number> {
  const s = fs.session;
  const call = s.call;
  let gets = 0;
  s.call = ((path: string, ...rest: unknown[]) => {
    if (path === '/v1/kv/get') gets++;
    return (call as (...a: unknown[]) => unknown).call(s, path, ...rest);
  }) as typeof s.call;
  try {
    await fn();
  } finally {
    s.call = call;
  }
  return gets;
}

describe('auto mode (§7.1)', () => {
  it('a function still running after autoSwitchMs re-runs in long mode', async () => {
    const db = await itemsDb(ns());
    const modes: string[] = [];
    await db.transaction(
      async (tx) => {
        modes.push(tx.mode);
        if (modes.length === 1) await sleep(80);
        await tx.table('items').put({ id: 'a', v: modes.length });
      },
      { mode: 'auto', autoSwitchMs: 50 },
    );
    expect(modes).toEqual(['short', 'long']);
    expect(await db.table('items').get('a')).toEqual({ id: 'a', v: 2 });
  });

  it('too_old in the short phase switches to long', async () => {
    const db = await itemsDb(ns());
    const modes: string[] = [];
    const out = await db.transaction(
      async (tx) => {
        modes.push(tx.mode);
        await tx.table('items').get('x');
        if (modes.length === 1) throw new ZenError(409, 'too_old', 'test');
        return 'done';
      },
      { mode: 'auto' },
    );
    expect(out).toBe('done');
    expect(modes).toEqual(['short', 'long']);
  });

  it('a conflict in the short phase re-runs it short', async () => {
    const db = await itemsDb(ns());
    const modes: string[] = [];
    await db.transaction(
      async (tx) => {
        modes.push(tx.mode);
        if (modes.length === 1) throw new ZenError(409, 'conflict', 'test');
      },
      { mode: 'auto' },
    );
    expect(modes).toEqual(['short', 'short']);
  });

  it('a range too large for long mode fails with too_large', async () => {
    const db = await itemsDb(ns());
    await db.importRows(
      'items',
      Array.from({ length: 60 }, (_, i) => ({ id: `i${i}` })),
    );
    const scan = (tx: Parameters<Parameters<Db['transaction']>[0]>[0]) =>
      tx.table('items').query().all();
    expect(await db.transaction(scan)).toHaveLength(60);
    expect(await code(db.transaction(scan, { mode: 'long' }))).toBe('too_large');
    const slow = db.transaction(
      async (tx) => {
        await sleep(30);
        return scan(tx);
      },
      { mode: 'auto', autoSwitchMs: 10 },
    );
    const e = await slow.catch((e) => e);
    expect(e).toBeInstanceOf(DbError);
    expect(e.message).toContain('split the work');
  });
});

describe('manual transactions (§7.2)', () => {
  it('commit writes, and the transaction closes', async () => {
    const db = await itemsDb(ns());
    const tx = db.begin();
    await tx.table<Item>('items').insert({ id: 'm', v: 1 });
    expect(await tx.commit()).toBeDefined();
    expect(await db.table('items').get('m')).toEqual({ id: 'm', v: 1 });
    expect(() => tx.table('items')).toThrow(DbError);
    expect(await code(tx.commit())).toBe('closed');
  });

  it('a concurrent write makes commit fail with conflict, once', async () => {
    const db = await itemsDb(ns());
    const name = ns();
    const db2 = await itemsDb(name);
    const db3 = await itemsDb(name);
    for (const mode of ['short', 'long'] as const) {
      const id = `c-${mode}`;
      const tx = db2.begin({ mode });
      expect(await tx.table('items').get(id)).toBeUndefined();
      await db3.table('items').put({ id, v: 1 });
      await tx.table('items').put({ id, v: 2 });
      expect(await code(tx.commit())).toBe('conflict');
      expect(await db3.table('items').get(id)).toEqual({ id, v: 1 });
    }
    db.close();
  });

  it('abort sends nothing; auto is refused', async () => {
    const db = await itemsDb(ns());
    const tx = db.begin({ mode: 'long' });
    await tx.table('items').put({ id: 'gone' });
    tx.abort();
    expect(await code(tx.commit())).toBe('closed');
    expect(await db.table('items').get('gone')).toBeUndefined();
    expect(() => db.begin({ mode: 'auto' as 'short' })).toThrow('bad_value');
  });
});

describe('row cache (§7.7)', () => {
  it('a cached row costs no read, and the caller may change it', async () => {
    const db = await itemsDb(ns());
    await db.table('items').put({ id: 'a', v: 1 });
    const first = await getsDuring(() => db.table('items').get('a'));
    const hits = db.stats.rowHits;
    let got: Fields | undefined;
    const second = await getsDuring(async () => {
      got = await db.table('items').get('a');
    });
    expect(db.stats.rowHits).toBe(hits + 1);
    expect(second).toBe(first - 1); // only the batched check of cached records
    got!.v = 99;
    expect(await db.table('items').get('a')).toEqual({ id: 'a', v: 1 });
  });

  it('a write using a stale cached row conflicts once, then commits fresh data', async () => {
    const name = ns();
    const db = await itemsDb(name);
    const other = await itemsDb(name);
    await db.table('items').put({ id: 'a', v: 1 });
    await db.table('items').get('a'); // cached at v 1
    await other.table<Item>('items').update('a', (r) => ({ ...r, v: 10 }));
    let runs = 0;
    await db.transaction(async (tx) => {
      runs++;
      await tx.table<Item>('items').update('a', (r) => ({ ...r, v: (r.v ?? 0) + 1 }));
    });
    expect(runs).toBe(2);
    expect(await other.table('items').get('a')).toEqual({ id: 'a', v: 11 });
  });

  it('insert checks a cached row on the server: a stale copy is no `exists`', async () => {
    const name = ns();
    const db = await itemsDb(name);
    const other = await itemsDb(name);
    await db.table('items').put({ id: 'a', v: 1 });
    await db.table('items').get('a'); // cached
    await other.table('items').delete('a');
    await db.table('items').insert({ id: 'a', v: 2 });
    expect(await other.table('items').get('a')).toEqual({ id: 'a', v: 2 });
    expect(await code(db.table('items').insert({ id: 'a' }))).toBe('exists');
  });

  it('a read-only transaction with a stale cached row re-runs and sees the new value', async () => {
    const name = ns();
    const db = await itemsDb(name);
    const other = await itemsDb(name);
    await db.table('items').put({ id: 'a', v: 1 });
    await db.table('items').get('a');
    await other.table('items').put({ id: 'a', v: 2 });
    let runs = 0;
    const r = await db.transaction(async (tx) => {
      runs++;
      return tx.table('items').get('a');
    });
    expect(runs).toBe(2);
    expect(r).toEqual({ id: 'a', v: 2 });
    // Deleted elsewhere: the cached copy is not used either.
    await other.table('items').delete('a');
    expect(await db.table('items').get('a')).toBeUndefined();
  });

  it('queries fill the cache; own writes and committed rows are not served stale', async () => {
    const db = await itemsDb(ns());
    await db.importRows(
      'items',
      Array.from({ length: 5 }, (_, i) => ({ id: `q${i}`, rank: i })),
    );
    await db.table('items').query().where('rank', '>=', 0).all();
    const hits = db.stats.rowHits;
    await db.table('items').get('q3');
    expect(db.stats.rowHits).toBe(hits + 1);
    await db.table<Item>('items').update('q3', (r) => ({ ...r, v: 7 }));
    expect(await db.table('items').get('q3')).toEqual({ id: 'q3', rank: 3, v: 7 });
  });

  it('rowCache: 0 turns it off', async () => {
    const db = await itemsDb(ns(), { rowCache: 0 });
    await db.table('items').put({ id: 'a', v: 1 });
    await db.table('items').get('a');
    await db.table('items').get('a');
    expect(db.stats.rowHits).toBe(0);
  });
});

describe('files in a transaction (§7.6)', () => {
  it('a row and a file write commit together; a conflict re-runs both', async () => {
    const name = ns();
    const db = await itemsDb(name);
    const other = await itemsDb(name);
    const tree = fs.tree(Tree.newId());
    const f = await tree.create(ROOT, 'doc');
    const data = new Uint8Array(100_000).map((_, i) => i % 251);
    const up = await tree.upload(f, data);
    await db.table('items').put({ id: 'doc', v: 0 });
    let runs = 0;
    await db.transaction(async (tx) => {
      runs++;
      const r = await tx.table<Item>('items').get('doc');
      if (runs === 1) await other.table('items').put({ id: 'doc', v: 5 });
      await tx.fs(tree).write(up);
      await tx.table('items').put({ id: 'doc', v: (r!.v ?? 0) + 1 });
    });
    expect(runs).toBe(2);
    expect(await db.table('items').get('doc')).toEqual({ id: 'doc', v: 6 });
    expect((await tree.versions(f)).length).toBe(1);
    expect(bytes.equal(await tree.readFile(f), data)).toBe(true);
  });

  it('tree batches ride along; two writes to one file are refused', async () => {
    const db = await itemsDb(ns());
    const tree = fs.tree(Tree.newId());
    const f = await tree.create(ROOT, 'f');
    const up = await tree.upload(f, new Uint8Array([1, 2, 3]));
    let dir: Uint8Array | undefined;
    await db.transaction(async (tx) => {
      const b = tree.batch();
      dir = b.mkdir(ROOT, 'attachments');
      await tx.fs(tree).ops(b);
      await tx.table('items').put({ id: 'with-dir' });
    });
    expect((await tree.getOne(dir!))?.parent).toEqual(ROOT);
    const twice = db.transaction(async (tx) => {
      await tx.fs(tree).write(up);
      await tx.fs(tree).write(up);
    });
    expect(await code(twice)).toBe('bad_value');
    expect((await tree.versions(f)).length).toBe(0);
  });
});

describe('importRows (§7.5)', () => {
  it('splits a load into commits under max_commit_ops, keeping the indexes right', async () => {
    const db = await itemsDb(ns());
    const rows: Item[] = Array.from({ length: 1000 }, (_, i) => ({
      id: `r${String(i).padStart(4, '0')}`,
      email: `u${i}@x`,
      rank: (i * 7919) % 1000,
    }));
    const progress: number[] = [];
    expect(await db.importRows('items', rows, { onCommit: (d) => progress.push(d) })).toBe(1000);
    expect(progress.length).toBeGreaterThan(3);
    expect(progress.at(-1)).toBe(1000);
    const ix = db.catalogEntry('items')!.record!.indexes.find((i) => i.name === 'by_rank')!;
    await expectCanonical(db, ix as IndexDef, rows);
    const top = await db.table('items').query().orderBy('rank', 'desc').limit(3).page();
    expect(top.rows.map((r) => r.rank)).toEqual([999, 998, 997]);
    expect((await db.table('items').query().where('email', '=', 'u500@x').all())[0]?.id).toBe(
      'r0500',
    );
    // `replace` overwrites.
    await db.importRows('items', [{ id: 'r0001', v: 1 }], { replace: true });
    expect(await db.table('items').get('r0001')).toEqual({ id: 'r0001', v: 1 });
    expect(await code(db.importRows('items', [{ id: 'r0002' }]))).toBe('exists');
  });

  it('a duplicate fails its commit; earlier commits stay', async () => {
    const db = await itemsDb(ns());
    const rows: Item[] = Array.from({ length: 300 }, (_, i) => ({
      id: `d${i}`,
      email: i === 290 ? 'u3@x' : `u${i}@x`,
    }));
    let done = 0;
    const r = db.importRows('items', rows, { onCommit: (d) => (done = d) });
    expect(await code(r)).toBe('unique_violation');
    expect(done).toBeGreaterThan(0);
    expect(done).toBeLessThan(291);
    expect(await db.table('items').get(`d${done - 1}`)).toBeDefined();
    expect(await db.table('items').get('d299')).toBeUndefined();
  });
});

/** In-memory Web Locks: one exclusive holder per name, a FIFO of waiters. */
class FakeLocks implements LockManagerLike {
  private readonly tails = new Map<string, Promise<void>>();

  async request(
    name: string,
    opts: { mode: 'exclusive'; signal?: AbortSignal },
    cb: (lock: unknown) => Promise<unknown>,
  ): Promise<unknown> {
    const prev = this.tails.get(name) ?? Promise.resolve();
    let done!: () => void;
    const mine = new Promise<void>((r) => {
      done = r;
    });
    this.tails.set(
      name,
      prev.then(() => mine),
    );
    try {
      await new Promise<void>((res, rej) => {
        prev.then(res);
        opts.signal?.addEventListener('abort', () => rej(new Error('AbortError')));
      });
      return await cb({ name });
    } finally {
      done();
    }
  }
}

describe('the owner (§9.2)', () => {
  it('one instance owns; closing it hands the role to the next', async () => {
    const name = ns();
    const locks = new FakeLocks();
    const a = await itemsDb(name, { locks });
    await sleep(0);
    expect(a.isOwner).toBe(true);
    const b = await itemsDb(name, { locks });
    const c = await itemsDb(name, { locks });
    await sleep(0);
    expect(b.isOwner).toBe(false);
    let fired = 0;
    b.onOwner(() => fired++);
    // A waiter that leaves never becomes the owner.
    c.close();
    const changed = b.ownerChange;
    a.close();
    expect(await changed).toBe(true);
    expect(b.isOwner).toBe(true);
    expect(a.isOwner).toBe(false);
    expect(c.isOwner).toBe(false);
    expect(fired).toBe(1);
    // Another database name has its own owner.
    const d = await itemsDb(ns(), { locks });
    await sleep(0);
    expect(d.isOwner).toBe(true);
    b.close();
    d.close();
  });

  it('without Web Locks every instance owns at once; owner: false never does', async () => {
    const db = await itemsDb(ns());
    expect(db.isOwner).toBe(true);
    let fired = 0;
    db.onOwner(() => fired++);
    await sleep(0);
    expect(fired).toBe(1);
    const no = await itemsDb(ns(), { owner: false });
    expect(no.isOwner).toBe(false);
  });
});
