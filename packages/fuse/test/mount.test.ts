// zen-mount against a spawned server: real FUSE mounts driven through
// node:fs. Skipped where FUSE can't mount (no /dev/fuse or fusermount).
import { execFileSync, spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import {
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rename,
  rm,
  rmdir,
  stat,
  unlink,
  writeFile,
} from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { bytes, Tree, type UnlockedFs, zw } from '@zen/client';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { type Mount, mount } from '../src/mount.js';

const canMount =
  process.platform === 'linux' &&
  existsSync('/dev/fuse') &&
  ['/bin/fusermount', '/usr/bin/fusermount'].some((p) => existsSync(p));
if (!canMount) console.warn('zen-mount tests skipped: FUSE is not available here');

let w: World;
let ufs: UnlockedFs;
let base: string;
const treeId = Tree.newId();
const mounts: Mount[] = [];

async function mountAt(name: string, u: UnlockedFs = ufs): Promise<Mount> {
  const m = await mount(u.tree(treeId), join(base, name), { attrTimeout: 0 });
  mounts.push(m);
  return m;
}

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
    for (const m of mounts) await m.unmount().catch(() => {});
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

    // The bytes really are on the server, sealed: read them through the API.
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
    const { open } = await import('node:fs/promises');
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
    expect(await eventually(() => readFile(join(b.mountpoint, 'shared.txt'), 'utf8'))).toBe('v1');

    // Both open the same version, then both write: siblings.
    const { open } = await import('node:fs/promises');
    const fa = await open(join(a.mountpoint, 'shared.txt'), 'r+');
    const fb = await open(join(b.mountpoint, 'shared.txt'), 'r+');
    await fa.write('from a', 0);
    await fb.write('from b', 0);
    await fa.close();
    await fb.close();
    const names = await eventually(async () => {
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
    for (const m of mounts.splice(0)) await m.unmount();
    const m = await mountAt('again');
    expect(await readFile(join(m.mountpoint, 'greeting.txt'), 'utf8')).toBe('hello again');
  });

  it('read-only mounts refuse changes', async () => {
    const m = await mount(ufs.tree(treeId), join(base, 'ro'), { readOnly: true });
    mounts.push(m);
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
