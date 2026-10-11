// A local replica of a tree (spec/fs.md §11): the whole node table, kept
// current through the change feed, and content on demand in a cache bounded
// by size, with pins for what must stay offline. Everything at rest is
// sealed as on the server, inside the local store's own layers.
//
// One instance per store writes the node table: the owner (a Web Lock
// across tabs, or the store's lock file in Node). Others read the store and
// reload the nodes the owner names on the tab channel.
import {
  type BroadcastChannelLike,
  bytes,
  CHUNK_SIZE,
  type ChannelFactory,
  type ChunkIndex,
  defaultChannel,
  defaultLocks,
  displayNames,
  type FileSource,
  type FileVersion,
  isCode,
  type LockManagerLike,
  Owner,
  ROOT,
  type SealedStore,
  TRASH,
  type Tree,
  type TreeChange,
  type TreeNode,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import type { Info, NodeMeta } from '@zen/wasm';
import { FileHandle, type OpenFlags, type OpenOptions } from './handle.js';
import {
  buildOverlay,
  emptyOverlay,
  isLocal,
  type LocalVersion,
  newestFirst,
  type Overlay,
  type ShownVersion,
} from './overlay.js';
import {
  decodeEntry,
  decodeJson,
  encodeEntry,
  encodeJson,
  INFLIGHT,
  isEntryKey,
  newKey,
  nodesOf,
  type QOp,
  type QueueEntry,
  type QWrite,
} from './queue.js';
import { Sender } from './sender.js';
import { Content, Records, replicaStores, useKey } from './store.js';
import { decodeXattrs, encodeXattrs } from './xattr.js';

/** Options of `openReplica`. */
export interface ReplicaOptions {
  /** Where the replica is kept. */
  local: SealedStore;
  /** The most content (sealed chunks and pages) to keep, in bytes (default 1 GiB). Pins don't count against it. */
  cacheBytes?: number;
  /** Web Locks for the owner election (default `navigator.locks`; `null`: none). */
  locks?: LockManagerLike | null;
  /** The tab channel (default `BroadcastChannel`; `null`: none). Used only with locks. */
  channel?: ChannelFactory | null;
  /** How long a change-feed poll waits, in ms (default 25,000). */
  waitMs?: number;
  /** Take part in the owner election (default true). */
  owner?: boolean;
}

export type NodeType = 'dir' | 'file' | 'symlink';

/** What `stat` returns. */
export interface Stat {
  type: NodeType;
  mode: number;
  /** Bytes of the version shown (0 for a directory). */
  size: number;
  mtimeMs: number;
  node: Uint8Array;
  /** The node's content versions (more than 1: siblings, shown as conflict copies). */
  versions: number;
  /** A conflict copy (read-only). */
  conflict: boolean;
  /** The media type (the `zen.content_type` xattr), if set. */
  contentType?: string;
}

/** A directory entry. */
export interface DirEntry {
  name: string;
  type: NodeType;
}

/** What `status` returns. */
export interface ReplicaStatus {
  online: boolean;
  owner: boolean;
  nodes: number;
  /** Kept content, in bytes. */
  cacheBytes: number;
  pins: number;
  /** Local writes not yet sent (entries). */
  queued: number;
  /** A batch of them is being sent. */
  sending: boolean;
}

/** Options of the write calls. */
export interface WriteCallOptions {
  /** Resolve once the server has the write, not once it is queued. */
  wait?: boolean;
}

/** Options of `writeFile`. */
export interface WriteFileOptions extends WriteCallOptions {
  /** POSIX mode of a new file (default 0o644). */
  mode?: number;
  /** The media type, kept in the `zen.content_type` xattr. */
  contentType?: string;
  /** Create the file when missing (default true). */
  create?: boolean;
}

/** A path's target: a node, or one of its sibling versions shown as a conflict copy. */
export interface Entry {
  node: TreeNode;
  sibling?: ShownVersion;
}

const DEFAULT_CACHE = 1 << 30;
const DEFAULT_WAIT = 25_000;
const BACKOFF_MIN = 250;
const BACKOFF_MAX = 30_000;
const FETCHERS = 2;
const CURSOR = bytes.utf8('cursor');
const FULL = bytes.utf8('full');
const INFO = bytes.utf8('info');
/** The xattr holding a node's media type (formats.md §11.2). */
export const CONTENT_TYPE = 'zen.content_type';

/** `name (conflict abcd1234-2).ext`: the name a sibling version is shown under (fs.md §11). */
export function conflictName(name: string, device: Uint8Array, n: number): string {
  const dot = name.lastIndexOf('.');
  const tag = ` (conflict ${bytes.hex(device).slice(0, 8)}-${n})`;
  return dot > 0 ? name.slice(0, dot) + tag + name.slice(dot) : name + tag;
}

const notFound = (path: string) =>
  new ZenError(404, 'not_found', `no such file or directory: ${path}`);
const offline = (what: string) =>
  new ZenError(0, 'offline', `${what} isn't kept and the server can't be reached`);
const bad = (why: string) => new ZenError(0, 'bad_value', why);

/** A path's parent path and last name. */
function splitPath(path: string): { parent: string; name: string } {
  const segs = path.split('/').filter(Boolean);
  const name = segs.pop();
  if (!name || name === '.' || name === '..') throw bad(`not a name: ${path}`);
  return { parent: `/${segs.join('/')}`, name };
}

const typeOf = (n: TreeNode): NodeType =>
  bytes.equal(n.id, ROOT) ? 'dir' : (n.meta?.type ?? 'file');

/** Open (or create) the replica of tree `treeId` on `opts.local`. */
export function openReplica(
  fs: UnlockedFs,
  treeId: Uint8Array,
  opts: ReplicaOptions,
): Promise<Replica> {
  return Replica.open(fs, treeId, opts);
}

export class Replica {
  readonly tree: Tree;
  /** False while the server can't be reached (the owner's last feed poll failed). */
  online = true;
  private readonly rec: Records;
  private readonly content: Content;
  private readonly nodes = new Map<string, TreeNode>();
  private readonly kids = new Map<string, Set<string>>();
  private readonly pins = new Map<string, boolean>();
  private readonly listeners = new Set<(ids: Uint8Array[]) => void>();
  private owner!: Owner;
  private chan: BroadcastChannelLike | undefined;
  private release: (() => Promise<void>) | undefined;
  private cursor: Uint8Array | undefined;
  private full = false;
  private seen = new Set<string>();
  private stopped = false;
  private running = false;
  private catching: Promise<void> = Promise.resolve();
  private readonly fetchQueue: Uint8Array[] = [];
  private fetchers = 0;
  private idleWaiters: (() => void)[] = [];
  private evicting = false;
  private readonly cacheBytes: number;
  private readonly waitMs: number;
  private readonly root: TreeNode;
  // Local writes: the queue, entries sent but not yet caught up with, and what they show.
  private queue: QueueEntry[] = [];
  private settling: QueueEntry[] = [];
  private ov: Overlay = emptyOverlay();
  private readonly sentDots = new Map<string, Uint8Array>();
  /** Dots of sent entries (by key), for later writes that replace them. */
  readonly learned = new Map<string, Uint8Array>();
  private readonly locals = new Map<string, LocalVersion>();
  /** Content sealed by a call that hasn't queued it yet (never evicted). */
  private readonly protect = new Set<string>();
  private readonly waiters = new Map<string, { ok: () => void; fail: (e: unknown) => void }[]>();
  private flushWaiters: { ok: () => void; fail: (e: unknown) => void }[] = [];
  private readonly errorListeners = new Set<(e: unknown, entries: string[]) => void>();
  private serialChain: Promise<unknown> = Promise.resolve();
  private recvChain: Promise<void> = Promise.resolve();
  private sender!: Sender;
  private infoP: Promise<Info> | undefined;

  private constructor(
    readonly fs: UnlockedFs,
    readonly id: Uint8Array,
    opts: ReplicaOptions,
  ) {
    this.cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE;
    this.waitMs = opts.waitMs ?? DEFAULT_WAIT;
    this.rec = new Records(opts.local, `zfs/${fs.id}/${bytes.hex(id)}`);
    this.content = new Content(this.rec, () => this.maybeEvict());
    this.tree = fs.tree(id, {
      chunkCache: this.content.chunks,
      pageCache: this.content.pages,
      info: () => this.info(),
    });
    this.root = {
      id: ROOT,
      versions: 0,
      changed: new Uint8Array(12),
      state: {
        node: ROOT,
        move_hlc: 0n,
        move_device: new Uint8Array(0),
        versions: 0,
        changed: new Uint8Array(12),
      },
    } as TreeNode;
  }

  static async open(fs: UnlockedFs, treeId: Uint8Array, opts: ReplicaOptions): Promise<Replica> {
    const r = new Replica(fs, treeId, opts);
    const locks = opts.locks === undefined ? defaultLocks() : (opts.locks ?? undefined);
    const lockName = `zfs/${fs.id}/${bytes.hex(treeId)}`;
    // Without Web Locks a store has one replica: the store's lock says which.
    if (!locks && opts.local.raw.lock && opts.owner !== false) {
      r.release = await opts.local.raw.lock(`zfs-${fs.id}-${bytes.hex(treeId)}`);
    }
    try {
      await r.load();
    } catch (e) {
      await r.release?.();
      throw e;
    }
    const factory = opts.channel === undefined ? defaultChannel() : (opts.channel ?? undefined);
    if (locks && factory) {
      r.chan = factory(lockName);
      r.chan.onmessage = (ev) => {
        r.recvChain = r.recvChain.then(() => r.recv(ev.data as Msg)).catch(() => {});
      };
    }
    r.sender = new Sender({
      tree: r.tree,
      rec: r.rec,
      learned: r.learned,
      entries: () => r.queue,
      running: () => !r.stopped && r.owner.isOwner,
      done: (es, dots) => r.done(es, dots),
      failed: (es, e) => r.failed(es, e),
      reachable: (ok, e) => r.reachable(ok, e),
    });
    r.owner = new Owner(locks, lockName, opts.owner !== false);
    r.owner.onOwner(() => {
      void r.run();
      void r.loadQueue().then(() => r.sender.start());
    });
    return r;
  }

  /** Whether this instance follows the feed and writes the node table. */
  get isOwner(): boolean {
    return this.owner.isOwner;
  }

  /** Call `fn` with the ids of nodes that changed (here or, for other tabs, in the owner). */
  onChange(fn: (ids: Uint8Array[]) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  // ------------------------------------------------------------ the node table

  private async load(): Promise<void> {
    for (const [, v] of await this.rec.list('nodes')) {
      try {
        this.setNode(this.tree.nodeOf(zw.decodeNodeState(v)));
      } catch {
        // A record that doesn't decode: the next full sync brings it back.
      }
    }
    for (const [k, v] of await this.rec.list('pins')) this.pins.set(k, v[0] === 1);
    this.cursor = await this.rec.get('sync', CURSOR);
    this.full = !this.cursor || (await this.rec.get('sync', FULL)) !== undefined;
    await this.loadQueue();
  }

  /** The server's `/v1/info`, kept for offline use. */
  private info(): Promise<Info> {
    this.infoP ??= (async () => {
      try {
        const info = await this.fs.session.client.info();
        this.rec.put('sync', INFO, zw.encodeInfo(info));
        return info;
      } catch (e) {
        const kept = await this.rec.get('sync', INFO);
        if (kept && isCode(e, 'network')) {
          this.infoP = undefined; // ask again next time
          return zw.decodeInfo(kept);
        }
        this.infoP = undefined;
        throw e;
      }
    })();
    return this.infoP;
  }

  private setNode(n: TreeNode): void {
    const key = bytes.hex(n.id);
    const old = this.nodes.get(key);
    if (old?.parent) this.kids.get(bytes.hex(old.parent))?.delete(key);
    this.nodes.set(key, n);
    if (n.parent) {
      const p = bytes.hex(n.parent);
      const set = this.kids.get(p) ?? new Set<string>();
      this.kids.set(p, set);
      set.add(key);
    }
  }

  private dropNode(key: string): void {
    const old = this.nodes.get(key);
    if (!old) return;
    if (old.parent) this.kids.get(bytes.hex(old.parent))?.delete(key);
    this.nodes.delete(key);
  }

  /** Apply feed changes (newer offsets win, so overlapping polls are harmless). */
  private apply(changes: TreeChange[]): Uint8Array[] {
    this.online = true; // the feed answered
    const changed: Uint8Array[] = [];
    for (const c of changes) {
      const key = bytes.hex(c.node);
      if (this.full) this.seen.add(key);
      const old = this.nodes.get(key);
      if (old && bytes.compare(old.changed, c.offset) >= 0) continue;
      if (old && old.versions > 0) this.rec.delete('versions', bytes.concat(old.id, old.changed));
      if (c.state) {
        this.setNode(c.state);
        this.rec.put('nodes', c.node, zw.encodeNodeState(c.state.state));
        if (c.state.versions > 0 && this.pinned(c.state)) this.queueFetch(c.state.id);
      } else {
        this.dropNode(key);
        this.rec.delete('nodes', c.node);
      }
      changed.push(c.node);
    }
    return changed;
  }

  private saveCursor(c: Uint8Array | undefined): void {
    if (!c || (this.cursor && bytes.compare(c, this.cursor) <= 0)) return;
    this.cursor = c;
    this.rec.put('sync', CURSOR, c);
  }

  private async announce(ids: Uint8Array[]): Promise<void> {
    if (!ids.length) return;
    this.rebuild();
    for (const fn of this.listeners) fn(ids);
    // Other tabs read the store: they hear of the nodes once written.
    await this.rec.flush();
    this.chan?.postMessage({ t: 'nodes', ids: ids.map((i) => bytes.hex(i)) });
  }

  /** Catch up with the feed (a full sync when there is no cursor), serialized. */
  private catchUp(): Promise<void> {
    this.catching = this.catching
      .catch(() => {})
      .then(async () => {
        if (this.full) {
          this.rec.put('sync', FULL, new Uint8Array([1]));
          this.seen = new Set();
        }
        const from = this.full ? undefined : this.cursor;
        for await (const b of this.tree.changes(from)) {
          if (b.resync) {
            this.full = true;
            this.seen = new Set();
            this.rec.put('sync', FULL, new Uint8Array([1]));
            continue;
          }
          const ids = this.apply(b.changes);
          this.saveCursor(b.cursor);
          await this.announce(ids);
        }
        this.online = true;
        if (this.full) {
          // Nodes a full sync didn't return are gone.
          const gone: Uint8Array[] = [];
          for (const key of [...this.nodes.keys()]) {
            if (this.seen.has(key)) continue;
            const n = this.nodes.get(key)!;
            if (n.versions > 0) this.rec.delete('versions', bytes.concat(n.id, n.changed));
            this.dropNode(key);
            this.rec.delete('nodes', bytes.fromHex(key));
            gone.push(bytes.fromHex(key));
          }
          this.full = false;
          this.seen = new Set();
          this.rec.delete('sync', FULL);
          await this.announce(gone);
        }
      });
    return this.catching;
  }

  /** The owner's loop: catch up, then long-poll, until closed or no longer owner. */
  private async run(): Promise<void> {
    if (this.running) return;
    this.running = true;
    let backoff = BACKOFF_MIN;
    try {
      while (!this.stopped && this.owner.isOwner) {
        try {
          await this.catchUp();
          this.schedulePins();
          backoff = BACKOFF_MIN;
          for await (const b of this.tree.changes(this.cursor, { waitMs: this.waitMs })) {
            if (this.stopped) return;
            if (b.resync) {
              this.full = true;
              break;
            }
            const ids = this.apply(b.changes);
            this.saveCursor(b.cursor);
            await this.announce(ids);
          }
        } catch (e) {
          if (this.stopped) return;
          if (!isCode(e, 'network')) console.warn(`zen-fs: sync failed: ${(e as Error).message}`);
          this.online = false;
          await new Promise((r) => setTimeout(r, backoff));
          backoff = Math.min(backoff * 2, BACKOFF_MAX);
        }
      }
    } finally {
      this.running = false;
    }
  }

  /**
   * Bring the node table up to date with the server now. The owner catches
   * up; another tab reloads what the owner announced meanwhile (nothing to do).
   */
  async sync(): Promise<void> {
    if (!this.owner.isOwner) return;
    try {
      await this.catchUp();
    } catch (e) {
      if (isCode(e, 'network')) this.online = false;
      throw e;
    }
  }

  private async recv(m: Msg): Promise<void> {
    if (this.owner.isOwner) {
      if (m.t === 'pins') await this.reloadPins().then(() => this.schedulePins());
      if (m.t === 'queued') {
        await this.loadQueue();
        this.sender.wake();
      }
      return;
    }
    if (m.t === 'nodes' && m.ids) await this.reload(m.ids);
    else if (m.t === 'queued') await this.loadQueue();
    else if (m.t === 'sent' && m.keys && m.dots) {
      // Committed: shown as before until the owner has caught up ('settled').
      for (const [k, d] of m.dots) {
        this.learned.set(k, bytes.fromHex(d));
        this.sentDots.set(k, bytes.fromHex(d));
      }
      const keys = new Set(m.keys);
      this.settling.push(...this.queue.filter((e) => keys.has(e.key)));
      this.queue = this.queue.filter((e) => !keys.has(e.key));
      this.rebuild();
      for (const k of m.keys) this.settle(k);
    } else if (m.t === 'settled' && m.keys) {
      const keys = new Set(m.keys);
      this.settling = this.settling.filter((e) => !keys.has(e.key));
      for (const k of m.keys) this.sentDots.delete(k);
      await this.loadQueue();
    } else if (m.t === 'failed' && m.keys) {
      const err = new ZenError(0, m.code ?? 'failed', m.message ?? 'refused');
      for (const k of m.keys) this.settle(k, err);
      await this.loadQueue();
    } else if (m.t === 'offline') {
      this.online = false;
      this.failFlush(new ZenError(0, 'offline', m.message ?? "the server can't be reached"));
    }
  }

  private async reload(ids: string[]): Promise<void> {
    const out: Uint8Array[] = [];
    for (const key of ids) {
      const id = bytes.fromHex(key);
      const v = await this.rec.get('nodes', id);
      if (v) this.setNode(this.tree.nodeOf(zw.decodeNodeState(v)));
      else this.dropNode(key);
      out.push(id);
    }
    this.rebuild();
    for (const fn of this.listeners) fn(out);
  }

  // ------------------------------------------------------------ paths

  private visible(n: TreeNode): boolean {
    return !!n.state.meta;
  }

  private childrenOf(dir: TreeNode): TreeNode[] {
    const out: TreeNode[] = [];
    const d = bytes.hex(dir.id);
    const keys = new Set(this.kids.get(d));
    for (const k of this.ov.kids.get(d) ?? []) keys.add(k);
    for (const k of keys) {
      const n = this.ov.nodes.get(k) ?? this.nodes.get(k);
      if (n?.parent && bytes.equal(n.parent, dir.id) && this.visible(n)) out.push(n);
    }
    return out;
  }

  /** A directory's entries by shown name: display names (fs.md §8) and conflict copies. */
  async entries(dir: TreeNode): Promise<Map<string, Entry>> {
    const kids = this.childrenOf(dir);
    const names = displayNames(kids);
    const out = new Map<string, Entry>();
    for (const n of kids) {
      const name = names.get(bytes.hex(n.id))!;
      out.set(name, { node: n });
      if (n.versions > 1 && typeOf(n) !== 'dir') {
        let vs: ShownVersion[];
        try {
          vs = await this.versionsOf(n);
        } catch (e) {
          if (isCode(e, 'offline')) continue; // copies show once their versions are known
          throw e;
        }
        vs = [...vs].sort(newestFirst);
        vs.slice(1).forEach((v, i) => {
          out.set(conflictName(n.meta?.name ?? name, v.device, i + 2), { node: n, sibling: v });
        });
      }
    }
    return out;
  }

  /** The entry at `path` (absolute, `/`-separated), by shown names. */
  async lookup(path: string): Promise<Entry> {
    let cur: Entry = { node: this.root };
    for (const seg of path.split('/').filter(Boolean)) {
      if (cur.sibling || typeOf(cur.node) !== 'dir') {
        throw new ZenError(0, 'not_dir', `not a directory: ${path}`);
      }
      const next = (await this.entries(cur.node)).get(seg);
      if (!next) throw notFound(path);
      cur = next;
    }
    return cur;
  }

  /** The node at `path`, or undefined. */
  async nodeAt(path: string): Promise<TreeNode | undefined> {
    try {
      return (await this.lookup(path)).node;
    } catch (e) {
      if (isCode(e, 'not_found') || isCode(e, 'not_dir')) return undefined;
      throw e;
    }
  }

  /** A node by id, as shown (queued writes applied). */
  node(id: Uint8Array): TreeNode | undefined {
    if (bytes.equal(id, ROOT)) return this.root;
    const k = bytes.hex(id);
    return this.ov.nodes.get(k) ?? this.nodes.get(k);
  }

  // ------------------------------------------------------------ content

  /**
   * A node's versions as shown, in dot order with a local version last:
   * the server's, less those queued writes replace, plus the last queued
   * write's.
   */
  async versionsOf(n: TreeNode): Promise<ShownVersion[]> {
    const k = bytes.hex(n.id);
    const f = this.ov.files.get(k);
    const server = this.nodes.get(k);
    if (!f) return server ? this.serverVersions(server) : [];
    let vs: FileVersion[] = [];
    try {
      if (server) vs = await this.serverVersions(server);
    } catch (e) {
      if (!isCode(e, 'offline')) throw e;
    }
    return [...vs.filter((v) => !f.replaced.has(bytes.hex(v.dot))), f.local];
  }

  /** A node's server versions, kept per change offset; fetched (and kept) on a miss. */
  private async serverVersions(n: TreeNode): Promise<FileVersion[]> {
    if (n.versions === 0) return [];
    const key = bytes.concat(n.id, n.changed);
    let raw = await this.rec.get('versions', key);
    if (!raw) {
      raw = await this.net(`the versions of ${bytes.hex(n.id)}`, () =>
        this.tree.fetchVersions(n.id),
      );
      this.rec.put('versions', key, raw);
    }
    return this.tree.openVersions(n.id, zw.decodeVersions(raw));
  }

  private async net<T>(what: string, f: () => Promise<T>): Promise<T> {
    try {
      return await f();
    } catch (e) {
      if (isCode(e, 'network')) throw offline(what);
      throw e;
    }
  }

  /** The version a path shows: the conflict copy's, else the newest. */
  async shown(e: Entry): Promise<ShownVersion | undefined> {
    if (e.sibling) return e.sibling;
    return (await this.versionsOf(e.node)).at(-1);
  }

  private async file(path: string): Promise<{ e: Entry; v: ShownVersion | undefined }> {
    const e = await this.lookup(path);
    if (typeOf(e.node) === 'dir') throw new ZenError(0, 'is_dir', `a directory: ${path}`);
    return { e, v: await this.shown(e) };
  }

  // ------------------------------------------------------------ reads

  async stat(path: string): Promise<Stat> {
    const e = await this.lookup(path);
    const type = typeOf(e.node);
    const v = type === 'dir' ? undefined : await this.shown(e);
    const meta = e.node.meta;
    const out: Stat = {
      type,
      mode: (meta?.mode ?? (type === 'dir' ? 0o755 : 0o644)) & (e.sibling ? 0o555 : 0o7777),
      size: v?.size ?? 0,
      mtimeMs: meta?.mtimeMs ?? 0,
      node: e.node.id,
      versions: this.ov.files.has(bytes.hex(e.node.id))
        ? (await this.versionsOf(e.node)).length
        : e.node.versions,
      conflict: !!e.sibling,
    };
    const ct = decodeXattrs(meta?.xattrs).get(CONTENT_TYPE);
    if (ct) out.contentType = bytes.fromUtf8(ct);
    return out;
  }

  async readdir(path: string): Promise<string[]>;
  async readdir(path: string, opts: { withTypes: true }): Promise<DirEntry[]>;
  async readdir(path: string, opts?: { withTypes?: boolean }): Promise<string[] | DirEntry[]> {
    const e = await this.lookup(path);
    if (e.sibling || typeOf(e.node) !== 'dir') {
      throw new ZenError(0, 'not_dir', `not a directory: ${path}`);
    }
    const all = [...(await this.entries(e.node))].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    if (!opts?.withTypes) return all.map(([name]) => name);
    return all.map(([name, x]) => ({ name, type: typeOf(x.node) }));
  }

  async readFile(path: string): Promise<Uint8Array> {
    const { e, v } = await this.file(path);
    if (!v) return new Uint8Array(0);
    return this.net(path, () => this.tree.read(e.node.id, v, 0, v.size));
  }

  /** `length` bytes at `offset` (shorter at the end). */
  async read(path: string, offset: number, length: number): Promise<Uint8Array> {
    const { e, v } = await this.file(path);
    if (!v) return new Uint8Array(0);
    return this.net(path, () => this.tree.read(e.node.id, v, offset, length));
  }

  /** A stream of bytes `start..=end` (both optional, as Node's `createReadStream`). */
  async createReadStream(
    path: string,
    opts: { start?: number; end?: number } = {},
  ): Promise<ReadableStream<Uint8Array>> {
    const { e, v } = await this.file(path);
    const start = opts.start ?? 0;
    const size = v?.size ?? 0;
    const end = Math.min(size, opts.end === undefined ? size : opts.end + 1);
    if (!v || start >= end) return new ReadableStream({ start: (c) => c.close() });
    const inner = this.tree.readStream(e.node.id, {
      version: v,
      offset: start,
      length: end - start,
    });
    const reader = inner.getReader();
    return new ReadableStream<Uint8Array>({
      pull: async (ctl) => {
        try {
          const r = await reader.read();
          if (r.done) ctl.close();
          else ctl.enqueue(r.value);
        } catch (err) {
          ctl.error(isCode(err, 'network') ? offline(path) : err);
        }
      },
      cancel: (why) => reader.cancel(why),
    });
  }

  /** A symlink's target. */
  async readlink(path: string): Promise<string> {
    const { e, v } = await this.file(path);
    if (typeOf(e.node) !== 'symlink') throw new ZenError(0, 'bad_value', `not a symlink: ${path}`);
    if (!v) return '';
    return bytes.fromUtf8(await this.net(path, () => this.tree.read(e.node.id, v, 0, v.size)));
  }

  /** The names of a node's extended attributes. */
  async listxattr(path: string): Promise<string[]> {
    const e = await this.lookup(path);
    return [...decodeXattrs(e.node.meta?.xattrs).keys()].sort();
  }

  /** An extended attribute's value; `no_attr` when absent. */
  async getxattr(path: string, name: string): Promise<Uint8Array> {
    const e = await this.lookup(path);
    const v = decodeXattrs(e.node.meta?.xattrs).get(name);
    if (!v) throw new ZenError(0, 'no_attr', `no attribute ${name} on ${path}`);
    return v;
  }

  // ------------------------------------------------------------ pins

  private pinned(n: TreeNode): boolean {
    let cur: TreeNode | undefined = n;
    let self = true;
    for (let depth = 0; cur && depth < 1100; depth++) {
      const p = this.pins.get(bytes.hex(cur.id));
      if (p !== undefined && (self || p)) return true;
      self = false;
      cur = cur.parent ? this.node(cur.parent) : undefined;
    }
    return false;
  }

  /**
   * Keep `path`'s content offline: a file's current versions, or with a
   * folder (`recursive`, the default for one) every file under it, now and
   * later. Pinned content is fetched by the owner and never evicted.
   */
  async pin(path: string, opts: { recursive?: boolean } = {}): Promise<void> {
    const e = await this.lookup(path);
    if (e.sibling) throw new ZenError(0, 'bad_value', 'pin the file, not a conflict copy');
    const recursive = opts.recursive ?? typeOf(e.node) === 'dir';
    this.pins.set(bytes.hex(e.node.id), recursive);
    this.rec.put('pins', e.node.id, new Uint8Array([recursive ? 1 : 0]));
    await this.rec.flush();
    if (this.owner.isOwner) this.schedulePins();
    else this.chan?.postMessage({ t: 'pins' });
  }

  async unpin(path: string): Promise<void> {
    const e = await this.lookup(path);
    this.pins.delete(bytes.hex(e.node.id));
    this.rec.delete('pins', e.node.id);
    await this.rec.flush();
    if (!this.owner.isOwner) this.chan?.postMessage({ t: 'pins' });
  }

  private async reloadPins(): Promise<void> {
    this.pins.clear();
    for (const [k, v] of await this.rec.list('pins')) this.pins.set(k, v[0] === 1);
  }

  /** Queue every pinned file's content. */
  private schedulePins(): void {
    for (const n of this.nodes.values()) {
      if (n.versions > 0 && this.pinned(n)) this.queueFetch(n.id);
    }
  }

  private queueFetch(id: Uint8Array): void {
    if (this.fetchQueue.some((x) => bytes.equal(x, id))) return;
    this.fetchQueue.push(id);
    while (this.fetchers < FETCHERS && this.fetchQueue.length) {
      this.fetchers++;
      void this.fetchLoop();
    }
  }

  private async fetchLoop(): Promise<void> {
    try {
      for (let id = this.fetchQueue.shift(); id; id = this.fetchQueue.shift()) {
        if (this.stopped) return;
        const n = this.node(id);
        if (!n || n.versions === 0) continue;
        try {
          for (const v of await this.versionsOf(n)) {
            if (!v.chunkCount) continue;
            const ids = await this.tree.chunkIds(v, 0, v.chunkCount - 1);
            await this.tree.fetchChunks(ids, v);
          }
        } catch (e) {
          if (!isCode(e, 'offline') && !isCode(e, 'network')) {
            console.warn(`zen-fs: pinned content not fetched: ${(e as Error).message}`);
          }
        }
      }
    } finally {
      this.fetchers--;
      if (!this.fetchers) {
        const ws = this.idleWaiters;
        this.idleWaiters = [];
        for (const w of ws) w();
      }
    }
  }

  /** Resolves once no pinned content is being fetched. */
  idle(): Promise<void> {
    if (!this.fetchers) return Promise.resolve();
    return new Promise((r) => this.idleWaiters.push(r));
  }

  // ------------------------------------------------------------ eviction

  private maybeEvict(): void {
    if (!this.owner?.isOwner || this.evicting) return;
    if (this.content.added < Math.max(1, this.cacheBytes / 16)) return;
    this.evicting = true;
    setTimeout(() => {
      void this.evict()
        .catch((e) => console.warn(`zen-fs: eviction failed: ${(e as Error).message}`))
        .finally(() => {
          this.evicting = false;
        });
    }, 0);
  }

  /** Evict content past `cacheBytes`, keeping pinned files' current content. Returns the total left. */
  async evict(): Promise<number> {
    const keep = new Set<string>();
    for (const n of this.nodes.values()) {
      if (n.versions === 0 || !this.pinned(n)) continue;
      const raw = await this.rec.get('versions', bytes.concat(n.id, n.changed));
      if (!raw) continue;
      for (const v of this.tree.openVersions(n.id, zw.decodeVersions(raw))) {
        for (const p of v.pages) keep.add(useKey('page', p));
        for (const c of v.chunks) keep.add(useKey('chunk', c));
        if (v.pages.length && v.chunkCount) {
          try {
            for (const c of await this.tree.chunkIds(v, 0, v.chunkCount - 1)) {
              keep.add(useKey('chunk', c));
            }
          } catch {
            // Pages not kept: their chunks can't be named, and aren't kept either.
          }
        }
      }
    }
    // Queued content isn't on the server yet.
    for (const e of [...this.settling, ...this.queue]) {
      for (const c of e.chunks) keep.add(useKey('chunk', c));
      for (const p of e.pages) keep.add(useKey('page', p));
    }
    for (const k of this.protect) keep.add(k);
    await this.rec.flush();
    const left = await this.content.evict(this.cacheBytes, keep);
    await this.rec.flush();
    return left;
  }

  // ------------------------------------------------------------ the queue

  /** Read the queue from the store (entries of every tab), in order. */
  private async loadQueue(): Promise<void> {
    const out: QueueEntry[] = [];
    const settling = new Set(this.settling.map((e) => e.key));
    for (const [k, v] of await this.rec.list('queue')) {
      if (settling.has(k) || !isEntryKey(bytes.fromHex(k))) continue;
      const e = decodeEntry(k, v);
      if (e) out.push(e);
    }
    out.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
    this.queue = out;
    this.rebuild();
    this.checkFlush();
  }

  /** Recompute what queued writes show. */
  private rebuild(): void {
    const q = [...this.settling, ...this.queue];
    if (!q.length) {
      this.ov = emptyOverlay();
      this.locals.clear();
      return;
    }
    this.ov = buildOverlay(
      this.tree,
      (k) => this.nodes.get(k),
      q,
      this.sentDots,
      this.learned,
      (e, op) => this.localVersion(e, op),
    );
  }

  /** A queued write as a version (zero dot, `local`). */
  private localVersion(e: QueueEntry, op: QWrite): LocalVersion {
    const key = `${e.key}:${bytes.hex(op.node)}`;
    let v = this.locals.get(key);
    if (!v) {
      const [opened] = this.tree.openVersions(op.node, {
        read_version: 0n,
        versions: [
          {
            dot: new Uint8Array(12),
            device: this.fs.session.deviceFp ?? new Uint8Array(0),
            chunks: op.chunks,
            pages: op.pages ?? [],
            manifest: op.manifest,
          },
        ],
      });
      v = {
        ...opened!,
        local: true,
        entry: e.key,
        replaces: op.replaces,
        ...(op.index ? { index: op.index } : {}),
      };
      this.locals.set(key, v);
    }
    return v;
  }

  /** Call `fn` when queued writes are refused for good (they are dropped). */
  onError(fn: (e: unknown, entries: string[]) => void): () => void {
    this.errorListeners.add(fn);
    return () => this.errorListeners.delete(fn);
  }

  private settle(key: string, err?: unknown): void {
    const ws = this.waiters.get(key);
    if (!ws) return;
    this.waiters.delete(key);
    for (const w of ws) err ? w.fail(err) : w.ok();
  }

  private checkFlush(): void {
    if (this.queue.length || this.settling.length) return;
    const ws = this.flushWaiters;
    this.flushWaiters = [];
    for (const w of ws) w.ok();
  }

  private failFlush(e: unknown): void {
    const ws = this.flushWaiters;
    this.flushWaiters = [];
    for (const w of ws) w.fail(e);
  }

  private reachable(ok: boolean, e?: unknown): void {
    if (ok) return;
    if (isCode(e, 'network') || isCode(e, 'commit_unknown')) {
      this.online = false;
      const err = new ZenError(
        0,
        'offline',
        "queued writes can't be sent: the server can't be reached",
      );
      this.failFlush(err);
      this.chan?.postMessage({ t: 'offline', message: err.message });
    }
  }

  /** The sender's report: `entries` are committed. */
  private async done(entries: QueueEntry[], dots: Map<string, Uint8Array>): Promise<void> {
    const keys = new Set(entries.map((e) => e.key));
    for (const [k, d] of dots) {
      this.learned.set(k, d);
      this.sentDots.set(k, d);
    }
    // Indexes of what was written, for later writes' chunk reuse.
    for (const e of entries) {
      const d = dots.get(e.key);
      for (const o of e.ops) {
        if (d && o.op === 'write' && o.index) {
          this.rec.put('index', bytes.concat(o.node, d), encodeJson(o.index));
        }
      }
    }
    // Later writes naming these entries replace their dots.
    for (const e of this.queue) {
      if (keys.has(e.key)) continue;
      let changed = false;
      for (const o of e.ops) {
        if (o.op !== 'write' || !o.after?.length) continue;
        const left: string[] = [];
        for (const a of o.after) {
          const d = dots.get(a);
          if (d) {
            o.replaces = [...o.replaces, d];
            changed = true;
          } else if (keys.has(a)) changed = true;
          else left.push(a);
        }
        if (left.length) o.after = left;
        else delete o.after;
      }
      if (changed) this.rec.put('queue', bytes.fromHex(e.key), encodeEntry(e));
    }
    // The entries leave the store with the commit in flight, at once.
    for (const k of keys) this.rec.delete('queue', bytes.fromHex(k));
    this.rec.delete('queue', INFLIGHT);
    await this.rec.flush();
    this.queue = this.queue.filter((e) => !keys.has(e.key));
    this.settling.push(...entries);
    this.rebuild();
    this.chan?.postMessage({
      t: 'sent',
      keys: [...keys],
      dots: [...dots].map(([k, d]) => [k, bytes.hex(d)]),
    });
    // They show as written until the node table has the server's state.
    try {
      await this.catchUp();
    } catch {
      // Offline again: the next catch-up brings it.
    }
    this.settling = this.settling.filter((e) => !keys.has(e.key));
    for (const k of keys) this.sentDots.delete(k);
    this.rebuild();
    // Keep the versions just written: a later write offline replaces them.
    for (const e of entries) {
      for (const o of e.ops) {
        const n = o.op === 'write' ? this.nodes.get(bytes.hex(o.node)) : undefined;
        if (n) await this.serverVersions(n).catch(() => {});
      }
    }
    const ids = entries.flatMap(nodesOf);
    for (const fn of this.listeners) fn(ids);
    this.chan?.postMessage({ t: 'settled', keys: [...keys] });
    for (const k of keys) this.settle(k);
    this.checkFlush();
  }

  /** The sender's report: `entries` were refused; they are dropped. */
  private async failed(entries: QueueEntry[], e: unknown): Promise<void> {
    const keys = entries.map((x) => x.key);
    for (const k of keys) this.rec.delete('queue', bytes.fromHex(k));
    this.rec.delete('queue', INFLIGHT);
    await this.rec.flush();
    const set = new Set(keys);
    this.queue = this.queue.filter((x) => !set.has(x.key));
    this.rebuild();
    const ids = entries.flatMap(nodesOf);
    for (const fn of this.listeners) fn(ids);
    console.warn(`zen-fs: queued writes refused: ${(e as Error).message}`);
    for (const fn of this.errorListeners) fn(e, keys);
    const z = e instanceof ZenError ? e : undefined;
    this.chan?.postMessage({
      t: 'failed',
      keys,
      code: z?.code ?? 'failed',
      message: (e as Error).message,
    });
    for (const k of keys) this.settle(k, e);
    this.checkFlush();
  }

  /** Run write calls one at a time (each reads what the previous one queued). */
  private serial<T>(f: () => Promise<T>): Promise<T> {
    const p = this.serialChain.then(f, f);
    this.serialChain = p.catch(() => {});
    return p;
  }

  /** Queue one call's operations (and the content it sealed). */
  async enqueue(
    ops: QOp[],
    content: { chunks: Uint8Array[]; pages: Uint8Array[] } = { chunks: [], pages: [] },
    wait = false,
  ): Promise<void> {
    if (this.stopped) throw new ZenError(0, 'closed', 'the replica is closed');
    const key = newKey();
    const e: QueueEntry = {
      key: bytes.hex(key),
      ops,
      chunks: content.chunks,
      pages: content.pages,
    };
    this.rec.put('queue', key, encodeEntry(e));
    await this.rec.flush();
    this.protect.clear();
    this.queue.push(e);
    this.queue.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
    this.rebuild();
    for (const fn of this.listeners) fn(nodesOf(e));
    const done = new Promise<void>((ok, fail) => {
      const ws = this.waiters.get(e.key) ?? [];
      ws.push({ ok, fail });
      this.waiters.set(e.key, ws);
    });
    if (!wait) done.catch(() => {});
    if (this.owner.isOwner) this.sender.wake();
    else this.chan?.postMessage({ t: 'queued' });
    if (wait) await done;
  }

  // ------------------------------------------------------------ writes

  private tick(): bigint {
    return this.tree.clock.tick();
  }

  /** A directory to write in: the parent of `path`, and the entry named, if any. */
  private async target(path: string): Promise<{ dir: TreeNode; name: string; e?: Entry }> {
    const { parent, name } = splitPath(path);
    const d = await this.lookup(parent);
    if (d.sibling || typeOf(d.node) !== 'dir') {
      throw new ZenError(0, 'not_dir', `not a directory: ${parent}`);
    }
    return { dir: d.node, name, e: (await this.entries(d.node)).get(name) };
  }

  private metaOp(n: TreeNode, patch: Partial<NodeMeta>): QOp {
    if (!n.meta) throw bad(`node ${bytes.hex(n.id)} has no readable meta`);
    const meta: NodeMeta = { ...n.meta, ...patch };
    if (patch.xattrs !== undefined && !patch.xattrs.length) delete meta.xattrs;
    return { op: 'meta', node: n.id, meta: this.tree.sealMeta(n.id, meta), hlc: this.tick() };
  }

  private createOp(dir: TreeNode, meta: NodeMeta): { id: Uint8Array; op: QOp } {
    const id = bytes.randomBytes(16);
    return {
      id,
      op: {
        op: 'move',
        node: id,
        parent: dir.id,
        meta: this.tree.sealMeta(id, meta),
        hlc: this.tick(),
      },
    };
  }

  /** A version's chunk index: kept, or built once with a streamed read (undefined offline). */
  async indexOf(node: Uint8Array, v: ShownVersion): Promise<ChunkIndex | undefined> {
    if (isLocal(v)) return v.index;
    const key = bytes.concat(node, v.dot);
    const raw = await this.rec.get('index', key);
    if (raw) return decodeJson<ChunkIndex>(raw);
    try {
      const ix = await this.tree.chunkIndex(node, v);
      this.rec.put('index', key, encodeJson(ix));
      return ix;
    } catch (e) {
      if (isCode(e, 'network') || isCode(e, 'offline')) return undefined;
      throw e;
    }
  }

  /**
   * What a write based on `base` replaces: `base` (a local version: what it
   * replaced, and itself once its dot is known), and the node's last local
   * write too (writes made here follow each other).
   */
  private replacesFor(node: Uint8Array, base: ShownVersion | undefined) {
    const replaces: Uint8Array[] = [];
    const after: string[] = [];
    const add = (v: ShownVersion | undefined) => {
      if (!v) return;
      if (!isLocal(v)) {
        replaces.push(v.dot);
        return;
      }
      replaces.push(...v.replaces);
      const d = this.learned.get(v.entry);
      if (d) replaces.push(d);
      else if (!after.includes(v.entry)) after.push(v.entry);
    };
    add(base);
    add(this.ov.files.get(bytes.hex(node))?.local);
    const seen = new Set<string>();
    return {
      replaces: replaces.filter((d) => {
        const h = bytes.hex(d);
        if (seen.has(h)) return false;
        seen.add(h);
        return true;
      }),
      after,
    };
  }

  /** Seal content for `node` into the store (nothing is sent) and build its write op. */
  async sealWrite(
    node: Uint8Array,
    data: FileSource,
    base: ShownVersion | undefined,
    previous?: ChunkIndex,
  ): Promise<{ op: QWrite; chunks: Uint8Array[]; pages: Uint8Array[] }> {
    const chunks: Uint8Array[] = [];
    const pages: Uint8Array[] = [];
    const u = await this.tree.seal(node, data, {
      ...(previous ? { previous } : {}),
      sink: async (cs, ps) => {
        for (const c of cs) {
          this.protect.add(useKey('chunk', c.id));
          this.content.chunks.put(c.id, c.data);
          chunks.push(c.id);
        }
        for (const p of ps) {
          this.protect.add(useKey('page', p.id));
          this.content.pages.put(p.id, p.data);
          pages.push(p.id);
        }
      },
    });
    const { replaces, after } = this.replacesFor(node, base);
    const op: QWrite = {
      ...this.tree.writeOpOf(u, replaces),
      ...(after.length ? { after } : {}),
      index: u.index,
    };
    return { op, chunks, pages };
  }

  /** The default chunk size for new content. */
  async chunkSize(): Promise<number> {
    const limits = await this.tree.limits();
    return Math.min(CHUNK_SIZE, limits.max_value_bytes - 64);
  }

  /** Create a directory; returns its node id. */
  mkdir(path: string, opts: WriteCallOptions & { mode?: number } = {}): Promise<Uint8Array> {
    return this.serial(async () => {
      const { dir, name, e } = await this.target(path);
      if (e) throw new ZenError(0, 'exists', `already exists: ${path}`);
      const { id, op } = this.createOp(dir, {
        type: 'dir',
        name,
        mode: opts.mode ?? 0o755,
        mtimeMs: Date.now(),
      });
      await this.enqueue([op], undefined, opts.wait);
      return id;
    });
  }

  /**
   * Write a file's whole content (creating it unless `create: false`).
   * Chunks whose bytes didn't change keep their ids and aren't sent again.
   * The write replaces the version shown, so a write made elsewhere
   * meanwhile stays as a conflict copy (fs.md §4).
   */
  writeFile(path: string, data: FileSource, opts: WriteFileOptions = {}): Promise<Uint8Array> {
    return this.serial(() => this.writeFileNow(path, data, opts, 'file'));
  }

  private async writeFileNow(
    path: string,
    data: FileSource,
    opts: WriteFileOptions,
    type: 'file' | 'symlink',
  ): Promise<Uint8Array> {
    const { dir, name, e } = await this.target(path);
    const ops: QOp[] = [];
    let node: Uint8Array;
    let base: ShownVersion | undefined;
    const xattrs = (old?: Uint8Array) => {
      if (opts.contentType === undefined) return old;
      const x = decodeXattrs(old);
      x.set(CONTENT_TYPE, bytes.utf8(opts.contentType));
      return encodeXattrs(x);
    };
    if (!e) {
      if (opts.create === false) throw notFound(path);
      const xa = xattrs();
      const c = this.createOp(dir, {
        type,
        name,
        mode: opts.mode ?? (type === 'symlink' ? 0o777 : 0o644),
        mtimeMs: Date.now(),
        ...(xa ? { xattrs: xa } : {}),
      });
      node = c.id;
      ops.push(c.op);
    } else {
      if (type === 'symlink') throw new ZenError(0, 'exists', `already exists: ${path}`);
      if (e.sibling) throw bad(`a conflict copy is read-only: ${path}`);
      if (typeOf(e.node) === 'dir') throw new ZenError(0, 'is_dir', `a directory: ${path}`);
      node = e.node.id;
      base = await this.shown(e);
      const xa = xattrs(e.node.meta?.xattrs);
      ops.push(this.metaOp(e.node, { mtimeMs: Date.now(), ...(xa ? { xattrs: xa } : {}) }));
    }
    const previous = base ? await this.indexOf(node, base) : undefined;
    const w = await this.sealWrite(node, data, base, previous);
    ops.push(w.op);
    await this.enqueue(ops, w, opts.wait);
    return node;
  }

  /** Create a symlink at `path` pointing to `target`. */
  symlink(target: string, path: string, opts: WriteCallOptions = {}): Promise<Uint8Array> {
    return this.serial(() => this.writeFileNow(path, bytes.utf8(target), opts, 'symlink'));
  }

  /**
   * Rename or move `from` to `to`. An existing `to` is replaced (moved to
   * the trash in the same entry): a file by a file, an empty directory by a
   * directory.
   */
  rename(from: string, to: string, opts: WriteCallOptions = {}): Promise<void> {
    return this.serial(async () => {
      const src = await this.lookup(from);
      if (src.sibling) throw bad(`a conflict copy can't be renamed: ${from}`);
      if (bytes.equal(src.node.id, ROOT)) throw bad("the root can't be renamed");
      const { dir, name, e } = await this.target(to);
      const ops: QOp[] = [];
      if (e) {
        if (bytes.equal(e.node.id, src.node.id) && !e.sibling) return;
        if (e.sibling) throw bad(`a conflict copy can't be replaced: ${to}`);
        const srcDir = typeOf(src.node) === 'dir';
        const dstDir = typeOf(e.node) === 'dir';
        if (dstDir && !srcDir) throw new ZenError(0, 'is_dir', `a directory: ${to}`);
        if (srcDir && !dstDir) throw new ZenError(0, 'not_dir', `not a directory: ${to}`);
        if (dstDir && this.childrenOf(e.node).length) {
          throw new ZenError(0, 'not_empty', `not empty: ${to}`);
        }
        ops.push({ op: 'move', node: e.node.id, parent: TRASH, hlc: this.tick() });
      }
      // No directory into itself.
      for (let n: TreeNode | undefined = dir; n; n = n.parent ? this.node(n.parent) : undefined) {
        if (bytes.equal(n.id, src.node.id)) throw bad(`a directory can't move into itself: ${to}`);
        if (bytes.equal(n.id, ROOT)) break;
      }
      if (!src.node.meta) throw bad(`node ${bytes.hex(src.node.id)} has no readable meta`);
      if (src.node.parent && bytes.equal(src.node.parent, dir.id)) {
        ops.push(this.metaOp(src.node, { name }));
      } else {
        const meta = { ...src.node.meta, name };
        ops.push({
          op: 'move',
          node: src.node.id,
          parent: dir.id,
          meta: this.tree.sealMeta(src.node.id, meta),
          hlc: this.tick(),
        });
      }
      await this.enqueue(ops, undefined, opts.wait);
    });
  }

  /**
   * Delete a file (to the trash). A conflict copy is resolved instead: the
   * file keeps the version shown, and that copy's version is replaced.
   */
  unlink(path: string, opts: WriteCallOptions = {}): Promise<void> {
    return this.serial(async () => {
      const e = await this.lookup(path);
      if (typeOf(e.node) === 'dir') throw new ZenError(0, 'is_dir', `a directory: ${path}`);
      if (!e.sibling) {
        await this.enqueue(
          [{ op: 'move', node: e.node.id, parent: TRASH, hlc: this.tick() }],
          undefined,
          opts.wait,
        );
        return;
      }
      const keep = await this.shown({ node: e.node });
      if (!keep) throw notFound(path);
      const { replaces, after } = this.replacesFor(e.node.id, keep);
      if (!isLocal(e.sibling)) replaces.push(e.sibling.dot);
      const op: QWrite = {
        op: 'write',
        node: e.node.id,
        replaces,
        ...(after.length ? { after } : {}),
        chunks: keep.chunks,
        ...(keep.pages.length ? { pages: keep.pages } : {}),
        // A manifest is bound to its tree and node, not its version.
        manifest: keep.sealedManifest,
        ...(isLocal(keep) && keep.index ? { index: keep.index } : {}),
      };
      await this.enqueue([op], undefined, opts.wait);
    });
  }

  /** Delete an empty directory (to the trash). */
  rmdir(path: string, opts: WriteCallOptions = {}): Promise<void> {
    return this.serial(async () => {
      const e = await this.lookup(path);
      if (e.sibling || typeOf(e.node) !== 'dir') {
        throw new ZenError(0, 'not_dir', `not a directory: ${path}`);
      }
      if (bytes.equal(e.node.id, ROOT)) throw bad("the root can't be removed");
      if (this.childrenOf(e.node).length) throw new ZenError(0, 'not_empty', `not empty: ${path}`);
      await this.enqueue(
        [{ op: 'move', node: e.node.id, parent: TRASH, hlc: this.tick() }],
        undefined,
        opts.wait,
      );
    });
  }

  private setMeta(
    path: string,
    patch: (n: TreeNode) => Partial<NodeMeta>,
    opts: WriteCallOptions,
  ): Promise<void> {
    return this.serial(async () => {
      const e = await this.lookup(path);
      if (e.sibling) throw bad(`a conflict copy is read-only: ${path}`);
      if (bytes.equal(e.node.id, ROOT)) throw bad("the root's meta can't change");
      await this.enqueue([this.metaOp(e.node, patch(e.node))], undefined, opts.wait);
    });
  }

  chmod(path: string, mode: number, opts: WriteCallOptions = {}): Promise<void> {
    return this.setMeta(path, () => ({ mode: mode & 0o7777 }), opts);
  }

  utimes(path: string, mtimeMs: number, opts: WriteCallOptions = {}): Promise<void> {
    return this.setMeta(path, () => ({ mtimeMs }), opts);
  }

  setxattr(
    path: string,
    name: string,
    value: Uint8Array,
    opts: WriteCallOptions = {},
  ): Promise<void> {
    if (!name) throw bad('an empty attribute name');
    return this.setMeta(
      path,
      (n) => {
        const x = decodeXattrs(n.meta?.xattrs);
        x.set(name, value);
        return { xattrs: encodeXattrs(x) };
      },
      opts,
    );
  }

  removexattr(path: string, name: string, opts: WriteCallOptions = {}): Promise<void> {
    return this.setMeta(
      path,
      (n) => {
        const x = decodeXattrs(n.meta?.xattrs);
        if (!x.delete(name)) throw new ZenError(0, 'no_attr', `no attribute ${name} on ${path}`);
        return { xattrs: encodeXattrs(x) };
      },
      opts,
    );
  }

  /** Cut or extend (with zeros) a file to `length` bytes. */
  async truncate(path: string, length: number, opts: WriteCallOptions = {}): Promise<void> {
    const h = await this.open(path, 'r+');
    try {
      await h.truncate(length);
      await h.flush(opts);
    } finally {
      await h.close().catch(() => {});
    }
  }

  /**
   * Open a file: `r` (read), `r+` (read and write), `w` (truncate; created
   * if missing), `a` (append; created if missing). Writes touch whole
   * chunks in memory; `flush` (and `close`) queues one write with only the
   * changed chunks sealed.
   */
  open(path: string, flags: OpenFlags = 'r', opts: OpenOptions = {}): Promise<FileHandle> {
    return this.serial(async () => {
      const create = opts.create ?? (flags === 'w' || flags === 'a');
      const { dir, name, e } = await this.target(path);
      let node: TreeNode;
      let base: ShownVersion | undefined;
      if (!e) {
        if (!create) throw notFound(path);
        const c = this.createOp(dir, {
          type: 'file',
          name,
          mode: opts.mode ?? 0o644,
          mtimeMs: Date.now(),
        });
        await this.enqueue([c.op]);
        node = this.node(c.id)!;
      } else {
        if (create && opts.exclusive) throw new ZenError(0, 'exists', `already exists: ${path}`);
        if (typeOf(e.node) === 'dir') throw new ZenError(0, 'is_dir', `a directory: ${path}`);
        if (e.sibling && flags !== 'r') throw bad(`a conflict copy is read-only: ${path}`);
        node = e.node;
        base = await this.shown(e);
      }
      const h = new FileHandle(
        this,
        path,
        node.id,
        base,
        flags,
        base?.manifest.chunkSize ?? (await this.chunkSize()),
      );
      if (flags === 'w') await h.truncate(0);
      return h;
    });
  }

  /** Queue a handle's flush (one entry): its meta (mtime) and its write. */
  async queueFlush(
    node: Uint8Array,
    data: FileSource,
    base: ShownVersion | undefined,
    previous: ChunkIndex | undefined,
    wait: boolean,
  ): Promise<LocalVersion> {
    return this.serial(async () => {
      const n = this.node(node);
      if (!n) throw new ZenError(404, 'not_found', 'the file is gone');
      const w = await this.sealWrite(node, data, base, previous);
      const ops: QOp[] = [];
      if (n.meta) ops.push(this.metaOp(n, { mtimeMs: Date.now() }));
      ops.push(w.op);
      await this.enqueue(ops, w, wait);
      return this.ov.files.get(bytes.hex(node))!.local;
    });
  }

  /** Read part of a version (fetched, or kept). */
  readVersion(
    node: Uint8Array,
    v: ShownVersion,
    offset: number,
    length: number,
  ): Promise<Uint8Array> {
    return this.net(`file ${bytes.hex(node)}`, () => this.tree.read(node, v, offset, length));
  }

  // ------------------------------------------------------------ lifecycle

  async status(): Promise<ReplicaStatus> {
    return {
      online: this.online,
      owner: this.owner.isOwner,
      nodes: this.nodes.size,
      cacheBytes: await this.content.total(),
      pins: this.pins.size,
      queued: this.queue.length,
      sending: this.sender.sending,
    };
  }

  /**
   * Resolves once every queued write is on the server (and the replica has
   * caught up with it); rejects with `offline` when it can't be sent now.
   */
  async flush(): Promise<void> {
    await this.rec.flush();
    if (!this.queue.length && !this.settling.length) return;
    const p = new Promise<void>((ok, fail) => this.flushWaiters.push({ ok, fail }));
    if (this.owner.isOwner) this.sender.wake();
    return p;
  }

  /** Stop syncing, write what is pending, and hand the role over. */
  async close(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    this.fetchQueue.length = 0;
    await this.sender.stopped();
    await this.catching.catch(() => {});
    await this.rec.flush();
    this.owner.close();
    this.chan?.close();
    await this.release?.();
  }

  /** Delete every record of a replica (sign-out). */
  static async wipe(local: SealedStore, fs: number, tree: Uint8Array): Promise<void> {
    for (const s of replicaStores(fs, tree)) await local.clear(s);
  }
}

/** Messages on the tab channel. */
interface Msg {
  t: 'nodes' | 'pins' | 'queued' | 'sent' | 'settled' | 'failed' | 'offline';
  ids?: string[];
  keys?: string[];
  dots?: [string, string][];
  code?: string;
  message?: string;
}
