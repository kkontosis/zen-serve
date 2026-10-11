// The `Db` class (spec/zendb.md §2–3, §16): opening a database, the
// catalog cache, tables and transactions.
import {
  bytes,
  Transaction as RawTransaction,
  type SealedStore,
  type Topic,
  type Transaction,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import type { CrdtKeys, IndexKeys, Info, Limits } from '@zen/wasm';
import {
  bindingOf,
  type ChangeBody,
  type ConsumeOptions,
  ConsumerHandle,
  checkType,
  type Delivered,
  DirectSource,
  type EmitOptions,
  type EphemeralMsg,
  type OnHandler,
  type OnOptions,
  OnSubscription,
  type OnTarget,
  openEphemeral,
  type SourceSub,
  type StreamSource,
} from './broker.js';
import {
  type DbRecord,
  decodeDbRecord,
  decodeTableRecord,
  encodeDbRecord,
  encodeTableRecord,
  type TableRecord,
} from './catalog.js';
import { decode, type Value } from './cbor.js';
import { CrdtCache, type CrdtRead, type Entry, isNetwork } from './crdtcache.js';
import { CrdtQueue, memoryQueueStore, type QueueStore } from './crdtq.js';
import { DbError } from './errors.js';
import { decodeNode, type Node } from './index/node.js';
import { decodeBlob, encodeBlob } from './index/sealed.js';
import { startIndexers } from './ixer.js';
import { Keys } from './keys.js';
import {
  decodeCachedRow,
  decodeEntry,
  decodeSealed,
  decodeTable,
  encodeCachedRow,
  encodeEntry,
  encodeSealed,
  encodeTable,
  LocalCache,
} from './localcache.js';
import { localQueueStore, localStores } from './localq.js';
import { type MigrationFn, migrate } from './migrate.js';
import { encodeMsg, type Msg } from './msg.js';
import { type BroadcastChannelLike, type ChannelFactory, TabSource } from './proxy.js';
import { type Page, Query } from './query.js';
import {
  DEDUP_TTL,
  dedupRecord,
  dedupSeen,
  errorBody,
  Inbox,
  type Reply,
  ReplyError,
  type RequestAllOptions,
  type RequestMsg,
  type RequestOptions,
  replyMsg,
  sendReply,
} from './reply.js';
import { type Fields, partsDigest } from './row.js';
import { Saga, type SagaOptions, type SagaStep } from './saga.js';
import { type ScheduleOptions, Scheduler, type SchedulerOptions } from './sched.js';
import { SYSTEM_TABLES, systemTable } from './system.js';
import { type LockManagerLike, Owner } from './tabs.js';
import {
  DbTransaction,
  ManualTransaction,
  type ReadRow,
  SwitchToLong,
  type TableTx,
} from './txn.js';

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
  /**
   * The channel to the other tabs (§9.2): with Web Locks, a tab that isn't
   * the owner subscribes through the owner's stream. Default
   * `BroadcastChannel` where it exists; `null`: every instance streams itself.
   */
  channel?: ChannelFactory | null;
  /** `false`: don't run the scheduler in the owner (§12.4; default true). */
  scheduler?: boolean;
  /** The scheduler's poll interval, lease and batch. */
  schedulerOptions?: SchedulerOptions;
  /**
   * `false`: don't run the indexer of CRDT tables in the owner (§19.6;
   * default true). Their indexes then lag until an instance runs it.
   */
  indexer?: boolean;
  /** CRDT object states kept for reads (§19.4). Default 1,024; 0 turns the cache off. */
  crdtCache?: number;
  /** How long a cached CRDT state is used unchecked, in `auto` reads (default 5,000 ms). */
  crdtCacheMs?: number;
  /** The default read mode of CRDT tables (§19.4; default `auto`). */
  crdtRead?: CrdtRead;
  /**
   * `false`: CRDT-only transactions commit at once, like any other, instead
   * of going through the queue (§19.7; default true).
   */
  crdtQueue?: boolean;
  /**
   * Where the queue of CRDT writes is kept (§19.7). Default: on `local`
   * when given, else in memory.
   */
  crdtStore?: QueueStore;
  /**
   * The sealed local store (§9.3) this database keeps its local state on:
   * the queue of CRDT writes, so it survives restarts. Tabs that share it
   * (IndexedDB) share the queue, and only the owner sends it.
   */
  local?: SealedStore;
  /**
   * `false`: keep the caches in memory even with `local` (the queue is
   * still kept there). Default: with `local`, the caches persist too
   * (§9.3), and the database opens offline from them.
   */
  persist?: boolean;
  /** How long the queue waits to gather writes into one commit (default 10 ms). */
  crdtBatchMs?: number;
}

