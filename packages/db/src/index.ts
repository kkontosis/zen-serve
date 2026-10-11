// @zen/db: a client-side database over zen-serve's encrypted KV
// (spec/zendb.md, docs/DB.md).

export type {
  ChangeDef,
  DbRecord,
  IndexDef,
  IndexField,
  IndexKind,
  MigrationRecord,
  TableRecord,
} from './catalog.js';
export type { Value } from './cbor.js';
export * as cbor from './cbor.js';
export { Db, type DbOptions, type DbTxnOptions, Table } from './db.js';
export { DbError, type DbErrorCode } from './errors.js';
export {
  type CreateIndexOptions,
  type CreateTableOptions,
  type FieldSpec,
  type MigrationFn,
  Migrator,
} from './migrate.js';
export { type Cursor, compareValues, type Op, type Page, Query } from './query.js';
export { decodeRow, encodeRow, type Fields, layoutRow, rowBucket } from './row.js';
export { type FieldType, MAX_SORT_KEY, type SortField, sortKey } from './sortkey.js';
export { DbTransaction, TableTx } from './txn.js';
