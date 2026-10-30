// The authenticated tier (spec/zendb.md §14): one state tree over every
// value under D, the NsRoot every transaction reads, and the checks every
// server answer goes through before a read returns it.
import {
  bytes,
  type Integrity,
  isCode,
  type ReadAnswer,
  Transaction,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import type { CheckpointEntry, IndexKeys } from '@zen/wasm';
import { decode, encode, type Value } from './cbor.js';
import { DbError } from './errors.js';
import { decodeNode, type Node, type RootRecord } from './index/node.js';
import {
  type Change,
  lookup,
  type NodeStore,
  rewrite,
  scan,
  type TreeKeys,
  treeKeys,
} from './index/prolly.js';
import type { Keys } from './keys.js';

const hex = bytes.hex;

/** The reserved index id of the state tree (§14.2). */
export const STATE_INDEX = new Uint8Array(16);
/** The state tree's fanout. */
const STATE_FANOUT = 64;
const ZERO32 = new Uint8Array(32);

/** `{1: v = 1, 2: seq, 3: prev, 4?: [root, height, count]}` (§14.4). */
export interface NsRoot {
  seq: bigint;
  prev: Uint8Array;
  tree?: RootRecord;
}

export function encodeNsRoot(r: NsRoot): Uint8Array {
  const m = new Map<Value, Value>([
    [1, 1],
    [2, r.seq],
    [3, r.prev],
  ]);
  if (r.tree) m.set(4, [r.tree.root, r.tree.height, r.tree.count]);
  return encode(m);
}

const corrupt = () => new DbError('corrupt', 'a malformed NsRoot');

/** Strict decode (§14.4): `v = 1`, `seq ≥ 1`, no unknown keys, deterministic. */
export function decodeNsRoot(b: Uint8Array): NsRoot {
  const m = decode(b);
  if (!(m instanceof Map) || m.get(1) !== 1) throw corrupt();
  for (const k of m.keys()) if (k !== 1 && k !== 2 && k !== 3 && k !== 4) throw corrupt();
  const seq = m.get(2);
  const prev = m.get(3);
  if (!(typeof seq === 'number' || typeof seq === 'bigint') || BigInt(seq) < 1n) throw corrupt();
  if (!(prev instanceof Uint8Array) || prev.length !== 32) throw corrupt();
  const r: NsRoot = { seq: BigInt(seq), prev };
  const t = m.get(4);
  if (t !== undefined) {
    if (!Array.isArray(t) || t.length !== 3) throw corrupt();
    const [root, height, count] = t;
    const nat = (x: Value | undefined) => typeof x === 'number' && Number.isInteger(x) && x >= 0;
    if (!(root instanceof Uint8Array) || root.length !== 32 || !nat(height) || !nat(count)) {
      throw corrupt();
    }
    r.tree = { root, height: height as number, count: count as number };
  }
  if (!bytes.equal(encodeNsRoot(r), b)) throw corrupt();
  return r;
}

/** The NsRoot a transaction read: absent (seq 0) or decoded, with its `NH`. */
interface RootRead {
  rec: NsRoot | undefined;
  seq: bigint;
  hash: Uint8Array;
}

const NO_ROOT: RootRead = { rec: undefined, seq: 0n, hash: ZERO32 };

function rootRead(b: Uint8Array | undefined): RootRead {
  if (!b) return NO_ROOT;
  const rec = decodeNsRoot(b);
  return { rec, seq: rec.seq, hash: zw.nsRootHash(b) };
}

/**
 * The authenticated tier of one `Db`: its keys, the state tree's keys and
 * which stored keys it covers.
 */
export class NsAuth {
  /** The stored key of `D`: the namespace's id in checkpoints (§14.5). */
  readonly ns: Uint8Array;
  readonly nsKey: Uint8Array;
  /** `entryKey` of the namespace's checkpoint entries. */
  readonly floorKey: string;
  readonly tk: TreeKeys;
  private readonly nodes: Uint8Array;
  private readonly end: Uint8Array;

  constructor(
    readonly fs: UnlockedFs,
    readonly integrity: Integrity,
    readonly keys: Keys,
    ik: IndexKeys,
  ) {
    this.ns = keys.db.prefix;
    this.nsKey = keys.nsRoot();
    this.floorKey = `3${hex(this.ns)}`;
    this.tk = treeKeys(ik, STATE_FANOUT, 32);
    this.nodes = keys.key('n');
    this.end = bytes.prefixEnd(this.ns)!;
  }

  /** Whether a stored key has a state-tree entry: under `D`, not a node, not the NsRoot. */
  covers(key: Uint8Array): boolean {
    const n = this.ns.length;
    if (key.length <= n || !bytes.equal(key.subarray(0, n), this.ns)) return false;
    if (bytes.equal(key, this.nsKey)) return false;
    const m = this.nodes.length;
    return !(key.length >= m && bytes.equal(key.subarray(0, m), this.nodes));
  }

  private suffix(key: Uint8Array): Uint8Array {
    return key.subarray(this.ns.length);
  }

  /** The prefix the entry of a key starts with: `esc(suffix) ‖ 0x00 0x00`. */
  prefix(key: Uint8Array): Uint8Array {
    return bytes.concat(esc(this.suffix(key)), new Uint8Array(2));
  }

  /** The entry of a value (§14.2). */
  entry(key: Uint8Array, plaintext: Uint8Array): Uint8Array {
    return zw.stateEntry(this.suffix(key), plaintext);
  }

  /**
   * The entry range of a stored-key range (§14.2), or undefined when the
   * range misses `D`. Bounds at or beyond `D`'s ends are no bounds.
   */
  entryRange(
    begin: Uint8Array,
    end: Uint8Array | undefined,
  ): { lo: Uint8Array | undefined; hi: Uint8Array | undefined } | undefined {
    const cmp = bytes.compare;
    if (cmp(begin, this.end) >= 0 || (end && cmp(end, this.ns) <= 0)) return undefined;
    const lo = cmp(begin, this.ns) <= 0 ? undefined : esc(this.suffix(begin));
    const hi = !end || cmp(end, this.end) >= 0 ? undefined : esc(this.suffix(end));
    return { lo, hi };
  }

  /** Read the latest NsRoot, outside any transaction's read set. */
  async latest(): Promise<RootRead> {
    const t = new Transaction(this.fs, 'short');
    const [b] = await t.snapshotGet([this.nsKey]);
    return rootRead(b?.value);
  }
}

/** `esc(x)`: each 0x00 replaced by 0x00 0xFF (§14.2). */
export function esc(x: Uint8Array): Uint8Array {
  let zeros = 0;
  for (const b of x) if (b === 0) zeros++;
  if (!zeros) return x.slice();
  const out = new Uint8Array(x.length + zeros);
  let j = 0;
  for (const b of x) {
    out[j++] = b;
    if (b === 0) out[j++] = 0xff;
  }
  return out;
}

/** What `TxAuth` needs of its transaction. */
export interface AuthTx {
  raw: Transaction;
  nodeStore(index: Uint8Array): NodeStore;
  cacheNode(id: Uint8Array, n: Node, b: Uint8Array): void;
}

/**
 * The authenticated tier in one attempt of a transaction (§14.5–14.7):
 * the NsRoot read once, every server answer checked against its state
 * tree, and the tree and NsRoot rewritten at `finish`.
 */
export class TxAuth {
  /** The floor when this attempt started (§14.7). */
  private readonly floorAt: CheckpointEntry | null;
  private rootP: Promise<RootRead> | undefined;
  private readonly store: NodeStore;

  constructor(
    readonly a: NsAuth,
    readonly tx: AuthTx,
  ) {
    this.floorAt = a.integrity.floor(a.floorKey) ?? null;
    const nodes = tx.nodeStore(STATE_INDEX);
    // A node of the NsRoot's tree that is gone: honest only if the NsRoot
    // moved since (§5.4.3); otherwise the server serves a root whose tree it
    // no longer has.
    this.store = {
      get: async (ids) => {
        try {
          return await nodes.get(ids);
        } catch (e) {
          if (!isCode(e, 'conflict') || !this.rootP) throw e;
          return this.mismatch(await this.rootP, 'a node of the state tree is missing');
        }
      },
    };
    tx.raw.onRead((r) => this.verify(r));
  }

  /** The NsRoot of this transaction, read once (in its read set) and checked (§14.7). */
  root(): Promise<RootRead> {
    this.rootP ??= this.readRoot();
    return this.rootP;
  }

  private async readRoot(): Promise<RootRead> {
    const a = this.a;
    const [e] = await this.tx.raw.getEntries([a.nsKey]);
    const r = rootRead(e?.value);
    const fl = this.floorAt;
    const cur = a.integrity.floor(a.floorKey);
    if (r.seq === 0n) {
      if (fl || cur) {
        throw a.integrity.raise({
          kind: 'rollback',
          object: 'db',
          id: a.ns,
          detail: 'no NsRoot, but checkpoints name the database',
        });
      }
      return r;
    }
    // The successor of the floor must name it (§14.7).
    for (const f of [fl, cur]) {
      if (f?.type === 'db' && f.seq + 1n === r.seq && !bytes.equal(f.hash, r.rec!.prev)) {
        throw a.integrity.raise({
          kind: 'fork',
          object: 'db',
          id: a.ns,
          detail: `NsRoot ${r.seq} doesn't follow the checkpointed ${f.seq}`,
        });
      }
    }
    a.integrity.observe({ type: 'db', ns: a.ns, seq: r.seq, hash: r.hash }, { floorAt: fl });
    return r;
  }

  /** Check one server answer against the state tree (§14.6). */
  private async verify(ans: ReadAnswer): Promise<void> {
    const a = this.a;
    if ('keys' in ans) {
      const idx = ans.keys.flatMap((k, i) => (a.covers(k) ? [i] : []));
      if (!idx.length) return;
      const r = await this.root();
      const got = await lookup(
        this.store,
        r.rec?.tree,
        idx.map((i) => a.prefix(ans.keys[i]!)),
      );
      let bad: string | undefined;
      idx.forEach((i, j) => {
        const v = ans.values[i];
        const want = v && a.entry(ans.keys[i]!, v);
        const have = got[j];
        if (want && have ? !bytes.equal(want, have) : want !== have) {
          bad = `a value ${v ? (have ? 'that changed' : 'not in the tree') : 'left out'}`;
        }
      });
      if (bad) await this.mismatch(r, bad);
      return;
    }
    const rg = a.entryRange(ans.begin, ans.end);
    if (!rg) return;
    const items = ans.items
      .filter((e) => a.covers(e.key))
      .map((e) => a.entry(e.key, e.value))
      .sort(bytes.compare);
    const r = await this.root();
    let i = 0;
    for await (const e of scan(this.store, r.rec?.tree, rg.lo, rg.hi)) {
      const it = items[i++];
      if (!it || !bytes.equal(it, e)) {
        const key = (x: Uint8Array) => x.subarray(0, x.length - 32);
        const why =
          !it || bytes.compare(e, it) < 0
            ? 'a range read with values left out'
            : bytes.equal(key(e), key(it))
              ? 'a range read with a value that changed'
              : 'a range read with values not in the tree';
        return this.mismatch(r, why);
      }
    }
    if (i < items.length) await this.mismatch(r, 'a range read with values not in the tree');
  }

  /**
   * An answer that doesn't match the tree (§14.6): a conflict if the NsRoot
   * moved since this transaction read it, a rollback if it didn't.
   */
  private async mismatch(r: RootRead, why: string): Promise<never> {
    const now = await this.a.latest();
    if (!bytes.equal(now.hash, r.hash)) {
      throw new ZenError(409, 'conflict', 'the database changed while it was read');
    }
    throw this.a.integrity.raise({ kind: 'rollback', object: 'db', id: this.a.ns, detail: why });
  }

  /**
   * Whether the tree's entry for `key` still has this VH (undefined: the
   * key is absent): a cached record may be used (§14.6).
   */
  async current(key: Uint8Array, vh: Uint8Array | undefined): Promise<boolean> {
    const r = await this.root();
    const [e] = await lookup(this.store, r.rec?.tree, [this.a.prefix(key)]);
    if (!e || !vh) return !e && !vh;
    return bytes.equal(e.subarray(e.length - 32), vh);
  }

  /**
   * Before the commit (§14.5): when it writes, deletes or clears values
   * under `D`, the state tree's changes, its nodes and the next NsRoot.
   * The device notes the new root once the commit applies.
   */
  async finish(): Promise<void> {
    const a = this.a;
    const raw = this.tx.raw;
    const clears = raw.clears.flatMap((c) => {
      const rg = a.entryRange(c.begin, c.end);
      return rg ? [rg] : [];
    });
    const writes = [...raw.writes.values()].filter((w) => a.covers(w.key));
    if (!clears.length && !writes.length) return;
    const r = await this.root();
    const tree = r.rec?.tree;
    const ch: Change[] = [];
    for (const rg of clears) {
      for await (const e of scan(this.store, tree, rg.lo, rg.hi)) ch.push({ key: e, add: false });
    }
    const old = await lookup(
      this.store,
      tree,
      writes.map((w) => a.prefix(w.key)),
    );
    writes.forEach((w, i) => {
      const o = old[i];
      if (o) ch.push({ key: o, add: false });
      if (w.value) ch.push({ key: a.entry(w.key, a.fs.kv.open(w.key, w.value)), add: true });
    });
    const rw = await rewrite(a.tk, this.store, tree, ch);
    for (const w of rw.put) raw.set(a.keys.node(STATE_INDEX, w.id), w.bytes);
    for (const id of rw.del) raw.delete(a.keys.node(STATE_INDEX, id));
    const next: NsRoot = { seq: r.seq + 1n, prev: r.hash, ...(rw.root ? { tree: rw.root } : {}) };
    const b = encodeNsRoot(next);
    raw.set(a.nsKey, b);
    const hash = zw.nsRootHash(b);
    raw.onCommit(() => {
      for (const w of rw.put) this.tx.cacheNode(w.id, w.node, w.bytes);
      a.integrity.note({ type: 'db', ns: a.ns, seq: next.seq, hash }, { count: true });
    });
  }
}

/** Check a fetched node against its 32-byte id (§14.3): `invalid` otherwise. */
export function checkNode(
  integrity: Integrity,
  ns: Uint8Array,
  nodeId32: (b: Uint8Array) => Uint8Array,
  id: Uint8Array,
  b: Uint8Array,
): Node {
  if (!bytes.equal(nodeId32(b), id)) {
    throw integrity.raise({
      kind: 'invalid',
      object: 'db',
      id: ns,
      detail: `a node that doesn't match its id ${hex(id)}`,
    });
  }
  return decodeNode(b, 32);
}
