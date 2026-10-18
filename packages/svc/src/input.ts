// A connection's input as pushed (api.md §14.10): chunks joined by offset,
// from `in_acked`. Pushes may repeat bytes (after a reconnect or a new
// claim they restart at `in_acked`) and may start before it.

/** Input bytes held for a connection, contiguous from `base`. */
export class Input {
  private parts: Uint8Array[] = [];
  private len = 0;
  /** Chunks past a gap, by offset. */
  private later = new Map<bigint, Uint8Array>();

  /** @param base The offset of the first byte held (`in_acked`). */
  constructor(public base: bigint) {}

  /** The offset after the last contiguous byte held. */
  get end(): bigint {
    return this.base + BigInt(this.len);
  }

  /** Bytes held past a gap (not yet usable). */
  get pending(): number {
    let n = 0;
    for (const d of this.later.values()) n += d.length;
    return n;
  }

  /** Add a chunk at `off`. Returns whether the contiguous input grew. */
  add(off: bigint, data: Uint8Array): boolean {
    if (data.length === 0) return false;
    const end = this.end;
    if (off > end) {
      const held = this.later.get(off);
      if (!held || held.length < data.length) this.later.set(off, data);
      return false;
    }
    const skip = end - off;
    if (skip >= BigInt(data.length)) return false;
    this.push(data.subarray(Number(skip)));
    // Chunks that the new end reaches.
    for (let again = true; again; ) {
      again = false;
      for (const [o, d] of this.later) {
        if (o > this.end) continue;
        this.later.delete(o);
        const s = this.end - o;
        if (s < BigInt(d.length)) this.push(d.subarray(Number(s)));
        again = true;
      }
    }
    return true;
  }

  private push(d: Uint8Array): void {
    this.parts.push(d);
    this.len += d.length;
  }

  /** The contiguous bytes from `base`. */
  bytes(): Uint8Array {
    if (this.parts.length > 1) {
      const all = new Uint8Array(this.len);
      let at = 0;
      for (const p of this.parts) {
        all.set(p, at);
        at += p.length;
      }
      this.parts = [all];
    }
    return this.parts[0] ?? new Uint8Array(0);
  }

  /** Drop the bytes below `to` (input acked up to it). */
  advance(to: bigint): void {
    if (to <= this.base) return;
    if (to >= this.end) {
      this.parts = [];
      this.len = 0;
    } else {
      const rest = this.bytes().subarray(Number(to - this.base));
      this.parts = [rest];
      this.len = rest.length;
    }
    this.base = to;
    for (const o of [...this.later.keys()]) {
      if (o < to) {
        const d = this.later.get(o)!;
        this.later.delete(o);
        if (o + BigInt(d.length) > to) this.add(o, d);
      }
    }
  }
}
