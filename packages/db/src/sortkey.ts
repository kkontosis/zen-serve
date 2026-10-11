// Order-preserving sort keys of private indexes (spec/zendb.md §5.1), and
// the type rules of indexed fields. The Rust reference is zen-core's
// `db::sortkey`.
import { isIntNumber, type Value } from './cbor.js';
import { DbError } from './errors.js';

/** A declared field type (§3.2). */
export type FieldType = 'text' | 'bytes' | 'int' | 'float' | 'bool';

/** Longest sort key an index accepts (§5.1). */
export const MAX_SORT_KEY = 4096;

const te = new TextEncoder();

type Norm =
  | { t: 'null' }
  | { t: 'bool'; v: boolean }
  | { t: 'int'; v: bigint }
  | { t: 'float'; v: number }
  | { t: 'text'; v: string }
  | { t: 'bytes'; v: Uint8Array }
  | { t: 'other' };

// A value as the encoding sees it: integral floats in ±2^53 are integers (§1).
function norm(v: Value | undefined): Norm {
  if (v === null || v === undefined) return { t: 'null' };
  if (typeof v === 'boolean') return { t: 'bool', v };
  if (typeof v === 'bigint') return { t: 'int', v };
  if (typeof v === 'number') {
    if (Number.isNaN(v)) return { t: 'float', v };
    return isIntNumber(v) ? { t: 'int', v: BigInt(v) } : { t: 'float', v };
  }
  if (typeof v === 'string') return { t: 'text', v };
  if (v instanceof Uint8Array) return { t: 'bytes', v };
  return { t: 'other' };
}

/** Whether `v` fits a field of type `ty`: null always; numbers per §5.1. */
export function fitsType(v: Value | undefined, ty: FieldType): boolean {
  const n = norm(v);
  switch (n.t) {
    case 'null':
      return true;
    case 'int':
      return ty === 'int' || ty === 'float';
    case 'float':
      return ty === 'float' && !Number.isNaN(n.v);
    case 'other':
      return false;
    default:
      return n.t === ty;
  }
}

const u64 = (x: bigint) => {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt.asUintN(64, x));
  return b;
};

function escaped(out: number[], tag: number, b: Uint8Array): void {
  out.push(tag);
  for (const x of b) {
    out.push(x);
    if (x === 0) out.push(0xff);
  }
  out.push(0);
}

function int(out: number[], i: bigint): void {
  if (i <= 2n ** 63n - 1n) {
    out.push(0x20, ...u64(BigInt.asUintN(64, i) ^ (1n << 63n)));
  } else {
    out.push(0x21, ...u64(i));
  }
}

function float(out: number[], f: number): void {
  if (Number.isNaN(f)) throw new DbError('bad_type', 'NaN in an index');
  const b = new DataView(new ArrayBuffer(8));
  b.setFloat64(0, f === 0 ? 0 : f);
  const bits = b.getBigUint64(0);
  out.push(0x28, ...u64(bits >> 63n === 0n ? bits ^ (1n << 63n) : ~bits));
}

/**
 * Append the encoding of one component. With `ty` the value must fit the
 * declared type; without it the value's own type decides (pk components).
 */
export function component(out: number[], v: Value | undefined, ty?: FieldType): void {
  const n = norm(v);
  const mismatch = () => new DbError('bad_type', `a ${n.t} value in a ${ty} field`);
  switch (n.t) {
    case 'null':
      out.push(0x01);
      return;
    case 'bool':
      if (ty && ty !== 'bool') throw mismatch();
      out.push(n.v ? 0x11 : 0x10);
      return;
    case 'int':
      if (ty === 'float') float(out, Number(n.v));
      else if (!ty || ty === 'int') int(out, n.v);
      else throw mismatch();
      return;
    case 'float':
      if (ty && ty !== 'float') throw mismatch();
      float(out, n.v);
      return;
    case 'text':
      if (ty && ty !== 'text') throw mismatch();
      escaped(out, 0x30, te.encode(n.v));
      return;
    case 'bytes':
      if (ty && ty !== 'bytes') throw mismatch();
      escaped(out, 0x40, n.v);
      return;
    default:
      throw new DbError('bad_type', 'an array or map in an index');
  }
}

/** One indexed field: its value, declared type and direction. */
export interface SortField {
  value: Value | undefined;
  type: FieldType;
  desc: boolean;
}

