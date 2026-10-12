// @zen/db: a client-side database over zen-serve's encrypted KV
// (spec/zendb.md, docs/DB.md).

export {
  appendMsg,
  attemptOf,
  type ChangeBody,
  type ConsumeOptions,
  ConsumerHandle,
  type Delivered,
  DirectSource,
  type EmitOptions,
  type EphemeralEvent,
  type EphemeralMsg,
  type OnHandler,
  type OnOptions,
  OnSubscription,
  type OnTarget,
  openMessage,
  type SourceSub,
  type StreamSource,
} from './broker.js';
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
export { CRDT_FEATURE, type CrdtFieldType } from './crdt.js';
export type { CrdtRead } from './crdtcache.js';
export {
  CrdtQueue,
  type Inflight,
  memoryQueueStore,
  type QueueEntry,
  type QueueRow,
  type QueueStore,
  type SyncEvent,
} from './crdtq.js';
export {
  Db,
  type DbOptions,
  type DbStats,
  type DbTxnOptions,
  type ImportOptions,
  type ServeOptions,
  Table,
  type TableOptions,
} from './db.js';
export { DbError, type DbErrorCode } from './errors.js';
export { TxFiles } from './files.js';
export { localQueueStore } from './localq.js';
export {
  type CreateIndexOptions,
  type CreateTableOptions,
  type FieldSpec,
  type MigrationFn,
  Migrator,
} from './migrate.js';
export { type BodyRef, decodeMsg, encodeMsg, eventId, type Message, type Msg } from './msg.js';
export { type BroadcastChannelLike, type ChannelFactory, TabSource } from './proxy.js';
export { type Cursor, compareValues, type Op, type Page, Query } from './query.js';
export {
  type Reply,
  ReplyError,
  type RequestAllOptions,
  type RequestOptions,
} from './reply.js';
export { decodeRow, encodeRow, type Fields, layoutRow, rowBucket } from './row.js';
export {
  SAGA_TIMEOUT,
  Saga,
  type SagaOptions,
  type SagaState,
  type SagaStatus,
  type SagaStep,
} from './saga.js';
export { type ScheduleOptions, Scheduler, type SchedulerOptions, type When } from './sched.js';
export { type FieldType, MAX_SORT_KEY, type SortField, sortKey } from './sortkey.js';
export { SYSTEM_TABLES } from './system.js';
export { type LockManagerLike, Owner } from './tabs.js';
export { DbTransaction, ManualTransaction, TableTx } from './txn.js';
