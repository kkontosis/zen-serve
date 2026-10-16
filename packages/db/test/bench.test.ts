// Measured costs of @zen/db against a spawned zen-serve (docs/STATS.md,
// "zen-db costs (measured)"). Opt-in, never in CI:
//
//   ZEN_BENCH=1 npx vitest run packages/db/test/bench.test.ts
//
// Each line is the median of its runs; commit ops and bytes are read from
// the `/v1/commit` requests the step sent (ops: KV writes, range clears,
// appends, consume steps, CRDT ops, chunks and pages).
import { readFile, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';
import { type Session, type UnlockedFs, zw } from '@zen/client';
import type { Commit } from '@zen/wasm';
import { afterAll, beforeAll, describe, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { type CreateIndexOptions, Db, type DbOptions, type Fields } from '../src/index.js';

const ROOT = join(import.meta.dirname, '../../..');
const quiet = { warn: () => {} };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `bench${++n}`;
const lines: string[] = [];

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(async () => {
  console.log(
    ['', '| What | ms (median) | commit ops | commit bytes |', '|---|---|---|---|', ...lines].join(
      '\n',
    ),
  );
  await w?.server.stop();
});

/** Record the commits a session sends while `fn` runs. */
async function commitsDuring<R>(
  s: Session,
  fn: () => Promise<R>,
): Promise<{ out: R; ops: number; bytes: number }> {
  const call = s.call;
  let ops = 0;
  let size = 0;
  s.call = ((path: string, enc: (r: unknown) => Uint8Array, dec: unknown, req: unknown) => {
    if (path === '/v1/commit') {
      const c = req as Commit;
      size += enc(req).length;
      ops +=
        (c.writes?.length ?? 0) +
        (c.clear_ranges?.length ?? 0) +
        (c.append?.length ?? 0) +
        (c.consume?.length ?? 0) +
        (c.crdt_ops?.length ?? 0) +
        (c.chunks?.length ?? 0) +
        (c.pages?.length ?? 0);
    }
    return (call as (...a: unknown[]) => unknown).call(s, path, enc, dec, req);
  }) as typeof s.call;
  try {
    return { out: await fn(), ops, bytes: size };
  } finally {
    s.call = call;
  }
}

const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)]!;
};
const fmt = (x: number) => (x >= 100 ? x.toFixed(0) : x.toFixed(1));

/** Run `fn(i)` `runs` times; record the median time and the median commit cost. */
async function measure(what: string, runs: number, fn: (i: number) => Promise<unknown>) {
  const ms: number[] = [];
  const ops: number[] = [];
  const size: number[] = [];
  for (let i = 0; i < runs; i++) {
    const t0 = performance.now();
    const r = await commitsDuring(fs.session, () => fn(i));
    ms.push(performance.now() - t0);
    ops.push(r.ops);
    size.push(r.bytes);
  }
  const o = median(ops);
  lines.push(`| ${what} | ${fmt(median(ms))} | ${o || '–'} | ${o ? median(size) : '–'} |`);
}

/** Record a set of durations measured some other way. */
function record(what: string, ms: number[], note = '') {
  lines.push(`| ${what} | ${fmt(median(ms))}${note} | – | – |`);
}

interface Row extends Fields {
  id: string;
  v?: number;
  blob?: string;
}

function open(extra: DbOptions & { indexes?: Record<string, Partial<CreateIndexOptions>> } = {}) {
  const { indexes = {}, ...opts } = extra;
  return Db.open(fs, ns(), {
    ...quiet,
    locks: null,
    schema: 1,
    migrations: {
      1: async (m) => {
        await m.createTable('r', { pk: ['id'] });
        await m.createTable('padded', { pk: ['id'], pad: true });
        for (const [name, def] of Object.entries(indexes)) {
          await m.createIndex('r', name, { fields: [['v', 'int']], ...def } as CreateIndexOptions);
        }
      },
    },
    ...opts,
  });
}

const preload = (db: Db, k: number) =>
  db.importRows<Row>(
    'r',
    Array.from({ length: k }, (_, i) => ({ id: `p${i}`, v: i * 2 })),
  );

