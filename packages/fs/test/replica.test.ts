// The local replica (fs.md §11): sync, paths, content on demand, pins,
// eviction, offline reads, the stores, and sharing a store.
import { mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  bytes,
  type LocalStore,
  memoryLocalStore,
  ROOT,
  sealedStore,
  Tree,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { nodeDiskLocalStore } from '@zen/client/node';
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { indexedDbLocalStore } from '../../client/src/local/idb.js';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, Replica, type ReplicaOptions } from '../src/index.js';
import { FakeBus, FakeLocks, installation, until } from './helpers.js';

let w: World;
let admin: UnlockedFs;
let dir: string;
beforeAll(async () => {
  // Small values: chunks of 1,984 bytes, 127 ids a page, about 55 chunks inline.
  w = await world({ limits: { max_value_bytes: 2048 } });
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
  dir = await mkdtemp(join(tmpdir(), 'zen-replica-'));
});
afterAll(async () => {
  w?.server.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

const text = (s: string) => bytes.utf8(s);
const str = (b: Uint8Array) => bytes.fromUtf8(b);
/** Deterministic content of `n` bytes. */
const blob = (n: number, seed = 1) =>
  Uint8Array.from({ length: n }, (_, i) => (i * 31 + seed) & 0xff);
const solo = (local = sealedStore(memoryLocalStore())): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 200,
});

/** A fresh tree with a writer on the admin's session. */
function freshTree(): { id: Uint8Array; t: Tree } {
  const id = Tree.newId();
  return { id, t: admin.tree(id) };
}

describe('sync (fs.md §11)', () => {
  it('follows another installation: creates, writes, renames, moves and trash', async () => {
    const { id, t } = freshTree();
    const a = await t.mkdir(ROOT, 'docs');
    const x = await t.create(a, 'x.txt');
    await t.writeFile(x, text('hello'));
    const { ufs } = await installation(w);
    const r = await openReplica(ufs, id, solo());
    await r.sync();
    expect(await r.readdir('/')).toEqual(['docs']);
    expect(await r.readdir('/docs', { withTypes: true })).toEqual([
      { name: 'x.txt', type: 'file' },
    ]);
    expect(str(await r.readFile('/docs/x.txt'))).toBe('hello');
    expect((await r.stat('/docs/x.txt')).size).toBe(5);

    // The loop follows on its own (long-poll).
    const changed: string[] = [];
    r.onChange((ids) => changed.push(...ids.map((i) => bytes.hex(i))));
    await t.rename(x, 'y.txt');
    const b = await t.mkdir(ROOT, 'old');
    await until(async () => expect(await r.readdir('/docs')).toEqual(['y.txt']));
    expect(changed).toContain(bytes.hex(x));
    await t.move(a, b);
    await until(async () => expect(await r.readdir('/old')).toEqual(['docs']));
    await t.remove(b);
    await until(async () => expect(await r.readdir('/')).toEqual([]));
    await expect(r.stat('/old')).rejects.toMatchObject({ code: 'not_found' });
    await r.close();
  });

  it('a restart resumes from its cursor; a resync drops what vanished', async () => {
    const { id, t } = freshTree();
    const f = await t.create(ROOT, 'a.txt');
    await t.writeFile(f, text('a'));
    const local = sealedStore(memoryLocalStore());
    const one = await installation(w);
    const r1 = await openReplica(one.ufs, id, { ...solo(local), owner: true });
    await r1.sync();
    // Plant a node the server never had: a full sync must drop it.
    const real = (await local.get(`zfs/1/${bytes.hex(id)}/nodes`, f))!;
    const ghost = zw.decodeNodeState(real);
    ghost.node = Tree.newId();
    await r1.close();
    await local.put(`zfs/1/${bytes.hex(id)}/nodes`, ghost.node, zw.encodeNodeState(ghost));

    const two = await installation(w);
    two.net.down = true;
    const r2 = await openReplica(two.ufs, id, solo(local));
    // From the store alone, before any call answers.
    expect((await r2.readdir('/')).sort()).toEqual(['a.txt', bytes.hex(ghost.node)].sort());
    await expect(r2.sync()).rejects.toMatchObject({ code: 'network' });
    expect(r2.online).toBe(false);
    two.net.down = false;
    two.net.reset();
    await t.create(ROOT, 'b.txt');
    await r2.sync();
    const first = two.net.calls.find((c) => c.path === '/v1/fs/tree/changes')!;
    expect((first.req as { after?: Uint8Array }).after).toBeDefined();
    expect(r2.online).toBe(true);
    // The ghost stays until a full sync.
    expect(await r2.readdir('/')).toContain(bytes.hex(ghost.node));
    await r2.close();

    // A cursor the server refuses (409 resync): a full sync drops the ghost.
    const three = await installation(w);
    const orig = three.ufs.session.call.bind(three.ufs.session);
    let refused = 0;
    three.ufs.session.call = ((path: string, ...rest: unknown[]) => {
      const req = rest[2] as { after?: Uint8Array };
      if (path === '/v1/fs/tree/changes' && req.after && !refused) {
        refused = 1;
        return Promise.reject(new ZenError(409, 'resync', 'test'));
      }
      return (orig as (...a: unknown[]) => unknown)(path, ...rest);
    }) as typeof three.ufs.session.call;
    const r3 = await openReplica(three.ufs, id, solo(local));
    await r3.sync();
    expect(refused).toBe(1);
    expect(await r3.readdir('/')).toEqual(['a.txt', 'b.txt']);
    await r3.close();
  });
});

