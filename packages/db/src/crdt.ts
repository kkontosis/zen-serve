// CRDT tables (spec/zendb.md §19): rows the server merges field by field.
// The class keeps a transaction's operations pending, stamps them when it
// finishes (§19.3), and builds the merged view from `/v1/crdt/*` states.
import { bytes, type Session, type TreeClock, ZenError, zw } from '@zen/client';
import type { CrdtKeys, CrdtOp, ObjState } from '@zen/wasm';
import type { TableRecord } from './catalog.js';
import { decode, encode, isIntNumber, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import type { Fields } from './row.js';
import type { DbTransaction } from './txn.js';

/** A CRDT field's type (§19.1). Fields not declared are `lww`. */
export type CrdtFieldType = 'lww' | 'counter' | 'set';

/** The `/v1/info` feature a server lists when it merges CRDT rows (api.md §13). */
export const CRDT_FEATURE = 'crdt_rows';

/** Objects per `/v1/crdt/get` (api.md §13.3). */
const GET_MAX = 1000;

const ZERO16 = new Uint8Array(16);

/** Refuse CRDT tables on a server without them. */
export function checkCrdtServer(db: Db): void {
  if (!db.features.includes(CRDT_FEATURE)) {
    throw new DbError('unsupported', `the server doesn't merge CRDT rows ("${CRDT_FEATURE}")`);
  }
}

/** A CRDT table's field type. */
export const fieldType = (t: TableRecord, name: string): CrdtFieldType =>
  t.crdtFields?.[name] ?? 'lww';

// ------------------------------------------------------------ the merged view

/** A CRDT row as merged so far: the parts of the view, before it becomes a row. */
export interface Merged {
  /** The row register: whether the row exists, and its pk. */
  alive: boolean;
  pk?: Value;
  lww: Map<string, Value>;
  counters: Map<string, bigint>;
  /** Set fields: elements by their CBOR (hex). */
  sets: Map<string, Map<string, Value>>;
}

const asBig = (v: Value): bigint => {
  if (typeof v === 'bigint') return v;
  if (typeof v === 'number' && Number.isInteger(v)) return BigInt(v);
  throw new DbError('corrupt', 'a counter value that is not an integer');
};

const fromBig = (b: bigint): number | bigint =>
  b >= -(2n ** 53n) && b <= 2n ** 53n ? Number(b) : b;

/** A plaintext `{1: value, 2?: name}` (§19.2). */
function plain(b: Uint8Array): { value: Value; name?: string } {
  const m = decode(b);
  if (!(m instanceof Map) || !m.has(1)) throw new DbError('corrupt', 'a malformed CRDT value');
  const name = m.get(2);
  return { value: m.get(1)!, ...(typeof name === 'string' ? { name } : {}) };
}

const plaintext = (value: Value, name?: string): Uint8Array =>
  encode(
    new Map<Value, Value>([
      [1, value],
      ...(name !== undefined ? ([[2, name]] as [Value, Value][]) : []),
    ]),
  );

/** Field names by token (hex), for the declared fields of a table. */
function declared(keys: CrdtKeys, t: TableRecord): Map<string, string> {
  const m = new Map<string, string>();
  for (const name of Object.keys(t.crdtFields ?? {})) m.set(bytes.hex(keys.field(name)), name);
  return m;
}

/**
 * Open an ObjState into its merged parts (§19.4): `lww` fields as stored,
 * counters summed over their entries, sets as the elements with a dot.
 * A value that doesn't open is `corrupt`.
 */
export function mergeState(db: Db, t: TableRecord, s: ObjState): Merged {
  const keys = db.crdtKeys(t.id);
  const names = declared(keys, t);
  const open = (field: Uint8Array, elem: Uint8Array, v: Uint8Array) => {
    try {
      return plain(db.fs.keys.openCrdtValue(s.object, field, elem, v));
    } catch (e) {
      if (e instanceof DbError) throw e;
      throw new DbError('corrupt', 'a CRDT value that does not open');
    }
  };
  const m: Merged = { alive: false, lww: new Map(), counters: new Map(), sets: new Map() };
  if (s.row) {
    m.alive = s.row.alive;
    m.pk = open(ZERO16, ZERO16, s.row.value).value;
  }
  for (const r of s.lww ?? []) {
    if (!r.value) continue;
    const p = open(r.field, ZERO16, r.value);
    const name = names.get(bytes.hex(r.field)) ?? p.name;
    if (name !== undefined) m.lww.set(name, p.value);
  }
  for (const c of s.ctr ?? []) {
    const name = names.get(bytes.hex(c.field));
    if (name === undefined) continue;
    m.counters.set(
      name,
      (m.counters.get(name) ?? 0n) + asBig(open(c.field, ZERO16, c.value).value),
    );
  }
  for (const d of s.set ?? []) {
    const name = names.get(bytes.hex(d.field));
    if (name === undefined) continue;
    let set = m.sets.get(name);
    if (!set) {
      set = new Map();
      m.sets.set(name, set);
    }
    const v = open(d.field, d.elem, d.value).value;
    set.set(bytes.hex(encode(v)), v);
  }
  return m;
}

/** A Merged without any state (a row never written). */
export const emptyMerged = (): Merged => ({
  alive: false,
  lww: new Map(),
  counters: new Map(),
  sets: new Map(),
});

/**
 * The row of a merged state, or undefined when the row doesn't exist.
 * Declared counters are 0 and declared sets empty until written; sets are
 * sorted by their elements' CBOR.
 */
export function viewOf(t: TableRecord, m: Merged): Fields | undefined {
  if (!m.alive || m.pk === undefined) return undefined;
  const row: Fields = {};
  for (const [name, type] of Object.entries(t.crdtFields ?? {})) {
    if (type === 'counter') row[name] = 0;
    if (type === 'set') row[name] = [];
  }
  for (const [k, v] of m.lww) row[k] = v;
  for (const [k, v] of m.counters) row[k] = fromBig(v);
  for (const [k, s] of m.sets) {
    row[k] = [...s.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).map(([, v]) => v);
  }
  if (t.pk.length === 1) row[t.pk[0]!] = m.pk;
  else if (Array.isArray(m.pk)) {
    for (const [i, f] of t.pk.entries()) row[f] = m.pk[i]!;
  }
  return row;
}

// ------------------------------------------------------------ reads

/** `/v1/crdt/get`, in batches of 1,000 objects: their states, absent ones left out. */
export async function crdtGet(
  db: Db,
  objects: Uint8Array[],
  readVersion?: bigint,
): Promise<ObjState[]> {
  const out: ObjState[] = [];
  for (let i = 0; i < objects.length; i += GET_MAX) {
    const r = await db.fs.session.call('/v1/crdt/get', zw.encodeCrdtGet, zw.decodeObjStates, {
      fs: db.fs.id,
      objects: objects.slice(i, i + GET_MAX),
      ...(readVersion !== undefined ? { read_version: readVersion } : {}),
    });
    out.push(...r.objects);
  }
  return out;
}

/** One page of `/v1/crdt/range` over a table's objects, from `begin`. */
export async function crdtRange(
  db: Db,
  t: TableRecord,
  begin: Uint8Array | undefined,
  limit: number,
  readVersion?: bigint,
): Promise<{ objects: ObjState[]; more: boolean }> {
  const [b, end] = db.keys.rows(t.id);
  const r = await db.fs.session.call('/v1/crdt/range', zw.encodeCrdtRange, zw.decodeObjStates, {
    fs: db.fs.id,
    begin: begin ?? b,
    ...(end ? { end } : {}),
    limit,
    ...(readVersion !== undefined ? { read_version: readVersion } : {}),
  });
  return { objects: r.objects, more: !!r.more };
}

// ------------------------------------------------------------ writes

/** An operation of a transaction, before it is stamped. */
export type Pending =
  | { op: 'row'; alive: boolean; hlc?: bigint }
  | { op: 'lww'; field: string; value: Value | undefined; hlc?: bigint }
  | { op: 'ctr'; field: string; delta: bigint }
  | { op: 'add'; field: string; elem: Value }
  | { op: 'rem'; field: string; elem: Value; dots: Uint8Array[] };

/** The pending operations on one row. */
export interface RowOps {
  t: TableRecord;
  pk: Value;
  pkEl: Uint8Array;
  object: Uint8Array;
  ops: Pending[];
}

/**
 * The counter state of one installation (its actor): this actor's total of
 * each counter it wrote, and a lock that keeps its commits in `seq` order.
 * Shared by every `Db` on one session clock.
 */
interface ActorState {
  totals: Map<string, bigint>;
  tail: Promise<void>;
}

const actors = new WeakMap<TreeClock, ActorState>();

function actorState(clock: TreeClock): ActorState {
  let s = actors.get(clock);
  if (!s) {
    s = { totals: new Map(), tail: Promise.resolve() };
    actors.set(clock, s);
  }
  return s;
}

/** Take the actor's lock; resolves with the function that releases it. */
async function lock(s: ActorState): Promise<() => void> {
  let release!: () => void;
  const mine = new Promise<void>((r) => {
    release = r;
  });
  const prev = s.tail;
  s.tail = prev.then(() => mine);
  await prev;
  let done = false;
  return () => {
    if (!done) {
      done = true;
      release();
    }
  };
}

/**
 * The CRDT operations of one transaction (§19.3), kept in call order per
 * row and stamped by `stamp` when the transaction finishes, so a re-run
 * stamps afresh.
 */
export class CrdtWrites {
  private readonly rows = new Map<string, RowOps>();
  private undo: (() => void) | undefined;

  constructor(private readonly tx: DbTransaction) {}

  get size(): number {
    return this.rows.size;
  }

  private row(t: TableRecord, pk: Value, pkEl: Uint8Array): RowOps {
    const k = `${bytes.hex(t.id)}/${bytes.hex(pkEl)}`;
    let r = this.rows.get(k);
    if (!r) {
      r = { t, pk, pkEl, object: this.tx.keys.row(t.id, pkEl), ops: [] };
      this.rows.set(k, r);
    }
    return r;
  }

  /** The pending operations on a row (to overlay on reads). */
  pending(t: TableRecord, pkEl: Uint8Array): Pending[] {
    return this.rows.get(`${bytes.hex(t.id)}/${bytes.hex(pkEl)}`)?.ops ?? [];
  }

  /** The rows written, with their operations (to queue them, §19.7). */
  rowOps(): RowOps[] {
    return [...this.rows.values()];
  }

  /**
   * @internal Operations from the queue (§19.7), in order: counter deltas
   * of a field add up into one operation, as one commit takes one per
   * field; `row` and `lww` keep their `hlc`.
   */
  load(t: TableRecord, pk: Value, pkEl: Uint8Array, ops: Pending[]): void {
    const r = this.row(t, pk, pkEl);
    for (const o of ops) {
      if (o.op === 'ctr') {
        const c = r.ops.find((x) => x.op === 'ctr' && x.field === o.field);
        if (c && c.op === 'ctr') {
          c.delta += o.delta;
          continue;
        }
      }
      r.ops.push({ ...o });
    }
  }

  /** Insert (`alive`) or delete a row. */
  setRow(t: TableRecord, pk: Value, pkEl: Uint8Array, alive: boolean): void {
    this.row(t, pk, pkEl).ops.push({ op: 'row', alive });
  }

  /** Set (or with `undefined`, unset) an `lww` field. */
  lww(t: TableRecord, pk: Value, pkEl: Uint8Array, field: string, value: Value | undefined): void {
    if (value !== undefined) encode(value); // refuse what CBOR can't hold, now
    this.row(t, pk, pkEl).ops.push({ op: 'lww', field, value });
  }

  /** Add `n` to a counter; several in one transaction are one operation. */
  incr(t: TableRecord, pk: Value, pkEl: Uint8Array, field: string, n: bigint): void {
    const ops = this.row(t, pk, pkEl).ops;
    const o = ops.find((x) => x.op === 'ctr' && x.field === field);
    if (o && o.op === 'ctr') o.delta += n;
    else ops.push({ op: 'ctr', field, delta: n });
  }

  /** Add an element to a set. */
  add(t: TableRecord, pk: Value, pkEl: Uint8Array, field: string, elem: Value): void {
    encode(elem);
    this.row(t, pk, pkEl).ops.push({ op: 'add', field, elem });
  }

  /**
   * Remove an element: the dots observed now (§19.3), and this
   * transaction's own pending adds of it, which never get a dot.
   */
  remove(
    t: TableRecord,
    pk: Value,
    pkEl: Uint8Array,
    field: string,
    elem: Value,
    dots: Uint8Array[],
  ): void {
    const r = this.row(t, pk, pkEl);
    const e = bytes.hex(encode(elem));
    r.ops = r.ops.filter(
      (o) => !(o.op === 'add' && o.field === field && bytes.hex(encode(o.elem)) === e),
    );
    r.ops.push({ op: 'rem', field, elem, dots });
  }

  /**
   * Stamp the operations into the commit (§19.3): `row` and `lww` get a
   * fresh `hlc`; a counter gets `seq` = a fresh tick and this actor's new
   * total (its committed total plus this attempt's increments). Counter
   * commits of one actor are kept in order by a lock held until the commit
   * resolves; a failed one forgets the totals it touched, which are read
   * again from the server.
   */
  async stamp(): Promise<void> {
    if (!this.rows.size) return;
    const db = this.tx.db;
    checkCrdtServer(db);
    const session = db.fs.session;
    const clock = session.clock;
    await clock.sync(session.client);
    const raw = this.tx.raw;
    const fs = db.fs.id;
    const seal = (object: Uint8Array, field: Uint8Array, elem: Uint8Array, p: Uint8Array) => {
      const v = db.fs.keys.sealCrdtValue(object, field, elem, p);
      if (v.length > db.limits.max_value_bytes) {
        throw new DbError('too_large', `a CRDT value of ${v.length} bytes`);
      }
      return v;
    };

    // Counters: take the actor's lock, then the totals not known yet.
    const rows = [...this.rows.values()];
    const ctrs = rows.flatMap((r) =>
      r.ops.flatMap((o) => (o.op === 'ctr' ? [{ r, field: o.field }] : [])),
    );
    const state = actorState(clock);
    let release: (() => void) | undefined;
    const touched: string[] = [];
    const next = new Map<string, bigint>();
    if (ctrs.length) {
      release = await lock(state);
      try {
        await this.loadTotals(state, ctrs, clock.actor);
      } catch (e) {
        release();
        throw e;
      }
    }
    const done = () => release?.();
    this.undo = () => {
      for (const k of touched) state.totals.delete(k);
      done();
    };
    try {
      const hlcs: bigint[] = [];
      const ops: CrdtOp[] = [];
      for (const r of rows) {
        const keys = db.crdtKeys(r.t.id);
        const names = r.t.crdtFields ?? {};
        for (const o of r.ops) {
          const object = r.object;
          if (o.op === 'row') {
            const hlc = o.hlc ?? clock.tick();
            hlcs.push(hlc);
            const value = seal(object, ZERO16, ZERO16, plaintext(r.pk));
            ops.push({ op: 'row', fs, object, hlc, alive: o.alive, value });
            continue;
          }
          const field = keys.field(o.field);
          if (o.op === 'lww') {
            const hlc = o.hlc ?? clock.tick();
            hlcs.push(hlc);
            // An undeclared field carries its name: tokens can't be reversed (§19.2).
            const name = o.field in names ? undefined : o.field;
            ops.push({
              op: 'lww',
              fs,
              object,
              field,
              hlc,
              ...(o.value !== undefined
                ? { value: seal(object, field, ZERO16, plaintext(o.value, name)) }
                : {}),
            });
          } else if (o.op === 'ctr') {
            const k = `${bytes.hex(object)}/${bytes.hex(field)}`;
            const total = (state.totals.get(k) ?? 0n) + o.delta;
            touched.push(k);
            next.set(k, total);
            const value = seal(object, field, ZERO16, plaintext(fromBig(total)));
            ops.push({
              op: 'ctr',
              fs,
              object,
              field,
              actor: clock.actor,
              seq: clock.tick(),
              value,
            });
          } else {
            const elemCbor = encode(o.elem);
            const elem = keys.elem(o.field, r.pkEl, elemCbor);
            if (o.op === 'add') {
              const value = seal(object, field, elem, plaintext(o.elem));
              ops.push({ op: 'add', fs, object, field, elem, value });
            } else {
              // Also the dots of this queue's adds of it, committed since it was observed.
              const dots = new Map(o.dots.map((d) => [bytes.hex(d), d]));
              for (const d of db.crdtQueue.learnedDots(object, field, elem)) {
                dots.set(bytes.hex(d), d);
              }
              ops.push({ op: 'rem', fs, object, field, elem, dots: [...dots.values()] });
            }
          }
        }
      }
      raw.addCrdtOps(ops);
      const objects = rows.map((r) => r.object);
      raw.onCommit(() => {
        db.crdtCache.markStale(objects);
        for (const h of hlcs) clock.accept(h);
        for (const [k, v] of next) state.totals.set(k, v);
        this.undo = undefined;
        done();
      });
      raw.onError(async (e, attempt) => {
        // Whether it landed is unknown after some failures: read the totals
        // and the states again.
        db.crdtCache.markStale(objects);
        this.release();
        return hlcs.length ? rebase(session, clock, e, attempt) : false;
      });
    } catch (e) {
      this.release();
      throw e;
    }
  }

  /** Undo `stamp` when the commit is never sent (a failed limit check). */
  release(): void {
    this.undo?.();
    this.undo = undefined;
  }

  /** Read this actor's committed totals of counters it doesn't know yet. */
  private async loadTotals(
    state: ActorState,
    ctrs: { r: RowOps; field: string }[],
    actor: Uint8Array,
  ): Promise<void> {
    const db = this.tx.db;
    const objects = new Map<string, Uint8Array>();
    const missing = new Map<string, bigint>();
    for (const { r, field } of ctrs) {
      const tok = db.crdtKeys(r.t.id).field(field);
      const k = `${bytes.hex(r.object)}/${bytes.hex(tok)}`;
      if (state.totals.has(k)) continue;
      objects.set(bytes.hex(r.object), r.object);
      missing.set(k, 0n);
    }
    if (!missing.size) return;
    for (const s of await crdtGet(db, [...objects.values()])) {
      for (const c of s.ctr ?? []) {
        const k = `${bytes.hex(s.object)}/${bytes.hex(c.field)}`;
        if (!bytes.equal(c.actor, actor) || !missing.has(k)) continue;
        const total = plain(db.fs.keys.openCrdtValue(s.object, c.field, ZERO16, c.value)).value;
        missing.set(k, asBig(total));
      }
    }
    for (const [k, v] of missing) state.totals.set(k, v);
  }
}

/**
 * After a failed commit with stamped `row` or `lww` operations: whether a
 * re-run (which stamps afresh) can fix it (§19.7). `stale_op` (older than
 * the horizon, or a timestamp another session of this device took): read
 * the server clock again. `clock_skew`: also restart a clock that ran ahead.
 */
async function rebase(
  session: Session,
  clock: TreeClock,
  e: unknown,
  attempt: number,
): Promise<boolean> {
  if (attempt >= 3 || !(e instanceof ZenError)) return false;
  if (e.code !== 'stale_op' && e.code !== 'clock_skew') return false;
  await clock.sync(session.client, true);
  if (e.code === 'clock_skew') {
    const skew = Number((await session.client.info()).limits.crdt_max_skew_ms ?? 60_000n);
    if (zw.hlcMs(clock.last) > clock.now() + skew / 2) clock.reset(clock.now());
  }
  return true;
}

/** Overlay a transaction's pending operations on a row's merged state (its own writes). */
export function overlay(m: Merged, pk: Value, ops: Pending[]): Merged {
  if (!ops.length) return m;
  const out: Merged = {
    alive: m.alive,
    ...(m.pk !== undefined ? { pk: m.pk } : {}),
    lww: new Map(m.lww),
    counters: new Map(m.counters),
    sets: new Map([...m.sets].map(([k, s]) => [k, new Map(s)])),
  };
  for (const o of ops) {
    switch (o.op) {
      case 'row':
        out.alive = o.alive;
        out.pk = pk;
        break;
      case 'lww':
        if (o.value === undefined) out.lww.delete(o.field);
        else out.lww.set(o.field, o.value);
        break;
      case 'ctr':
        out.counters.set(o.field, (out.counters.get(o.field) ?? 0n) + o.delta);
        break;
      case 'add': {
        let s = out.sets.get(o.field);
        if (!s) {
          s = new Map();
          out.sets.set(o.field, s);
        }
        s.set(bytes.hex(encode(o.elem)), o.elem);
        break;
      }
      case 'rem':
        out.sets.get(o.field)?.delete(bytes.hex(encode(o.elem)));
        break;
    }
  }
  return out;
}

/** An integer increment as a bigint (`bad_value` otherwise). */
export function incrAmount(n: number | bigint): bigint {
  if (typeof n === 'bigint') return n;
  if (typeof n === 'number' && isIntNumber(n)) return BigInt(n);
  throw new DbError('bad_value', 'a counter increment is an integer');
}
