// @zen/fs: a local replica of a zen-serve filesystem tree (spec/fs.md §11,
// docs/CLIENT.md). Worker-safe: no DOM or Node imports.
export {
  conflictName,
  type DirEntry,
  type Entry,
  type NodeType,
  openReplica,
  Replica,
  type ReplicaOptions,
  type ReplicaStatus,
  type Stat,
} from './replica.js';
export { replicaStores } from './store.js';
export { decodeXattrs, encodeXattrs } from './xattr.js';
