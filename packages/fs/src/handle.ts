// File handles of a replica (spec/fs.md §11): random reads and writes in
// memory by whole chunks. Only the chunks written are kept (dirty); a flush
// seals just those, takes the others over from the base version as known
// chunks, and queues one write.
import { type ChunkIndex, type KnownChunk, ZenError } from '@zen/client';
import type { ShownVersion } from './overlay.js';
import type { Replica, Stat, WriteCallOptions } from './replica.js';

/** `r` read; `r+` read and write; `w` truncate (created if missing); `a` append (created if missing). */
export type OpenFlags = 'r' | 'r+' | 'w' | 'a';

export interface OpenOptions {
  /** Create the file when missing (default: with `w` and `a`). */
  create?: boolean;
  /** With `create`: fail with `exists` when the file is there. */
  exclusive?: boolean;
  /** Mode of a new file (default 0o644). */
  mode?: number;
}

export class FileHandle {
  /** The current size, writes included. */
  size: number;
  private readonly dirty = new Map<number, Uint8Array>();
  /** Bytes of the base still valid (a truncation cuts them). */
  private baseLimit: number;
  private baseSize: number;
  private changed = false;
  private closed = false;
  private index: ChunkIndex | undefined;

  constructor(
    private readonly r: Replica,
    readonly path: string,
    readonly node: Uint8Array,
    private base: ShownVersion | undefined,
    readonly flags: 'r' | 'r+' | 'w' | 'a',
    private readonly chunkSize: number,
  ) {
    this.baseSize = base?.size ?? 0;
    this.baseLimit = this.baseSize;
    this.size = this.baseSize;
  }

  private check(write: boolean): void {
    if (this.closed) throw new ZenError(0, 'closed', 'the file is closed');
    if (write && this.flags === 'r') throw new ZenError(0, 'bad_value', 'opened read-only');
  }

  private expected(i: number): number {
    return Math.max(0, Math.min(this.chunkSize, this.size - i * this.chunkSize));
  }

  /** Chunk `i` as it stands, at its current length. */
  private async chunk(i: number): Promise<Uint8Array> {
    const len = this.expected(i);
    const d = this.dirty.get(i);
    if (d) {
      if (d.length === len) return d;
      const out = new Uint8Array(len);
      out.set(d.subarray(0, len));
      return out;
    }
    const out = new Uint8Array(len);
    const start = i * this.chunkSize;
    const valid = Math.min(len, Math.min(this.baseLimit, this.size) - start);
    if (valid > 0 && this.base) {
      out.set(await this.r.readVersion(this.node, this.base, start, valid));
    }
    return out;
  }

  /** Up to `length` bytes at `position` (shorter at the end). */
  async read(length: number, position: number): Promise<Uint8Array> {
    this.check(false);
    const end = Math.min(this.size, position + length);
    if (position >= end) return new Uint8Array(0);
    const cs = this.chunkSize;
    const out = new Uint8Array(end - position);
    const first = Math.floor(position / cs);
    const last = Math.floor((end - 1) / cs);
    // Untouched runs read straight from the base.
    for (let i = first; i <= last; i++) {
      const start = i * cs;
      const from = Math.max(position, start);
      const to = Math.min(end, start + cs);
      const c = await this.chunk(i);
      out.set(c.subarray(from - start, to - start), from - position);
    }
    return out;
  }

  /** Write `data` at `position` (with `a`: at the end). Returns the bytes written. */
  async write(data: Uint8Array, position?: number): Promise<number> {
    this.check(true);
    const pos = this.flags === 'a' || position === undefined ? this.size : position;
    const end = pos + data.length;
    if (!data.length) return 0;
    if (end > this.size) await this.grow(end);
    const cs = this.chunkSize;
    for (let i = Math.floor(pos / cs); i <= Math.floor((end - 1) / cs); i++) {
      const c = await this.chunk(i);
      const start = i * cs;
      const from = Math.max(pos, start);
      const to = Math.min(end, start + cs);
      c.set(data.subarray(from - pos, to - pos), from - start);
      this.dirty.set(i, c);
    }
    this.changed = true;
    return data.length;
  }

