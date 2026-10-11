// zen-db transactions (spec/zendb.md §4.4, §5.5, §7): rows and their index
// entries on top of an M4 `Transaction`.
import {
  bytes,
  commitTransaction,
  type Topic,
  type Transaction,
  type Tree,
  ZenError,
} from '@zen/client';
import type { CommitResult } from '@zen/wasm';
import { appendMsg, type Binding, checkType, type EmitOptions } from './broker.js';
import { encodeTableRecord, type IndexDef, type TableRecord } from './catalog.js';
import { encode, isPlainObject, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import { TxFiles } from './files.js';
import { fastChange } from './index/fast.js';
import type { RootRecord } from './index/node.js';
import { decodeRoot } from './index/node.js';
import { applyPrivate, fieldPart, privateChange, sortKeyOf } from './index/private.js';
import type { Change } from './index/prolly.js';
import { readSealed, SEALED_MAX_BYTES, SealedState, sealedChange } from './index/sealed.js';
import type { IndexTx } from './index/types.js';
import { uniqueChange } from './index/unique.js';
import type { Keys } from './keys.js';
import { Query } from './query.js';
import { decodeRow, type Fields, joinParts, layoutRow } from './row.js';
import { fitsType } from './sortkey.js';

/** A row as read back: its fields and how many parts it is stored in. */
export interface ReadRow {
  fields: Fields;
  parts: number;
}

/**
 * @internal Thrown in the short phase of an `auto` transaction once its
 * `autoSwitchMs` passed: the function re-runs in long mode (§7.1).
 */
export class SwitchToLong extends Error {
  override name = 'SwitchToLong';
}

/** A cached record (catalog or row) a transaction used: its version, and how to drop it. */
interface Used {
  key: Uint8Array;
  version: Uint8Array | undefined;
  drop: () => void;
}

/** A zen-db transaction. Every method may run several times (§7.1). */
export class DbTransaction implements IndexTx {
  private readonly tables = new Map<string, TableRecord>();
  /** Pending private-index changes, by index and shard (§5.5). */
  private readonly ops = new Map<
    string,
    { ix: IndexDef; shard: number; ops: Map<string, Change> }
  >();
  /** Private-index roots this transaction read (its snapshot of each tree). */
  private readonly roots = new Map<string, RootRecord | undefined>();
  private readonly sealedStates = new Map<string, SealedState>();
  /** Cached records this transaction used (catalog and rows), by stored key. */
  private readonly cached = new Map<string, Used>();
  /** @internal Nodes written by `fs(tree).write` (one write per node per commit). */
  readonly fileWrites = new Set<string>();
  /** @internal Rows of tables with change events: the first old state and the last (§12.7). */
  private readonly changed = new Map<
    string,
    { t: TableRecord; pk: Value; pkEl: Uint8Array; existed: boolean; row: Fields | undefined }
  >();
  /**
   * The id of the event this transaction handles (§10.2): every event it
   * appends carries it as `causation`. Set for consumer handlers.
   */
  causation: Uint8Array | undefined;

  constructor(
    readonly db: Db,
    /** The M4 transaction underneath: writes, appends, file operations. */
    readonly raw: Transaction,
    /** @internal In the short phase of `auto`: when to switch to long (ms since epoch). */
    readonly deadline?: number,
  ) {
    raw.onError(async (e) => {
      // A stale cached record fails its `expect`: read it afresh next time.
      if (e instanceof ZenError && e.code === 'conflict') {
        for (const name of this.tables.keys()) db.catalogStale(name);
        for (const u of this.cached.values()) u.drop();
      }
      return false;
    });
    // Rows this commit wrote have new versions: drop their cached copies.
    raw.onCommit(() => db.rowsWritten(raw));
  }

  /**
   * @internal Called before every read and at `finish`: the short phase of
   * `auto` switches to long once its time is up; a closed manual
   * transaction refuses.
   */
  live(): void {
    if (this.deadline !== undefined && Date.now() > this.deadline) throw new SwitchToLong();
  }

  /** @internal Use a cached record: its version joins the read set (§7.7). */
  useCached(key: Uint8Array, version: Uint8Array | undefined, drop: () => void): void {
    this.raw.expectKey(key, version);
    this.cached.set(bytes.hex(key), { key, version, drop });
  }

  /** @internal Keep a row read in this transaction in the row cache. */
  noteRow(key: Uint8Array, version: Uint8Array | undefined, r: ReadRow): void {
    if (!version || this.raw.owns(key)) return;
    this.db.cacheRow(bytes.hex(key), { version, fields: r.fields, parts: r.parts });
  }

  get keys(): Keys {
    return this.db.keys;
  }

  /**
   * Emit a message (§11.1): it is appended if and only if this transaction
   * commits. Returns its id.
   */
  emit(topic: Topic, type: string, body: Value, opts: EmitOptions = {}): Uint8Array {
    this.live();
    checkType(type);
    const id = opts.id ?? bytes.randomBytes(16);
    appendMsg(
      this,
      topic,
      { type, id, ...(opts.headers ? { headers: opts.headers } : {}), body },
      opts.key,
    );
    return id;
  }

  /**
   * Run `fn` after a successful commit (§12.6). It may never run, if the
   * process dies first: use it for UI updates only.
   */
  after(fn: () => void): void {
    this.raw.onCommit(() => {
      try {
        fn();
      } catch (e) {
        this.db.warn(`after: ${e instanceof Error ? e.message : String(e)}`);
      }
    });
  }

  /** @internal A row of a table with change events was written. */
  noteChange(
    t: TableRecord,
    pk: Value,
    pkEl: Uint8Array,
    old: Fields | undefined,
    row: Fields | undefined,
  ): void {
    const k = `${bytes.hex(t.id)}/${bytes.hex(pkEl)}`;
    const c = this.changed.get(k);
    if (c) c.row = row;
    else this.changed.set(k, { t, pk, pkEl, existed: old !== undefined, row });
  }

  /** Append one change event per changed row, with its final state (§12.7). */
  private emitChanges(): void {
    for (const c of this.changed.values()) {
      const def = c.t.changes;
      if (!def || (!c.existed && !c.row)) continue;
      const lean = { op: c.row ? 'put' : 'delete', table: c.t.name, pk: c.pk };
      const m = { type: 'zen.change', id: bytes.randomBytes(16) };
      const full = def.image === 'full' && c.row ? { ...lean, row: c.row } : undefined;
      appendMsg(
        this,
        this.db.topic(...def.topic),
        { ...m, body: full ?? lean },
        c.pkEl,
        full ? { ...m, body: lean } : undefined,
      );
    }
    this.changed.clear();
  }

  get mode(): 'short' | 'long' {
    return this.raw.mode;
  }

  /** File operations in this transaction's commit (§7.6). */
  fs(tree: Tree): TxFiles {
    this.live();
    return new TxFiles(this, tree);
  }

  get partSize(): number {
    return this.db.limits.max_value_bytes - 1024;
  }

  /** @internal The pending changes of one private-index shard. */
  indexOps(ix: IndexDef, shard: number): Map<string, Change> {
    const k = `${bytes.hex(ix.id)}/${shard}`;
    let p = this.ops.get(k);
    if (!p) {
      p = { ix, shard, ops: new Map() };
      this.ops.set(k, p);
    }
    return p.ops;
  }

  /** @internal */
  peekIndexOps(ix: IndexDef, shard: number): Map<string, Change> | undefined {
    return this.ops.get(`${bytes.hex(ix.id)}/${shard}`)?.ops;
  }

  /**
   * @internal The RootRecord of a private-index shard, in the read set
   * (§5.4.3). Read once: the transaction keeps one snapshot of each tree.
   */
  async indexRoot(ix: IndexDef, shard: number): Promise<RootRecord | undefined> {
    const key = this.keys.root(ix.id, shard);
    const h = bytes.hex(key);
    if (this.roots.has(h)) return this.roots.get(h);
    this.live();
    const b = await this.raw.get(key);
    const r = b ? decodeRoot(b) : undefined;
    this.roots.set(h, r);
    return r;
  }

  /** @internal A sealed index's entries in this transaction, its head in the read set (§5.7). */
  async sealedState(ix: IndexDef): Promise<SealedState> {
    const h = bytes.hex(ix.id);
    let s = this.sealedStates.get(h);
    if (!s) {
      this.live();
      const r = await readSealed(this, ix.id, (d) => this.db.cachedSealed(h, d));
      if (r.digest) this.db.cacheSealed(h, bytes.hex(r.digest), [...r.keys]);
      s = new SealedState(this, ix.id, ix.maxBytes ?? SEALED_MAX_BYTES, r.keys, r.parts);
      this.sealedStates.set(h, s);
    }
    return s;
  }

  /** A table in this transaction. */
  table<T extends Fields = Fields>(t: string | { name: string }): TableTx<T> {
    this.live();
    return new TableTx<T>(this, typeof t === 'string' ? t : t.name);
  }

  /**
   * The TableRecord of a table, in the read set (§7.4): a cached record by
   * its version, otherwise read afresh. Throws `not_found` for no table.
   */
  async tableRecord(name: string): Promise<TableRecord> {
    const t = await this.tableRecordOrNone(name);
    if (t?.state !== 'active') throw new DbError('not_found', `no table ${name}`);
    return t;
  }

  /** Like `tableRecord`, but undefined for no table (and dropping tables too). */
  async tableRecordOrNone(name: string): Promise<TableRecord | undefined> {
    const mine = this.tables.get(name);
    if (mine) return mine;
    this.live();
    const key = this.keys.tableRecord(name);
    if (this.raw.writes.has(bytes.hex(key))) {
      const [e] = await this.raw.snapshotGet([key]);
      return e ? this.db.decodeTable(e.value) : undefined;
    }
    const cached = this.db.catalogEntry(name);
    let rec: TableRecord | undefined;
    if (cached) {
      this.useCached(key, cached.version, () => this.db.catalogStale(name));
      rec = cached.record;
    } else {
      const [e] = await this.raw.snapshotGet([key]);
      this.raw.expectKey(key, e?.version);
      rec = e ? this.db.decodeTable(e.value) : undefined;
      this.db.catalogSet(name, rec, e?.version);
    }
    if (rec) this.tables.set(name, rec);
    return rec;
  }

  /** Write a TableRecord (migrations). */
  putTableRecord(t: TableRecord): void {
    this.raw.set(this.keys.tableRecord(t.name), encodeTableRecord(t));
    this.tables.set(t.name, t);
    this.raw.onCommit(() => this.db.catalogStale(t.name));
  }

  /** Delete a TableRecord (migrations). */
  deleteTableRecord(name: string): void {
    this.raw.delete(this.keys.tableRecord(name));
    this.tables.delete(name);
    this.raw.onCommit(() => this.db.catalogStale(name));
  }

  /**
   * Before committing. A read-only transaction commits nothing, so the
   * server never checks its `expect`s: check the cached records (catalog
   * and rows) it used here, with one read, and re-run it on a stale one. Then fail early with
   * `too_large` if the commit would break the server's limits (§7.5).
   */
  async finish(): Promise<void> {
    this.live();
    this.emitChanges();
    for (const p of this.ops.values()) {
      if (p.ops.size) await applyPrivate(this, p.ix, p.shard, p.ops);
    }
    for (const [h, s] of this.sealedStates) {
      if (!s.dirty) continue;
      const digest = s.write();
      const keys = [...s.keys];
      this.raw.onCommit(() => this.db.cacheSealed(h, bytes.hex(digest), keys));
    }
    if (this.raw.empty) {
      if (!this.cached.size) return;
      const used = [...this.cached.values()];
      const now = await this.raw.snapshotGet(used.map((u) => u.key));
      const stale = used.filter((u, i) => {
        const v = now[i]?.version;
        return !(v && u.version ? bytes.equal(v, u.version) : !v && !u.version);
      });
      if (!stale.length) return;
      for (const u of stale) u.drop();
      throw new ZenError(409, 'conflict', 'a cached record changed');
    }
    this.checkLimits();
  }

  /** Fail early with `too_large` if the commit would break the server's limits (§7.5). */
  checkLimits(): void {
    const l = this.db.limits;
    const c = this.raw.build();
    const ops =
      (c.chunks?.length ?? 0) +
      (c.crdt_ops?.length ?? 0) +
      (c.read_conflicts?.length ?? 0) +
      (c.expect?.length ?? 0) +
      (c.expect_ranges?.length ?? 0) +
      (c.writes?.length ?? 0) +
      (c.clear_ranges?.length ?? 0) +
      (c.append?.length ?? 0) +
      (c.consume?.length ?? 0);
    if (ops > l.max_commit_ops) {
      throw new DbError('too_large', `${ops} operations; the server takes ${l.max_commit_ops}`);
    }
    let size = ops * 512;
    for (const w of c.writes ?? []) size += w.key.length + (w.value?.length ?? 0);
    for (const a of c.append ?? []) size += a.envelope.length;
    for (const ch of c.chunks ?? []) size += ch.data.length;
    if (size > l.max_commit_bytes) {
      throw new DbError('too_large', `about ${size} bytes; the server takes ${l.max_commit_bytes}`);
    }
  }
}

/** The pk value of a row: its one pk field, or the array of them (§4.1). */
export function pkOf(t: TableRecord, row: Fields): Value {
  const parts = t.pk.map((f) => {
    const v = row[f];
    if (v === undefined || v === null) {
      throw new DbError('bad_value', `the row has no ${f} (table ${t.name}'s primary key)`);
    }
    return v;
  });
  return parts.length === 1 ? parts[0]! : parts;
}

/** The indexed value of a row (§5.1): one value per field, null when missing. */
export function indexedValue(ix: IndexDef, row: Fields): Value[] {
  return ix.fields.map(([name]) => row[name] ?? null);
}

/** Refuse an indexed value of the wrong type (§5.1): `bad_type`. */
export function checkTypes(ix: IndexDef, values: Value[]): void {
  ix.fields.forEach(([name, type], i) => {
    if (!fitsType(values[i], type)) {
      throw new DbError('bad_type', `field ${name} of index ${ix.name} must be ${type}`);
    }
  });
}

/** The element of an indexed value in unique and fast entries: absent with a null. */
export function entryElement(values: Value[] | undefined): Uint8Array | undefined {
  return values && !values.includes(null) ? encode(values) : undefined;
}

/** Indexes writers maintain (§3.2). */
export const maintained = (t: TableRecord) => t.indexes.filter((i) => i.state !== 'dropping');

/** One table inside a transaction. */
export class TableTx<T extends Fields = Fields> {
  constructor(
    readonly tx: DbTransaction,
    readonly name: string,
  ) {}

  private get keys(): Keys {
    return this.tx.keys;
  }

  /** The row with this primary key, or undefined. */
  async get(pk: Value): Promise<T | undefined> {
    const t = await this.tx.tableRecord(this.name);
    return (await this.read(t, encode(pk)))?.fields as T | undefined;
  }

  /** Insert a row; `exists` if its primary key is taken. */
  async insert(row: T): Promise<void> {
    const t = await this.tx.tableRecord(this.name);
    const pk = pkOf(t, row);
    const pkEl = encode(pk);
    const old = await this.read(t, pkEl);
    if (old) throw new DbError('exists', `table ${t.name} already has this row`, { pk });
    await this.write(t, pk, pkEl, undefined, row);
  }

  /** Insert or replace a row. */
  async put(row: T): Promise<void> {
    const t = await this.tx.tableRecord(this.name);
    const pk = pkOf(t, row);
    const pkEl = encode(pk);
    const old = await this.read(t, pkEl);
    await this.write(t, pk, pkEl, old, row);
  }

  /** Change a row with `fn`; `not_found` if there is none. Returns the new row. */
  async update(pk: Value, fn: (row: T) => T | Promise<T>): Promise<T> {
    const t = await this.tx.tableRecord(this.name);
    const pkEl = encode(pk);
    const old = await this.read(t, pkEl);
    if (!old) throw new DbError('not_found', `table ${t.name} has no such row`, { pk });
    const row = await fn(structuredClone(old.fields) as T);
    if (!bytes.equal(encode(pkOf(t, row)), pkEl)) {
      throw new DbError('bad_value', 'update must not change the primary key');
    }
    await this.write(t, pk, pkEl, old, row);
    return row;
  }

  /** Delete a row. Returns whether there was one. */
  async delete(pk: Value): Promise<boolean> {
    const t = await this.tx.tableRecord(this.name);
    const pkEl = encode(pk);
    const old = await this.read(t, pkEl);
    if (!old) return false;
    await this.write(t, pk, pkEl, old, undefined);
    return true;
  }

  /** A query on this table, in this transaction (§6). */
  query(): Query<T> {
    return new Query<T>(this.name, (run) => run(this.tx));
  }

  // ------------------------------------------------------------ internals

  /**
   * Read a row by its pk element (into the read set), with its parts: a
   * cached copy by its version (§7.7), otherwise from the server. @internal
   */
  async read(t: TableRecord, pkEl: Uint8Array): Promise<ReadRow | undefined> {
    const tx = this.tx;
    tx.live();
    const key = this.keys.row(t.id, pkEl);
    if (!tx.raw.owns(key)) {
      const h = bytes.hex(key);
      const c = tx.db.cachedRow(h);
      if (c) {
        tx.useCached(key, c.version, () => tx.db.dropRow(h));
        return { fields: c.fields, parts: c.parts };
      }
    }
    const [e] = await tx.raw.getEntries([key]);
    if (!e) return undefined;
    const r = await this.decode(t, pkEl, e.value);
    tx.noteRow(key, e.version, r);
    return r;
  }

  /** Decode a stored Row, fetching its parts. @internal */
  async decode(t: TableRecord, pkEl: Uint8Array, b: Uint8Array): Promise<ReadRow> {
    const r = decodeRow(b);
    if (!r.parts) return { fields: r.fields, parts: 0 };
    const keys = Array.from({ length: r.parts }, (_, i) => this.keys.part(t.id, pkEl, i));
    const got = await this.tx.raw.snapshotGet(keys);
    if (got.some((e) => !e)) throw await this.torn(t, pkEl, b);
    try {
      const f = joinParts(
        got.map((e) => e!.value),
        r.digest!,
      );
      if (!isPlainObject(f)) throw new DbError('corrupt', 'row parts that are not a map');
      return { fields: f as Fields, parts: r.parts };
    } catch (e) {
      if (e instanceof DbError && e.code === 'corrupt') throw await this.torn(t, pkEl, b, e);
      throw e;
    }
  }

  // Parts that don't match their Row: in long mode the row may have changed
  // between reads (retry); otherwise the row is corrupt.
  private async torn(t: TableRecord, pkEl: Uint8Array, row: Uint8Array, e?: DbError) {
    if (this.tx.mode === 'long') {
      const [now] = await this.tx.raw.snapshotGet([this.keys.row(t.id, pkEl)]);
      if (!now || !bytes.equal(now.value, row)) {
        return new ZenError(409, 'conflict', 'a row changed while its parts were read');
      }
    }
    return e ?? new DbError('corrupt', 'a row with missing parts');
  }

  /** Write (or delete, with no `row`) a row, its parts and its index entries. @internal */
  async write(
    t: TableRecord,
    pk: Value,
    pkEl: Uint8Array,
    old: ReadRow | undefined,
    row: T | Fields | undefined,
  ): Promise<void> {
    const raw = this.tx.raw;
    let parts = 0;
    if (row) {
      if (!isPlainObject(row)) throw new DbError('bad_value', 'a row must be a plain object');
      const s = layoutRow(pk, row, t.pad, this.tx.db.limits.max_value_bytes);
      raw.set(this.keys.row(t.id, pkEl), s.row);
      for (const [i, p] of s.parts.entries()) raw.set(this.keys.part(t.id, pkEl, i), p);
      parts = s.parts.length;
    } else {
      raw.delete(this.keys.row(t.id, pkEl));
    }
    for (let i = parts; i < (old?.parts ?? 0); i++) raw.delete(this.keys.part(t.id, pkEl, i));
    if (t.changes) this.tx.noteChange(t, pk, pkEl, old?.fields, row);
    for (const ix of maintained(t)) {
      const neu = row ? indexedValue(ix, row) : undefined;
      if (neu) checkTypes(ix, neu);
      const oldV = old ? indexedValue(ix, old.fields) : undefined;
      await changeIndex(this.tx, t, ix, pk, pkEl, oldV, neu);
    }
  }
}

/** Apply one row's change to one index (§5.5). */
export async function changeIndex(
  tx: DbTransaction,
  t: TableRecord,
  ix: IndexDef,
  pk: Value,
  pkEl: Uint8Array,
  oldV: Value[] | undefined,
  newV: Value[] | undefined,
): Promise<void> {
  const c = {
    tx,
    table: t,
    index: ix,
    pk,
    pkElement: pkEl,
    old: entryElement(oldV),
    new: entryElement(newV),
    oldValues: oldV,
    newValues: newV,
    pkParts: t.pk.length === 1 ? [pk] : (pk as Value[]),
  };
  switch (ix.kind) {
    case 'none':
      await uniqueChange(c);
      return;
    case 'fast':
      if (ix.unique) await uniqueChange(c);
      fastChange(c);
      return;
    case 'private':
      if (ix.unique) await uniqueChange(c);
      privateChange(tx, c);
      return;
    case 'sealed': {
      const o = oldV && sortKeyOf(ix, oldV, c.pkParts);
      const n = newV && sortKeyOf(ix, newV, c.pkParts);
      const check = ix.unique && newV && !newV.includes(null) ? fieldPart(ix, newV) : undefined;
      sealedChange(await tx.sealedState(ix), c, o, n, check);
      return;
    }
    default:
      throw new DbError('format', `index ${ix.name}: kind ${ix.kind} is not supported yet`);
  }
}

/**
 * A transaction the app commits or aborts itself (§7.2): no automatic
 * retry, so `conflict` reaches the caller. Get one with `db.begin()`.
 */
export class ManualTransaction extends DbTransaction {
  private closed = false;

  /** @internal Bind to a delivery (§11.3): the commit carries its consume step. */
  consumes(b: Binding, cause: Uint8Array): void {
    this.causation = cause;
    void b.consumer.ack(b.delivery, this.raw);
    this.raw.onCommit(() => {
      b.acked = true;
    });
  }

  override live(): void {
    if (this.closed) throw new DbError('closed', 'the transaction was committed or aborted');
  }

  /** Commit; undefined if there was nothing to send. Afterwards the transaction is closed. */
  async commit(): Promise<CommitResult | undefined> {
    this.live();
    try {
      await this.finish();
      return await commitTransaction(this.raw);
    } catch (e) {
      for (const h of this.raw.errorHooks) await h(e, 1);
      throw e;
    } finally {
      this.closed = true;
    }
  }

  /** Drop the transaction: nothing is sent. */
  abort(): void {
    this.closed = true;
  }
}
