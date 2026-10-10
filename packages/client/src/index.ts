// @zen/client: the zen-serve client library (docs/CLIENT.md).
export { type AclVersion, claim, fullGrants, member, verifyNext } from './acl.js';
export * as bytes from './bytes.js';
export { Client, type ConnectOptions, connect, type DeviceCredentials } from './client.js';
export { type ClockStore, memoryClockStore, SessionClock, TreeClock } from './clock.js';
export { isCode, ZenError } from './errors.js';
export {
  Fs,
  openHeader,
  prfSlotSalts,
  SlotType,
  type StoredHeader,
  type Unlock,
  UnlockedFs,
} from './fs.js';
export {
  Kv,
  type KvEntry,
  type Path,
  type RangeOptions,
  sendCommit,
  Transaction,
  type TxnOptions,
  transaction,
} from './kv.js';
export {
  type AppendOptions,
  Consumer,
  type Delivery,
  Dlq,
  type DlqEntry,
  type GroupOptions,
  Leader,
  type LeaderOptions,
  type LeaseOptions,
  type NextOptions,
  type ReadOptions,
  Topic,
  type TopicEvent,
} from './log.js';
export {
  type Authenticator,
  browserAuthenticator,
  type CreateRequest,
  type CreateResult,
  type GetRequest,
  type GetResult,
} from './passkey.js';
export { type Argon2, Session, type UnlockMaterial } from './session.js';
export {
  type EphemeralMessage,
  Stream,
  type StreamEvent,
  type StreamOptions,
  type StreamTarget,
  type SubscribeOptions,
  Subscription,
} from './stream.js';
export { Transport, type TransportOptions } from './transport.js';
export {
  CHUNK_SIZE,
  type ChangeBatch,
  type ChunkCache,
  type ChunkIndex,
  type CreateOptions,
  computeChain,
  displayNames,
  type FileVersion,
  type NodeStat,
  type OpRecord,
  opBytes,
  type PendingOp,
  type Prepared,
  ROOT,
  TRASH,
  Tree,
  TreeBatch,
  type TreeChange,
  type TreeNode,
  type TreeOptions,
  type Upload,
  type WriteOptions,
  type WrittenFile,
} from './tree.js';
export { initWasm, zw } from './wasm.js';
