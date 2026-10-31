// The CRDT filesystem: tree operations, files, chunks and the change feed
// (spec/fs.md, api.md §6 and §12, formats.md §11).
import type {
  ChunkPut,
  CommitResult,
  CrdtOp,
  FsPolicy,
  Info,
  Limits,
  ManifestV2,
  NodeMeta,
  NodeState,
  PagePut,
  PageRef,
  StateNode,
  Versions,
  AnyManifest as WasmAnyManifest,
} from '@zen/wasm';
import { compare, equal, hex, randomBytes } from './bytes.js';

/**
 * A manifest this client reads: version 1 or 2 (formats.md §11.3). A
 * version-3 manifest (variable chunks, milestone 8) is refused until the
 * client reads it.
 */
export type AnyManifest = Exclude<WasmAnyManifest, { version: 3 }>;

import type { Client } from './client.js';
import type { TreeClock } from './clock.js';
import { isCode, ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import { sendCommit, type Transaction } from './kv.js';
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
const MAX_GET_PAGES = 64;
const FETCH_PARALLEL = 4;
/** Chunks a `readStream` pull fetches at once. */
const STREAM_WINDOW = 64;
/** Decoded pages kept per tree. */
const PAGE_MEMO = 256;

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

/**
 * One content version of a file, its manifest opened and checked. The
 * manifest is version 1 (old writers) or 2 (formats.md §11.3): inline or
 * paged, its chunks under the fs keys or a file key of its own.
 */
export interface FileVersion {
  dot: Uint8Array;
  /** The writing device's fingerprint. */
  device: Uint8Array;
  manifest: AnyManifest;
  /** Chunk ids in file order when inline (equal to the manifest's); empty when paged. */
  chunks: Uint8Array[];
  /** Page ids in file order when paged (equal to the manifest's); empty when inline. */
  pages: Uint8Array[];
  /** The file size in bytes. */
  size: number;
  /** The number of chunks. */
  chunkCount: number;
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
  /** The file key the chunks are sealed under; absent: under the fs keys. */
  fileKey?: Uint8Array;
  /** The file key's epoch. */
  keyEpoch?: number;
  /** Ids per page, when the version is paged: pages whose ids didn't change are reused. */
  pageSize?: number;
  pages?: PageRef[];
}

/**
 * A chunk of the `previous` version taken over as is, without its
 * plaintext (its id, SHA-256 and size, from that version's index). It
 * must start at a chunk boundary and be whole, unless it is the last.
 */
export interface KnownChunk {
  id: Uint8Array;
  hash: Uint8Array;
  size: number;
}

/**
 * A file's content: bytes, or a stream of byte arrays of any sizes, which
 * may hold known chunks of the previous version.
 */
export type FileSource =
  | Uint8Array
  | AsyncIterable<Uint8Array | KnownChunk>
  | Iterable<Uint8Array | KnownChunk>
  | ReadableStream<Uint8Array>;

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
  /** Plaintext bytes per chunk (default 64 KiB, or less on a server with a small `max_value_bytes`). */
  chunkSize?: number;
}

/**
 * Where an `upload` stands after a commit: plain data to keep (it holds the
 * file key, so keep it sealed, e.g. on a `SealedStore`) and to pass as
 * `resume` with the rest of the source. Everything in it is on the server.
 */
export interface UploadState {
  node: Uint8Array;
  chunkSize: number;
  /** Bytes uploaded so far: whole chunks, until the end of the file. */
  size: number;
  /** Chunk ids so far, in file order. */
  chunks: Uint8Array[];
  /** Their SHA-256s. */
  hashes: Uint8Array[];
  /** The full pages uploaded or reused so far (paged files). */
  pages: PageRef[];
  /** The file key and its epoch; absent: chunks under the fs keys. */
  fileKey?: Uint8Array;
  keyEpoch?: number;
  uploaded: number;
  pagesUploaded: number;
  commits: number;
}

/** Options of `upload`. */
export interface UploadOptions extends Omit<WriteOptions, 'replaces'> {
  /** Called after each commit of the upload. */
  onProgress?: (state: UploadState) => void | Promise<void>;
  /**
   * Continue an upload from its last `onProgress` state; the source is then
   * the rest of the file, from `state.size`. Works while the uploaded chunks
   * are within `chunk_grace_secs`.
   */
  resume?: UploadState;
  /**
   * Hand each batch of sealed chunks and pages to `sink` instead of sending
   * it (`Tree.seal`): the content can then be kept and sent later.
   */
  sink?: (chunks: ChunkPut[], pages: PagePut[]) => Promise<void>;
}

