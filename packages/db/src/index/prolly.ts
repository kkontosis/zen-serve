// The prolly tree of a private index (spec/zendb.md §5.4): incremental
// canonical writes, range scans, decoy rewrites. The code runs against a
// `NodeStore`, so it is tested in memory against zen-core's reference
// builder (`IndexKeys.build`) and runs on a transaction in the database.
import { bytes } from '@zen/client';
import type { IndexKeys } from '@zen/wasm';
import { DbError } from '../errors.js';
import {
  type Child,
  type Entry,
  encodeNode,
  entryCount,
  entryKey,
  entryLen,
  MAX_NODE_BYTES,
  MAX_NODE_ENTRIES,
  type Node,
  nodeLen,
  type RootRecord,
  sameEntry,
} from './node.js';

const hex = bytes.hex;
const cmp = bytes.compare;

/** Where nodes are read from. `get` throws when a node is missing. */
export interface NodeStore {
  get(ids: Uint8Array[]): Promise<Node[]>;
}

/** The keyed hashes a tree needs. */
export interface TreeKeys {
  /** For each key, whether it ends a node at `level` by hash. */
  boundaries(level: number, keys: Uint8Array[]): boolean[];
  /** `PRF16(K_node, node)`. */
  nodeId(node: Uint8Array): Uint8Array;
}

/** The `TreeKeys` of an index with this fanout. */
export function treeKeys(ik: IndexKeys, fanout: number): TreeKeys {
  return {
    boundaries: (level, keys) =>
      keys.length ? Array.from(ik.boundaries(level, keys, fanout), (x) => x === 1) : [],
    nodeId: (b) => ik.nodeId(b),
  };
}

/** A change of one level-0 entry: add or remove a sort key. */
export interface Change {
  key: Uint8Array;
  add: boolean;
}

/** Options of a rewrite. */
export interface RewriteOptions {
  /** Decoy rewrites per commit (§5.4.6); every node written is then salted and padded. */
  decoys?: number;
  /** Uniform in [0, 1), for choosing decoy leaves. */
  random?: () => number;
}

/** A node written by a rewrite. */
export interface Written {
  id: Uint8Array;
  node: Node;
  bytes: Uint8Array;
}

/** What a rewrite changes: the new root, the nodes to write and to delete. */
export interface Rewritten {
  root: RootRecord | undefined;
  put: Written[];
  del: Uint8Array[];
  /** False when the changes left the tree as it was. */
  changed: boolean;
}

/** A change at some level: remove the entry with `key`, or set `entry` there. */
interface LevelChange {
  key: Uint8Array;
  entry?: Entry;
}

interface Frame {
  id: Uint8Array;
  node: Node;
  /** Index of the entry the path goes through (unused at the bottom). */
  i: number;
}

interface Held {
  id: Uint8Array;
  node: Node;
}

const byKey = (a: { key: Uint8Array }, b: { key: Uint8Array }) => cmp(a.key, b.key);
const childOf = (e: Entry) => (e as Child).child;
const firstKey = (n: Node) => entryKey(n.entries[0]!);
const total = (n: Node) => n.entries.reduce((s, e) => s + entryCount(e), 0);
const childEntry = (w: Held): Child => ({
  key: firstKey(w.node),
  child: w.id,
  count: total(w.node),
});