describe.skipIf(!process.env.ZEN_BENCH)('zen-db costs', () => {
  it('rows', async () => {
    const db = await open({ scheduler: false });
    const t = db.table<Row>('r');
    await measure('row insert (small)', 20, (i) => t.insert({ id: `a${i}`, v: i }));
    await measure('row get, cached', 20, (i) => t.get(`a${i}`));
    await measure('row update', 20, (i) => t.update(`a${i}`, (r) => ({ ...r, v: -i })));
    const cold = await Db.open(fs, db.ns, {
      ...quiet,
      locks: null,
      scheduler: false,
      rowCache: 0,
      schema: 1,
    });
    await measure('row get, not cached', 20, (i) => cold.table<Row>('r').get(`a${i}`));
    const pad = db.table<Row>('padded');
    await measure('padded row insert', 20, (i) => pad.insert({ id: `p${i}`, v: i }));
    const big = 'x'.repeat(200_000);
    await measure('row insert, 200 KB (parts)', 10, (i) => t.insert({ id: `b${i}`, blob: big }));
    await measure('row get, 200 KB, not cached', 10, (i) => cold.table<Row>('r').get(`b${i}`));
    await cold.close();
    await db.close();
  });

  it('one index write per kind, at 1,000 rows', async () => {
    const kinds: Record<string, Partial<CreateIndexOptions>> = {
      unique: { kind: 'none', unique: true },
      fast: { kind: 'fast' },
      'private (fanout 64, 1 shard)': { kind: 'private' },
      'private (4 shards)': { kind: 'private', shards: 4 },
      'private (2 decoys)': { kind: 'private', decoys: 2 },
      sealed: { kind: 'sealed' },
    };
    const base = await open({ scheduler: false });
    await preload(base, 1000);
    await measure('insert, no index', 20, (i) =>
      base.table<Row>('r').insert({ id: `n${i}`, v: 2 * i + 1 }),
    );
    await base.close();
    for (const [label, def] of Object.entries(kinds)) {
      const db = await open({ scheduler: false, indexes: { ix: def } });
      await preload(db, 1000);
      await measure(`insert, ${label} index`, 20, (i) =>
        db.table<Row>('r').insert({ id: `n${i}`, v: 2 * i + 1 }),
      );
      await db.close();
    }
  });

  it('queries at 1,000 rows', async () => {
    const db = await open({
      scheduler: false,
      indexes: {
        u: { kind: 'none', unique: true },
        f: { kind: 'fast' },
        p: { kind: 'private' },
      },
    });
    await preload(db, 1000);
    const q = () => db.table<Row>('r').query();
    await measure('get by pk', 20, (i) => db.table<Row>('r').get(`p${i * 7}`));
    await measure('unique equality', 20, (i) =>
      q()
        .where('v', '=', i * 14)
        .page(),
    );
    await measure('private range, a page of 20', 20, (i) =>
      q()
        .where('v', '>=', i * 40)
        .orderBy('v')
        .limit(20)
        .page(),
    );
    await measure('scan of 1,000 rows with a filter (no index)', 5, () =>
      q().where('blob', '=', 'none').limit(1000).page(),
    );
    await db.close();
    const sealed = await open({ scheduler: false, indexes: { s: { kind: 'sealed' } } });
    await preload(sealed, 1000);
    await measure('sealed range, a page of 20', 20, (i) =>
      sealed
        .table<Row>('r')
        .query()
        .where('v', '>=', i * 40)
        .orderBy('v')
        .limit(20)
        .page(),
    );
    await sealed.close();
    const fast = await open({ scheduler: false, indexes: { f: { kind: 'fast' } } });
    await preload(fast, 1000);
    await measure('fast equality', 20, (i) =>
      fast
        .table<Row>('r')
        .query()
        .where('v', '=', i * 14)
        .page(),
    );
    await fast.close();
  });

  it('the broker', async () => {
    const db = await open({ schedulerOptions: { pollMs: 1000 } });
    const t = db.topic('bench', db.ns);
    // emit → on
    const lat: number[] = [];
    const sub = await db.on<number>(t, (m) => void lat.push(Date.now() - m.body));
    await measure('emit (its own commit)', 20, () => db.emit(t, 'n', Date.now()));
    await until(() => lat.length === 20);
    record('emit → on, latency', lat);
    await sub.close();

    // consume throughput
    for (const mode of ['sequential', 'per_key'] as const) {
      const ct = db.topic('consume', mode, db.ns);
      for (let b = 0; b < 4; b++) {
        await db.transaction(async (tx) => {
          for (let i = 0; i < 50; i++) tx.emit(ct, 'n', i, { key: `k${i % 8}` });
        });
      }
      let done = 0;
      const t0 = performance.now();
      const h = db.consume(
        { group: 'g', topic: ct, mode, maxInflight: 8, start: 'earliest' },
        async (_m, tx) => {
          await tx.table('r').insert({ id: `${mode}${done++}` });
        },
      );
      await until(() => done >= 200, 120_000);
      const s = (performance.now() - t0) / 1000;
      lines.push(
        `| consume, ${mode}: 200 messages | ${fmt((s * 1000) / 200)} per message (${fmt(200 / s)}/s) | – | – |`,
      );
      await h.stop();
    }

    // request/reply
    const rt = db.topic('rr', db.ns);
    const h = db.serve<number>(
      { group: 'rr', topic: rt, mode: 'sequential' },
      async (m) => m.body + 1,
    );
    await db.request(rt, 'inc', 0, { timeoutMs: 30_000 });
    await measure('request → reply (event)', 20, (i) => db.request(rt, 'inc', i));
    await h.stop();

    // a schedule's lateness
    const st = db.topic('sched', db.ns);
    const late: number[] = [];
    const s2 = await db.on<number>(st, (m) => void late.push(Date.now() - m.body));
    for (let i = 0; i < 10; i++) {
      const at = Date.now() + 300;
      await db.schedule(st, 'due', at, { at });
      await sleep(350);
    }
    await until(() => late.length === 10);
    record('schedule: delivered after its `at`', late);
    await s2.close();
    await db.close();
  });

  it('CRDT rows', async () => {
    const crdtDb = (extra: DbOptions = {}) =>
      Db.open(fs, name, {
        ...quiet,
        locks: null,
        schema: 1,
        migrations: {
          1: async (m) => {
            await m.createTable('c', {
              pk: ['id'],
              merge: 'crdt',
              fields: { n: 'counter', tags: 'set' },
              changes: {},
            });
            await m.createIndex('c', 'by_v', { fields: [['v', 'int']], kind: 'fast' });
          },
        },
        ...extra,
      });
    const name = ns();
    const db = await crdtDb();
    const t = db.table('c');
    const tw = db.table('c', { wait: true });
    // A CRDT row exists once inserted (a patch alone writes no row register).
    await measure('CRDT insert, waiting for its commit', 20, (i) =>
      tw.insert({ id: `w${i}`, v: i }),
    );
    await db.transaction(async (tx) => {
      for (let i = 0; i < 20; i++) await tx.table('c').insert({ id: `q${i}`, v: i });
    });
    await db.crdt.flush();
    await measure('CRDT patch, queued (resolves once queued)', 20, (i) =>
      t.patch(`q${i}`, { v: i }),
    );
    await db.crdt.flush();
    await measure('CRDT patch, waiting for its commit', 20, (i) => tw.patch(`w${i}`, { v: i }));
    await measure('CRDT incr, waiting', 20, (i) => tw.incr(`w${i}`, 'n', 1));
    await measure('CRDT add, waiting', 20, (i) => tw.add(`w${i}`, 'tags', 'a', 'b'));
    await measure('CRDT remove, waiting', 20, (i) => tw.remove(`w${i}`, 'tags', 'a'));
    await measure('CRDT get, cache hit', 20, (i) => t.get(`w${i}`));
    const checked = await crdtDb({ crdtCacheMs: 0, indexer: false });
    for (let i = 0; i < 20; i++) await checked.table('c').get(`w${i}`);
    await measure('CRDT get, expired (a version check)', 20, (i) =>
      checked.table('c').get(`w${i}`),
    );
    await measure('CRDT get, not cached', 20, (i) => checked.table('c').get(`q${i}`));
    await checked.close();
    // indexer lag: a patch until a query by the new value finds it
    const lag: number[] = [];
    for (let i = 0; i < 10; i++) {
      const v = 10_000 + i;
      const t0 = performance.now();
      await tw.patch(`w${i}`, { v });
      await until(async () => (await t.query().where('v', '=', v).page()).rows.length === 1);
      lag.push(performance.now() - t0);
    }
    record('CRDT indexer: a patch until an index query finds it', lag);
    await db.close();
  });

  it('sizes', async () => {
    const files = [
      ['zen_wasm_bg.wasm', join(ROOT, 'packages/zen-wasm/pkg/zen_wasm_bg.wasm')],
      ['loro_wasm_bg.wasm', join(ROOT, 'node_modules/loro-crdt/browser/loro_wasm_bg.wasm')],
      ['apps/dist/kanban/main.js', join(ROOT, 'apps/dist/kanban/main.js')],
    ];
    for (const [label, path] of files) {
      try {
        const size = (await stat(path!)).size;
        const gz = gzipSync(await readFile(path!)).length;
        lines.push(`| size: ${label} | ${size} bytes (${gz} gzipped) | – | – |`);
      } catch {
        lines.push(`| size: ${label} | not built | – | – |`);
      }
    }
  });
});

async function until(cond: () => boolean | Promise<boolean>, ms = 30_000): Promise<void> {
  const end = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(5);
  }
}
