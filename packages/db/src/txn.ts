// zen-db transactions (spec/zendb.md §4.4, §5.5, §7): rows and their index
// entries on top of an M4 `Transaction`.
import { bytes, type Transaction, ZenError } from '@zen/client';
import { encodeTableRecord, type IndexDef, type TableRecord } from './catalog.js';
import { encode, isPlainObject, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import { fastChange } from './index/fast.js';
import { uniqueChange } from './index/unique.js';
import type { Keys } from './keys.js';
import { Query } from './query.js';
import { decodeRow, type Fields, joinParts, layoutRow } from './row.js';
import { fitsType } from './sortkey.js';

/** A row as read back: its fields and how many parts it is stored in. */
interface ReadRow {
  fields: Fields;
  parts: number;
}

/** A zen-db transaction. Every method may run several times (§7.1). */
export class DbTransaction {
  private readonly tables = new Map<string, TableRecord>();
  /** Cached catalog records this transaction used, by stored key: their versions. */
  private readonly cached = new Map<
    string,
    { name: string; key: Uint8Array; version?: Uint8Array }
  >();

  constructor(
    readonly db: Db,
    /** The M4 transaction underneath: writes, appends, file operations. */
    readonly raw: Transaction,
  ) {
    raw.onError(async (e) => {
      // A stale cached catalog record fails its `expect`: read it afresh.
      if (e instanceof ZenError && e.code === 'conflict') {
        for (const name of this.tables.keys()) db.catalogStale(name);
      }
      return false;
    });
  }

  get keys(): Keys {
    return this.db.keys;
  }

  get mode(): 'short' | 'long' {
    return this.raw.mode;
  }

  /** A table in this transaction. */
  table<T extends Fields = Fields>(t: string | { name: string }): TableTx<T> {
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
    const key = this.keys.tableRecord(name);
    if (this.raw.writes.has(bytes.hex(key))) {
      const [e] = await this.raw.snapshotGet([key]);
      return e ? this.db.decodeTable(e.value) : undefined;
    }
    const cached = this.db.catalogEntry(name);
    let rec: TableRecord | undefined;
    if (cached) {
      this.raw.expectKey(key, cached.version);
      this.cached.set(bytes.hex(key), {
        name,
        key,
        ...(cached.version ? { version: cached.version } : {}),
      });
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
   * server never checks its `expect`s: check the cached catalog records it
   * used here, and re-run it on a stale one. Then fail early with
   * `too_large` if the commit would break the server's limits (§7.5).
   */
  async finish(): Promise<void> {
    if (this.raw.empty) {
      if (!this.cached.size) return;
      const used = [...this.cached.values()];
      const now = await this.raw.snapshotGet(used.map((u) => u.key));
      const stale = used.filter((u, i) => {
        const v = now[i]?.version;
        return !(v && u.version ? bytes.equal(v, u.version) : !v && !u.version);
      });
      if (!stale.length) return;
      for (const u of stale) this.db.catalogStale(u.name);
      throw new ZenError(409, 'conflict', 'a cached catalog record changed');
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

  /** Read a row by its pk element (into the read set), with its parts. @internal */
  async read(t: TableRecord, pkEl: Uint8Array): Promise<ReadRow | undefined> {
    const b = await this.tx.raw.get(this.keys.row(t.id, pkEl));
    return b ? this.decode(t, pkEl, b) : undefined;
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
  };
  switch (ix.kind) {
    case 'none':
      await uniqueChange(c);
      return;
    case 'fast':
      if (ix.unique) await uniqueChange(c);
      fastChange(c);
      return;
    default:
      throw new DbError('format', `index ${ix.name}: kind ${ix.kind} is not supported yet`);
  }
}
