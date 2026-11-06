// File handles of a replica (spec/fs.md §11.1): random reads and writes
// over extents of the base version. The content is a sorted list of
// extents: bytes of the base at the same offsets, bytes in memory, and
// zeros. At most `ReplicaOptions.dirtyBytes` stay in memory; past that the
// handle queues what it has as a write, as a flush does. A flush of a file
// whose base is still partly there is an edit of the base (`Tree.edit`):
// only the pages and chunks around what changed are sealed.
import { type Edit, ZenError } from '@zen/client';
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

/** Bytes of the base at the same offsets (writes overwrite in place, so a base range never moves). */
interface BaseExt {
  kind: 'base';
  start: number;
  end: number;
}

/** Bytes in memory: `buf[0 .. end - start]`, `buf.length` its capacity. */
interface MemExt {
  kind: 'mem';
  start: number;
  end: number;
  buf: Uint8Array;
}

interface ZeroExt {
  kind: 'zero';
  start: number;
  end: number;
}

type Ext = BaseExt | MemExt | ZeroExt;

/** A memory extent grows (doubling) up to this, so small writes in a row make few extents. */
const MAX_EXT = 4 << 20;
/** Zeros are streamed in pieces of this size. */
const ZERO_PIECE = 1 << 20;

/** The part `[from, to)` of extent `e`. */
function cut(e: Ext, from: number, to: number): Ext {
  if (e.kind !== 'mem') return { kind: e.kind, start: from, end: to };
  let buf = e.buf.subarray(from - e.start, to - e.start);
  // A small piece of a large buffer is copied, so the rest can be freed.
  if (buf.length * 2 < e.buf.buffer.byteLength) buf = buf.slice();
  return { kind: 'mem', start: from, end: to, buf };
}

export class FileHandle {
  /** The current size, writes included. */
  size: number;
  private exts: Ext[];
  private dirty = 0;
  private changed = false;
  private closed = false;
  /** A flush in progress (writes wait for it). */
  private flushing: Promise<void> | undefined;

  constructor(
    private readonly r: Replica,
    readonly path: string,
    readonly node: Uint8Array,
    private base: ShownVersion | undefined,
    readonly flags: 'r' | 'r+' | 'w' | 'a',
    private readonly bound: number,
  ) {
    this.size = base?.size ?? 0;
    this.exts = this.size ? [{ kind: 'base', start: 0, end: this.size }] : [];
  }

  /** Bytes written and held in memory, not yet queued. */
  get dirtyBytes(): number {
    return this.dirty;
  }

  private check(write: boolean): void {
    if (this.closed) throw new ZenError(0, 'closed', 'the file is closed');
    if (write && this.flags === 'r') throw new ZenError(0, 'bad_value', 'opened read-only');
  }

  /** The index of the first extent ending after `pos`. */
  private find(pos: number): number {
    let lo = 0;
    let hi = this.exts.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (this.exts[mid]!.end <= pos) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }

  /** Replace `[a, b)` (within the content) with `insert`, which covers it exactly. */
  private splice(a: number, b: number, insert: Ext[]): void {
    const i = this.find(a);
    let j = i;
    const out: Ext[] = [];
    if (i < this.exts.length && this.exts[i]!.start < a) {
      out.push(cut(this.exts[i]!, this.exts[i]!.start, a));
    }
    out.push(...insert);
    while (j < this.exts.length && this.exts[j]!.start < b) j++;
    if (j > i) {
      const last = this.exts[j - 1]!;
      if (last.end > b) out.push(cut(last, b, last.end));
    }
    for (let k = i; k < j; k++) {
      const e = this.exts[k]!;
      if (e.kind === 'mem') this.dirty -= e.end - e.start;
    }
    for (const e of out) if (e.kind === 'mem') this.dirty += e.end - e.start;
    this.exts.splice(i, j - i, ...out);
  }

  /** Up to `length` bytes at `position` (shorter at the end). */
  async read(length: number, position: number): Promise<Uint8Array> {
    this.check(false);
    await this.flushing;
    const end = Math.min(this.size, position + length);
    if (position >= end) return new Uint8Array(0);
    const out = new Uint8Array(end - position);
    const base = this.base;
    for (let i = this.find(position); i < this.exts.length; i++) {
      const e = this.exts[i]!;
      if (e.start >= end) break;
      const from = Math.max(position, e.start);
      const to = Math.min(end, e.end);
      if (e.kind === 'mem') {
        out.set(e.buf.subarray(from - e.start, to - e.start), from - position);
      } else if (e.kind === 'base' && base) {
        out.set(await this.r.readVersion(this.node, base, from, to - from), from - position);
      }
    }
    return out;
  }

  /** Write `data` at `position` (with `a`: at the end). Returns the bytes written. */
  async write(data: Uint8Array, position?: number): Promise<number> {
    this.check(true);
    await this.flushing;
    const pos = this.flags === 'a' || position === undefined ? this.size : position;
    if (!data.length) return 0;
    if (pos > this.size) this.extend(pos);
    const end = pos + data.length;
    const i = this.find(pos);
    const e = this.exts[i];
    const prev = i > 0 ? this.exts[i - 1] : undefined;
    if (e?.kind === 'mem' && e.start <= pos && end <= e.end) {
      // Within a memory extent: in place.
      e.buf.set(data, pos - e.start);
    } else if (e?.kind === 'mem' && e.start <= pos && end - e.start <= MAX_EXT) {
      // Overlapping a memory extent's end: it grows.
      this.grow(e, pos, data);
    } else if (prev?.kind === 'mem' && prev.end === pos && end - prev.start <= MAX_EXT) {
      // Right after one: it grows.
      this.grow(prev, pos, data);
    } else {
      this.splice(pos, Math.min(end, this.size), [
        // A copy (`Buffer.slice` would share the caller's memory).
        { kind: 'mem', start: pos, end, buf: new Uint8Array(data) },
      ]);
    }
    this.size = Math.max(this.size, end);
    this.changed = true;
    await this.spill();
    return data.length;
  }

