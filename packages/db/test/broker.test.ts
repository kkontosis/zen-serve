// The broker's primitives against a spawned zen-serve (zendb.md §10–11,
// §12.7): emit, `on` (plain, after, durable cursors, prefixes), consumer
// groups, change events, ephemeral messages and the system tables.
import { bytes, type Topic, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openMessage } from '../src/broker.js';
import { encode } from '../src/cbor.js';
import {
  type ChangeBody,
  Db,
  DbError,
  type DbOptions,
  type DbTransaction,
  type Delivered,
  decodeMsg,
  type Fields,
  type LockManagerLike,
  type Message,
  SYSTEM_TABLES,
} from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `br${++n}`;
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
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

/** Poll until `cond` holds (or fail after `ms`). */
async function until(cond: () => boolean | Promise<boolean>, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(25);
  }
}

interface Order extends Fields {
  id: string;
  status?: string;
  blob?: Uint8Array;
}

/** A database with `orders` (change events, full image) and `log`. */
function shop(name: string, extra: DbOptions = {}): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    locks: null,
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('orders', { pk: ['id'], changes: { image: 'full' } });
        await m.createTable('log', { pk: ['id'] });
      },
    },
    ...extra,
  });
}

/** Every event of a topic, with its Msg. */
async function events(t: Topic) {
  const out = [];
  for await (const e of t.read()) out.push({ ...e, msg: decodeMsg(e.payload) });
  return out;
}

/** Incompressible bytes, more than `randomBytes` gives at once. */
const noise = (n: number) =>
  bytes
    .concat(...Array.from({ length: Math.ceil(n / 65_536) }, () => bytes.randomBytes(65_536)))
    .slice(0, n);

const logRows = async (db: Db) => (await db.table('log').query().limit(10_000).page()).rows;

describe('the system tables (§11.5)', () => {
  it('exist after open, also in a database created before them', async () => {
    const name = ns();
    const db = await shop(name);
    const has = (d: Db) =>
      d.transaction(async (tx) => {
        const got = [];
        for (const t of SYSTEM_TABLES) got.push((await tx.tableRecordOrNone(t))?.name);
        return got;
      });
    expect(await has(db)).toEqual(SYSTEM_TABLES);
    expect(db.tableNames().sort()).toEqual(['log', 'orders']);
    // A database that lacks one (made before step 6) gets it on open.
    await db.transaction(async (tx) => tx.raw.delete(db.keys.tableRecord('$sched')));
    db.close();
    const again = await shop(name);
    expect(await has(again)).toEqual(SYSTEM_TABLES);
    again.close();
  });

  it('app tables may not start with $', async () => {
    const r = Db.open(fs, ns(), {
      ...quiet,
      locks: null,
      schema: 1,
      migrations: { 1: (m) => m.createTable('$mine', { pk: ['id'] }) },
    });
    expect(await code(r)).toBe('bad_value');
  });
});