/** Options of `db.table`. */
export interface TableOptions {
  /** CRDT writes resolve when committed, not when queued (§19.7). */
  wait?: boolean;
}

/** Options of `serve` (§12.3). */
export interface ServeOptions extends ConsumeOptions {
  /** `ephemeral`: reply with `epub` after the commit instead of an event. */
  reply?: 'event' | 'ephemeral';
}

/** Transaction options (§7.1). */
export interface DbTxnOptions {
  /** `short` (default), `long`, or `auto` (short, then long if it runs too long). */
  mode?: 'short' | 'long' | 'auto';
  /** Attempts on `conflict` / `too_old` (default 8). */
  attempts?: number;
  /** `auto`: how long the short phase may run (default `DbOptions.autoSwitchMs`). */
  autoSwitchMs?: number;
  /**
   * A transaction of CRDT writes only is queued (§19.7) and resolves once
   * queued; `false`: commit it at once, like any other (default
   * `DbOptions.crdtQueue`).
   */
  queue?: boolean;
  /** A queued transaction resolves when committed, not when queued. */
  wait?: boolean;
  /** The read mode of its CRDT reads (§19.4; default `DbOptions.crdtRead`). */
  read?: CrdtRead;
  /** @internal One call of a `Table`: a single entry of the queue. */
  single?: boolean;
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

/** The records of store `db/<ns>` (§9.3). */
const INFO_KEY = bytes.utf8('info');
const DB_KEY = bytes.utf8('db');
const tableKey = (name: string) => bytes.utf8(`t${name}`);

/** What an offline open reads from the local store (§3.1). */
interface Kept {
  info: Info;
  record: DbRecord;
  tables: Map<string, CatalogEntry>;
}

/** The kept info, DbRecord and catalog of database `ns`, if all are there. */
async function keptOf(local: SealedStore, ns: string): Promise<Kept | undefined> {
  const store = `db/${ns}`;
  try {
    const i = await local.get(store, INFO_KEY);
    const r = await local.get(store, DB_KEY);
    if (!i || !r) return undefined;
    const tables = new Map<string, CatalogEntry>();
    for await (const [k, v] of local.list(store)) {
      if (k[0] !== 0x74) continue; // "t"
      const t = decodeTable(v);
      const record = decodeTableRecord(t.record);
      tables.set(record.name, { record, version: t.version });
    }
    return { info: zw.decodeInfo(i) as Info, record: decodeDbRecord(r), tables };
  } catch {
    return undefined;
  }
}

/** A database `(fs, ns)`. */
export class Db {
  private readonly catalog = new Map<string, CatalogEntry>();
  private readonly nodes = new Map<string, Node>();
  private readonly ixKeys = new Map<string, IndexKeys>();
  private readonly crdtKs = new Map<string, CrdtKeys>();
  /** Sealed indexes by index id: the entries of the blob with this digest. */
  private readonly sealed = new Map<string, { digest: string; keys: Uint8Array[] }>();
  /** Rows by stored key (hex), least recently used first. */
  private readonly rows = new Map<string, CachedRow>();
  private owner: Owner | undefined;
  /** The caches on the local store (§9.3), with `DbOptions.local`. */
  private readonly persisted:
    | { db: LocalCache; rows: LocalCache; crdt: LocalCache; nodes: LocalCache; sealed: LocalCache }
    | undefined;
  /** Whether this instance opened offline, from the state the local store kept (§3.1). */
  offline = false;
  /** @internal Web Locks, when used (§9.2). */
  locks: LockManagerLike | undefined;
  private unQueue: (() => void) | undefined;
  private readonly topics = new Map<string, Topic>();
  private stream: StreamSource | undefined;
  private readonly subs = new Set<OnSubscription | SourceSub>();
  private readonly consumers = new Set<ConsumerHandle>();
  private box: Inbox | undefined;
  /** The scheduler (§12.4), unless `DbOptions.scheduler` is false. It runs in the owner. */
  scheduler: Scheduler | undefined;
  private unSched: (() => void) | undefined;
  /** The stored schema version after opening. */
  schema = 0;
  /** Cache counters. */
  readonly stats: DbStats = { rowHits: 0, rowMisses: 0 };
  /** @internal CRDT states (§19.4). */
  readonly crdtCache: CrdtCache;
  /** @internal The queue of CRDT writes (§19.7). */
  crdtQueue!: CrdtQueue;

