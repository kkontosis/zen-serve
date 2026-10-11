// The `Db` class (spec/zendb.md §2–3, §16): opening a database, the
// catalog cache, tables and transactions.
import {
  bytes,
  Transaction as RawTransaction,
  type Transaction,
  type UnlockedFs,
  ZenError,
} from '@zen/client';
import type { IndexKeys, Limits } from '@zen/wasm';
import { decodeDbRecord, decodeTableRecord, encodeDbRecord, type TableRecord } from './catalog.js';
import type { Value } from './cbor.js';
import { DbError } from './errors.js';
import type { Node } from './index/node.js';
import { Keys } from './keys.js';
import { type MigrationFn, migrate } from './migrate.js';
import { type Page, Query } from './query.js';
import type { Fields } from './row.js';
import { type LockManagerLike, Owner } from './tabs.js';
import { DbTransaction, ManualTransaction, type ReadRow, SwitchToLong } from './txn.js';

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
  /** The default `autoSwitchMs` of `auto` transactions (default 3,000). */
  autoSwitchMs?: number;
  /** Rows kept in the row cache (§7.7). Default 1,024; 0 turns it off. */
  rowCache?: number;
  /**
   * Web Locks for the owner election (§9.2). Default `navigator.locks`
   * where it exists; `null`: none, every instance is its own owner.
   */
  locks?: LockManagerLike | null;
  /** `false`: never become the owner (default true). */
  owner?: boolean;
}

/** Transaction options (§7.1). */
export interface DbTxnOptions {
  /** `short` (default), `long`, or `auto` (short, then long if it runs too long). */
  mode?: 'short' | 'long' | 'auto';
  /** Attempts on `conflict` / `too_old` (default 8). */
  attempts?: number;
  /** `auto`: how long the short phase may run (default `DbOptions.autoSwitchMs`). */
  autoSwitchMs?: number;
}

/** Options of `importRows`. */
export interface ImportOptions {
  /** Replace existing rows (`put`) instead of failing with `exists` (`insert`). */
  replace?: boolean;
  /** Called after each commit with the number of rows written so far. */
  onCommit?: (done: number) => void;
}

/** Cache counters (tests, tuning). */
export interface DbStats {
  rowHits: number;
  rowMisses: number;
}

/** The format this code reads and writes (§3.1). */
const FORMAT = 1;

/** Decoded private-index nodes kept per `Db` (they never change, §5.4.3). */
const NODE_CACHE = 4096;

/** `importRows` batch sizes (§7.5). */
const IMPORT_FIRST = 256;
const IMPORT_MAX = 4096;

/** A cached row: its fields at `version` (§7.7). */
interface CachedRow {
  version: Uint8Array;
  fields: Fields;
  parts: number;
}

const isTooLarge = (e: unknown) =>
  (e instanceof DbError || e instanceof ZenError) && e.code === 'too_large';

interface CatalogEntry {
  record: TableRecord | undefined;
  version: Uint8Array | undefined;
}

/** A database `(fs, ns)`. */
export class Db {
  private readonly catalog = new Map<string, CatalogEntry>();
  private readonly nodes = new Map<string, Node>();
  private readonly ixKeys = new Map<string, IndexKeys>();
  /** Sealed indexes by index id: the entries of the blob with this digest. */
  private readonly sealed = new Map<string, { digest: string; keys: Uint8Array[] }>();
  /** Rows by stored key (hex), least recently used first. */
  private readonly rows = new Map<string, CachedRow>();
  private owner: Owner | undefined;
  /** The stored schema version after opening. */
  schema = 0;
  /** Cache counters. */
  readonly stats: DbStats = { rowHits: 0, rowMisses: 0 };

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
    db.owner = new Owner(
      opts.owner === false
        ? undefined
        : opts.locks === undefined
          ? (globalThis as { navigator?: { locks?: LockManagerLike } }).navigator?.locks
          : (opts.locks ?? undefined),
      `zen/db/${fs.id}/${ns}`,
      opts.owner !== false,
    );
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
  async transaction<R>(fn: (tx: DbTransaction) => Promise<R>, opts: DbTxnOptions = {}): Promise<R> {
    const mode = opts.mode ?? 'short';
    const attempts = opts.attempts ?? 8;
    if (mode !== 'auto') return this.run(fn, mode, attempts);
    // `auto` (§7.1): short, retried on `conflict` only; past its time or on
    // `too_old`, the whole function again in long mode.
    const ms = opts.autoSwitchMs ?? this.opts.autoSwitchMs ?? 3000;
    let runs = 0;
    try {
      return await this.run(fn, 'short', attempts, {
        deadline: () => Date.now() + ms,
        onRun: () => runs++,
        retry: (e) => e instanceof ZenError && e.code === 'conflict',
      });
    } catch (e) {
      if (!(e instanceof SwitchToLong || (e instanceof ZenError && e.code === 'too_old'))) throw e;
      return this.run(fn, 'long', Math.max(1, attempts - runs));
    }
  }

  private run<R>(
    fn: (tx: DbTransaction) => Promise<R>,
    mode: 'short' | 'long',
    attempts: number,
    auto?: { deadline: () => number; onRun: () => void; retry: (e: unknown) => boolean },
  ): Promise<R> {
    return this.fs.transaction(
      async (raw) => {
        auto?.onRun();
        const tx = new DbTransaction(this, raw, auto?.deadline());
        let out: R;
        try {
          out = await fn(tx);
        } catch (e) {
          throw longRange(raw, e);
        }
        await tx.finish();
        return out;
      },
      { mode, attempts, ...(auto ? { retry: auto.retry } : {}) },
    );
  }

