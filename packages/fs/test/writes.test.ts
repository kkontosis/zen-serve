// Local writes of the replica (fs.md §11): every operation online and
// offline, the queue across restarts, a lost reply, changed chunks only,
// conflicts, move cycles, refusals, rebases, tabs and eviction.
import {
  bytes,
  memoryLocalStore,
  ROOT,
  type SealedStore,
  sealedStore,
  Tree,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { indexedDbLocalStore } from '../../client/src/local/idb.js';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type Replica, type ReplicaOptions } from '../src/index.js';
import { FakeBus, FakeLocks, installation, type Net, until } from './helpers.js';

let w: World;
let admin: UnlockedFs;
beforeAll(async () => {
  // Small values: chunks of 1,984 bytes, 127 ids a page.
  w = await world({ limits: { max_value_bytes: 2048 } });
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const CS = 1984;
const text = (s: string) => bytes.utf8(s);
const str = (b: Uint8Array) => bytes.fromUtf8(b);
const blob = (n: number, seed = 1) =>
  Uint8Array.from({ length: n }, (_, i) => (i * 31 + seed) & 0xff);
const solo = (local: SealedStore = sealedStore(memoryLocalStore())): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 200,
});
const code = async (p: Promise<unknown>) => {
  try {
    await p;
  } catch (e) {
    return (e as ZenError).code;
  }
  return 'ok';
};

/** A fresh tree, a replica of it on another installation, and a view on the admin's session. */
async function setup(opts: Partial<ReplicaOptions> = {}) {
  const id = Tree.newId();
  const t = admin.tree(id);
  await t.mkdir(ROOT, '.keep'); // the tree exists
  const inst = await installation(w);
  const local = sealedStore(memoryLocalStore());
  const r = await openReplica(inst.ufs, id, { ...solo(local), ...opts });
  await r.sync();
  const view = await openReplica(admin, id, solo());
  const seen = async () => {
    await view.sync();
    return view;
  };
  return { id, t, r, net: inst.net, ufs: inst.ufs, local, seen };
}

/** Commits with tree ops, and the chunks and pages uploaded, since `net.reset()`. */
function sent(net: Net) {
  let commits = 0;
  let chunks = 0;
  let pages = 0;
  for (const c of net.calls) {
    if (c.path !== '/v1/commit') continue;
    const req = c.req as { crdt_ops?: unknown[]; chunks?: unknown[]; pages?: unknown[] };
    if (req.crdt_ops?.length) commits++;
    chunks += req.chunks?.length ?? 0;
    pages += req.pages?.length ?? 0;
  }
  return { commits, chunks, pages };
}