describe('paths', () => {
  it('duplicate names and conflict copies', async () => {
    const { id, t } = freshTree();
    await t.create(ROOT, 'same.txt');
    await t.create(ROOT, 'same.txt');
    const f = await t.create(ROOT, 'notes.md');
    // Two writes that don't know each other: siblings.
    await t.writeFile(f, text('one'), { replaces: [] });
    await t.writeFile(f, text('two'), { replaces: [] });
    const { ufs } = await installation(w);
    const r = await openReplica(ufs, id, solo());
    await r.sync();
    const names = await r.readdir('/');
    expect(names).toContain('same.txt');
    expect(names).toContain('same (2).txt');
    const copy = names.find((n) => n.startsWith('notes (conflict '))!;
    expect(copy).toMatch(/^notes \(conflict [0-9a-f]{8}-2\)\.md$/);
    const vs = (await t.versions(f)).sort((a, b) => bytes.compare(b.dot, a.dot));
    expect(str(await r.readFile('/notes.md'))).toBe(str(await t.readFile(f, { version: vs[0]! })));
    expect(str(await r.readFile(`/${copy}`))).toBe(str(await t.readFile(f, { version: vs[1]! })));
    const st = await r.stat(`/${copy}`);
    expect(st.conflict).toBe(true);
    expect(st.mode & 0o222).toBe(0);
    await expect(r.readdir('/notes.md')).rejects.toMatchObject({ code: 'not_dir' });
    await expect(r.readFile('/')).rejects.toMatchObject({ code: 'is_dir' });
    await r.close();
  });

  it('symlinks and xattrs', async () => {
    const { id, t } = freshTree();
    const l = await t.create(ROOT, 'link', { type: 'symlink' });
    await t.writeFile(l, text('target/file'));
    const x = await t.create(ROOT, 'x', {
      xattrs: (await import('../src/index.js')).encodeXattrs(new Map([['user.k', text('v')]])),
    });
    const { ufs } = await installation(w);
    const r = await openReplica(ufs, id, solo());
    await r.sync();
    expect(await r.readlink('/link')).toBe('target/file');
    expect((await r.stat('/link')).type).toBe('symlink');
    expect(await r.listxattr('/x')).toEqual(['user.k']);
    expect(str(await r.getxattr('/x', 'user.k'))).toBe('v');
    await expect(r.getxattr('/x', 'user.none')).rejects.toMatchObject({ code: 'no_attr' });
    expect(x).toBeDefined();
    await r.close();
  });
});

