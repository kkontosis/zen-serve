// @zen/client: the zen-serve client library (docs/CLIENT.md).
export { type AclVersion, claim, fullGrants, member, verifyNext } from './acl.js';
export * as bytes from './bytes.js';
export { Client, type ConnectOptions, connect, type DeviceCredentials } from './client.js';
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
export { Topic } from './log.js';
export {
  type Authenticator,
  browserAuthenticator,
  type CreateRequest,
  type CreateResult,
  type GetRequest,
  type GetResult,
} from './passkey.js';
export { type Argon2, Session, type UnlockMaterial } from './session.js';
export { Stream } from './stream.js';
export { Transport, type TransportOptions } from './transport.js';
export { Tree } from './tree.js';
export { initWasm, zw } from './wasm.js';
