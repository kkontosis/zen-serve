// The offline queue of CRDT writes (spec/zendb.md §19.7). CRDT-only
// transactions and single calls are queued as entries, visible to reads at
// once, and sent in batches: as few commits as the server's limits allow,
// each sent again with the same `commit_id` until its outcome is known.
// With a store that tabs share, only the owner sends (§9.2, §19.7).
import { bytes, commitTransaction, sendCommit, ZenError } from '@zen/client';
import type { Commit, CommitResult, CrdtOp } from '@zen/wasm';
import { encode, type Value } from './cbor.js';
import { actorLock, forgetTotals, type Pending } from './crdt.js';
import { isNetwork } from './crdtcache.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import type { BroadcastChannelLike } from './proxy.js';
import type { ManualTransaction } from './txn.js';

/** The queued operations on one row. `row` and `lww` carry the `hlc` of the edit. */
export interface QueueRow {
  table: string;
  tableId: Uint8Array;
  pk: Value;
  pkEl: Uint8Array;
  ops: Pending[];
}

/**
 * A queued write (§19.7): a CRDT-only transaction, sent whole in one
 * commit, or a single call (`single`), which may be coalesced with the
 * single calls next to it.
 */
export interface QueueEntry {
  id: string;
  single: boolean;
  rows: QueueRow[];
  /** When it was queued (ms). */
  at: number;
}

/**
 * The commit in flight (§19.7): built once, sent until its outcome is
 * known. Kept so a restart sends it again with the same `commit_id`.
 */
export interface Inflight {
  commit: Commit;
  /** The entries it carries. */
  ids: string[];
}

/**
 * Where queued entries are kept. An entry is in the store before its
 * write resolves, and leaves it once committed or failed for good. A
 * store that keeps the commit in flight too (`putInflight`) lets a restart
 * send it again as it was.
 */
export interface QueueStore {
  load(): Promise<QueueEntry[]>;
  put(e: QueueEntry): Promise<void>;
  remove(ids: string[]): Promise<void>;
  putInflight?(r: Inflight): Promise<void>;
  loadInflight?(): Promise<Inflight | undefined>;
  removeInflight?(): Promise<void>;
}

/** A store in memory: the queue lasts as long as the process. */
export function memoryQueueStore(): QueueStore {
  const m = new Map<string, QueueEntry>();
  let inflight: Inflight | undefined;
  return {
    load: async () => [...m.values()],
    put: async (e) => {
      m.set(e.id, e);
    },
    remove: async (ids) => {
      for (const id of ids) m.delete(id);
    },
    putInflight: async (r) => {
      inflight = r;
    },
    loadInflight: async () => inflight,
    removeInflight: async () => {
      inflight = undefined;
    },
  };
}

/** A message between the tabs that share a queue's store. */
type QueueMsg =
  | { t: 'queued' }
  | { t: 'flush' }
  | {
      t: 'sync';
      online: boolean;
      sent?: string[];
      failed?: { ids: string[]; code: string; message: string };
    };

/** What `onSync` reports. */
export interface SyncEvent {
  /** Whether the last attempt reached the server. */
  online: boolean;
  /** Entries committed by the batch just sent. */
  sent?: number;
  /** Entries that failed for good, and why. */
  failed?: { entries: number; error: unknown };
}

/** Commit-size margin for the catalog expects and the change events. */
const SLACK_OPS = 16;
const BACKOFF_MIN = 100;
const BACKOFF_MAX = 30_000;
/** `stale_op` / `clock_skew` re-stamps of one batch. */
const MAX_RESTAMPS = 3;

const rowKey = (tableId: Uint8Array, pkEl: Uint8Array) =>
  `${bytes.hex(tableId)}/${bytes.hex(pkEl)}`;

const elemKey = (r: QueueRow, field: string, elem: Value) =>
  `${rowKey(r.tableId, r.pkEl)}/${field}/${bytes.hex(encode(elem))}`;

const codeOf = (e: unknown) => (e instanceof ZenError || e instanceof DbError ? e.code : '');