  /**
   * A transaction the app commits or aborts itself (§7.2), with no
   * automatic retry: `conflict` reaches the caller.
   */
  begin(opts: { mode?: 'short' | 'long' } = {}): ManualTransaction {
    const mode = opts.mode ?? 'short';
    if ((mode as string) === 'auto') {
      throw new DbError('bad_value', 'a manual transaction is short or long');
    }
    return new ManualTransaction(this, new RawTransaction(this.fs, mode));
  }

  /**
   * Write many rows in several commits (§7.5): batches start at 256 rows,
   * halve on `too_large` and double (up to 4,096) after 4 commits in a row.
   * Each commit is atomic; the load as a whole is not. Returns the number
   * of rows written.
   */
  async importRows<T extends Fields>(
    table: string,
    rows: Iterable<T>,
    opts: ImportOptions = {},
  ): Promise<number> {
    const list = [...rows];
    let n = IMPORT_FIRST;
    let streak = 0;
    let done = 0;
    while (done < list.length) {
      const batch = list.slice(done, done + n);
      try {
        await this.transaction(async (tx) => {
          const tt = tx.table<T>(table);
          for (const r of batch) await (opts.replace ? tt.put(r) : tt.insert(r));
        });
      } catch (e) {
        if (!isTooLarge(e) || batch.length === 1) throw e;
        n = Math.ceil(batch.length / 2);
        streak = 0;
        continue;
      }
      done += batch.length;
      opts.onCommit?.(done);
      if (++streak >= 4) {
        n = Math.min(n * 2, IMPORT_MAX);
        streak = 0;
      }
    }
    return done;
  }

  /** Whether this instance is the owner (§9.2). */
  get isOwner(): boolean {
    return this.owner?.isOwner ?? false;
  }

  /**
   * Call `fn` when this instance becomes the owner (at once if it is).
   * Returns a function that unsubscribes.
   */
  onOwner(fn: () => void): () => void {
    return this.owner!.onOwner(fn);
  }

  /** Resolves at the next change of `isOwner` (tests). */
  get ownerChange(): Promise<boolean> {
    return this.owner!.changed();
  }

  /** Free the database keys and caches, and hand the owner role over. */
  close(): void {
    this.owner?.close();
    for (const k of this.ixKeys.values()) k.free();
    this.ixKeys.clear();
    this.rows.clear();
    this.nodes.clear();
    this.sealed.clear();
    this.keys.db.free();
  }

  // ------------------------------------------------------------ row cache

  /** @internal A cached row, by stored key; a copy the caller may change. */
  cachedRow(key: string): CachedRow | undefined {
    const r = this.rows.get(key);
    if (!r) {
      this.stats.rowMisses++;
      return undefined;
    }
    this.stats.rowHits++;
    this.rows.delete(key);
    this.rows.set(key, r);
    return { ...r, fields: structuredClone(r.fields) };
  }

  /** @internal */
  cacheRow(key: string, r: CachedRow): void {
    const max = this.opts.rowCache ?? 1024;
    if (max <= 0) return;
    this.rows.delete(key);
    this.rows.set(key, { ...r, fields: structuredClone(r.fields) });
    if (this.rows.size > max) this.rows.delete(this.rows.keys().next().value!);
  }

  /** @internal */
  dropRow(key: string): void {
    this.rows.delete(key);
  }

  /** @internal After a commit: its writes have new versions, so drop their cached copies. */
  rowsWritten(raw: Transaction): void {
    if (raw.clears.length) this.rows.clear();
    for (const h of raw.writes.keys()) this.rows.delete(h);
  }

  // ------------------------------------------------------------ index caches

  /** @internal The keys of a private index. */
  indexKeys(id: Uint8Array): IndexKeys {
    const h = bytes.hex(id);
    let k = this.ixKeys.get(h);
    if (!k) {
      k = this.keys.db.index(id);
      this.ixKeys.set(h, k);
    }
    return k;
  }

  /** @internal A cached node, by id. */
  cachedNode(id: string): Node | undefined {
    const n = this.nodes.get(id);
    if (n) {
      this.nodes.delete(id);
      this.nodes.set(id, n);
    }
    return n;
  }

  /** @internal */
  cacheNode(id: string, n: Node): void {
    this.nodes.set(id, n);
    if (this.nodes.size > NODE_CACHE) this.nodes.delete(this.nodes.keys().next().value!);
  }

  /** @internal A sealed index's entries, if the cached blob has this digest. */
  cachedSealed(index: string, digest: string): Uint8Array[] | undefined {
    const c = this.sealed.get(index);
    return c?.digest === digest ? c.keys : undefined;
  }

  /** @internal */
  cacheSealed(index: string, digest: string, keys: Uint8Array[]): void {
    this.sealed.set(index, { digest, keys });
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

/** A long-mode range too large to check (§7.1): split the work. */
function longRange(raw: Transaction, e: unknown): unknown {
  if (raw.mode === 'long' && e instanceof ZenError && e.code === 'too_large') {
    return new DbError('too_large', 'a range too large for a long transaction: split the work');
  }
  return e;
}

export type { Page, ReadRow };