describe('content on demand', () => {
  it('reads fetch only their chunks once; paged files stream by range', async () => {
    const { id, t } = freshTree();
    const f = await t.create(ROOT, 'big.bin');
    const data = blob(400 * 1984 + 7);
    await t.writeFile(f, data);
    expect((await t.versions(f))[0]!.pages.length).toBe(4);
    const { ufs, net } = await installation(w);
    const r = await openReplica(ufs, id, solo());
    await r.sync();
    net.reset();
    const mid = await r.read('/big.bin', 300_000, 5000);
    expect(mid).toEqual(data.slice(300_000, 305_000));
    expect(net.count('/v1/fs/chunks/get')).toBe(1);
    expect(net.count('/v1/fs/pages/get')).toBe(1);
    net.reset();
    expect(await r.read('/big.bin', 300_000, 5000)).toEqual(mid);
    expect(net.calls).toEqual([]);

    const s = await r.createReadStream('/big.bin', { start: 500_000, end: 600_000 });
    const parts: Uint8Array[] = [];
    for await (const p of s as unknown as AsyncIterable<Uint8Array>) parts.push(p);
    expect(bytes.concat(...parts)).toEqual(data.slice(500_000, 600_001));
    await r.close();
  });

  it('eviction keeps the total under the limit and never drops pinned content', async () => {
    const { id, t } = freshTree();
    const p = await t.mkdir(ROOT, 'keep');
    const kept = await t.create(p, 'k.bin');
    await t.writeFile(kept, blob(30_000, 2));
    for (let i = 0; i < 4; i++) {
      const f = await t.create(ROOT, `f${i}.bin`);
      await t.writeFile(f, blob(30_000, i + 10));
    }
    const { ufs, net } = await installation(w);
    const r = await openReplica(ufs, id, { ...solo(), cacheBytes: 50_000 });
    await r.sync();
    await r.pin('/keep');
    await r.idle();
    for (let i = 0; i < 4; i++) await r.readFile(`/f${i}.bin`);
    const left = await r.evict();
    expect(left).toBeLessThanOrEqual(50_000);
    net.down = true;
    expect(await r.readFile('/keep/k.bin')).toEqual(blob(30_000, 2));
    await expect(r.readFile('/f0.bin')).rejects.toMatchObject({ code: 'offline' });
    net.down = false;
    await r.close();
  });

  it('a pinned folder keeps files added under it later', async () => {
    const { id, t } = freshTree();
    const p = await t.mkdir(ROOT, 'pinned');
    const { ufs, net } = await installation(w);
    const r = await openReplica(ufs, id, solo());
    await r.sync();
    await r.pin('/pinned');
    const f = await t.create(p, 'late.txt');
    await t.writeFile(f, text('late'));
    await r.sync();
    await r.idle();
    net.down = true;
    expect(str(await r.readFile('/pinned/late.txt'))).toBe('late');
    expect((await r.status()).pins).toBe(1);
    net.down = false;
    await r.close();
  });
});

describe('offline', () => {
  it('a reopened replica lists and reads what it kept, and catches up once back', async () => {
    const { id, t } = freshTree();
    const a = await t.create(ROOT, 'read.txt');
    await t.writeFile(a, text('kept'));
    const b = await t.create(ROOT, 'unread.txt');
    await t.writeFile(b, text('not kept'));
    const local = sealedStore(memoryLocalStore());
    const one = await installation(w);
    const r1 = await openReplica(one.ufs, id, solo(local));
    await r1.sync();
    await r1.readFile('/read.txt');
    await r1.close();

    const two = await installation(w);
    two.net.down = true;
    const r2 = await openReplica(two.ufs, id, solo(local));
    expect(await r2.readdir('/')).toEqual(['read.txt', 'unread.txt']);
    expect(str(await r2.readFile('/read.txt'))).toBe('kept');
    await expect(r2.readFile('/unread.txt')).rejects.toMatchObject({ code: 'offline' });
    await until(async () => expect((await r2.status()).online).toBe(false));
    await t.create(ROOT, 'new.txt');
    two.net.down = false;
    await until(async () => expect(await r2.readdir('/')).toContain('new.txt'));
    expect((await r2.status()).online).toBe(true);
    await r2.close();
  });
});

let n = 0;
const backends: [string, () => LocalStore][] = [
  ['memory', () => memoryLocalStore()],
  ['node disk', () => nodeDiskLocalStore(join(dir, `d${n++}`))],
  [
    'IndexedDB',
    () => indexedDbLocalStore(`r${n++}`, { factory: new IDBFactory(), keyRange: IDBKeyRange }),
  ],
];

