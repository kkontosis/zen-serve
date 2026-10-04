// The FUSE mapping: POSIX calls onto a zen-serve filesystem tree
// (docs/MILESTONE-4.md, step 3b). No local replica (that is milestone 5):
// directories are read from the server and cached until the tree's change
// feed reports a change; file contents are read by chunk ranges and written
// back as whole new versions when the file is closed or synced.
import { createRequire } from 'node:module';
import {
  bytes,
  type ChunkIndex,
  displayNames,
  type FileVersion,
  ROOT,
  type Tree,
  type TreeNode,
  ZenError,
} from '@zen/client';

const require = createRequire(import.meta.url);

type Cb = (err: number, ...rest: unknown[]) => void;

/** The part of @cocalc/fuse-native used here (its typings don't load as ESM). */
interface FuseInstance {
  mount(cb: (err: Error | null) => void): void;
  unmount(cb: (err: Error | null) => void): void;
}
type FuseClass = (new (
  mnt: string,
  ops: Record<string, (...args: never[]) => void>,
  opts: Record<string, unknown>,
) => FuseInstance) &
  Record<string, number>;

/** Options of `mount`. */
export interface MountOptions {
  /** Refuse every change with EROFS. */
  readOnly?: boolean;
  /** Seconds the kernel may cache attributes and names (default 1). */
  attrTimeout?: number;
  /** Log FUSE errors other than ENOENT to stderr. */
  verbose?: boolean;
}

/** A mounted tree. */
export interface Mount {
  readonly mountpoint: string;
  /** Flush open files, stop the change feed and unmount. */
  unmount(): Promise<void>;
}

/** A directory entry: a node, or a conflict copy (a sibling version of a file). */
interface Entry {
  node: TreeNode;
  /** For a conflict copy: the sibling version shown. */
  sibling?: FileVersion;
}

interface Dir {
  entries: Map<string, Entry>;
}

interface Handle {
  node: Uint8Array;
  /** The version open for reading (absent: no content yet). */
  version?: FileVersion;
  /** The versions this handle's write replaces: what it saw at open. */
  replaces: Uint8Array[];
  /** Whole contents, loaded on the first write or truncate. */
  buffer?: Uint8Array;
  size: number;
  dirty: boolean;
  readOnly: boolean;
  index?: ChunkIndex;
}

const O_TRUNC = 0o1000;
const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const S_IFLNK = 0o120000;

/** `name (conflict abcd1234-2).ext`: the name a sibling version is shown under. */
export function conflictName(name: string, device: Uint8Array, n: number): string {
  const dot = name.lastIndexOf('.');
  const tag = ` (conflict ${bytes.hex(device).slice(0, 8)}-${n})`;
  return dot > 0 ? name.slice(0, dot) + tag + name.slice(dot) : name + tag;
}

