// Private indexes in a transaction (spec/zendb.md §5.4, §5.5): changes are
// collected per shard and applied to the tree once, at commit; reads merge
// the shards' trees with the transaction's own pending changes.
import { bytes, ZenError } from '@zen/client';
import type { IndexDef } from '../catalog.js';
import type { Value } from '../cbor.js';
import { sortKey } from '../sortkey.js';
import type { DbTransaction } from '../txn.js';
import { decodeNode, encodeRoot, type Node, type RootRecord } from './node.js';
import { type Change, type NodeStore, rewrite, scan, treeKeys } from './prolly.js';
import type { IndexChange } from './types.js';

const hex = bytes.hex;

/** Default fanout of a private index (§3.2). */
export const DEFAULT_FANOUT = 64;

/** The sort key of an indexed value and pk (§5.1). */
export function sortKeyOf(ix: IndexDef, values: Value[], pk: Value[]): Uint8Array {
  return sortKey(
    ix.fields.map(([, type, desc], i) => ({ value: values[i] ?? null, type, desc })),
    pk,
  );
}

/** The fields' part of a sort key: every entry with these values starts with it. */
export const fieldPart = (ix: IndexDef, values: Value[]) => sortKeyOf(ix, values, []);

/** The shard of a row (§5.4.5). */
export function shardOf(tx: DbTransaction, ix: IndexDef, pkElement: Uint8Array): number {
  const k = ix.shards ?? 1;
  return k > 1 ? tx.db.indexKeys(ix.id).shard(pkElement, k) : 0;
}

/**
 * Nodes of one index, read at the transaction's snapshot without entering
 * its read set, through the database's node cache. A node that is gone
 * means the root changed since it was read: `conflict` re-runs the
 * transaction from a fresh root (§5.4.3).
 */
export class TxNodeStore implements NodeStore {
  constructor(
    readonly tx: DbTransaction,
    readonly index: Uint8Array,
  ) {}

  async get(ids: Uint8Array[]): Promise<Node[]> {
    const db = this.tx.db;
    const out: Node[] = new Array(ids.length);
    const miss: number[] = [];
    ids.forEach((id, i) => {
      const n = db.cachedNode(hex(id));
      if (n) out[i] = n;
      else miss.push(i);
    });
    if (!miss.length) return out;
    // Then the nodes the local store kept (§9.3), then the server.
    const left: number[] = [];
    for (const i of miss) {
      const n = await db.keptNode(this.index, ids[i]!);
      if (n) out[i] = n;
      else left.push(i);
    }
    if (!left.length) return out;
    const got = await this.tx.raw.snapshotGet(
      left.map((i) => this.tx.keys.node(this.index, ids[i]!)),
    );
    for (let j = 0; j < left.length; j++) {
      const e = got[j];
      if (!e) throw new ZenError(409, 'conflict', 'a private index changed while it was read');
      const n = decodeNode(e.value);
      db.cacheNode(hex(ids[left[j]!]!), n, e.value);
      out[left[j]!] = n;
    }
    return out;
  }
}

/** Record a row's change in a private index; applied at commit (§5.5). */
export function privateChange(tx: DbTransaction, c: IndexChange): void {
  const o = c.oldValues && sortKeyOf(c.index, c.oldValues, c.pkParts);
  const n = c.newValues && sortKeyOf(c.index, c.newValues, c.pkParts);
  if (o && n && bytes.equal(o, n)) return;
  const ops = tx.indexOps(c.index, shardOf(tx, c.index, c.pkElement));
  if (o) ops.set(hex(o), { key: o, add: false });
  if (n) ops.set(hex(n), { key: n, add: true });
}

