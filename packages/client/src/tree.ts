// The CRDT filesystem: tree operations, files, chunks and the change feed
// (spec/fs.md, api.md §6 and §12, formats.md §11).
import type {
  ChunkPut,
  CommitResult,
  CrdtOp,
  Info,
  Limits,
  Manifest,
  NodeMeta,
  NodeState,
} from '@zen/wasm';
import { compare, equal, hex, randomBytes } from './bytes.js';
import type { Client } from './client.js';
import { isCode, ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import { sendCommit } from './kv.js';
import type { Session } from './session.js';
import { zw } from './wasm.js';

/** The root of every tree (formats.md §11). */
export const ROOT: Uint8Array = new Uint8Array(16);
/** Deleted nodes live under TRASH until purged (fs.md §6). */
export const TRASH: Uint8Array = new Uint8Array(16).fill(0xff);

/** The plaintext chunk size clients use (fs.md §4.1). */
export const CHUNK_SIZE = 65536;

const OP_OVERHEAD = 512; // per commit operation, as the server counts (api.md §6)
const MAX_GET_NODES = 1000;
const MAX_GET_CHUNKS = 64;
const FETCH_PARALLEL = 4;

// ------------------------------------------------------------------ clock

/** Where a clock keeps its last timestamp across restarts. */
export interface ClockStore {
  load(): bigint | undefined;
  save(last: bigint): void;
}

/** An in-memory clock store (the default). */
export function memoryClockStore(initial?: bigint): ClockStore {
  let v = initial;
  return {
    load: () => v,
    save: (last) => {
      v = last;
    },
  };
}

/**
 * The hybrid logical clock of a device (fs.md §2, formats.md §11.1). Every
 * tree a device writes may share one: timestamps only need to be unique
 * per tree and device, and a shared clock makes them unique everywhere.
 *
 * The wall clock is corrected by the server's (`/v1/info` `time_ms`, read
 * once before the first operation), so a device whose clock is off still
 * writes timestamps the server accepts.
 */
export class TreeClock {
  private readonly store: ClockStore;
  private clock: InstanceType<typeof zw.Clock>;
  private offsetMs = 0;
  private synced: Promise<void> | undefined;
  /** The highest timestamp the server accepted from this clock. */
  private accepted = 0n;

  constructor(private readonly opts: { store?: ClockStore; now?: () => number } = {}) {
    this.store = opts.store ?? memoryClockStore();
    this.clock = new zw.Clock(this.store.load() ?? 0n);
  }

  /** The last timestamp issued or observed. */
  get last(): bigint {
    return this.clock.last;
  }

  /** The corrected wall clock, unix ms. */
  now(): number {
    return (this.opts.now ?? Date.now)() + this.offsetMs;
  }

  /** Read the server clock once (again with `force`): wall-clock offset and observation. */
  sync(client: Client, force = false): Promise<void> {
    if (force || !this.synced) {
      const p = this.syncNow(client);
      this.synced = p.catch((e) => {
        this.synced = undefined;
        throw e;
      });
    }
    return this.synced;
  }

  private async syncNow(client: Client): Promise<void> {
    const t0 = (this.opts.now ?? Date.now)();
    const info = await client.info();
    const t1 = (this.opts.now ?? Date.now)();
    const server = Number(info.time_ms);
    this.offsetMs = server - Math.round((t0 + t1) / 2);
    this.observe(zw.hlc(server, 0));
  }

  /** A new timestamp, later than everything seen. */
  tick(): bigint {
    const h = this.clock.tick(this.now());
    this.store.save(this.clock.last);
    return h;
  }

  /** Observe a timestamp from the server (node states, other devices' ops). */
  observe(h: bigint | undefined): void {
    if (h === undefined || h <= this.clock.last) return;
    this.clock.observe(h);
    this.store.save(this.clock.last);
  }

  /** Note a timestamp the server accepted. */
  accept(h: bigint): void {
    if (h > this.accepted) this.accepted = h;
  }

  /**
   * After `clock_skew`: the clock ran ahead of the server (a wrong wall
   * clock, persisted). Restart it at the server's time, but never before a
   * timestamp the server already took from it, so this device's own later
   * operations still win over its earlier ones.
   */
  reset(serverMs: number): void {
    const start = zw.hlc(serverMs, 0);
    this.clock.free();
    this.clock = new zw.Clock(start > this.accepted ? start : this.accepted);
    this.store.save(this.clock.last);
  }

  /** Free the WASM clock. */
  free(): void {
    this.clock.free();
  }
}

const sessionClocks = new WeakMap<Session, TreeClock>();
const serverInfo = new WeakMap<Client, Promise<Info>>();

function infoOf(client: Client): Promise<Info> {
  let p = serverInfo.get(client);
  if (!p) {
    p = client.info();
    serverInfo.set(client, p);
    p.catch(() => serverInfo.delete(client));
  }
  return p;
}

// ------------------------------------------------------------------ types

/** A node as read from the server, with its meta opened. */
export interface TreeNode {
  id: Uint8Array;
  /** Absent: invisible (a creation that was undone, fs.md §1). */
  parent?: Uint8Array;
  /** The opened meta; absent when the node has none, or it doesn't open (`unreadable`). */
  meta?: NodeMeta;
  /** The meta is present but didn't open (corrupt, or an epoch these keys can't reach). */
  unreadable?: boolean;
  /** Content versions; more than one means siblings (fs.md §4). */
  versions: number;
  /** Change offset (fs.md §5). */
  changed: Uint8Array;
  /** The raw state. */
  state: NodeState;
}

/** A node with its current size (`stat`). */
export interface NodeStat extends TreeNode {
  /** The newest version's size; absent when the node has no content. */
  size?: number;
}

/** One content version of a file, its manifest opened and checked. */
export interface FileVersion {
  dot: Uint8Array;
  /** The writing device's fingerprint. */
  device: Uint8Array;
  manifest: Manifest;
  /** Chunk ids in file order (equal to `manifest.chunks`). */
  chunks: Uint8Array[];
  /** The sealed manifest, as stored. */
  sealedManifest: Uint8Array;
}

/**
 * What a writer knows of a version's plaintext, for chunk reuse: a SHA-256
 * per chunk, kept in memory only (never sent). `writeFile` returns one for
 * what it wrote; `chunkIndex` builds one from a version's bytes.
 */
export interface ChunkIndex {
  chunkSize: number;
  chunks: Uint8Array[];
  hashes: Uint8Array[];
}

/** Options of `writeFile`. */
export interface WriteOptions {
  /**
   * The versions this write replaces (fs.md §4). `'current'` (default)
   * replaces every version on the server at the moment of the write, so
   * concurrent writes are overwritten; pass the dots seen when the file was
   * opened to keep a concurrent write as a sibling instead.
   */
  replaces?: Uint8Array[] | 'current';
  /** The previous version's chunk index: chunks whose bytes didn't change keep their ids. */
  previous?: ChunkIndex;
  /** Plaintext bytes per chunk (default 64 KiB). */
  chunkSize?: number;
}

/** The result of `writeFile`. */
export interface WrittenFile {
  /** The new version's dot. */
  dot: Uint8Array;
  manifest: Manifest;
  /** The new content's index, for the next write's `previous`. */
  index: ChunkIndex;
  /** Chunks uploaded (the rest were reused). */
  uploaded: number;
  /** Commits sent, the write's included. */
  commits: number;
}

/** One change of the feed; `state` absent: the node was purged. */
export interface TreeChange {
  offset: Uint8Array;
  node: Uint8Array;
  state?: TreeNode;
}

/**
 * One batch of the change feed. `resync` (with no changes): the cursor
 * was too old (409 `resync`, fs.md §5); the caller drops its replica, and
 * the batches that follow are a full sync.
 */
export interface ChangeBatch {
  resync?: true;
  changes: TreeChange[];
  /** Pass to `changes()` to resume after this batch. */
  cursor?: Uint8Array;
  /** More changes are ready (no wait). */
  more: boolean;
}

/** An operation as sent, for the op chain (formats.md §11.5). */
export interface OpRecord {
  /** The canonical op bytes. */
  bytes: Uint8Array;
  /** The sending device's fingerprint. */
  device: Uint8Array;
}

/** A sealed chunk cache (sealed bytes, as on the server). */
export interface ChunkCache {
  get(id: Uint8Array): Uint8Array | undefined | Promise<Uint8Array | undefined>;
  put(id: Uint8Array, sealed: Uint8Array): void | Promise<void>;
}

/** Options of a `Tree`. */
export interface TreeOptions {
  /** The clock; default one shared by every tree of the session. */
  clock?: TreeClock;
  /** Called with each operation the server accepted from this client, in order. */
  onOp?: (rec: OpRecord) => void;
  /** A cache of sealed chunks for reads. */
  chunkCache?: ChunkCache;
}

/** Metadata given to `mkdir` / `create`. */
export interface CreateOptions {
  /** POSIX permission bits (default 0o755 for directories, 0o644 for files). */
  mode?: number;
  /** Unix ms (default now). */
  mtimeMs?: number;
  xattrs?: Uint8Array;
}

/** An operation before its timestamp is assigned (`Tree.send`). */
export type PendingOp =
  | { op: 'move'; node: Uint8Array; parent: Uint8Array; meta?: Uint8Array }
  | { op: 'meta'; node: Uint8Array; meta: Uint8Array }
  | {
      op: 'write';
      node: Uint8Array;
      replaces: Uint8Array[];
      chunks: Uint8Array[];
      manifest: Uint8Array;
    };

// ------------------------------------------------------------------ tree

/** A tree of an fs (spec/fs.md). Get one with `UnlockedFs.tree(id)`. */
export class Tree {
  readonly clock: TreeClock;

  constructor(
    readonly fs: UnlockedFs,
    readonly id: Uint8Array,
    readonly opts: TreeOptions = {},
  ) {
    let c = opts.clock;
    if (!c) {
      c = sessionClocks.get(fs.session);
      if (!c) {
        c = new TreeClock();
        sessionClocks.set(fs.session, c);
      }
    }
    this.clock = c;
  }

  /** A random id for a new tree; the tree exists once it has an operation. */
  static newId(): Uint8Array {
    return randomBytes(16);
  }

  /** The trees of an fs, with their operation counts. */
  static async list(fs: UnlockedFs): Promise<{ tree: Uint8Array; ops: bigint }[]> {
    const r = await fs.session.call('/v1/fs/tree/list', zw.encodeTreeList, zw.decodeTrees, {
      fs: fs.id,
    });
    return r.trees;
  }

  /** The server limits (`/v1/info`, read once per client). */
  async limits(): Promise<Limits> {
    return (await infoOf(this.fs.session.client)).limits;
  }

  private get session(): Session {
    return this.fs.session;
  }

  // ------------------------------------------------------------ reading

  private node(s: NodeState): TreeNode {
    this.clock.observe(s.move_hlc);
    this.clock.observe(s.meta_hlc);
    const n: TreeNode = { id: s.node, versions: s.versions, changed: s.changed, state: s };
    if (s.parent) n.parent = s.parent;
    if (s.meta) {
      try {
        n.meta = this.fs.keysFor(s.meta).openMeta(this.id, s.node, s.meta);
      } catch {
        n.unreadable = true;
      }
    }
    return n;
  }

  /** Nodes by id, in request order; undefined for unknown ids. */
  async get(nodes: Uint8Array[]): Promise<(TreeNode | undefined)[]> {
    const found = new Map<string, TreeNode>();
    for (let i = 0; i < nodes.length; i += MAX_GET_NODES) {
      const r = await this.session.call('/v1/fs/tree/get', zw.encodeTreeGet, zw.decodeNodes, {
        fs: this.fs.id,
        tree: this.id,
        nodes: nodes.slice(i, i + MAX_GET_NODES),
      });
      for (const s of r.nodes) found.set(hex(s.node), this.node(s));
    }
    return nodes.map((n) => found.get(hex(n)));
  }

  /** One node, or undefined. */
  async getOne(node: Uint8Array): Promise<TreeNode | undefined> {
    return (await this.get([node]))[0];
  }

  /**
   * A node with the size of its newest version. For a node with content
   * this costs a second request (`file/get`, which returns every version's
   * manifest and chunk list); a node without content costs one.
   */
  async stat(node: Uint8Array): Promise<NodeStat | undefined> {
    const n = await this.getOne(node);
    if (!n || n.versions === 0) return n;
    const v = (await this.versions(node)).at(-1);
    return v ? { ...n, size: v.manifest.size } : n;
  }

  /**
   * The children of a node, in node-id order, paging through the server's
   * limit. A missing (purged) parent id still lists its orphans (fs.md §6).
   */
  async *children(parent: Uint8Array, opts: { pageSize?: number } = {}): AsyncGenerator<TreeNode> {
    let after: Uint8Array | undefined;
    for (;;) {
      const r = await this.session.call(
        '/v1/fs/tree/children',
        zw.encodeTreeChildren,
        zw.decodeNodes,
        {
          fs: this.fs.id,
          tree: this.id,
          parent,
          ...(after ? { after } : {}),
          ...(opts.pageSize ? { limit: opts.pageSize } : {}),
        },
      );
      for (const s of r.nodes) yield this.node(s);
      const last = r.nodes.at(-1);
      if (!r.more || !last) return;
      after = last.node;
    }
  }

  /** Every child of a node, collected. */
  async list(parent: Uint8Array): Promise<TreeNode[]> {
    const out: TreeNode[] = [];
    for await (const n of this.children(parent)) out.push(n);
    return out;
  }

  /**
   * The change feed (fs.md §5) from `cursor` (absent: a full sync). Without
   * `waitMs` it ends once caught up; with it, it long-polls for new changes
   * until the caller stops iterating. Batches with no changes aren't yielded.
   * A cursor older than the kept tombstones yields `{resync: true}` and goes
   * on with a full sync.
   */
  async *changes(
    cursor?: Uint8Array,
    opts: { waitMs?: number; limit?: number } = {},
  ): AsyncGenerator<ChangeBatch> {
    let after = cursor;
    for (;;) {
      let r: Awaited<ReturnType<typeof zw.decodeChanges>>;
      try {
        r = await this.session.call('/v1/fs/tree/changes', zw.encodeTreeChanges, zw.decodeChanges, {
          fs: this.fs.id,
          tree: this.id,
          ...(after ? { after } : {}),
          ...(opts.limit ? { limit: opts.limit } : {}),
          ...(opts.waitMs ? { wait_ms: opts.waitMs } : {}),
        });
      } catch (e) {
        if (!isCode(e, 'resync') || !after) throw e;
        after = undefined;
        yield { resync: true, changes: [], more: true };
        continue;
      }
      after = r.cursor ?? after;
      const more = r.more ?? false;
      if (r.changes.length) {
        yield {
          changes: r.changes.map((c) => ({
            offset: c.offset,
            node: c.node,
            ...(c.state ? { state: this.node(c.state) } : {}),
          })),
          ...(after ? { cursor: after } : {}),
          more,
        };
      }
      if (!more && opts.waitMs === undefined) return;
    }
  }

  // ------------------------------------------------------------ files

  /** A node's content versions in dot order (siblings when more than one), manifests opened. */
  async versions(node: Uint8Array): Promise<FileVersion[]> {
    const r = await this.session.call('/v1/fs/file/get', zw.encodeFileGet, zw.decodeVersions, {
      fs: this.fs.id,
      tree: this.id,
      node,
    });
    return r.versions.map((v) => {
      const manifest = this.fs.keysFor(v.manifest).openManifest(this.id, node, v.manifest);
      const same =
        manifest.chunks.length === v.chunks.length &&
        manifest.chunks.every((c, i) => equal(c, v.chunks[i]!));
      if (!same) throw new ZenError(0, 'format', 'a manifest disagrees with its chunk list');
      return {
        dot: v.dot,
        device: v.device,
        manifest,
        chunks: v.chunks,
        sealedManifest: v.manifest,
      };
    });
  }

  private async pickVersion(
    node: Uint8Array,
    version?: FileVersion | Uint8Array,
  ): Promise<FileVersion | undefined> {
    if (version && !(version instanceof Uint8Array)) return version;
    const all = await this.versions(node);
    if (!version) return all.at(-1);
    const v = all.find((x) => equal(x.dot, version));
    if (!v) throw new ZenError(404, 'not_found', 'no such version');
    return v;
  }

  /**
   * A file's bytes: `version` (a version or its dot), by default the newest
   * (greatest dot). A node without content reads as empty. Siblings: read
   * `versions()` and each one.
   */
  async readFile(
    node: Uint8Array,
    opts: { version?: FileVersion | Uint8Array } = {},
  ): Promise<Uint8Array> {
    const v = await this.pickVersion(node, opts.version);
    if (!v) return new Uint8Array(0);
    return this.read(node, v, 0, v.manifest.size);
  }

  /**
   * `length` bytes at `offset` of a version, fetching only the chunks they
   * fall in. Shorter at the end of the file.
   */
  async read(
    node: Uint8Array,
    version: FileVersion | Uint8Array | undefined,
    offset: number,
    length: number,
  ): Promise<Uint8Array> {
    const v = await this.pickVersion(node, version);
    if (!v) return new Uint8Array(0);
    const { size, chunkSize } = v.manifest;
    const end = Math.min(size, offset + length);
    if (offset >= end) return new Uint8Array(0);
    const first = Math.floor(offset / chunkSize);
    const last = Math.floor((end - 1) / chunkSize);
    const ids = v.chunks.slice(first, last + 1);
    const data = await this.fetchChunks(ids);
    const out = new Uint8Array(end - offset);
    for (let i = first; i <= last; i++) {
      const plain = data.get(hex(v.chunks[i]!))!;
      const want = i === v.chunks.length - 1 ? size - i * chunkSize : chunkSize;
      if (plain.length !== want) throw new ZenError(0, 'format', `chunk ${i} has the wrong size`);
      const start = i * chunkSize;
      const from = Math.max(offset, start) - start;
      const to = Math.min(end, start + want) - start;
      out.set(plain.subarray(from, to), start + from - offset);
    }
    return out;
  }

  /** Fetch and open chunks (64 per request, a few requests at a time), by hex id. */
  async fetchChunks(ids: Uint8Array[]): Promise<Map<string, Uint8Array>> {
    const out = new Map<string, Uint8Array>();
    const todo: Uint8Array[] = [];
    const queued = new Set<string>();
    const cache = this.opts.chunkCache;
    for (const id of ids) {
      const h = hex(id);
      if (out.has(h) || queued.has(h)) continue;
      const sealed = cache ? await cache.get(id) : undefined;
      if (sealed) out.set(h, this.openChunk(id, sealed));
      else {
        todo.push(id);
        queued.add(h);
      }
    }
    const batches: Uint8Array[][] = [];
    for (let i = 0; i < todo.length; i += MAX_GET_CHUNKS) {
      batches.push(todo.slice(i, i + MAX_GET_CHUNKS));
    }
    const work = async () => {
      for (let b = batches.shift(); b; b = batches.shift()) {
        const r = await this.session.call(
          '/v1/fs/chunks/get',
          zw.encodeChunksGet,
          zw.decodeChunks,
          {
            fs: this.fs.id,
            ids: b,
          },
        );
        for (const c of r.chunks) {
          if (!c.data) throw new ZenError(404, 'not_found', `chunk ${hex(c.id)} is missing`);
          await cache?.put(c.id, c.data);
          out.set(hex(c.id), this.openChunk(c.id, c.data));
        }
      }
    };
    await Promise.all(Array.from({ length: Math.min(FETCH_PARALLEL, batches.length) }, work));
    return out;
  }

  private openChunk(id: Uint8Array, sealed: Uint8Array): Uint8Array {
    return this.fs.keysFor(sealed).openChunk(id, sealed);
  }

  /** A version's chunk index, from its bytes (read if not given). */
  async chunkIndex(node: Uint8Array, version: FileVersion, data?: Uint8Array): Promise<ChunkIndex> {
    const bytes = data ?? (await this.readFile(node, { version }));
    const cs = version.manifest.chunkSize;
    const hashes: Uint8Array[] = [];
    for (let i = 0; i < version.chunks.length; i++) {
      hashes.push(await sha256(bytes.subarray(i * cs, (i + 1) * cs)));
    }
    return { chunkSize: cs, chunks: version.chunks, hashes };
  }

  /**
   * Write a file's content (fs.md §4): `chunkSize` chunks with random ids,
   * uploaded over as many commits as `max_commit_bytes` needs, then the
   * `write` op (with the last chunks). With `previous`, a chunk whose bytes
   * equal one of the previous version's keeps that chunk's id and isn't
   * uploaded; that version should still exist (or have been replaced
   * within `chunk_grace_secs`), else the write fails with 400.
   */
  async writeFile(
    node: Uint8Array,
    data: Uint8Array | AsyncIterable<Uint8Array> | Iterable<Uint8Array>,
    opts: WriteOptions = {},
  ): Promise<WrittenFile> {
    const keys = this.fs.sealKeys();
    const limits = await this.limits();
    const chunkSize = opts.chunkSize ?? CHUNK_SIZE;
    const reuse = new Map<string, Uint8Array[]>();
    if (opts.previous && opts.previous.chunkSize === chunkSize) {
      opts.previous.hashes.forEach((h, i) => {
        const list = reuse.get(hex(h)) ?? [];
        list.push(opts.previous!.chunks[i]!);
        reuse.set(hex(h), list);
      });
    }
    const ids: Uint8Array[] = [];
    const hashes: Uint8Array[] = [];
    let pending: ChunkPut[] = [];
    let pendingBytes = 0;
    let size = 0;
    let uploaded = 0;
    let commits = 0;
    const flush = async () => {
      if (!pending.length) return;
      await sendCommit(this.session, { commit_id: randomBytes(16), chunks: pending });
      commits++;
      pending = [];
      pendingBytes = 0;
    };
    for await (const block of blocks(data, chunkSize)) {
      size += block.length;
      const h = await sha256(block);
      hashes.push(h);
      // Each previous id is used once: a version shouldn't list a chunk twice.
      const old = reuse.get(hex(h))?.shift();
      if (old) {
        ids.push(old);
        continue;
      }
      const id = randomBytes(16);
      const sealed = keys.sealChunk(id, block);
      if (sealed.length > limits.max_value_bytes) {
        throw new ZenError(
          413,
          'too_large',
          `a sealed chunk exceeds max_value_bytes; lower chunkSize`,
        );
      }
      const cost = sealed.length + 16 + OP_OVERHEAD;
      if (
        pendingBytes + cost > limits.max_commit_bytes ||
        pending.length + 1 > limits.max_commit_ops
      ) {
        await flush();
      }
      pending.push({ fs: this.fs.id, id, data: sealed });
      pendingBytes += cost;
      ids.push(id);
      uploaded++;
    }
    const manifest: Manifest = { size, chunkSize, chunks: ids };
    const sealedManifest = keys.sealManifest(this.id, node, manifest);
    if (sealedManifest.length + 16 * ids.length > limits.max_value_bytes) {
      throw new ZenError(
        413,
        'too_large',
        `a file of ${ids.length} chunks exceeds max_value_bytes`,
      );
    }
    const replaces =
      opts.replaces === undefined || opts.replaces === 'current'
        ? (await this.versions(node)).map((v) => v.dot)
        : opts.replaces;
    const writeCost =
      OP_OVERHEAD + 48 + sealedManifest.length + 16 * ids.length + 12 * replaces.length;
    if (
      pendingBytes + writeCost > limits.max_commit_bytes ||
      pending.length + 1 > limits.max_commit_ops
    ) {
      await flush();
    }
    const r = await this.send(
      [{ op: 'write', node, replaces, chunks: ids, manifest: sealedManifest }],
      pending,
    );
    commits++;
    return {
      dot: r.dots![0]!,
      manifest,
      index: { chunkSize, chunks: ids, hashes },
      uploaded,
      commits,
    };
  }

  /**
   * Resolve siblings: write `keep` (a version or its dot) again, replacing
   * every current version. Uploads nothing. Returns the new dot.
   */
  async resolve(node: Uint8Array, keep: FileVersion | Uint8Array): Promise<Uint8Array> {
    const all = await this.versions(node);
    const dot = keep instanceof Uint8Array ? keep : keep.dot;
    const v = all.find((x) => equal(x.dot, dot));
    if (!v) throw new ZenError(404, 'not_found', 'no such version');
    const r = await this.send([
      {
        op: 'write',
        node,
        replaces: all.map((x) => x.dot),
        chunks: v.chunks,
        // A manifest is bound to its tree and node, not its version: reuse it.
        manifest: v.sealedManifest,
      },
    ]);
    return r.dots![0]!;
  }

  // ------------------------------------------------------------ operations

  /** A batch of operations sent as one commit. */
  batch(): TreeBatch {
    return new TreeBatch(this);
  }

  /** Create a directory under `parent`; returns its id. */
  async mkdir(parent: Uint8Array, name: string, opts: CreateOptions = {}): Promise<Uint8Array> {
    const b = this.batch();
    const id = b.mkdir(parent, name, opts);
    await b.commit();
    return id;
  }

  /** Create an empty file (or a symlink, whose target is then its content); returns its id. */
  async create(
    parent: Uint8Array,
    name: string,
    opts: CreateOptions & { type?: 'file' | 'symlink' } = {},
  ): Promise<Uint8Array> {
    const b = this.batch();
    const id = b.create(parent, name, opts);
    await b.commit();
    return id;
  }

  /** Move a node under `parent`, optionally renaming it in the same op. */
  async move(node: Uint8Array, parent: Uint8Array, name?: string): Promise<void> {
    const b = this.batch();
    await b.move(node, parent, name);
    await b.commit();
  }

  /**
   * Rename a node in place: a `meta` op, so a concurrent move of the node
   * survives too (fs.md §3.3).
   */
  async rename(node: Uint8Array, name: string): Promise<void> {
    const b = this.batch();
    await b.rename(node, name);
    await b.commit();
  }

  /** Delete: move to TRASH (restore by moving it out before it's purged). */
  async remove(node: Uint8Array): Promise<void> {
    const b = this.batch();
    b.remove(node);
    await b.commit();
  }

  /**
   * Change a node's meta (mode, mtime, …). Meta is one last-writer-wins
   * register: a concurrent change of another field by another device is lost.
   */
  async setMeta(node: Uint8Array, meta: Partial<NodeMeta>): Promise<void> {
    const b = this.batch();
    await b.setMeta(node, meta);
    await b.commit();
  }

  /** The current meta of a node (404 when unknown or without meta). */
  async metaOf(node: Uint8Array): Promise<NodeMeta> {
    const n = await this.getOne(node);
    if (!n?.meta) throw new ZenError(404, 'not_found', `node ${hex(node)} has no readable meta`);
    return n.meta;
  }

  /** Seal meta for a node. */
  sealMeta(node: Uint8Array, meta: NodeMeta): Uint8Array {
    return this.fs.sealKeys().sealMeta(this.id, node, meta);
  }

  /**
   * Send operations (and chunks) as one commit. Timestamps are assigned
   * here; on `stale_op` (unless the node was purged) or `clock_skew` the
   * operations are rebased: the clock is set from the server, and they are
   * re-issued with fresh timestamps (fs.md §3.4).
   */
  async send(ops: PendingOp[], chunks: ChunkPut[] = []): Promise<CommitResult> {
    const client = this.session.client;
    await this.clock.sync(client);
    for (let attempt = 1; ; attempt++) {
      const hlcs: bigint[] = [];
      const wire: CrdtOp[] = ops.map((o) => {
        const base = { fs: this.fs.id, tree: this.id, node: o.node };
        if (o.op === 'write') {
          return {
            op: 'write',
            ...base,
            replaces: o.replaces,
            chunks: o.chunks,
            manifest: o.manifest,
          };
        }
        const hlc = this.clock.tick();
        hlcs.push(hlc);
        if (o.op === 'meta') return { op: 'meta', ...base, hlc, meta: o.meta };
        return { op: 'move', ...base, parent: o.parent, hlc, ...(o.meta ? { meta: o.meta } : {}) };
      });
      try {
        const r = await sendCommit(this.session, {
          commit_id: randomBytes(16),
          ...(chunks.length ? { chunks } : {}),
          crdt_ops: wire,
        });
        for (const h of hlcs) this.clock.accept(h);
        this.record(wire);
        return r;
      } catch (e) {
        if (attempt >= 3 || !(e instanceof ZenError)) throw e;
        if (e.code === 'stale_op' && hlcs.length && !e.message.includes('purged')) {
          await this.clock.sync(client, true);
          if (attempt === 1) {
            // Observe what the ops touch: their later moves are what the
            // server would have had to undo.
            const touched = ops.flatMap((o) => (o.op === 'move' ? [o.node, o.parent] : [o.node]));
            await this.get(touched);
          } else {
            // Later moves on other nodes: go past anything within the skew.
            const skew = Number((await this.limits()).crdt_max_skew_ms ?? 60_000n);
            this.clock.observe(zw.hlc(this.clock.now() + Math.floor(skew / 2), 0));
          }
          continue;
        }
        if (e.code === 'clock_skew' && hlcs.length) {
          await this.clock.sync(client, true);
          const skew = Number((await this.limits()).crdt_max_skew_ms ?? 60_000n);
          if (zw.hlcMs(this.clock.last) > this.clock.now() + skew / 2)
            this.clock.reset(this.clock.now());
          continue;
        }
        throw e;
      }
    }
  }

  private record(ops: CrdtOp[]): void {
    const onOp = this.opts.onOp;
    if (!onOp) return;
    const device = this.session.deviceFp;
    if (!device) return; // an API-token session: the device isn't known here
    for (const o of ops) onOp({ bytes: opBytes(o), device });
  }

  // ------------------------------------------------------------ op chain

  /** The server's op chain head (formats.md §11.5). */
  async chain(): Promise<{ ops: bigint; chain: Uint8Array }> {
    return this.session.call('/v1/fs/tree/chain', zw.encodeTreeRef, zw.decodeTreeChain, {
      fs: this.fs.id,
      tree: this.id,
    });
  }

  /**
   * Whether the server's chain equals the one recomputed from `ops`: every
   * operation of the tree, from every device, in arrival order (for example
   * what `onOp` recorded, when this client is the tree's only writer).
   */
  async verifyChain(ops: OpRecord[]): Promise<boolean> {
    const c = await this.chain();
    return c.ops === BigInt(ops.length) && equal(c.chain, computeChain(ops));
  }
}

/** The op chain over `ops` in arrival order (formats.md §11.5), from `start` (zeros). */
export function computeChain(ops: OpRecord[], start: Uint8Array = new Uint8Array(32)): Uint8Array {
  let c = start;
  for (const o of ops) c = zw.chainNext(c, o.bytes, o.device);
  return c;
}

/**
 * The canonical bytes of a wire op (formats.md §11.5). A write's own dot
 * isn't part of them: it's known only once the commit commits.
 */
export function opBytes(o: CrdtOp): Uint8Array {
  switch (o.op) {
    case 'move':
      return zw.moveOpBytes(o.fs, o.tree, o.node, o.parent, o.hlc, o.meta ?? new Uint8Array(0));
    case 'meta':
      return zw.metaOpBytes(o.fs, o.tree, o.node, o.hlc, o.meta);
    case 'write':
      return zw.writeOpBytes(o.fs, o.tree, o.node, o.replaces ?? [], o.chunks ?? [], o.manifest);
    default:
      throw new Error(`not a filesystem operation: ${o.op}`);
  }
}

// ------------------------------------------------------------------ batch

/**
 * Several operations in one commit. They apply atomically and in order;
 * `commit` assigns their timestamps. Nodes created in the batch can be
 * renamed or moved in it.
 */
export class TreeBatch {
  readonly ops: PendingOp[] = [];
  private readonly metas = new Map<string, NodeMeta>();

  constructor(readonly tree: Tree) {}

  private async currentMeta(node: Uint8Array): Promise<NodeMeta> {
    return this.metas.get(hex(node)) ?? (await this.tree.metaOf(node));
  }

  private put(parent: Uint8Array, meta: NodeMeta): Uint8Array {
    const node = randomBytes(16);
    this.metas.set(hex(node), meta);
    this.ops.push({ op: 'move', node, parent, meta: this.tree.sealMeta(node, meta) });
    return node;
  }

  /** Create a directory; returns its id. */
  mkdir(parent: Uint8Array, name: string, opts: CreateOptions = {}): Uint8Array {
    return this.put(parent, metaOf('dir', name, opts.mode ?? 0o755, opts));
  }

  /** Create a file or symlink; returns its id. */
  create(
    parent: Uint8Array,
    name: string,
    opts: CreateOptions & { type?: 'file' | 'symlink' } = {},
  ): Uint8Array {
    return this.put(parent, metaOf(opts.type ?? 'file', name, opts.mode ?? 0o644, opts));
  }

  /** Move a node, optionally renaming it (then its meta is read first). */
  async move(node: Uint8Array, parent: Uint8Array, name?: string): Promise<this> {
    if (name === undefined) {
      this.ops.push({ op: 'move', node, parent });
      return this;
    }
    const meta = { ...(await this.currentMeta(node)), name };
    this.metas.set(hex(node), meta);
    this.ops.push({ op: 'move', node, parent, meta: this.tree.sealMeta(node, meta) });
    return this;
  }

  /** Rename in place (a `meta` op). */
  rename(node: Uint8Array, name: string): Promise<this> {
    return this.setMeta(node, { name });
  }

  /** Change meta fields (a `meta` op with the merged meta). */
  async setMeta(node: Uint8Array, meta: Partial<NodeMeta>): Promise<this> {
    const merged = { ...(await this.currentMeta(node)), ...meta };
    this.metas.set(hex(node), merged);
    this.ops.push({ op: 'meta', node, meta: this.tree.sealMeta(node, merged) });
    return this;
  }

  /** Move to TRASH. */
  remove(node: Uint8Array): this {
    this.ops.push({ op: 'move', node, parent: TRASH });
    return this;
  }

  /** Send the batch (rebasing as `Tree.send` does). */
  commit(): Promise<CommitResult> {
    return this.tree.send(this.ops);
  }
}

function metaOf(type: NodeMeta['type'], name: string, mode: number, opts: CreateOptions): NodeMeta {
  return {
    type,
    name,
    mode,
    mtimeMs: opts.mtimeMs ?? Date.now(),
    ...(opts.xattrs ? { xattrs: opts.xattrs } : {}),
  };
}

// ------------------------------------------------------------------ helpers

/**
 * Display names for one directory's children (fs.md §8): the server
 * allows duplicate names, so the first by node id keeps its name and the
 * others become `foo (2).txt`, `foo (3).txt`, …, skipping names a sibling
 * already has. A node without readable meta shows as its hex id. Keyed by
 * hex node id.
 */
export function displayNames(children: TreeNode[]): Map<string, string> {
  const sorted = [...children].sort((a, b) => compare(a.id, b.id));
  const nameOf = (n: TreeNode) => n.meta?.name ?? hex(n.id);
  const taken = new Set(sorted.map(nameOf));
  const seen = new Set<string>();
  const out = new Map<string, string>();
  for (const n of sorted) {
    const name = nameOf(n);
    if (!seen.has(name)) {
      seen.add(name);
      out.set(hex(n.id), name);
      continue;
    }
    const dot = name.lastIndexOf('.');
    const [stem, ext] = dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ''];
    let i = 2;
    while (taken.has(`${stem} (${i})${ext}`)) i++;
    const shown = `${stem} (${i})${ext}`;
    taken.add(shown);
    out.set(hex(n.id), shown);
  }
  return out;
}

async function sha256(b: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', b as Uint8Array<ArrayBuffer>));
}

/** Split bytes or a stream of byte arrays into `size`-byte blocks (the last shorter). */
async function* blocks(
  data: Uint8Array | AsyncIterable<Uint8Array> | Iterable<Uint8Array>,
  size: number,
): AsyncGenerator<Uint8Array> {
  if (data instanceof Uint8Array) {
    for (let i = 0; i < data.length; i += size) yield data.subarray(i, i + size);
    return;
  }
  let buf = new Uint8Array(size);
  let fill = 0;
  for await (const part of data) {
    let p = 0;
    while (p < part.length) {
      const n = Math.min(size - fill, part.length - p);
      buf.set(part.subarray(p, p + n), fill);
      fill += n;
      p += n;
      if (fill === size) {
        yield buf;
        buf = new Uint8Array(size);
        fill = 0;
      }
    }
  }
  if (fill > 0) yield buf.subarray(0, fill);
}
