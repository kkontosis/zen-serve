// What a replica shows (spec/fs.md §11): the node table with the queued
// writes applied in order. Moves and meta changes rewrite copies of the
// nodes they touch; a write gives its node a local version, which replaces
// the versions the write replaces.
import { bytes, type ChunkIndex, type FileVersion, type Tree, type TreeNode } from '@zen/client';
import type { NodeState } from '@zen/wasm';
import type { QueueEntry, QWrite } from './queue.js';

/** A version written here and not yet sent (or sent, until the replica catches up). */
export interface LocalVersion extends FileVersion {
  local: true;
  /** The queue entry that writes it (hex key). */
  entry: string;
  /** The server versions it replaces. */
  replaces: Uint8Array[];
  index?: ChunkIndex;
}

/** A version as shown: from the server, or local. */
export type ShownVersion = FileVersion | LocalVersion;

export const isLocal = (v: ShownVersion | undefined): v is LocalVersion =>
  !!v && (v as LocalVersion).local === true;

/** A file's queued writes: the versions they replace, and the last one's local version. */
export interface OvFile {
  replaced: Set<string>;
  local: LocalVersion;
}

export interface Overlay {
  nodes: Map<string, TreeNode>;
  kids: Map<string, Set<string>>;
  files: Map<string, OvFile>;
}

export const emptyOverlay = (): Overlay => ({
  nodes: new Map(),
  kids: new Map(),
  files: new Map(),
});

/**
 * Apply `queue` (in order) to the node table `base`. `sent` holds the dots
 * entries already got from the server (they are hidden: the local version
 * shows the same content); `learned` the dots of earlier entries named by
 * a write's `after`.
 */
export function buildOverlay(
  tree: Tree,
  base: (key: string) => TreeNode | undefined,
  queue: QueueEntry[],
  sent: Map<string, Uint8Array>,
  learned: Map<string, Uint8Array>,
  local: (e: QueueEntry, op: QWrite) => LocalVersion,
): Overlay {
  const o = emptyOverlay();
  const cur = (k: string) => o.nodes.get(k) ?? base(k);
  for (const e of queue) {
    for (const op of e.ops) {
      const k = bytes.hex(op.node);
      if (op.op === 'write') {
        const f = o.files.get(k);
        const replaced = f?.replaced ?? new Set<string>();
        for (const d of op.replaces) replaced.add(bytes.hex(d));
        for (const a of op.after ?? []) {
          const d = learned.get(a) ?? sent.get(a);
          if (d) replaced.add(bytes.hex(d));
        }
        const d = sent.get(e.key);
        if (d) replaced.add(bytes.hex(d));
        o.files.set(k, { replaced, local: local(e, op) });
        continue;
      }
      const old = cur(k);
      const state: NodeState = old
        ? { ...old.state }
        : {
            node: op.node,
            move_hlc: 0n,
            move_device: new Uint8Array(0),
            versions: 0,
            changed: new Uint8Array(12),
          };
      if (op.op === 'move') {
        state.parent = op.parent;
        if (op.hlc !== undefined) state.move_hlc = op.hlc;
        if (op.meta) state.meta = op.meta;
      } else {
        state.meta = op.meta;
        if (op.hlc !== undefined) state.meta_hlc = op.hlc;
      }
      o.nodes.set(k, tree.nodeOf(state));
    }
  }
  // A written node counts its local version (an upper bound: what it
  // replaces is known once its server versions are read).
  for (const k of o.files.keys()) {
    const n = cur(k);
    if (n) o.nodes.set(k, { ...n, versions: n.versions + 1 });
  }
  for (const [k, n] of o.nodes) {
    if (!n.parent) continue;
    const p = bytes.hex(n.parent);
    const set = o.kids.get(p) ?? new Set<string>();
    o.kids.set(p, set);
    set.add(k);
  }
  return o;
}

/** Newest first: local versions, then by dot, descending. */
export function newestFirst(a: ShownVersion, b: ShownVersion): number {
  if (isLocal(a) !== isLocal(b)) return isLocal(a) ? -1 : 1;
  return bytes.compare(b.dot, a.dot);
}