/** Apply one shard's pending changes: new nodes, deleted nodes, the root (§5.4.4). */
export async function applyPrivate(
  tx: DbTransaction,
  ix: IndexDef,
  shard: number,
  ops: Map<string, Change>,
): Promise<void> {
  const root = await tx.indexRoot(ix, shard);
  const r = await rewrite(
    treeKeys(tx.db.indexKeys(ix.id), ix.fanout ?? DEFAULT_FANOUT),
    new TxNodeStore(tx, ix.id),
    root,
    [...ops.values()],
    { decoys: ix.decoys ?? 0 },
  );
  if (!r.changed) return;
  const raw = tx.raw;
  for (const w of r.put) raw.set(tx.keys.node(ix.id, w.id), w.bytes);
  for (const id of r.del) raw.delete(tx.keys.node(ix.id, id));
  const rk = tx.keys.root(ix.id, shard);
  if (r.root) raw.set(rk, encodeRoot(r.root));
  else raw.delete(rk);
  raw.onCommit(() => {
    for (const w of r.put) tx.db.cacheNode(hex(w.id), w.node, w.bytes);
  });
}

type Cmp = (a: Uint8Array, b: Uint8Array) => number;

/** Merge sorted key streams (in `cmp` order), dropping duplicates. */
export async function* mergeKeys(
  streams: AsyncGenerator<Uint8Array>[],
  cmp: Cmp,
): AsyncGenerator<Uint8Array> {
  const heads = await Promise.all(streams.map((s) => s.next()));
  for (;;) {
    let best = -1;
    for (let i = 0; i < heads.length; i++) {
      const h = heads[i]!;
      if (!h.done && (best < 0 || cmp(h.value, heads[best]!.value as Uint8Array) < 0)) best = i;
    }
    if (best < 0) return;
    const k = heads[best]!.value as Uint8Array;
    yield k;
    for (let i = 0; i < heads.length; i++) {
      const h = heads[i]!;
      if (!h.done && bytes.equal(h.value, k)) heads[i] = await streams[i]!.next();
    }
  }
}

async function* fromArray(keys: Uint8Array[]): AsyncGenerator<Uint8Array> {
  yield* keys;
}

async function* without(
  s: AsyncGenerator<Uint8Array>,
  removed: Set<string>,
): AsyncGenerator<Uint8Array> {
  for (;;) {
    const r = await s.next();
    if (r.done) return;
    if (!removed.has(hex(r.value))) yield r.value;
  }
}

const inRange = (k: Uint8Array, lo?: Uint8Array, hi?: Uint8Array) =>
  (!lo || bytes.compare(k, lo) >= 0) && (!hi || bytes.compare(k, hi) < 0);

/**
 * The sort keys of a private index in `[lo, hi)`, in order (descending with
 * `reverse`): every shard's tree, each root in the read set, merged with
 * this transaction's own pending changes.
 */
export async function* privateScan(
  tx: DbTransaction,
  ix: IndexDef,
  lo: Uint8Array | undefined,
  hi: Uint8Array | undefined,
  reverse: boolean,
): AsyncGenerator<Uint8Array> {
  const cmp: Cmp = reverse ? (a, b) => bytes.compare(b, a) : bytes.compare;
  const store = new TxNodeStore(tx, ix.id);
  const streams: AsyncGenerator<Uint8Array>[] = [];
  for (let s = 0; s < (ix.shards ?? 1); s++) {
    const root = await tx.indexRoot(ix, s);
    let tree = scan(store, root, lo, hi, reverse);
    const ops = tx.peekIndexOps(ix, s);
    if (ops?.size) {
      const removed = new Set<string>();
      const added: Uint8Array[] = [];
      for (const [h, c] of ops) {
        if (!c.add) removed.add(h);
        else if (inRange(c.key, lo, hi)) added.push(c.key);
      }
      tree = mergeKeys([without(tree, removed), fromArray(added.sort(cmp))], cmp);
    }
    streams.push(tree);
  }
  yield* streams.length === 1 ? streams[0]! : mergeKeys(streams, cmp);
}

/** The sort keys of a sealed index in `[lo, hi)`, in order (§5.7). */
export async function* sealedScan(
  tx: DbTransaction,
  ix: IndexDef,
  lo: Uint8Array | undefined,
  hi: Uint8Array | undefined,
  reverse: boolean,
): AsyncGenerator<Uint8Array> {
  const keys = (await tx.sealedState(ix)).keys.filter((k) => inRange(k, lo, hi));
  if (reverse) keys.reverse();
  yield* keys;
}

export type { RootRecord };
