// @zen/fs: a local replica of a zen-serve filesystem tree (spec/fs.md §11,
// docs/CLIENT.md). Worker-safe: no DOM or Node imports.
export { FileHandle, type OpenFlags, type OpenOptions } from './handle.js';
export { isLocal, type LocalVersion, type ShownVersion } from './overlay.js';
export {
  CONTENT_TYPE,
  conflictName,
  type DirEntry,
  type Entry,
  type NodeType,
  openReplica,
  Replica,
  type ReplicaOptions,
  type ReplicaStatus,
  type Stat,
  type WriteCallOptions,
  type WriteFileOptions,
} from './replica.js';
export { replicaStores } from './store.js';
export { decodeXattrs, encodeXattrs } from './xattr.js';