/**
 * The sort key of an index entry: each field (inverted when descending),
 * then the pk components ascending. Throws `bad_type` on a type mismatch
 * and `too_large` over 4,096 bytes.
 */
export function sortKey(fields: SortField[], pk: Value[]): Uint8Array {
  const out: number[] = [];
  for (const f of fields) {
    const start = out.length;
    component(out, f.value, f.type);
    if (f.desc) for (let i = start; i < out.length; i++) out[i] = out[i]! ^ 0xff;
  }
  for (const p of pk) {
    if (p === null || Array.isArray(p) || (typeof p === 'object' && !(p instanceof Uint8Array))) {
      throw new DbError('bad_type', 'a pk component that is null, an array or a map');
    }
    component(out, p);
  }
  if (out.length > MAX_SORT_KEY) throw new DbError('too_large', 'a sort key over 4,096 bytes');
  return Uint8Array.from(out);
}

/** The encoding of one field's value as a range bound: by `type`, inverted when `desc`. */
export function encodeComponent(v: Value, type: FieldType, desc: boolean): Uint8Array {
  const out: number[] = [];
  component(out, v, type);
  if (desc) for (let i = 0; i < out.length; i++) out[i] = out[i]! ^ 0xff;
  return Uint8Array.from(out);
}

/**
 * The bytes every text or bytes value starting with `v` begins with: the
 * tag and the escaped bytes, without the terminator. Inverted when `desc`.
 */
export function encodePrefix(v: string | Uint8Array, desc: boolean): Uint8Array {
  const out: number[] = [];
  escaped(out, typeof v === 'string' ? 0x30 : 0x40, typeof v === 'string' ? te.encode(v) : v);
  out.pop(); // the terminator
  if (desc) for (let i = 0; i < out.length; i++) out[i] = out[i]! ^ 0xff;
  return Uint8Array.from(out);
}

const td = new TextDecoder('utf-8', { fatal: true });
const corrupt = () => new DbError('corrupt', 'a malformed sort key');

// Reads components of a sort key; `mask` un-inverts a descending field.
class Reader {
  i = 0;
  constructor(readonly b: Uint8Array) {}

  byte(mask: number): number {
    if (this.i >= this.b.length) throw corrupt();
    return this.b[this.i++]! ^ mask;
  }

  u64(mask: number): bigint {
    let x = 0n;
    for (let k = 0; k < 8; k++) x = (x << 8n) | BigInt(this.byte(mask));
    return x;
  }

  escaped(mask: number): Uint8Array {
    const out: number[] = [];
    for (;;) {
      const x = this.byte(mask);
      if (x !== 0) {
        out.push(x);
        continue;
      }
      if (this.i < this.b.length && (this.b[this.i]! ^ mask) === 0xff) {
        this.i++;
        out.push(0);
        continue;
      }
      return Uint8Array.from(out);
    }
  }

  component(mask: number): Value {
    const tag = this.byte(mask);
    switch (tag) {
      case 0x01:
        return null;
      case 0x10:
        return false;
      case 0x11:
        return true;
      case 0x20: {
        const i = BigInt.asIntN(64, this.u64(mask) ^ (1n << 63n));
        return i >= -(2n ** 53n) && i <= 2n ** 53n ? Number(i) : i;
      }
      case 0x21:
        return this.u64(mask);
      case 0x28: {
        const x = this.u64(mask);
        const bits = x >> 63n === 1n ? x ^ (1n << 63n) : BigInt.asUintN(64, ~x);
        const d = new DataView(new ArrayBuffer(8));
        d.setBigUint64(0, bits);
        return d.getFloat64(0);
      }
      case 0x30:
        try {
          return td.decode(this.escaped(mask));
        } catch {
          throw corrupt();
        }
      case 0x40:
        return this.escaped(mask);
      default:
        throw corrupt();
    }
  }
}

/**
 * Split a sort key into its fields' values (by their directions) and the
 * pk components (§5.1).
 */
export function decodeSortKey(
  fields: { desc: boolean }[],
  key: Uint8Array,
): { values: Value[]; pk: Value[] } {
  const r = new Reader(key);
  const values = fields.map((f) => r.component(f.desc ? 0xff : 0));
  const pk: Value[] = [];
  while (r.i < key.length) pk.push(r.component(0));
  return { values, pk };
}
