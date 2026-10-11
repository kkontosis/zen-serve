// The tab proxy against a spawned zen-serve (zendb.md §9.2): tabs are `Db`s
// on one fs and namespace, with fake Web Locks and a fake BroadcastChannel.
// Only the owner holds a stream; the others subscribe through it.
import { bytes, type UnlockedFs, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import {
  type BroadcastChannelLike,
  Db,
  type DbOptions,
  DirectSource,
  type LockManagerLike,
  type Message,
  TabSource,
} from '../src/index.js';

let w: World;
let fs: UnlockedFs;
let n = 0;
const ns = () => `px${++n}`;
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
    await sleep(20);
  }
}

/** In-memory Web Locks: one exclusive queue per name. */
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

/** In-memory BroadcastChannels: async, structured-cloned, never to the sender. */
class FakeBus {
  private readonly chans = new Map<string, Set<BroadcastChannelLike>>();
  /** Messages posted, by type. */
  readonly sent = new Map<string, number>();

  open = (name: string): BroadcastChannelLike => {
    const set = this.chans.get(name) ?? new Set();
    this.chans.set(name, set);
    const bus = this;
    const ch: BroadcastChannelLike = {
      onmessage: null,
      postMessage(m: unknown) {
        const data = structuredClone(m);
        const t = (data as { t: string }).t;
        bus.sent.set(t, (bus.sent.get(t) ?? 0) + 1);
        for (const c of set) {
          if (c === ch) continue;
          setTimeout(() => set.has(c) && c.onmessage?.({ data }), 0);
        }
      },
      close() {
        set.delete(ch);
      },
    };
    set.add(ch);
    return ch;
  };
}

/** Tabs of one database: shared locks and bus. */
function tabs() {
  const locks = new FakeLocks();
  const bus = new FakeBus();
  const name = ns();
  const open = (extra: DbOptions = {}) =>
    Db.open(fs, name, {
      ...quiet,
      locks,
      channel: bus.open,
      scheduler: false,
      schema: 1,
      migrations: { 1: (m) => m.createTable('log', { pk: ['id'] }) },
      ...extra,
    });
  return { name, bus, open };
}

const streaming = (db: Db) => (db.source as TabSource).streaming;

describe('the tab proxy (§9.2)', () => {
  it('only the owner streams; other tabs’ `on` (plain and cursor) goes through it', async () => {
    const { open } = tabs();
    const a = await open();
    await until(() => a.isOwner);
    const b = await open();
    expect(b.isOwner).toBe(false);
    expect(b.source).toBeInstanceOf(TabSource);
    const t = a.topic('feed');
    const plain: unknown[] = [];
    await b.on(t, (m) => {
      plain.push(m.body);
    });
    await b.on(
      t,
      async (m, tx) => {
        await tx.table('log').insert({ id: String(m.body) });
      },
      { cursor: 'c' },
    );
    for (let i = 0; i < 5; i++) await a.emit(t, 'n', i);
    await until(() => plain.length === 5);
    expect(plain).toEqual([0, 1, 2, 3, 4]);
    await until(async () => (await b.table('log').query().page()).rows.length === 5);
    expect(streaming(a)).toBe(true);
    expect(streaming(b)).toBe(false);
    a.close();
    b.close();
  });

  it('a non-owner’s request, ephemeral subscription and publish', async () => {
    const { open } = tabs();
    const a = await open();
    await until(() => a.isOwner);
    const b = await open();
    const t = a.topic('calc');
    a.serve<{ x: number }>({ group: 'calc', topic: t, mode: 'sequential' }, (m) => m.body.x * 2);
    const r = await b.request(t, 'double', { x: 21 }, { timeoutMs: 8000 });
    expect(r.body).toBe(42);

    const e = a.topic('presence');
    const got: unknown[] = [];
    await b.onEphemeral(e, (m) => {
      got.push(m.body);
    });
    // Another device's stream (no proxy) publishes and listens.
    const x = await Db.open(fs, `${ns()}x`, { ...quiet, locks: null, scheduler: false });
    const xe = x.fs.topic(...e.path);
    const xs: unknown[] = [];
    await x.onEphemeral(xe, (m) => {
      xs.push(m.body);
    });
    await until(async () => {
      await x.publishEphemeral(xe, 'hi', 'from x');
      return got.includes('from x');
    });
    await until(async () => {
      await b.publishEphemeral(e, 'hi', 'from b');
      return xs.includes('from b');
    });
    expect(streaming(b)).toBe(false);
    a.close();
    b.close();
    x.close();
  });

  it('handover: no gap or duplicate in any tab, and consumers move', async () => {
    const { open } = tabs();
    const a = await open();
    await until(() => a.isOwner);
    const b = await open();
    const c = await open();
    const t = a.topic('ticks');
    const seen = new Map<Db, { offset: string; body: unknown }[]>([
      [b, []],
      [c, []],
    ]);
    for (const db of [b, c]) {
      await db.on(t, (m: Message) => {
        seen.get(db)!.push({ offset: bytes.hex(m.offset), body: m.body });
      });
    }
    // b's consumer can only run once it owns.
    const jobs = a.topic('jobs');
    const done: unknown[] = [];
    const h = b.consume({ group: 'j', topic: jobs, mode: 'sequential' }, (m) => {
      done.push(m.body);
    });
    expect(h.running).toBe(false);
    // A writer outside the tabs.
    const x = await Db.open(fs, `${ns()}x`, { ...quiet, locks: null, scheduler: false });
    const xt = x.fs.topic(...t.path);
    const writing = (async () => {
      for (let i = 0; i < 30; i++) {
        await x.emit(xt, 'tick', i);
        await sleep(15);
      }
    })();
    await until(() => seen.get(b)!.length >= 8);
    const moved = b.ownerChange;
    a.close();
    expect(await moved).toBe(true);
    await writing;
    for (const db of [b, c]) {
      await until(() => seen.get(db)!.length >= 30);
      await sleep(200);
      const s = seen.get(db)!;
      expect(s.map((e) => e.body)).toEqual(Array.from({ length: 30 }, (_, i) => i));
      expect(new Set(s.map((e) => e.offset)).size).toBe(30);
    }
    expect(streaming(b)).toBe(true);
    expect(streaming(c)).toBe(false);
    await x.emit(x.fs.topic(...jobs.path), 'job', 'j1');
    await until(() => done.length === 1);
    expect(h.running).toBe(true);
    b.close();
    c.close();
    x.close();
  });

  it('without a channel, or without locks, every instance streams itself', async () => {
    const { name } = tabs();
    const a = await Db.open(fs, name, { ...quiet, locks: new FakeLocks(), channel: null });
    expect(a.source).toBeInstanceOf(DirectSource);
    const bus = new FakeBus();
    const b = await Db.open(fs, name, { ...quiet, locks: null, channel: bus.open });
    expect(b.source).toBeInstanceOf(DirectSource);
    expect(bus.sent.size).toBe(0);
    a.close();
    b.close();
  });
});
