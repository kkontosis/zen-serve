// The CRDT filesystem: tree operations, files, chunks and the change feed
// (spec/fs.md, api.md §6 and §12, formats.md §11).
import type {
  AnyPage,
  CdcParams,
  ChunkPut,
  CommitResult,
  CrdtOp,
  Info,
  Limits,
  ManifestV2,
  ManifestV3,
  NodeMeta,
  NodeState,
  PagePut,
  PageRef,
  PageRefV3,
  StateNode,
  UploadRef,
  Versions,
  AnyManifest as WasmAnyManifest,
} from '@zen/wasm';
import { compare, concat, equal, hex, randomBytes } from './bytes.js';

/** A manifest of any version (formats.md §11.3): 1, 2 (fixed chunks) or 3 (content-defined). */
export type AnyManifest = WasmAnyManifest;
/** A manifest this client writes: version 2 (fixed chunks) or 3 (content-defined). */
export type FileManifest = ManifestV2 | ManifestV3;
/** How a file's content is cut into chunks (formats.md §11.7, §12). */
export type Chunking = 'fixed' | 'cdc';

import type { Client } from './client.js';
import type { TreeClock } from './clock.js';
import { isCode, ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import { sendCommit, type Transaction } from './kv.js';
import type { Session } from './session.js';
import { zw } from './wasm.js';
import {
  editSource,
  locate,
  OP_OVERHEAD,
  overflows,
  type Staged,
  sha256,
  splitBy,
  stage,
} from './writer.js';

/** The root of every tree (formats.md §11). */
export const ROOT: Uint8Array = new Uint8Array(16);
/** Deleted nodes live under TRASH until purged (fs.md §6). */
export const TRASH: Uint8Array = new Uint8Array(16).fill(0xff);

/** The plaintext chunk size clients use (fs.md §4.1). */
export const CHUNK_SIZE = 65536;

const MAX_GET_NODES = 1000;
const MAX_GET_CHUNKS = 64;
const MAX_GET_PAGES = 64;
const FETCH_PARALLEL = 4;
/** Plaintext bytes a `readStream` window covers (rounded out to whole chunks). */
const STREAM_WINDOW = 4 << 20;
/** Windows `readStream` fetches ahead by default. */
const READ_AHEAD = 3;
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
 * manifest is version 1 (old writers), 2 (fixed chunks) or 3 (chunks cut
 * by content), formats.md §11.3: inline or paged, its chunks under the fs
 * keys or a file key of its own.
 */
export interface FileVersion {
  dot: Uint8Array;
  /** The writing device's fingerprint. */
  device: Uint8Array;
  manifest: AnyManifest;
  /** `fixed` (versions 1 and 2) or `cdc` (version 3). */
  chunking: Chunking;
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
 * What a writer knows of a version's content, for reuse: its chunk ids and
 * lengths, and a SHA-256 per chunk when its plaintext was seen, kept in
 * memory only (never sent). `writeFile` returns one for what it wrote;
 * `chunkIndex` builds one from a version (its bytes, or only its pages).
 */
export interface ChunkIndex {
  /** Fixed chunks: plaintext bytes per chunk. Absent under CDC. */
  chunkSize?: number;
  /** CDC: the chunker's parameters. */
  cdc?: CdcParams;
  chunks: Uint8Array[];
  /** Plaintext lengths (CDC). */
  lens?: number[];
  /** Plaintext SHA-256s; absent when not every chunk's plaintext was seen. */
  hashes?: Uint8Array[];
  /** The file key the chunks are sealed under; absent: under the fs keys. */
  fileKey?: Uint8Array;
  /** The file key's epoch. */
  keyEpoch?: number;
  /** The most ids per page, when the version is paged: pages whose ids didn't change are reused. */
  pageSize?: number;
  pages?: (PageRef | PageRefV3)[];
}

/**
 * A chunk of the base version (`previous`, or `base`) taken over without
 * its plaintext. Where it doesn't fall on a cut of the new content, the
 * writer fetches its plaintext and cuts it again (resyncing with the base).
 */
export interface KnownChunk {
  id: Uint8Array;
  /** Its plaintext SHA-256, when known (from an index). */
  hash?: Uint8Array;
  size: number;
  /**
   * Whether it is the base's last chunk (cut by the end of the file, not
   * its content, so taken over only if it stays last); default looked up.
   */
  last?: boolean;
}

/**
 * A whole page of the base version (`WriteOptions.base`) taken over
 * without fetching it, when it falls on a cut; otherwise the writer
 * fetches it and takes its chunks over one by one.
 */
export interface KnownPage {
  page: PageRef | PageRefV3;
}

/**
 * A file's content: bytes, or a stream of byte arrays of any sizes, which
 * may hold known chunks and pages of the base version.
 */
export type FileSource =
  | Uint8Array
  | AsyncIterable<Uint8Array | KnownChunk | KnownPage>
  | Iterable<Uint8Array | KnownChunk | KnownPage>
  | ReadableStream<Uint8Array>;

/** A change of `Tree.edit`: `remove` bytes at `offset` of the base, then `insert` there. */
export interface Edit {
  offset: number;
  remove?: number;
  insert?: Uint8Array;
}

/** An entry of an upload's tail (`UploadState.tail`). */
export interface TailEntry {
  id: Uint8Array;
  len: number;
  hash?: Uint8Array;
}

/** An open upload session (api.md §12.1). */
export interface UploadSessionInfo {
  session: Uint8Array;
  /** Its expiry on the server's clock (unix ms). */
  expiresMs: number;
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
  /**
   * The version the source's known pages (and known chunks) are of; their
   * plaintext is fetched from it where they don't fall on a cut.
   */
  base?: FileVersion;
  /**
   * `fixed` or `cdc` (default: `fixed` when `chunkSize` is given, else the
   * form of `base` or `previous`, so their chunks are reused, else the fs
   * policy). `cdc` falls back to `fixed` on a server without `fs_cdc`,
   * with no chunker parameters for its `max_value_bytes`, or for a known
   * size past what a CDC version holds.
   */
  chunking?: Chunking;
  /** Fixed: plaintext bytes per chunk (default 64 KiB, or less on a server with a small `max_value_bytes`). */
  chunkSize?: number;
  /** CDC: the chunker's parameters (default those of `base` or `previous`, else `TreeOptions.cdc`, else `cdcDefaults`). */
  cdc?: CdcParams;
  /** The content's size, when known ahead (a `Uint8Array` is): picks `fixed` past the CDC capacity. */
  size?: number;
  /** Upload commits in flight at once (default 3, 1–8). */
  parallel?: number;
  /**
   * Upload under an upload session (api.md §12.1), so the content is kept
   * past `chunk_grace_secs` until the write: `writeFile` closes it after
   * the write; `upload` leaves it open (`Upload.session`, `closeUpload`).
   */
  session?: boolean;
  /** The session's lifetime (default the server's `upload_ttl_secs`); renewed as the upload goes. */
  sessionTtlSecs?: number;
}

/**
 * Where an `upload` stands after a commit: plain data to keep (it holds the
 * file key, so keep it sealed, e.g. on a `SealedStore`) and to pass as
 * `resume` with the rest of the source. Everything in it is on the server.
 * Its size is the pages done plus at most one page of chunks.
 */
export interface UploadState {
  node: Uint8Array;
  chunking: Chunking;
  /** Fixed: bytes per chunk. */
  chunkSize?: number;
  /** CDC: the chunker's parameters. */
  cdc?: CdcParams;
  /** Bytes uploaded so far: up to a cut (a chunk's end). */
  size: number;
  /** The pages done (paged files). */
  pages: (PageRef | PageRefV3)[];
  /** The chunks since the last page (all of them while the file is inline). */
  tail: TailEntry[];
  /** Whether the file is paged. */
  paged: boolean;
  /** The file key and its epoch; absent: chunks under the fs keys. */
  fileKey?: Uint8Array;
  keyEpoch?: number;
  /** The upload session, when the upload has one. */
  session?: Uint8Array;
  uploaded: number;
  pagesUploaded: number;
  commits: number;
}

/** What one upload commit put (the second argument of `onProgress`). */
export interface UploadCommit {
  chunks: Uint8Array[];
  pages: Uint8Array[];
}

/** Options of `upload`. */
export interface UploadOptions extends Omit<WriteOptions, 'replaces'> {
  /** Called after each commit of the upload, in commit order. */
  onProgress?: (state: UploadState, commit: UploadCommit) => void | Promise<void>;
  /**
   * Continue an upload from its last `onProgress` state; the source is then
   * the rest of the file, from `state.size`. Works while the uploaded chunks
   * are within `chunk_grace_secs`, or while its session is open.
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
  manifest: FileManifest;
  /**
   * The new content's index, for the next write's `previous`; absent when
   * the write took pages over unread (`edit`), or resumed past a page.
   */
  index?: ChunkIndex;
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
  manifest: FileManifest;
  /** The manifest sealed for this tree and node. */
  sealedManifest: Uint8Array;
  /** The content's index, for the next write's `previous` (absent as in `WrittenFile`). */
  index?: ChunkIndex;
  /** Page ids when paged (the write lists these, not the chunks); empty when inline. */
  pages: Uint8Array[];
  /** Chunks uploaded (the rest were reused). */
  uploaded: number;
  /** Pages uploaded (the rest were reused). */
  pagesUploaded: number;
  /** Chunk ids when inline (the write lists these); empty when paged. */
  chunks: Uint8Array[];
  /** Commits the upload took. */
  commits: number;
  /** The upload session it was uploaded under, still open (close it after the write). */
  session?: Uint8Array;
}

/** Options of `edit` and `append`. */
export interface EditOptions extends WriteOptions {
  /** Re-cut every page full (the writer does this itself when a version would overflow). */
  repack?: boolean;
}

/** Options of `readStream`. */
export interface ReadStreamOptions {
  /** A version or its dot; default the newest. */
  version?: FileVersion | Uint8Array;
  /** First byte (default 0). */
  offset?: number;
  /** Bytes to read (default to the end). */
  length?: number;
  /** Windows (of about 4 MiB) fetched ahead (default 3). */
  readAhead?: number;
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
  /** The chunker's parameters for new content under CDC (default `cdcDefaults(max_value_bytes)`). */
  cdc?: CdcParams;
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
      const manifest = this.fs.keysFor(v.manifest).openAnyManifest(this.id, node, v.manifest);
      const chunks = v.chunks ?? [];
      const pages = v.pages ?? [];
      const inline = inlineIds(manifest);
      const refs = pageRefsOf(manifest);
      const same = inline
        ? !pages.length && sameIds(inline, chunks)
        : !chunks.length &&
          sameIds(
            refs!.map((p) => p.id),
            pages,
          );
      if (!same)
        throw new ZenError(0, 'format', 'a manifest disagrees with its chunk or page list');
      return {
        dot: v.dot,
        device: v.device,
        manifest,
        chunking: manifest.version === 3 ? 'cdc' : 'fixed',
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

  /** The newest version of `node` (or the one given by its dot); undefined when it has no content. */
  version(node: Uint8Array, version?: FileVersion | Uint8Array): Promise<FileVersion | undefined> {
    return this.pickVersion(node, version);
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
    const end = Math.min(v.size, offset + length);
    if (offset >= end) return new Uint8Array(0);
    return this.readSpans(v, await this.spans(v, offset, end), offset, end);
  }

  /** Bytes `offset..end` from the chunks `spans` (which cover them). */
  private async readSpans(
    v: FileVersion,
    spans: Span[],
    offset: number,
    end: number,
  ): Promise<Uint8Array> {
    const data = await this.fetchChunks(
      spans.map((s) => s.id),
      v,
    );
    const out = new Uint8Array(end - offset);
    for (const s of spans) {
      const plain = data.get(hex(s.id))!;
      if (plain.length !== s.len) {
        throw new ZenError(0, 'format', `the chunk at byte ${s.start} has the wrong size`);
      }
      const from = Math.max(offset, s.start) - s.start;
      const to = Math.min(end, s.start + s.len) - s.start;
      out.set(plain.subarray(from, to), s.start + from - offset);
    }
    return out;
  }

  /**
   * A version's bytes as a stream, fetched lazily in windows of about
   * 4 MiB (whole chunks), `readAhead` windows ahead (default 3), so memory
   * stays at a few windows. Cancel it to stop fetching.
   */
  readStream(node: Uint8Array, opts: ReadStreamOptions = {}): ReadableStream<Uint8Array> {
    let v: FileVersion | undefined;
    let end = 0;
    let next = opts.offset ?? 0;
    let started = false;
    let cancelled = false;
    const ahead = Math.max(0, opts.readAhead ?? READ_AHEAD);
    const queue: Promise<Uint8Array>[] = [];
    const plan = async () => {
      while (!cancelled && next < end && queue.length <= ahead) {
        const a = next;
        const spans = await this.spans(v!, a, Math.min(end, a + STREAM_WINDOW));
        const last = spans.at(-1)!;
        const b = Math.min(end, last.start + last.len);
        next = b;
        const p = this.readSpans(v!, spans, a, b);
        p.catch(() => {});
        queue.push(p);
      }
    };
    return new ReadableStream<Uint8Array>({
      pull: async (ctl) => {
        if (!started) {
          started = true;
          v = await this.pickVersion(node, opts.version);
          end = v ? Math.min(v.size, opts.length === undefined ? v.size : next + opts.length) : 0;
        }
        if (v) await plan();
        const p = queue.shift();
        if (!p) {
          ctl.close();
          return;
        }
        ctl.enqueue(await p);
      },
      cancel: () => {
        cancelled = true;
        queue.length = 0;
      },
    });
  }

  private readonly layouts = new WeakMap<object, Layout>();

  /**
   * Where each page of a paged version starts, in bytes and in chunks
   * (one more entry than pages, for the end); for an inline version of
   * variable chunks, where each chunk starts. Computed once per manifest.
   */
  layout(v: FileVersion): Layout {
    const m = v.manifest;
    let l = this.layouts.get(m);
    if (l) return l;
    const bytes: number[] = [0];
    const chunks: number[] = [0];
    if (m.version === 3) {
      if (m.pages) {
        for (const p of m.pages) {
          bytes.push(bytes.at(-1)! + p.bytes);
          chunks.push(chunks.at(-1)! + p.count);
        }
      } else {
        for (const c of m.chunks!) {
          bytes.push(bytes.at(-1)! + c.len);
          chunks.push(chunks.length);
        }
      }
    } else if (m.version === 2 && m.pages) {
      const ps = m.pageSize!;
      for (let p = 1; p <= m.pages.length; p++) {
        chunks.push(Math.min(m.n, p * ps));
        bytes.push(Math.min(m.size, p * ps * m.chunkSize));
      }
    }
    l = { bytes, chunks };
    this.layouts.set(m, l);
    return l;
  }

  /** The chunks bytes `offset..end` of a version fall in, fetching only their pages. */
  async spans(v: FileVersion, offset: number, end: number): Promise<Span[]> {
    const m = v.manifest;
    if (offset >= end) return [];
    if (m.version !== 3) {
      const cs = m.chunkSize;
      const first = Math.floor(offset / cs);
      const last = Math.floor((end - 1) / cs);
      const ids = await this.chunkIds(v, first, last);
      return ids.map((id, j) => {
        const i = first + j;
        return { id, start: i * cs, len: i === v.chunkCount - 1 ? m.size - i * cs : cs };
      });
    }
    const out: Span[] = [];
    const lay = this.layout(v);
    const take = (ids: Uint8Array[], lens: number[], start: number) => {
      for (let i = 0; i < ids.length && start < end; i++) {
        if (start + lens[i]! > offset) out.push({ id: ids[i]!, start, len: lens[i]! });
        start += lens[i]!;
      }
    };
    if (!m.pages) {
      const i = locate(lay.bytes, offset);
      const entries = m.chunks!;
      for (let j = i, start = lay.bytes[i]!; j < entries.length && start < end; j++) {
        out.push({ id: entries[j]!.id, start, len: entries[j]!.len });
        start += entries[j]!.len;
      }
      return out;
    }
    const p0 = locate(lay.bytes, offset);
    const p1 = locate(lay.bytes, end - 1);
    const pages = await this.pagesOf(v, p0, p1);
    for (let p = p0; p <= p1; p++) take(pages[p - p0]!.ids, pages[p - p0]!.lens, lay.bytes[p]!);
    return out;
  }

  /** The chunk ids `first..=last` of a version, through its pages when paged. */
  async chunkIds(v: FileVersion, first: number, last: number): Promise<Uint8Array[]> {
    return (await this.entries(v, first, last)).ids;
  }

  /** The chunks `first..=last` of a version, ids and plaintext lengths, through its pages when paged. */
  async entries(v: FileVersion, first: number, last: number): Promise<PageEntries> {
    const m = v.manifest;
    if (last < first) return { ids: [], lens: [] };
    const fixedLen = (i: number) =>
      m.version === 3 ? 0 : i === v.chunkCount - 1 ? m.size - i * m.chunkSize : m.chunkSize;
    const range = (from: number, to: number) => {
      const lens: number[] = [];
      for (let i = from; i <= to; i++) lens.push(fixedLen(i));
      return lens;
    };
    if (m.version === 1 || (m.version === 2 && m.chunks)) {
      const ids = (m.chunks as Uint8Array[]).slice(first, last + 1);
      return { ids, lens: range(first, first + ids.length - 1) };
    }
    if (m.version === 3 && m.chunks) {
      const e = m.chunks.slice(first, last + 1);
      return { ids: e.map((c) => c.id), lens: e.map((c) => c.len) };
    }
    const lay = this.layout(v);
    const p0 = locate(lay.chunks, first);
    const p1 = locate(lay.chunks, last);
    const ids: Uint8Array[] = [];
    const lens: number[] = [];
    for (const pg of await this.pagesOf(v, p0, p1)) {
      ids.push(...pg.ids);
      lens.push(...pg.lens);
    }
    const from = first - lay.chunks[p0]!;
    return {
      ids: ids.slice(from, from + last - first + 1),
      lens: lens.slice(from, from + last - first + 1),
    };
  }

  private readonly pageMemo = new Map<string, AnyPage>();

  /**
   * Pages `p0..=p1` of a paged version, each checked against its digest,
   * its count and (version 3) its bytes: their chunk ids and lengths.
   */
  async pagesOf(v: FileVersion, p0: number, p1: number): Promise<PageEntries[]> {
    const refs = pageRefsOf(v.manifest)!;
    const out: (PageEntries | undefined)[] = [];
    const todo: number[] = [];
    const cache = this.opts.pageCache;
    const decode = (bytes: Uint8Array): AnyPage | undefined => {
      try {
        return zw.decodeAnyPage(bytes);
      } catch {
        return undefined;
      }
    };
    const check = (p: number, bytes: Uint8Array): [AnyPage, PageEntries] | undefined => {
      if (!equal(zw.pageDigest(bytes), refs[p]!.digest)) return undefined;
      const d = decode(bytes);
      const e = d && this.pageEntries(v, p, d);
      return e && [d, e];
    };
    for (let p = p0; p <= p1; p++) {
      const key = hex(refs[p]!.digest);
      let e: PageEntries | undefined;
      const memo = this.pageMemo.get(key);
      if (memo) {
        // Most recently used last.
        this.pageMemo.delete(key);
        this.pageMemo.set(key, memo);
        e = this.pageEntries(v, p, memo);
      } else if (cache) {
        const bytes = await cache.get(refs[p]!.id);
        const c = bytes && check(p, bytes);
        if (c) {
          this.pageMemo.set(key, c[0]);
          e = c[1];
        }
      }
      out.push(e);
      if (!e) todo.push(p);
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
        const c = check(p, d.data);
        if (!c) throw new ZenError(0, 'format', `page ${p} doesn't match its manifest`);
        await cache?.put(refs[p]!.id, d.data);
        this.pageMemo.set(hex(refs[p]!.digest), c[0]);
        out[p - p0] = c[1];
      }
    }
    while (this.pageMemo.size > PAGE_MEMO) {
      this.pageMemo.delete(this.pageMemo.keys().next().value!);
    }
    return out as PageEntries[];
  }

  /** A decoded page as page `p` of `v`: its entries, or undefined when it doesn't fit there. */
  private pageEntries(v: FileVersion, p: number, d: AnyPage): PageEntries | undefined {
    const m = v.manifest;
    const lay = this.layout(v);
    if (m.version === 3) {
      const ref = m.pages![p]!;
      if (!d.lens || d.ids.length !== ref.count) return undefined;
      let sum = 0;
      for (const l of d.lens) {
        if (l < 1 || l > m.cdc.max) return undefined;
        sum += l;
      }
      return sum === ref.bytes ? { ids: d.ids, lens: d.lens } : undefined;
    }
    if (m.version !== 2 || d.lens) return undefined;
    const first = lay.chunks[p]!;
    if (d.ids.length !== lay.chunks[p + 1]! - first) return undefined;
    const lens = d.ids.map((_, j) =>
      first + j === m.n - 1 ? m.size - (m.n - 1) * m.chunkSize : m.chunkSize,
    );
    return { ids: d.ids, lens };
  }

  /**
   * Fetch and open chunks (64 per request, a few requests at a time), by hex
   * id. `version` (or a file key) says how they are sealed (default: under
   * the fs keys).
   */
  async fetchChunks(
    ids: Uint8Array[],
    version?: FileVersion | ChunkKey,
  ): Promise<Map<string, Uint8Array>> {
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
  private opener(v?: FileVersion | ChunkKey): (id: Uint8Array, sealed: Uint8Array) => Uint8Array {
    const k = v && 'manifest' in v ? chunkKeyOf(v.manifest) : v;
    if (k?.fileKey) {
      const { fileKey, keyEpoch } = k;
      return (id, sealed) => zw.openFileChunk(this.fs.id, fileKey, keyEpoch!, id, sealed);
    }
    return (id, sealed) => this.fs.keysFor(sealed).openChunk(id, sealed);
  }

  /**
   * A version's chunk index: its ids and lengths (from the manifest and
   * pages), and with `hashes` (default) a SHA-256 per chunk from its bytes,
   * `data` if given, else read as a stream, so the file is never held whole.
   */
  async chunkIndex(
    node: Uint8Array,
    version: FileVersion,
    data?: Uint8Array,
    opts: { hashes?: boolean } = {},
  ): Promise<ChunkIndex> {
    const m = version.manifest;
    const n = version.chunkCount;
    const all = await this.entries(version, 0, n - 1);
    const index: ChunkIndex =
      m.version === 3
        ? { cdc: m.cdc, chunks: all.ids, lens: all.lens }
        : { chunkSize: m.chunkSize, chunks: all.ids };
    if (opts.hashes !== false) {
      const hashes: Uint8Array[] = [];
      const src = data ?? this.readStream(node, { version });
      for await (const block of splitBy(src, all.lens)) hashes.push(await sha256(block));
      if (hashes.length !== n) {
        throw new ZenError(0, 'format', 'a version has the wrong number of chunks');
      }
      index.hashes = hashes;
    }
    if (m.version !== 1) {
      if (m.fileKey) Object.assign(index, { fileKey: m.fileKey, keyEpoch: m.keyEpoch });
      if (m.pages) Object.assign(index, { pageSize: m.pageSize, pages: m.pages });
    }
    return index;
  }

  /**
   * Write a file's content (fs.md §4.1): cut by content (version 3, the fs
   * policy's default `chunking: cdc`) or into fixed-size chunks (version
   * 2), with random ids, uploaded in commits of `max_commit_bytes` (a few
   * in flight), then the `write` op with the last chunks. A file too long
   * for one inline manifest is paged (formats.md §11.6). With `previous`, a
   * chunk whose bytes equal one of the previous version's keeps that
   * chunk's id and isn't uploaded (when both are under the same key), and
   * so does a page whose ids didn't change; that version should still
   * exist (or have been replaced within `chunk_grace_secs`), else the write
   * fails with 400. With `session`, the upload runs under an upload session,
   * closed after the write.
   */
  async writeFile(
    node: Uint8Array,
    data: FileSource,
    opts: WriteOptions = {},
  ): Promise<WrittenFile> {
    return this.finish(await stage(this, node, data, opts), opts.replaces);
  }

  /** The write commit of a staged upload (with its last puts), then its session closed. */
  private async finish(
    st: Staged,
    replaces: Uint8Array[] | 'current' | undefined,
  ): Promise<WrittenFile> {
    const op = await this.writeOp(st.upload, replaces);
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
    await st.drain();
    const r = await this.send([op], st.pending(), st.pendingPages(), st.ref);
    const u = st.upload;
    if (u.session) {
      // The version holds the content now; a session left open expires.
      await this.closeUpload(u.session).catch(() => {});
    }
    return {
      dot: r.dots![0]!,
      manifest: u.manifest,
      ...(u.index ? { index: u.index } : {}),
      uploaded: u.uploaded,
      pagesUploaded: u.pagesUploaded,
      commits: u.commits + 1,
    };
  }

  /**
   * Upload a file's content without writing it (as `writeFile`, every
   * chunk and page in commits of its own). Write it later with `writeOp`,
   * alone (`send`) or in a transaction (`writeIn`). Chunks never written
   * are collected after `chunk_grace_secs`, or after the upload session
   * (`session`) is closed or expires: close it (`closeUpload`) once
   * written. `onProgress` and `resume` make it resumable.
   */
  async upload(node: Uint8Array, data: FileSource, opts: UploadOptions = {}): Promise<Upload> {
    const st = await stage(this, node, data, opts);
    await st.flush();
    await st.drain();
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

  /**
   * Edit a version (default the newest) and write the result: each edit
   * removes bytes at an offset and inserts others there. Only the pages
   * the edits touch are fetched and uploaded, and the chunks around each
   * edit, until the cuts resync with the base's; untouched pages are taken
   * over unread. The base keeps its form (`fixed` or `cdc`) unless
   * `chunking` says otherwise. Under fixed chunks an insertion or deletion
   * of other than whole chunks shifts every later chunk, so the rest of
   * the file is read and uploaded again.
   */
  async edit(
    node: Uint8Array,
    version: FileVersion | Uint8Array | undefined,
    edits: Edit[],
    opts: EditOptions = {},
  ): Promise<WrittenFile> {
    const v = await this.pickVersion(node, version);
    return repacking(opts.repack, async (repack) =>
      this.finish(await this.stageEdit(node, v, edits, opts, repack), opts.replaces),
    );
  }

  /** `edit` without the write: the content uploaded (or sealed into `sink`), as `upload`. */
  async editUpload(
    node: Uint8Array,
    version: FileVersion | Uint8Array | undefined,
    edits: Edit[],
    opts: UploadOptions & { repack?: boolean } = {},
  ): Promise<Upload> {
    const v = await this.pickVersion(node, version);
    return repacking(opts.repack, async (repack) => {
      const st = await this.stageEdit(node, v, edits, opts, repack);
      await st.flush();
      await st.drain();
      return st.upload;
    });
  }

  /** Append bytes to the newest version (an `edit` at its end; a new file's content when it has none). */
  async append(node: Uint8Array, data: Uint8Array, opts: EditOptions = {}): Promise<WrittenFile> {
    const v = await this.pickVersion(node);
    return this.edit(node, v, [{ offset: v?.size ?? 0, insert: data }], opts);
  }

  private stageEdit(
    node: Uint8Array,
    v: FileVersion | undefined,
    edits: Edit[],
    opts: UploadOptions,
    repack: boolean,
  ): Promise<Staged> {
    if (!v) {
      if (edits.some((e) => e.offset !== 0 || (e.remove ?? 0) !== 0)) {
        throw new ZenError(0, 'bad_value', 'edits must be within the file');
      }
      return stage(this, node, concat(...edits.map((e) => e.insert ?? new Uint8Array(0))), opts);
    }
    const size = edits.reduce((a, e) => a - (e.remove ?? 0) + (e.insert?.length ?? 0), v.size);
    return stage(this, node, editSource(this, v, edits), {
      ...opts,
      base: v,
      chunking: opts.chunking ?? v.chunking,
      size,
      repack,
    });
  }

  /** The caller's open upload sessions on this fs (api.md §12.1). */
  async uploads(): Promise<UploadSessionInfo[]> {
    const r = await this.session.call(
      '/v1/fs/upload/list',
      zw.encodeUploadList,
      zw.decodeUploadSessions,
      { fs: this.fs.id },
    );
    return r.sessions.map((s) => ({ session: s.session, expiresMs: Number(s.expires_ms) }));
  }

  /** Close an upload session: what it holds is released (and collected after `chunk_grace_secs` unless written). */
  async closeUpload(session: Uint8Array): Promise<void> {
    await this.session.call('/v1/fs/upload/close', zw.encodeUploadClose, zw.decodeEmpty, {
      fs: this.fs.id,
      session,
    });
  }

  /** The `write` operation of an upload, replacing exactly `replaces` (no read). */
  writeOpOf(u: Upload, replaces: Uint8Array[]): Extract<PendingOp, { op: 'write' }> {
    return {
      op: 'write',
      node: u.node,
      replaces,
      chunks: u.pages.length ? [] : u.chunks,
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
    upload?: UploadRef,
  ): Promise<CommitResult> {
    for (let attempt = 1; ; attempt++) {
      const p = await this.prepare(ops);
      try {
        const r = await sendCommit(this.session, {
          commit_id: randomBytes(16),
          ...(chunks.length ? { chunks } : {}),
          ...(pages.length ? { pages } : {}),
          ...(upload ? { upload } : {}),
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

function sameIds(a: Uint8Array[], b: Uint8Array[]): boolean {
  return a.length === b.length && a.every((x, i) => equal(x, b[i]!));
}

/** A chunk a byte range falls in: its id, where it starts and its length. */
export interface Span {
  id: Uint8Array;
  start: number;
  len: number;
}

/** Chunk ids and their plaintext lengths (a page's, or a run of a version's). */
export interface PageEntries {
  ids: Uint8Array[];
  lens: number[];
}

/** Where each page (or, inline under CDC, each chunk) of a version starts: in bytes and in chunks. */
export interface Layout {
  bytes: number[];
  chunks: number[];
}

/** A file key and its epoch, for opening chunks. */
export interface ChunkKey {
  fileKey?: Uint8Array;
  keyEpoch?: number;
}

function inlineIds(m: AnyManifest): Uint8Array[] | undefined {
  if (m.version === 3) return m.chunks?.map((c) => c.id);
  return m.chunks;
}

function pageRefsOf(m: AnyManifest): (PageRef | PageRefV3)[] | undefined {
  return m.version === 1 ? undefined : m.pages;
}

function chunkKeyOf(m: AnyManifest): ChunkKey | undefined {
  return m.version !== 1 && m.fileKey ? { fileKey: m.fileKey, keyEpoch: m.keyEpoch } : undefined;
}

async function repacking<T>(
  repack: boolean | undefined,
  f: (repack: boolean) => Promise<T>,
): Promise<T> {
  try {
    return await f(!!repack);
  } catch (e) {
    // Pages left short by edits overflowed one manifest: write them again full.
    if (repack || !overflows(e)) throw e;
    return f(true);
  }
}
