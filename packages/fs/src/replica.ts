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
  type ChannelFactory,
  defaultChannel,
  defaultLocks,
  displayNames,
  type FileVersion,
  isCode,
  type LockManagerLike,
  Owner,
  ROOT,
  type SealedStore,
  type Tree,
  type TreeChange,
  type TreeNode,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import { Content, Records, replicaStores, useKey } from './store.js';
import { decodeXattrs } from './xattr.js';

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
}

/** A path's target: a node, or one of its sibling versions shown as a conflict copy. */
export interface Entry {
  node: TreeNode;
  sibling?: FileVersion;
}

const DEFAULT_CACHE = 1 << 30;
const DEFAULT_WAIT = 25_000;
const BACKOFF_MIN = 250;
const BACKOFF_MAX = 30_000;
const FETCHERS = 2;
const CURSOR = bytes.utf8('cursor');
const FULL = bytes.utf8('full');

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

  private constructor(
    readonly fs: UnlockedFs,
    readonly id: Uint8Array,
    opts: ReplicaOptions,
  ) {
    this.cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE;
    this.waitMs = opts.waitMs ?? DEFAULT_WAIT;
    this.rec = new Records(opts.local, `zfs/${fs.id}/${bytes.hex(id)}`);
    this.content = new Content(this.rec, () => this.maybeEvict());
    this.tree = fs.tree(id, { chunkCache: this.content.chunks, pageCache: this.content.pages });
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
      r.chan.onmessage = (ev) => r.recv(ev.data as { t: string; ids?: string[] });
    }
    r.owner = new Owner(locks, lockName, opts.owner !== false);
    r.owner.onOwner(() => void r.run());
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

  private recv(m: { t: string; ids?: string[] }): void {
    if (this.owner.isOwner) {
      if (m.t === 'pins') void this.reloadPins().then(() => this.schedulePins());
      return;
    }
    if (m.t === 'nodes' && m.ids) void this.reload(m.ids);
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
    for (const fn of this.listeners) fn(out);
  }

  // ------------------------------------------------------------ paths

  private visible(n: TreeNode): boolean {
    return !!n.state.meta;
  }

  private childrenOf(dir: TreeNode): TreeNode[] {
    const out: TreeNode[] = [];
    for (const k of this.kids.get(bytes.hex(dir.id)) ?? []) {
      const n = this.nodes.get(k);
      if (n && this.visible(n)) out.push(n);
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
      if (n.versions > 1 && typeOf(n) === 'file') {
        let vs: FileVersion[];
        try {
          vs = await this.versionsOf(n);
        } catch (e) {
          if (isCode(e, 'offline')) continue; // copies show once their versions are known
          throw e;
        }
        vs = [...vs].sort((a, b) => bytes.compare(b.dot, a.dot));
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

  /** A node by id, from the table. */
  node(id: Uint8Array): TreeNode | undefined {
    return bytes.equal(id, ROOT) ? this.root : this.nodes.get(bytes.hex(id));
  }

  // ------------------------------------------------------------ content

  /** A node's versions, kept per change offset; fetched (and kept) on a miss. */
  async versionsOf(n: TreeNode): Promise<FileVersion[]> {
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
  private async shown(e: Entry): Promise<FileVersion | undefined> {
    if (e.sibling) return e.sibling;
    return (await this.versionsOf(e.node)).at(-1);
  }

  private async file(path: string): Promise<{ e: Entry; v: FileVersion | undefined }> {
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
    return {
      type,
      mode: (meta?.mode ?? (type === 'dir' ? 0o755 : 0o644)) & (e.sibling ? 0o555 : 0o7777),
      size: v?.size ?? 0,
      mtimeMs: meta?.mtimeMs ?? 0,
      node: e.node.id,
      versions: e.node.versions,
      conflict: !!e.sibling,
    };
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
    await this.rec.flush();
    const left = await this.content.evict(this.cacheBytes, keep);
    await this.rec.flush();
    return left;
  }

  // ------------------------------------------------------------ lifecycle

  async status(): Promise<ReplicaStatus> {
    return {
      online: this.online,
      owner: this.owner.isOwner,
      nodes: this.nodes.size,
      cacheBytes: await this.content.total(),
      pins: this.pins.size,
    };
  }

  /** Write what is pending to the store. */
  flush(): Promise<void> {
    return this.rec.flush();
  }

  /** Stop syncing, write what is pending, and hand the role over. */
  async close(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    this.fetchQueue.length = 0;
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
