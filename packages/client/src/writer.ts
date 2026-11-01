// Writing a file's content (fs.md §4.1, formats.md §11.3–§11.7): fixed or
// content-defined chunks, a base version's chunks and pages taken over,
// pages and their fill rule, pipelined upload commits and upload sessions
// (api.md §12.1). `Tree.writeFile`, `upload`, `edit` and `seal` use it.
import type {
  CdcParams,
  Chunker,
  ChunkPut,
  FsPolicy,
  ManifestV2,
  ManifestV3,
  PagePut,
  PageRef,
  PageRefV3,
  UploadRef,
} from '@zen/wasm';
import { concat, equal, hex, randomBytes } from './bytes.js';
import { isCode, ZenError } from './errors.js';
import { sendCommit } from './kv.js';
import type {
  ChunkIndex,
  Chunking,
  Edit,
  FileManifest,
  FileSource,
  FileVersion,
  KnownChunk,
  KnownPage,
  TailEntry,
  Tree,
  Upload,
  UploadOptions,
  UploadState,
} from './tree.js';
import { zw } from './wasm.js';

/** Per commit operation, as the server counts (api.md §6). */
export const OP_OVERHEAD = 512;
/** The plaintext chunk size of fixed chunks (fs.md §4.1). */
const CHUNK_SIZE = 65536;
/** Upload commits in flight by default. */
const MAX_IN_FLIGHT = 3;
/** A put's `ur` key and reference count, under an upload session (api.md §6). */
const SESSION_COST = 64;

/** Options of `stage`: `upload`'s, a base for known pages, and repacking. */
export interface StageOptions extends UploadOptions {
  repack?: boolean;
}

/** A staged upload: content sealed and sent, the last puts pending for the write. */
export interface Staged {
  upload: Upload;
  /** The upload session's reference, for the write's commit. */
  ref?: UploadRef;
  /** Send the pending puts as a commit (in flight with others). */
  flush(): Promise<void>;
  /** Wait for every commit in flight. */
  drain(): Promise<void>;
  pending(): ChunkPut[];
  pendingPages(): PagePut[];
  pendingOps(): number;
  pendingBytes(): number;
}

type Item = Uint8Array | KnownChunk | KnownPage;

const isPage = (x: Item): x is KnownPage => !(x instanceof Uint8Array) && 'page' in x;

const tooLarge = (what: string) =>
  new ZenError(
    413,
    'too_large',
    `${what} exceeds what one manifest holds; write it with chunking: 'fixed'`,
  );

/** Whether `e` is a write that doesn't fit one manifest (`edit` repacks then). */
export function overflows(e: unknown): boolean {
  return isCode(e, 'too_large') && e.message.includes('one manifest');
}

export async function sha256(b: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', b as Uint8Array<ArrayBuffer>));
}