/** Mount `tree` on `mountpoint` (created if missing). */
export async function mount(
  tree: Tree,
  mountpoint: string,
  opts: MountOptions = {},
): Promise<Mount> {
  const Fuse = require('@cocalc/fuse-native') as FuseClass;
  const F = Fuse as unknown as Record<string, number>;
  const dirs = new Map<string, Dir>();
  const handles = new Map<number, Handle>();
  let nextFd = 10;
  const uid = process.getuid?.() ?? 0;
  const gid = process.getgid?.() ?? 0;
  const ro = !!opts.readOnly;

  const errno = (e: unknown): number => {
    if (typeof e === 'number') return e;
    if (e instanceof ZenError) {
      if (e.status === 404) return F.ENOENT!;
      if (e.status === 403) return F.EACCES!;
      if (e.status === 413) return F.EFBIG!;
      if (e.code === 'stale_slot') return F.EROFS!;
    }
    if (opts.verbose)
      process.stderr.write(`zen-mount: ${e instanceof Error ? e.stack : String(e)}\n`);
    return F.EIO!;
  };

  /** Run an async op and answer FUSE: `f` returns the callback's extra arguments. */
  const op =
    <A extends unknown[]>(f: (...a: A) => Promise<unknown[] | undefined>) =>
    (...args: [...A, Cb]) => {
      const cb = args.pop() as Cb;
      f(...(args as unknown as A)).then(
        (r) => cb(0, ...(r ?? [])),
        (e) => cb(errno(e)),
      );
    };

  const split = (path: string) => {
    const i = path.lastIndexOf('/');
    return { parent: path.slice(0, i) || '/', name: path.slice(i + 1) };
  };

  // ---------------------------------------------------------------- lookups

  async function readDir(id: Uint8Array): Promise<Dir> {
    const key = bytes.hex(id);
    const cached = dirs.get(key);
    if (cached) return cached;
    const gen = generation;
    const kids = (await tree.list(id)).filter((n) => n.meta);
    const names = displayNames(kids);
    const entries = new Map<string, Entry>();
    for (const n of kids) {
      entries.set(names.get(bytes.hex(n.id))!, { node: n });
      if (n.versions > 1 && n.meta!.type === 'file') {
        const vs = (await tree.versions(n.id)).sort((a, b) => bytes.compare(b.dot, a.dot));
        vs.slice(1).forEach((v, i) => {
          entries.set(conflictName(n.meta!.name, v.device, i + 2), { node: n, sibling: v });
        });
      }
    }
    const d = { entries };
    // A listing that raced a change must not be cached over the invalidation.
    if (gen === generation) dirs.set(key, d);
    return d;
  }

  async function lookup(path: string): Promise<Entry> {
    if (path === '/') {
      return {
        node: {
          id: ROOT,
          versions: 0,
          meta: { type: 'dir', name: '/', mode: 0o755, mtimeMs: Date.now() },
        } as TreeNode,
      };
    }
    let cur = ROOT;
    let entry: Entry | undefined;
    for (const part of path.split('/').slice(1)) {
      entry = (await readDir(cur)).entries.get(part);
      if (!entry) throw F.ENOENT!;
      cur = entry.node.id;
    }
    return entry!;
  }

  async function lookupDir(path: string): Promise<Uint8Array> {
    const e = await lookup(path);
    if (e.node.meta?.type !== 'dir') throw F.ENOTDIR!;
    return e.node.id;
  }

  /** Forget cached directories (all of them: changes are rare next to reads). */
  let generation = 0;
  const invalidate = () => {
    generation++;
    dirs.clear();
  };

  const sizes = new Map<string, number>();
  async function sizeOf(e: Entry): Promise<number> {
    if (e.sibling) return e.sibling.manifest.size;
    if (e.node.versions === 0) return 0;
    const key = `${bytes.hex(e.node.id)}:${bytes.hex(e.node.changed)}`;
    const known = sizes.get(key);
    if (known !== undefined) return known;
    const st = await tree.stat(e.node.id);
    const size = st?.size ?? 0;
    sizes.set(key, size);
    return size;
  }

  async function attr(e: Entry) {
    const m = e.node.meta!;
    const open = [...handles.values()].find((h) => h.dirty && bytes.equal(h.node, e.node.id));
    const kind = m.type === 'dir' ? S_IFDIR : m.type === 'symlink' ? S_IFLNK : S_IFREG;
    const mode = kind | (e.sibling ? m.mode & 0o555 : m.mode) | (ro ? 0 : 0);
    const t = new Date(m.mtimeMs);
    return {
      mode,
      uid,
      gid,
      size: m.type === 'dir' ? 4096 : open ? open.size : await sizeOf(e),
      nlink: m.type === 'dir' ? 2 : 1,
      mtime: t,
      atime: t,
      ctime: t,
      dev: 0,
      ino: 0,
      rdev: 0,
      blksize: 4096,
      blocks: 0,
    };
  }

  const mutating = () => {
    if (ro) throw F.EROFS!;
  };

  // ---------------------------------------------------------------- files

  async function load(h: Handle): Promise<Uint8Array> {
    if (!h.buffer) {
      h.buffer = h.version
        ? await tree.readFile(h.node, { version: h.version })
        : new Uint8Array(0);
      if (h.version) h.index = await tree.chunkIndex(h.node, h.version, h.buffer);
    }
    return h.buffer;
  }

  async function flush(h: Handle): Promise<void> {
    if (!h.dirty) return;
    const w = await tree.writeFile(h.node, h.buffer!.subarray(0, h.size), {
      replaces: h.replaces,
      ...(h.index ? { previous: h.index } : {}),
    });
    h.dirty = false;
    h.index = w.index;
    h.replaces = [w.dot];
    invalidate();
  }

  async function openHandle(path: string, readOnly: boolean): Promise<number> {
    const e = await lookup(path);
    if (e.node.meta?.type === 'dir') throw F.EISDIR!;
    const vs = e.sibling ? [e.sibling] : await tree.versions(e.node.id);
    const newest = [...vs].sort((a, b) => bytes.compare(b.dot, a.dot))[0];
    const fd = nextFd++;
    handles.set(fd, {
      node: e.node.id,
      ...(newest ? { version: newest } : {}),
      replaces: newest ? [newest.dot] : [],
      size: newest?.manifest.size ?? 0,
      dirty: false,
      readOnly: readOnly || !!e.sibling || ro,
    });
    return fd;
  }

  const handle = (fd: number): Handle => {
    const h = handles.get(fd);
    if (!h) throw F.EBADF!;
    return h;
  };

  // O_ACCMODE: 0 read-only, 1 write-only, 2 read-write.
  const writes = (flags: number) => (flags & 3) !== 0;

  const ops = {
    getattr: op(async (path: string) => [await attr(await lookup(path))]),
    fgetattr: op(async (path: string) => [await attr(await lookup(path))]),
    readdir: op(async (path: string) => [
      ['.', '..', ...(await readDir(await lookupDir(path))).entries.keys()],
    ]),
    statfs: op(async () => [
      {
        bsize: 4096,
        frsize: 4096,
        blocks: 2 ** 30,
        bfree: 2 ** 29,
        bavail: 2 ** 29,
        files: 2 ** 24,
        ffree: 2 ** 23,
        favail: 2 ** 23,
        fsid: 0x7a656e,
        flag: 0,
        namemax: 255,
      },
    ]),
    open: op(async (path: string, flags: number) => {
      if (writes(flags)) mutating();
      const fd = await openHandle(path, !writes(flags));
      const h = handle(fd);
      if (writes(flags) && h.readOnly) {
        handles.delete(fd);
        throw F.EACCES!;
      }
      if (writes(flags) && flags & O_TRUNC) {
        h.buffer = new Uint8Array(0);
        h.size = 0;
        h.dirty = true;
      }
      return [fd];
    }),
    create: op(async (path: string, mode: number) => {
      mutating();
      const { parent, name } = split(path);
      const dir = await lookupDir(parent);
      if ((await readDir(dir)).entries.has(name)) throw F.EEXIST!;
      const node = await tree.create(dir, name, { mode: mode & 0o7777 });
      invalidate();
      const fd = nextFd++;
      handles.set(fd, { node, replaces: [], size: 0, dirty: false, readOnly: false });
      return [fd];
    }),
    read: (
      _path: string,
      fd: number,
      buf: Buffer,
      len: number,
      pos: number,
      cb: (n: number) => void,
    ) => {
      (async () => {
        const h = handle(fd);
        const data = h.buffer
          ? h.buffer.subarray(Math.min(pos, h.size), Math.min(pos + len, h.size))
          : h.version
            ? await tree.read(h.node, h.version, pos, len)
            : new Uint8Array(0);
        buf.set(data);
        return data.length;
      })().then(cb, (e) => cb(errno(e)));
    },
    write: (
      _path: string,
      fd: number,
      buf: Buffer,
      len: number,
      pos: number,
      cb: (n: number) => void,
    ) => {
      (async () => {
        const h = handle(fd);
        if (h.readOnly) throw F.EBADF!;
        const data = await load(h);
        const end = pos + len;
        if (end > data.length) {
          const grown = new Uint8Array(Math.max(end, data.length * 2));
          grown.set(data.subarray(0, h.size));
          h.buffer = grown;
        }
        h.buffer!.set(buf.subarray(0, len), pos);
        h.size = Math.max(h.size, end);
        h.dirty = true;
        return len;
      })().then(cb, (e) => cb(errno(e)));
    },
    ftruncate: op(async (_path: string, fd: number, size: number) => {
      const h = handle(fd);
      if (h.readOnly) throw F.EBADF!;
      await truncateHandle(h, size);
      return undefined;
    }),
    truncate: op(async (path: string, size: number) => {
      mutating();
      const e = await lookup(path);
      // An open writable handle of the node takes the truncation, so it and
      // the handle's later writes become one new version, not two siblings.
      const open = [...handles.values()].find((h) => !h.readOnly && bytes.equal(h.node, e.node.id));
      if (open) {
        await truncateHandle(open, size);
        return undefined;
      }
      const fd = await openHandle(path, false);
      const h = handle(fd);
      await truncateHandle(h, size);
      await flush(h);
      handles.delete(fd);
      return undefined;
    }),
    flush: op(async (_path: string, fd: number) => {
      await flush(handle(fd));
      return undefined;
    }),
    fsync: op(async (_path: string, _datasync: boolean, fd: number) => {
      await flush(handle(fd));
      return undefined;
    }),
    release: op(async (_path: string, fd: number) => {
      const h = handles.get(fd);
      handles.delete(fd);
      if (h) await flush(h);
      return undefined;
    }),
    mkdir: op(async (path: string, mode: number) => {
      mutating();
      const { parent, name } = split(path);
      const dir = await lookupDir(parent);
      if ((await readDir(dir)).entries.has(name)) throw F.EEXIST!;
      await tree.mkdir(dir, name, { mode: mode & 0o7777 });
      invalidate();
      return undefined;
    }),
    unlink: op(async (path: string) => {
      mutating();
      const e = await lookup(path);
      if (e.node.meta?.type === 'dir') throw F.EISDIR!;
      if (e.sibling) {
        // Deleting a conflict copy resolves the conflict for the main version.
        const vs = (await tree.versions(e.node.id)).sort((a, b) => bytes.compare(b.dot, a.dot));
        await tree.resolve(e.node.id, vs[0]!);
      } else {
        await tree.remove(e.node.id);
      }
      invalidate();
      return undefined;
    }),
    rmdir: op(async (path: string) => {
      mutating();
      const id = await lookupDir(path);
      if ((await readDir(id)).entries.size) throw F.ENOTEMPTY!;
      await tree.remove(id);
      invalidate();
      return undefined;
    }),
    rename: op(async (src: string, dest: string) => {
      mutating();
      const e = await lookup(src);
      if (e.sibling) throw F.EACCES!;
      const to = split(dest);
      const dir = await lookupDir(to.parent);
      const existing = (await readDir(dir)).entries.get(to.name);
      if (existing && !bytes.equal(existing.node.id, e.node.id)) {
        if (existing.node.meta?.type === 'dir') {
          if ((await readDir(existing.node.id)).entries.size) throw F.ENOTEMPTY!;
        }
        await tree.remove(existing.node.id);
      }
      const from = split(src);
      if (from.parent === to.parent) await tree.rename(e.node.id, to.name);
      else await tree.move(e.node.id, dir, to.name);
      invalidate();
      return undefined;
    }),
    chmod: op(async (path: string, mode: number) => {
      mutating();
      const e = await lookup(path);
      await tree.setMeta(e.node.id, { mode: mode & 0o7777 });
      invalidate();
      return undefined;
    }),
    utimens: op(async (path: string, _atime: Date, mtime: Date) => {
      mutating();
      const e = await lookup(path);
      await tree.setMeta(e.node.id, { mtimeMs: new Date(mtime).getTime() });
      invalidate();
      return undefined;
    }),
    chown: op(async () => undefined),
  };

  async function truncateHandle(h: Handle, size: number) {
    const data = await load(h);
    if (size > data.length) {
      const grown = new Uint8Array(size);
      grown.set(data.subarray(0, h.size));
      h.buffer = grown;
    } else if (size > h.size) {
      h.buffer!.fill(0, h.size, size);
    }
    h.size = size;
    h.dirty = true;
  }

  const fuse = new Fuse(mountpoint, ops as unknown as Record<string, (...args: never[]) => void>, {
    force: true,
    mkdir: true,
    attrTimeout: opts.attrTimeout ?? 1,
    entryTimeout: opts.attrTimeout ?? 1,
  });
  await new Promise<void>((ok, fail) => fuse.mount((err) => (err ? fail(err) : ok())));

  // Follow the change feed: any change drops the cached directories.
  let stopped = false;
  const feed = (async () => {
    let cursor: Uint8Array | undefined;
    while (!stopped) {
      try {
        for await (const b of tree.changes(cursor, { waitMs: 5000 })) {
          if (stopped) return;
          if (b.resync || b.changes.length) invalidate();
          if (b.cursor) cursor = b.cursor;
        }
      } catch (e) {
        if (opts.verbose) process.stderr.write(`zen-mount: change feed: ${String(e)}\n`);
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
  })();

  return {
    mountpoint,
    async unmount() {
      stopped = true;
      for (const h of handles.values()) await flush(h).catch(() => {});
      handles.clear();
      await new Promise<void>((ok, fail) => fuse.unmount((err) => (err ? fail(err) : ok())));
      void feed;
    },
  };
}
