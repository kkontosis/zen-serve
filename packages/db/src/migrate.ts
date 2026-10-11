// Migrations (spec/zendb.md §8). Every operation of a step runs in pages;
// each page is a transaction that reads and advances the step's
// MigrationRecord, so a killed step resumes where it stopped and duplicate
// runners conflict instead of repeating work.
import { bytes } from '@zen/client';
import {
  decodeDbRecord,
  decodeMigrationRecord,
  encodeDbRecord,
  encodeMigrationRecord,
  type IndexDef,
  type IndexKind,
  type MigrationRecord,
  type TableRecord,
} from './catalog.js';
import { decode, encode, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import { decodeRow, type Fields } from './row.js';
import type { FieldType } from './sortkey.js';
import { changeIndex, checkTypes, type DbTransaction, indexedValue } from './txn.js';

/** One step's migration function (§8.1). */
export type MigrationFn = (m: Migrator) => Promise<void>;

/** `createTable` options. */
export interface CreateTableOptions {
  /** Primary-key field names, at least one. */
  pk: string[];
  /** Pad rows to size buckets (§4.3). */
  pad?: boolean;
  /** A change topic (§12.7); its appends come with the broker. */
  changes?: { topic?: (string | Uint8Array)[]; image?: 'keys' | 'full' };
}

/** An indexed field: name, type, and optionally `'desc'`. */
export type FieldSpec =
  | [name: string, type: FieldType]
  | [name: string, type: FieldType, dir: 'asc' | 'desc'];

/** `createIndex` options (§3.2). */
export interface CreateIndexOptions {
  fields: FieldSpec[];
  /** Default `private`. */
  kind?: IndexKind;
  unique?: boolean;
  fanout?: number;
  shards?: number;
  decoys?: number;
  maxBytes?: number;
}

/** Index kinds this version builds. */
const SUPPORTED: IndexKind[] = ['none', 'fast'];

interface Progress {
  op: number;
  key?: Uint8Array;
}

function encodeProgress(p: Progress): Uint8Array {
  const m = new Map<Value, Value>([[1, p.op]]);
  if (p.key) m.set(2, p.key);
  return encode(m);
}

function decodeProgress(b: Uint8Array | undefined): Progress {
  if (!b) return { op: 0 };
  const m = decode(b);
  if (!(m instanceof Map) || typeof m.get(1) !== 'number') {
    throw new DbError('corrupt', 'a malformed migration progress');
  }
  const key = m.get(2);
  return { op: m.get(1) as number, ...(key instanceof Uint8Array ? { key } : {}) };
}

const utf8 = (s: string | Uint8Array) => (typeof s === 'string' ? bytes.utf8(s) : s);

/** The operations a migration function uses (§8.1). */
export class Migrator {
  private op = 0;

  constructor(
    readonly db: Db,
    readonly step: number,
  ) {}

  /** Create a table (no-op if it exists). */
  createTable(name: string, opts: CreateTableOptions): Promise<void> {
    if (!opts.pk.length) throw new DbError('bad_value', 'a table needs a primary key');
    return this.run(async (tx) => {
      if (await tx.tableRecordOrNone(name)) return true;
      const t: TableRecord = {
        name,
        id: bytes.randomBytes(16),
        pk: opts.pk,
        indexes: [],
        pad: opts.pad ?? false,
        state: 'active',
      };
      if (opts.changes) {
        t.changes = {
          topic: (opts.changes.topic ?? ['zen', 'db', this.db.ns, 'changes', name]).map(utf8),
          image: opts.changes.image ?? 'keys',
        };
      }
      tx.putTableRecord(t);
      return true;
    });
  }

  /** Rename a table (no-op if it already has the new name). */
  renameTable(from: string, to: string): Promise<void> {
    return this.run(async (tx) => {
      const t = await tx.tableRecordOrNone(from);
      if (!t) {
        if (await tx.tableRecordOrNone(to)) return true;
        throw new DbError('not_found', `no table ${from}`);
      }
      if (await tx.tableRecordOrNone(to)) throw new DbError('exists', `table ${to} exists`);
      tx.deleteTableRecord(from);
      tx.putTableRecord({ ...t, name: to });
      return true;
    });
  }

  /** Drop a table: mark it dropping, clear its keys in pages, remove it (§8.3). */
  dropTable(name: string): Promise<void> {
    return this.run(async (tx) => {
      const t = await tx.tableRecordOrNone(name);
      if (!t) return true;
      if (t.state !== 'dropping') {
        tx.putTableRecord({ ...t, state: 'dropping' });
        return false;
      }
      const ranges = [tx.keys.rows(t.id), tx.keys.range('o', t.id)];
      for (const ix of t.indexes) ranges.push(...indexRanges(tx, ix));
      if (await clearPage(tx, ranges)) return false;
      tx.deleteTableRecord(name);
      return true;
    });
  }

  /**
   * Create an index and build it: it is maintained by writers at once, and
   * a backfill adds the existing rows in pages (§8.2).
   */
  createIndex(table: string, name: string, opts: CreateIndexOptions): Promise<void> {
    const kind = opts.kind ?? 'private';
    if (!SUPPORTED.includes(kind)) {
      throw new DbError('format', `index kind ${kind} is not supported yet`);
    }
    if (kind === 'none' && !opts.unique) {
      throw new DbError('bad_value', 'an index of kind "none" must be unique');
    }
    if (!opts.fields.length) throw new DbError('bad_value', 'an index needs fields');
    return this.run(async (tx) => {
      const t = await tx.tableRecord(table);
      const ix = t.indexes.find((i) => i.name === name);
      if (!ix) {
        const def: IndexDef = {
          name,
          id: bytes.randomBytes(16),
          fields: opts.fields.map(([f, ty, dir]) => [f, ty, dir === 'desc']),
          kind,
          unique: opts.unique ?? false,
          state: 'building',
        };
        for (const k of ['fanout', 'shards', 'decoys', 'maxBytes'] as const) {
          if (opts[k] !== undefined) def[k] = opts[k];
        }
        tx.putTableRecord({ ...t, indexes: [...t.indexes, def] });
        return false;
      }
      if (ix.state !== 'building') return true;
      const [begin, end] = tx.keys.rows(t.id);
      const from = ix.builtTo ? bytes.keyAfter(ix.builtTo) : begin;
      const size = this.db.pageSize;
      const page = await tx.raw.rangeStored(from, end, { limit: size });
      const dups: { pk: Value; holder: Value }[] = [];
      for (const e of page) {
        const r = decodeRow(e.value);
        const pkEl = encode(r.pk);
        const row = await tx.table(table).decode(t, pkEl, e.value);
        const values = indexedValue(ix, row.fields);
        checkTypes(ix, values);
        try {
          await changeIndex(tx, t, ix, r.pk, pkEl, undefined, values);
        } catch (err) {
          if (!(err instanceof DbError && err.code === 'unique_violation')) throw err;
          dups.push({ pk: r.pk, holder: (err.detail as { pk: Value }).pk });
        }
      }
      if (dups.length) {
        throw new DbError('building', `index ${name} on ${table} found duplicates`, dups);
      }
      const done = page.length < size;
      const next: IndexDef = { ...ix, state: done ? 'active' : 'building' };
      delete next.builtTo;
      if (!done) next.builtTo = page.at(-1)!.key;
      tx.putTableRecord({ ...t, indexes: t.indexes.map((i) => (i === ix ? next : i)) });
      return done;
    });
  }

  /** Drop an index: mark it dropping, clear its entries in pages, remove it (§8.3). */
  dropIndex(table: string, name: string): Promise<void> {
    return this.run(async (tx) => {
      const t = await tx.tableRecordOrNone(table);
      const ix = t?.indexes.find((i) => i.name === name);
      if (!t || !ix) return true;
      if (ix.state !== 'dropping') {
        const next: IndexDef = { ...ix, state: 'dropping' };
        tx.putTableRecord({ ...t, indexes: t.indexes.map((i) => (i === ix ? next : i)) });
        return false;
      }
      if (await clearPage(tx, indexRanges(tx, ix))) return false;
      tx.putTableRecord({ ...t, indexes: t.indexes.filter((i) => i !== ix) });
      return true;
    });
  }

  /**
   * Rewrite every row of a table in pages. `fn` returns the new row (same
   * primary key), or null to delete it.
   */
  transform<T extends Fields = Fields>(
    table: string,
    fn: (row: T) => T | null | Promise<T | null>,
  ): Promise<void> {
    return this.run(async (tx, key) => {
      const t = await tx.tableRecord(table);
      const [begin, end] = tx.keys.rows(t.id);
      const size = this.db.pageSize;
      const page = await tx.raw.rangeStored(key ? bytes.keyAfter(key) : begin, end, {
        limit: size,
      });
      const tt = tx.table<T>(table);
      for (const e of page) {
        const pk = decodeRow(e.value).pk;
        const pkEl = encode(pk);
        const old = await tt.decode(t, pkEl, e.value);
        const row = await fn(structuredClone(old.fields) as T);
        if (row === null) await tt.write(t, pk, pkEl, old, undefined);
        else await tt.write(t, pk, pkEl, old, row);
      }
      if (page.length < size) return true;
      return page.at(-1)!.key;
    });
  }

  /**
   * Run one operation: pages until `page` returns true. A page returns
   * false to run again, or a stored key to record as its progress.
   */
  private async run(
    page: (tx: DbTransaction, key: Uint8Array | undefined) => Promise<boolean | Uint8Array>,
  ): Promise<void> {
    const op = this.op++;
    const mkey = this.db.keys.migration(this.step);
    for (;;) {
      const finished = await this.db.transaction(async (tx) => {
        const b = await tx.raw.get(mkey);
        if (!b) throw new DbError('corrupt', `migration step ${this.step} has no record`);
        const rec = decodeMigrationRecord(b);
        const p = decodeProgress(rec.progress);
        if (rec.state === 'done' || p.op > op) return true;
        const r = await page(tx, p.op === op ? p.key : undefined);
        const next: Progress =
          r === true
            ? { op: op + 1 }
            : {
                op,
                ...(r instanceof Uint8Array
                  ? { key: r }
                  : p.op === op && p.key
                    ? { key: p.key }
                    : {}),
              };
        tx.raw.set(mkey, encodeMigrationRecord({ ...rec, progress: encodeProgress(next) }));
        return r === true;
      });
      if (finished) return;
    }
  }
}

/** The stored-key ranges of one index's entries, of every kind. */
function indexRanges(tx: DbTransaction, ix: IndexDef): [Uint8Array, Uint8Array | undefined][] {
  return ['u', 'f', 'n', 'r', 's', 'p', 'q'].map((k) => tx.keys.range(k, ix.id));
}

/**
 * Clear one page of the first non-empty range, at most `max_range_items`
 * keys (§8.3). Returns false when every range is already empty.
 */
async function clearPage(
  tx: DbTransaction,
  ranges: [Uint8Array, Uint8Array | undefined][],
): Promise<boolean> {
  const n = Math.min(tx.db.limits.max_range_items, 1000);
  for (const [begin, end] of ranges) {
    const keys: Uint8Array[] = [];
    for await (const e of tx.raw.snapshotRangeStored(begin, end, { limit: n })) keys.push(e.key);
    if (!keys.length) continue;
    tx.raw.clearRange(begin, keys.length < n ? end : bytes.keyAfter(keys.at(-1)!));
    return true;
  }
  return false;
}

/**
 * Bring the database from its stored schema to `target`, step by step
 * (§8.1). Returns the final DbRecord schema.
 */
export async function migrate(
  db: Db,
  target: number,
  migrations: Record<number, MigrationFn>,
): Promise<void> {
  const dkey = db.keys.dbRecord();
  for (;;) {
    const stored = await db.transaction(async (tx) => {
      const b = await tx.raw.get(dkey);
      return b ? decodeDbRecord(b).schema : 0;
    });
    if (stored >= target) return;
    const step = stored + 1;
    const fn = migrations[step];
    if (!fn) throw new DbError('bad_value', `no migration for schema step ${step}`);
    const mkey = db.keys.migration(step);
    await db.transaction(async (tx) => {
      if (await tx.raw.get(mkey)) return;
      const rec: MigrationRecord = { step, state: 'running', hlc: db.fs.session.clock.tick() };
      tx.raw.set(mkey, encodeMigrationRecord(rec));
    });
    await fn(new Migrator(db, step));
    await db.transaction(async (tx) => {
      const [m, d] = await tx.raw.getAll([mkey, dkey]);
      const rec = decodeMigrationRecord(m!);
      const dbr = decodeDbRecord(d!);
      if (rec.state === 'done') return;
      tx.raw.set(mkey, encodeMigrationRecord({ step, state: 'done', hlc: rec.hlc }));
      tx.raw.set(dkey, encodeDbRecord({ ...dbr, schema: Math.max(dbr.schema, step) }));
    });
  }
}