/** The result of `writeFile`. */
export interface WrittenFile {
  /** The new version's dot. */
  dot: Uint8Array;
  manifest: ManifestV2;
  /** The new content's index, for the next write's `previous`. */
  index: ChunkIndex;
  /** Chunks uploaded (the rest were reused). */
  uploaded: number;
  /** Pages uploaded (the rest were reused); 0 when inline. */
  pagesUploaded: number;
  /** Commits sent, the write's included. */
  commits: number;
}

/** A file's content uploaded by `Tree.upload`, not yet written to its node. */
export interface Upload {
  node: Uint8Array;
  manifest: ManifestV2;
  /** The manifest sealed for this tree and node. */
  sealedManifest: Uint8Array;
  /** The content's index, for the next write's `previous`. */
  index: ChunkIndex;
  /** Page ids when paged (the write lists these, not the chunks); empty when inline. */
  pages: Uint8Array[];
  /** Chunks uploaded (the rest were reused). */
  uploaded: number;
  /** Pages uploaded (the rest were reused). */
  pagesUploaded: number;
  /** Commits the upload took. */
  commits: number;
}

/** Options of `readStream`. */
export interface ReadStreamOptions {
  /** A version or its dot; default the newest. */
  version?: FileVersion | Uint8Array;
  /** First byte (default 0). */
  offset?: number;
  /** Bytes to read (default to the end). */
  length?: number;
}

/** Operations with their timestamps assigned (`Tree.prepare`). */
export interface Prepared {
  /** The operations as sent. */
  ops: CrdtOp[];
  /** The timestamps taken, in order. */
  hlcs: bigint[];
  /** The operations given, to re-prepare after a rebase. */
  pending: PendingOp[];
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
  /**
   * The tree's op count and chain at this point (api.md §12), when the
   * response was caught up (`more` false). A replica that has applied
   * this batch and nothing newer holds the state they name (fs.md §9).
   */
  head?: { ops: bigint; chain: Uint8Array };
}

