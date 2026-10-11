// Delayed messages and `gc` jobs (spec/zendb.md §12.4): `$sched` rows, and
// the scheduler that delivers them while it leads.
import { bytes, isCode, type Leader, type Topic, ZenError } from '@zen/client';
import { pathBytes, sealMsg } from './broker.js';
import type { Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import type { Element } from './keys.js';
import type { Msg } from './msg.js';
import type { DbTransaction } from './txn.js';

/** When a scheduled message or job is due: unix ms, or a delay from now. */
export interface When {
  /** Unix ms, by the session clock. */
  at?: number;
  /** Milliseconds from now. */
  delayMs?: number;
}

/** Options of `schedule`. */
export interface ScheduleOptions extends When {
  /** The event key of the delivered message. */
  key?: Uint8Array | string;
  /** The message id (default: random); also the id `cancel` takes. */
  id?: Uint8Array;
  /** App metadata. Names starting with `zen.` are reserved. */
  headers?: Record<string, string>;
}

/** Options of the scheduler (`DbOptions.schedulerOptions`). */
export interface SchedulerOptions {
  /** The longest sleep between reads of `$sched` (default 1,000 ms). */
  pollMs?: number;
  /** The leader lease's lifetime (default 10,000 ms): a takeover after a crash waits this long. */
  ttlMs?: number;
  /** Rows per transaction (default 100). */
  batch?: number;
}

/** A `$sched` row (§11.5). */
interface SchedRow {
  [field: string]: Value;
  id: Uint8Array;
  at: number;
  kind: 'emit' | 'gc';
}

/** How often the leader corrects its clock by the server's. */
const SYNC_EVERY = 10 * 60_000;

/** @internal The due time of `when`, by the session clock. */
export function dueAt(db: Db, when: When): number {
  if (when.at !== undefined) {
    if (!Number.isSafeInteger(when.at) || when.at < 0) throw new DbError('bad_value', 'bad at');
    return when.at;
  }
  const d = when.delayMs ?? 0;
  if (!Number.isFinite(d) || d < 0) throw new DbError('bad_value', 'bad delayMs');
  return Math.ceil(db.fs.session.clock.now() + d);
}

/**
 * @internal Schedule a Msg (§12.4): a `$sched` row holding the encoded
 * message, delivered at `at` with `deliver_at = at`. A large body goes to
 * parts now, in this transaction.
 */
export async function scheduleMsg(
  tx: DbTransaction,
  topic: Topic,
  m: Msg,
  key: Uint8Array | string | undefined,
  at: number,
): Promise<void> {
  // The scheduler appends with no causation.
  const { payload } = sealMsg(tx, { ...m, deliverAt: at }, undefined, 0);
  await tx.table('$sched').insert({
    id: m.id,
    at,
    kind: 'emit',
    topic: pathBytes(topic),
    ...(key !== undefined ? { key: typeof key === 'string' ? bytes.utf8(key) : key } : {}),
    msg: payload,
  });
  tx.raw.onCommit(() => tx.db.scheduler?.wake(at));
}

/**
 * @internal A `gc` job: at `at`, delete each path (elements under the
 * database's `D`) and everything under it.
 */
export async function scheduleGc(tx: DbTransaction, paths: Element[][], at: number): Promise<void> {
  await tx.table('$sched').insert({
    id: bytes.randomBytes(16),
    at,
    kind: 'gc',
    gc: paths.map((p) => p.map((e) => (typeof e === 'string' ? bytes.utf8(e) : e))),
  });
  tx.raw.onCommit(() => tx.db.scheduler?.wake(at));
}

const sleep = (ms: number, signal: AbortSignal, wake: (fn: () => void) => void) =>
  new Promise<void>((r) => {
    const done = () => {
      clearTimeout(t);
      r();
    };
    const t = setTimeout(done, ms);
    signal.addEventListener('abort', done);
    wake(done);
  });

/**
 * The scheduler (§12.4). It runs in the owner (§9.2) and delivers while it
 * holds the lease of the group `"sched"` on `("zen", "db", ns, "sched")`.
 * The lease only saves duplicate work: each row is emitted in the
 * transaction that deletes it, so two schedulers conflict and one retries.
 */
export class Scheduler {
  private ctl: AbortController | undefined;
  private loop: Promise<void> | undefined;
  private leader: Leader | undefined;
  private batch: number;
  private synced = 0;
  private next = Number.POSITIVE_INFINITY;
  private waker: (() => void) | undefined;

  /** @internal */
  constructor(
    private readonly db: Db,
    private readonly opts: SchedulerOptions = {},
  ) {
    this.batch = this.max;
  }

  private get max(): number {
    return Math.max(1, this.opts.batch ?? 100);
  }

  private get ttl(): number {
    return this.opts.ttlMs ?? 10_000;
  }

  /** Whether it holds the lease now. */
  get leading(): boolean {
    return this.leader?.isLeader ?? false;
  }

  /** Whether it runs (started and not stopped). */
  get running(): boolean {
    return this.ctl !== undefined;
  }

  /** Start (the owner does this, §9.2). */
  start(): void {
    if (this.ctl) return;
    const ctl = new AbortController();
    this.ctl = ctl;
    this.loop = this.run(ctl.signal);
  }

  /**
   * Stop. `release: false` drops the lease without releasing it, so
   * another scheduler takes over only when it lapses (a crash, in tests).
   */
  async stop(opts: { release?: boolean } = {}): Promise<void> {
    const ctl = this.ctl;
    if (!ctl) return;
    this.ctl = undefined;
    ctl.abort();
    await this.loop;
    const l = this.leader;
    this.leader = undefined;
    if (!l) return;
    if (opts.release === false) l.consumer.forget();
    else await l.stop().catch(() => {});
  }

  /** @internal A row due at `at` was committed here: wake up if that is before the next read. */
  wake(at: number): void {
    if (at < this.next) this.waker?.();
  }

  private async run(signal: AbortSignal): Promise<void> {
    let backoff = 100;
    while (!signal.aborted) {
      try {
        if (!(await this.lead(signal))) {
          await sleep(Math.max(50, this.ttl / 3), signal, () => {});
          continue;
        }
        const session = this.db.fs.session;
        if (Date.now() - this.synced > SYNC_EVERY) {
          await session.clock.sync(session.client, true);
          this.synced = Date.now();
        }
        const n = await this.runOnce();
        backoff = 100;
        if (n >= this.batch || signal.aborted) continue;
        const next = await this.nextAt();
        const now = session.clock.now();
        const poll = this.opts.pollMs ?? 1000;
        this.next = next ?? Number.POSITIVE_INFINITY;
        const ms = Math.max(0, Math.min((next ?? now + poll) - now, poll));
        await sleep(ms, signal, (fn) => {
          this.waker = fn;
        });
        this.waker = undefined;
        this.next = Number.POSITIVE_INFINITY;
      } catch (e) {
        if (signal.aborted) break;
        this.db.warn(`scheduler: ${e instanceof Error ? e.message : String(e)}`);
        await sleep(backoff, signal, () => {});
        backoff = Math.min(backoff * 2, 5000);
      }
    }
  }

  /** Take (or keep) the lease. */
  private async lead(signal: AbortSignal): Promise<boolean> {
    if (!this.leader) {
      const t = this.db.topic('zen', 'db', this.db.ns, 'sched');
      const name = t.keysAt(t.fs.keys.epoch).groupId('sched');
      this.leader = await t.leader(name, { ttlMs: this.ttl });
    }
    if (signal.aborted) return false;
    return this.leader.isLeader || (await this.leader.acquire());
  }

  /** The earliest `at` of the rows left, if any. */
  private async nextAt(): Promise<number | undefined> {
    const [r] = await this.db.transaction((tx) =>
      tx.table<SchedRow>('$sched').query().orderBy('at').limit(1).all(),
    );
    return r?.at;
  }

  /**
   * @internal One transaction: deliver the rows due now (up to the batch)
   * and delete them. Returns how many. A commit too large halves the batch.
   */
  async runOnce(): Promise<number> {
    const now = this.db.fs.session.clock.now();
    try {
      const n = await this.db.transaction(async (tx) => {
        const st = tx.table<SchedRow>('$sched');
        const rows = await st.query().where('at', '<=', now).orderBy('at').limit(this.batch).all();
        for (const r of rows) {
          if (r.kind === 'emit') {
            const topic = this.db.topic(...(r.topic as Uint8Array[]));
            topic.appendIn(tx.raw, r.msg as Uint8Array, {
              ...(r.key instanceof Uint8Array ? { key: r.key } : {}),
            });
          } else {
            for (const p of (r.gc as Uint8Array[][]) ?? []) {
              const [begin, end] = tx.keys.range(...p);
              tx.raw.clearRange(begin, end);
            }
          }
          await st.delete(r.id);
        }
        return rows.length;
      });
      this.batch = Math.min(this.max, this.batch * 2);
      return n;
    } catch (e) {
      const big = (e instanceof DbError || e instanceof ZenError) && e.code === 'too_large';
      if (big && this.batch > 1) {
        this.batch = Math.ceil(this.batch / 2);
        return 0;
      }
      if (isCode(e, 'conflict')) return 0;
      throw e;
    }
  }
}