  private constructor(
    readonly fs: UnlockedFs,
    readonly ns: string,
    readonly keys: Keys,
    /** The server's limits (`/v1/info`). */
    readonly limits: Limits,
    /** The server's features (`/v1/info`). */
    readonly features: string[],
    private readonly opts: DbOptions,
  ) {
    this.crdtCache = new CrdtCache(opts.crdtCache ?? 1024, opts.crdtCacheMs ?? 5000);
    if (opts.local && opts.persist !== false) {
      const warn = (m: string) => this.warn(m);
      const at = (s: string) => new LocalCache(opts.local!, `${s}/${ns}`, warn);
      this.persisted = {
        db: at('db'),
        rows: at('rows'),
        crdt: at('crdt'),
        nodes: at('nodes'),
        sealed: at('sealed'),
      };
      const crdt = this.persisted.crdt;
      this.crdtCache.sink = {
        put: (h, e) => crdt.put(bytes.fromHex(h), encodeEntry(e)),
        delete: (h) => crdt.delete(bytes.fromHex(h)),
      };
    }
  }

  /**
   * The queue of CRDT writes (§19.7): `pending` entries, `online`,
   * `flush()` and `onSync(fn)`.
   */
  get crdt(): CrdtQueue {
    return this.crdtQueue;
  }

  /**
   * Open (or create) a database and bring it to `opts.schema` (§3.1):
   * an unknown format or integrity level is refused (`format`), a newer
   * stored schema too (`schema_newer`).
   */
  static async open(fs: UnlockedFs, ns: string, opts: DbOptions = {}): Promise<Db> {
    // Offline: open from what the local store kept (§3.1), if anything.
    const keptOr = async (e: unknown): Promise<Kept> => {
      const k =
        isNetwork(e) && opts.local && opts.persist !== false
          ? await keptOf(opts.local, ns)
          : undefined;
      if (!k) throw e;
      return k;
    };
    let info: Info;
    let kept: Kept | undefined;
    try {
      info = await fs.session.client.info();
    } catch (e) {
      kept = await keptOr(e);
      info = kept.info;
    }
    const db = new Db(fs, ns, new Keys(fs.keys.db(ns)), info.limits, info.features, opts);
    const locks =
      opts.locks === undefined
        ? (globalThis as { navigator?: { locks?: LockManagerLike } }).navigator?.locks
        : (opts.locks ?? undefined);
    db.locks = locks;
    const name = `zen/db/${fs.id}/${ns}`;
    const BC = (globalThis as { BroadcastChannel?: new (n: string) => BroadcastChannelLike })
      .BroadcastChannel;
    const channel =
      opts.channel === undefined ? (BC ? (n: string) => new BC(n) : undefined) : opts.channel;
    // A queue on a store the tabs may share is sent by the owner only (§19.7).
    const store =
      opts.crdtStore ?? (opts.local ? localQueueStore(opts.local, ns) : memoryQueueStore());
    const shared = !!locks && !!(opts.crdtStore || opts.local);
    db.crdtQueue = new CrdtQueue(
      db,
      store,
      opts.crdtBatchMs ?? 10,
      shared,
      shared ? channel?.(`${name}/queue`) : undefined,
    );
    if (!kept) {
      try {
        db.keepOpen(info, await db.init());
      } catch (e) {
        kept = await keptOr(e);
      }
    }
    if (kept) await db.openKept(kept);
    await db.loadLocal();
    await db.crdtQueue.load();
    db.owner = new Owner(opts.owner === false ? undefined : locks, name, opts.owner !== false);
    if (shared) db.unQueue = db.onOwner(() => void db.crdtQueue.start());
    // Without locks every instance owns: nothing to proxy.
    if (locks && channel) db.stream = new TabSource(db, channel(name));
    if (opts.scheduler !== false) {
      const s = new Scheduler(db, opts.schedulerOptions);
      db.scheduler = s;
      db.unSched = db.onOwner(() => s.start());
    }
    if (opts.indexer !== false) {
      startIndexers(
        db,
        [...db.catalog.values()].flatMap((e) => (e.record ? [e.record] : [])),
      );
    }
    return db;
  }

