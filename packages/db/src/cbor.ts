// Deterministic CBOR for zen-db values (spec/zendb.md §1). The Rust
// reference is zen-core's `db::cbor`; both reproduce
// spec/test-vectors/zendb.json.
import { DbError } from './errors.js';

/** A zen-db value. Maps with text keys are plain objects; others are `Map`s. */
export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Uint8Array
  | Value[]
  | { [key: string]: Value | undefined }
  | Map<Value, Value>;

const SAFE = 2 ** 53;
const INT_MIN = -(2n ** 63n);
const INT_MAX = 2n ** 64n - 1n;
/** Nesting limit, as the Rust decoder's. */
export const MAX_DEPTH = 64;

const te = new TextEncoder();
const td = new TextDecoder('utf-8', { fatal: true });

const bad = (why: string) => new DbError('bad_value', why);

/** Whether a number is encoded as an integer (§1). */
export const isIntNumber = (x: number) => Number.isInteger(x) && Math.abs(x) <= SAFE;

/** Whether `v` is a plain object (an object literal or `Object.create(null)`). */
export function isPlainObject(v: unknown): v is Record<string, Value | undefined> {
  if (v === null || typeof v !== 'object') return false;
  const p = Object.getPrototypeOf(v);
  return p === Object.prototype || p === null;
}

class Out {
  buf = new Uint8Array(256);
  n = 0;

  room(k: number): void {
    if (this.n + k <= this.buf.length) return;
    let size = this.buf.length * 2;
    while (size < this.n + k) size *= 2;
    const b = new Uint8Array(size);
    b.set(this.buf.subarray(0, this.n));
    this.buf = b;
  }

  byte(x: number): void {
    this.room(1);
    this.buf[this.n++] = x;
  }

  bytes(b: Uint8Array): void {
    this.room(b.length);
    this.buf.set(b, this.n);
    this.n += b.length;
  }

  head(major: number, arg: number | bigint): void {
    const m = major << 5;
    if (typeof arg === 'bigint' && arg <= 0xffff_ffffn) arg = Number(arg);
    if (typeof arg === 'number') {
      if (arg < 24) this.byte(m | arg);
      else if (arg <= 0xff) {
        this.byte(m | 24);
        this.byte(arg);
      } else if (arg <= 0xffff) {
        this.byte(m | 25);
        this.byte(arg >> 8);
        this.byte(arg & 0xff);
      } else if (arg <= 0xffff_ffff) {
        this.byte(m | 26);
        this.room(4);
        new DataView(this.buf.buffer).setUint32(this.n, arg);
        this.n += 4;
      } else {
        this.head(major, BigInt(arg));
      }
      return;
    }
    this.byte(m | 27);
    this.room(8);
    new DataView(this.buf.buffer).setBigUint64(this.n, arg);
    this.n += 8;
  }

  result(): Uint8Array {
    return this.buf.slice(0, this.n);
  }
}

/** The length of a head with argument `arg`. */
export function headLen(arg: number): number {
  if (arg < 24) return 1;
  if (arg <= 0xff) return 2;
  if (arg <= 0xffff) return 3;
  if (arg <= 0xffff_ffff) return 5;
  return 9;
}

function encInt(o: Out, i: bigint | number): void {
  if (typeof i === 'number') {
    if (i >= 0) o.head(0, i);
    else o.head(1, -1 - i);
    return;
  }
  if (i < INT_MIN || i > INT_MAX) throw bad('an integer outside −2^63 … 2^64−1');
  if (i >= 0n) o.head(0, i);
  else o.head(1, -1n - i);
}

function encNumber(o: Out, x: number): void {
  if (Number.isNaN(x)) throw bad('NaN');
  if (isIntNumber(x)) {
    encInt(o, x === 0 ? 0 : x);
    return;
  }
  o.byte(0xfb);
  o.room(8);
  new DataView(o.buf.buffer).setFloat64(o.n, x);
  o.n += 8;
}

function encMap(o: Out, entries: [Value, Value | undefined][], depth: number): void {
  const items: [Uint8Array, Uint8Array][] = [];
  for (const [k, v] of entries) {
    if (v === undefined) continue;
    const okKey =
      typeof k === 'string' ||
      (typeof k === 'number' && isIntNumber(k) && k >= 0) ||
      (typeof k === 'bigint' && k >= 0n);
    if (!okKey) throw bad('a map key that is neither text nor an unsigned integer');
    const kb = new Out();
    enc(kb, k, depth + 1);
    const vb = new Out();
    enc(vb, v, depth + 1);
    items.push([kb.result(), vb.result()]);
  }
  items.sort((a, b) => cmp(a[0], b[0]));
  for (let i = 1; i < items.length; i++) {
    if (cmp(items[i - 1]![0], items[i]![0]) === 0) throw bad('a duplicate map key');
  }
  o.head(5, items.length);
  for (const [k, v] of items) {
    o.bytes(k);
    o.bytes(v);
  }
}