describe('operations online', () => {
  it('mkdir, writeFile, symlink, chmod, utimes, xattrs and content type', async () => {
    const { r, seen } = await setup();
    await r.mkdir('/docs');
    await r.writeFile('/docs/a.txt', text('hello'), { contentType: 'text/plain' });
    await r.writeFile('/docs/a.txt', text('hello world'));
    await r.symlink('docs/a.txt', '/link');
    await r.chmod('/docs/a.txt', 0o600);
    await r.utimes('/docs/a.txt', 1_000_000);
    await r.setxattr('/docs/a.txt', 'user.k', text('v'));
    await r.setxattr('/docs/a.txt', 'user.gone', text('x'));
    await r.removexattr('/docs/a.txt', 'user.gone');
    expect(await code(r.mkdir('/docs'))).toBe('exists');
    expect(await code(r.removexattr('/docs/a.txt', 'nope'))).toBe('no_attr');
    // Shown at once, before the server has it.
    expect(str(await r.readFile('/docs/a.txt'))).toBe('hello world');
    await r.flush();
    expect((await r.status()).queued).toBe(0);

    const v = await seen();
    expect(await v.readdir('/')).toEqual(['.keep', 'docs', 'link']);
    expect(str(await v.readFile('/docs/a.txt'))).toBe('hello world');
    const st = await v.stat('/docs/a.txt');
    expect(st).toMatchObject({ mode: 0o600, mtimeMs: 1_000_000, versions: 1, size: 11 });
    expect(st.contentType).toBe('text/plain');
    expect(await v.listxattr('/docs/a.txt')).toEqual(['user.k', 'zen.content_type']);
    expect(await v.readlink('/link')).toBe('docs/a.txt');
    expect((await v.stat('/link')).type).toBe('symlink');
  });

  it('rename in place, across, over a file and an empty dir; refusals', async () => {
    const { r, seen } = await setup();
    await r.mkdir('/a');
    await r.mkdir('/b');
    await r.mkdir('/b/full');
    await r.writeFile('/b/full/x', text('x'));
    await r.mkdir('/empty');
    await r.writeFile('/a/one', text('1'));
    await r.writeFile('/a/two', text('2'));
    await r.rename('/a/one', '/a/uno'); // in place
    await r.rename('/a/uno', '/b/uno'); // across
    await r.rename('/a/two', '/b/uno'); // over a file
    expect(await code(r.rename('/a', '/b/full'))).toBe('not_empty');
    expect(await code(r.rename('/b/uno', '/empty'))).toBe('is_dir');
    expect(await code(r.rename('/a', '/a/inside'))).toBe('bad_value');
    await r.rename('/a', '/empty'); // over an empty dir
    expect(await r.readdir('/')).toEqual(['.keep', 'b', 'empty']);
    await r.flush();
    const v = await seen();
    expect(await v.readdir('/')).toEqual(['.keep', 'b', 'empty']);
    expect(await v.readdir('/b')).toEqual(['full', 'uno']);
    expect(str(await v.readFile('/b/uno'))).toBe('2');
  });

  it('unlink, rmdir and truncate', async () => {
    const { r, seen } = await setup();
    await r.mkdir('/d');
    await r.writeFile('/d/f', blob(3 * CS + 5));
    await r.writeFile('/g', text('gone'));
    expect(await code(r.rmdir('/d'))).toBe('not_empty');
    expect(await code(r.unlink('/d'))).toBe('is_dir');
    await r.unlink('/g');
    await r.truncate('/d/f', CS + 3);
    expect((await r.stat('/d/f')).size).toBe(CS + 3);
    await r.flush();
    let v = await seen();
    expect(await v.readdir('/')).toEqual(['.keep', 'd']);
    expect(await v.readFile('/d/f')).toEqual(blob(3 * CS + 5).slice(0, CS + 3));
    await r.truncate('/d/f', CS + 10); // extended with zeros
    await r.unlink('/d/f');
    await r.rmdir('/d', { wait: true });
    v = await seen();
    expect(await v.readdir('/')).toEqual(['.keep']);
  });
});

describe('offline', () => {
  it('shows writes at once, keeps them across a restart, and sends them once back', async () => {
    const { id, r, net, ufs, local, seen } = await setup();
    await r.writeFile('/base.txt', text('v1'), { wait: true });
    net.down = true;
    await r.mkdir('/d');
    await r.writeFile('/d/new.txt', text('offline'));
    await r.writeFile('/base.txt', text('v2'));
    await r.rename('/base.txt', '/d/base.txt');
    await r.setxattr('/d', 'user.x', text('1'));
    const shown = async (x: Replica) => {
      expect(await x.readdir('/')).toEqual(['.keep', 'd']);
      expect(await x.readdir('/d')).toEqual(['base.txt', 'new.txt']);
      expect(str(await x.readFile('/d/base.txt'))).toBe('v2');
      expect(str(await x.readFile('/d/new.txt'))).toBe('offline');
    };
    await shown(r);
    expect(await code(r.flush())).toBe('offline');
    await r.close();

    const again = await openReplica(ufs, id, solo(local));
    expect((await again.status()).queued).toBe(5);
    await shown(again);
    net.down = false;
    net.reset();
    await again.flush();
    expect((await again.status()).queued).toBe(0);
    // Five entries: the batch stored in flight before the restart (the
    // first entries), then the rest.
    expect(sent(net).commits).toBeLessThanOrEqual(2);
    await shown(again);
    await shown(await seen());
    const node = (await again.nodeAt('/d/base.txt'))!;
    expect((await again.tree.versions(node.id)).length).toBe(1);
    await again.close();
  });

  it('a lost reply, then a crash: the stored commit is sent again with its commit_id', async () => {
    const { id, r, net, ufs, local, seen } = await setup();
    const s = ufs.session;
    const orig = s.call.bind(s);
    const ids: string[] = [];
    let lose = true;
    s.call = (async (path: string, enc: never, dec: never, req: never) => {
      const c = req as { commit_id?: Uint8Array; crdt_ops?: unknown[] };
      if (path === '/v1/commit' && c.crdt_ops?.length) ids.push(bytes.hex(c.commit_id!));
      const out = await orig(path, enc, dec, req);
      if (path === '/v1/commit' && c.crdt_ops?.length && lose) {
        lose = false;
        net.down = true; // and the crash comes before any retry
        throw new ZenError(0, 'network', 'reply lost (test)');
      }
      return out;
    }) as typeof s.call;
    await r.writeFile('/f.txt', text('once'));
    await until(() => expect(ids.length).toBe(1));
    await r.close();
    net.down = false;
    const again = await openReplica(ufs, id, solo(local));
    await again.flush();
    s.call = orig;
    expect(ids.length).toBe(2);
    expect(ids[1]).toBe(ids[0]);
    const v = await seen();
    expect(str(await v.readFile('/f.txt'))).toBe('once');
    const n = (await v.nodeAt('/f.txt'))!;
    expect((await v.tree.versions(n.id)).length).toBe(1);
    expect(await v.readdir('/')).toEqual(['.keep', 'f.txt']);
    await again.close();
  });
});

