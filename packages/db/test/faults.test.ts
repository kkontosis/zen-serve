// The failures of zendb.md §13.2 against a spawned zen-serve: stalled and
// deposed handlers, racing claims, a dropped stream, a replayed commit,
// a scheduler dying mid-batch, a responder that doesn't answer. (The owner
// tab closing is in proxy.test.ts.)
import { bytes, type Session, type Topic, type UnlockedFs, ZenError, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import {
  Db,
  DbError,
  type DbOptions,
  type DirectSource,
  decodeMsg,
  type Message,
  Scheduler,
} from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `ft${++n}`;
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

async function until(cond: () => boolean | Promise<boolean>, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(25);
  }
}

function open(name: string, extra: DbOptions = {}): Promise<Db> {
  return Db.open(fs, name, {
    ...quiet,
    locks: null,
    scheduler: false,
    schema: 1,
    migrations: { 1: (m) => m.createTable('log', { pk: ['id'] }) },
    ...extra,
  });
}

async function events(t: Topic) {
  const out = [];
  for await (const e of t.read()) out.push(decodeMsg(e.payload));
  return out;
}

const rows = async (db: Db) => (await db.table('log').query().limit(1000).page()).rows;

/** A gate a handler waits on. */
function gate() {
  let open!: () => void;
  const p = new Promise<void>((r) => {
    open = r;
  });
  return { p, open };
}

/** Wrap `session.call` for `/v1/commit` (restored by the returned function). */
function onCommit(
  session: Session,
  wrap: (send: () => Promise<unknown>) => Promise<unknown>,
): () => void {
  const orig = session.call.bind(session);
  session.call = ((path, enc, dec, req) =>
    path === '/v1/commit'
      ? wrap(() => orig(path, enc, dec, req))
      : orig(path, enc, dec, req)) as Session['call'];
  return () => {
    session.call = orig;
  };
}

describe('failures (§13.2)', () => {
  for (const mode of ['sequential', 'per_key'] as const) {
    it(`${mode}: a stalled handler loses the event to another holder, and its late commit lands nothing`, async () => {
      const name = ns();
      const a = await open(name);
      const b = await open(name);
      const t = a.topic('work', name);
      const g = gate();
      const entered = gate();
      const errors: unknown[] = [];
      const ha = a.consume(
        { group: 'g', topic: t, mode, ttlMs: 1000, onError: (e) => errors.push(e) },
        async (_m, tx) => {
          entered.open();
          await g.p;
          await tx.table('log').insert({ id: 'a' });
        },
      );
      await a.emit(t, 'job', null, { key: 'k' });
      await entered.p;
      let ranB = 0;
      const hb = b.consume({ group: 'g', topic: t, mode, ttlMs: 1000 }, async (_m, tx) => {
        ranB++;
        await tx.table('log').insert({ id: 'b' });
      });
      // The lease (or claim) lapses: b gets the same event.
      await until(async () => (await rows(b)).length === 1, 8000);
      g.open();
      await sleep(500);
      expect(await rows(a)).toEqual([{ id: 'b' }]);
      expect(ranB).toBe(1);
      void ha.stop();
      void hb.stop();
      a.close();
      b.close();
    });
  }

  it('a dropped stream: `on` misses nothing and repeats nothing', async () => {
    const db = await open(ns());
    const t = db.topic('ticks', ns());
    const got: unknown[] = [];
    await db.on(t, (m: Message) => {
      got.push(m.body);
    });
    for (let i = 0; i < 5; i++) await db.emit(t, 'n', i);
    await until(() => got.length === 5);
    await (db.source as DirectSource).reconnect();
    for (let i = 5; i < 10; i++) await db.emit(t, 'n', i);
    await until(() => got.length >= 10);
    await sleep(300);
    expect(got).toEqual(Array.from({ length: 10 }, (_, i) => i));
    db.close();
  });

  it('a lost commit response: the replayed commit_id lands once', async () => {
    const db = await open(ns());
    const t = db.topic('once', ns());
    let lost = 1;
    const restore = onCommit(db.fs.session, async (send) => {
      const r = await send();
      if (lost-- > 0) throw new ZenError(0, 'network', 'the response was lost');
      return r;
    });
    try {
      await db.transaction(async (tx) => {
        await tx.table('log').insert({ id: 'x' });
        tx.emit(t, 'made', 'x');
      });
    } finally {
      restore();
    }
    expect(lost).toBe(-1);
    expect(await rows(db)).toEqual([{ id: 'x' }]);
    expect((await events(t)).length).toBe(1);
    db.close();
  });

  it('a scheduler dying mid-batch: the next one emits the same rows, once', async () => {
    const name = ns();
    const a = await open(name);
    const b = await open(name);
    const t = a.topic('due', name);
    await a.transaction(async (tx) => {
      for (let i = 0; i < 20; i++) await tx.schedule(t, 'r', i, { delayMs: 0 });
    });
    const sa = new Scheduler(a, { batch: 5 });
    expect(await sa.runOnce()).toBe(5);
    // Its next batch never commits.
    const restore = onCommit(a.fs.session, async () => {
      throw new DbError('closed', 'the process died');
    });
    try {
      await expect(sa.runOnce()).rejects.toThrow('the process died');
    } finally {
      restore();
    }
    const sb = new Scheduler(b, { batch: 5 });
    while ((await sb.runOnce()) > 0);
    const got = (await events(t)).map((m) => m.body as number).sort((x, y) => x - y);
    expect(got).toEqual(Array.from({ length: 20 }, (_, i) => i));
    a.close();
    b.close();
  });

  it('a responder that never answers: timeout, then a retry with the same corr runs once', async () => {
    const name = ns();
    const a = await open(name);
    const b = await open(name);
    const t = a.topic('svc', name);
    const corr = bytes.randomBytes(16);
    expect(
      await a.request(t, 'do', 1, { corr, timeoutMs: 300 }).then(
        () => 'ok',
        (e) => (e as DbError).code,
      ),
    ).toBe('timeout');
    let ran = 0;
    b.serve({ group: 's', topic: t, mode: 'sequential', dedup: true }, async (_m, tx) => {
      ran++;
      await tx.table('log').insert({ id: 'done' });
      return 'done';
    });
    // The first request is handled now; its reply finds no waiter.
    await until(async () => (await rows(a)).length === 1);
    const r = await a.request(t, 'do', 1, { corr, timeoutMs: 8000 });
    expect(r.body).toBe('done');
    expect(ran).toBe(1);
    expect(await rows(a)).toEqual([{ id: 'done' }]);
    a.close();
    b.close();
  });
});