  /** Extend to `size`: the old last chunk grows (with zeros), so it is rewritten. */
  private async grow(size: number): Promise<void> {
    if (this.size > 0 && this.size % this.chunkSize) {
      const last = Math.floor((this.size - 1) / this.chunkSize);
      const c = await this.chunk(last);
      this.size = size;
      const grown = new Uint8Array(this.expected(last));
      grown.set(c);
      this.dirty.set(last, grown);
      return;
    }
    this.size = size;
  }

  /** Cut or extend (with zeros) to `length`. */
  async truncate(length: number): Promise<void> {
    this.check(true);
    if (length === this.size) return;
    if (length > this.size) {
      await this.grow(length);
    } else {
      const cs = this.chunkSize;
      for (const i of [...this.dirty.keys()]) if (i * cs >= length) this.dirty.delete(i);
      if (length % cs) {
        const last = Math.floor(length / cs);
        const c = await this.chunk(last);
        this.dirty.set(last, c.slice(0, length - last * cs));
      }
      this.size = length;
      this.baseLimit = Math.min(this.baseLimit, length);
    }
    this.changed = true;
  }

  async stat(): Promise<Stat> {
    this.check(false);
    const s = await this.r.stat(this.path).catch(() => undefined);
    return {
      type: 'file',
      mode: s?.mode ?? 0o644,
      mtimeMs: s?.mtimeMs ?? Date.now(),
      versions: s?.versions ?? 1,
      conflict: false,
      ...s,
      node: this.node,
      size: this.size,
    };
  }

  /**
   * The content: unchanged chunks of the base as known chunks (no
   * plaintext, nothing sealed), the others as bytes.
   */
  private async *source(known: boolean): AsyncGenerator<Uint8Array | KnownChunk> {
    const cs = this.chunkSize;
    const n = Math.ceil(this.size / cs);
    const ix = known ? this.index : undefined;
    for (let i = 0; i < n; i++) {
      const baseLen = Math.max(0, Math.min(cs, this.baseSize - i * cs));
      if (
        ix &&
        !this.dirty.has(i) &&
        i < ix.chunks.length &&
        baseLen === this.expected(i) &&
        i * cs + baseLen <= this.baseLimit
      ) {
        yield { id: ix.chunks[i]!, ...(ix.hashes ? { hash: ix.hashes[i]! } : {}), size: baseLen };
      } else {
        yield await this.chunk(i);
      }
    }
  }

  /** Queue what changed as one write (nothing when nothing changed). */
  async flush(opts: WriteCallOptions = {}): Promise<void> {
    this.check(false);
    if (!this.changed) return;
    const base = this.base;
    this.index ??= base ? await this.r.indexOf(this.node, base) : undefined;
    let v: Awaited<ReturnType<Replica['queueFlush']>>;
    const usable = this.index?.chunkSize === this.chunkSize ? this.index : undefined;
    try {
      v = await this.r.queueFlush(this.node, this.source(!!usable), base, usable, !!opts.wait);
    } catch (e) {
      // Known chunks need the base's key; a write under a fresh key seals them all.
      if (!usable || !(e instanceof ZenError) || !e.message.includes('known chunk')) throw e;
      v = await this.r.queueFlush(this.node, this.source(false), base, usable, !!opts.wait);
    }
    this.base = v;
    this.index = v.index;
    this.baseSize = this.size;
    this.baseLimit = this.size;
    this.dirty.clear();
    this.changed = false;
  }

  /** Flush and close. */
  async close(opts: WriteCallOptions = {}): Promise<void> {
    if (this.closed) return;
    try {
      await this.flush(opts);
    } finally {
      this.closed = true;
    }
  }
}
