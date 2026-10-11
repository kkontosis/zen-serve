// Prolly-tree nodes and root records (spec/zendb.md §5.4.2, §5.4.6). The
// Rust reference is zen-core's `db::prolly`.
import { bytes } from '@zen/client';
import { decode, encode, headLen, type Value } from '../cbor.js';
import { DbError } from '../errors.js';

/** A node closes at this many entries at the latest. */
export const MAX_NODE_ENTRIES = 1024;
/** A node closes once its encoding (unsalted, unpadded) reaches this size. */
export const MAX_NODE_BYTES = 60_000;

/** An entry above level 0: the child's first key, its id, its level-0 count. */
export interface Child {
  key: Uint8Array;
  child: Uint8Array;
  count: number;
}

/** A level-0 entry (a sort key) or a child reference. */
export type Entry = Uint8Array | Child;

/** `key(e)`: the entry at level 0, `first_key` above. */
export const entryKey = (e: Entry): Uint8Array => (e instanceof Uint8Array ? e : e.key);

/** Level-0 entries under an entry. */
export const entryCount = (e: Entry): number => (e instanceof Uint8Array ? 1 : e.count);

/** Encoded length of an entry. */
export function entryLen(e: Entry): number {
  if (e instanceof Uint8Array) return headLen(e.length) + e.length;
  return 1 + headLen(e.key.length) + e.key.length + 17 + headLen(e.count);
}

/** Encoded length of an unsalted, unpadded node with `n` entries of `entriesLen` bytes. */
export const nodeLen = (level: number, entriesLen: number, n: number): number =>
  1 + 1 + headLen(level) + 1 + headLen(n) + entriesLen;

export function sameEntry(a: Entry, b: Entry): boolean {
  if (a instanceof Uint8Array || b instanceof Uint8Array) {
    return a instanceof Uint8Array && b instanceof Uint8Array && bytes.equal(a, b);
  }
  return a.count === b.count && bytes.equal(a.key, b.key) && bytes.equal(a.child, b.child);
}

/** A node: `{1: level, 2: entries, 3?: salt, 4?: pad}`. */
export interface Node {
  level: number;
  entries: Entry[];
  salt?: Uint8Array;
  pad?: number;
}

/** Decoy padding buckets (§5.4.6): 1, 2, 4 KiB, then multiples of 4 KiB. */
export function nodeBucket(len: number): number {
  if (len <= 1024) return 1024;
  if (len <= 2048) return 2048;
  return Math.ceil(len / 4096) * 4096;
}

function nodeMap(n: Node, pad: number | undefined): Map<Value, Value> {
  const m = new Map<Value, Value>([
    [1, n.level],
    [2, n.entries.map((e) => (e instanceof Uint8Array ? e : [e.key, e.child, e.count]))],
  ]);
  if (n.salt) m.set(3, n.salt);
  if (pad !== undefined) m.set(4, new Uint8Array(pad));
  return m;
}

/**
 * Deterministic CBOR of a node. With `pad` set (a decoy index), the pad is
 * the longest that keeps the node within its size bucket.
 */
export function encodeNode(n: Node): Uint8Array {
  if (n.pad === undefined) return encode(nodeMap(n, undefined));
  const base = encode(nodeMap(n, 0)).length - 1; // without the empty pad's head
  const room = nodeBucket(base + 1) - base;
  let p = Math.max(room - 1, 0);
  while (p > 0 && headLen(p) + p > room) p--;
  return encode(nodeMap(n, p));
}

const corrupt = () => new DbError('corrupt', 'a malformed index node');

function count(v: Value | undefined): number {
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0) throw corrupt();
  return v;
}

export function decodeNode(b: Uint8Array): Node {
  const m = decode(b);
  if (!(m instanceof Map)) throw corrupt();
  const level = count(m.get(1));
  const es = m.get(2);
  if (!Array.isArray(es) || !es.length) throw corrupt();
  const entries: Entry[] = es.map((e) => {
    if (level === 0) {
      if (!(e instanceof Uint8Array)) throw corrupt();
      return e;
    }
    if (!Array.isArray(e) || e.length !== 3) throw corrupt();
    const [key, child, c] = e;
    if (!(key instanceof Uint8Array) || !(child instanceof Uint8Array) || child.length !== 16) {
      throw corrupt();
    }
    return { key, child, count: count(c) };
  });
  const n: Node = { level, entries };
  const salt = m.get(3);
  if (salt !== undefined) {
    if (!(salt instanceof Uint8Array)) throw corrupt();
    n.salt = salt;
  }
  const pad = m.get(4);
  if (pad !== undefined) {
    if (!(pad instanceof Uint8Array)) throw corrupt();
    n.pad = pad.length;
  }
  return n;
}

/** `{1: root, 2: height, 3: count}`; height = the root's level + 1. */
export interface RootRecord {
  root: Uint8Array;
  height: number;
  count: number;
}

export function encodeRoot(r: RootRecord): Uint8Array {
  return encode(
    new Map<Value, Value>([
      [1, r.root],
      [2, r.height],
      [3, r.count],
    ]),
  );
}

export function decodeRoot(b: Uint8Array): RootRecord {
  const m = decode(b);
  if (!(m instanceof Map)) throw corrupt();
  const root = m.get(1);
  if (!(root instanceof Uint8Array) || root.length !== 16) throw corrupt();
  return { root, height: count(m.get(2)), count: count(m.get(3)) };
}
