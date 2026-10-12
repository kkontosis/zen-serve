// Fractional position keys: a string that sorts strictly between two others,
// so moving an item rewrites only that item (EXAMPLES.md, kanban "reorder").
// Keys use the digits 0-9A-Za-z (ASCII order) and never end in '0', so there
// is always room between two of them.

const DIGITS = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz';
const BASE = DIGITS.length;
const idx = (c: string): number => DIGITS.indexOf(c);

function check(k: string): void {
  if (!k || k.endsWith('0') || [...k].some((c) => idx(c) < 0)) {
    throw new RangeError(`not a position key: ${JSON.stringify(k)}`);
  }
}

/** A key strictly between `a` ('' = the start) and `b` (undefined = the end). */
function midpoint(a: string, b: string | undefined): string {
  if (b !== undefined) {
    // Keep the common prefix (`a` read as padded with zeros).
    let n = 0;
    while (n < b.length && (a[n] ?? '0') === b[n]) n++;
    if (n > 0) return b.slice(0, n) + midpoint(a.slice(n), b.slice(n));
  }
  const da = a ? idx(a[0]!) : 0;
  const db = b !== undefined ? idx(b[0]!) : BASE;
  if (db - da > 1) {
    // Appending and prepending step by one digit, so a list grown at either
    // end gains a character every ~30 items; inserts between two keys halve.
    if (b === undefined && a) return DIGITS[da + 1]!;
    if (!a && b !== undefined) return DIGITS[db - 1]!;
    return DIGITS[Math.round((da + db) / 2)]!;
  }
  // Adjacent first digits: b's first digit alone, if b goes on, is between.
  if (b !== undefined && b.length > 1) return b[0]!;
  return DIGITS[da]! + midpoint(a.slice(1), undefined);
}

/**
 * A key strictly between `a` and `b` (`undefined`: the start or the end).
 * `keyBetween()` is the first key of an empty list.
 */
export function keyBetween(a?: string, b?: string): string {
  if (a !== undefined) check(a);
  if (b !== undefined) check(b);
  if (a !== undefined && b !== undefined && a >= b) {
    throw new RangeError(`${JSON.stringify(a)} is not before ${JSON.stringify(b)}`);
  }
  return midpoint(a ?? '', b);
}

/** `n` keys in order, between `a` and `b`. */
export function keysBetween(a: string | undefined, b: string | undefined, n: number): string[] {
  const out: string[] = [];
  let prev = a;
  for (let i = 0; i < n; i++) {
    prev = keyBetween(prev, b);
    out.push(prev);
  }
  return out;
}
