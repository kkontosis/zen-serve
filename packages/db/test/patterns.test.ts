// The broker's patterns against a spawned zen-serve (zendb.md §12.2–12.4,
// §12.6): the scheduler, work queues and request/reply.
import { bytes, type Topic, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openMessage } from '../src/broker.js';
import {
  attemptOf,
  Db,
  DbError,
  type DbOptions,
  decodeMsg,
  type Fields,
  type Message,
  ReplyError,
  Scheduler,
} from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `pt${++n}`;
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

/** A database with `log(id)`, and a fast scheduler (1 s lease). */
function open(name: string, extra: DbOptions = {}): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    locks: null,
    schedulerOptions: { pollMs: 100, ttlMs: 1000 },
    schema: 1,
    migrations: { 1: (m) => m.createTable('log', { pk: ['id'] }) },
    ...extra,
  });
}

/** Every event of a topic, with its Msg. */
async function events(t: Topic) {
  const out = [];
  for await (const e of t.read()) out.push({ ...e, msg: decodeMsg(e.payload) });
  return out;
}

const noise = (n: number) =>
  bytes
    .concat(...Array.from({ length: Math.ceil(n / 65_536) }, () => bytes.randomBytes(65_536)))
    .slice(0, n);

const rows = async (db: Db, table: string) =>
  (await db.table(table).query().limit(10_000).page()).rows;

/** Whether a promise settled within `ms`. */
const settles = (p: Promise<unknown>, ms: number) =>
  Promise.race([
    p.then(
      () => true,
      () => true,
    ),
    sleep(ms).then(() => false),
  ]);

