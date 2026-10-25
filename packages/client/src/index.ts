// @zen/client: the zen-serve client library (docs/CLIENT.md).
export { type AclVersion, claim, fullGrants, member, verifyNext } from './acl.js';
export * as bytes from './bytes.js';
export { Client, type ConnectOptions, connect, type DeviceCredentials } from './client.js';
export { type ClockStore, memoryClockStore, SessionClock, TreeClock } from './clock.js';
export { type NewDevice, newDevice } from './devices.js';
export { isCode, ZenError } from './errors.js';
export {
  Fs,
  type HeaderOptions,
  openHeader,
  prfSlotSalts,
  SlotType,
  type StoredHeader,
  type Unlock,
  UnlockedFs,
} from './fs.js';
export {
  type AlarmObject,
  type DevicePin,
  entryKey,
  Integrity,
  type IntegrityAlarm,
  IntegrityError,
  type IntegrityOptions,
  type Signer,
  type TopicPin,
} from './integrity.js';
export {
  commitTransaction,
  Kv,
  type KvEntry,
  type Outcome,
  type Path,
  type RangeOptions,
  sendCommit,
  Transaction,
  type TxEntry,
  type TxnOptions,
  transaction,
} from './kv.js';
export { indexedDbLocalStore } from './local/idb.js';
export {
  type SealedStore,
  type SealedStoreOptions,
  sealedStore,
} from './local/sealed.js';
export { type LocalStore, memoryLocalStore } from './local/store.js';
export {
  type BroadcastChannelLike,
  type ChannelFactory,
  defaultChannel,
  defaultLocks,
  type LockManagerLike,
  Owner,
} from './local/tabs.js';
export {
  type CredentialVault,
  credentialVault,
  type StoredDevice,
  type VaultOptions,
  type VaultSignInOptions,
} from './local/vault.js';
export {
  type AppendOptions,
  ChainCheck,
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
  type CheckpointPush,
  type EphemeralMessage,
  offsetAfterVersion,
  Stream,
  type StreamEvent,
  type StreamOptions,
  type StreamTarget,
  type SubscribeOptions,
  Subscription,
  type SvcPush,
} from './stream.js';
export {
  REVEAL_ALREADY,
  REVEAL_BAD_KEY,
  REVEAL_NOT_COMMITTED,
  REVEALED,
  Service,
  SVC_FRAME,
  type SvcConnectOptions,
  SvcSocket,
} from './svc.js';
export { Transport, type TransportOptions } from './transport.js';
export {
  CHUNK_SIZE,
  type ChangeBatch,
  type ChunkCache,
  type ChunkIndex,
  type CreateOptions,
  computeChain,
  displayNames,
  type FileSource,
  type FileVersion,
  type KnownChunk,
  type NodeStat,
  type OpRecord,
  opBytes,
  type PendingOp,
  type Prepared,
  type ReadStreamOptions,
  ROOT,
  TRASH,
  Tree,
  TreeBatch,
  type TreeChange,
  type TreeNode,
  type TreeOptions,
  type Upload,
  type UploadOptions,
  type UploadState,
  type WriteOptions,
  type WrittenFile,
} from './tree.js';
export { initWasm, zw } from './wasm.js';
