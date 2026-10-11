// The queue of CRDT writes on the sealed local store (zendb.md §9.3,
// §19.7): it survives restarts, sends the commit in flight again with the
// same `commit_id`, is sent by the owner tab only, and keeps one
// installation's counters right across tabs. Offline is a wrapped
// `session.call` that fails with `network`.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  bytes,
  indexedDbLocalStore,
  memoryClockStore,
  memoryLocalStore,
  type SealedStoreOptions,
  type Session,
  sealedStore,
  TreeClock,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import type { Commit } from '@zen/wasm';
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import {
  Db,
  type DbOptions,
  type Fields,
  type LockManagerLike,
  localQueueStore,
} from '../src/index.js';
import type { BroadcastChannelLike } from '../src/proxy.js';

let w: World;
let fs: UnlockedFs;
let fs2: UnlockedFs;
let other: UnlockedFs;
let dir: string;
let n = 0;
const ns = () => `persist${++n}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  fs = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  fs2 = await tab();
  other = await w.session.fs(2).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  dir = await mkdtemp(join(tmpdir(), 'zen-persist-'));
});
afterAll(async () => {
  w?.server.stop();
  await rm(dir, { recursive: true, force: true });
});

/** Another session of this device on fs 1: a tab. With `clock`, on that shared clock store. */
async function tab(clock?: ReturnType<typeof memoryClockStore>): Promise<UnlockedFs> {
  const s = await w.client.signInDevice(w.admin);
  if (clock) s.useClock(new TreeClock({ store: clock }));
  return s.fs(1).unlock({ device: w.admin.device });
}

/** A session's network: commits sent, and switches to go offline or lose replies. */
interface Net {
  offline: boolean;
  commits: Commit[];
  codes: string[];
  loseReplies: number;
  restore(): void;
}

function net(s: Session): Net {
  const orig = s.call.bind(s);
  const n: Net = {
    offline: false,
    commits: [],
    codes: [],
    loseReplies: 0,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.offline) throw new ZenError(0, 'network', 'offline (test)');
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

/** Web Locks in memory: one exclusive queue per name. */
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

  open = (name: string): BroadcastChannelLike => {
    const set = this.chans.get(name) ?? new Set();
    this.chans.set(name, set);
    const ch: BroadcastChannelLike = {
      onmessage: null,
      postMessage(m: unknown) {
        const data = structuredClone(m);
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

interface Card extends Fields {
  id: string;
  title?: string;
  votes?: number;
  labels?: string[];
}

const migrations: DbOptions['migrations'] = {
  1: async (m) => {
    await m.createTable('cards', {
      pk: ['id'],
      merge: 'crdt',
      fields: { votes: 'counter', labels: 'set' },
    });
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

/** What the server holds, read by a fresh instance with no queue. */
async function serverRow(name: string, id: string): Promise<Card | undefined> {
  const d = await open(fs2, name, { crdtQueue: false });
  try {
    return await d.table<Card>('cards').get(id, { read: 'server' });
  } finally {
    d.close();
  }
}

const until = async (f: () => boolean, ms = 5000) => {
  const end = Date.now() + ms;
  while (!f()) {
    if (Date.now() > end) throw new Error('timed out');
    await sleep(5);
  }
};

describe('a queue that survives restarts (§19.7)', () => {
  it('comes back after a restart, shows in reads, and is sent once online', async () => {
    const name = ns();
    const disk = join(dir, name);
    const a = await open(fs, name, { local: sealedStore(nodeDiskLocalStore(disk)) });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1', title: 'one' });
    const na = net(fs.session);
    na.offline = true;
    const cards = a.table<Card>('cards');
    await cards.patch('r1', { title: 'two' });
    await cards.incr('r1', 'votes', 3);
    await cards.add('r1', 'labels', 'red');
    await cards.insert({ id: 'r2', title: 'new' });
    expect(a.crdt.pending).toBe(4);
    a.close();
    na.restore();

    // A new process: the same disk, a long batch delay so nothing goes yet.
    const b = await open(fs, name, {
      local: sealedStore(nodeDiskLocalStore(disk)),
      crdtBatchMs: 60_000,
    });
    expect(b.crdt.pending).toBe(4);
    const view = b.table<Card>('cards');
    expect(await view.get('r1', { read: 'cache' })).toMatchObject({
      title: 'two',
      votes: 3,
      labels: ['red'],
    });
    expect(await view.get('r2', { read: 'cache' })).toMatchObject({ title: 'new' });
    await b.crdt.flush();
    expect(b.crdt.pending).toBe(0);
    expect(await serverRow(name, 'r1')).toMatchObject({ title: 'two', votes: 3, labels: ['red'] });
    expect(await serverRow(name, 'r2')).toMatchObject({ title: 'new' });
    b.close();
    // Nothing is left to send.
    const c = await open(fs, name, { local: sealedStore(nodeDiskLocalStore(disk)) });
    expect(c.crdt.pending).toBe(0);
    c.close();
  });

  it('a crash after a lost reply sends the same commit again: applied once', async () => {
    const name = ns();
    const disk = join(dir, name);
    const local = () => sealedStore(nodeDiskLocalStore(disk));
    const a = await open(fs, name, { local: local() });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1' });
    const na = net(fs.session);
    na.loseReplies = 1;
    await a.table<Card>('cards').incr('r1', 'votes', 5);
    // The commit applied; its reply was lost. Then the process dies.
    await until(() => na.codes.includes('network'));
    na.offline = true;
    a.close();
    na.restore();
    expect(na.commits.length).toBe(1);

    const nb = net(fs.session);
    const b = await open(fs, name, { local: local() });
    expect(b.crdt.pending).toBe(1);
    await b.crdt.flush();
    nb.restore();
    // Sent again as it was: the same commit id, and it counted once.
    expect(nb.commits.length).toBe(1);
    expect(bytes.hex(nb.commits[0]!.commit_id)).toBe(bytes.hex(na.commits[0]!.commit_id));
    expect((await serverRow(name, 'r1'))?.votes).toBe(5);
    // The installation's later increments build on the right total.
    await b.table<Card>('cards', { wait: true }).incr('r1', 'votes', 1);
    expect((await serverRow(name, 'r1'))?.votes).toBe(6);
    b.close();
  });

  it('a crash before any send: the entries are sent at the next start', async () => {
    const name = ns();
    const local = sealedStore(memoryLocalStore());
    const a = await open(fs, name, { local });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1' });
    const na = net(fs.session);
    na.offline = true;
    await a.table<Card>('cards').incr('r1', 'votes', 2);
    await a.table<Card>('cards').incr('r1', 'votes', 2);
    a.close();
    na.restore();
    const b = await open(fs, name, { local });
    await b.crdt.flush();
    expect((await serverRow(name, 'r1'))?.votes).toBe(4);
    b.close();
  });

  const layerSets: [string, () => SealedStoreOptions][] = [
    ['fs layer only', () => ({ device: false, fs })],
    ['device and fs layers', () => ({ fs })],
    ['plaintext', () => ({ device: false, plaintext: true })],
  ];

  it.each(layerSets)('round-trips with the %s', async (_, opts) => {
    const name = ns();
    const raw = memoryLocalStore();
    const a = await open(fs, name, { local: sealedStore(raw, opts()) });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1' });
    const na = net(fs.session);
    na.offline = true;
    await a.table<Card>('cards').patch('r1', { title: 'kept' });
    a.close();
    na.restore();
    const b = await open(fs, name, { local: sealedStore(raw, opts()) });
    expect(b.crdt.pending).toBe(1);
    await b.crdt.flush();
    expect((await serverRow(name, 'r1'))?.title).toBe('kept');
    b.close();
  });

  it('a record sealed for another fs is dropped with a warning', async () => {
    const raw = memoryLocalStore();
    const mine = localQueueStore(sealedStore(raw, { device: false, fs }), 'x');
    await mine.put({ id: 'e1', single: true, rows: [], at: 1 });
    expect((await mine.load()).length).toBe(1);
    const warned: string[] = [];
    const theirs = localQueueStore(
      sealedStore(raw, { device: false, fs: other, warn: (m) => warned.push(m) }),
      'x',
    );
    expect(await theirs.load()).toEqual([]);
    expect(warned.length).toBe(1);
    // Dropped: not there for the right fs either.
    expect(await mine.load()).toEqual([]);
  });

  it('Db.wipe leaves nothing of the database', async () => {
    const name = ns();
    const raw = memoryLocalStore();
    const local = sealedStore(raw);
    const a = await open(fs, name, { local });
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1' });
    const na = net(fs.session);
    na.offline = true;
    await a.table<Card>('cards').incr('r1', 'votes', 1);
    a.close();
    na.restore();
    await Db.wipe(local, name);
    for await (const _ of raw.list(`queue/${name}`)) throw new Error('left behind');
    const b = await open(fs, name, { local });
    expect(b.crdt.pending).toBe(0);
    b.close();
  });
});

describe('tabs sharing a queue (§9.2, §19.7)', () => {
  it('only the owner sends; the other tab hands over when it closes', async () => {
    const name = ns();
    const locks = new FakeLocks();
    const bus = new FakeBus();
    const factory = new IDBFactory();
    const local = () => sealedStore(indexedDbLocalStore('app', { factory, keyRange: IDBKeyRange }));
    const opts = { locks, channel: bus.open };
    const a = await open(fs, name, { ...opts, local: local() });
    await until(() => a.isOwner);
    const tabB = await tab();
    const b = await open(tabB, name, { ...opts, local: local() });
    expect(b.isOwner).toBe(false);
    await a.table<Card>('cards', { wait: true }).insert({ id: 'r1' });
    const na = net(fs.session);
    const nb = net(tabB.session);

    // B queues; A sends; B's write resolves when A has sent it.
    await b.table<Card>('cards', { wait: true }).incr('r1', 'votes', 2);
    await b.table<Card>('cards').add('r1', 'labels', 'blue');
    await b.crdt.flush();
    expect(nb.commits.length).toBe(0);
    expect(na.commits.length).toBeGreaterThan(0);
    expect(await serverRow(name, 'r1')).toMatchObject({ votes: 2, labels: ['blue'] });

    // A closes with B's write queued (offline): B becomes the owner and sends it.
    na.offline = true;
    await b.table<Card>('cards').patch('r1', { title: 'from b' });
    await sleep(50);
    const handed = b.ownerChange;
    a.close();
    na.restore();
    expect(await handed).toBe(true);
    await b.crdt.flush();
    expect(nb.commits.length).toBeGreaterThan(0);
    expect((await serverRow(name, 'r1'))?.title).toBe('from b');
    nb.restore();
    b.close();
  });

  it('two tabs of one installation increment one counter at once: the total is the sum', async () => {
    const name = ns();
    const clock = memoryClockStore();
    const locks = new FakeLocks();
    const [t1, t2] = [await tab(clock), await tab(clock)];
    expect(bytes.hex(t1.session.clock.actor)).toBe(bytes.hex(t2.session.clock.actor));
    const opts = { locks, channel: null, crdtQueue: false };
    const a = await open(t1, name, opts);
    const b = await open(t2, name, opts);
    await a.table<Card>('cards').insert({ id: 'r1' });
    const incr = (d: Db) =>
      Array.from({ length: 8 }, () => d.table<Card>('cards').incr('r1', 'votes', 1));
    await Promise.all([...incr(a), ...incr(b)]);
    expect((await serverRow(name, 'r1'))?.votes).toBe(16);
    a.close();
    b.close();
  });
});