  /** Open offline from the kept DbRecord and catalog: the schema must be the app's (§3.1). */
  private async openKept(k: Kept): Promise<void> {
    const target = this.opts.schema ?? 0;
    const rec = k.record;
    if (rec.format !== FORMAT) throw new DbError('format', `database format ${rec.format}`);
    if (rec.integrity !== 'basic') throw new DbError('format', `integrity level ${rec.integrity}`);
    if (rec.schema !== target) {
      throw new DbError(
        'offline',
        `the database is at schema ${rec.schema}, the app at ${target}: opening needs the server`,
      );
    }
    this.schema = target;
    this.offline = true;
    this.catalog.clear();
    for (const [name, e] of k.tables) this.catalog.set(name, e);
  }

  /** Keep what an offline open needs (§3.1): the server's info, the DbRecord, the catalog. */
  private keepOpen(info: Info, rec: DbRecord): void {
    const p = this.persisted;
    if (!p) return;
    p.db.clear();
    p.db.put(INFO_KEY, zw.encodeInfo(info));
    p.db.put(DB_KEY, encodeDbRecord(rec));
    for (const [name, e] of this.catalog) {
      if (e.record) p.db.put(tableKey(name), encodeTable(encodeTableRecord(e.record), e.version));
    }
  }

  /** Load the kept rows and CRDT states into their caches (§9.3). */
  private async loadLocal(): Promise<void> {
    const p = this.persisted;
    if (!p) return;
    const max = this.opts.rowCache ?? 1024;
    const extra: Uint8Array[] = [];
    for await (const [k, v] of p.rows.list()) {
      try {
        if (this.rows.size >= max) throw new Error('over the cache size');
        this.rows.set(bytes.hex(k), decodeCachedRow(v));
      } catch {
        extra.push(k);
      }
    }
    for (const k of extra) p.rows.delete(k);
    const states: [string, Entry][] = [];
    for await (const [k, v] of p.crdt.list()) {
      try {
        states.push([bytes.hex(k), decodeEntry(v)]);
      } catch {
        p.crdt.delete(k);
      }
    }
    // The most recently checked last, so the LRU keeps them.
    states.sort((a, b) => a[1].checked - b[1].checked);
    for (const [h, e] of states) this.crdtCache.restore(h, e);
  }

