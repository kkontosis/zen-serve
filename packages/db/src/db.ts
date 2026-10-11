// The `Db` class (spec/zendb.md §2–3, §16): opening a database, the
// catalog cache, tables and transactions.
import type { UnlockedFs } from '@zen/client';
import type { Limits } from '@zen/wasm';
import { decodeDbRecord, decodeTableRecord, encodeDbRecord, type TableRecord } from './catalog.js';
import type { Value } from './cbor.js';
import { DbError } from './errors.js';
import { Keys } from './keys.js';
import { type MigrationFn, migrate } from './migrate.js';
import { type Page, Query } from './query.js';
import type { Fields } from './row.js';
import { DbTransaction } from './txn.js';

/** Options of `Db.open`. */
export interface DbOptions {
  /** The app's schema version (default 0). */
  schema?: number;
  /** For each step `k` in `1..schema`, its migration (§8.1). */
  migrations?: Record<number, MigrationFn>;
  /** Where warnings go (table scans). Default: `console.warn`. */
  warn?: (msg: string) => void;
  /** Rows per migration page (backfills, transforms). Default 256. */
  pageSize?: number;
}

/** Transaction options (§7.1). */
export interface DbTxnOptions {
  /** `short` (default) or `long`. */
  mode?: 'short' | 'long';
  /** Attempts on `conflict` / `too_old` (default 8). */
  attempts?: number;
}

/** The format this code reads and writes (§3.1). */
const FORMAT = 1;

interface CatalogEntry {
  record: TableRecord | undefined;
  version: Uint8Array | undefined;
}

/** A database `(fs, ns)`. */
export class Db {
  private readonly catalog = new Map<string, CatalogEntry>();
  /** The stored schema version after opening. */
  schema = 0;

  private constructor(
    readonly fs: UnlockedFs,
    readonly ns: string,
    readonly keys: Keys,
    /** The server's limits (`/v1/info`). */
    readonly limits: Limits,
    private readonly opts: DbOptions,
  ) {}

  /**
   * Open (or create) a database and bring it to `opts.schema` (§3.1):
   * an unknown format or integrity level is refused (`format`), a newer
   * stored schema too (`schema_newer`).
   */
  static async open(fs: UnlockedFs, ns: string, opts: DbOptions = {}): Promise<Db> {
    const info = await fs.session.client.info();
    const db = new Db(fs, ns, new Keys(fs.keys.db(ns)), info.limits, opts);
    await db.init();
    return db;
  }

  private async init(): Promise<void> {
    const target = this.opts.schema ?? 0;
    const key = this.keys.dbRecord();
    const rec = await this.transaction(async (tx) => {
      const b = await tx.raw.get(key);
      if (b) return decodeDbRecord(b);
      const r = {
        format: FORMAT,
        schema: 0,
        integrity: 'basic',
        createdHlc: this.fs.session.clock.tick(),
      };
      tx.raw.set(key, encodeDbRecord(r));
      return r;
    });
    if (rec.format !== FORMAT) throw new DbError('format', `database format ${rec.format}`);
    if (rec.integrity !== 'basic') {
      throw new DbError('format', `integrity level ${rec.integrity}`);
    }
    if (rec.schema > target) {
      throw new DbError(
        'schema_newer',
        `the database is at schema ${rec.schema}, the app at ${target}`,
      );
    }
    await migrate(this, target, this.opts.migrations ?? {});
    this.schema = target;
    await this.loadCatalog();
  }

  /** Read every TableRecord into the catalog cache. */
  async loadCatalog(): Promise<void> {
    const [begin, end] = this.keys.tables();
    this.catalog.clear();
    for await (const e of this.fs.kv.rangeStored(begin, end)) {
      const t = decodeTableRecord(e.value);
      this.catalog.set(t.name, { record: t, version: e.version });
    }
  }

  /** The tables (from the catalog cache). */
  tableNames(): string[] {
    return [...this.catalog.values()].flatMap((e) => (e.record ? [e.record.name] : []));
  }

  /** Rows per migration page. */
  get pageSize(): number {
    return this.opts.pageSize ?? 256;
  }

  /** A table handle. Its operations are transactions of their own. */
  table<T extends Fields = Fields>(name: string): Table<T> {
    return new Table<T>(this, name);
  }

  /**
   * Run `fn` in a transaction and commit it (§7). It may run several times,
   * so it must have no side effects but through the transaction.
   */
  transaction<R>(fn: (tx: DbTransaction) => Promise<R>, opts: DbTxnOptions = {}): Promise<R> {
    return this.fs.transaction(
      async (raw) => {
        const tx = new DbTransaction(this, raw);
        const out = await fn(tx);
        await tx.finish();
        return out;
      },
      { mode: opts.mode ?? 'short', ...(opts.attempts ? { attempts: opts.attempts } : {}) },
    );
  }

  /** Free the database keys. */
  close(): void {
    this.keys.db.free();
  }

  // ------------------------------------------------------------ catalog cache

  /** @internal */
  catalogEntry(name: string): CatalogEntry | undefined {
    return this.catalog.get(name);
  }

  /** @internal */
  catalogSet(name: string, record: TableRecord | undefined, version: Uint8Array | undefined) {
    this.catalog.set(name, { record, version });
  }

  /** @internal */
  catalogStale(name: string): void {
    this.catalog.delete(name);
  }

  /** @internal */
  decodeTable(b: Uint8Array): TableRecord {
    return decodeTableRecord(b);
  }

  /** @internal */
  warn(msg: string): void {
    (this.opts.warn ?? console.warn)(msg);
  }
}

/** A table outside a transaction: each operation is a transaction of its own. */
export class Table<T extends Fields = Fields> {
  constructor(
    readonly db: Db,
    readonly name: string,
  ) {}

  get(pk: Value): Promise<T | undefined> {
    return this.db.transaction((tx) => tx.table<T>(this.name).get(pk));
  }

  insert(row: T): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).insert(row));
  }

  put(row: T): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).put(row));
  }

  update(pk: Value, fn: (row: T) => T | Promise<T>): Promise<T> {
    return this.db.transaction((tx) => tx.table<T>(this.name).update(pk, fn));
  }

  delete(pk: Value): Promise<boolean> {
    return this.db.transaction((tx) => tx.table<T>(this.name).delete(pk));
  }

  /** A query, run in a read-only transaction of its own. */
  query(): Query<T> {
    return new Query<T>(
      this.name,
      (run) => this.db.transaction(run),
      (m) => this.db.warn(m),
    );
  }
}

export type { Page };