describe('the scheduler (§12.4)', () => {
  it('delivers in `at` order, once each, never early', async () => {
    const db = await open(ns());
    const t = db.topic('sched', ns());
    const got: { type: string; deliverAt?: number; now: number }[] = [];
    await db.on(t, (m) => {
      got.push({ type: m.type, deliverAt: m.deliverAt, now: db.fs.session.clock.now() });
    });
    const now = db.fs.session.clock.now();
    await db.schedule(t, 'b', 2, { at: Math.ceil(now + 600) });
    await db.schedule(t, 'a', 1, { delayMs: 300 });
    await db.schedule(t, 'z', 0, { at: Math.floor(now) });
    await until(() => got.length === 3);
    await sleep(400);
    expect(got.map((g) => g.type)).toEqual(['z', 'a', 'b']);
    for (const g of got) expect(g.now).toBeGreaterThanOrEqual(g.deliverAt! - 20);
    expect(await rows(db, '$sched')).toEqual([]);
    db.close();
  });

  it('cancel: true before delivery, false after', async () => {
    const db = await open(ns());
    const t = db.topic('sched', ns());
    const id = await db.schedule(t, 'later', null, { delayMs: 400 });
    expect(await db.cancel(id)).toBe(true);
    const now = await db.schedule(t, 'now', null, { delayMs: 0 });
    await until(async () => (await events(t)).length === 1);
    expect(await db.cancel(now)).toBe(false);
    await sleep(600);
    expect((await events(t)).map((e) => e.msg.type)).toEqual(['now']);
    db.close();
  });

  it('a transaction that throws schedules nothing', async () => {
    const db = await open(ns());
    const t = db.topic('sched', ns());
    await expect(
      db.transaction(async (tx) => {
        await tx.schedule(t, 'x', null, { delayMs: 0 });
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    expect(await rows(db, '$sched')).toEqual([]);
    await expect(db.schedule(t, 'zen.x', null, { delayMs: 0 })).rejects.toThrow('bad_value');
    db.close();
  });

  it("deletes a large body's parts after its ttl (gc job)", async () => {
    const db = await open(ns());
    const t = db.topic('big', ns());
    await db.emit(t, 'blob', noise(150_000), { ttlMs: 300 });
    const [e] = await events(t);
    expect(e!.msg.bodyRef).toBeDefined();
    const [job] = await rows(db, '$sched');
    expect(job?.kind).toBe('gc');
    await until(async () => (await rows(db, '$sched')).length === 0);
    expect(await code(openMessage(db, t, e!))).toBe('poisoned');
    db.close();
  });

  it('two schedulers racing emit each row once', async () => {
    const name = ns();
    const a = await open(name, { scheduler: false });
    const b = await open(name, { scheduler: false });
    const t = a.topic('race', ns());
    await a.transaction(async (tx) => {
      for (let i = 0; i < 50; i++) await tx.schedule(t, 'r', i, { delayMs: 0 });
    });
    const sa = new Scheduler(a);
    const sb = new Scheduler(b);
    let done = 0;
    while (done < 50) {
      const [x, y] = await Promise.all([sa.runOnce(), sb.runOnce()]);
      done += x + y;
    }
    expect(done).toBe(50);
    const got = await events(t);
    expect(got.map((e) => e.msg.body).sort((p, q) => Number(p) - Number(q))).toEqual(
      Array.from({ length: 50 }, (_, i) => i),
    );
    expect(await rows(a, '$sched')).toEqual([]);
    a.close();
    b.close();
  });

  it('fails over to another instance when the leader dies', async () => {
    const name = ns();
    const a = await open(name);
    const b = await open(name);
    await until(() => a.scheduler!.leading || b.scheduler!.leading);
    const [lead, other] = a.scheduler!.leading ? [a, b] : [b, a];
    await lead.scheduler!.stop({ release: false });
    const t = lead.topic('failover', ns());
    const at = Date.now();
    await lead.schedule(t, 'after', null, { delayMs: 0 });
    await until(async () => (await events(t)).length === 1, 8000);
    expect(other.scheduler!.leading).toBe(true);
    // The lease had to lapse first (1 s).
    expect(Date.now() - at).toBeGreaterThan(300);
    a.close();
    b.close();
  });
});

describe('work queues (§12.2)', () => {
  it('spread: any worker takes any message, each once', async () => {
    const name = ns();
    const a = await open(name, { scheduler: false });
    const b = await open(name, { scheduler: false });
    const t = a.topic('jobs', ns());
    const by = new Map<string, string>();
    const worker = (db: Db, who: string) =>
      db.consume({ group: 'w', topic: t, mode: 'per_key', maxInflight: 4 }, async (m, tx) => {
        await sleep(30);
        by.set(String(m.body), who);
        await tx.table('log').insert({ id: String(m.body) });
      });
    worker(a, 'a');
    worker(b, 'b');
    for (let i = 0; i < 24; i++) await a.emit(t, 'job', i, { spread: true });
    await until(async () => (await rows(a, 'log')).length === 24, 15_000);
    expect(new Set(by.values())).toEqual(new Set(['a', 'b']));
    const keys = new Set((await events(t)).map((e) => bytes.hex(e.keyToken!)));
    expect(keys.size).toBe(24);
    a.close();
    b.close();
  });

  it("retry 'delay': attempts 1..max, then the class DLQ with the same id", async () => {
    const db = await open(ns());
    const t = db.topic('retry', ns());
    const seen: number[] = [];
    db.consume(
      {
        group: 'r',
        topic: t,
        mode: 'sequential',
        retry: 'delay',
        backoff: () => 100,
        maxAttempts: 3,
      },
      (m) => {
        seen.push(attemptOf(m));
        throw new Error('fails');
      },
    );
    const id = await db.emit(t, 'job', 'x');
    const dlq = t.child('zen.dlq');
    await until(async () => (await events(dlq)).length === 1);
    expect(seen).toEqual([1, 2, 3]);
    const [dead] = await events(dlq);
    expect(dead!.msg.id).toEqual(id);
    expect(dead!.msg.headers?.['zen.attempt']).toBe('3');
    // The copies were scheduled: each attempt is its own event.
    expect((await events(t)).length).toBe(3);
    db.close();
  });

  it("retry 'delay': success stops the retries", async () => {
    const db = await open(ns());
    const t = db.topic('retry', ns());
    const seen: number[] = [];
    db.consume(
      { group: 'r', topic: t, mode: 'sequential', retry: 'delay', backoff: () => 100 },
      async (m, tx) => {
        seen.push(attemptOf(m));
        if (attemptOf(m) < 2) throw new Error('once');
        await tx.table('log').insert({ id: 'done' });
      },
    );
    await db.emit(t, 'job', null);
    await until(async () => (await rows(db, 'log')).length === 1);
    await sleep(400);
    expect(seen).toEqual([1, 2]);
    expect(await events(t.child('zen.dlq'))).toEqual([]);
    db.close();
  });

  it('refuses reserved headers and types, and spread with a key', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('x', ns());
    expect(await code(db.emit(t, 'job', null, { headers: { 'zen.attempt': '9' } }))).toBe(
      'bad_value',
    );
    expect(await code(db.emit(t, 'zen.job', null))).toBe('bad_value');
    expect(await code(db.emit(t, 'job', null, { spread: true, key: 'k' }))).toBe('bad_value');
    expect(await code(db.request(t, 'zen.ask', null))).toBe('bad_value');
    db.close();
  });
});

describe('request/reply (§12.3)', () => {
  interface Add extends Fields {
    x: number;
    y: number;
    ms?: number;
  }

  /** A responder on `t` that logs each request and replies with the sum. */
  function adder(db: Db, t: Topic, extra: Partial<Parameters<Db['serve']>[0]> = {}) {
    const calls: Message[] = [];
    const h = db.serve<Add>(
      { group: 'add', topic: t, mode: 'sequential', ...extra },
      async (m, tx) => {
        calls.push(m);
        if (m.body.ms) await sleep(m.body.ms);
        if (m.body.x < 0) throw new ReplyError('negative', { x: m.body.x });
        await tx.table('log').insert({ id: bytes.hex(m.id) });
        return { sum: m.body.x + m.body.y };
      },
    );
    return { calls, h };
  }

  it('a round trip: the reply commits with the request’s effects', async () => {
    const name = ns();
    const a = await open(name, { scheduler: false });
    const b = await open(name, { scheduler: false });
    const t = a.topic('calc', ns());
    const { calls } = adder(b, t);
    const r = await a.request(t, 'add', { x: 1, y: 2 });
    expect(r.type).toBe('add.reply');
    expect(r.body).toEqual({ sum: 3 });
    expect(calls).toHaveLength(1);
    expect(r.corr).toEqual(calls[0]!.corr);
    expect(await rows(a, 'log')).toEqual([{ id: bytes.hex(calls[0]!.id) }]);
    a.close();
    b.close();
  });

  it('a timeout, then the late reply is dropped', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('calc', ns());
    adder(db, t);
    expect(await code(db.request(t, 'add', { x: 1, y: 1, ms: 800 }, { timeoutMs: 200 }))).toBe(
      'timeout',
    );
    const r = await db.request(t, 'add', { x: 2, y: 2 });
    expect(r.body).toEqual({ sum: 4 });
    db.close();
  });

  it('ReplyError: an error reply, and the request is consumed', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('calc', ns());
    const { calls } = adder(db, t);
    const e = await db.request(t, 'add', { x: -1, y: 0 }).catch((x) => x);
    expect(e).toBeInstanceOf(ReplyError);
    expect(e.message).toBe('negative');
    expect(e.data).toEqual({ x: -1 });
    await sleep(500);
    expect(calls).toHaveLength(1);
    expect(await rows(db, 'log')).toEqual([]);
    db.close();
  });

  it('dedup: a repeated corr gets the stored reply without the handler', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('calc', ns());
    const { calls } = adder(db, t, { dedup: true });
    const corr = bytes.randomBytes(16);
    const r1 = await db.request(t, 'add', { x: 3, y: 4 }, { corr });
    const r2 = await db.request(t, 'add', { x: 3, y: 4 }, { corr });
    expect(r1.body).toEqual({ sum: 7 });
    expect(r2.body).toEqual({ sum: 7 });
    expect(calls).toHaveLength(1);
    expect(await rows(db, 'log')).toHaveLength(1);
    const [row] = await rows(db, '$inbox');
    expect(row).toMatchObject({ scope: 'add', id: corr, reply: { type: 'add.reply' } });
    const [job] = await rows(db, '$sched');
    expect(job?.kind).toBe('gc');
    db.close();
  });

  it('an ephemeral reply', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('calc', ns());
    adder(db, t, { reply: 'ephemeral' });
    const r = await db.request(t, 'add', { x: 5, y: 5 });
    expect(r.body).toEqual({ sum: 10 });
    expect('offset' in r).toBe(false);
    db.close();
  });

  it('scatter-gather: every `on` responder answers', async () => {
    const name = ns();
    const a = await open(name, { scheduler: false });
    const b = await open(name, { scheduler: false });
    const c = await open(name, { scheduler: false });
    const t = a.topic('who', ns());
    for (const [db, who] of [
      [b, 'b'],
      [c, 'c'],
    ] as const) {
      await db.on(t, (m) => db.reply(m, who));
    }
    const all = await a.requestAll(t, 'ping', null, { timeoutMs: 800 });
    expect(all.map((r) => r.body).sort()).toEqual(['b', 'c']);
    const first = await a.requestAll(t, 'ping', null, { timeoutMs: 5000, max: 1 });
    expect(first).toHaveLength(1);
    for (const d of [a, b, c]) d.close();
  });

  it('a request in a transaction that throws is never sent', async () => {
    const db = await open(ns(), { scheduler: false });
    const t = db.topic('calc', ns());
    const { calls } = adder(db, t);
    let p: Promise<unknown> | undefined;
    await expect(
      db.transaction(async (tx) => {
        p = tx.request(t, 'add', { x: 1, y: 1 });
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    expect(await settles(p!, 500)).toBe(false);
    expect(calls).toHaveLength(0);
    expect(await events(t)).toEqual([]);
    // In a transaction that commits, it settles after the commit.
    const { q } = await db.transaction(async (tx) => ({ q: tx.request(t, 'add', { x: 2, y: 3 }) }));
    expect((await q).body).toEqual({ sum: 5 });
    db.close();
  });
});