  private async init(): Promise<DbRecord> {
    const target = this.opts.schema ?? 0;
    const key = this.keys.dbRecord();
    const rec = await this.transaction(async (tx) => {
      const sys = SYSTEM_TABLES.map((n) => this.keys.tableRecord(n));
      const [b, ...have] = await tx.raw.getAll([key, ...sys]);
      // The system tables come with the DbRecord, or on the first open of
      // a database created before them (§11.5).
      have.forEach((h, i) => {
        if (!h) tx.raw.set(sys[i]!, encodeTableRecord(systemTable(SYSTEM_TABLES[i]!)));
      });
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
    return { ...rec, schema: target };
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

  /**
   * Write what the caches have pending to the local store (§9.3). `close`
   * does it too; this resolves once it is written.
   */
  async flushLocal(): Promise<void> {
    const p = this.persisted;
    if (p) await Promise.all(Object.values(p).map((c) => c.flush()));
  }

  /** The app's tables (from the catalog cache); the system tables (§11.5) are left out. */
  tableNames(): string[] {
    return [...this.catalog.values()].flatMap((e) =>
      e.record && !e.record.name.startsWith('$') ? [e.record.name] : [],
    );
  }

  /** Rows per migration page. */
  get pageSize(): number {
    return this.opts.pageSize ?? 256;
  }

  /** A table handle. Its operations are transactions of their own. */
  table<T extends Fields = Fields>(name: string, opts: TableOptions = {}): Table<T> {
    return new Table<T>(this, name, opts);
  }

  /**
   * Run `fn` in a transaction and commit it (§7). It may run several times,
   * so it must have no side effects but through the transaction.
   */
  async transaction<R>(fn: (tx: DbTransaction) => Promise<R>, opts: DbTxnOptions = {}): Promise<R> {
    let last: DbTransaction | undefined;
    const read = opts.read ?? this.opts.crdtRead ?? 'auto';
    const queue = opts.queue ?? this.opts.crdtQueue ?? true;
    const queueAs = queue ? { single: !!opts.single } : undefined;
    const out = await this.attempts((tx) => {
      last = tx;
      tx.crdtRead = read;
      tx.queueAs = queueAs;
      tx.direct = true;
      return fn(tx);
    }, opts);
    // A CRDT-only transaction became a queue entry (§19.7): it completes when queued.
    const q = last?.queued;
    if (q) {
      await this.crdtQueue.add(q);
      last!.runAfters();
      if (opts.wait) await this.crdtQueue.wait(q.id);
    }
    return out;
  }

  private async attempts<R>(fn: (tx: DbTransaction) => Promise<R>, opts: DbTxnOptions): Promise<R> {
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
  begin(opts: { mode?: 'short' | 'long'; consumes?: Delivered<unknown> } = {}): ManualTransaction {
    const mode = opts.mode ?? 'short';
    if ((mode as string) === 'auto') {
      throw new DbError('bad_value', 'a manual transaction is short or long');
    }
    const tx = new ManualTransaction(this, new RawTransaction(this.fs, mode));
    if (opts.consumes) {
      // Bound to a delivery (§11.3): the commit consumes it.
      const b = bindingOf(opts.consumes);
      if (!b) throw new DbError('bad_value', 'consumes: not a delivered message');
      tx.consumes(b, opts.consumes.eventId);
    }
    return tx;
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

  // ------------------------------------------------------------ broker

  /** A topic of this database's fs, by path (cached). */
  topic(...path: (string | Uint8Array)[]): Topic {
    const k = path.map((p) => bytes.hex(typeof p === 'string' ? bytes.utf8(p) : p)).join('/');
    let t = this.topics.get(k);
    if (!t) {
      t = this.fs.topic(...path);
      this.topics.set(k, t);
    }
    return t;
  }

  /** Emit a message in a transaction of its own (§11.1). Returns its id. */
  emit(topic: Topic, type: string, body: Value, opts: EmitOptions = {}): Promise<Uint8Array> {
    const id = opts.id ?? bytes.randomBytes(16);
    return this.transaction(async (tx) => tx.emit(topic, type, body, { ...opts, id }));
  }

  /**
   * Subscribe to a topic or a prefix (§11.2). Plain handlers get every
   * event from now (or after `after`) while the subscription runs; with
   * `cursor`, the handler runs in a transaction that advances a durable
   * cursor, exactly once per device and name.
   */
  async on<B = Value>(
    target: OnTarget,
    handler: OnHandler<B>,
    opts: OnOptions = {},
  ): Promise<OnSubscription> {
    const sub = new OnSubscription(this, target, handler as OnHandler<never>, opts);
    await sub.start();
    this.subs.add(sub);
    return sub;
  }

  /**
   * Run a consumer group (§11.3): each delivery is one transaction with the
   * handler's writes and emits and the consume step. It runs while this
   * instance is the owner (§9.2).
   */
  consume<B = Value>(
    opts: ConsumeOptions,
    handler: (msg: Delivered<B>, tx: DbTransaction) => unknown,
  ): ConsumerHandle {
    const run = opts.dedup
      ? async (msg: Delivered<B>, tx: DbTransaction) => {
          if (await dedupSeen(tx, opts.group, msg.id)) return;
          await handler(msg, tx);
          await dedupRecord(tx, opts.group, msg.id, opts.dedupTtlMs ?? DEDUP_TTL);
        }
      : handler;
    const h = new ConsumerHandle(this, opts, run as never, false);
    this.consumers.add(h);
    return h;
  }

  /**
   * A consumer whose handler acknowledges each message itself: `msg.ack()`,
   * or a `db.begin({consumes: msg})` it commits. A throw, or neither, nacks.
   */
  consumeManual<B = Value>(
    opts: ConsumeOptions,
    handler: (msg: Delivered<B>) => unknown,
  ): ConsumerHandle {
    const h = new ConsumerHandle(this, opts, handler as never, true);
    this.consumers.add(h);
    return h;
  }

  /** Publish an ephemeral message (§11.4): never stored, best effort, rate-limited. */
  async publishEphemeral(topic: Topic, type: string, body: Value): Promise<void> {
    checkType(type);
    await this.source.publish(topic, encodeMsg({ type, id: bytes.randomBytes(16), body }));
  }

  /** Receive ephemeral messages published from now on (§11.4). */
  async onEphemeral<B = Value>(
    topic: Topic,
    handler: (m: EphemeralMsg<B>) => void,
  ): Promise<SourceSub> {
    const sub = await this.source.subscribeEphemeral(topic, {
      onEvent: (e) => {
        const m = openEphemeral<B>(topic, e);
        if (m) handler(m);
      },
    });
    this.subs.add(sub);
    return sub;
  }

  /** Schedule a message (§12.4) in a transaction of its own. Returns its id. */
  schedule(topic: Topic, type: string, body: Value, opts: ScheduleOptions): Promise<Uint8Array> {
    const id = opts.id ?? bytes.randomBytes(16);
    return this.transaction((tx) => tx.schedule(topic, type, body, { ...opts, id }));
  }

  /** Cancel a scheduled message. False if it was delivered (or never scheduled). */
  cancel(id: Uint8Array): Promise<boolean> {
    return this.transaction((tx) => tx.cancel(id));
  }

  /** @internal This instance's inbox (§12.3), created on first use. */
  get inbox(): Inbox {
    this.box ??= new Inbox(this);
    return this.box;
  }

  /**
   * Send a request and wait for its reply (§12.3): the reply message, or
   * `timeout`; an error reply rejects with a `ReplyError`.
   */
  async request<B = Value>(
    topic: Topic,
    type: string,
    body: Value,
    opts: RequestOptions = {},
  ): Promise<Reply<B>> {
    await this.inbox.start();
    const { p } = await this.transaction(async (tx) => ({
      p: tx.request<B>(topic, type, body, opts),
    }));
    return p;
  }

  /** Scatter-gather (§12.3): the replies that arrive before the timeout (or the first `max`). */
  async requestAll<B = Value>(
    topic: Topic,
    type: string,
    body: Value,
    opts: RequestAllOptions = {},
  ): Promise<Reply<B>[]> {
    await this.inbox.start();
    const { p } = await this.transaction(async (tx) => ({
      p: tx.requestAll<B>(topic, type, body, opts),
    }));
    return p;
  }

  /**
   * A responder (§12.3): a consumer whose handler returns the reply body.
   * The reply goes to the request's `reply_to` in the transaction that
   * consumes it; a `ReplyError` sends an error reply and consumes the
   * request; any other throw nacks (or retries, `retry: 'delay'`). With
   * `dedup`, a repeated `corr` gets the stored reply without the handler.
   */
  serve<B = Value>(
    opts: ServeOptions,
    handler: (msg: Delivered<B>, tx: DbTransaction) => Value | Promise<Value>,
  ): ConsumerHandle {
    const eph = opts.reply === 'ephemeral';
    const scope = opts.group;
    const ttl = opts.dedupTtlMs ?? DEDUP_TTL;
    const run = async (msg: Delivered<B>, tx: DbTransaction) => {
      const key = msg.corr ?? msg.id;
      if (opts.dedup) {
        const seen = await dedupSeen(tx, scope, key);
        if (seen) {
          if (seen.reply) sendReply(tx, msg, seen.reply, eph);
          return;
        }
      }
      const r = { type: `${msg.type}.reply`, body: (await handler(msg, tx)) ?? null };
      sendReply(tx, msg, r, eph);
      if (opts.dedup) await dedupRecord(tx, scope, key, ttl, r);
    };
    const fail = async (msg: Delivered, e: unknown, tx: DbTransaction) => {
      if (!(e instanceof ReplyError)) return false;
      const r = { type: `${msg.type}.error`, body: errorBody(e) };
      sendReply(tx, msg, r, eph);
      if (opts.dedup) await dedupRecord(tx, scope, msg.corr ?? msg.id, ttl, r);
      return true;
    };
    const h = new ConsumerHandle(this, opts, run as never, false, fail);
    this.consumers.add(h);
    return h;
  }

  /**
   * A saga and its orchestrator (§12.5): `start(data, tx?)` runs the steps'
   * commands in order and, on an error reply or a timeout, the completed
   * steps' compensations newest first. The orchestrator runs in the owner.
   */
  saga<D extends Record<string, Value> = Record<string, Value>>(
    name: string,
    steps: SagaStep[],
    opts: SagaOptions = {},
  ): Saga<D> {
    return new Saga<D>(this, name, steps, opts);
  }

  /** Reply to a request outside a consumer (§12.3): an `on` responder, scatter-gather. */
  async reply(req: RequestMsg, body: Value, opts: { ephemeral?: boolean } = {}): Promise<void> {
    if (!req.replyTo) return;
    const r = { type: `${req.type}.reply`, body };
    if (opts.ephemeral) {
      await this.publishMsg(this.topic(...req.replyTo), replyMsg(req, r));
      return;
    }
    await this.transaction(async (tx) => sendReply(tx, req, r));
  }

  /** @internal Publish an encoded Msg ephemerally. */
  async publishMsg(topic: Topic, m: Msg): Promise<void> {
    await this.source.publish(topic, encodeMsg(m));
  }

  /** @internal The stream subscriptions come from. */
  get source(): StreamSource {
    this.stream ??= new DirectSource(this.fs.session, (e) => this.warn(`stream: ${e.message}`));
    return this.stream;
  }

  /** @internal This device's fingerprint (durable cursors are per device). */
  get device(): Uint8Array {
    return this.fs.session.deviceFp ?? new Uint8Array(32);
  }

  /**
   * Delete everything database `ns` keeps in a local store (§9.3): its
   * queue of unsent writes too. For sign-out; close the database first.
   */
  static async wipe(local: SealedStore, ns: string): Promise<void> {
    for (const s of localStores(ns)) await local.clear(s);
  }

  /**
   * Free the database keys and caches, stop its consumers and
   * subscriptions, and hand the owner role over.
   */
  close(): Promise<void> {
    const flushed = this.flushLocal();
    this.unQueue?.();
    this.crdtQueue?.close();
    this.crdtCache.clear();
    this.unSched?.();
    void this.scheduler?.stop();
    this.box?.close();
    for (const c of this.consumers) void c.stop();
    this.consumers.clear();
    for (const s of this.subs) void s.close().catch(() => {});
    this.subs.clear();
    this.stream?.close();
    this.stream = undefined;
    for (const t of this.topics.values()) t.close();
    this.topics.clear();
    this.owner?.close();
    for (const k of this.ixKeys.values()) k.free();
    this.ixKeys.clear();
    for (const k of this.crdtKs.values()) k.free();
    this.crdtKs.clear();
    this.rows.clear();
    this.nodes.clear();
    this.sealed.clear();
    this.keys.db.free();
    return flushed;
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
    const p = this.persisted?.rows;
    if (p) {
      try {
        p.put(bytes.fromHex(key), encodeCachedRow(r));
      } catch {
        // a row CBOR can't hold stays in memory only
      }
    }
    if (this.rows.size > max) this.dropRow(this.rows.keys().next().value!);
  }

  /** @internal */
  dropRow(key: string): void {
    this.rows.delete(key);
    this.persisted?.rows.delete(bytes.fromHex(key));
  }

  /** @internal After a commit: its writes have new versions, so drop their cached copies. */
  rowsWritten(raw: Transaction): void {
    if (raw.clears.length) {
      this.rows.clear();
      this.persisted?.rows.clear();
    }
    for (const h of raw.writes.keys()) this.dropRow(h);
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

  /** @internal The token keys of a CRDT table (§19.2). */
  crdtKeys(tableId: Uint8Array): CrdtKeys {
    const h = bytes.hex(tableId);
    let k = this.crdtKs.get(h);
    if (!k) {
      k = this.keys.db.crdt(tableId);
      this.crdtKs.set(h, k);
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
  cacheNode(id: string, n: Node, b?: Uint8Array): void {
    this.nodes.set(id, n);
    if (b) this.persisted?.nodes.put(bytes.fromHex(id), b);
    if (this.nodes.size > NODE_CACHE) {
      const old = this.nodes.keys().next().value!;
      this.nodes.delete(old);
      this.persisted?.nodes.delete(bytes.fromHex(old));
    }
  }

  /** @internal A node the local store kept, checked against its id (§9.3). */
  async keptNode(index: Uint8Array, id: Uint8Array): Promise<Node | undefined> {
    const p = this.persisted?.nodes;
    const b = p && (await p.get(id));
    if (!b) return undefined;
    try {
      if (!bytes.equal(this.indexKeys(index).nodeId(b), id)) throw new Error('another node');
      const n = decodeNode(b);
      this.cacheNode(bytes.hex(id), n);
      return n;
    } catch {
      p.delete(id);
      return undefined;
    }
  }

  /** @internal A sealed index's entries, if the cached blob has this digest. */
  cachedSealed(index: string, digest: string): Uint8Array[] | undefined {
    const c = this.sealed.get(index);
    return c?.digest === digest ? c.keys : undefined;
  }

  /** @internal */
  cacheSealed(index: string, digest: string, keys: Uint8Array[]): void {
    if (this.sealed.get(index)?.digest === digest) return;
    this.sealed.set(index, { digest, keys });
    this.persisted?.sealed.put(
      bytes.fromHex(index),
      encodeSealed(bytes.fromHex(digest), encodeBlob(keys)),
    );
  }

  /** @internal A sealed blob the local store kept, if it has this digest (checked, §9.3). */
  async keptSealed(index: string, digest: string): Promise<Uint8Array[] | undefined> {
    const p = this.persisted?.sealed;
    const b = p && (await p.get(bytes.fromHex(index)));
    if (!b) return undefined;
    try {
      const k = decodeSealed(b);
      if (bytes.hex(k.digest) !== digest) return undefined;
      if (bytes.hex(partsDigest(k.blob)) !== digest) throw new Error('a blob of another digest');
      const keys = decodeBlob(decode(k.blob));
      this.sealed.set(index, { digest, keys });
      return [...keys];
    } catch {
      p.delete(bytes.fromHex(index));
      return undefined;
    }
  }

  // ------------------------------------------------------------ catalog cache

  /** @internal */
  catalogEntry(name: string): CatalogEntry | undefined {
    return this.catalog.get(name);
  }

  /** @internal */
  catalogSet(name: string, record: TableRecord | undefined, version: Uint8Array | undefined) {
    this.catalog.set(name, { record, version });
    const p = this.persisted?.db;
    if (!p) return;
    if (record) p.put(tableKey(name), encodeTable(encodeTableRecord(record), version));
    else p.delete(tableKey(name));
  }

  /** @internal */
  catalogStale(name: string): void {
    this.catalog.delete(name);
    this.persisted?.db.delete(tableKey(name));
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
    private readonly opts: TableOptions = {},
  ) {}

  /** A call's transaction: on a CRDT table, a single entry of the queue (§19.7). */
  private one<R>(fn: (tt: TableTx<T>) => Promise<R>): Promise<R> {
    return this.db.transaction((tx) => fn(tx.table<T>(this.name)), {
      single: true,
      ...(this.opts.wait ? { wait: true } : {}),
    });
  }

  /** The row, or undefined; on a CRDT table, read as `read` says (§19.4). */
  get(pk: Value, opts: { read?: CrdtRead } = {}): Promise<T | undefined> {
    return this.db.transaction(
      (tx) => tx.table<T>(this.name).get(pk, opts),
      opts.read ? { read: opts.read } : {},
    );
  }

  insert(row: T): Promise<void> {
    return this.one((tt) => tt.insert(row));
  }

  put(row: T): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).put(row));
  }

  update(pk: Value, fn: (row: T) => T | Promise<T>): Promise<T> {
    return this.db.transaction((tx) => tx.table<T>(this.name).update(pk, fn));
  }

  delete(pk: Value): Promise<boolean> {
    return this.one((tt) => tt.delete(pk));
  }

  /** CRDT tables (§19.3): set `lww` fields; `undefined` unsets one. */
  patch(pk: Value, fields: Partial<T>): Promise<void> {
    return this.one((tt) => tt.patch(pk, fields));
  }

  /** CRDT tables: add `n` (default 1) to a counter. */
  incr(pk: Value, field: string, n: number | bigint = 1): Promise<void> {
    return this.one((tt) => tt.incr(pk, field, n));
  }

  /** CRDT tables: add elements to a set. */
  add(pk: Value, field: string, ...elems: Value[]): Promise<void> {
    return this.one((tt) => tt.add(pk, field, ...elems));
  }

  /** CRDT tables: remove the copies of elements seen now (add-wins). */
  remove(pk: Value, field: string, ...elems: Value[]): Promise<void> {
    return this.one((tt) => tt.remove(pk, field, ...elems));
  }

  /** Subscribe to this table's change events (§12.7). */
  async on(handler: OnHandler<ChangeBody<T>>, opts: OnOptions = {}): Promise<OnSubscription> {
    const t = await this.db.transaction((tx) => tx.tableRecord(this.name));
    if (!t.changes) throw new DbError('bad_value', `table ${this.name} has no change events`);
    return this.db.on(this.db.topic(...t.changes.topic), handler, opts);
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