/** A web stream as an async iterable (not every browser makes it one). */
export async function* streamParts(s: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
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

function items(data: FileSource): AsyncIterable<Item> | Iterable<Item> {
  if (data instanceof Uint8Array) return [data];
  if (typeof (data as ReadableStream).getReader === 'function') {
    return streamParts(data as ReadableStream<Uint8Array>);
  }
  return data as AsyncIterable<Item> | Iterable<Item>;
}

/** Split bytes (or a stream of them) into blocks of the given lengths; the total must match. */
export async function* splitBy(data: FileSource, lens: number[]): AsyncGenerator<Uint8Array> {
  let i = 0;
  let buf: Uint8Array | undefined;
  let fill = 0;
  for await (const part of items(data)) {
    if (!(part instanceof Uint8Array)) throw new ZenError(0, 'bad_value', 'bytes only');
    let p = 0;
    while (p < part.length) {
      if (i >= lens.length) throw new ZenError(0, 'format', 'more bytes than the chunks hold');
      const want = lens[i]!;
      if (!fill && part.length - p >= want) {
        yield part.subarray(p, p + want);
        p += want;
        i++;
        continue;
      }
      buf ??= new Uint8Array(want);
      const n = Math.min(want - fill, part.length - p);
      buf.set(part.subarray(p, p + n), fill);
      fill += n;
      p += n;
      if (fill === want) {
        yield buf;
        buf = undefined;
        fill = 0;
        i++;
      }
    }
  }
  if (fill) throw new ZenError(0, 'format', 'fewer bytes than the chunks hold');
}

/** Cuts bytes into chunks: fixed blocks, or by content. */
interface Cutter {
  /** Feed bytes; the chunks they complete. */
  push(b: Uint8Array): Uint8Array[];
  /** The end: the last chunk, if bytes are left. */
  finish(): Uint8Array | undefined;
  /** Bytes fed since the last cut. */
  readonly pending: number;
  free(): void;
}

function fixedCutter(cs: number): Cutter {
  let buf: Uint8Array | undefined;
  let fill = 0;
  return {
    get pending() {
      return fill;
    },
    push(b) {
      const out: Uint8Array[] = [];
      let p = 0;
      if (fill) {
        const n = Math.min(cs - fill, b.length);
        buf!.set(b.subarray(0, n), fill);
        fill += n;
        p = n;
        if (fill < cs) return out;
        out.push(buf!);
        buf = undefined;
        fill = 0;
      }
      for (; b.length - p >= cs; p += cs) out.push(b.subarray(p, p + cs));
      if (p < b.length) {
        buf = new Uint8Array(cs);
        buf.set(b.subarray(p));
        fill = b.length - p;
      }
      return out;
    },
    finish() {
      if (!fill) return undefined;
      const r = buf!.subarray(0, fill);
      buf = undefined;
      fill = 0;
      return r;
    },
    free() {},
  };
}

function cdcCutter(ch: Chunker): Cutter {
  let carry: Uint8Array[] = [];
  let carryLen = 0;
  return {
    get pending() {
      return carryLen;
    },
    push(b) {
      const out: Uint8Array[] = [];
      let p = 0;
      for (const c of ch.push(b)) {
        const piece = b.subarray(p, c);
        if (carryLen) {
          out.push(concat(...carry, piece));
          carry = [];
          carryLen = 0;
        } else out.push(piece);
        p = c;
      }
      // What follows the last cut is kept (copied: the caller may reuse its buffer).
      if (p < b.length) {
        carry.push(b.slice(p));
        carryLen += b.length - p;
      }
      return out;
    },
    finish() {
      const last = ch.finish();
      if (last !== carryLen) throw new ZenError(0, 'format', 'the chunker lost count');
      if (!carryLen) return undefined;
      const r = concat(...carry);
      carry = [];
      carryLen = 0;
      return r;
    },
    free() {
      ch.free();
    },
  };
}

function cdcEqual(a: CdcParams, b: CdcParams): boolean {
  return a.min === b.min && a.avgBits === b.avgBits && a.max === b.max;
}

/** The file key a version's chunks are under, if any. */
function fileKeyOfVersion(
  v: FileVersion | undefined,
): { key: Uint8Array; epoch: number } | undefined {
  const m = v?.manifest;
  return m && m.version !== 1 && m.fileKey ? { key: m.fileKey, epoch: m.keyEpoch! } : undefined;
}

/**
 * The file key a write of `node` uses under a `file_keys: on` policy
 * (fs.md §4.2): the key of `previous` (or `base`), or else of the node's
 * newest version, unless the policy replaces it after a rotation; a fresh
 * one when there is none.
 */
async function fileKeyFor(
  tree: Tree,
  node: Uint8Array,
  previous: ChunkIndex | undefined,
  base: FileVersion | undefined,
  policy: FsPolicy,
  epoch: number,
): Promise<{ key: Uint8Array; epoch: number }> {
  let from: { key: Uint8Array; epoch: number } | undefined;
  const given = !!previous || !!base;
  if (previous) {
    if (previous.fileKey) from = { key: previous.fileKey, epoch: previous.keyEpoch! };
  } else if (base) {
    from = fileKeyOfVersion(base);
  } else {
    try {
      from = fileKeyOfVersion((await tree.versions(node)).at(-1));
    } catch (e) {
      // Offline (e.g. a replica sealing a write to send later): a fresh key.
      if (!isCode(e, 'network')) throw e;
    }
  }
  const fresh = () => ({ key: zw.newFileKey(), epoch });
  if (!from) return fresh();
  if (from.epoch < epoch) {
    if (policy.rekeyAfterRotate === 'eager') return fresh();
    if (policy.rekeyAfterRotate === 'rewrite' && !given) return fresh();
  }
  return from;
}

/**
 * The source of an edit of `base`: its untouched pages as known pages, the
 * untouched chunks of touched pages as known chunks, the kept bytes of the
 * chunks an edit cuts into, and the inserted bytes.
 */
export async function* editSource(
  tree: Tree,
  base: FileVersion,
  edits: Edit[],
): AsyncGenerator<Item> {
  // The new content as kept ranges of the base and inserts, in order.
  type Seg = { keep: [number, number] } | { insert: Uint8Array };
  const segs: Seg[] = [];
  let pos = 0;
  for (const e of edits) {
    const remove = e.remove ?? 0;
    if (
      !Number.isSafeInteger(e.offset) ||
      !Number.isSafeInteger(remove) ||
      remove < 0 ||
      e.offset < pos ||
      e.offset + remove > base.size
    ) {
      throw new ZenError(0, 'bad_value', 'edits must be in order, apart, and within the file');
    }
    if (e.offset > pos) segs.push({ keep: [pos, e.offset] });
    if (e.insert?.length) segs.push({ insert: e.insert });
    pos = e.offset + remove;
  }
  if (pos < base.size) segs.push({ keep: [pos, base.size] });

  const m = base.manifest;
  const refs = m.version === 1 ? undefined : m.pages;
  const lay = tree.layout(base);
  // A chunk an edit cuts into is read for both its sides.
  const memo = new Map<string, Uint8Array>();
  const fetch = async (id: Uint8Array) => {
    const h = hex(id);
    let plain = memo.get(h);
    if (!plain) {
      plain = (await tree.fetchChunks([id], base)).get(h)!;
      if (memo.size >= 4) memo.delete(memo.keys().next().value!);
      memo.set(h, plain);
    }
    return plain;
  };
  for (const s of segs) {
    if ('insert' in s) {
      yield s.insert;
      continue;
    }
    const [a, b] = s.keep;
    // Whole pages within the range are taken over; the rest chunk by chunk.
    let at = a;
    while (at < b) {
      if (refs) {
        const p = locate(lay.bytes, at);
        const ps = lay.bytes[p]!;
        const pe = lay.bytes[p + 1]!;
        if (ps === at && pe <= b) {
          yield { page: refs[p]! };
          at = pe;
          continue;
        }
        const pg = (await tree.pagesOf(base, p, p))[0]!;
        at = yield* keptChunks(fetch, base.size, pg.ids, pg.lens, ps, at, Math.min(b, pe));
      } else {
        const all = await tree.entries(base, 0, base.chunkCount - 1);
        at = yield* keptChunks(fetch, base.size, all.ids, all.lens, 0, at, b);
      }
    }
  }
}

/** The bytes `a..b` of chunks starting at `start`: whole ones known, cut ones fetched. */
async function* keptChunks(
  fetch: (id: Uint8Array) => Promise<Uint8Array>,
  size: number,
  ids: Uint8Array[],
  lens: number[],
  start: number,
  a: number,
  b: number,
): AsyncGenerator<Item, number> {
  let s = start;
  for (let i = 0; i < ids.length && s < b; s += lens[i]!, i++) {
    const e = s + lens[i]!;
    if (e <= a) continue;
    if (s >= a && e <= b) {
      yield { id: ids[i]!, size: lens[i]!, last: e === size };
      continue;
    }
    const plain = await fetch(ids[i]!);
    if (plain.length !== lens[i]) {
      throw new ZenError(0, 'format', 'a chunk of the base has the wrong size');
    }
    yield plain.subarray(Math.max(a, s) - s, Math.min(b, e) - s);
  }
  return b;
}

/** The index `i` with `starts[i] ≤ x < starts[i + 1]` (`starts` ascending, one longer). */
export function locate(starts: number[], x: number): number {
  let lo = 0;
  let hi = starts.length - 2;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid]! <= x) lo = mid;
    else hi = mid - 1;
  }
  return Math.max(0, lo);
}

