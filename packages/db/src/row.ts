// Rows (spec/zendb.md §4): the Row record, parts of large rows, padding.
import { zw } from '@zen/client';
import { decode, decodePrefix, encode, headLen, isPlainObject, type Value } from './cbor.js';
import { DbError } from './errors.js';

/** A row: text field names to values. */
export type Fields = { [field: string]: Value | undefined };

/** A decoded Row record (§4.1). */
export interface RowRecord {
  pk: Value;
  fields: Fields;
  parts?: number;
  digest?: Uint8Array;
}

/** Row size buckets (§4.3): 256 B, 1 KiB, 4 KiB, 16 KiB, then multiples of 16 KiB. */
export function rowBucket(len: number): number {
  if (len <= 256) return 256;
  if (len <= 1024) return 1024;
  if (len <= 4096) return 4096;
  return Math.ceil(len / 16384) * 16384;
}

/** Size of the zero-padded last part of a padded row's parts (§4.3). */
const PART_PAD = 16384;

/**
 * The encoded Row `{1: pk, 2: fields, 3?: parts, 4?: digest, 5?: pad}`.
 * With `pad`, the pad is the longest that keeps the Row within its bucket.
 */
export function encodeRow(
  pk: Value,
  fields: Fields,
  parts: { n: number; digest: Uint8Array } | undefined,
  pad: boolean,
): Uint8Array {
  const m = new Map<Value, Value>([
    [1, pk],
    [2, fields],
  ]);
  if (parts) {
    m.set(3, parts.n);
    m.set(4, parts.digest);
  }
  if (!pad) return encode(m);
  m.set(5, new Uint8Array(0));
  const base = encode(m).length - 1; // without the empty pad's head
  const room = rowBucket(base + 1) - base;
  let p = Math.max(room - 1, 0);
  while (p > 0 && headLen(p) + p > room) p--;
  m.set(5, new Uint8Array(p));
  return encode(m);
}

/** Decode a Row record. */
export function decodeRow(bytes: Uint8Array): RowRecord {
  const m = decode(bytes);
  if (!(m instanceof Map) || !m.has(1) || !isPlainObject(m.get(2))) {
    throw new DbError('corrupt', 'not a Row');
  }
  const r: RowRecord = { pk: m.get(1)!, fields: m.get(2) as Fields };
  const parts = m.get(3);
  const digest = m.get(4);
  if (parts !== undefined) {
    if (typeof parts !== 'number' || !(digest instanceof Uint8Array) || digest.length !== 32) {
      throw new DbError('corrupt', 'bad Row parts');
    }
    r.parts = parts;
    r.digest = digest;
  }
  return r;
}

/** `H("zen/v1/db-parts-digest", bytes)`. */
export const partsDigest = (b: Uint8Array): Uint8Array => zw.partsDigest(b);

/** Cut `bytes` into parts of `size`; with `padLast`, zero-pad the last one (§4.3). */
export function cutParts(bytes: Uint8Array, size: number, padLast: boolean): Uint8Array[] {
  const out: Uint8Array[] = [];
  for (let i = 0; i < bytes.length; i += size) out.push(bytes.slice(i, i + size));
  if (padLast && out.length) {
    const last = out.at(-1)!;
    const len = Math.min(size, Math.ceil(last.length / PART_PAD) * PART_PAD);
    const p = new Uint8Array(len);
    p.set(last);
    out[out.length - 1] = p;
  }
  return out;
}

/**
 * Join parts and decode the one CBOR item they hold: every byte after it
 * must be zero, and the item's bytes must match `digest` (§4.2, §4.3).
 */
export function joinParts(parts: Uint8Array[], digest: Uint8Array): Value {
  const total = parts.reduce((n, p) => n + p.length, 0);
  const all = new Uint8Array(total);
  let o = 0;
  for (const p of parts) {
    all.set(p, o);
    o += p.length;
  }
  const [v, n] = decodePrefix(all);
  for (let i = n; i < all.length; i++) {
    if (all[i] !== 0) throw new DbError('corrupt', 'non-zero bytes after the parts');
  }
  const d = partsDigest(all.subarray(0, n));
  if (d.length !== digest.length || d.some((x, i) => x !== digest[i])) {
    throw new DbError('corrupt', 'the parts do not match their digest');
  }
  return v;
}

/** How a row is stored: the Row record and its parts. */
export interface StoredRow {
  row: Uint8Array;
  parts: Uint8Array[];
}

/**
 * Lay out a row for storage. Values hold `maxValueBytes − 48` bytes of
 * plaintext. Fields over `maxValueBytes − 1024` bytes go to parts (§4.2),
 * and so does a padded Row that its bucket would push past one value.
 */
export function layoutRow(
  pk: Value,
  fields: Fields,
  pad: boolean,
  maxValueBytes: number,
): StoredRow {
  const f = encode(fields);
  const partSize = maxValueBytes - 1024;
  if (f.length <= partSize) {
    const row = encodeRow(pk, fields, undefined, pad);
    if (row.length <= maxValueBytes - 48) return { row, parts: [] };
  }
  const parts = cutParts(f, partSize, pad);
  const row = encodeRow(pk, {}, { n: parts.length, digest: partsDigest(f) }, pad);
  if (row.length > maxValueBytes - 48)
    throw new DbError('too_large', 'the primary key is too large');
  return { row, parts };
}
