// The stored keys of a database (spec/zendb.md §2.1). Every key is
// `D ‖ elements` with `D = ("zen", "db", ns)`; the server sees 16-byte PRF
// tokens per element, so a prefix of elements is a prefix of stored keys.
import { bytes } from '@zen/client';
import type { DbKeys } from '@zen/wasm';

/** A path element: text (UTF-8) or bytes. */
export type Element = string | Uint8Array;

/** `u32(x)` as an element. */
export function u32(x: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, x);
  return b;
}

/** `u64(x)` as an element. */
export function u64(x: number | bigint): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(x));
  return b;
}

/** The stored keys of one database. */
export class Keys {
  constructor(readonly db: DbKeys) {}

  /** The stored key of `D ‖ elements`. */
  key(...elements: Element[]): Uint8Array {
    return this.db.key(elements.map((e) => (typeof e === 'string' ? bytes.utf8(e) : e)));
  }

  /** The stored-key range of everything under `D ‖ elements`. */
  range(...elements: Element[]): [Uint8Array, Uint8Array | undefined] {
    const begin = this.key(...elements);
    return [begin, bytes.prefixEnd(begin)];
  }

  dbRecord = () => this.key('cat', 'db');
  tableRecord = (name: string) => this.key('cat', 't', name);
  tables = () => this.range('cat', 't');
  migration = (step: number) => this.key('cat', 'm', u64(step));
  row = (table: Uint8Array, pk: Uint8Array) => this.key('t', table, pk);
  rows = (table: Uint8Array) => this.range('t', table);
  part = (table: Uint8Array, pk: Uint8Array, i: number) => this.key('o', table, pk, u32(i));
  unique = (index: Uint8Array, value: Uint8Array) => this.key('u', index, value);
  fast = (index: Uint8Array, value: Uint8Array, pk: Uint8Array) => this.key('f', index, value, pk);
  fastValue = (index: Uint8Array, value: Uint8Array) => this.range('f', index, value);
}