describe('changed chunks only', () => {
  it('a file of variable chunks (version 3) reads, and its edits flush in fixed chunks', async () => {
    const { id, r, seen } = await setup();
    const cdc = admin.tree(id, { cdc: { min: 64, avgBits: 8, max: 1984 } });
    const data = blob(150_000, 3);
    const f = await cdc.create(ROOT, 'cdc.bin');
    expect((await cdc.writeFile(f, data)).manifest.version).toBe(3);
    await r.sync();
    expect(await r.read('/cdc.bin', 70_000, 3000)).toEqual(data.slice(70_000, 73_000));
    expect(await r.readFile('/cdc.bin')).toEqual(data);
    const h = await r.open('/cdc.bin', 'r+');
    await h.write(text('edited'), 1000);
    await h.write(text('!'), data.length);
    await h.close();
    await r.flush();
    const next = bytes.concat(data, text('!'));
    next.set(text('edited'), 1000);
    const view = await seen();
    expect(await view.readFile('/cdc.bin')).toEqual(next);
    const [v] = await admin.tree(id).versions(f);
    expect(v!.manifest.version).toBe(2);
    // A second edit reuses the first one's chunks.
    const h2 = await r.open('/cdc.bin', 'r+');
    await h2.write(text('E'), 1000);
    await h2.close();
    await r.flush();
    next.set(text('E'), 1000);
    expect(await (await seen()).readFile('/cdc.bin')).toEqual(next);
  });

  it('a 1-byte write in a 300-chunk paged file sends one chunk and one page', async () => {
    const { t, r, net, seen } = await setup();
    const data = blob(300 * CS, 7);
    const f = await t.create(ROOT, 'big.bin');
    await t.writeFile(f, data);
    await r.sync();
    const h = await r.open('/big.bin', 'r+');
    expect(h.size).toBe(data.length);
    const at = 150 * CS + 17;
    await h.write(new Uint8Array([data[at]! ^ 0xff]), at);
    expect((await h.read(4, at - 1))[1]).toBe(data[at]! ^ 0xff);
    net.reset();
    await h.close();
    await r.flush();
    expect(sent(net)).toEqual({ commits: 1, chunks: 1, pages: 1 });
    const next = data.slice();
    next[at]! ^= 0xff;
    expect(await (await seen()).readFile('/big.bin')).toEqual(next);

    // Offline too: the content is pinned, so the index is built from it.
    await r.pin('/big.bin');
    await r.idle();
    net.down = true;
    const h2 = await r.open('/big.bin', 'r+');
    await h2.write(new Uint8Array([1, 2, 3]), 10);
    await h2.write(new Uint8Array([9]), next.length); // appended
    await h2.close();
    next.set([1, 2, 3], 10);
    expect(await r.readFile('/big.bin')).toEqual(bytes.concat(next, new Uint8Array([9])));
    net.down = false;
    net.reset();
    await r.flush();
    // Chunk 0, and the last one (grown); pages 0 and 2.
    expect(sent(net)).toEqual({ commits: 1, chunks: 2, pages: 2 });
    expect(await (await seen()).readFile('/big.bin')).toEqual(
      bytes.concat(next, new Uint8Array([9])),
    );
  });

  it('handles: append, read your writes, the last flush wins', async () => {
    const { r, seen } = await setup();
    const a = await r.open('/log', 'a');
    await a.write(text('one,'));
    await a.flush();
    await a.write(text('two'));
    expect(str(await a.read(100, 0))).toBe('one,two');
    const b = await r.open('/log', 'r+');
    await b.write(text('ONE'), 0);
    await a.close();
    await b.close(); // based on 'one,', replaces a's write too
    expect(str(await r.readFile('/log'))).toBe('ONE,');
    expect((await r.stat('/log')).versions).toBe(1);
    await r.flush();
    const v = await seen();
    expect(str(await v.readFile('/log'))).toBe('ONE,');
    expect((await v.stat('/log')).versions).toBe(1);
    expect(await code(r.open('/log', 'w', { exclusive: true }))).toBe('exists');
    expect(await code(r.open('/nope'))).toBe('not_found');
    const ro = await r.open('/log');
    expect(await code(ro.write(text('x'), 0))).toBe('bad_value');
  });
});

