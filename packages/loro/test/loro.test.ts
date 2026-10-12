// Loro documents on zen-db topics (zendb.md §20): two installations
// converge, a load is the snapshot plus the tail, edits survive offline
// and restarts, resends are harmless, and only owners compact. Offline is
// a wrapped `session.call` that fails with `network`.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  bytes,
  type SealedStore,
  type Session,
  sealedStore,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { Db, type DbOptions, type LockManagerLike } from '@zen/db';
import type { Commit } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import type { BroadcastChannelLike } from '../../db/src/proxy.js';
import { type LoroHandle, openLoro } from '../src/index.js';

let w: World;
let dir: string;
let n = 0;
const ns = () => `loro${++n}`;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

beforeAll(async () => {
  w = await world({ limits: { claim_ttl_ms: 1000 } });
  await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  dir = await mkdtemp(join(tmpdir(), 'zen-loro-'));
});
afterAll(async () => {
  w?.server.stop();
  await rm(dir, { recursive: true, force: true });
});

/** An installation: a session of its own (its own clock) on fs 1. */
async function installation(): Promise<{ s: Session; fs: UnlockedFs }> {
  const s = await w.client.signInDevice(w.admin);
  return { s, fs: await s.fs(1).unlock({ device: w.admin.device }) };
}

const open = (fs: UnlockedFs, name: string, extra: DbOptions = {}) =>
  Db.open(fs, name, { warn: () => {}, locks: null, scheduler: false, indexer: false, ...extra });

/** A session's network: commits and log reads, and a switch to go offline or lose replies. */
function net(s: Session) {
  const orig = s.call.bind(s);
  const n = {
    offline: false,
    loseReplies: 0,
    commits: [] as Commit[],
    /** Events returned by `/v1/log/read`. */
    readEvents: 0,
    rowReads: 0,
    restore: () => {
      s.call = orig;
    },
  };
  s.call = (async (path, enc, dec, req) => {
    if (n.offline) throw new ZenError(0, 'network', 'offline (test)');
    if (path === '/v1/commit') n.commits.push(req as Commit);
    const r = await orig(path, enc, dec, req);
    if (path === '/v1/log/read') n.readEvents += (r as { events: unknown[] }).events.length;
    if (path === '/v1/kv/get') n.rowReads++;
    if (path === '/v1/commit' && n.loseReplies > 0) {
      n.loseReplies--;
      throw new ZenError(0, 'network', 'reply lost (test)');
    }
    return r;
  }) as Session['call'];
  return n;
}

const until = async (f: () => boolean | Promise<boolean>, ms = 10_000) => {
  const end = Date.now() + ms;
  while (!(await f())) {
    if (Date.now() > end) throw new Error('timed out');
    await sleep(10);
  }
};

const text = (h: LoroHandle) => h.doc.getText('t').toString();

function type(h: LoroHandle, s: string, at = text(h).length): void {
  h.doc.getText('t').insert(at, s);
  h.doc.commit();
}

/** The events on a document's topic, read fresh. */
async function updates(h: LoroHandle): Promise<number> {
  let k = 0;
  for await (const _ of h.topic.read()) k++;
  return k;
}

const snapRow = (db: Db, name: string) => db.table('$loro').get(name);

