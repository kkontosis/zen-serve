// CRDT tables against a spawned zen-serve (zendb.md §19): rows the server
// merges field by field, from two installations (two sessions, two clocks).
import {
  bytes,
  memoryClockStore,
  type Session,
  type Topic,
  TreeClock,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { crdtRange } from '../src/crdt.js';
import {
  type ChangeBody,
  Db,
  DbError,
  type DbOptions,
  decodeMsg,
  type Fields,
  type Message,
  type Migrator,
} from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let fs2: UnlockedFs;
let n = 0;
const ns = () => `cr${++n}`;
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Another installation of the same device: its own session and clock. */
async function installation(clock?: TreeClock): Promise<UnlockedFs> {
  const s = await w.client.signInDevice(w.admin);
  if (clock) s.useClock(clock);
  return s.fs(1).unlock({ device: w.admin.device });
}

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  fs2 = await installation();
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

async function until(cond: () => boolean | Promise<boolean>, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(25);
  }
}

async function events(t: Topic) {
  const out = [];
  for await (const e of t.read()) out.push({ key: e.keyToken, msg: decodeMsg(e.payload) });
  return out;
}

interface Card extends Fields {
  id: string;
  title?: string;
  status?: string;
  votes?: number;
  labels?: string[];
  n?: number;
}

const migrations: DbOptions['migrations'] = {
  1: async (m) => {
    await m.createTable('cards', {
      pk: ['id'],
      merge: 'crdt',
      fields: { votes: 'counter', labels: 'set' },
      changes: {},
    });
    await m.createTable('log', { pk: ['id'] });
  },
};

const open = (f: UnlockedFs, name: string, extra: DbOptions = {}) =>
  Db.open(f, name, { ...quiet, locks: null, scheduler: false, schema: 1, migrations, ...extra });

/** One database seen from two installations. */
async function pair() {
  const name = ns();
  const a = await open(fs, name);
  const b = await open(fs2, name);
  return {
    a,
    b,
    close: () => {
      a.close();
      b.close();
    },
  };
}

/** The error codes of `/v1/commit` while `fn` runs; `inject` fails the first commits. */
async function commitsDuring(
  s: Session,
  fn: () => Promise<unknown>,
  inject: string[] = [],
): Promise<string[]> {
  const codes: string[] = [];
  const orig = s.call.bind(s);
  s.call = (async (path, enc, dec, req) => {
    if (path !== '/v1/commit') return orig(path, enc, dec, req);
    const c = inject.shift();
    if (c) {
      codes.push(c);
      throw new ZenError(409, c, 'injected');
    }
    try {
      return await orig(path, enc, dec, req);
    } catch (e) {
      if (e instanceof ZenError) codes.push(e.code);
      throw e;
    }
  }) as Session['call'];
  try {
    await fn();
  } finally {
    s.call = orig;
  }
  return codes;
}

