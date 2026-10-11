// The FUSE mapping: POSIX calls onto a local replica of a zen-serve tree
// (@zen/fs, spec/fs.md §11). Lookups, listings and metadata come from the
// replica's node table; reads and writes go through its file handles, which
// keep only the chunks written and queue one write per flush. Offline, what
// the replica keeps still reads, and writes queue until the server is back.
import { createRequire } from 'node:module';
import { bytes, isCode, ZenError } from '@zen/client';
import type { FileHandle, OpenFlags, Replica, Stat } from '@zen/fs';

export { conflictName } from '@zen/fs';

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

/** A mounted replica. */
export interface Mount {
  readonly mountpoint: string;
  /** Close open files (queuing their writes) and unmount. The replica stays open. */
  unmount(): Promise<void>;
}

interface Open {
  h: FileHandle;
  node: Uint8Array;
  writable: boolean;
  /** The handle's calls, one at a time. */
  chain: Promise<unknown>;
}

const O_TRUNC = 0o1000;
const O_APPEND = 0o2000;
const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const S_IFLNK = 0o120000;
const XATTR_CREATE = 1;
const XATTR_REPLACE = 2;
/** The kernel's xattr namespace that maps onto the stored names (formats.md §11.2). */
const USER = 'user.';

/** Replica error codes and their errno names. */
const ERRNO: Record<string, string> = {
  not_found: 'ENOENT',
  not_dir: 'ENOTDIR',
  is_dir: 'EISDIR',
  exists: 'EEXIST',
  not_empty: 'ENOTEMPTY',
  no_attr: 'ENODATA',
  bad_value: 'EINVAL',
  closed: 'EBADF',
  too_large: 'EFBIG',
  stale_slot: 'EROFS',
};

