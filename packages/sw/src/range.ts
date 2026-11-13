// `Range` (spec/sw.md §6), as the server's `statics.rs` `parse_range`.

/** What a `Range` header asks of a file. */
export type Range =
  | { kind: 'full' }
  /** Bytes `first..=last`, within the file. */
  | { kind: 'part'; first: number; last: number }
  | { kind: 'unsatisfiable' };

const FULL: Range = { kind: 'full' };
const UNSAT: Range = { kind: 'unsatisfiable' };

function digits(s: string): number | undefined {
  if (!/^[0-9]+$/.test(s)) return undefined;
  const n = Number(s);
  return Number.isSafeInteger(n) ? n : undefined;
}

/**
 * Read a `Range` header against a file of `size` bytes. One range of bytes
 * is honoured; several, another unit or malformed syntax mean the whole file.
 */
export function parseRange(header: string, size: number): Range {
  const h = header.trim();
  if (h.slice(0, 6).toLowerCase() !== 'bytes=') return FULL;
  const spec = h.slice(6);
  if (spec.includes(',')) return FULL;
  const dash = spec.indexOf('-');
  if (dash < 0) return FULL;
  const a = spec.slice(0, dash).trim();
  const b = spec.slice(dash + 1).trim();
  if (!a) {
    // `-n`: the last n bytes.
    const n = digits(b);
    if (n === undefined) return FULL;
    if (n === 0 || size === 0) return UNSAT;
    return { kind: 'part', first: Math.max(0, size - n), last: size - 1 };
  }
  const first = digits(a);
  if (first === undefined) return FULL;
  let last = Number.MAX_SAFE_INTEGER;
  if (b) {
    const l = digits(b);
    if (l === undefined || l < first) return FULL;
    last = l;
  }
  if (first >= size) return UNSAT;
  return { kind: 'part', first, last: Math.min(last, size - 1) };
}