describe.each(backends)('%s store', (_, make) => {
  it('keeps the replica across a restart, sealed', async () => {
    const { id, t } = freshTree();
    const f = await t.create(ROOT, 'plain-name-xyz.txt');
    await t.writeFile(f, text('plain-content-xyz'));
    const raw = make();
    const one = await installation(w);
    const r1 = await openReplica(one.ufs, id, solo(sealedStore(raw)));
    await r1.sync();
    await r1.readFile('/plain-name-xyz.txt');
    await r1.close();
    const two = await installation(w);
    two.net.down = true;
    const r2 = await openReplica(two.ufs, id, solo(sealedStore(raw)));
    expect(str(await r2.readFile('/plain-name-xyz.txt'))).toBe('plain-content-xyz');
    await r2.close();
    // Nothing in the clear: neither the name nor the content appear in any record.
    for (const kind of ['nodes', 'versions', 'chunks', 'pages', 'lru', 'sync']) {
      for await (const [, v] of raw.list(`zfs/1/${bytes.hex(id)}/${kind}`)) {
        expect(new TextDecoder('latin1').decode(v)).not.toContain('plain-');
      }
    }
    await Replica.wipe(sealedStore(raw), 1, id);
    for await (const _ of raw.list(`zfs/1/${bytes.hex(id)}/nodes`)) throw new Error('not wiped');
  });
});

describe('sharing a store', () => {
  it('tabs: only the owner follows the feed; the others reload; closing hands over', async () => {
    const { id, t } = freshTree();
    const locks = new FakeLocks();
    const bus = new FakeBus();
    const store = indexedDbLocalStore(`share${n++}`, {
      factory: new IDBFactory(),
      keyRange: IDBKeyRange,
    });
    const opts = (): ReplicaOptions => ({
      local: sealedStore(store),
      locks,
      channel: bus.open,
      waitMs: 200,
    });
    const one = await installation(w);
    const two = await installation(w);
    const a = await openReplica(one.ufs, id, opts());
    const b = await openReplica(two.ufs, id, opts());
    await until(() => expect(a.isOwner).toBe(true));
    expect(b.isOwner).toBe(false);
    const seen: string[] = [];
    b.onChange((ids) => seen.push(...ids.map((i) => bytes.hex(i))));
    const f = await t.create(ROOT, 'shared.txt');
    await until(async () => expect(await b.readdir('/')).toEqual(['shared.txt']));
    expect(seen).toContain(bytes.hex(f));
    expect(two.net.count('/v1/fs/tree/changes')).toBe(0);
    expect(one.net.count('/v1/fs/tree/changes')).toBeGreaterThan(0);

    await a.close();
    await until(() => expect(b.isOwner).toBe(true));
    await t.create(ROOT, 'after.txt');
    await until(async () => expect(await b.readdir('/')).toEqual(['after.txt', 'shared.txt']));
    expect(two.net.count('/v1/fs/tree/changes')).toBeGreaterThan(0);
    await b.close();
  });

  it('Node: one replica per disk store; a stale lock is taken over', async () => {
    const { id } = freshTree();
    const d = join(dir, `lock${n++}`);
    const one = await installation(w);
    const r1 = await openReplica(one.ufs, id, solo(sealedStore(nodeDiskLocalStore(d))));
    await expect(
      openReplica(one.ufs, id, solo(sealedStore(nodeDiskLocalStore(d)))),
    ).rejects.toMatchObject({ code: 'locked' });
    await r1.close();
    const r2 = await openReplica(one.ufs, id, solo(sealedStore(nodeDiskLocalStore(d))));
    await r2.close();
    // A lock left by a process that is gone.
    const lock = (await readdir(d)).find((x) => x.endsWith('.lock'));
    expect(lock).toBeUndefined();
    const name = `${encodeURIComponent(`zfs-1-${bytes.hex(id)}`)}.lock`;
    await writeFile(join(d, name), '2147483646');
    const r3 = await openReplica(one.ufs, id, solo(sealedStore(nodeDiskLocalStore(d))));
    expect(Number(await readFile(join(d, name), 'utf8'))).toBe(process.pid);
    await r3.close();
  });
});