describe('documents', () => {
  it('two installations editing at once converge', async () => {
    const name = ns();
    const a = await installation();
    const b = await installation();
    const da = await open(a.fs, name);
    const db = await open(b.fs, name);
    const ha = await openLoro(da, 'doc');
    const hb = await openLoro(db, 'doc');
    await Promise.all([ha.ready, hb.ready]);
    expect(ha.online).toBe(true);
    let changed = 0;
    hb.onChange(() => changed++);
    for (let i = 0; i < 20; i++) {
      type(ha, 'a');
      type(hb, 'b', 0);
      ha.doc.getList('l').push(`a${i}`);
      hb.doc.getList('l').push(`b${i}`);
      ha.doc.commit();
      hb.doc.commit();
      if (i % 5 === 0) await sleep(20);
    }
    await Promise.all([ha.flush(), hb.flush()]);
    await until(() => JSON.stringify(ha.doc.toJSON()) === JSON.stringify(hb.doc.toJSON()));
    const json = ha.doc.toJSON() as { t: string; l: string[] };
    expect(json.t.length).toBe(40);
    expect(json.l.length).toBe(40);
    expect(changed).toBeGreaterThan(0);
    // Local updates gathered in one batch went out merged: fewer events than edits.
    expect(await updates(ha)).toBeLessThan(80);
    await ha.close();
    await hb.close();
    await da.close();
    await db.close();
  });

  it('a load is the snapshot and the tail only (§20.2, §20.5)', async () => {
    const name = ns();
    const a = await installation();
    const da = await open(a.fs, name);
    const ha = await openLoro(da, 'doc', { snapshotEvery: 50 });
    await ha.ready;
    for (let i = 0; i < 160; i++) {
      type(ha, `${i % 10}`);
      await ha.flush();
    }
    await until(() => !!ha.lastOffset && ha.pending === 0);
    // The writer compacted every 50 events, as it imported its own: the tail is 10.
    await until(async () => {
      const row = await snapRow(da, 'doc');
      return !!row && (await updates(ha)) === 160;
    });
    const row = (await snapRow(da, 'doc'))!;
    expect(row.snapshot).toBeInstanceOf(Uint8Array);

    const b = await installation();
    const nb = net(b.s);
    const db = await open(b.fs, name);
    const hb = await openLoro(db, 'doc');
    await hb.ready;
    expect(text(hb)).toBe(text(ha));
    expect(nb.readEvents).toBeGreaterThan(0);
    expect(nb.readEvents).toBeLessThan(50);
    await ha.close();
    await hb.close();
    await da.close();
    await db.close();
  });

  it('offline edits survive a restart and go once online (§20.3)', async () => {
    const name = ns();
    const a = await installation();
    const local = sealedStore(nodeDiskLocalStore(join(dir, name)));
    const na = net(a.s);
    let da = await open(a.fs, name, { local });
    let ha = await openLoro(da, 'doc');
    await ha.ready;
    type(ha, 'online ');
    await ha.flush();

    na.offline = true;
    type(ha, 'offline');
    expect(text(ha)).toBe('online offline');
    await expect(ha.flush()).rejects.toMatchObject({ code: 'offline' });
    expect(ha.pending).toBeGreaterThan(0);
    await ha.close();
    await da.close();

    // A new process, the server still unreachable: the kept state and the outbox.
    da = await open(a.fs, name, { local });
    expect(da.offline).toBe(true);
    ha = await openLoro(da, 'doc');
    await ha.ready;
    expect(ha.online).toBe(false);
    expect(text(ha)).toBe('online offline');
    expect(ha.pending).toBeGreaterThan(0);

    na.offline = false;
    await until(async () => {
      await ha.flush().catch(() => {});
      return ha.pending === 0;
    });
    const b = await installation();
    const db = await open(b.fs, name);
    const hb = await openLoro(db, 'doc');
    await hb.ready;
    expect(text(hb)).toBe('online offline');
    await ha.close();
    await hb.close();
    await da.close();
    await db.close();
  });

  it('a resend after a lost reply or a crash duplicates nothing (§20.3)', async () => {
    const name = ns();
    const a = await installation();
    const b = await installation();
    const local = sealedStore(nodeDiskLocalStore(join(dir, name)));
    const na = net(a.s);
    const db = await open(b.fs, name);
    const hb = await openLoro(db, 'doc');
    let da = await open(a.fs, name, { local });
    let ha = await openLoro(da, 'doc');
    await Promise.all([ha.ready, hb.ready]);

    // Lost replies: the client replays the commit (same commit_id, applied
    // once), then gives up; the sender emits the update again in a new commit.
    na.loseReplies = 4;
    type(ha, 'hello');
    await until(() => ha.pending === 0);
    expect(await updates(ha)).toBe(2);

    // A crash after the commit: the outbox still holds the update.
    na.loseReplies = 4;
    const sent = na.commits.length;
    type(ha, ' world');
    await until(() => na.commits.length >= sent + 4);
    na.offline = true;
    await ha.close();
    await da.close();
    na.offline = false;
    da = await open(a.fs, name, { local });
    ha = await openLoro(da, 'doc');
    await ha.flush();
    expect(await updates(ha)).toBe(4);
    await until(() => text(hb) === 'hello world');
    await sleep(100);
    expect(text(hb)).toBe('hello world');
    expect(text(ha)).toBe('hello world');
    await ha.close();
    await hb.close();
    await da.close();
    await db.close();
  });

  it('two owners compacting at once: the row moves forward only (§20.5)', async () => {
    const name = ns();
    const a = await installation();
    const b = await installation();
    const da = await open(a.fs, name);
    const db = await open(b.fs, name);
    const big = { snapshotEvery: 1e9, snapshotBytes: 1e12 };
    const ha = await openLoro(da, 'doc', big);
    const hb = await openLoro(db, 'doc', big);
    await Promise.all([ha.ready, hb.ready]);
    for (let i = 0; i < 30; i++) {
      type(i % 2 ? ha : hb, 'x');
      await sleep(5);
    }
    await Promise.all([ha.flush(), hb.flush()]);
    await until(() => text(ha).length === 30 && text(hb).length === 30);
    await Promise.all([ha.compact(), hb.compact(), ha.compact()]);
    const row = (await snapRow(da, 'doc'))!;
    const max = [ha.lastOffset!, hb.lastOffset!].sort(bytes.compare)[1]!;
    expect(bytes.equal(row.offset as Uint8Array, max)).toBe(true);

    const c = await installation();
    const dc = await open(c.fs, name);
    const hc = await openLoro(dc, 'doc');
    await hc.ready;
    expect(text(hc)).toBe(text(ha));
    for (const h of [ha, hb, hc]) await h.close();
    for (const d of [da, db, dc]) await d.close();
  });

  it('an explicit compaction writes the row; a fresh load reads no events', async () => {
    const name = ns();
    const a = await installation();
    const da = await open(a.fs, name);
    const ha = await openLoro(da, 'doc');
    await ha.ready;
    type(ha, 'compact me');
    await ha.flush();
    await until(() => !!ha.lastOffset);
    await ha.compact();
    const b = await installation();
    const nb = net(b.s);
    const db = await open(b.fs, name);
    const hb = await openLoro(db, 'doc');
    await hb.ready;
    expect(text(hb)).toBe('compact me');
    expect(nb.readEvents).toBe(0);
    await ha.close();
    await hb.close();
    await da.close();
    await db.close();
  });

  it('wipe clears the kept state and the outbox', async () => {
    const name = ns();
    const a = await installation();
    const local = sealedStore(nodeDiskLocalStore(join(dir, name)));
    const na = net(a.s);
    const da = await open(a.fs, name, { local });
    const ha = await openLoro(da, 'doc');
    await ha.ready;
    na.offline = true;
    type(ha, 'unsent');
    await ha.close();
    await da.close();
    expect(await count(local, `loro/${name}`)).toBeGreaterThan(1);
    await Db.wipe(local, name);
    expect(await count(local, `loro/${name}`)).toBe(0);
  });
});

