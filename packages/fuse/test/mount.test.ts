// zen-mount against a spawned server: real FUSE mounts of @zen/fs replicas
// driven through node:fs. Skipped where FUSE can't mount (no /dev/fuse or
// fusermount).
import { type ChildProcess, execFile, execFileSync, spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import {
  lstat,
  mkdir,
  mkdtemp,
  open,
  readdir,
  readFile,
  readlink,
  rename,
  rm,
  rmdir,
  stat,
  symlink,
  unlink,
  writeFile,
} from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { bytes, memoryLocalStore, ROOT, sealedStore, Tree, type UnlockedFs, zw } from '@zen/client';
import { openReplica, type Replica } from '@zen/fs';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { type Net, watch } from '../../fs/test/helpers.js';
import { type Mount, type MountOptions, mount } from '../src/mount.js';

const canMount =
  process.platform === 'linux' &&
  existsSync('/dev/fuse') &&
  ['/bin/fusermount', '/usr/bin/fusermount'].some((p) => existsSync(p));
if (!canMount) console.warn('zen-mount tests skipped: FUSE is not available here');

let w: World;
let ufs: UnlockedFs;
let base: string;
const treeId = Tree.newId();

/** A mount and the replica under it. */
interface Mounted {
  m: Mount;
  r: Replica;
  mountpoint: string;
}
const mounts: Mounted[] = [];

/** Open a replica of `id` (in memory), catch up, and mount it at `dir`. */
async function mountReplica(
  u: UnlockedFs,
  id: Uint8Array,
  dir: string,
  opts: MountOptions = {},
): Promise<Mounted> {
  const r = await openReplica(u, id, {
    local: sealedStore(memoryLocalStore()),
    locks: null,
    channel: null,
    waitMs: 1000,
  });
  await r.sync();
  const m = await mount(r, dir, { attrTimeout: 0, ...opts });
  return { m, r, mountpoint: m.mountpoint };
}

async function mountAt(name: string, u: UnlockedFs = ufs, opts: MountOptions = {}) {
  const x = await mountReplica(u, treeId, join(base, name), opts);
  mounts.push(x);
  return x;
}

/** Unmount, send what is queued, and close the replica. */
async function release(x: Mounted): Promise<void> {
  await x.m.unmount();
  await x.r.flush();
  await x.r.close();
}

/** Run a Python snippet (Node has no xattr calls); its stdout. Async: the FUSE handlers run in this process. */
const run = promisify(execFile);

const py = async (code: string, ...args: string[]) =>
  (await run('python3', ['-c', code, ...args], { encoding: 'utf8' })).stdout.trim();

/** Poll until `f` stops throwing (the other mount learns through the change feed). */
async function eventually<T>(f: () => Promise<T>, ms = 15000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    try {
      return await f();
    } catch (e) {
      if (Date.now() > end) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

describe.skipIf(!canMount)('zen-mount', () => {
  beforeAll(async () => {
    w = await world({ limits: { max_commit_bytes: 300000 } });
    ufs = await w.session
      .fs(1)
      .init((k) => [zw.createPassphraseSlot(k, bytes.utf8('pp'), 65536, 1, 1)]);
    base = await mkdtemp(join(tmpdir(), 'zen-mnt-'));
  });

  afterAll(async () => {
    for (const x of mounts) await release(x).catch(() => {});
    if (base) await rm(base, { recursive: true, force: true });
    await w?.server.stop();
  });

  it('files and directories through the kernel', async () => {
    const m = await mountAt('a');
    const p = (s: string) => join(m.mountpoint, s);
    await mkdir(p('docs'));
    await writeFile(p('docs/hello.txt'), 'hello world');
    expect(await readFile(p('docs/hello.txt'), 'utf8')).toBe('hello world');
    expect(await readdir(p('docs'))).toEqual(['hello.txt']);
    expect((await stat(p('docs/hello.txt'))).size).toBe(11);
    expect((await stat(p('docs'))).isDirectory()).toBe(true);

    await rename(p('docs/hello.txt'), p('greeting.txt'));
    expect((await readdir(m.mountpoint)).sort()).toEqual(['docs', 'greeting.txt']);
    await writeFile(p('greeting.txt'), 'hello again'); // overwrite
    expect(await readFile(p('greeting.txt'), 'utf8')).toBe('hello again');

    await expect(rmdir(m.mountpoint)).rejects.toBeTruthy();
    await writeFile(p('docs/x'), 'x');
    await expect(rmdir(p('docs'))).rejects.toMatchObject({ code: 'ENOTEMPTY' });
    await unlink(p('docs/x'));
    await rmdir(p('docs'));
    expect(await readdir(m.mountpoint)).toEqual(['greeting.txt']);

    // The bytes really are on the server once sent, sealed: read them through the API.
    await m.r.flush();
    const node = (await ufs.tree(treeId).list(new Uint8Array(16))).find(
      (n) => n.meta?.name === 'greeting.txt',
    )!;
    expect(bytes.fromUtf8(await ufs.tree(treeId).readFile(node.id))).toBe('hello again');
  });

  it('a file larger than one commit, and ranged reads', async () => {
    const m = mounts[0]!;
    const big = new Uint8Array(1_000_000).map((_, i) => (i * 7) % 251);
    await writeFile(join(m.mountpoint, 'big.bin'), big);
    const back = new Uint8Array(await readFile(join(m.mountpoint, 'big.bin')));
    expect(back.length).toBe(big.length);
    expect(bytes.equal(back, big)).toBe(true);
    const fh = await open(join(m.mountpoint, 'big.bin'), 'r');
    const buf = Buffer.alloc(100);
    await fh.read(buf, 0, 100, 65500);
    await fh.close();
    expect(bytes.equal(new Uint8Array(buf), big.subarray(65500, 65600))).toBe(true);
  });

  it('a second mount sees changes, and concurrent writes show a conflict copy', async () => {
    const a = mounts[0]!;
    const other = await w.session.fs(1).unlock({ passphrase: 'pp' });
    const b = await mountAt('b', other);
    await writeFile(join(a.mountpoint, 'shared.txt'), 'v1');
    // The create may arrive before the content: wait for the content.
    await eventually(async () => {
      const t = await readFile(join(b.mountpoint, 'shared.txt'), 'utf8');
      if (t !== 'v1') throw new Error(`not yet: ${t}`);
    });

    // Both open the same version, then both write: siblings.
    const fa = await open(join(a.mountpoint, 'shared.txt'), 'r+');
    const fb = await open(join(b.mountpoint, 'shared.txt'), 'r+');
    await fa.write('from a', 0);
    await fb.write('from b', 0);
    await fa.close();
    await fb.close();
    // Both sent and A caught up: what A shows no longer changes (both copies
    // are named after the same device, so a changing view could swap them).
    await a.r.flush();
    await b.r.flush();
    const names = await eventually(async () => {
      await a.r.sync();
      const ls = await readdir(a.mountpoint);
      if (!ls.some((n) => n.includes('(conflict'))) throw new Error('no conflict copy yet');
      return ls;
    });
    const copy = names.find((n) => n.startsWith('shared (conflict'))!;
    expect(copy).toMatch(/\.txt$/);
    const texts = [
      await readFile(join(a.mountpoint, 'shared.txt'), 'utf8'),
      await readFile(join(a.mountpoint, copy), 'utf8'),
    ];
    expect(texts.sort()).toEqual(['from a', 'from b']);
    // Deleting the copy resolves the conflict.
    await unlink(join(a.mountpoint, copy));
    expect((await readdir(a.mountpoint)).some((n) => n.includes('(conflict'))).toBe(false);
  });

  it('data persists across unmount and mount', async () => {
    for (const x of mounts.splice(0)) await release(x);
    const m = await mountAt('again');
    expect(await readFile(join(m.mountpoint, 'greeting.txt'), 'utf8')).toBe('hello again');
  });

  it('symlinks', async () => {
    const m = mounts[0]!;
    const p = (s: string) => join(m.mountpoint, s);
    await symlink('greeting.txt', p('link'));
    expect(await readlink(p('link'))).toBe('greeting.txt');
    expect((await lstat(p('link'))).isSymbolicLink()).toBe(true);
    expect(await readFile(p('link'), 'utf8')).toBe('hello again');
    await mkdir(p('sub'));
    await symlink('../greeting.txt', p('sub/up'));
    expect(await readFile(p('sub/up'), 'utf8')).toBe('hello again');
    await unlink(p('sub/up'));
    await rmdir(p('sub'));
    expect(await m.r.readlink('/link')).toBe('greeting.txt');
  });

  it('xattrs: user.<x> is the stored <x>, other namespaces are refused', async () => {
    const m = mounts[0]!;
    const f = join(m.mountpoint, 'greeting.txt');
    const out = (
      await py(
        `import os, sys
p = sys.argv[1]
os.setxattr(p, 'user.k', b'v1')
os.setxattr(p, 'user.zen.content_type', b'text/plain')
os.setxattr(p, 'user.gone', b'x')
os.removexattr(p, 'user.gone')
print(os.getxattr(p, 'user.k').decode())
print(','.join(sorted(os.listxattr(p))))
def err(f):
    try:
        f()
        return 'ok'
    except OSError as e:
        return str(e.errno)
print(err(lambda: os.getxattr(p, 'user.missing')))
print(err(lambda: os.setxattr(p, 'security.zen', b'x')))
print(err(lambda: os.setxattr(p, 'user.k', b'x', os.XATTR_CREATE)))
print(err(lambda: os.setxattr(p, 'user.new', b'x', os.XATTR_REPLACE)))`,
        f,
      )
    ).split('\n');
    expect(out).toEqual([
      'v1',
      'user.k,user.zen.content_type',
      '61', // ENODATA
      '95', // EOPNOTSUPP
      '17', // EEXIST
      '61',
    ]);
    expect(bytes.fromUtf8(await m.r.getxattr('/greeting.txt', 'k'))).toBe('v1');
    expect((await m.r.stat('/greeting.txt')).contentType).toBe('text/plain');
    // Sent, they reach another replica.
    await m.r.flush();
    const other = await mountReplica(ufs, treeId, join(base, 'x2'), { readOnly: true });
    try {
      expect((await other.r.stat('/greeting.txt')).contentType).toBe('text/plain');
      expect(
        await py(
          'import os, sys; print(os.getxattr(sys.argv[1], "user.k").decode())',
          join(other.mountpoint, 'greeting.txt'),
        ),
      ).toBe('v1');
    } finally {
      await release(other);
    }
  });

  it('read-only mounts refuse changes', async () => {
    const m = await mountAt('ro', ufs, { readOnly: true });
    expect(await readFile(join(m.mountpoint, 'greeting.txt'), 'utf8')).toBe('hello again');
    await expect(writeFile(join(m.mountpoint, 'new.txt'), 'x')).rejects.toMatchObject({
      code: 'EROFS',
    });
  });

  it('the zen-mount CLI mounts in the background and unmounts on SIGTERM', async () => {
    const root = resolve(import.meta.dirname, '../../..');
    execFileSync('npx', ['tsc', '-p', 'packages/fuse/tsconfig.json'], { cwd: root });
    await w.session.setPassword('cli', 'cli password');
    const mnt = join(base, 'cli');
    const child = spawn(
      process.execPath,
      [
        join(root, 'packages/fuse/dist/cli.js'),
        w.server.url,
        mnt,
        '--origin',
        w.client.origin,
        '--user',
        'cli',
        '--passphrase',
        '--tree',
        bytes.hex(treeId),
        '--foreground',
      ],
      {
        env: { ...process.env, ZEN_PASSWORD: 'cli password', ZEN_PASSPHRASE: 'pp' },
        stdio: ['ignore', 'pipe', 'pipe'],
      },
    );
    let err = '';
    child.stderr.on('data', (d) => {
      err += d;
    });
    try {
      expect(await eventually(() => readFile(join(mnt, 'greeting.txt'), 'utf8'))).toBe(
        'hello again',
      );
    } finally {
      const done = new Promise((r) => child.once('exit', r));
      child.kill('SIGTERM');
      await done;
    }
    expect(err).toContain('mounted fs 1');
    expect(existsSync(join(mnt, 'greeting.txt'))).toBe(false);
  });
});

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

describe.skipIf(!canMount)('zen-mount, large files', () => {
  // A small max_value_bytes: chunks of 1,984 bytes, 127 ids a page.
  const CS = 1984;
  let wl: World;
  let dir: string;
  let x: Mounted;
  let t: Tree;
  let net: Net;
  beforeAll(async () => {
    wl = await world({ limits: { max_value_bytes: 2048, max_commit_bytes: 300000 } });
    const u = await wl.session
      .fs(1)
      .init((k) => [zw.createPassphraseSlot(k, bytes.utf8('pp'), 65536, 1, 1)]);
    net = watch(wl.session);
    dir = await mkdtemp(join(tmpdir(), 'zen-mnt-large-'));
    const id = Tree.newId();
    t = u.tree(id);
    x = await mountReplica(u, id, join(dir, 'm'));
  });

  afterAll(async () => {
    if (x) await release(x).catch(() => {});
    if (dir) await rm(dir, { recursive: true, force: true });
    await wl?.server.stop();
  });

  it('a paged file written and read through the kernel', async () => {
    const data = new Uint8Array(400 * CS + 3).map((_, i) => (i * 31) % 251);
    const p = join(x.mountpoint, 'big.bin');
    await writeFile(p, data);
    expect((await stat(p)).size).toBe(data.length);
    expect(bytes.equal(new Uint8Array(await readFile(p)), data)).toBe(true);
    await x.r.flush();
    const node = (await t.list(ROOT)).find((n) => n.meta?.name === 'big.bin')!;
    const [v] = await t.versions(node.id);
    expect(v!.pages.length).toBe(4);
  });

  it('a 1-byte write through the kernel sends one chunk and one page', async () => {
    const data = new Uint8Array(300 * CS).map((_, i) => (i * 7 + 3) & 0xff);
    const f = await t.create(ROOT, 'edit.bin');
    await t.writeFile(f, data);
    await x.r.sync();
    const p = join(x.mountpoint, 'edit.bin');
    const at = 150 * CS + 17;
    const fh = await open(p, 'r+');
    await fh.write(new Uint8Array([data[at]! ^ 0xff]), 0, 1, at);
    net.reset();
    await fh.close();
    await x.r.flush();
    expect(sent(net)).toEqual({ commits: 1, chunks: 1, pages: 1 });
    const next = data.slice();
    next[at]! ^= 0xff;
    expect(bytes.equal(await t.readFile(f), next)).toBe(true);
    expect(bytes.equal(new Uint8Array(await readFile(p)), next)).toBe(true);
  });
});

describe.skipIf(!canMount)('the zen-mount CLI with a vault and a cache', () => {
  let wv: World;
  let dir: string;
  const id = Tree.newId();
  const root = resolve(import.meta.dirname, '../../..');
  const children: ChildProcess[] = [];

  beforeAll(async () => {
    wv = await world();
    const u = await wv.session
      .fs(1)
      .init((k) => [zw.createDeviceSlot(k, wv.admin.device.devicePublic)]);
    const t = u.tree(id);
    const docs = await t.mkdir(ROOT, 'docs');
    await t.writeFile(await t.create(docs, 'a.txt'), bytes.utf8('pinned text'));
    await t.writeFile(await t.create(ROOT, 'b.txt'), bytes.utf8('never read'));
    u.close();
    dir = await mkdtemp(join(tmpdir(), 'zen-mnt-cli-'));
    execFileSync('npx', ['tsc', '-p', 'packages/fuse/tsconfig.json'], { cwd: root });
  });

  afterAll(async () => {
    for (const c of children) c.kill('SIGKILL');
    try {
      execFileSync('fusermount', ['-uz', join(dir, 'mnt')], { stdio: 'ignore' });
    } catch {
      // not mounted
    }
    if (dir) await rm(dir, { recursive: true, force: true });
    await wv?.server.stop();
  });

  /** Run the CLI in the foreground, with no secret in its environment. */
  function cli(...args: string[]) {
    const env = Object.fromEntries(
      Object.entries(process.env).filter(([k]) => !k.startsWith('ZEN_')),
    );
    const child = spawn(
      process.execPath,
      [
        join(root, 'packages/fuse/dist/cli.js'),
        wv.server.url,
        join(dir, 'mnt'),
        '--origin',
        wv.client.origin,
        '--tree',
        bytes.hex(id),
        '--vault',
        join(dir, 'vault'),
        '--cache-dir',
        join(dir, 'cache'),
        '--foreground',
        ...args,
      ],
      { env, stdio: ['ignore', 'pipe', 'pipe'] },
    );
    children.push(child);
    let err = '';
    child.stderr!.on('data', (d) => {
      err += d;
    });
    const exit = new Promise<number | null>((r) => child.once('exit', (c) => r(c)));
    return {
      exit,
      err: () => err,
      async stop() {
        child.kill('SIGTERM');
        return exit;
      },
    };
  }

  const p = (s: string) => join(dir, 'mnt', s);

  it('signs in with a device key once, then from the vault, then offline', async () => {
    // 1. The device key is stored in the vault; /docs is pinned.
    const keyFile = join(dir, 'device.json');
    await writeFile(
      keyFile,
      JSON.stringify({
        identity_seed: bytes.hex(new Uint8Array(32).fill(1)),
        device_seed: bytes.hex(new Uint8Array(32).fill(0x81)),
        cert: bytes.hex(wv.admin.cert),
      }),
    );
    let run = cli('--device-key', keyFile, '--pin', '/docs');
    expect(await eventually(() => readFile(p('docs/a.txt'), 'utf8'))).toBe('pinned text');
    expect(await run.stop()).toBe(0);
    expect(run.err()).toContain('mounted fs 1');
    await rm(keyFile);

    // 2. No secret anywhere but the vault.
    run = cli();
    expect(await eventually(() => readFile(p('docs/a.txt'), 'utf8'))).toBe('pinned text');
    expect(await run.stop()).toBe(0);

    // 3. The server is down: refused without --offline-ok, mounted from the cache with it.
    await wv.server.kill();
    run = cli();
    expect(await run.exit).toBe(1);
    expect(run.err()).toMatch(/zen-mount: /);
    run = cli('--offline-ok');
    const ls = await eventually(async () => {
      const names = await readdir(p(''));
      if (!names.length) throw new Error('not mounted yet');
      return names;
    });
    expect(ls.sort()).toEqual(['b.txt', 'docs']);
    expect(await readFile(p('docs/a.txt'), 'utf8')).toBe('pinned text');
    expect((await stat(p('b.txt'))).size).toBe(0);
    await expect(readFile(p('b.txt'))).rejects.toMatchObject({ code: 'EIO' });
    expect(await run.stop()).toBe(0);
    expect(run.err()).toContain("can't be reached");
  });
});