describe('emit (§11.1)', () => {
  it('appends only if the transaction commits, once across re-runs', async () => {
    const db = await shop(ns());
    const t = db.topic('emit', ns());
    await expect(
      db.transaction(async (tx) => {
        tx.emit(t, 'order.paid', { id: 'a' });
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    expect(await events(t)).toEqual([]);

    // A conflict re-runs the function: the message is emitted once.
    const other = await shop(db.ns);
    await db.table('orders').put({ id: 'o1' });
    let runs = 0;
    await db.transaction(async (tx) => {
      runs++;
      await tx.table<Order>('orders').get('o1');
      if (runs === 1) await other.table('orders').put({ id: 'o1', status: 'x' });
      await tx.table('orders').put({ id: 'o1', status: 'paid' });
      tx.emit(t, 'order.paid', { id: 'o1' }, { key: 'o1', headers: { trace: 't' } });
    });
    expect(runs).toBe(2);
    const ev = await events(t);
    expect(ev.map((e) => [e.msg.type, e.msg.body, e.msg.headers])).toEqual([
      ['order.paid', { id: 'o1' }, { trace: 't' }],
    ]);
    expect(ev[0]!.keyToken).toEqual(t.keyToken('o1'));

    // Outside a transaction: a commit of its own; the id is the message's.
    const id = await db.emit(t, 'order.shipped', { id: 'o1' });
    expect((await events(t)).at(-1)!.msg.id).toEqual(id);
    // `zen.` types are the class's own.
    expect(await code(db.emit(t, 'zen.change', {}))).toBe('bad_value');
    other.close();
    db.close();
  });

  it('a large body goes to parts in the same commit', async () => {
    const db = await shop(ns());
    const t = db.topic('big', ns());
    const blob = noise(150_000);
    await db.emit(t, 'photo', { blob });
    const [e] = await events(t);
    expect(e!.msg.body).toBeUndefined();
    expect(e!.msg.bodyRef?.parts).toBeGreaterThan(1);
    const m = await openMessage<{ blob: Uint8Array }>(db, t, e!);
    expect(m.body.blob).toEqual(blob);
    // Parts deleted (as `gc` will, §10.3): the message is poisoned.
    await db.transaction(async (tx) => tx.raw.clearRange(...db.keys.range('m', e!.msg.id)));
    expect(await code(openMessage(db, t, e!))).toBe('poisoned');
    db.close();
  });

  it('tx.after runs after the commit only', async () => {
    const db = await shop(ns());
    const seen: string[] = [];
    await db.transaction(async (tx) => {
      tx.after(() => seen.push('a'));
      await tx.table('log').insert({ id: 'x' });
    });
    await expect(
      db.transaction(async (tx) => {
        tx.after(() => seen.push('b'));
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    expect(seen).toEqual(['a']);
    db.close();
  });
});

describe('on (§11.2)', () => {
  it('from now, and after an offset (history, then live)', async () => {
    const db = await shop(ns());
    const t = db.topic('on', ns());
    await db.emit(t, 'n', 0);
    const got: number[] = [];
    const sub = await db.on<number>(t, (m) => {
      got.push(m.body);
    });
    for (let i = 1; i <= 3; i++) await db.emit(t, 'n', i);
    await until(() => got.length === 3);
    expect(got).toEqual([1, 2, 3]);

    const [first] = await events(t);
    const later: Message<number>[] = [];
    const sub2 = await db.on<number>(
      t,
      (m) => {
        later.push(m);
      },
      { after: first!.offset },
    );
    await db.emit(t, 'n', 4);
    await until(() => later.length === 4);
    expect(later.map((m) => m.body)).toEqual([1, 2, 3, 4]);
    expect(later[0]!.topic).toBe(t);
    expect(later[0]!.eventId).toEqual(bytes.concat(t.id, later[0]!.offset));
    await sub.close();
    await sub2.close();
    db.close();
  });

  it('a durable cursor: effects exactly once per device, across a crash', async () => {
    const name = ns();
    const db = await shop(name);
    const t = db.topic('durable', name);
    let failAt = 6;
    let errors = 0;
    const handler = async (m: Message<number>, tx: DbTransaction) => {
      if (m.body === failAt) {
        failAt = -1;
        throw new Error('crash');
      }
      await tx.table('log').insert({ id: `n${m.body}` });
    };
    const sub = await db.on<number>(t, handler, { cursor: 'c1', onError: () => errors++ });
    for (let i = 1; i <= 8; i++) await db.emit(t, 'n', i);
    // It stops at the failing event; rows 1..5 are in, with the cursor.
    await until(() => errors === 1);
    expect(sub.active).toBe(false);
    expect((await logRows(db)).length).toBe(5);
    db.close();

    // A restart (same device) resumes from the cursor, in batches of 3.
    const again = await shop(name);
    const sub2 = await again.on<number>(again.topic('durable', name), handler, {
      cursor: 'c1',
      after: undefined,
      batch: 3,
    });
    await until(async () => (await logRows(again)).length === 8);
    // A second subscription on the same cursor skips what is done.
    await sub2.close();
    const sub3 = await again.on<number>(again.topic('durable', name), handler, { cursor: 'c1' });
    await again.emit(again.topic('durable', name), 'n', 9);
    await until(async () => (await logRows(again)).length === 9);
    const cur = await again.table('$cursors').get([again.device, 'c1']);
    expect(cur?.offset).toBeInstanceOf(Uint8Array);
    await sub3.close();
    again.close();
  });

  it('a durable handler that keeps losing conflicts runs again later, not stopped', async () => {
    const name = ns();
    const db = await shop(name);
    const t = db.topic('busy', name);
    await db.table('log').insert({ id: 'hot', n: 0 });
    let errors = 0;
    let runs = 0;
    // One attempt per transaction, and a writer bumping the row it reads.
    const sub = await db.on<number>(
      t,
      async (m, tx) => {
        runs++;
        const hot = await tx.table('log').get('hot');
        await sleep(5);
        await tx.table('log').put({ id: 'hot', n: (hot?.n as number) + 1 });
        await tx.table('log').insert({ id: `e${m.body}` });
      },
      { cursor: 'busy', onError: () => errors++, txn: { attempts: 1 } },
    );
    let writing = true;
    const writer = (async () => {
      while (writing) {
        await db.table('log').update('hot', (r) => ({ ...r, n: (r.n as number) + 1 }));
      }
    })();
    for (let i = 1; i <= 5; i++) await db.emit(t, 'n', i);
    await until(() => runs > 8);
    writing = false;
    await writer;
    await until(
      async () => (await logRows(db)).filter((r) => String(r.id).startsWith('e')).length === 5,
    );
    expect(runs).toBeGreaterThan(5); // some conflicted and ran again
    expect(errors).toBe(0);
    expect(sub.active).toBe(true);
    await sub.close();
    await db.close();
  });

  it("start: 'earliest' reads the whole history; a stored cursor wins", async () => {
    const name = ns();
    const db = await shop(name);
    const t = db.topic('earliest', name);
    for (let i = 1; i <= 3; i++) await db.emit(t, 'n', i);
    const handler = async (m: Message<number>, tx: DbTransaction) => {
      await tx.table('log').insert({ id: `n${m.body}` });
    };
    const sub = await db.on<number>(t, handler, { cursor: 'e1', start: 'earliest' });
    await db.emit(t, 'n', 4);
    await until(async () => (await logRows(db)).length === 4);
    await sub.close();
    // Reopened, the cursor (after 4) wins over `earliest`: nothing again.
    await db.emit(t, 'n', 5);
    const sub2 = await db.on<number>(t, handler, { cursor: 'e1', start: 'earliest' });
    await until(async () => (await logRows(db)).length === 5);
    await sleep(100);
    expect((await logRows(db)).length).toBe(5);
    // A plain subscription with `earliest` sees every event too.
    const plain: number[] = [];
    const sub3 = await db.on<number>(t, (m) => void plain.push(m.body), { start: 'earliest' });
    await until(() => plain.length === 5);
    expect(plain).toEqual([1, 2, 3, 4, 5]);
    await sub2.close();
    await sub3.close();
    await db.close();
  });

  it('a prefix delivers the topics under it', async () => {
    const db = await shop(ns());
    const p = db.topic('pre', db.ns);
    const a = db.topic('pre', db.ns, 'a');
    const b = db.topic('pre', db.ns, 'b');
    const got: string[] = [];
    const sub = await db.on<string>({ prefix: p, topics: [a, b] }, (m) => {
      got.push(`${m.topic === a ? 'a' : 'b'}:${m.body}`);
    });
    await db.emit(a, 'x', '1');
    await db.emit(b, 'x', '2');
    await until(() => got.length === 2);
    expect(got).toEqual(['a:1', 'b:2']);
    await sub.close();
    db.close();
  });
});

describe('consume (§11.3)', () => {
  it('sequential: effects, emits with causation, one transaction per delivery', async () => {
    const db = await shop(ns());
    const t = db.topic('jobs', db.ns);
    const out = db.topic('done', db.ns);
    const h = db.consume<{ n: number }>(
      { group: 'worker', topic: t, mode: 'sequential', ttlMs: 2000, waitMs: 500 },
      async (m, tx) => {
        await tx.table('log').insert({ id: `j${m.body.n}` });
        tx.emit(out, 'job.done', { n: m.body.n });
      },
    );
    for (let i = 0; i < 4; i++) await db.emit(t, 'job', { n: i });
    await until(async () => (await logRows(db)).length === 4);
    await until(async () => (await events(out)).length === 4);
    const src = await events(t);
    const done = await events(out);
    for (const [i, e] of done.entries()) {
      expect(e.causation).toEqual(bytes.concat(t.id, src[i]!.offset));
    }
    await h.stop();
    db.close();
  });

  it('a handler that throws is nacked, then dead-lettered', async () => {
    const db = await shop(ns());
    const t = db.topic('poison', db.ns);
    let calls = 0;
    const errs: unknown[] = [];
    const h = db.consume(
      {
        group: 'p',
        topic: t,
        mode: 'sequential',
        maxAttempts: 2,
        ttlMs: 2000,
        waitMs: 500,
        onError: (e) => errs.push(e),
      },
      async () => {
        calls++;
        throw new Error('bad');
      },
    );
    await db.emit(t, 'job', 1);
    await until(() => h.group !== undefined);
    await until(async () => (await h.group!.dlq.list()).length === 1);
    expect(calls).toBe(2);
    expect(errs.length).toBe(2);
    await h.stop();
    db.close();
  });

  it('per_key across two instances: every message once', async () => {
    const name = ns();
    const a = await shop(name);
    const b = await shop(name);
    const t = a.topic('keys', name);
    const errs: unknown[] = [];
    const opts = {
      group: 'pk',
      topic: t,
      mode: 'per_key' as const,
      maxInflight: 4,
      waitMs: 300,
      onError: (e: unknown) => errs.push(e),
    };
    const handler = async (m: Delivered<number>, tx: DbTransaction) => {
      await tx.table('log').insert({ id: `m${m.body}` });
    };
    const ha = a.consume(opts, handler);
    const hb = b.consume({ ...opts, topic: b.topic('keys', name) }, handler);
    await a.transaction(async (tx) => {
      for (let i = 0; i < 20; i++) tx.emit(t, 'm', i, { key: `k${i % 5}` });
    });
    await until(async () => (await logRows(a)).length === 20);
    await sleep(300);
    expect((await logRows(a)).length).toBe(20);
    expect(errs).toEqual([]);
    await ha.stop();
    await hb.stop();
    a.close();
    b.close();
  });

  it('runs only in the owner, and moves with it', async () => {
    const name = ns();
    const locks = new FakeLocks();
    const a = await shop(name, { locks });
    const b = await shop(name, { locks });
    await sleep(0);
    expect(a.isOwner).toBe(true);
    const t = b.topic('owned', name);
    const h = b.consume(
      { group: 'o', topic: t, mode: 'sequential', ttlMs: 2000, waitMs: 300 },
      async (m, tx) => {
        await tx.table('log').insert({ id: `o${m.body}` });
      },
    );
    await b.emit(t, 'x', 1);
    await sleep(500);
    expect(h.running).toBe(false);
    expect((await logRows(b)).length).toBe(0);
    a.close();
    await until(() => h.running);
    await until(async () => (await logRows(b)).length === 1);
    await h.stop();
    b.close();
  });

  it('manual: msg.ack() and begin({consumes})', async () => {
    const db = await shop(ns());
    const t = db.topic('manual', db.ns);
    let calls = 0;
    const h = db.consumeManual<number>(
      { group: 'man', topic: t, mode: 'sequential', ttlMs: 2000, waitMs: 300 },
      async (m) => {
        calls++;
        if (m.body % 2) return m.ack();
        const tx = db.begin({ consumes: m });
        await tx.table('log').insert({ id: `e${m.body}` });
        tx.emit(t.child('out'), 'even', m.body);
        await tx.commit();
      },
    );
    for (let i = 0; i < 4; i++) await db.emit(t, 'n', i);
    await until(async () => (await events(t.child('out'))).length === 2);
    await sleep(500);
    expect(calls).toBe(4);
    expect((await logRows(db)).map((r) => r.id).sort()).toEqual(['e0', 'e2']);
    const out = await events(t.child('out'));
    expect(out[0]!.causation.length).toBeGreaterThan(0);
    await h.stop();
    db.close();
  });
});

describe('change events (§12.7)', () => {
  it('one event per changed row per transaction, with its final state', async () => {
    const db = await shop(ns());
    const orders = db.table<Order>('orders');
    const got: Message<ChangeBody<Order>>[] = [];
    const sub = await orders.on((m) => {
      got.push(m);
    });
    await db.transaction(async (tx) => {
      const o = tx.table<Order>('orders');
      await o.insert({ id: 'o1', status: 'new' });
      await o.update('o1', (r) => ({ ...r, status: 'paid' }));
      await o.insert({ id: 'o2' });
      await o.delete('o2');
    });
    await orders.delete('o1');
    await orders.put({ id: 'big', blob: noise(120_000) });
    await until(() => got.length === 3);
    expect(got.map((m) => [m.type, m.body.op, m.body.pk, m.body.row?.status])).toEqual([
      ['zen.change', 'put', 'o1', 'paid'],
      ['zen.change', 'delete', 'o1', undefined],
      ['zen.change', 'put', 'big', undefined],
    ]);
    // A full row too large for one envelope is left out; the key is the pk element.
    expect(got[2]!.body.row).toBeUndefined();
    expect(got[0]!.keyToken).toEqual(got[0]!.topic.keyToken(encode('o1')));
    expect(await code(db.table('log').on(() => {}))).toBe('bad_value');
    await sub.close();
    db.close();
  });
});

describe('ephemeral messages (§11.4)', () => {
  it('reach subscribers, not the log', async () => {
    const name = ns();
    const a = await shop(name);
    const b = await shop(name);
    const got: string[] = [];
    await b.onEphemeral<string>(b.topic('presence', name), (m) => {
      got.push(`${m.type}:${m.body}`);
    });
    await a.publishEphemeral(a.topic('presence', name), 'typing', 'room1');
    await until(() => got.length === 1);
    expect(got).toEqual(['typing:room1']);
    expect(await events(a.topic('presence', name))).toEqual([]);
    a.close();
    b.close();
  });
});

/** An in-memory Web Locks queue (see txn.test.ts). */
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