describe('conflicts and cycles', () => {
  it.each([
    ['after another write', false],
    ['as the first thing the replica sees', true],
  ])('a write the feed brings back before its reply is no conflict, %s', async (_, empty) => {
    // An empty tree: the replica has no cursor when the write is sent.
    const r = empty
      ? await openReplica((await installation(w)).ufs, Tree.newId(), solo())
      : (await setup()).r;
    if (empty) await r.sync();
    else {
      await r.writeFile('/f.txt', text('one'));
      await r.flush();
    }
    // Hold the commit's reply until the replica has caught up with the feed.
    const s = r.fs.session;
    const orig = s.call.bind(s);
    let held: { names: string[]; versions: number; text: string } | undefined;
    s.call = (async (path: string, enc: never, dec: never, req: unknown) => {
      const out = await orig(path, enc, dec, req as never);
      const ops = (req as { crdt_ops?: { op: string }[] }).crdt_ops;
      if (path === '/v1/commit' && !held && ops?.some((o) => o.op === 'write')) {
        await r.sync();
        held = {
          names: await r.readdir('/'),
          versions: (await r.stat('/f.txt')).versions,
          text: str(await r.readFile('/f.txt')),
        };
      }
      return out;
    }) as typeof s.call;
    try {
      await r.writeFile('/f.txt', text('two'));
      await r.flush();
    } finally {
      s.call = orig;
    }
    const names = empty ? ['f.txt'] : ['.keep', 'f.txt'];
    expect(held).toEqual({ names, versions: 1, text: 'two' });
    expect(await r.readdir('/')).toEqual(names);
    await r.close();
  });

  it('two replicas write one file offline: a conflict copy, resolved by unlink', async () => {
    const { id, r, net, seen } = await setup();
    await r.writeFile('/f.txt', text('base'), { wait: true });
    const two = await installation(w);
    const r2 = await openReplica(two.ufs, id, solo());
    await r2.sync();
    expect(str(await r2.readFile('/f.txt'))).toBe('base'); // its versions are kept
    net.down = true;
    two.net.down = true;
    await r.writeFile('/f.txt', text('from one'));
    await r2.writeFile('/f.txt', text('from two'));
    net.down = false;
    two.net.down = false;
    await r.flush();
    await r2.flush();
    await r.sync();
    for (const x of [r, r2, await seen()]) {
      const names = await x.readdir('/');
      expect(names).toHaveLength(3);
      expect(names.filter((n) => n.includes('(conflict '))).toHaveLength(1);
    }
    const copy = (await r.readdir('/')).find((n) => n.includes('(conflict '))!;
    const main = str(await r.readFile('/f.txt'));
    expect([main, str(await r.readFile(`/${copy}`))].sort()).toEqual(['from one', 'from two']);
    expect(await code(r.rename(`/${copy}`, '/x'))).toBe('bad_value');
    await r.unlink(`/${copy}`);
    expect(await r.readdir('/')).toEqual(['.keep', 'f.txt']);
    await r.flush();
    await r2.sync();
    for (const x of [r, r2, await seen()]) {
      expect(await x.readdir('/')).toEqual(['.keep', 'f.txt']);
      expect(str(await x.readFile('/f.txt'))).toBe(main);
    }
    await r2.close();
  });

  it('a move cycle made offline: the server keeps one move, both replicas agree', async () => {
    const { id, t, r, net } = await setup();
    await r.mkdir('/x');
    await r.mkdir('/y', { wait: true });
    const two = await installation(w);
    const r2 = await openReplica(two.ufs, id, solo());
    await r2.sync();
    net.down = true;
    two.net.down = true;
    const errors: unknown[] = [];
    r.onError((e) => errors.push(e));
    r2.onError((e) => errors.push(e));
    await r.rename('/x', '/y/x');
    // Both installations are the admin's device: keep their timestamps apart.
    await new Promise((res) => setTimeout(res, 5));
    await r2.rename('/y', '/x/y');
    net.down = false;
    two.net.down = false;
    await r.flush();
    await r2.flush();
    await r.sync();
    await r2.sync();
    const x = (await t.list(ROOT)).length;
    expect(x).toBe(2); // .keep and one of x, y
    for (const rep of [r, r2]) {
      for (const n of await t.list(ROOT)) {
        expect(rep.node(n.id)?.parent).toEqual(ROOT);
      }
    }
    expect(await r.readdir('/')).toEqual(await r2.readdir('/'));
    expect(errors).toEqual([]);
    await r2.close();
  });
});