  /** Write `data` at `pos` into memory extent `e` (which starts at or before `pos`), growing it. */
  private grow(e: MemExt, pos: number, data: Uint8Array): void {
    const end = pos + data.length;
    const old = e.end;
    // What `e` grows over is taken out of the extents after it.
    if (end > old) this.splice(old, Math.min(end, this.size), []);
    const need = end - e.start;
    if (need > e.buf.length) {
      const buf = new Uint8Array(Math.min(MAX_EXT, Math.max(need, e.buf.length * 2)));
      buf.set(e.buf.subarray(0, old - e.start));
      e.buf = buf;
    }
    e.buf.set(data, pos - e.start);
    if (end > old) {
      this.dirty += end - old;
      e.end = end;
    }
  }

  /** Extend to `size` with zeros. */
  private extend(size: number): void {
    const last = this.exts.at(-1);
    if (last?.kind === 'zero') last.end = size;
    else this.exts.push({ kind: 'zero', start: this.size, end: size });
    this.size = size;
  }

  /** Cut or extend (with zeros) to `length`. */
  async truncate(length: number): Promise<void> {
    this.check(true);
    await this.flushing;
    if (length === this.size) return;
    if (length > this.size) this.extend(length);
    else {
      this.splice(length, this.size, []);
      this.size = length;
    }
    this.changed = true;
    await this.spill();
  }

  /** Past the bound, queue what is held (the handle stays open). */
  private async spill(): Promise<void> {
    if (this.dirty > this.bound) await this.flush();
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

  /** The whole content, the base's ranges read from it. */
  private async *bytes(): AsyncGenerator<Uint8Array> {
    for (const e of this.exts) {
      if (e.kind === 'mem') yield e.buf.subarray(0, e.end - e.start);
      else if (e.kind === 'zero') yield* zeros(e.end - e.start);
      else {
        for (let at = e.start; at < e.end; at += MAX_EXT) {
          const n = Math.min(MAX_EXT, e.end - at);
          yield await this.r.readVersion(this.node, this.base!, at, n);
        }
      }
    }
  }

  /**
   * The edits that turn the base into the content: each gap between base
   * extents removes the base bytes it covers and inserts its own parts
   * (memory and zeros), one edit each, at the gap.
   */
  private edits(): Edit[] {
    const baseSize = this.base?.size ?? 0;
    const out: Edit[] = [];
    let i = 0;
    let at = 0;
    while (at < this.size || at < baseSize) {
      const e = this.exts[i];
      if (e?.kind === 'base') {
        at = e.end;
        i++;
        continue;
      }
      // A gap: from `at` to the next base extent (or the end).
      let j = i;
      while (j < this.exts.length && this.exts[j]!.kind !== 'base') j++;
      const next = this.exts[j]?.start ?? baseSize;
      const removed = Math.max(0, Math.min(next, baseSize) - at);
      if (j === i) out.push({ offset: at, remove: removed });
      for (let k = i; k < j; k++) {
        const x = this.exts[k] as MemExt | ZeroExt;
        const part: Edit = k === i ? { offset: at, remove: removed } : { offset: at + removed };
        if (x.kind === 'mem') part.insert = x.buf.subarray(0, x.end - x.start);
        else part.zeros = x.end - x.start;
        out.push(part);
      }
      at = Math.max(next, at + removed);
      i = j;
      if (j >= this.exts.length) break;
    }
    return out;
  }

  /** Queue what changed as one write (nothing when nothing changed). */
  flush(opts: WriteCallOptions = {}): Promise<void> {
    this.check(false);
    const p = (this.flushing ?? Promise.resolve()).then(() => this.flushNow(opts));
    const settled = p.then(
      () => {},
      () => {},
    );
    this.flushing = settled;
    void settled.then(() => {
      if (this.flushing === settled) this.flushing = undefined;
    });
    return p;
  }

  private async flushNow(opts: WriteCallOptions): Promise<void> {
    if (!this.changed) return;
    const base = this.base;
    const edit = !!base && this.exts.some((e) => e.kind === 'base');
    const v = edit
      ? await this.r.queueEdit(this.node, base, this.edits(), !!opts.wait)
      : await this.r.queueFlush(this.node, this.bytes(), this.size, base, !!opts.wait);
    this.base = v;
    this.exts = this.size ? [{ kind: 'base', start: 0, end: this.size }] : [];
    this.dirty = 0;
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

/** `n` zero bytes in pieces of at most 1 MiB. */
function* zeros(n: number): Generator<Uint8Array> {
  const piece = new Uint8Array(Math.min(n, ZERO_PIECE));
  for (let left = n; left > 0; left -= piece.length) {
    yield left >= piece.length ? piece : piece.subarray(0, left);
  }
}