/** A node state in the form `TreeState` and `stateLeaf` take (fs.md §9.1). */
export function stateNodeOf(s: NodeState): StateNode {
  return {
    node: s.node,
    ...(s.parent ? { parent: s.parent } : {}),
    moveHlc: s.move_hlc,
    moveDevice: s.move_device,
    ...(s.meta ? { meta: s.meta } : {}),
    ...(s.meta_hlc !== undefined ? { metaHlc: s.meta_hlc } : {}),
    ...(s.meta_device ? { metaDevice: s.meta_device } : {}),
    versions: s.versions,
    changed: s.changed,
  };
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
  /** A cache of pages (their bytes, by page id), checked against their digests on use. */
  pageCache?: ChunkCache;
  /** Where the server's `/v1/info` comes from (e.g. kept for offline use); default read once per client. */
  info?: () => Promise<Info>;
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
  /** `hlc`: a timestamp taken earlier (queued offline); `prepare` ticks one otherwise. */
  | { op: 'move'; node: Uint8Array; parent: Uint8Array; meta?: Uint8Array; hlc?: bigint }
  | { op: 'meta'; node: Uint8Array; meta: Uint8Array; hlc?: bigint }
  | {
      op: 'write';
      node: Uint8Array;
      replaces: Uint8Array[];
      /** Chunk ids (inline), or empty when paged. */
      chunks: Uint8Array[];
      /** Page ids (paged). */
      pages?: Uint8Array[];
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
    this.clock = opts.clock ?? fs.session.clock;
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
    return (await this.info()).limits;
  }

  /** `/v1/info`: `TreeOptions.info` when given, else read once per client. */
  info(): Promise<Info> {
    return this.opts.info ? this.opts.info() : infoOf(this.fs.session.client);
  }

  private get session(): Session {
    return this.fs.session;
  }

  // ------------------------------------------------------------ reading

  /**
   * A node from its wire state (as the change feed or a replica's store
   * gives it): observes its timestamps and opens its meta.
   */
  nodeOf(s: NodeState): TreeNode {
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
      for (const s of r.nodes) found.set(hex(s.node), this.nodeOf(s));
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
      for (const s of r.nodes) yield this.nodeOf(s);
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
   * until the caller stops iterating. Batches with no changes aren't yielded,
   * unless `heads` is set and they carry a `head` (so a follower sees every
   * caught-up point). A cursor older than the kept tombstones yields
   * `{resync: true}` and goes on with a full sync.
   */
  async *changes(
    cursor?: Uint8Array,
    opts: { waitMs?: number; limit?: number; heads?: boolean } = {},
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
      const head =
        !more && r.ops !== undefined && r.chain
          ? { ops: BigInt(r.ops), chain: r.chain }
          : undefined;
      if (r.changes.length || (opts.heads && head)) {
        yield {
          changes: r.changes.map((c) => ({
            offset: c.offset,
            node: c.node,
            ...(c.state ? { state: this.nodeOf(c.state) } : {}),
          })),
          ...(after ? { cursor: after } : {}),
          more,
          ...(head ? { head } : {}),
        };
      }
      if (!more && opts.waitMs === undefined) return;
    }
  }

  // ------------------------------------------------------------ files

  /** A node's content versions in dot order (siblings when more than one), manifests opened. */
  async versions(node: Uint8Array): Promise<FileVersion[]> {
    return this.openVersions(node, zw.decodeVersions(await this.fetchVersions(node)));
  }

  /** A node's versions as the server sends them (wire `Versions`, manifests sealed). */
  fetchVersions(node: Uint8Array): Promise<Uint8Array> {
    return this.session.call('/v1/fs/file/get', zw.encodeFileGet, (b) => b, {
      fs: this.fs.id,
      tree: this.id,
      node,
    });
  }

  /** Open wire versions of `node` (from `fetchVersions`, or kept by a replica). */
  openVersions(node: Uint8Array, r: Versions): FileVersion[] {
    return r.versions.map((v) => {
      const opened = this.fs.keysFor(v.manifest).openAnyManifest(this.id, node, v.manifest);
      if (opened.version === 3) {
        throw new ZenError(0, 'format', 'a version-3 manifest: this client reads versions 1 and 2');
      }
      const manifest: AnyManifest = opened;
      const chunks = v.chunks ?? [];
      const pages = v.pages ?? [];
      const inline = manifest.chunks;
      const same = inline
        ? !pages.length && sameIds(inline, chunks)
        : !chunks.length &&
          sameIds(
            (manifest as ManifestV2).pages!.map((p) => p.id),
            pages,
          );
      if (!same)
        throw new ZenError(0, 'format', 'a manifest disagrees with its chunk or page list');
      return {
        dot: v.dot,
        device: v.device,
        manifest,
        chunks,
        pages,
        size: manifest.size,
        chunkCount: manifest.version === 1 ? manifest.chunks.length : manifest.n,
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
   * `versions()` and each one. The whole file is in memory: use
   * `readStream` for large files.
   */
  async readFile(
    node: Uint8Array,
    opts: { version?: FileVersion | Uint8Array } = {},
  ): Promise<Uint8Array> {
    const v = await this.pickVersion(node, opts.version);
    if (!v) return new Uint8Array(0);
    return this.read(node, v, 0, v.size);
  }

  /**
   * `length` bytes at `offset` of a version, fetching only the chunks (and
   * pages) they fall in. Shorter at the end of the file.
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
    const ids = await this.chunkIds(v, first, last);
    const data = await this.fetchChunks(ids, v);
    const out = new Uint8Array(end - offset);
    for (let i = first; i <= last; i++) {
      const plain = data.get(hex(ids[i - first]!))!;
      const want = i === v.chunkCount - 1 ? size - i * chunkSize : chunkSize;
      if (plain.length !== want) throw new ZenError(0, 'format', `chunk ${i} has the wrong size`);
      const start = i * chunkSize;
      const from = Math.max(offset, start) - start;
      const to = Math.min(end, start + want) - start;
      out.set(plain.subarray(from, to), start + from - offset);
    }
    return out;
  }

  /**
   * A version's bytes as a stream, fetched lazily a window of chunks at a
   * time (and their pages as they are reached), so memory stays at one
   * window. Cancel it to stop fetching.
   */
  readStream(node: Uint8Array, opts: ReadStreamOptions = {}): ReadableStream<Uint8Array> {
    let v: FileVersion | undefined;
    let pos = opts.offset ?? 0;
    let end = 0;
    let started = false;
    return new ReadableStream<Uint8Array>({
      pull: async (ctl) => {
        if (!started) {
          started = true;
          v = await this.pickVersion(node, opts.version);
          end = v ? Math.min(v.size, opts.length === undefined ? v.size : pos + opts.length) : 0;
        }
        if (!v || pos >= end) {
          ctl.close();
          return;
        }
        const cs = v.manifest.chunkSize;
        const upto = Math.min(end, (Math.floor(pos / cs) + STREAM_WINDOW) * cs);
        const part = await this.read(node, v, pos, upto - pos);
        pos += part.length;
        ctl.enqueue(part);
      },
    });
  }

  /** The chunk ids `first..=last` of a version, through its pages when paged. */
  async chunkIds(v: FileVersion, first: number, last: number): Promise<Uint8Array[]> {
    const m = v.manifest;
    if (m.version === 1) return m.chunks.slice(first, last + 1);
    if (m.chunks) return m.chunks.slice(first, last + 1);
    const ps = m.pageSize!;
    const p0 = Math.floor(first / ps);
    const p1 = Math.floor(last / ps);
    const pages = await this.fetchPages(m, p0, p1);
    const ids: Uint8Array[] = [];
    for (let p = p0; p <= p1; p++) ids.push(...pages[p - p0]!);
    return ids.slice(first - p0 * ps, last - p0 * ps + 1);
  }

  private readonly pageMemo = new Map<string, Uint8Array[]>();

  /** Pages `p0..=p1` of a paged manifest, each checked against its digest and count. */
  private async fetchPages(m: ManifestV2, p0: number, p1: number): Promise<Uint8Array[][]> {
    const refs = m.pages!;
    const ps = m.pageSize!;
    const count = (p: number) => Math.min(ps, m.n - p * ps);
    const out: (Uint8Array[] | undefined)[] = [];
    const todo: number[] = [];
    const cache = this.opts.pageCache;
    const check = (p: number, bytes: Uint8Array): Uint8Array[] | undefined => {
      if (!equal(zw.pageDigest(bytes), refs[p]!.digest)) return undefined;
      const ids = zw.decodePage(bytes);
      return ids.length === count(p) ? ids : undefined;
    };
    for (let p = p0; p <= p1; p++) {
      const key = hex(refs[p]!.digest);
      let ids = this.pageMemo.get(key);
      if (ids) {
        // Most recently used last.
        this.pageMemo.delete(key);
        this.pageMemo.set(key, ids);
      } else if (cache) {
        const bytes = await cache.get(refs[p]!.id);
        ids = bytes && check(p, bytes);
      }
      out.push(ids);
      if (!ids) todo.push(p);
    }
    for (let i = 0; i < todo.length; i += MAX_GET_PAGES) {
      const batch = todo.slice(i, i + MAX_GET_PAGES);
      const r = await this.session.call('/v1/fs/pages/get', zw.encodePagesGet, zw.decodePages, {
        fs: this.fs.id,
        ids: batch.map((p) => refs[p]!.id),
      });
      for (const [j, p] of batch.entries()) {
        const d = r.pages[j];
        if (!d?.data) throw new ZenError(404, 'not_found', `page ${hex(refs[p]!.id)} is missing`);
        const ids = check(p, d.data);
        if (!ids) throw new ZenError(0, 'format', `page ${p} doesn't match its manifest`);
        await cache?.put(refs[p]!.id, d.data);
        out[p - p0] = ids;
      }
    }
    for (const [i, ids] of out.entries()) {
      this.pageMemo.set(hex(refs[p0 + i]!.digest), ids!);
    }
    while (this.pageMemo.size > PAGE_MEMO) {
      this.pageMemo.delete(this.pageMemo.keys().next().value!);
    }
    return out as Uint8Array[][];
  }

  /**
   * Fetch and open chunks (64 per request, a few requests at a time), by hex
   * id. `version` says how they are sealed (default: under the fs keys).
   */
  async fetchChunks(ids: Uint8Array[], version?: FileVersion): Promise<Map<string, Uint8Array>> {
    const open = this.opener(version);
    const out = new Map<string, Uint8Array>();
    const todo: Uint8Array[] = [];
    const queued = new Set<string>();
    const cache = this.opts.chunkCache;
    for (const id of ids) {
      const h = hex(id);
      if (out.has(h) || queued.has(h)) continue;
      const sealed = cache ? await cache.get(id) : undefined;
      if (sealed) out.set(h, open(id, sealed));
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
          out.set(hex(c.id), open(c.id, c.data));
        }
      }
    };
    await Promise.all(Array.from({ length: Math.min(FETCH_PARALLEL, batches.length) }, work));
    return out;
  }

  /** How a version's chunks open: under its file key, or the fs keys of their epoch. */
  private opener(v?: FileVersion): (id: Uint8Array, sealed: Uint8Array) => Uint8Array {
    const m = v?.manifest;
    if (m?.version === 2 && m.fileKey) {
      const { fileKey, keyEpoch } = m;
      return (id, sealed) => zw.openFileChunk(this.fs.id, fileKey, keyEpoch!, id, sealed);
    }
    return (id, sealed) => this.fs.keysFor(sealed).openChunk(id, sealed);
  }

  /**
   * A version's chunk index, from its bytes: `data` if given, else read as
   * a stream, so the file is never held whole.
   */
  async chunkIndex(node: Uint8Array, version: FileVersion, data?: Uint8Array): Promise<ChunkIndex> {
    const m = version.manifest;
    const cs = m.chunkSize;
    const hashes: Uint8Array[] = [];
    const src = data ?? this.readStream(node, { version });
    for await (const block of blocks(src, cs)) hashes.push(await sha256(block as Uint8Array));
    if (hashes.length !== version.chunkCount) {
      throw new ZenError(0, 'format', 'a version has the wrong number of chunks');
    }
    const index: ChunkIndex = {
      chunkSize: cs,
      chunks: version.chunkCount ? await this.chunkIds(version, 0, version.chunkCount - 1) : [],
      hashes,
    };
    if (m.version === 2) {
      if (m.fileKey) Object.assign(index, { fileKey: m.fileKey, keyEpoch: m.keyEpoch });
      if (m.pages) Object.assign(index, { pageSize: m.pageSize, pages: m.pages });
    }
    return index;
  }

  /**
   * Write a file's content (fs.md §4): `chunkSize` chunks with random ids,
   * uploaded over as many commits as `max_commit_bytes` needs, then the
   * `write` op (with the last chunks). A file too long for one inline
   * manifest is paged (formats.md §11.6). With `previous`, a chunk whose
   * bytes equal one of the previous version's keeps that chunk's id and
   * isn't uploaded (when both are under the same key), and so does a page
   * whose ids didn't change; that version should still exist (or have been
   * replaced within `chunk_grace_secs`), else the write fails with 400.
   */
  async writeFile(
    node: Uint8Array,
    data: FileSource,
    opts: WriteOptions = {},
  ): Promise<WrittenFile> {
    const st = await this.stage(node, data, opts);
    const op = await this.writeOp(st.upload, opts.replaces);
    const limits = await this.limits();
    const writeCost =
      OP_OVERHEAD +
      48 +
      op.manifest.length +
      16 * (op.chunks.length + (op.pages?.length ?? 0)) +
      12 * op.replaces.length;
    if (
      st.pendingBytes() + writeCost > limits.max_commit_bytes ||
      st.pendingOps() + 1 > limits.max_commit_ops
    ) {
      await st.flush();
    }
    const r = await this.send([op], st.pending(), st.pendingPages());
    const u = st.upload;
    return {
      dot: r.dots![0]!,
      manifest: u.manifest,
      index: u.index,
      uploaded: u.uploaded,
      pagesUploaded: u.pagesUploaded,
      commits: u.commits + 1,
    };
  }

  /**
   * Upload a file's content without writing it (as `writeFile`, every
   * chunk and page in commits of its own). Write it later with `writeOp`,
   * alone (`send`) or in a transaction (`writeIn`). Chunks never written
   * are collected after `chunk_grace_secs`. `onProgress` and `resume` make
   * it resumable.
   */
  async upload(node: Uint8Array, data: FileSource, opts: UploadOptions = {}): Promise<Upload> {
    const st = await this.stage(node, data, opts);
    await st.flush();
    return st.upload;
  }

  /**
   * Seal a file's content without sending anything: every batch of sealed
   * chunks and pages goes to `opts.sink` (to be uploaded later, before the
   * write). `commits` counts the batches.
   */
  seal(
    node: Uint8Array,
    data: FileSource,
    opts: UploadOptions & { sink: NonNullable<UploadOptions['sink']> },
  ): Promise<Upload> {
    return this.upload(node, data, opts);
  }

  /** The `write` operation of an upload, replacing exactly `replaces` (no read). */
  writeOpOf(u: Upload, replaces: Uint8Array[]): Extract<PendingOp, { op: 'write' }> {
    return {
      op: 'write',
      node: u.node,
      replaces,
      chunks: u.pages.length ? [] : u.index.chunks,
      ...(u.pages.length ? { pages: u.pages } : {}),
      manifest: u.sealedManifest,
    };
  }

  /**
   * The `write` operation of an upload. `replaces` as in `writeFile`
   * (`'current'`, the default, reads the node's versions now).
   */
  async writeOp(
    u: Upload,
    replaces: Uint8Array[] | 'current' = 'current',
  ): Promise<Extract<PendingOp, { op: 'write' }>> {
    return this.writeOpOf(
      u,
      replaces === 'current' ? (await this.versions(u.node)).map((v) => v.dot) : replaces,
    );
  }

  /**
   * The file key a write of `node` uses under a `file_keys: on` policy
   * (fs.md §4.2): the key of `previous`, or else of the node's newest
   * version, unless the policy replaces it after a rotation; a fresh one
   * when there is none.
   */
  private async fileKeyFor(
    node: Uint8Array,
    previous: ChunkIndex | undefined,
    policy: FsPolicy,
    epoch: number,
  ): Promise<{ key: Uint8Array; epoch: number }> {
    let base: { key: Uint8Array; epoch: number } | undefined;
    if (previous) {
      if (previous.fileKey) base = { key: previous.fileKey, epoch: previous.keyEpoch! };
    } else {
      try {
        const m = (await this.versions(node)).at(-1)?.manifest;
        if (m?.version === 2 && m.fileKey) base = { key: m.fileKey, epoch: m.keyEpoch! };
      } catch (e) {
        // Offline (e.g. a replica sealing a write to send later): a fresh key.
        if (!isCode(e, 'network')) throw e;
      }
    }
    const fresh = () => ({ key: zw.newFileKey(), epoch });
    if (!base) return fresh();
    if (base.epoch < epoch) {
      if (policy.rekeyAfterRotate === 'eager') return fresh();
      if (policy.rekeyAfterRotate === 'rewrite' && !previous) return fresh();
    }
    return base;
  }

  /**
   * Seal and upload chunks (and pages); the last ones stay pending for the
   * caller's commit.
   */
  private async stage(node: Uint8Array, data: FileSource, opts: UploadOptions) {
    const keys = this.fs.sealKeys();
    const limits = await this.limits();
    const resume = opts.resume;
    if (resume && !equal(resume.node, node)) {
      throw new ZenError(0, 'bad_value', 'resume: the state is of another node');
    }
    const chunkSize =
      resume?.chunkSize ?? opts.chunkSize ?? Math.min(CHUNK_SIZE, limits.max_value_bytes - 64);
    const fk: { key: Uint8Array; epoch: number } | undefined = resume
      ? resume.fileKey && { key: resume.fileKey, epoch: resume.keyEpoch! }
      : this.fs.policy.fileKeys
        ? await this.fileKeyFor(node, opts.previous, this.fs.policy, keys.epoch)
        : undefined;
    const keyOf = fk ? { fileKey: fk.key, keyEpoch: fk.epoch } : {};

    // Reuse only what is sealed under the same key.
    const prev = opts.previous;
    const sameKey =
      !!prev &&
      prev.chunkSize === chunkSize &&
      (fk ? !!prev.fileKey && equal(prev.fileKey, fk.key) : !prev.fileKey);
    const reuse = new Map<string, Uint8Array[]>();
    if (sameKey) {
      prev!.hashes.forEach((h, i) => {
        const list = reuse.get(hex(h)) ?? [];
        list.push(prev!.chunks[i]!);
        reuse.set(hex(h), list);
      });
    }

    // Forms: inline up to what one value holds, then paged.
    const empty: ManifestV2 = { version: 2, size: 0, chunkSize, n: 0, ...keyOf, chunks: [] };
    const inlineMax = Math.floor(
      (limits.max_value_bytes - keys.sealManifestV2(this.id, node, empty).length) / 32,
    );
    const pageSize = zw.pageCapacity(limits.max_value_bytes);
    const prevPages = sameKey && prev!.pageSize === pageSize ? (prev!.pages ?? []) : [];

    const ids: Uint8Array[] = resume ? [...resume.chunks] : [];
    const hashes: Uint8Array[] = resume ? [...resume.hashes] : [];
    const pages: PageRef[] = resume ? [...resume.pages] : [];
    let pending: ChunkPut[] = [];
    let pendingPages: PagePut[] = [];
    let pendingBytes = 0;
    let size = resume?.size ?? 0;
    const upload = {
      uploaded: resume?.uploaded ?? 0,
      pagesUploaded: resume?.pagesUploaded ?? 0,
      commits: resume?.commits ?? 0,
    } as Upload;
    const state = (): UploadState => ({
      node,
      chunkSize,
      size,
      chunks: [...ids],
      hashes: [...hashes],
      pages: [...pages],
      ...keyOf,
      uploaded: upload.uploaded,
      pagesUploaded: upload.pagesUploaded,
      commits: upload.commits,
    });
    const flush = async () => {
      if (!pending.length && !pendingPages.length) return;
      if (opts.sink) await opts.sink(pending, pendingPages);
      else {
        await sendCommit(this.session, {
          commit_id: randomBytes(16),
          ...(pending.length ? { chunks: pending } : {}),
          ...(pendingPages.length ? { pages: pendingPages } : {}),
        });
      }
      upload.commits++;
      pending = [];
      pendingPages = [];
      pendingBytes = 0;
      await opts.onProgress?.(state());
    };
    const add = async (cost: number) => {
      if (
        pendingBytes + cost > limits.max_commit_bytes ||
        pending.length + pendingPages.length + 1 > limits.max_commit_ops
      ) {
        await flush();
      }
      pendingBytes += cost;
    };
    // The pages of `ids` from `pages.length` on: full ones, or every one at the end.
    const cutPages = async (end: boolean) => {
      while (ids.length - pages.length * pageSize >= (end ? 1 : pageSize)) {
        const i = pages.length;
        const slice = ids.slice(i * pageSize, (i + 1) * pageSize);
        const old = prevPages[i];
        if (old && sameIds(slice, prev!.chunks.slice(i * pageSize, (i + 1) * pageSize))) {
          pages.push(old);
          continue;
        }
        const bytes = zw.encodePage(slice);
        const id = randomBytes(16);
        await add(OP_OVERHEAD + bytes.length + 16 + 48 * slice.length);
        pendingPages.push({ fs: this.fs.id, id, data: bytes });
        pages.push({ id, digest: zw.pageDigest(bytes) });
        upload.pagesUploaded++;
      }
    };
    const paged = () => ids.length > inlineMax;

    const known = sameKey ? new Set(prev!.chunks.map(hex)) : undefined;
    for await (const block of blocks(data, chunkSize)) {
      if (size % chunkSize !== 0) {
        throw new ZenError(0, 'bad_value', 'a short chunk before the end of the file');
      }
      if (!(block instanceof Uint8Array)) {
        // A known chunk: taken over as is.
        if (!known?.has(hex(block.id))) {
          throw new ZenError(0, 'bad_value', 'a known chunk not of `previous` (or another key)');
        }
        if (block.size < 1 || block.size > chunkSize) {
          throw new ZenError(0, 'bad_value', 'a known chunk of the wrong size');
        }
        ids.push(block.id);
        hashes.push(block.hash);
        size += block.size;
        if (paged()) await cutPages(false);
        continue;
      }
      const h = await sha256(block);
      // Each previous id is used once: a version shouldn't list a chunk twice.
      const old = reuse.get(hex(h))?.shift();
      if (old) {
        ids.push(old);
      } else {
        const id = randomBytes(16);
        const sealed = fk
          ? zw.sealFileChunk(this.fs.id, fk.key, fk.epoch, id, block)
          : keys.sealChunk(id, block);
        if (sealed.length > limits.max_value_bytes) {
          throw new ZenError(
            413,
            'too_large',
            `a sealed chunk exceeds max_value_bytes; lower chunkSize`,
          );
        }
        // A flush here reports a state without this chunk.
        await add(sealed.length + 16 + OP_OVERHEAD);
        pending.push({ fs: this.fs.id, id, data: sealed });
        ids.push(id);
        upload.uploaded++;
      }
      hashes.push(h);
      size += block.length;
      if (paged()) await cutPages(false);
    }

    let manifest: ManifestV2;
    if (paged()) {
      const info = await this.info();
      if (!info.features.includes('fs_pages')) {
        throw new ZenError(
          413,
          'too_large',
          `a file of ${ids.length} chunks needs paged versions, which this server lacks`,
        );
      }
      await cutPages(true);
      manifest = { version: 2, size, chunkSize, n: ids.length, ...keyOf, pageSize, pages };
    } else {
      manifest = { version: 2, size, chunkSize, n: ids.length, ...keyOf, chunks: ids };
    }
    const sealedManifest = keys.sealManifestV2(this.id, node, manifest);
    const listed = paged() ? pages.length : ids.length;
    if (sealedManifest.length + 16 * listed > limits.max_value_bytes) {
      throw new ZenError(
        413,
        'too_large',
        `a file of ${ids.length} chunks exceeds what one manifest holds`,
      );
    }
    const index: ChunkIndex = { chunkSize, chunks: ids, hashes, ...keyOf };
    if (paged()) Object.assign(index, { pageSize, pages });
    Object.assign(upload, {
      node,
      manifest,
      sealedManifest,
      index,
      pages: paged() ? pages.map((p) => p.id) : [],
    });
    return {
      upload,
      flush,
      pending: () => pending,
      pendingPages: () => pendingPages,
      pendingOps: () => pending.length + pendingPages.length,
      pendingBytes: () => pendingBytes,
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
        ...(v.pages.length ? { pages: v.pages } : {}),
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
  async send(
    ops: PendingOp[],
    chunks: ChunkPut[] = [],
    pages: PagePut[] = [],
  ): Promise<CommitResult> {
    for (let attempt = 1; ; attempt++) {
      const p = await this.prepare(ops);
      try {
        const r = await sendCommit(this.session, {
          commit_id: randomBytes(16),
          ...(chunks.length ? { chunks } : {}),
          ...(pages.length ? { pages } : {}),
          crdt_ops: p.ops,
        });
        this.accept(p);
        return r;
      } catch (e) {
        if (!(await this.rebase(p, e, attempt))) throw e;
      }
    }
  }

  /**
   * Add operations (and chunks) to a transaction's commit. They are
   * stamped now; if the commit fails with `stale_op` or `clock_skew` they
   * are rebased as in `send`, and the transaction runs again (calling this
   * again stamps them afresh).
   */
  async writeIn(tx: Transaction, ops: PendingOp[], chunks: ChunkPut[] = []): Promise<Prepared> {
    const p = await this.prepare(ops);
    tx.addCrdtOps(p.ops);
    if (chunks.length) tx.addChunks(chunks);
    tx.onCommit(() => this.accept(p));
    tx.onError((e, attempt) => this.rebase(p, e, attempt));
    return p;
  }

  /** Assign timestamps (reading the server clock once first). */
  async prepare(ops: PendingOp[]): Promise<Prepared> {
    if (ops.some((o) => o.op !== 'write' && o.hlc === undefined)) {
      await this.clock.sync(this.session.client);
    }
    const hlcs: bigint[] = [];
    const wire: CrdtOp[] = ops.map((o) => {
      const base = { fs: this.fs.id, tree: this.id, node: o.node };
      if (o.op === 'write') {
        return {
          op: 'write',
          ...base,
          replaces: o.replaces,
          chunks: o.chunks,
          ...(o.pages?.length ? { pages: o.pages } : {}),
          manifest: o.manifest,
        };
      }
      let hlc = o.hlc;
      if (hlc === undefined) hlc = this.clock.tick();
      else this.clock.observe(hlc);
      hlcs.push(hlc);
      if (o.op === 'meta') return { op: 'meta', ...base, hlc, meta: o.meta };
      return { op: 'move', ...base, parent: o.parent, hlc, ...(o.meta ? { meta: o.meta } : {}) };
    });
    return { ops: wire, hlcs, pending: ops };
  }

  /** Note that the server accepted prepared operations. */
  accept(p: Prepared): void {
    for (const h of p.hlcs) this.clock.accept(h);
    this.record(p.ops);
  }

  /**
   * After a failed commit of prepared operations: whether they can be
   * prepared again and resent (the clock was moved for a rebase).
   */
  async rebase(p: Prepared, e: unknown, attempt: number): Promise<boolean> {
    if (attempt >= 3 || !(e instanceof ZenError) || !p.hlcs.length) return false;
    const client = this.session.client;
    // Timestamps taken earlier are ticked afresh.
    const fresh = () => {
      for (const o of p.pending) if (o.op !== 'write') delete o.hlc;
    };
    if (e.code === 'stale_op' && !e.message.includes('purged')) {
      await this.clock.sync(client, true);
      if (attempt === 1) {
        // Observe what the ops touch: their later moves are what the
        // server would have had to undo.
        const touched = p.pending.flatMap((o) => (o.op === 'move' ? [o.node, o.parent] : [o.node]));
        await this.get(touched);
      } else {
        // Later moves on other nodes: go past anything within the skew.
        const skew = Number((await this.limits()).crdt_max_skew_ms ?? 60_000n);
        this.clock.observe(zw.hlc(this.clock.now() + Math.floor(skew / 2), 0));
      }
      fresh();
      return true;
    }
    if (e.code === 'clock_skew') {
      await this.clock.sync(client, true);
      const skew = Number((await this.limits()).crdt_max_skew_ms ?? 60_000n);
      if (zw.hlcMs(this.clock.last) > this.clock.now() + skew / 2)
        this.clock.reset(this.clock.now());
      fresh();
      return true;
    }
    return false;
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
      if (o.pages?.length) {
        return zw.writePagedOpBytes(o.fs, o.tree, o.node, o.replaces ?? [], o.pages, o.manifest);
      }
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

  /** Add the batch to a transaction's commit (`Tree.writeIn`). */
  commitIn(tx: Transaction): Promise<Prepared> {
    return this.tree.writeIn(tx, this.ops);
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

function sameIds(a: Uint8Array[], b: Uint8Array[]): boolean {
  return a.length === b.length && a.every((x, i) => equal(x, b[i]!));
}

/** A web stream as an async iterable (not every browser makes it one). */
async function* streamParts(s: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
  const reader = s.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      yield value;
    }
  } finally {
    reader.releaseLock();
  }
}

/**
 * Split bytes or a stream of byte arrays into `size`-byte blocks (the last
 * shorter). Known chunks pass through, at a block boundary.
 */
async function* blocks(data: FileSource, size: number): AsyncGenerator<Uint8Array | KnownChunk> {
  if (data instanceof Uint8Array) {
    for (let i = 0; i < data.length; i += size) yield data.subarray(i, i + size);
    return;
  }
  const parts =
    typeof (data as ReadableStream).getReader === 'function'
      ? streamParts(data as ReadableStream<Uint8Array>)
      : (data as AsyncIterable<Uint8Array | KnownChunk> | Iterable<Uint8Array | KnownChunk>);
  let buf = new Uint8Array(size);
  let fill = 0;
  for await (const part of parts) {
    if (!(part instanceof Uint8Array)) {
      if (fill > 0) {
        throw new ZenError(0, 'bad_value', 'a known chunk must start at a chunk boundary');
      }
      yield part;
      continue;
    }
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
