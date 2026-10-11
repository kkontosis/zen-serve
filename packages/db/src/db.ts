// The `Db` class (spec/zendb.md §2–3, §16): opening a database, the
// catalog cache, tables and transactions.
import {
  bytes,
  Transaction as RawTransaction,
  type Topic,
  type Transaction,
  type UnlockedFs,
  ZenError,
} from '@zen/client';
import type { CrdtKeys, IndexKeys, Limits } from '@zen/wasm';
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
  decodeDbRecord,
  decodeTableRecord,
  encodeDbRecord,
  encodeTableRecord,
  type TableRecord,
} from './catalog.js';
import type { Value } from './cbor.js';
import { DbError } from './errors.js';
import type { Node } from './index/node.js';
import { startIndexers } from './ixer.js';
import { Keys } from './keys.js';
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
import type { Fields } from './row.js';
import { Saga, type SagaOptions, type SagaStep } from './saga.js';
import { type ScheduleOptions, Scheduler, type SchedulerOptions } from './sched.js';
import { SYSTEM_TABLES, systemTable } from './system.js';
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
  private readonly crdtKs = new Map<string, CrdtKeys>();
  /** Sealed indexes by index id: the entries of the blob with this digest. */
  private readonly sealed = new Map<string, { digest: string; keys: Uint8Array[] }>();
  /** Rows by stored key (hex), least recently used first. */
  private readonly rows = new Map<string, CachedRow>();
  private owner: Owner | undefined;
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

  private constructor(
    readonly fs: UnlockedFs,
    readonly ns: string,
    readonly keys: Keys,
    /** The server's limits (`/v1/info`). */
    readonly limits: Limits,
    /** The server's features (`/v1/info`). */
    readonly features: string[],
    private readonly opts: DbOptions,
  ) {}

  /**
   * Open (or create) a database and bring it to `opts.schema` (§3.1):
   * an unknown format or integrity level is refused (`format`), a newer
   * stored schema too (`schema_newer`).
   */
  static async open(fs: UnlockedFs, ns: string, opts: DbOptions = {}): Promise<Db> {
    const info = await fs.session.client.info();
    const db = new Db(fs, ns, new Keys(fs.keys.db(ns)), info.limits, info.features, opts);
    await db.init();
    const locks =
      opts.locks === undefined
        ? (globalThis as { navigator?: { locks?: LockManagerLike } }).navigator?.locks
        : (opts.locks ?? undefined);
    const name = `zen/db/${fs.id}/${ns}`;
    db.owner = new Owner(opts.owner === false ? undefined : locks, name, opts.owner !== false);
    const BC = (globalThis as { BroadcastChannel?: new (n: string) => BroadcastChannelLike })
      .BroadcastChannel;
    const channel =
      opts.channel === undefined ? (BC ? (n: string) => new BC(n) : undefined) : opts.channel;
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

  private async init(): Promise<void> {
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
   * Free the database keys and caches, stop its consumers and
   * subscriptions, and hand the owner role over.
   */
  close(): void {
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

  /** CRDT tables (§19.3): set `lww` fields; `undefined` unsets one. */
  patch(pk: Value, fields: Partial<T>): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).patch(pk, fields));
  }

  /** CRDT tables: add `n` (default 1) to a counter. */
  incr(pk: Value, field: string, n: number | bigint = 1): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).incr(pk, field, n));
  }

  /** CRDT tables: add elements to a set. */
  add(pk: Value, field: string, ...elems: Value[]): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).add(pk, field, ...elems));
  }

  /** CRDT tables: remove the copies of elements seen now (add-wins). */
  remove(pk: Value, field: string, ...elems: Value[]): Promise<void> {
    return this.db.transaction((tx) => tx.table<T>(this.name).remove(pk, field, ...elems));
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