/** Mount `replica` on `mountpoint` (created if missing). */
export async function mount(
  replica: Replica,
  mountpoint: string,
  opts: MountOptions = {},
): Promise<Mount> {
  const Fuse = require('@cocalc/fuse-native') as FuseClass;
  const F = Fuse as unknown as Record<string, number>;
  const handles = new Map<number, Open>();
  let nextFd = 10;
  const uid = process.getuid?.() ?? 0;
  const gid = process.getgid?.() ?? 0;
  const ro = !!opts.readOnly;

  const errno = (e: unknown): number => {
    if (typeof e === 'number') return e;
    if (e instanceof ZenError) {
      const name = ERRNO[e.code];
      if (name) return F[name]!;
      if (e.status === 404) return F.ENOENT!;
      if (e.status === 403) return F.EACCES!;
      if (e.status === 413) return F.EFBIG!;
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

  const mutating = () => {
    if (ro) throw F.EROFS!;
  };

  const handle = (fd: number): Open => {
    const o = handles.get(fd);
    if (!o) throw F.EBADF!;
    return o;
  };

  /** Run `f` on a handle after its earlier calls (the kernel may send them in parallel). */
  function onHandle<T>(o: Open, f: () => Promise<T>): Promise<T> {
    const p = o.chain.then(f);
    o.chain = p.catch(() => {});
    return p;
  }

  /** An open writable handle of a node: its size is the file's until it flushes. */
  const writer = (node: Uint8Array) =>
    [...handles.values()].find((o) => o.writable && bytes.equal(o.node, node));

  async function stat(path: string): Promise<Stat> {
    try {
      return await replica.stat(path);
    } catch (e) {
      if (!isCode(e, 'offline')) throw e;
      // A file whose versions aren't kept still lists offline, with size 0;
      // reading it fails (EIO).
      const { node } = await replica.lookup(path);
      const m = node.meta;
      return {
        type: m?.type ?? 'file',
        mode: m?.mode ?? 0o644,
        size: 0,
        mtimeMs: m?.mtimeMs ?? 0,
        node: node.id,
        versions: node.versions,
        conflict: false,
      };
    }
  }

  async function attr(path: string) {
    const s = await stat(path);
    const kind = s.type === 'dir' ? S_IFDIR : s.type === 'symlink' ? S_IFLNK : S_IFREG;
    const open = s.type === 'file' && !s.conflict ? writer(s.node) : undefined;
    const t = new Date(s.mtimeMs);
    return {
      mode: kind | s.mode,
      uid,
      gid,
      size: s.type === 'dir' ? 4096 : open ? open.h.size : s.size,
      nlink: s.type === 'dir' ? 2 : 1,
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

  async function openFile(path: string, flags: OpenFlags, create?: number): Promise<number> {
    const writable = flags !== 'r';
    if (writable) {
      mutating();
      if (create === undefined && (await stat(path)).conflict) throw F.EACCES!;
    }
    const h = await replica.open(
      path,
      flags,
      create === undefined
        ? { create: false }
        : { create: true, exclusive: true, mode: create & 0o7777 },
    );
    const fd = nextFd++;
    handles.set(fd, { h, node: h.node, writable, chain: Promise.resolve() });
    return fd;
  }

  /** The stored name of a kernel xattr name, or ENOTSUP for other namespaces. */
  const xname = (name: string): string => {
    if (!name.startsWith(USER) || name.length === USER.length) throw F.EOPNOTSUPP!;
    return name.slice(USER.length);
  };

  // O_ACCMODE: 0 read-only, 1 write-only, 2 read-write.
  const flagsOf = (flags: number): OpenFlags =>
    (flags & 3) === 0 ? 'r' : flags & O_TRUNC ? 'w' : flags & O_APPEND ? 'a' : 'r+';

  const ops = {
    getattr: op(async (path: string) => [await attr(path)]),
    fgetattr: op(async (path: string) => [await attr(path)]),
    readdir: op(async (path: string) => [['.', '..', ...(await replica.readdir(path))]]),
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
    open: op(async (path: string, flags: number) => [await openFile(path, flagsOf(flags))]),
    create: op(async (path: string, mode: number) => [await openFile(path, 'w', mode)]),
    read: (
      _path: string,
      fd: number,
      buf: Buffer,
      len: number,
      pos: number,
      cb: (n: number) => void,
    ) => {
      (async () => {
        const o = handle(fd);
        const data = await onHandle(o, () => o.h.read(len, pos));
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
        const o = handle(fd);
        if (!o.writable) throw F.EBADF!;
        return onHandle(o, () => o.h.write(buf.subarray(0, len), pos));
      })().then(cb, (e) => cb(errno(e)));
    },
    ftruncate: op(async (_path: string, fd: number, size: number) => {
      const o = handle(fd);
      if (!o.writable) throw F.EBADF!;
      await onHandle(o, () => o.h.truncate(size));
      return undefined;
    }),
    truncate: op(async (path: string, size: number) => {
      mutating();
      // An open writable handle of the node takes the truncation, so it and
      // the handle's later writes become one new version, not two siblings.
      const { node } = await replica.lookup(path);
      const o = writer(node.id);
      if (o) await onHandle(o, () => o.h.truncate(size));
      else await replica.truncate(path, size);
      return undefined;
    }),
    flush: op(async (_path: string, fd: number) => {
      const o = handle(fd);
      await onHandle(o, () => o.h.flush());
      return undefined;
    }),
    fsync: op(async (_path: string, _datasync: boolean, fd: number) => {
      const o = handle(fd);
      await onHandle(o, () => o.h.flush());
      return undefined;
    }),
    release: op(async (_path: string, fd: number) => {
      const o = handles.get(fd);
      if (o) {
        try {
          await onHandle(o, () => o.h.close());
        } finally {
          handles.delete(fd);
        }
      }
      return undefined;
    }),
    mkdir: op(async (path: string, mode: number) => {
      mutating();
      await replica.mkdir(path, { mode: mode & 0o7777 });
      return undefined;
    }),
    unlink: op(async (path: string) => {
      mutating();
      // Deleting a conflict copy resolves the conflict for the main version.
      await replica.unlink(path);
      return undefined;
    }),
    rmdir: op(async (path: string) => {
      mutating();
      await replica.rmdir(path);
      return undefined;
    }),
    rename: op(async (src: string, dest: string) => {
      mutating();
      if ((await stat(src)).conflict) throw F.EACCES!;
      await replica.rename(src, dest);
      return undefined;
    }),
    link: op(async () => {
      throw F.EPERM!;
    }),
    symlink: op(async (target: string, path: string) => {
      mutating();
      await replica.symlink(target, path);
      return undefined;
    }),
    readlink: op(async (path: string) => [await replica.readlink(path)]),
    chmod: op(async (path: string, mode: number) => {
      mutating();
      await replica.chmod(path, mode);
      return undefined;
    }),
    utimens: op(async (path: string, _atime: Date, mtime: Date) => {
      mutating();
      await replica.utimes(path, new Date(mtime).getTime());
      return undefined;
    }),
    chown: op(async () => undefined),
    getxattr: op(async (path: string, name: string, _position: number) => [
      Buffer.from(await replica.getxattr(path, xname(name))),
    ]),
    listxattr: op(async (path: string) => [(await replica.listxattr(path)).map((n) => USER + n)]),
    setxattr: op(
      async (path: string, name: string, value: Buffer, _position: number, flags: number) => {
        mutating();
        const n = xname(name);
        if (flags & (XATTR_CREATE | XATTR_REPLACE)) {
          const has = (await replica.listxattr(path)).includes(n);
          if (flags & XATTR_CREATE && has) throw F.EEXIST!;
          if (flags & XATTR_REPLACE && !has) throw F.ENODATA!;
        }
        await replica.setxattr(path, n, new Uint8Array(value));
        return undefined;
      },
    ),
    removexattr: op(async (path: string, name: string) => {
      mutating();
      await replica.removexattr(path, xname(name));
      return undefined;
    }),
  };

  const fuse = new Fuse(mountpoint, ops as unknown as Record<string, (...args: never[]) => void>, {
    force: true,
    mkdir: true,
    attrTimeout: opts.attrTimeout ?? 1,
    entryTimeout: opts.attrTimeout ?? 1,
  });
  await new Promise<void>((ok, fail) => fuse.mount((err) => (err ? fail(err) : ok())));

  return {
    mountpoint,
    async unmount() {
      for (const o of handles.values()) await onHandle(o, () => o.h.close()).catch(() => {});
      handles.clear();
      await new Promise<void>((ok, fail) => fuse.unmount((err) => (err ? fail(err) : ok())));
    },
  };
}
