// The queue of CRDT writes on the sealed local store (spec/zendb.md §9.3,
// §19.7): it survives restarts, and keeps the commit in flight so a
// restart sends it again with the same `commit_id`.
//
// Store `queue/<ns>`: key "e" ‖ id → an entry, key "inflight" → the commit
// in flight. Both are CBOR maps with integer keys:
//   entry     {1: id, 2: single, 3: [row], 4: at}
//   row       {1: table, 2: table id, 3: pk, 4: pk element, 5: [op]}
//   op        {1: kind, 2?: field, 3?: alive, 4?: hlc, 5?: value, 6?: delta, 7?: elem, 8?: dots}
//   inflight  {1: Commit (wire encoding), 2: [entry id]}
// Counter totals are never stored: the server's entry for the installation
// is the truth (§19.7).
import { bytes, type SealedStore, zw } from '@zen/client';
import type { Commit } from '@zen/wasm';
import { decode, encode, type Value } from './cbor.js';
import type { Pending } from './crdt.js';
import type { Inflight, QueueEntry, QueueRow, QueueStore } from './crdtq.js';
import { DbError } from './errors.js';

/** The stores of database `ns` in a local store (wiped together). */
export const localStores = (ns: string) => [`queue/${ns}`];

const INFLIGHT = bytes.utf8('inflight');
const entryKey = (id: string) => bytes.utf8(`e${id}`);

const corrupt = (why: string) => new DbError('corrupt', `a stored queue entry: ${why}`);

type M = Map<Value, Value>;

function get(m: M, k: number): Value | undefined {
  return m.get(k);
}

function asMap(v: Value | undefined): M {
  if (!(v instanceof Map)) throw corrupt('not a map');
  return v;
}

function text(v: Value | undefined): string {
  if (typeof v !== 'string') throw corrupt('not text');
  return v;
}

function bin(v: Value | undefined): Uint8Array {
  if (!(v instanceof Uint8Array)) throw corrupt('not bytes');
  return v;
}

function big(v: Value | undefined): bigint {
  if (typeof v === 'bigint') return v;
  if (typeof v === 'number' && Number.isInteger(v)) return BigInt(v);
  throw corrupt('not an integer');
}

function list(v: Value | undefined): Value[] {
  if (!Array.isArray(v)) throw corrupt('not an array');
  return v;
}

function encodeOp(o: Pending): M {
  const m: M = new Map<Value, Value>([[1, o.op]]);
  if (o.op !== 'row') m.set(2, o.field);
  if (o.op === 'row') m.set(3, o.alive);
  if ((o.op === 'row' || o.op === 'lww') && o.hlc !== undefined) m.set(4, o.hlc);
  if (o.op === 'lww' && o.value !== undefined) m.set(5, o.value);
  if (o.op === 'ctr') m.set(6, o.delta);
  if (o.op === 'add' || o.op === 'rem') m.set(7, o.elem);
  if (o.op === 'rem') m.set(8, o.dots);
  return m;
}

function decodeOp(v: Value): Pending {
  const m = asMap(v);
  const op = text(get(m, 1));
  const hlc = get(m, 4) === undefined ? {} : { hlc: big(get(m, 4)) };
  switch (op) {
    case 'row': {
      const alive = get(m, 3);
      if (typeof alive !== 'boolean') throw corrupt('alive');
      return { op, alive, ...hlc };
    }
    case 'lww':
      return { op, field: text(get(m, 2)), value: get(m, 5), ...hlc };
    case 'ctr':
      return { op, field: text(get(m, 2)), delta: big(get(m, 6)) };
    case 'add':
      return { op, field: text(get(m, 2)), elem: get(m, 7) ?? null };
    case 'rem':
      return {
        op,
        field: text(get(m, 2)),
        elem: get(m, 7) ?? null,
        dots: list(get(m, 8)).map(bin),
      };
    default:
      throw corrupt(`an operation ${op}`);
  }
}

/** @internal An entry as stored. */
export function encodeEntry(e: QueueEntry): Uint8Array {
  const rows = e.rows.map(
    (r) =>
      new Map<Value, Value>([
        [1, r.table],
        [2, r.tableId],
        [3, r.pk],
        [4, r.pkEl],
        [5, r.ops.map(encodeOp)],
      ]),
  );
  return encode(
    new Map<Value, Value>([
      [1, e.id],
      [2, e.single],
      [3, rows],
      [4, e.at],
    ]),
  );
}

/** @internal The entry `encodeEntry` stored. */
export function decodeEntry(b: Uint8Array): QueueEntry {
  const m = asMap(decode(b));
  const single = get(m, 2);
  const at = get(m, 4);
  if (typeof single !== 'boolean' || typeof at !== 'number') throw corrupt('single, at');
  const rows: QueueRow[] = list(get(m, 3)).map((rv) => {
    const r = asMap(rv);
    const pk = get(r, 3);
    if (pk === undefined) throw corrupt('pk');
    return {
      table: text(get(r, 1)),
      tableId: bin(get(r, 2)),
      pk,
      pkEl: bin(get(r, 4)),
      ops: list(get(r, 5)).map(decodeOp),
    };
  });
  return { id: text(get(m, 1)), single, rows, at };
}

/**
 * A `QueueStore` on a sealed local store (§19.7): the queue of database
 * `ns` survives restarts. Tabs that share the store share the queue; only
 * the owner sends it.
 */
export function localQueueStore(local: SealedStore, ns: string): QueueStore {
  const store = `queue/${ns}`;
  return {
    async load() {
      const out: QueueEntry[] = [];
      for await (const [k, v] of local.list(store)) {
        if (k[0] !== 0x65) continue; // "e"
        try {
          out.push(decodeEntry(v));
        } catch (e) {
          // A record that opens but doesn't decode: another version wrote it.
          console.warn(`zen-db: ${e instanceof Error ? e.message : String(e)}; dropped`);
          await local.delete(store, [k]);
        }
      }
      return out;
    },
    put: (e) => local.put(store, entryKey(e.id), encodeEntry(e)),
    remove: (ids) => local.delete(store, ids.map(entryKey)),
    putInflight: (r) =>
      local.put(
        store,
        INFLIGHT,
        encode(
          new Map<Value, Value>([
            [1, zw.encodeCommit(r.commit)],
            [2, r.ids],
          ]),
        ),
      ),
    async loadInflight() {
      const b = await local.get(store, INFLIGHT);
      if (!b) return undefined;
      try {
        const m = asMap(decode(b));
        const commit = zw.decodeCommit(bin(get(m, 1))) as Commit;
        return { commit, ids: list(get(m, 2)).map(text) } satisfies Inflight;
      } catch {
        await local.delete(store, [INFLIGHT]);
        return undefined;
      }
    },
    removeInflight: () => local.delete(store, [INFLIGHT]),
  };
}