/** Index of the last entry whose key is ≤ `key`, or 0. */
function route(n: Node, key: Uint8Array): number {
  let lo = 0;
  let hi = n.entries.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (cmp(entryKey(n.entries[mid]!), key) <= 0) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** The first key of the node after the path's bottom node, at its level. */
function peekNext(stack: Frame[]): Uint8Array | undefined {
  for (let d = stack.length - 2; d >= 0; d--) {
    const f = stack[d]!;
    if (f.i + 1 < f.node.entries.length) return entryKey(f.node.entries[f.i + 1]!);
  }
  return undefined;
}

class Rewrite {
  readonly fresh = new Map<string, Written>();
  readonly gone = new Map<string, Uint8Array>();

  constructor(
    readonly tk: TreeKeys,
    readonly store: NodeStore,
    readonly opts: RewriteOptions,
  ) {}

  async old(id: Uint8Array): Promise<Node> {
    return (await this.store.get([id]))[0]!;
  }

  async load(id: Uint8Array): Promise<Node> {
    return this.fresh.get(hex(id))?.node ?? this.old(id);
  }

  /** A new node; with decoys, salted and padded. */
  make(level: number, entries: Entry[]): Written {
    const node: Node = { level, entries };
    if (this.opts.decoys) {
      node.salt = bytes.randomBytes(16);
      node.pad = 0;
    }
    const b = encodeNode(node);
    const w = { id: this.tk.nodeId(b), node, bytes: b };
    this.fresh.set(hex(w.id), w);
    return w;
  }

  /** A node leaves the tree. */
  drop(id: Uint8Array): void {
    if (!this.fresh.delete(hex(id))) this.gone.set(hex(id), id);
  }

  async locate(root: RootRecord, level: number, key: Uint8Array): Promise<Frame[]> {
    const stack: Frame[] = [];
    let id = root.root;
    for (;;) {
      const node = await this.old(id);
      if (node.level === level) {
        stack.push({ id, node, i: 0 });
        return stack;
      }
      const i = route(node, key);
      stack.push({ id, node, i });
      id = childOf(node.entries[i]!);
    }
  }

  async advance(stack: Frame[], level: number): Promise<void> {
    let d = stack.length - 2;
    while (d >= 0 && stack[d]!.i + 1 >= stack[d]!.node.entries.length) d--;
    if (d < 0) throw new Error('no next node');
    stack.length = d + 1;
    const top = stack[d]!;
    top.i++;
    let id = childOf(top.node.entries[top.i]!);
    for (;;) {
      const node = await this.old(id);
      stack.push({ id, node, i: 0 });
      if (node.level === level) return;
      id = childOf(node.entries[0]!);
    }
  }

  /** Cut a level with no old nodes into new nodes (§5.4.1, step 2). */
  chunk(level: number, items: Entry[]): Written[] {
    const flags = this.tk.boundaries(level, items.map(entryKey));
    const out: Written[] = [];
    let cur: Entry[] = [];
    let len = 0;
    items.forEach((e, i) => {
      cur.push(e);
      len += entryLen(e);
      if (
        flags[i] ||
        cur.length >= MAX_NODE_ENTRIES ||
        nodeLen(level, len, cur.length) >= MAX_NODE_BYTES ||
        i === items.length - 1
      ) {
        out.push(this.make(level, cur));
        cur = [];
        len = 0;
      }
    });
    return out;
  }

  /**
   * Apply sorted changes to one level of the old tree. Each change goes to
   * the node the old tree routes its key to. From the start of that node,
   * the merged entries are re-chunked, pulling in the following old nodes,
   * until a new node closes where an old one did and no change lies before
   * the next old node: from there on the old chunking is unchanged.
   * Returns the changes for the level above and every node the regions made.
   */
  async level(
    root: RootRecord,
    level: number,
    ch: LevelChange[],
  ): Promise<{ up: LevelChange[]; produced: Held[] }> {
    const up = new Map<string, LevelChange>();
    const produced: Held[] = [];
    const adds = ch.flatMap((c, i) => (c.entry ? [i] : []));
    const af = this.tk.boundaries(
      level,
      adds.map((i) => ch[i]!.key),
    );
    const cflag: boolean[] = [];
    adds.forEach((ci, j) => {
      cflag[ci] = af[j]!;
    });

    let ci = 0;
    while (ci < ch.length) {
      const stack = await this.locate(root, level, ch[ci]!.key);
      const consumed: Held[] = [];
      const reused = new Set<number>();
      const made: Written[] = [];
      let buf: Entry[] = [];
      let bflag: boolean[] = [];
      let bi = 0;
      const consume = (f: Frame) => {
        consumed.push({ id: f.id, node: f.node });
        buf = f.node.entries;
        bflag = this.tk.boundaries(level, buf.map(entryKey));
        bi = 0;
      };
      let cur: Entry[] = [];
      let len = 0;
      const close = () => {
        const k = consumed.findIndex(
          (o, j) =>
            !reused.has(j) &&
            o.node.entries.length === cur.length &&
            o.node.entries.every((e, x) => sameEntry(e, cur[x]!)),
        );
        if (k >= 0) {
          reused.add(k);
          produced.push(consumed[k]!);
        } else {
          const w = this.make(level, cur);
          produced.push(w);
          made.push(w);
        }
        cur = [];
        len = 0;
      };
      const emit = (e: Entry, flag: boolean) => {
        cur.push(e);
        len += entryLen(e);
        if (
          flag ||
          cur.length >= MAX_NODE_ENTRIES ||
          nodeLen(level, len, cur.length) >= MAX_NODE_BYTES
        ) {
          close();
        }
      };
      const take = () => {
        const c = ch[ci]!;
        if (c.entry) emit(c.entry, cflag[ci]!);
        ci++;
      };

      consume(stack.at(-1)!);
      for (;;) {
        if (bi < buf.length) {
          const c = ch[ci];
          const o = c ? cmp(c.key, entryKey(buf[bi]!)) : 1;
          if (o === 0) bi++; // the change replaces or removes the old entry
          if (o <= 0) take();
          else {
            emit(buf[bi]!, bflag[bi]!);
            bi++;
          }
          continue;
        }
        const next = peekNext(stack);
        const c = ch[ci];
        if (c && (!next || cmp(c.key, next) < 0)) {
          take();
          continue;
        }
        if (!cur.length) break; // aligned with the old chunking
        if (next) {
          await this.advance(stack, level);
          consume(stack.at(-1)!);
          continue;
        }
        close(); // the last entry of the level
        break;
      }

      consumed.forEach((o, j) => {
        if (reused.has(j)) return;
        this.drop(o.id);
        const k = firstKey(o.node);
        up.set(hex(k), { key: k });
      });
      for (const w of made) {
        const e = childEntry(w);
        up.set(hex(e.key), { key: e.key, entry: e });
      }
    }
    return { up: [...up.values()].sort(byKey), produced };
  }

  /** Re-salt a random leaf and its ancestors (§5.4.6). */
  async decoy(root: RootRecord, random: () => number): Promise<RootRecord> {
    let r = Math.floor(random() * root.count);
    const path: Frame[] = [];
    let id = root.root;
    for (;;) {
      const node = await this.load(id);
      if (node.level === 0) {
        path.push({ id, node, i: 0 });
        break;
      }
      let i = 0;
      while (i < node.entries.length - 1 && r >= entryCount(node.entries[i]!)) {
        r -= entryCount(node.entries[i]!);
        i++;
      }
      path.push({ id, node, i });
      id = childOf(node.entries[i]!);
    }
    let below: Uint8Array | undefined;
    for (let d = path.length - 1; d >= 0; d--) {
      const { id: old, node, i } = path[d]!;
      const entries = below
        ? node.entries.map((e, j) => (j === i ? { ...(e as Child), child: below! } : e))
        : node.entries;
      this.drop(old);
      below = this.make(node.level, entries).id;
    }
    return { ...root, root: below! };
  }
}

/**
 * Apply level-0 changes to the tree under `root` (undefined: empty) and
 * return the new canonical tree's root, with the nodes to write and to
 * delete (§5.4.4). Adding a present key or removing an absent one is a
 * no-op. Several changes of one key: the last wins.
 */
export async function rewrite(
  tk: TreeKeys,
  store: NodeStore,
  root: RootRecord | undefined,
  changes: Change[],
  opts: RewriteOptions = {},
): Promise<Rewritten> {
  const rw = new Rewrite(tk, store, opts);
  const last = new Map<string, Change>();
  for (const c of changes) last.set(hex(c.key), c);
  let ch: LevelChange[] = [...last.values()]
    .sort(byKey)
    .map((c) => (c.add ? { key: c.key, entry: c.key } : { key: c.key }));
  const height = root?.height ?? 0;
  const unchanged: Rewritten = { root, put: [], del: [], changed: false };
  if (!height && !ch.some((c) => c.entry)) return unchanged;

  let top: Held | undefined;
  for (let level = 0; ; level++) {
    if (level >= 0xff) throw new DbError('too_large', 'a private index over 255 levels');
    let nodes: Held[];
    if (level < height) {
      if (!ch.length) return unchanged;
      const r = await rw.level(root!, level, ch);
      if (!r.up.length) return unchanged;
      if (level < height - 1) {
        ch = r.up;
        continue;
      }
      nodes = r.produced; // the old root's level: all of its nodes
    } else {
      const items = ch.flatMap((c) => (c.entry ? [c.entry] : []));
      nodes = items.length ? rw.chunk(level, items) : [];
    }
    if (nodes.length <= 1) {
      top = nodes[0];
      break;
    }
    ch = nodes.map((n) => {
      const e = childEntry(n);
      return { key: e.key, entry: e };
    });
  }

  // The root is the first level with one node: drop single-entry tops.
  while (top && top.node.level > 0 && top.node.entries.length === 1) {
    const id = childOf(top.node.entries[0]!);
    rw.drop(top.id);
    top = { id, node: await rw.load(id) };
  }
  let newRoot: RootRecord | undefined = top && {
    root: top.id,
    height: top.node.level + 1,
    count: total(top.node),
  };
  if (newRoot && opts.decoys) {
    for (let k = 0; k < opts.decoys; k++) {
      newRoot = await rw.decoy(newRoot, opts.random ?? Math.random);
    }
  }
  const put = [...rw.fresh.values()].filter((w) => !rw.gone.has(hex(w.id)));
  const del = [...rw.gone.entries()].filter(([h]) => !rw.fresh.has(h)).map(([, id]) => id);
  return { root: newRoot, put, del, changed: true };
}

/**
 * The level-0 keys of the tree in `[lo, hi)` (either bound may be absent),
 * ascending or, with `reverse`, descending. Nodes are fetched lazily, a few
 * siblings at a time.
 */
export async function* scan(
  store: NodeStore,
  root: RootRecord | undefined,
  lo: Uint8Array | undefined,
  hi: Uint8Array | undefined,
  reverse = false,
): AsyncGenerator<Uint8Array> {
  if (!root) return;
  const inRange = (k: Uint8Array) => (!lo || cmp(k, lo) >= 0) && (!hi || cmp(k, hi) < 0);
  async function* walk(node: Node): AsyncGenerator<Uint8Array> {
    const es = node.entries;
    if (node.level === 0) {
      for (let j = 0; j < es.length; j++) {
        const k = es[reverse ? es.length - 1 - j : j] as Uint8Array;
        if (inRange(k)) yield k;
      }
      return;
    }
    const idx: number[] = [];
    for (let i = 0; i < es.length; i++) {
      const startsBelowHi = !hi || cmp(entryKey(es[i]!), hi) < 0;
      const endsAboveLo = !lo || i + 1 === es.length || cmp(entryKey(es[i + 1]!), lo) > 0;
      if (startsBelowHi && endsAboveLo) idx.push(i);
    }
    if (reverse) idx.reverse();
    for (let b = 0; b < idx.length; b += 8) {
      const batch = idx.slice(b, b + 8);
      const kids = await store.get(batch.map((i) => childOf(es[i]!)));
      for (const k of kids) yield* walk(k);
    }
  }
  yield* walk((await store.get([root.root]))[0]!);
}