async function count(local: SealedStore, store: string): Promise<number> {
  let k = 0;
  for await (const _ of local.list(store)) k++;
  return k;
}

describe('a snapshot too large for a commit (§20.5)', () => {
  let small: World;
  beforeAll(async () => {
    small = await world({ limits: { max_commit_bytes: 64 * 1024 } });
    await small.session
      .fs(1)
      .init((k) => [zw.createDeviceSlot(k, small.admin.device.devicePublic)]);
  });
  afterAll(() => small?.server.stop());

  it('keeps the old row and warns once', async () => {
    const s = await small.client.signInDevice(small.admin);
    const fs = await s.fs(1).unlock({ device: small.admin.device });
    const warns: string[] = [];
    const db = await Db.open(fs, 'big', {
      warn: (m) => warns.push(m),
      locks: null,
      scheduler: false,
      indexer: false,
    });
    const h = await openLoro(db, 'doc', { snapshotEvery: 1e9, snapshotBytes: 1e12 });
    await h.ready;
    type(h, 'small');
    await h.flush();
    await until(() => !!h.lastOffset);
    await h.compact();
    const first = (await db.table('$loro').get('doc'))!.offset as Uint8Array;
    // Random text doesn't compress: 100 KiB of it, sent in small updates.
    for (let i = 0; i < 100; i++) {
      type(h, bytes.hex(bytes.randomBytes(512)));
      await h.flush();
    }
    await until(() => !!h.lastOffset && bytes.compare(h.lastOffset, first) > 0);
    await expect(h.compact()).rejects.toMatchObject({ code: 'too_large' });
    const row = (await db.table('$loro').get('doc'))!;
    expect(bytes.equal(row.offset as Uint8Array, first)).toBe(true);
    expect(warns.filter((m) => m.includes('TD-LORO-LARGE-SNAPSHOT'))).toHaveLength(1);
    await h.close();
    await db.close();
  });
});

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

describe('tabs', () => {
  it('only the owner compacts; both tabs converge', async () => {
    const name = ns();
    const locks = new FakeLocks();
    const bus = new FakeBus();
    const a = await installation();
    const b = await installation();
    const na = net(a.s);
    const nb = net(b.s);
    const tabOpts = { locks, channel: bus.open } as DbOptions;
    const ta = await open(a.fs, name, tabOpts);
    await until(() => ta.isOwner);
    const tb = await open(b.fs, name, tabOpts);
    expect(tb.isOwner).toBe(false);
    const opts = { snapshotEvery: 5 };
    const ha = await openLoro(ta, 'doc', opts);
    const hb = await openLoro(tb, 'doc', opts);
    await Promise.all([ha.ready, hb.ready]);
    for (let i = 0; i < 20; i++) {
      type(i % 2 ? ha : hb, `${i % 10}`);
      await (i % 2 ? ha : hb).flush();
    }
    await until(() => text(ha).length === 20 && text(hb).length === 20);
    expect(text(ha)).toBe(text(hb));
    await until(async () => !!(await snapRow(ta, 'doc')));
    const wrote = (c: Commit[]) => c.filter((x) => x.writes?.length).length;
    expect(wrote(na.commits)).toBeGreaterThan(0);
    expect(wrote(nb.commits)).toBe(0);
    // Every commit of the other tab was an update's emit.
    expect(nb.commits.every((c) => c.append?.length)).toBe(true);
    await ha.close();
    await hb.close();
    await ta.close();
    await tb.close();
  });
});
