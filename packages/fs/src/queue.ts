// The replica's queue of local writes (spec/fs.md §11, zendb.md §9.3):
// entries in store `zfs/…/queue`, keyed `e ‖ u64 seq ‖ 4 random bytes`, as
// JSON with bytes in base64url and bigints as decimal text; plus the commit
// in flight under `inflight`, kept until its outcome is known.
import { bytes, type ChunkIndex, type PendingOp } from '@zen/client';
import type { Commit } from '@zen/wasm';

/** A write op as queued: `after` names earlier entries whose write it also replaces. */
export type QWrite = Extract<PendingOp, { op: 'write' }> & {
  /** Earlier entries (hex keys) writing the node: their dots, once known, are replaced too. */
  after?: string[];
  /** The new content's index (for later writes' chunk reuse). */
  index?: ChunkIndex;
};

export type QOp = Exclude<PendingOp, { op: 'write' }> | QWrite;

/** One API call's operations, applied together. */
export interface QueueEntry {
  /** The record key, hex (also the order). */
  key: string;
  ops: QOp[];
  /** Sealed content kept in the replica's `chunks`/`pages` stores until sent. */
  chunks: Uint8Array[];
  pages: Uint8Array[];
}

/** The commit in flight: sent again, as is, until its outcome is known. */
export interface Inflight {
  keys: string[];
  commit: Commit;
}

export const INFLIGHT = bytes.utf8('inflight');
const ENTRY = 0x65; // 'e'

let last = 0;
/** A new entry key: increasing within this process, unique across tabs. */
export function newKey(): Uint8Array {
  last = Math.max(last + 1, Date.now() * 1024);
  const k = new Uint8Array(13);
  k[0] = ENTRY;
  new DataView(k.buffer).setBigUint64(1, BigInt(last));
  k.set(bytes.randomBytes(4), 9);
  return k;
}

export const isEntryKey = (k: Uint8Array): boolean => k.length === 13 && k[0] === ENTRY;

/** JSON with bytes and bigints. */
export function encodeJson(v: unknown): Uint8Array {
  return bytes.utf8(
    JSON.stringify(v, (_k, x) => {
      if (x instanceof Uint8Array) return { $b: bytes.b64url(x) };
      if (typeof x === 'bigint') return { $n: x.toString() };
      return x;
    }),
  );
}

export function decodeJson<T>(b: Uint8Array): T {
  return JSON.parse(bytes.fromUtf8(b), (_k, x) => {
    if (x && typeof x === 'object' && !Array.isArray(x)) {
      if (typeof x.$b === 'string' && Object.keys(x).length === 1) return bytes.fromB64url(x.$b);
      if (typeof x.$n === 'string' && Object.keys(x).length === 1) return BigInt(x.$n);
    }
    return x;
  }) as T;
}

export const encodeEntry = (e: QueueEntry): Uint8Array => encodeJson({ v: 1, ...e });

export function decodeEntry(key: string, b: Uint8Array): QueueEntry | undefined {
  try {
    const e = decodeJson<QueueEntry & { v: number }>(b);
    if (e.v !== 1 || !Array.isArray(e.ops)) return undefined;
    return { key, ops: e.ops, chunks: e.chunks ?? [], pages: e.pages ?? [] };
  } catch {
    return undefined;
  }
}

/** The nodes an entry touches. */
export const nodesOf = (e: QueueEntry): Uint8Array[] => e.ops.map((o) => o.node);