/** An entry's size in a commit: operations (one change event per row) and bytes. */
function estimate(e: QueueEntry): { ops: number; bytes: number } {
  let ops = 0;
  let size = 0;
  for (const r of e.rows) {
    ops += r.ops.length + 1;
    size += 600;
    for (const o of r.ops) {
      const v = o.op === 'lww' ? o.value : o.op === 'add' || o.op === 'rem' ? o.elem : undefined;
      size += 600 + (v === undefined ? 16 : encode(v).length);
    }
  }
  return { ops, bytes: size };
}

/**
 * Coalesce the operations of a run of single calls on one row (§19.7):
 * the last `lww` of a field wins, counter deltas add up, and a `rem`
 * cancels the earlier `add`s of its element.
 */
function coalesce(into: Pending[], ops: Pending[]): void {
  for (const o of ops) {
    if (o.op === 'lww') {
      const i = into.findIndex((x) => x.op === 'lww' && x.field === o.field);
      if (i >= 0) into.splice(i, 1);
      into.push({ ...o });
    } else if (o.op === 'ctr') {
      const c = into.find((x) => x.op === 'ctr' && x.field === o.field);
      if (c && c.op === 'ctr') c.delta += o.delta;
      else into.push({ ...o });
    } else if (o.op === 'rem') {
      const e = bytes.hex(encode(o.elem));
      for (let i = into.length - 1; i >= 0; i--) {
        const x = into[i]!;
        if (x.op === 'add' && x.field === o.field && bytes.hex(encode(x.elem)) === e) {
          into.splice(i, 1);
        }
      }
      into.push({ ...o });
    } else into.push({ ...o });
  }
}

/**
 * The queue of one `Db` (§19.7): `db.crdt`. One sender sends its entries
 * in order.
 */
export class CrdtQueue {
  private entries: QueueEntry[] = [];
  private readonly waiters = new Map<string, { ok: () => void; fail: (e: unknown) => void }[]>();
  private readonly flushers: { ok: () => void; fail: (e: unknown) => void }[] = [];
  private readonly listeners = new Set<(e: SyncEvent) => void>();
  /** Dots the server gave to queued `add`s, by element, for later `rem`s. */
  private readonly learned = new Map<string, Uint8Array[]>();
  private sending: Promise<void> | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private wakeUp: (() => void) | undefined;
  private closed = false;
  private isOnline = true;
  private readonly offOnline: (() => void) | undefined;
  /** Whether this instance sends: always, or (`shared`) while it is the owner. */
  private sender: boolean;
  /** The stored commit in flight, sent first (§19.7). */
  private inflight: Inflight | undefined;
  /** The counter lock, taken at load for the commit in flight before any other commit stamps. */
  private held: Promise<() => void> | undefined;
  private lastAt = 0;

  /**
   * `shared`: the store is shared with other tabs, which only queue into
   * it; they hear about each other over `channel`, and only the owner
   * sends (`start`).
   */
  constructor(
    private readonly db: Db,
    private readonly store: QueueStore,
    private readonly batchMs: number,
    shared = false,
    private readonly channel?: BroadcastChannelLike,
  ) {
    this.sender = !shared;
    if (channel) channel.onmessage = (ev) => void this.onMessage(ev.data as QueueMsg);
    const g = globalThis as {
      addEventListener?: (t: string, f: () => void) => void;
      removeEventListener?: (t: string, f: () => void) => void;
    };
    if (g.addEventListener && g.removeEventListener) {
      const f = () => this.wake();
      g.addEventListener('online', f);
      this.offOnline = () => g.removeEventListener!('online', f);
    }
  }

  /** @internal Load the stored entries (at open) and, as a sender, start sending them. */
  async load(): Promise<void> {
    this.entries = (await this.store.load()).sort((a, b) => a.at - b.at);
    this.lastAt = this.entries.at(-1)?.at ?? 0;
    if (!this.sender) return;
    this.inflight = await this.store.loadInflight?.();
    if (this.inflight) this.held = actorLock(this.db);
    if (this.entries.length || this.inflight) this.schedule();
  }