describe('failures', () => {
  it('a refused entry is dropped: onError, wait rejects, the server state shows', async () => {
    const { r, ufs } = await setup();
    await r.mkdir('/ok', { wait: true });
    const s = ufs.session;
    const orig = s.call.bind(s);
    let refuse = 1;
    s.call = (async (path: string, enc: never, dec: never, req: never) => {
      const c = req as { crdt_ops?: unknown[] };
      if (path === '/v1/commit' && c.crdt_ops?.length && refuse-- > 0) {
        throw new ZenError(403, 'forbidden', 'no write right (test)');
      }
      return orig(path, enc, dec, req);
    }) as typeof s.call;
    const errors: string[] = [];
    r.onError((e) => errors.push((e as ZenError).code));
    const changed: string[] = [];
    r.onChange((ids) => changed.push(...ids.map((i) => bytes.hex(i))));
    const p = r.mkdir('/refused', { wait: true });
    expect(await code(p)).toBe('forbidden');
    expect(errors).toEqual(['forbidden']);
    expect(await r.readdir('/')).toEqual(['.keep', 'ok']);
    expect(changed.length).toBeGreaterThan(0);
    // The next one goes through.
    await r.mkdir('/later', { wait: true });
    expect(await r.readdir('/')).toEqual(['.keep', 'later', 'ok']);
    s.call = orig;
  });
});

describe('a queued edit past the horizon', () => {
  let w2: World;
  let fs2: UnlockedFs;
  beforeAll(async () => {
    w2 = await world({ limits: { crdt_horizon_secs: 1 } });
    fs2 = await w2.session
      .fs(1)
      .init((k) => [zw.createDeviceSlot(k, w2.admin.device.devicePublic)]);
  });
  afterAll(() => w2?.server.stop());

  it('is rebased (ticked afresh) and applied', async () => {
    const id = Tree.newId();
    const t = fs2.tree(id);
    const d = await t.mkdir(ROOT, 'old');
    const inst = await installation(w2);
    const r = await openReplica(inst.ufs, id, solo());
    await r.sync();
    inst.net.down = true;
    await r.rename('/old', '/new');
    await new Promise((res) => setTimeout(res, 1500));
    inst.net.down = false;
    await r.flush();
    expect((await t.getOne(d))!.meta?.name).toBe('new');
    expect(inst.net.calls.filter((c) => c.path === '/v1/commit').length).toBeGreaterThanOrEqual(2);
    await r.close();
  });
});

describe('tabs', () => {
  it("the owner sends the others' writes; closing it hands sending over", async () => {
    const id = Tree.newId();
    const t = admin.tree(id);
    await t.mkdir(ROOT, '.keep');
    const locks = new FakeLocks();
    const bus = new FakeBus();
    const store = indexedDbLocalStore(`writes${Date.now()}`, {
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
    await b.writeFile('/from-b.txt', text('b'), { wait: true });
    expect(sent(two.net).commits).toBe(0);
    expect(sent(one.net).commits).toBe(1);
    await until(async () => expect(await a.readdir('/')).toEqual(['.keep', 'from-b.txt']));
    expect(await b.readdir('/')).toEqual(['.keep', 'from-b.txt']);

    // Queued in b while the owner is offline, then the owner closes.
    one.net.down = true;
    await b.mkdir('/d1');
    await b.writeFile('/d1/x', text('x'));
    await a.close();
    await until(() => expect(b.isOwner).toBe(true));
    await b.flush();
    await b.mkdir('/d2', { wait: true });
    const names = (await t.list(ROOT)).map((n) => n.meta?.name).sort();
    expect(names).toEqual(['.keep', 'd1', 'd2', 'from-b.txt']);
    const d1 = (await t.list(ROOT)).find((n) => n.meta?.name === 'd1')!;
    const xs = await t.list(d1.id);
    expect(xs.map((n) => n.meta?.name)).toEqual(['x']);
    expect((await t.versions(xs[0]!.id)).length).toBe(1);
    await b.close();
  });
});

describe('eviction', () => {
  it('never drops queued content', async () => {
    const { r, net, seen } = await setup({ cacheBytes: 1000 });
    net.down = true;
    const data = blob(10 * CS, 3);
    await r.writeFile('/big', data);
    expect(await r.evict()).toBeGreaterThan(1000);
    net.down = false;
    await r.flush();
    expect(await (await seen()).readFile('/big')).toEqual(data);
  });
});