describe('CRDT tables (§19)', () => {
  it('insert, get and patch; an undeclared field keeps its name', async () => {
    const { a, close } = await pair();
    const cards = a.table<Card>('cards');
    await cards.insert({ id: 'c1', title: 'Buy milk', votes: 2, labels: ['b', 'a'] });
    expect(await cards.get('c1')).toEqual({
      id: 'c1',
      title: 'Buy milk',
      votes: 2,
      labels: ['a', 'b'],
    });
    await cards.patch('c1', { title: 'Buy oat milk', status: 'open' });
    await cards.patch('c1', { status: undefined });
    expect(await cards.get('c1')).toEqual({
      id: 'c1',
      title: 'Buy oat milk',
      votes: 2,
      labels: ['a', 'b'],
    });
    expect(await cards.get('nope')).toBeUndefined();
    close();
  });

  it('delete hides a row; patch does not resurrect it, insert does', async () => {
    const { a, close } = await pair();
    const cards = a.table<Card>('cards');
    await cards.insert({ id: 'c', title: 't1' });
    expect(await cards.delete('c')).toBe(true);
    expect(await cards.get('c')).toBeUndefined();
    await cards.patch('c', { title: 't2' });
    expect(await cards.get('c')).toBeUndefined();
    await cards.insert({ id: 'c' });
    expect(await cards.get('c')).toEqual({ id: 'c', title: 't2', votes: 0, labels: [] });
    close();
  });

  it('two installations: different fields both survive; one field, the later write wins', async () => {
    const { a, b, close } = await pair();
    await a.table<Card>('cards').insert({ id: 'c' });
    await Promise.all([
      a.table<Card>('cards').patch('c', { title: 'from a' }),
      b.table<Card>('cards').patch('c', { status: 'from b' }),
    ]);
    expect(await b.table<Card>('cards').get('c')).toMatchObject({
      title: 'from a',
      status: 'from b',
    });
    await a.table<Card>('cards').patch('c', { title: 'a again' });
    await b.table<Card>('cards').patch('c', { title: 'b last' });
    expect((await a.table<Card>('cards').get('c'))!.title).toBe('b last');
    close();
  });

  it('counters: installations sum, parallel increments of one all count', async () => {
    const { a, b, close } = await pair();
    const ca = a.table<Card>('cards');
    await ca.insert({ id: 'c' });
    await ca.incr('c', 'votes', 2);
    await b.table<Card>('cards').incr('c', 'votes', 3);
    await ca.incr('c', 'votes', -1);
    expect((await ca.get('c'))!.votes).toBe(4);
    await Promise.all(Array.from({ length: 10 }, () => ca.incr('c', 'votes')));
    expect((await b.table<Card>('cards').get('c'))!.votes).toBe(14);
    // A new Db on the same installation continues its total.
    const a2 = await open(fs, a.ns);
    await a2.table<Card>('cards').incr('c', 'votes', 100);
    expect((await ca.get('c'))!.votes).toBe(114);
    a2.close();
    close();
  });

  it('a conflict re-run does not count an increment twice', async () => {
    const { a, b, close } = await pair();
    await a.table<Card>('cards').insert({ id: 'c', votes: 5 });
    let runs = 0;
    await a.transaction(async (tx) => {
      runs++;
      await tx.table('log').get('x');
      // Another writer changes the row read, on the first run only.
      if (runs === 1) await b.table('log').put({ id: 'x', v: 1 });
      await tx.table<Card>('cards').incr('c', 'votes', 1);
      await tx.table('log').put({ id: 'y' });
    });
    expect(runs).toBe(2);
    expect((await b.table<Card>('cards').get('c'))!.votes).toBe(6);
    close();
  });

  it('sets: remove takes the copies seen; an add it did not see stays (add-wins)', async () => {
    const { a, b, close } = await pair();
    const ca = a.table<Card>('cards');
    await ca.insert({ id: 'c', labels: ['red', 'blue'] });
    await b.table<Card>('cards').remove('c', 'labels', 'red');
    expect((await ca.get('c'))!.labels).toEqual(['blue']);
    await b.transaction(async (tx) => {
      await tx.table<Card>('cards').remove('c', 'labels', 'blue');
      // a adds blue again after b read its copies, before b commits.
      await ca.add('c', 'labels', 'blue');
    });
    expect((await ca.get('c'))!.labels).toEqual(['blue']);
    await b.table<Card>('cards').remove('c', 'labels', 'blue');
    expect((await ca.get('c'))!.labels).toEqual([]);
    // An add and a remove in one transaction: removed.
    await a.transaction(async (tx) => {
      const t = tx.table<Card>('cards');
      await t.add('c', 'labels', 'green');
      expect((await t.get('c'))!.labels).toEqual(['green']);
      await t.remove('c', 'labels', 'green');
    });
    expect((await ca.get('c'))!.labels).toEqual([]);
    close();
  });

  it('one transaction: rows, CRDT operations and emits commit together or not at all', async () => {
    const { a, close } = await pair();
    const t = a.topic('notes', a.ns);
    await expect(
      a.transaction(async (tx) => {
        await tx.table('log').insert({ id: 'l1' });
        await tx.table<Card>('cards').insert({ id: 'c1', title: 'x' });
        await tx.table<Card>('cards').incr('c1', 'votes', 2);
        tx.emit(t, 'made', 'c1');
        throw new Error('no');
      }),
    ).rejects.toThrow('no');
    expect(await a.table('log').get('l1')).toBeUndefined();
    expect(await a.table('cards').get('c1')).toBeUndefined();
    expect(await events(t)).toEqual([]);
    await a.transaction(async (tx) => {
      await tx.table('log').insert({ id: 'l1' });
      await tx.table<Card>('cards').insert({ id: 'c1', title: 'x' });
      // Its own writes, before they commit.
      expect(await tx.table<Card>('cards').get('c1')).toMatchObject({ title: 'x' });
      tx.emit(t, 'made', 'c1');
    });
    expect(await a.table('log').get('l1')).toEqual({ id: 'l1' });
    expect(await a.table('cards').get('c1')).toMatchObject({ title: 'x' });
    expect((await events(t)).length).toBe(1);
    close();
  });

  it('CRDT reads are not in the read set', async () => {
    const { a, b, close } = await pair();
    await a.table<Card>('cards').insert({ id: 'c', title: 'one' });
    let runs = 0;
    await a.transaction(async (tx) => {
      runs++;
      const c = await tx.table<Card>('cards').get('c');
      if (runs === 1) await b.table<Card>('cards').patch('c', { title: 'two' });
      await tx.table('log').put({ id: 'seen', title: c!.title! });
    });
    expect(runs).toBe(1);
    expect(await a.table('log').get('seen')).toEqual({ id: 'seen', title: 'one' });
    close();
  });

  it('rebases after clock_skew from a clock persisted ahead', async () => {
    const name = ns();
    const a = await open(fs, name);
    const ahead = new TreeClock({ store: memoryClockStore(zw.hlc(Date.now() + 10 * 60_000, 0)) });
    const fs3 = await installation(ahead);
    const c = await open(fs3, name);
    await a.table<Card>('cards').insert({ id: 'c', title: 'a' });
    const codes = await commitsDuring(fs3.session, () =>
      c.table<Card>('cards').patch('c', { title: 'skewed' }),
    );
    expect(codes).toEqual(['clock_skew']);
    expect((await a.table<Card>('cards').get('c'))!.title).toBe('skewed');
    expect(zw.hlcMs(ahead.last)).toBeLessThan(Date.now() + 60_000);
    a.close();
    c.close();
  });

  it('a stale_op is rebased with fresh timestamps', async () => {
    const { a, close } = await pair();
    await a.table<Card>('cards').insert({ id: 'c' });
    const before = a.fs.session.clock.last;
    const codes = await commitsDuring(
      a.fs.session,
      () => a.table<Card>('cards').patch('c', { title: 'rebased' }),
      ['stale_op'],
    );
    expect(codes).toEqual(['stale_op']);
    expect((await a.table<Card>('cards').get('c'))!.title).toBe('rebased');
    expect(a.fs.session.clock.last).toBeGreaterThan(before);
    close();
  });

  it('change events: one per touched row per transaction, keyed by pk, no row', async () => {
    const { a, close } = await pair();
    const got: ChangeBody[] = [];
    const sub = await a.table('cards').on((m: Message<ChangeBody>) => {
      got.push(m.body);
    });
    await a.transaction(async (tx) => {
      const t = tx.table<Card>('cards');
      await t.insert({ id: 'c1' });
      await t.patch('c1', { title: 'x' });
      await t.incr('c2', 'votes');
    });
    await a.transaction(async (tx) => {
      const t = tx.table<Card>('cards');
      await t.patch('c1', { title: 'y' });
      await t.delete('c1');
    });
    await until(() => got.length === 3);
    expect(got).toEqual([
      { op: 'put', table: 'cards', pk: 'c1' },
      { op: 'put', table: 'cards', pk: 'c2' },
      { op: 'delete', table: 'cards', pk: 'c1' },
    ]);
    const evs = await events(a.topic('zen', 'db', a.ns, 'changes', 'cards'));
    expect(evs.length).toBe(3);
    expect(bytes.equal(evs[0]!.key!, evs[2]!.key!)).toBe(true);
    await sub.close();
    close();
  });

  it('scans: predicates, paging with cursors, orderBy, needs_index', async () => {
    const { a, close } = await pair();
    const cards = a.table<Card>('cards');
    await a.transaction(async (tx) => {
      for (let i = 0; i < 30; i++) {
        await tx.table<Card>('cards').insert({ id: `c${i}`, n: i, status: i % 2 ? 'odd' : 'even' });
      }
    });
    await cards.delete('c0');
    const seen: number[] = [];
    let cursor: Awaited<ReturnType<ReturnType<typeof cards.query>['page']>>['cursor'];
    for (;;) {
      const p = await cards.query().where('status', '=', 'even').limit(4).after(cursor).page();
      seen.push(...p.rows.map((r) => r.n!));
      cursor = p.cursor;
      if (!cursor) break;
    }
    expect(seen.sort((x, y) => x - y)).toEqual(Array.from({ length: 14 }, (_, i) => 2 * i + 2));
    const top = await cards.query().where('n', '>=', 25).orderBy('n', 'desc').all();
    expect(top.map((r) => r.n)).toEqual([29, 28, 27, 26, 25]);
    expect(await code(cards.query().orderBy('n').limit(3).page())).toBe('needs_index');
    // By pk: a lookup.
    expect(await cards.query().where('id', '=', 'c7').all()).toMatchObject([{ n: 7 }]);
    expect(await cards.query().where('id', '=', 'c0').all()).toEqual([]);
    close();
  });

  it('refuses what CRDT tables do not offer', async () => {
    const { a, close } = await pair();
    const cards = a.table<Card>('cards');
    await cards.insert({ id: 'c' });
    expect(await code(cards.put({ id: 'c' }))).toBe('bad_value');
    expect(await code(cards.update('c', (r) => r))).toBe('bad_value');
    expect(await code(cards.patch('c', { votes: 3 }))).toBe('bad_value');
    expect(await code(cards.patch('c', { id: 'd' }))).toBe('bad_value');
    expect(await code(cards.incr('c', 'title'))).toBe('bad_value');
    expect(await code(cards.incr('c', 'votes', 1.5))).toBe('bad_value');
    expect(await code(cards.add('c', 'votes', 1))).toBe('bad_value');
    expect(await code(a.table('log').patch('x', {}))).toBe('bad_value');
    const name = ns();
    const step2 = (fn: (m: Migrator) => Promise<void>) =>
      open(fs, name, { schema: 2, migrations: { ...migrations, 2: fn } });
    expect(
      await code(
        step2((m) => m.createIndex('cards', 'u', { fields: [['title', 'text']], unique: true })),
      ),
    ).toBe('bad_value');
    expect(
      await code(step2((m) => m.createTable('x', { pk: ['id'], merge: 'crdt', pad: true }))),
    ).toBe('bad_value');
    expect(
      await code(
        step2((m) => m.createTable('x', { pk: ['id'], merge: 'crdt', changes: { image: 'full' } })),
      ),
    ).toBe('bad_value');
    expect(
      await code(step2((m) => m.createTable('x', { pk: ['id'], fields: { v: 'counter' } }))),
    ).toBe('bad_value');
    close();
  });

  it('unsupported without the server feature', async () => {
    const client = fs.session.client;
    const orig = client.info.bind(client);
    client.info = async () => {
      const i = await orig();
      return { ...i, features: i.features.filter((f) => f !== 'crdt_rows') };
    };
    try {
      expect(await code(open(fs, ns()))).toBe('unsupported');
    } finally {
      client.info = orig;
    }
  });

  it('dropTable deletes the rows, then the table', async () => {
    const name = ns();
    const a = await open(fs, name, { pageSize: 4 });
    await a.transaction(async (tx) => {
      for (let i = 0; i < 10; i++) await tx.table<Card>('cards').insert({ id: `c${i}` });
    });
    const t = (await a.transaction((tx) => tx.tableRecord('cards')))!;
    a.close();
    const b = await open(fs, name, {
      pageSize: 4,
      schema: 2,
      migrations: { ...migrations, 2: (m) => m.dropTable('cards') },
    });
    expect(b.tableNames()).toEqual(['log']);
    const { objects } = await crdtRange(b, t, undefined, 100);
    expect(objects.length).toBe(10);
    expect(objects.every((s) => s.row && !s.row.alive)).toBe(true);
    b.close();
  });
});