/**
 * Seal and upload a file's content (and its pages); the last puts stay
 * pending for the caller's write commit.
 */
export async function stage(
  tree: Tree,
  node: Uint8Array,
  data: FileSource,
  opts: StageOptions,
): Promise<Staged> {
  const fs = tree.fs;
  const keys = fs.sealKeys();
  const limits = await tree.limits();
  const info = await tree.info();
  const mvb = limits.max_value_bytes;
  const resume = opts.resume;
  if (resume && !equal(resume.node, node)) {
    throw new ZenError(0, 'bad_value', 'resume: the state is of another node');
  }
  const base = opts.base;
  const prev = opts.previous;
  const baseM = base?.manifest;

  // ---- the file key
  const fk: { key: Uint8Array; epoch: number } | undefined = resume
    ? resume.fileKey && { key: resume.fileKey, epoch: resume.keyEpoch! }
    : fs.policy.fileKeys
      ? await fileKeyFor(tree, node, prev, base, fs.policy, keys.epoch)
      : undefined;
  const keyOf = fk ? { fileKey: fk.key, keyEpoch: fk.epoch } : {};

  // ---- the form: fixed (version 2) or content-defined (version 3)
  let chunking: Chunking;
  let chunkSize = 0;
  let cdc: CdcParams | undefined;
  const forms = (c: Chunking, p: CdcParams | undefined, cs: number) => {
    const pageSize = c === 'cdc' ? zw.pageCapacity2(mvb) : zw.pageCapacity(mvb);
    const emptyInline =
      c === 'cdc'
        ? keys.sealManifestV3(tree.id, node, {
            version: 3,
            size: 0,
            n: 0,
            cdc: p!,
            ...keyOf,
            chunks: [],
          })
        : keys.sealManifestV2(tree.id, node, {
            version: 2,
            size: 0,
            chunkSize: cs,
            n: 0,
            ...keyOf,
            chunks: [],
          });
    const emptyPaged =
      c === 'cdc'
        ? keys.sealManifestV3(tree.id, node, {
            version: 3,
            size: 0,
            n: 0,
            cdc: p!,
            ...keyOf,
            pageSize,
            pages: [],
          })
        : keys.sealManifestV2(tree.id, node, {
            version: 2,
            size: 0,
            chunkSize: cs,
            n: 0,
            ...keyOf,
            pageSize,
            pages: [],
          });
    // Per entry: the manifest's bytes and 16 in the stored version (api.md §6).
    return {
      pageSize,
      inlineMax: Math.floor((mvb - emptyInline.length) / (c === 'cdc' ? 36 : 32)),
      maxPages: Math.floor((mvb - emptyPaged.length) / (c === 'cdc' ? 72 : 64)),
    };
  };
  if (resume) {
    chunking = resume.chunking;
    if (chunking === 'cdc') cdc = resume.cdc;
    else chunkSize = resume.chunkSize!;
  } else {
    // Explicit, else the form of what this write builds on, else the policy.
    chunking =
      opts.chunking ??
      (opts.chunkSize !== undefined
        ? 'fixed'
        : (base?.chunking ??
          (prev ? (prev.cdc ? 'cdc' : 'fixed') : undefined) ??
          fs.policy.chunking ??
          'cdc'));
    if (chunking === 'cdc') {
      cdc =
        opts.cdc ??
        (baseM?.version === 3 ? baseM.cdc : undefined) ??
        prev?.cdc ??
        tree.opts.cdc ??
        zw.cdcDefaults(mvb);
      if (!cdc || !info.features.includes('fs_cdc')) {
        chunking = 'fixed';
        cdc = undefined;
      } else if (opts.chunking !== 'cdc') {
        // A known size past what a version of variable chunks holds: fixed.
        const f = forms('cdc', cdc, 0);
        const cap = f.maxPages * f.pageSize * 2 ** cdc.avgBits;
        const known = opts.size ?? (data instanceof Uint8Array ? data.length : undefined);
        if (known !== undefined && known > cap) {
          chunking = 'fixed';
          cdc = undefined;
        }
      }
    }
    if (chunking === 'fixed') {
      chunkSize =
        opts.chunkSize ??
        (baseM && baseM.version !== 3 ? baseM.chunkSize : undefined) ??
        prev?.chunkSize ??
        Math.min(CHUNK_SIZE, mvb - 64);
    }
  }
  if (cdc && cdc.max > mvb - 64) {
    throw new ZenError(0, 'bad_value', 'cdc: max exceeds what one value holds');
  }
  const { pageSize, inlineMax, maxPages } = forms(chunking, cdc, chunkSize);
  const cutter: Cutter =
    chunking === 'cdc'
      ? cdcCutter(fk ? zw.fileChunker(fs.id, fk.key, cdc!) : keys.chunker(cdc!))
      : fixedCutter(chunkSize);

  // ---- what known items may be taken over as is
  const sameKey = (k: Uint8Array | undefined) => (fk ? !!k && equal(k, fk.key) : !k);
  const sameCut = (c: Chunking, cs: number | undefined, p: CdcParams | undefined) =>
    c === chunking && (c === 'fixed' ? cs === chunkSize : !!p && cdcEqual(p, cdc!));
  const baseOk =
    !!base &&
    sameKey(fileKeyOfVersion(base)?.key) &&
    sameCut(
      base.chunking,
      baseM!.version !== 3 ? baseM!.chunkSize : undefined,
      baseM!.version === 3 ? baseM!.cdc : undefined,
    );
  const prevOk =
    !!prev &&
    sameKey(prev.fileKey) &&
    sameCut(prev.cdc ? 'cdc' : 'fixed', prev.chunkSize, prev.cdc);
  const prevIds = prevOk ? new Set(prev!.chunks.map(hex)) : undefined;
  // Where a known item's plaintext comes from when it must be cut again.
  const fetchFrom: FileVersion | { fileKey?: Uint8Array; keyEpoch?: number } | undefined =
    base ?? (prev ? { fileKey: prev.fileKey, keyEpoch: prev.keyEpoch } : undefined);

  // Reuse by SHA-256: equal bytes under the same key (any cut).
  const reuse = new Map<string, Uint8Array[]>();
  if (prev?.hashes && sameKey(prev.fileKey)) {
    prev.hashes.forEach((h, i) => {
      const list = reuse.get(hex(h)) ?? [];
      list.push(prev.chunks[i]!);
      reuse.set(hex(h), list);
    });
  }

  // Pages of `previous`, by their ids, to reuse; and where they end (CDC).
  let prevPages: Map<string, PageRef | PageRefV3> | undefined;
  let prevEnds: Map<string, number> | undefined;
  const prevPagesOf = async () => {
    if (prevPages) return;
    prevPages = new Map();
    prevEnds = new Map();
    if (!prevOk || !prev!.pages || (chunking === 'fixed' && prev!.pageSize !== pageSize)) return;
    const refs = prev!.pages;
    let i = 0;
    for (const [p, ref] of refs.entries()) {
      const count = 'count' in ref ? ref.count : Math.min(prev!.pageSize!, prev!.chunks.length - i);
      const ids = prev!.chunks.slice(i, i + count);
      i += count;
      prevPages.set(hex(await sha256(concat(...ids))), ref);
      if (chunking === 'cdc' && ids.length) {
        const nextRef = refs[p + 1];
        prevEnds.set(hex(ids.at(-1)!), nextRef && 'count' in nextRef ? nextRef.count : 0);
      }
    }
  };

  // ---- the session
  let session: Uint8Array | undefined = resume?.session;
  let renewAt = 0;
  const ttlSecs = Number(opts.sessionTtlSecs ?? limits.upload_ttl_secs ?? 0);
  const sessionCall = async (path: 'open' | 'renew') => {
    const s = tree.fs.session;
    if (path === 'open') {
      const r = await s.call('/v1/fs/upload/open', zw.encodeUploadOpen, zw.decodeUploadSession, {
        fs: fs.id,
        ...(ttlSecs ? { ttl_secs: BigInt(ttlSecs) } : {}),
      });
      session = r.session;
    } else {
      await s.call('/v1/fs/upload/renew', zw.encodeUploadRenew, zw.decodeUploadExpiry, {
        fs: fs.id,
        session: session!,
        ...(ttlSecs ? { ttl_secs: BigInt(ttlSecs) } : {}),
      });
    }
    renewAt = Date.now() + ((ttlSecs || 604_800) * 1000) / 2;
  };
  if ((opts.session || session) && !opts.sink) {
    if (!info.features.includes('upload_sessions')) {
      throw new ZenError(0, 'unsupported', 'this server has no upload sessions');
    }
    await sessionCall(session ? 'renew' : 'open');
  }
  const ref: UploadRef | undefined = session ? { fs: fs.id, session } : undefined;
  const putCost = ref ? SESSION_COST : 0;

  // ---- the content so far
  const pages: (PageRef | PageRefV3)[] = resume ? [...resume.pages] : [];
  const open: TailEntry[] = resume ? [...resume.tail] : [];
  let paged = resume?.paged ?? false;
  let size = resume?.size ?? 0;
  // Version-2 pages are full; version-3 pages say how many chunks they hold.
  let n = open.length + pages.reduce((a, p) => a + ('count' in p ? p.count : pageSize), 0);
  // The whole index, while every chunk is seen (none taken over in a page unread).
  let indexing = !resume?.pages.length;
  const idx = { ids: open.map((e) => e.id), lens: open.map((e) => e.len) };
  const hashes: (Uint8Array | undefined)[] = open.map((e) => e.hash);

  let pending: ChunkPut[] = [];
  let pendingPages: PagePut[] = [];
  let pendingBytes = 0;
  const upload = {
    uploaded: resume?.uploaded ?? 0,
    pagesUploaded: resume?.pagesUploaded ?? 0,
    commits: resume?.commits ?? 0,
  } as Upload;
  const state = (s: Snapshot): UploadState => ({
    node,
    chunking,
    ...(chunking === 'cdc' ? { cdc: cdc! } : { chunkSize }),
    size: s.size,
    pages: s.pages,
    tail: s.tail,
    paged: s.paged,
    ...keyOf,
    ...(session ? { session } : {}),
    uploaded: s.uploaded,
    pagesUploaded: s.pagesUploaded,
    commits: s.commits,
  });

  // ---- commits, several in flight, reported in order
  const parallel = opts.sink ? 1 : Math.min(8, Math.max(1, opts.parallel ?? MAX_IN_FLIGHT));
  // Each commit is reported once it and every one before it are done.
  const inflight: { p: Promise<unknown>; done: Promise<void> }[] = [];
  let reported: Promise<void> = Promise.resolve();
  const settleOldest = async () => {
    const x = inflight.shift()!;
    try {
      await x.done;
    } catch (e) {
      await Promise.allSettled(inflight.map((i) => i.p));
      inflight.length = 0;
      throw e;
    }
  };
  const drain = async () => {
    while (inflight.length) await settleOldest();
  };
  const flush = async () => {
    if (!pending.length && !pendingPages.length) return;
    const chunks = pending;
    const pgs = pendingPages;
    pending = [];
    pendingPages = [];
    pendingBytes = 0;
    upload.commits++;
    const snap: Snapshot = {
      size,
      pages: pages.slice(),
      tail: open.slice(),
      paged,
      uploaded: upload.uploaded,
      pagesUploaded: upload.pagesUploaded,
      commits: upload.commits,
    };
    const put = { chunks: chunks.map((c) => c.id), pages: pgs.map((p) => p.id) };
    // A page lists chunks of this commit or earlier ones, which the server
    // must have applied first (pages are a few per hundred commits).
    if (pgs.length) await drain();
    let p: Promise<unknown>;
    if (opts.sink) p = opts.sink(chunks, pgs);
    else {
      if (ref && Date.now() >= renewAt) await sessionCall('renew');
      p = sendCommit(tree.fs.session, {
        commit_id: randomBytes(16),
        ...(chunks.length ? { chunks } : {}),
        ...(pgs.length ? { pages: pgs } : {}),
        ...(ref ? { upload: ref } : {}),
      });
    }
    const done = reported.then(async () => {
      await p;
      await opts.onProgress?.(state(snap), put);
    });
    reported = done;
    p.catch(() => {});
    done.catch(() => {});
    inflight.push({ p, done });
    while (inflight.length >= parallel) await settleOldest();
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

  // ---- pages
  // Pages this write made (not the base's).
  const own = new Set<string>();
  const closePage = async (count: number) => {
    // A short page after a short page of the base taken over unread (whose
    // neighbour an edit shrank): one page, if the two fit (the fill rule).
    const before = pages.at(-1);
    if (chunking === 'cdc' && count < pageSize && before && 'count' in before && base) {
      const bp = basePages.get(hex(before.id));
      if (
        bp !== undefined &&
        !own.has(hex(before.id)) &&
        2 * before.count < pageSize &&
        before.count + count <= pageSize
      ) {
        const pg = (await tree.pagesOf(base, bp, bp))[0]!;
        pages.pop();
        open.unshift(...pg.ids.map((id, i) => ({ id, len: pg.lens[i]! })));
        count += before.count;
      }
    }
    const entries = open.slice(0, count);
    const ids = entries.map((e) => e.id);
    await prevPagesOf();
    const old = prevPages!.size ? prevPages!.get(hex(await sha256(concat(...ids)))) : undefined;
    const bytes = entries.reduce((a, e) => a + e.len, 0);
    if (old) {
      open.splice(0, count);
      pages.push(
        chunking === 'cdc' ? { ...(old as PageRefV3) } : { id: old.id, digest: old.digest },
      );
    } else {
      const data =
        chunking === 'cdc'
          ? zw.encodePage2(entries.map((e) => ({ id: e.id, len: e.len })))
          : zw.encodePage(ids);
      const id = randomBytes(16);
      // A flush here reports the entries still in the tail, without this page.
      await add(OP_OVERHEAD + data.length + 16 + 48 * Math.floor(data.length / 16) + putCost);
      open.splice(0, count);
      pendingPages.push({ fs: fs.id, id, data });
      own.add(hex(id));
      const digest = zw.pageDigest(data);
      pages.push(chunking === 'cdc' ? { id, digest, bytes, count } : { id, digest });
      upload.pagesUploaded++;
    }
    if (pages.length > maxPages) throw tooLarge(`a file of ${pages.length} pages`);
  };
  // Pages from entries: once past inline, full ones are closed as they fill.
  const afterPush = async (e: TailEntry) => {
    if (!paged && open.length > inlineMax) paged = true;
    if (!paged) return;
    if (open.length >= pageSize) {
      await closePage(pageSize);
      return;
    }
    // CDC: where a page of `previous` ends, close here too, so the pages
    // after realign with its own (and are reused), under the fill rule.
    if (chunking !== 'cdc' || !prevOk || !prev!.pages) return;
    await prevPagesOf();
    const next = prevEnds!.get(hex(e.id));
    if (next === undefined) return;
    // Its last page: close only a page at least half full (what follows is new).
    const k = open.length;
    if (2 * k >= pageSize || (next > 0 && k + next > pageSize)) await closePage(k);
  };
  const pushEntry = async (e: TailEntry) => {
    open.push(e);
    size += e.len;
    n++;
    if (indexing) {
      idx.ids.push(e.id);
      idx.lens.push(e.len);
      hashes.push(e.hash);
    }
    await afterPush(e);
  };

  // ---- one new chunk's bytes
  const addNew = async (block: Uint8Array) => {
    const h = await sha256(block);
    // Each previous id is used once: a version shouldn't list a chunk twice.
    const old = reuse.get(hex(h))?.shift();
    if (old) {
      await pushEntry({ id: old, len: block.length, hash: h });
      return;
    }
    const id = randomBytes(16);
    const sealed = fk
      ? zw.sealFileChunk(fs.id, fk.key, fk.epoch, id, block)
      : keys.sealChunk(id, block);
    if (sealed.length > mvb) {
      throw new ZenError(
        413,
        'too_large',
        'a sealed chunk exceeds max_value_bytes; lower chunkSize',
      );
    }
    // A flush here reports a state without this chunk.
    await add(sealed.length + 16 + OP_OVERHEAD + putCost);
    pending.push({ fs: fs.id, id, data: sealed });
    upload.uploaded++;
    await pushEntry({ id, len: block.length, hash: h });
  };
  const feed = async (b: Uint8Array) => {
    for (const c of cutter.push(b)) await addNew(c);
  };

  // ---- the source, one item of lookahead; expansions go first
  const queue: Item[] = [];
  const src = items(data);
  const it = (
    Symbol.asyncIterator in src
      ? (src as AsyncIterable<Item>)[Symbol.asyncIterator]()
      : (src as Iterable<Item>)[Symbol.iterator]()
  ) as AsyncIterator<Item> | Iterator<Item>;
  let peeked: IteratorResult<Item> | undefined;
  const next = async (): Promise<Item | undefined> => {
    if (queue.length) return queue.shift();
    const r = peeked ?? (await it.next());
    peeked = undefined;
    return r.done ? undefined : r.value;
  };
  const isLast = async () => {
    if (queue.length) return false;
    peeked ??= await it.next();
    return !!peeked.done;
  };

  // The base's last chunk ends at its end, not where its content cuts.
  let lastId: string | undefined | null = null;
  const isBaseLast = async (id: Uint8Array) => {
    if (lastId === null) {
      lastId = undefined;
      if (base?.chunkCount) {
        lastId = hex((await tree.chunkIds(base, base.chunkCount - 1, base.chunkCount - 1))[0]!);
      } else if (prev?.chunks.length) lastId = hex(prev.chunks.at(-1)!);
    }
    return lastId === hex(id);
  };
  const basePages = new Map<string, number>(
    (baseM && baseM.version !== 1 ? (baseM.pages ?? []) : []).map((p, i) => [hex(p.id), i]),
  );
  const badKnown = () =>
    new ZenError(0, 'bad_value', 'a known chunk not of `previous` (or another key)');

  const knownChunk = async (k: KnownChunk) => {
    const max = chunking === 'cdc' ? cdc!.max : chunkSize;
    if (!Number.isSafeInteger(k.size) || k.size < 1 || k.size > max) {
      throw new ZenError(0, 'bad_value', 'a known chunk of the wrong size');
    }
    // Of the base (fetched from it when cut again), or else of `previous`.
    let ok: boolean;
    if (base) ok = baseOk;
    else if (prevIds?.has(hex(k.id))) ok = true;
    else throw badKnown();
    if (ok && cutter.pending === 0) {
      if (chunking === 'fixed') ok = k.size === chunkSize || (await isLast());
      else ok = !(k.last ?? (await isBaseLast(k.id))) || (await isLast());
    } else ok = false;
    if (ok) {
      await pushEntry({ id: k.id, len: k.size, ...(k.hash ? { hash: k.hash } : {}) });
      return;
    }
    if (!fetchFrom) {
      throw new ZenError(0, 'bad_value', 'a known chunk must start at a chunk boundary');
    }
    const plain = (await tree.fetchChunks([k.id], fetchFrom)).get(hex(k.id))!;
    if (plain.length !== k.size) {
      throw new ZenError(0, 'bad_value', 'a known chunk of the wrong size');
    }
    await feed(plain);
  };

  const knownPage = async (kp: KnownPage) => {
    const p = base ? basePages.get(hex(kp.page.id)) : undefined;
    if (p === undefined) throw new ZenError(0, 'bad_value', 'a known page not of the base');
    const ref = (baseM as ManifestV2 | ManifestV3).pages![p]!;
    const last = p === basePages.size - 1;
    const fits =
      'count' in ref ? ref.count <= pageSize : (baseM as ManifestV2).pageSize === pageSize;
    let asIs =
      baseOk && fits && !opts.repack && cutter.pending === 0 && (!last || (await isLast()));
    if (asIs) {
      if (!paged) {
        paged = true;
        while (open.length >= pageSize) await closePage(pageSize);
      }
      if (chunking === 'fixed') {
        // Version-2 pages are full but the last.
        asIs = open.length === 0;
      } else {
        const k = open.length;
        if (k && 2 * k < pageSize && k + (ref as PageRefV3).count <= pageSize) asIs = false;
        else if (k) await closePage(k);
      }
    }
    if (!asIs) {
      const pg = (await tree.pagesOf(base!, p, p))[0]!;
      queue.unshift(
        ...pg.ids.map((id, i) => ({
          id,
          size: pg.lens[i]!,
          last: last && i === pg.ids.length - 1,
        })),
      );
      return;
    }
    const lay = tree.layout(base!);
    pages.push(ref);
    n += lay.chunks[p + 1]! - lay.chunks[p]!;
    size += lay.bytes[p + 1]! - lay.bytes[p]!;
    if (indexing) {
      indexing = false;
      idx.ids.length = 0;
      idx.lens.length = 0;
      hashes.length = 0;
    }
    if (pages.length > maxPages) throw tooLarge(`a file of ${pages.length} pages`);
  };

  try {
    for (let item = await next(); item !== undefined; item = await next()) {
      if (item instanceof Uint8Array) await feed(item);
      else if (isPage(item)) await knownPage(item);
      else await knownChunk(item);
    }
    const tail = cutter.finish();
    if (tail) await addNew(tail);
  } catch (e) {
    await Promise.allSettled(inflight.map((i) => i.p));
    throw e;
  } finally {
    cutter.free();
  }

  // ---- the manifest
  if (paged) {
    if (chunking === 'fixed' && !info.features.includes('fs_pages')) {
      throw new ZenError(
        413,
        'too_large',
        `a file of ${n} chunks needs paged versions, which this server lacks`,
      );
    }
    if (open.length) await closePage(open.length);
  }
  let manifest: FileManifest;
  if (chunking === 'cdc') {
    manifest = paged
      ? { version: 3, size, n, cdc: cdc!, ...keyOf, pageSize, pages: pages as PageRefV3[] }
      : {
          version: 3,
          size,
          n,
          cdc: cdc!,
          ...keyOf,
          chunks: open.map((e) => ({ id: e.id, len: e.len })),
        };
  } else {
    manifest = paged
      ? {
          version: 2,
          size,
          chunkSize,
          n,
          ...keyOf,
          pageSize,
          pages: pages.map((p) => ({ id: p.id, digest: p.digest })),
        }
      : { version: 2, size, chunkSize, n, ...keyOf, chunks: open.map((e) => e.id) };
  }
  const sealedManifest =
    manifest.version === 3
      ? keys.sealManifestV3(tree.id, node, manifest)
      : keys.sealManifestV2(tree.id, node, manifest);
  const listed = paged ? pages.length : open.length;
  if (sealedManifest.length + 16 * listed > mvb) throw tooLarge(`a file of ${n} chunks`);
  let index: ChunkIndex | undefined;
  if (indexing) {
    index = {
      ...(chunking === 'cdc' ? { cdc: cdc!, lens: idx.lens } : { chunkSize }),
      chunks: idx.ids,
      ...keyOf,
    };
    if (hashes.every((h) => h)) index.hashes = hashes as Uint8Array[];
    if (paged) Object.assign(index, { pageSize, pages });
  }
  Object.assign(upload, {
    node,
    manifest,
    sealedManifest,
    ...(index ? { index } : {}),
    chunks: paged ? [] : open.map((e) => e.id),
    pages: paged ? pages.map((p) => p.id) : [],
    ...(session ? { session } : {}),
  });
  return {
    upload,
    ...(ref ? { ref } : {}),
    flush,
    drain,
    pending: () => pending,
    pendingPages: () => pendingPages,
    pendingOps: () => pending.length + pendingPages.length,
    pendingBytes: () => pendingBytes,
  };
}

/** What a progress report covers: the content up to a commit. */
interface Snapshot {
  size: number;
  pages: (PageRef | PageRefV3)[];
  tail: TailEntry[];
  paged: boolean;
  uploaded: number;
  pagesUploaded: number;
  commits: number;
}
