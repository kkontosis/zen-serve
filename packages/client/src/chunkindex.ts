// A version's chunk index, compact: what a writer knows of a version's
// content for reuse (spec/fs.md §4.1). Ids, lengths and hashes are packed
// in one buffer each, about 52 bytes a chunk. As arrays of small
// `Uint8Array`s it took about 390 (milestone 8 step 8), which a large file's
// index made hundreds of megabytes.
import type { CdcParams, PageRef, PageRefV3 } from '@zen/wasm';

/**
 * What a writer knows of a version's content, for reuse: its chunk ids and
 * lengths, and a SHA-256 per chunk when its plaintext was seen. Chunk `i`'s
 * id is `chunkId(ix, i)`, its hash `chunkHash(ix, i)`.
 */
export interface ChunkIndex {
  /** Fixed chunks: plaintext bytes per chunk. Absent under CDC. */
  chunkSize?: number;
  /** CDC: the chunker's parameters. */
  cdc?: CdcParams;
  /** The number of chunks. */
  n: number;
  /** The chunk ids in file order, 16 bytes each (`n × 16`). */
  ids: Uint8Array;
  /** Plaintext lengths (CDC). */
  lens?: Uint32Array;
  /** Plaintext SHA-256s, 32 bytes each (`n × 32`); absent when not every chunk's plaintext was seen. */
  hashes?: Uint8Array;
  /** The file key the chunks are sealed under; absent: under the fs keys. */
  fileKey?: Uint8Array;
  /** The file key's epoch. */
  keyEpoch?: number;
  /** The most ids per page, when the version is paged: pages whose ids didn't change are reused. */
  pageSize?: number;
  pages?: (PageRef | PageRefV3)[];
}

/** Chunk `i`'s id (a view of the index's buffer). */
export const chunkId = (ix: ChunkIndex, i: number): Uint8Array =>
  ix.ids.subarray(16 * i, 16 * i + 16);

/** Chunk `i`'s plaintext SHA-256 (a view), when the index has hashes. */
export const chunkHash = (ix: ChunkIndex, i: number): Uint8Array | undefined =>
  ix.hashes?.subarray(32 * i, 32 * i + 32);

/** Pieces of `size` bytes each, packed into one buffer. */
export function pack(items: readonly Uint8Array[], size: number): Uint8Array {
  const out = new Uint8Array(size * items.length);
  items.forEach((b, i) => {
    if (b.length !== size) throw new RangeError(`an item of ${b.length} bytes, not ${size}`);
    out.set(b, size * i);
  });
  return out;
}

/** Bytes appended in pieces, in a buffer that doubles as it grows. */
export class Bytes {
  private buf: Uint8Array;
  length = 0;
  constructor(initial = 1024) {
    this.buf = new Uint8Array(initial);
  }
  push(b: Uint8Array): void {
    if (this.length + b.length > this.buf.length) {
      const next = new Uint8Array(Math.max(2 * this.buf.length, this.length + b.length));
      next.set(this.buf.subarray(0, this.length));
      this.buf = next;
    }
    this.buf.set(b, this.length);
    this.length += b.length;
  }
  clear(): void {
    this.length = 0;
  }
  /** A copy of what was pushed, exactly its length. */
  take(): Uint8Array {
    return this.buf.slice(0, this.length);
  }
}

/** `u32` values appended, in a buffer that doubles as it grows. */
export class U32s {
  private buf = new Uint32Array(256);
  length = 0;
  push(v: number): void {
    if (this.length === this.buf.length) {
      const next = new Uint32Array(2 * this.buf.length);
      next.set(this.buf);
      this.buf = next;
    }
    this.buf[this.length++] = v;
  }
  clear(): void {
    this.length = 0;
  }
  take(): Uint32Array {
    return this.buf.slice(0, this.length);
  }
}

/**
 * An index in the array form (`chunks`, `hashes`, `lens` as arrays: legacy
 * records and queue entries) made compact; a compact one is returned as is.
 */
export function compactIndex(ix: ChunkIndex | LegacyChunkIndex): ChunkIndex {
  if (!('chunks' in ix) || !Array.isArray(ix.chunks)) return ix as ChunkIndex;
  const { chunks, hashes, lens, ...rest } = ix as LegacyChunkIndex;
  return {
    ...rest,
    n: chunks.length,
    ids: pack(chunks, 16),
    ...(lens ? { lens: Uint32Array.from(lens) } : {}),
    ...(hashes ? { hashes: pack(hashes, 32) } : {}),
  };
}

/** The array form of an index, as legacy records and queue entries hold it. */
export interface LegacyChunkIndex extends Omit<ChunkIndex, 'n' | 'ids' | 'lens' | 'hashes'> {
  chunks: Uint8Array[];
  lens?: number[];
  hashes?: Uint8Array[];
}
