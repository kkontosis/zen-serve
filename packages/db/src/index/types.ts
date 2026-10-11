// What index maintenance gets for one row (spec/zendb.md §5.5).
import type { Transaction } from '@zen/client';
import type { IndexDef, TableRecord } from '../catalog.js';
import type { Value } from '../cbor.js';
import type { Keys } from '../keys.js';

/** The part of a transaction index code uses. */
export interface IndexTx {
  raw: Transaction;
  keys: Keys;
}

/**
 * One row's change in one index. `old` and `new` are the CBOR of the
 * indexed values, absent when the row has no entry (no row, or a null in
 * the indexed value).
 */
export interface IndexChange {
  tx: IndexTx;
  table: TableRecord;
  index: IndexDef;
  pk: Value;
  pkElement: Uint8Array;
  old: Uint8Array | undefined;
  new: Uint8Array | undefined;
}
