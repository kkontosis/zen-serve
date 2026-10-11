// CRDT tables offline (zendb.md §19.4, §19.7): read modes over the cache
// of merged states, the queue of writes, and three installations editing
// on and off line until they converge (§19.9). Offline is a wrapped
// `session.call` that fails with `network`.
import { type Session, type UnlockedFs, ZenError, zw } from '@zen/client';
import type { Commit, CrdtOp, Limits } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { Db, DbError, type DbOptions, type Fields, type SyncEvent } from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let fs2: UnlockedFs;
let fs3: UnlockedFs;
let n = 0;
const ns = () => `off${++n}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Another installation of the same device: its own session and clock. */
async function installation(): Promise<UnlockedFs> {
  const s = await w.client.signInDevice(w.admin);
  return s.fs(1).unlock({ device: w.admin.device });
}

beforeAll(async () => {
  // A 1 s horizon: an edit queued longer than that is refused (`stale_op`).
  w = await world({ limits: { claim_ttl_ms: 1000, crdt_horizon_secs: 1 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  fs2 = await installation();
  fs3 = await installation();
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

/** A session's network: calls by path, commits sent, and a switch to go offline. */
interface Net {
  offline: boolean;
  calls: string[];
  commits: Commit[];
  codes: string[];
  /** Apply this many commits, then lose their responses. */
  loseReplies: number;
  count(path: string): number;
  restore(): void;
}

function net(s: Session): Net {
  const orig = s.call.bind(s);
  const n: Net = {
    offline: false,
    calls: [],
    commits: [],
    codes: [],
    loseReplies: 0,
    count: (p) => n.calls.filter((c) => c === p).length,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.offline) throw new ZenError(0, 'network', 'offline (test)');
    n.calls.push(path);
    if (path === '/v1/commit') n.commits.push(req as Commit);
    try {
      const r = await orig(path, enc, dec, req);
      if (path === '/v1/commit' && n.loseReplies > 0) {
        n.loseReplies--;
        throw new ZenError(0, 'network', 'reply lost (test)');
      }
      return r;
    } catch (e) {
      if (path === '/v1/commit' && e instanceof ZenError) n.codes.push(e.code);
      throw e;
    }
  }) as Session['call'];
  return n;
}

interface Card extends Fields {
  id: string;
  title?: string;
  status?: string;
  votes?: number;
  labels?: string[];
}

const migrations: DbOptions['migrations'] = {
  1: async (m) => {
    await m.createTable('cards', {
      pk: ['id'],
      merge: 'crdt',
      fields: { votes: 'counter', labels: 'set' },
      changes: {},
    });
    await m.createTable('scratch', { pk: ['id'], merge: 'crdt' });
    await m.createTable('log', { pk: ['id'] });
  },
  2: async (m) => {
    await m.dropTable('scratch');
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

/** One database seen from two installations, each with its network. */
async function pair(extra: DbOptions = {}) {
  const name = ns();
  const a = await open(fs, name, extra);
  const b = await open(fs2, name, extra);
  const na = net(fs.session);
  const nb = net(fs2.session);
  return {
    a,
    b,
    na,
    nb,
    close: () => {
      na.restore();
      nb.restore();
      a.close();
      b.close();
    },
  };
}

const opsOf = (c: Commit, op: CrdtOp['op']) => (c.crdt_ops ?? []).filter((o) => o.op === op);

describe('read modes (§19.4)', () => {
  it('auto: fresh entries cost nothing, old ones a version check, changed ones a get', async () => {
    const { a, b, na, close } = await pair({ crdtCacheMs: 300 });
    const cards = a.table<Card>('cards', { wait: true });
    await cards.insert({ id: 'r1', title: 'one' });
    na.calls = [];
    expect((await cards.get('r1'))?.title).toBe('one');
    expect(na.count('/v1/crdt/get')).toBe(1);
    na.calls = [];
    expect((await cards.get('r1'))?.title).toBe('one');
    expect(na.count('/v1/crdt/get') + na.count('/v1/crdt/versions')).toBe(0);
    await sleep(350);
    na.calls = [];
    expect((await cards.get('r1'))?.title).toBe('one');
    expect([na.count('/v1/crdt/versions'), na.count('/v1/crdt/get')]).toEqual([1, 0]);
    // Another installation changes it: the version differs, so a get.
    await b.table<Card>('cards', { wait: true }).patch('r1', { title: 'two' });
    await sleep(350);
    na.calls = [];
    expect((await cards.get('r1'))?.title).toBe('two');
    expect([na.count('/v1/crdt/versions'), na.count('/v1/crdt/get')]).toEqual([1, 1]);
    // Offline: the cached row, however old; a row never read is `offline`.
    await sleep(350);
    na.offline = true;
    expect((await cards.get('r1'))?.title).toBe('two');
    expect(await code(cards.get('never-read'))).toBe('offline');
    close();
  });

  it('cache: an old entry without a call; server: always a get, and fails offline', async () => {
    const { a, na, close } = await pair({ crdtCacheMs: 100 });
    const cards = a.table<Card>('cards', { wait: true });
    await cards.insert({ id: 'r1', title: 'one' });
    await cards.get('r1');
    await sleep(150);
    na.calls = [];
    expect((await cards.get('r1', { read: 'cache' }))?.title).toBe('one');
    expect(na.count('/v1/crdt/get') + na.count('/v1/crdt/versions')).toBe(0);
    na.calls = [];
    await cards.get('r1', { read: 'server' });
    await cards.get('r1', { read: 'server' });
    expect(na.count('/v1/crdt/get')).toBe(2);
    na.offline = true;
    expect(await code(cards.get('r1', { read: 'server' }))).toBe('network');
    // The database's default mode.
    const c = await open(fs3, a.ns, { crdtRead: 'cache' });
    const nc = net(fs3.session);
    await c.table<Card>('cards').get('r1');
    nc.calls = [];
    await c.table<Card>('cards').get('r1');
    expect(nc.count('/v1/crdt/get')).toBe(0);
    nc.restore();
    c.close();
    close();
  });

  it('queries: cache answers from cached and queued rows; auto falls back offline', async () => {
    const { a, na, close } = await pair();
    const cards = a.table<Card>('cards', { wait: true });
    for (const id of ['q1', 'q2', 'q3']) await cards.insert({ id, status: 'open' });
    await cards.get('q1');
    await cards.get('q2');
    // q3 isn't cached; q4 exists only in the queue.
    na.offline = true;
    await a.table<Card>('cards').insert({ id: 'q4', status: 'open' });
    await a.table<Card>('cards').patch('q2', { status: 'done' });
    const ids = async (mode?: 'cache') => {
      const q = a.table<Card>('cards').query().where('status', '=', 'open');
      return (await (mode ? q.read(mode) : q).all()).map((r) => r.id);
    };
    expect(await ids('cache')).toEqual(['q1', 'q4']);
    expect(await ids()).toEqual(['q1', 'q4']);
    // Paging by pk.
    const p1 = await a.table<Card>('cards').query().read('cache').limit(2).page();
    expect(p1.rows.map((r) => r.id)).toEqual(['q1', 'q2']);
    const p2 = await a.table<Card>('cards').query().read('cache').limit(2).after(p1.cursor).page();
    expect(p2.rows.map((r) => r.id)).toEqual(['q4']);
    na.offline = false;
    await a.crdt.flush();
    // A scan is in stored-key order.
    expect((await ids()).sort()).toEqual(['q1', 'q3', 'q4']);
    close();
  });
});

describe('the queue (§19.7)', () => {
  it('writes resolve offline, show at once, and arrive once online; wait resolves then', async () => {
    const { a, b, na, close } = await pair();
    const events: SyncEvent[] = [];
    a.crdt.onSync((e) => events.push(e));
    na.offline = true;
    const cards = a.table<Card>('cards');
    await cards.insert({ id: 'w1', title: 'one', votes: 2, labels: ['a'] });
    expect(a.crdt.pending).toBe(1);
    expect(await cards.get('w1')).toEqual({ id: 'w1', title: 'one', votes: 2, labels: ['a'] });
    let done = false;
    const waited = a
      .table<Card>('cards', { wait: true })
      .patch('w1', { title: 'two' })
      .then(() => {
        done = true;
      });
    await sleep(100);
    expect(done).toBe(false);
    expect(await code(a.crdt.flush())).toBe('offline');
    expect(a.crdt.online).toBe(false);
    expect((await cards.get('w1'))?.title).toBe('two');
    na.offline = false;
    await a.crdt.flush();
    await waited;
    expect(a.crdt.pending).toBe(0);
    expect(a.crdt.online).toBe(true);
    expect(await b.table<Card>('cards').get('w1', { read: 'server' })).toEqual({
      id: 'w1',
      title: 'two',
      votes: 2,
      labels: ['a'],
    });
    expect(events.some((e) => !e.online)).toBe(true);
    expect(events.some((e) => e.online && e.sent)).toBe(true);
    close();
  });

  it('two installations edit offline, then reconnect: fields, counters and sets merge', async () => {
    const { a, b, na, nb, close } = await pair();
    await a.table<Card>('cards', { wait: true }).insert({ id: 'm1', votes: 1, labels: ['z'] });
    // Both have read the row: their removes see its dots.
    await a.table<Card>('cards').get('m1');
    await b.table<Card>('cards').get('m1');
    na.offline = true;
    nb.offline = true;
    const ca = a.table<Card>('cards');
    const cb = b.table<Card>('cards');
    await ca.patch('m1', { title: 'from a' });
    await cb.patch('m1', { status: 'from b' });
    await ca.incr('m1', 'votes', 3);
    await cb.incr('m1', 'votes', 4);
    await ca.add('m1', 'labels', 'x');
    await cb.add('m1', 'labels', 'y');
    // a removes the z it saw; b adds z again, which a never saw: it stays.
    await ca.remove('m1', 'labels', 'z');
    await cb.add('m1', 'labels', 'z');
    expect((await ca.get('m1'))?.labels).toEqual(['x']);
    na.offline = false;
    nb.offline = false;
    await Promise.all([a.crdt.flush(), b.crdt.flush()]);
    const want = {
      id: 'm1',
      title: 'from a',
      status: 'from b',
      votes: 8,
      labels: ['x', 'y', 'z'],
    };
    expect(await ca.get('m1', { read: 'server' })).toEqual(want);
    expect(await cb.get('m1', { read: 'server' })).toEqual(want);
    close();
  });

  it('batches single calls into few commits, coalescing them', async () => {
    const { a, b, na, close } = await pair({ crdtBatchMs: 300 });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'b1' });
    na.commits = [];
    const cards = a.table<Card>('cards');
    for (let i = 0; i < 50; i++) await cards.incr('b1', 'votes');
    for (let i = 0; i < 5; i++) await cards.patch('b1', { title: `t${i}` });
    await a.crdt.flush();
    expect(na.commits.length).toBeLessThanOrEqual(2);
    const ops = na.commits.flatMap((c) => c.crdt_ops ?? []);
    expect(ops.filter((o) => o.op === 'ctr').length).toBe(na.commits.length);
    expect(ops.filter((o) => o.op === 'lww').length).toBe(1);
    expect(await b.table<Card>('cards').get('b1', { read: 'server' })).toEqual({
      id: 'b1',
      title: 't4',
      votes: 50,
      labels: [],
    });
    close();
  });

  it('a transaction is one entry, sent whole even when the batch is cut', async () => {
    const { a, na, close } = await pair({ crdtBatchMs: 300 });
    // 16 operations of slack, 2 per single patch, 9 for the transaction:
    // the transaction can't join the patches before it.
    (a as unknown as { limits: Limits }).limits = { ...a.limits, max_commit_ops: 30 };
    await a.table<Card>('cards', { wait: true }).insert({ id: 's0' });
    na.commits = [];
    const cards = a.table<Card>('cards');
    for (let i = 0; i < 3; i++) await cards.patch('s0', { title: `t${i}` });
    await a.transaction(async (tx) => {
      for (const id of ['s1', 's2', 's3']) await tx.table<Card>('cards').insert({ id, title: id });
    });
    expect(a.crdt.pending).toBe(4);
    await a.crdt.flush();
    const withRows = na.commits.filter((c) => opsOf(c, 'row').length);
    expect(na.commits.length).toBe(2);
    expect(withRows.length).toBe(1);
    expect(opsOf(withRows[0]!, 'row').length).toBe(3);
    // Too large alone: refused when queued.
    (a as unknown as { limits: Limits }).limits = { ...a.limits, max_commit_ops: 18 };
    expect(
      await code(
        a.transaction(async (tx) => {
          for (const id of ['s4', 's5']) await tx.table<Card>('cards').insert({ id, title: id });
        }),
      ),
    ).toBe('too_large');
    close();
  });

  it('a lost response: the same commit_id again applies once', async () => {
    const { a, b, na, close } = await pair();
    await a.table<Card>('cards', { wait: true }).insert({ id: 'l1' });
    na.commits = [];
    na.loseReplies = 1;
    await a.table<Card>('cards', { wait: true }).incr('l1', 'votes', 5);
    expect(na.commits.length).toBe(2);
    expect(na.commits[0]!.commit_id).toEqual(na.commits[1]!.commit_id);
    expect((await b.table<Card>('cards').get('l1', { read: 'server' }))?.votes).toBe(5);
    await a.table<Card>('cards', { wait: true }).incr('l1', 'votes', 1);
    expect((await b.table<Card>('cards').get('l1', { read: 'server' }))?.votes).toBe(6);
    close();
  });

  it('an edit queued past the horizon is rebased on send, and wins', async () => {
    const { a, b, na, close } = await pair();
    await a.table<Card>('cards', { wait: true }).insert({ id: 'h1', title: 'start' });
    na.offline = true;
    await a.table<Card>('cards').patch('h1', { title: 'offline edit' });
    await sleep(1500);
    await b.table<Card>('cards', { wait: true }).patch('h1', { title: 'online edit' });
    na.offline = false;
    na.codes = [];
    await a.crdt.flush();
    expect(na.codes).toContain('stale_op');
    expect((await b.table<Card>('cards').get('h1', { read: 'server' }))?.title).toBe(
      'offline edit',
    );
    close();
  });

  it('mixed transactions flush the queue first; queue: false commits at once', async () => {
    const { a, b, na, close } = await pair({ crdtBatchMs: 5000 });
    na.offline = true;
    await a.table<Card>('cards').insert({ id: 'x1', title: 'queued' });
    na.offline = false;
    na.commits = [];
    await a.transaction(async (tx) => {
      await tx.table('log').insert({ id: 'L1' });
      await tx.table<Card>('cards').patch('x1', { title: 'mixed' });
    });
    expect(a.crdt.pending).toBe(0);
    // The queued insert went first, in its own commit (the transaction may
    // then re-run once: the flush moved its snapshot on).
    expect(opsOf(na.commits[0]!, 'row').length).toBe(1);
    expect(na.commits[0]!.writes).toBeUndefined();
    expect(na.commits.at(-1)!.writes?.length).toBe(1);
    expect((await b.table<Card>('cards').get('x1', { read: 'server' }))?.title).toBe('mixed');
    await a.transaction((tx) => tx.table<Card>('cards').patch('x1', { title: 'direct' }), {
      queue: false,
    });
    expect(a.crdt.pending).toBe(0);
    expect((await b.table<Card>('cards').get('x1', { read: 'server' }))?.title).toBe('direct');
    // Offline, a mixed transaction with CRDT writes can't flush, so it fails.
    na.offline = true;
    await a.table<Card>('cards').patch('x1', { title: 'later' });
    expect(
      await code(
        a.transaction(async (tx) => {
          await tx.table('log').insert({ id: 'L2' });
          await tx.table<Card>('cards').patch('x1', { title: 'mixed 2' });
        }),
      ),
    ).not.toBe('ok');
    na.offline = false;
    await a.crdt.flush();
    close();
  });

  it("a dropped table's queued writes are dropped, with a warning", async () => {
    const warnings: string[] = [];
    const { a, na, close } = await pair({ warn: (m) => warnings.push(m) });
    await a.table('scratch').get('s'); // the catalog is cached
    na.offline = true;
    await a.table('scratch').insert({ id: 's', v: 1 });
    // Another instance drops the table (schema 2).
    const c = await open(fs3, a.ns, { schema: 2 });
    c.close();
    na.offline = false;
    await a.crdt.flush();
    expect(a.crdt.pending).toBe(0);
    expect(warnings.some((m) => m.includes('scratch') && m.includes('gone'))).toBe(true);
    close();
  });

  it('a remove of an element added in the queue removes it (the dots learned)', async () => {
    const { a, b, na, close } = await pair();
    await a.table<Card>('cards', { wait: true }).insert({ id: 'd1', labels: ['keep'] });
    await a.table<Card>('cards').get('d1');
    na.offline = true;
    await a.transaction((tx) => tx.table<Card>('cards').add('d1', 'labels', 'k'));
    expect((await a.table<Card>('cards').get('d1'))?.labels).toEqual(['k', 'keep']);
    await a.table<Card>('cards').remove('d1', 'labels', 'k');
    expect((await a.table<Card>('cards').get('d1'))?.labels).toEqual(['keep']);
    na.offline = false;
    na.commits = [];
    await a.crdt.flush();
    // The add's commit first: the remove names the dot it got.
    expect(na.commits.length).toBe(2);
    expect(opsOf(na.commits[1]!, 'rem').length).toBe(1);
    expect((await b.table<Card>('cards').get('d1', { read: 'server' }))?.labels).toEqual(['keep']);
    close();
  });
});

describe('convergence (§19.9)', () => {
  function xorshift(seed: number) {
    let x = seed >>> 0 || 1;
    return () => {
      x ^= x << 13;
      x >>>= 0;
      x ^= x >>> 17;
      x ^= x << 5;
      x >>>= 0;
      return x / 2 ** 32;
    };
  }

  for (const seed of [7, 1234, 99991]) {
    it(`three installations, random edits on and off line (seed ${seed})`, async () => {
      const rnd = xorshift(seed);
      const pick = <T>(xs: T[]) => xs[Math.floor(rnd() * xs.length)]!;
      const name = ns();
      const fss = [fs, fs2, fs3];
      const dbs = await Promise.all(fss.map((f) => open(f, name, { crdtCacheMs: 200 })));
      const nets = fss.map((f) => net(f.session));
      const ids = ['p0', 'p1', 'p2'];
      for (const id of ids) await dbs[0]!.table<Card>('cards', { wait: true }).insert({ id });
      const votes = new Map(ids.map((id) => [id, 0]));
      const added = new Map(ids.map((id) => [id, new Set<string>()]));
      const removed = new Map(ids.map((id) => [id, new Set<string>()]));
      for (let step = 0; step < 80; step++) {
        const d = Math.floor(rnd() * 3);
        const cards = dbs[d]!.table<Card>('cards');
        const id = pick(ids);
        const r = rnd();
        if (r < 0.12) {
          nets[d]!.offline = !nets[d]!.offline;
        } else if (r < 0.3) {
          await cards.patch(id, { [pick(['title', 'status'])]: `v${step}` } as Partial<Card>);
        } else if (r < 0.5) {
          const k = Math.floor(rnd() * 5) - 1;
          votes.set(id, votes.get(id)! + k);
          await cards.incr(id, 'votes', k);
        } else if (r < 0.7) {
          const e = pick(['a', 'b', 'c', 'd']);
          added.get(id)!.add(e);
          await cards.add(id, 'labels', e);
        } else if (r < 0.82) {
          const e = pick(['a', 'b', 'c', 'd']);
          removed.get(id)!.add(e);
          await cards.remove(id, 'labels', e);
        } else if (r < 0.9) {
          await cards.delete(id);
        } else if (r < 0.95) {
          await cards.insert({ id });
        } else {
          // A transaction over two rows: one entry.
          await dbs[d]!.transaction(async (tx) => {
            await tx.table<Card>('cards').patch('p0', { status: `tx${step}` });
            await tx.table<Card>('cards').incr('p1', 'votes', 1);
          });
          votes.set('p1', votes.get('p1')! + 1);
        }
        if (rnd() < 0.2) await sleep(Math.floor(rnd() * 50));
      }
      for (const nt of nets) nt.offline = false;
      await Promise.all(dbs.map((db) => db.crdt.flush()));
      // Every row exists at the end, so every view can be compared.
      for (const id of ids) await dbs[0]!.table<Card>('cards', { wait: true }).insert({ id });
      for (const id of ids) {
        const views = await Promise.all(
          dbs.map((db) => db.table<Card>('cards').get(id, { read: 'server' })),
        );
        expect(views[1]).toEqual(views[0]);
        expect(views[2]).toEqual(views[0]);
        expect(views[0]!.votes).toBe(votes.get(id));
        const labels = new Set(views[0]!.labels);
        for (const e of added.get(id)!) if (!removed.get(id)!.has(e)) expect(labels).toContain(e);
      }
      for (const nt of nets) nt.restore();
      for (const db of dbs) db.close();
    }, 60_000);
  }
});