function enc(o: Out, v: Value, depth: number): void {
  if (depth > MAX_DEPTH) throw bad('nested too deeply');
  if (v === null) o.byte(0xf6);
  else if (v === false) o.byte(0xf4);
  else if (v === true) o.byte(0xf5);
  else if (typeof v === 'number') encNumber(o, v);
  else if (typeof v === 'bigint') encInt(o, v);
  else if (typeof v === 'string') {
    const b = te.encode(v);
    o.head(3, b.length);
    o.bytes(b);
  } else if (v instanceof Uint8Array) {
    o.head(2, v.length);
    o.bytes(v);
  } else if (Array.isArray(v)) {
    o.head(4, v.length);
    for (const x of v) {
      if (x === undefined) throw bad('undefined in an array');
      enc(o, x, depth + 1);
    }
  } else if (v instanceof Map) {
    encMap(o, [...v.entries()], depth);
  } else if (isPlainObject(v)) {
    encMap(o, Object.entries(v), depth);
  } else {
    const u = v as unknown;
    throw bad(
      `a value CBOR can't hold: ${typeof u === 'object' ? u?.constructor?.name : typeof u}`,
    );
  }
}

function cmp(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return a.length - b.length;
}

/** Encode a value deterministically. Throws `bad_value` for what §1 refuses. */
export function encode(v: Value): Uint8Array {
  if (v === undefined) throw bad('undefined');
  const o = new Out();
  enc(o, v, 0);
  return o.result();
}

class Dec {
  i = 0;
  readonly view: DataView;

  constructor(readonly b: Uint8Array) {
    this.view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  }

  need(n: number): void {
    if (this.i + n > this.b.length) throw new DbError('corrupt', 'truncated CBOR');
  }

  arg(info: number): number | bigint {
    if (info < 24) return info;
    switch (info) {
      case 24:
        this.need(1);
        return this.b[this.i++]!;
      case 25:
        this.need(2);
        this.i += 2;
        return this.view.getUint16(this.i - 2);
      case 26:
        this.need(4);
        this.i += 4;
        return this.view.getUint32(this.i - 4);
      case 27: {
        this.need(8);
        this.i += 8;
        const x = this.view.getBigUint64(this.i - 8);
        return x <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(x) : x;
      }
      default:
        throw new DbError('corrupt', 'a reserved or indefinite CBOR length');
    }
  }

  len(info: number): number {
    const n = this.arg(info);
    // Every item takes at least one byte, so a longer length can't be real.
    if (typeof n === 'bigint' || n > this.b.length - this.i) {
      throw new DbError('corrupt', 'a CBOR length past the end');
    }
    return n;
  }

  value(depth: number): Value {
    if (depth > MAX_DEPTH) throw new DbError('corrupt', 'CBOR nested too deeply');
    this.need(1);
    const ib = this.b[this.i++]!;
    const major = ib >> 5;
    const info = ib & 0x1f;
    switch (major) {
      case 0:
        return int(this.arg(info));
      case 1: {
        const a = this.arg(info);
        return int(typeof a === 'number' ? -1 - a : -1n - a);
      }
      case 2: {
        const n = this.len(info);
        this.i += n;
        return this.b.slice(this.i - n, this.i);
      }
      case 3: {
        const n = this.len(info);
        this.i += n;
        try {
          return td.decode(this.b.subarray(this.i - n, this.i));
        } catch {
          throw new DbError('corrupt', 'invalid UTF-8 in CBOR text');
        }
      }
      case 4: {
        const n = this.len(info);
        const a: Value[] = [];
        for (let k = 0; k < n; k++) a.push(this.value(depth + 1));
        return a;
      }
      case 5: {
        const n = this.len(info);
        const keys: Value[] = [];
        const vals: Value[] = [];
        const seen = new Set<string>();
        for (let k = 0; k < n; k++) {
          const start = this.i;
          const key = this.value(depth + 1);
          const ok =
            typeof key === 'string' ||
            (typeof key === 'number' && key >= 0) ||
            (typeof key === 'bigint' && key >= 0n);
          const tag = String.fromCharCode(...this.b.subarray(start, this.i));
          if (!ok || seen.has(tag)) throw new DbError('corrupt', 'a bad or duplicate map key');
          seen.add(tag);
          keys.push(key);
          vals.push(this.value(depth + 1));
        }
        if (keys.every((k) => typeof k === 'string')) {
          const obj: Record<string, Value> = {};
          keys.forEach((k, j) => {
            Object.defineProperty(obj, k as string, {
              value: vals[j],
              enumerable: true,
              writable: true,
              configurable: true,
            });
          });
          return obj;
        }
        return new Map(keys.map((k, j) => [k, vals[j]!]));
      }
      case 7:
        if (info === 20) return false;
        if (info === 21) return true;
        if (info === 22) return null;
        if (info === 27) {
          this.need(8);
          this.i += 8;
          const f = this.view.getFloat64(this.i - 8);
          if (Number.isNaN(f)) throw new DbError('corrupt', 'NaN in CBOR');
          return f;
        }
        throw new DbError('corrupt', 'a CBOR simple value zen-db does not use');
      default:
        throw new DbError('corrupt', 'a CBOR tag');
    }
  }
}

function int(x: number | bigint): number | bigint {
  if (typeof x === 'number') return x;
  return x >= -BigInt(SAFE) && x <= BigInt(SAFE) ? Number(x) : x;
}

/** Decode exactly one value. */
export function decode(bytes: Uint8Array): Value {
  const d = new Dec(bytes);
  const v = d.value(0);
  if (d.i !== bytes.length) throw new DbError('corrupt', 'trailing bytes after CBOR');
  return v;
}

/** Decode one value at the start of `bytes`: the value and its length. */
export function decodePrefix(bytes: Uint8Array): [Value, number] {
  const d = new Dec(bytes);
  const v = d.value(0);
  return [v, d.i];
}

/** Whether two values encode alike. */
export function same(a: Value, b: Value): boolean {
  return cmp(encode(a), encode(b)) === 0;
}