  /**
   * @internal Become the sender of a shared store (the owner, §9.2): the
   * store holds what the other tabs queued, and the commit the last owner
   * had in flight.
   */
  async start(): Promise<void> {
    if (this.sender || this.closed) return;
    this.sender = true;
    await this.reload(true);
    this.inflight = await this.store.loadInflight?.();
    if (this.inflight) this.held = actorLock(this.db);
    if (this.entries.length || this.inflight) this.wake();
  }

  /**
   * Take in the entries other tabs stored. `all`: also forget entries no
   * longer stored (sent by an owner before this one).
   */
  private async reload(all: boolean): Promise<void> {
    const stored = await this.store.load();
    const ids = new Set(stored.map((e) => e.id));
    const have = new Set(this.entries.map((e) => e.id));
    if (all) {
      const gone = this.entries.filter((e) => !ids.has(e.id)).map((e) => e.id);
      if (gone.length) this.settled(gone);
    }
    const added = stored.filter((e) => !have.has(e.id));
    if (!added.length) return;
    this.entries = [...this.entries, ...added].sort((a, b) => a.at - b.at);
  }

  private async onMessage(m: QueueMsg): Promise<void> {
    if (this.closed) return;
    try {
      if (m.t === 'queued' || m.t === 'flush') {
        await this.reload(false);
        if (this.sender) this.wake();
        return;
      }
      if (m.t !== 'sync' || this.sender) return;
      // From the owner: what it sent, what failed, whether it is online.
      if (m.sent?.length) this.settled(m.sent);
      // The owner's error, as a `ZenError` with its code.
      const failed = m.failed && new ZenError(0, m.failed.code, m.failed.message);
      if (m.failed?.ids.length) this.settled(m.failed.ids, failed);
      if (m.online !== this.isOnline) {
        this.isOnline = m.online;
        if (!m.online) {
          const off = new DbError(
            'offline',
            'the server cannot be reached; the writes stay queued',
          );
          for (const f of this.flushers.splice(0)) f.fail(off);
        }
      }
      this.emit({
        online: m.online,
        ...(m.sent?.length ? { sent: m.sent.length } : {}),
        ...(m.failed?.ids.length
          ? { failed: { entries: m.failed.ids.length, error: failed } }
          : {}),
      });
    } catch (e) {
      this.db.warn(`zen-db: the queue store failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /** Entries another tab settled: out of this view, their writes resolved (or rejected). */
  private settled(ids: string[], error?: unknown): void {
    const set = new Set(ids);
    this.entries = this.entries.filter((e) => !set.has(e.id));
    for (const id of ids) {
      for (const w of this.waiters.get(id) ?? []) error ? w.fail(error) : w.ok();
      this.waiters.delete(id);
    }
    if (!this.entries.length) for (const f of this.flushers.splice(0)) f.ok();
  }

  private post(m: QueueMsg): void {
    try {
      this.channel?.postMessage(m);
    } catch {
      // closed
    }
  }

  /** Entries not yet committed. */
  get pending(): number {
    return this.entries.length;
  }

  /** Whether the last attempt to send reached the server. */
  get online(): boolean {
    return this.isOnline;
  }

  /**
   * Resolves once the queue is empty. Offline it rejects (`offline`) at the
   * next failed attempt; the entries stay queued.
   */
  flush(): Promise<void> {
    if (!this.entries.length) return Promise.resolve();
    if (this.closed) return Promise.reject(new DbError('closed', 'the database is closed'));
    const p = new Promise<void>((ok, fail) => this.flushers.push({ ok, fail }));
    if (this.sender) this.wake();
    else this.post({ t: 'flush' });
    return p;
  }

  /** Called after each batch sent, and when going offline or online. Returns an unsubscribe. */
  onSync(fn: (e: SyncEvent) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** @internal Queue an entry; resolves once it is stored. */
  async add(e: QueueEntry): Promise<void> {
    if (this.closed) throw new DbError('closed', 'the database is closed');
    const { ops, bytes: size } = estimate(e);
    const l = this.db.limits;
    if (ops + SLACK_OPS > l.max_commit_ops || size > l.max_commit_bytes) {
      throw new DbError('too_large', `a CRDT write of ${ops} operations, about ${size} bytes`);
    }
    // Queue order survives a restart: `at` never repeats within a tab.
    e.at = Math.max(e.at, this.lastAt + 0.001);
    this.lastAt = e.at;
    await this.store.put(e);
    this.entries.push(e);
    if (this.sender) this.schedule();
    else this.post({ t: 'queued' });
  }

  /** @internal Resolves when the entry is committed; rejects if it fails for good. */
  wait(id: string): Promise<void> {
    if (!this.entries.some((e) => e.id === id)) return Promise.resolve();
    return new Promise((ok, fail) => {
      const list = this.waiters.get(id) ?? [];
      list.push({ ok, fail });
      this.waiters.set(id, list);
    });
  }

  /** @internal The queued operations on a row, in queue order (to overlay on reads). */
  pendingOps(tableId: Uint8Array, pkEl: Uint8Array): Pending[] {
    const k = rowKey(tableId, pkEl);
    const out: Pending[] = [];
    for (const e of this.entries) {
      for (const r of e.rows) if (rowKey(r.tableId, r.pkEl) === k) out.push(...r.ops);
    }
    return out;
  }

  /** @internal The rows of a table with queued operations, by pk element (hex). */
  rowsOf(tableId: Uint8Array): Map<string, { pk: Value; pkEl: Uint8Array; ops: Pending[] }> {
    const out = new Map<string, { pk: Value; pkEl: Uint8Array; ops: Pending[] }>();
    const t = bytes.hex(tableId);
    for (const e of this.entries) {
      for (const r of e.rows) {
        if (bytes.hex(r.tableId) !== t) continue;
        const h = bytes.hex(r.pkEl);
        const x = out.get(h) ?? { pk: r.pk, pkEl: r.pkEl, ops: [] };
        x.ops.push(...r.ops);
        out.set(h, x);
      }
    }
    return out;
  }

  /** @internal Dots the server gave to this queue's `add`s of an element. */
  learnedDots(object: Uint8Array, field: Uint8Array, elem: Uint8Array): Uint8Array[] {
    return this.learned.get(`${bytes.hex(object)}/${bytes.hex(field)}/${bytes.hex(elem)}`) ?? [];
  }

  /** @internal Stop sending; the entries stay in the store. */
  close(): void {
    this.closed = true;
    void this.held?.then((r) => r());
    this.held = undefined;
    this.channel?.close();
    this.offOnline?.();
    if (this.timer) clearTimeout(this.timer);
    this.wakeUp?.();
    const closed = new DbError('closed', 'the database is closed');
    for (const f of this.flushers.splice(0)) f.fail(closed);
    for (const list of this.waiters.values()) for (const w of list) w.fail(closed);
    this.waiters.clear();
  }

  private wake(): void {
    if (this.wakeUp) {
      this.wakeUp();
      return;
    }
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    this.schedule(0);
  }

  private schedule(ms = this.batchMs): void {
    if (this.closed || !this.sender || this.sending || this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.sending = this.loop().finally(() => {
        this.sending = undefined;
        if ((this.entries.length || this.inflight) && !this.closed) this.schedule();
      });
    }, ms);
  }

  private emit(e: SyncEvent): void {
    for (const f of this.listeners) {
      try {
        f(e);
      } catch (err) {
        this.db.warn(`onSync: ${err instanceof Error ? err.message : String(err)}`);
      }
    }
  }

  private setOnline(on: boolean): void {
    if (on === this.isOnline) return;
    this.isOnline = on;
    this.emit({ online: on });
    this.post({ t: 'sync', online: on });
  }

  /** Offline: tell the flushers, then wait (a backoff, or a wake). */
  private async offline(attempt: number): Promise<void> {
    this.setOnline(false);
    const off = new DbError('offline', 'the server cannot be reached; the writes stay queued');
    for (const f of this.flushers.splice(0)) f.fail(off);
    const ms = Math.min(BACKOFF_MIN * 2 ** attempt, BACKOFF_MAX);
    await new Promise<void>((r) => {
      const t = setTimeout(done, ms);
      function done() {
        clearTimeout(t);
        r();
      }
      this.wakeUp = done;
    });
    this.wakeUp = undefined;
  }

  /** Send entries until the queue is empty, or the queue closes. */
  private async loop(): Promise<void> {
    let limit = Number.POSITIVE_INFINITY;
    let restamps = 0;
    let down = 0;
    if (this.inflight) {
      down = await this.resend(this.inflight);
      if (this.closed) return;
    }
    while (!this.closed && this.sender && this.entries.length) {
      const batch = this.pack(limit);
      let tx: ManualTransaction;
      try {
        tx = await this.build(batch);
      } catch (e) {
        if (isNetwork(e)) {
          await this.offline(down++);
          continue;
        }
        if (codeOf(e) === 'conflict') continue;
        if (codeOf(e) === 'too_large' && batch.length > 1) {
          limit = Math.ceil(batch.length / 2);
          continue;
        }
        await this.fail(batch, e);
        continue;
      }
      // Kept before the first send: a restart sends this very commit (§19.7).
      const commit = tx.raw.build();
      await this.store.putInflight?.({ commit, ids: batch.map((e) => e.id) }).catch((e) => {
        this.db.warn(
          `zen-db: the queue store failed: ${e instanceof Error ? e.message : String(e)}`,
        );
      });
      // The same commit until its outcome is known (api.md §6).
      let err: unknown;
      let result: CommitResult | undefined;
      for (let attempt = 1; ; attempt++) {
        try {
          result = await commitTransaction(tx.raw, { attempts: 1 });
          err = undefined;
          break;
        } catch (e) {
          err = e;
          if (isNetwork(e) && !this.closed) {
            await this.offline(down++);
            if (!this.closed) continue;
          }
          if (codeOf(e) === 'commit_unknown' && !this.closed) {
            await new Promise((r) =>
              setTimeout(r, Math.min(BACKOFF_MIN * 2 ** attempt, BACKOFF_MAX)),
            );
            if (!this.closed) continue;
          }
          // The stamp's hooks: release the actor's lock, sync the clock on a rebase.
          for (const h of tx.raw.errorHooks) await h(e, attempt).catch(() => false);
          break;
        }
      }
      // Closed before the outcome: the stored commit is sent at the next start.
      if (this.closed && err) return;
      tx.abort();
      down = 0;
      this.setOnline(true);
      if (!err) {
        await this.done(batch, commit.crdt_ops ?? [], result);
        limit = Number.POSITIVE_INFINITY;
        restamps = 0;
        continue;
      }
      // The server refused it: it never applied, so it is built again.
      await this.dropInflight();
      const code = codeOf(err);
      if (code === 'conflict' || code === 'too_old') continue;
      if ((code === 'stale_op' || code === 'clock_skew') && restamps++ < MAX_RESTAMPS) {
        await this.restamp(batch);
        continue;
      }
      if (code === 'too_large' && batch.length > 1) {
        limit = Math.ceil(batch.length / 2);
        continue;
      }
      await this.fail(batch, err);
    }
  }

  private async dropInflight(): Promise<void> {
    this.inflight = undefined;
    await this.store.removeInflight?.().catch((e) => {
      this.db.warn(`zen-db: the queue store failed: ${e instanceof Error ? e.message : String(e)}`);
    });
  }

  /**
   * Send the stored commit in flight (§19.7), as it was built, until its
   * outcome is known: committed, its entries are done; refused, it never
   * applied, and its entries are built again. The counter lock is held
   * meanwhile: this commit's `seq`s must reach the server before any later
   * one of this installation, or it would be ignored. Returns the offline
   * count, for the backoff.
   */
  private async resend(r: Inflight): Promise<number> {
    const db = this.db;
    let down = 0;
    const held = this.held ?? actorLock(db);
    this.held = undefined;
    const release = await held;
    let result: CommitResult | undefined;
    let err: unknown;
    try {
      for (let attempt = 1; ; attempt++) {
        if (this.closed) return down;
        try {
          result = await sendCommit(db.fs.session, r.commit, 1);
          break;
        } catch (e) {
          if (isNetwork(e)) {
            await this.offline(down++);
            continue;
          }
          if (codeOf(e) === 'commit_unknown') {
            await new Promise((ok) =>
              setTimeout(ok, Math.min(BACKOFF_MIN * 2 ** attempt, BACKOFF_MAX)),
            );
            continue;
          }
          err = e;
          break;
        }
      }
    } finally {
      // Whatever happened, the states and totals are read again from the server.
      const ops = r.commit.crdt_ops ?? [];
      db.crdtCache.markStale(ops.flatMap((o) => ('object' in o ? [o.object] : [])));
      forgetTotals(
        db,
        ops.flatMap((o) =>
          o.op === 'ctr' ? [`${bytes.hex(o.object)}/${bytes.hex(o.field)}`] : [],
        ),
      );
      release();
    }
    this.setOnline(true);
    const ids = new Set(r.ids);
    const batch = this.entries.filter((e) => ids.has(e.id));
    if (err) {
      db.warn(
        `zen-db: the commit in flight was refused (${codeOf(err) || String(err)}); its writes are sent again`,
      );
      await this.dropInflight();
      return 0;
    }
    await this.done(batch, r.commit.crdt_ops ?? [], result, r.ids);
    return 0;
  }

  /**
   * The next batch: entries in order while they fit the server's limits.
   * A batch stops before an entry that removes an element an earlier entry
   * of the batch adds (its dot exists only once that commit is in), unless
   * both are single calls of one run, which cancel out.
   */
  private pack(limit: number): QueueEntry[] {
    const l = this.db.limits;
    const out: QueueEntry[] = [];
    let ops = SLACK_OPS;
    let size = 0;
    const batchAdds = new Set<string>();
    const runAdds = new Set<string>();
    for (const e of this.entries) {
      if (out.length >= limit) break;
      const est = estimate(e);
      if (
        out.length &&
        (ops + est.ops > l.max_commit_ops || size + est.bytes > l.max_commit_bytes)
      ) {
        break;
      }
      if (!e.single) {
        for (const k of runAdds) batchAdds.add(k);
        runAdds.clear();
      }
      const adds: string[] = [];
      let cut = false;
      for (const r of e.rows) {
        for (const o of r.ops) {
          if (o.op === 'add') adds.push(elemKey(r, o.field, o.elem));
          if (o.op === 'rem' && batchAdds.has(elemKey(r, o.field, o.elem))) cut = true;
        }
      }
      if (cut && out.length) break;
      for (const k of adds) (e.single ? runAdds : batchAdds).add(k);
      out.push(e);
      ops += est.ops;
      size += est.bytes;
    }
    return out;
  }

  /** Build a batch's commit: its operations (runs of single calls coalesced) and change events. */
  private async build(batch: QueueEntry[]): Promise<ManualTransaction> {
    const db = this.db;
    // Runs of single calls become one row list each; whole entries stay whole.
    const groups: QueueRow[][] = [];
    let run: Map<string, QueueRow> | undefined;
    for (const e of batch) {
      if (!e.single) {
        run = undefined;
        groups.push(e.rows);
        continue;
      }
      if (!run) {
        run = new Map();
        groups.push([]);
      }
      const g = groups.at(-1)!;
      for (const r of e.rows) {
        const k = rowKey(r.tableId, r.pkEl);
        let x = run.get(k);
        if (!x) {
          x = { ...r, ops: [] };
          run.set(k, x);
          g.push(x);
        }
        coalesce(x.ops, r.ops);
      }
    }
    // Long mode: catalog reads become `expect`s, so the commit carries no
    // read version and stays valid however long it waits to be sent.
    const tx = db.begin({ mode: 'long' });
    const alive = new Map<string, { row: QueueRow; alive: boolean }>();
    for (const rows of groups) {
      for (const r of rows) {
        // The table must still be the one queued: dropped or replaced, its operations go.
        const t = await tx.tableRecordOrNone(r.table);
        if (t?.state !== 'active' || t.merge !== 'crdt' || !bytes.equal(t.id, r.tableId)) {
          db.warn(`zen-db: queued writes to ${r.table}, which is gone; dropped`);
          continue;
        }
        tx.crdt.load(t, r.pk, r.pkEl, r.ops);
        if (!t.changes) continue;
        const k = rowKey(r.tableId, r.pkEl);
        const last = r.ops.findLast((o) => o.op === 'row');
        const prev = alive.get(k);
        alive.set(k, {
          row: r,
          alive: last ? last.op === 'row' && last.alive : (prev?.alive ?? true),
        });
        tx.noteChange(t, r.pk, r.pkEl, {}, alive.get(k)!.alive ? {} : undefined);
      }
    }
    try {
      await tx.finish();
    } catch (e) {
      tx.abort();
      throw e;
    }
    return tx;
  }

  /** A batch committed: learn the dots of its `add`s, and resolve its writes. */
  private async done(
    batch: QueueEntry[],
    ops: CrdtOp[],
    result: CommitResult | undefined,
    ids = batch.map((e) => e.id),
  ): Promise<void> {
    if (result) {
      const adds = ops.filter((o) => o.op === 'add');
      adds.forEach((o, i) => {
        const dot = result.set_dots?.[i];
        if (!dot || o.op !== 'add') return;
        const k = `${bytes.hex(o.object)}/${bytes.hex(o.field)}/${bytes.hex(o.elem)}`;
        this.learned.delete(k);
        this.learned.set(k, [...(this.learned.get(k) ?? []), dot]);
      });
      while (this.learned.size > 4096) this.learned.delete(this.learned.keys().next().value!);
    }
    await this.settle(ids);
    for (const id of ids) for (const w of this.waiters.get(id) ?? []) w.ok();
    for (const id of ids) this.waiters.delete(id);
    this.emit({ online: true, sent: batch.length });
    this.post({ t: 'sync', online: true, sent: ids });
    if (!this.entries.length) for (const f of this.flushers.splice(0)) f.ok();
  }

  /** A batch failed for good: its entries leave the queue, and their writes reject. */
  private async fail(batch: QueueEntry[], error: unknown): Promise<void> {
    this.db.warn(
      `zen-db: ${batch.length} queued CRDT writes failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    const ids = batch.map((e) => e.id);
    await this.settle(ids);
    for (const id of ids) for (const w of this.waiters.get(id) ?? []) w.fail(error);
    for (const id of ids) this.waiters.delete(id);
    this.emit({ online: this.isOnline, failed: { entries: batch.length, error } });
    this.post({
      t: 'sync',
      online: this.isOnline,
      failed: {
        ids,
        code: codeOf(error) || 'failed',
        message: error instanceof Error ? error.message : String(error),
      },
    });
    if (!this.entries.length) for (const f of this.flushers.splice(0)) f.ok();
  }

  /** Entries done with: out of the queue and the store, with the commit in flight. */
  private async settle(ids: string[]): Promise<void> {
    const set = new Set(ids);
    this.entries = this.entries.filter((e) => !set.has(e.id));
    this.inflight = undefined;
    try {
      await this.store.remove(ids);
      await this.store.removeInflight?.();
    } catch (e) {
      this.db.warn(`zen-db: the queue store failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /**
   * After `stale_op` / `clock_skew` (§19.7): fresh timestamps for the
   * batch's `row` and `lww` operations, from the clock just synced. The
   * edits then win over the ones made since they were queued.
   */
  private async restamp(batch: QueueEntry[]): Promise<void> {
    const clock = this.db.fs.session.clock;
    for (const e of batch) {
      for (const r of e.rows) {
        for (const o of r.ops) if (o.op === 'row' || o.op === 'lww') o.hlc = clock.tick();
      }
      await this.store.put(e);
    }
  }
}
