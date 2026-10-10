// Topics: the encrypted event log, consumer groups and leaders
// (spec/api.md §7–8, DESIGN-3 §3, DESIGN-4 §1).
import type {
  Consume,
  DlqItem,
  EventBody,
  GroupDef,
  Mode,
  OnPoison,
  Start,
  TopicKeys,
  Delivery as WireDelivery,
} from '@zen/wasm';
import { equal, randomBytes, utf8 } from './bytes.js';
import { isCode, ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import { type Path, sendCommit, type Transaction } from './kv.js';
import type { Session } from './session.js';
import { sleep } from './transport.js';
import { zw } from './wasm.js';

/** A decrypted event. */
export interface TopicEvent {
  /** The event's 12-byte offset. */
  offset: Uint8Array;
  /** The event key's token, for keyed events. */
  keyToken?: Uint8Array;
  /** The sending device's fingerprint (32 zero bytes for an API token), as the sender sealed it. */
  sender: Uint8Array;
  /** The sender's hybrid logical clock. */
  hlc: bigint;
  /** The causing event's id (empty for none). */
  causation: Uint8Array;
  payload: Uint8Array;
}

/** Options of an append. */
export interface AppendOptions {
  /** The event key: per-key order and filtering (DESIGN-4 §1.1). Only its token reaches the server. */
  key?: Uint8Array | string;
  /** The causing event's id, e.g. the offset of the event being processed. */
  causation?: Uint8Array;
}

/** Options of `Topic.read`. */
export interface ReadOptions {
  /** Start strictly after this offset (default: the beginning). */
  after?: Uint8Array;
  /** Only this key's events. */
  key?: Uint8Array | string;
  /** Stop after this many events. */
  limit?: number;
}

const ZERO32 = new Uint8Array(32);
const NO_CAUSE: Uint8Array = new Uint8Array(0);
// Ephemeral messages are sealed as events of this reserved key, so that the
// server can neither replay a log event as an ephemeral message nor slip an
// ephemeral message into the log: the log refuses events of this key.
const EPHEMERAL_KEY = utf8('\u0000zen/v1/ephemeral');

/** The device fingerprint a session's events carry. */
export function senderOf(session: Session): Uint8Array {
  return session.deviceFp ?? ZERO32;
}

const bytesOf = (k: Uint8Array | string): Uint8Array => (typeof k === 'string' ? utf8(k) : k);

/**
 * A topic of an fs, by path. Its id and key tokens come from the naming key
 * (stable across epochs); its event keys from the data key of each epoch.
 */
export class Topic {
  private readonly segments: Uint8Array[];
  private readonly byEpoch = new Map<number, TopicKeys>();
  /** The topic id the server sees (16 bytes per path element). */
  readonly id: Uint8Array;

  constructor(
    readonly fs: UnlockedFs,
    readonly path: Path,
  ) {
    this.segments = path.map(bytesOf);
    this.id = this.keysAt(fs.keys.epoch).id;
  }

  /** The topic keys of an epoch. */
  keysAt(epoch: number): TopicKeys {
    let k = this.byEpoch.get(epoch);
    if (!k) {
      k = this.fs.keysAt(epoch).topic(this.segments);
      this.byEpoch.set(epoch, k);
    }
    return k;
  }

  /** A child topic. */
  child(...path: (string | Uint8Array)[]): Topic {
    return new Topic(this.fs, [...this.path, ...path]);
  }

  /** The 16-byte token of an event key (scoped to this topic). */
  keyToken(key: Uint8Array | string): Uint8Array {
    return this.keysAt(this.fs.keys.epoch).eventKeyToken(bytesOf(key));
  }

  /** Free the derived keys (the fs's keys stay; `fs.close()` frees those). */
  close(): void {
    for (const k of this.byEpoch.values()) k.free();
    this.byEpoch.clear();
  }

  /** Seal an event body under the current epoch. */
  seal(keyToken: Uint8Array | undefined, payload: Uint8Array, causation = NO_CAUSE): Uint8Array {
    const keys = this.fs.sealKeys();
    const body: EventBody = {
      sender: senderOf(this.fs.session),
      hlc: this.fs.session.clock.tick(),
      causation,
      payload,
    };
    return keys.sealEvent(this.keysAt(keys.epoch), keyToken, body);
  }

  /** Open a sealed event of this topic (with the keys of its epoch). */
  open(keyToken: Uint8Array | undefined, envelope: Uint8Array): EventBody {
    const keys = this.fs.keysFor(envelope);
    let body: EventBody;
    try {
      body = keys.openEvent(this.keysAt(keys.epoch), keyToken, envelope);
    } catch (e) {
      throw new ZenError(0, 'decrypt', e instanceof Error ? e.message : String(e));
    }
    this.fs.session.clock.observeUnchecked(body.hlc);
    return body;
  }

  /** Decrypt a stored event. Throws `decrypt` for one that doesn't open. */
  decode(e: { offset: Uint8Array; key_token?: Uint8Array; envelope: Uint8Array }): TopicEvent {
    if (e.key_token && equal(e.key_token, this.ephemeralToken())) {
      throw new ZenError(0, 'decrypt', 'an ephemeral message in the log');
    }
    const b = this.open(e.key_token, e.envelope);
    return {
      offset: e.offset,
      ...(e.key_token ? { keyToken: e.key_token } : {}),
      sender: b.sender,
      hlc: b.hlc,
      causation: b.causation,
      payload: b.payload,
    };
  }

  /** The reserved key token ephemeral messages are sealed under (see `Stream.publish`). */
  ephemeralToken(): Uint8Array {
    return this.keyToken(EPHEMERAL_KEY);
  }

  private append1(payload: Uint8Array, opts: AppendOptions) {
    const keyToken = opts.key === undefined ? undefined : this.keyToken(opts.key);
    return {
      fs: this.fs.id,
      topic: this.id,
      ...(keyToken ? { key_token: keyToken } : {}),
      envelope: this.seal(keyToken, payload, opts.causation),
    };
  }

  /** Append one event; returns its offset. */
  async append(payload: Uint8Array, opts: AppendOptions = {}): Promise<Uint8Array> {
    const r = await this.fs.session.call(
      '/v1/log/append',
      zw.encodeLogAppend,
      zw.decodeCommitResult,
      { commit_id: randomBytes(16), append: [this.append1(payload, opts)] },
    );
    return r.appended[0]!;
  }

  /**
   * Append inside a transaction: the event exists only if the transaction
   * commits. Its offset is in `tx.result.appended` afterwards.
   */
  appendIn(tx: Transaction, payload: Uint8Array, opts: AppendOptions = {}): void {
    tx.appends.push(this.append1(payload, opts));
  }

  /** The events after `after` (all, or one key's), decrypted, paging as needed. */
  async *read(opts: ReadOptions = {}): AsyncGenerator<TopicEvent> {
    const keyToken = opts.key === undefined ? undefined : this.keyToken(opts.key);
    let after = opts.after;
    let left = opts.limit ?? Number.POSITIVE_INFINITY;
    while (left > 0) {
      const r = await this.fs.session.call('/v1/log/read', zw.encodeLogRead, zw.decodeLogEvents, {
        fs: this.fs.id,
        topic: this.id,
        ...(after ? { after } : {}),
        ...(keyToken ? { key_token: keyToken } : {}),
        ...(Number.isFinite(left) ? { limit: left } : {}),
      });
      for (const e of r.events) {
        yield this.decode(e);
        left--;
      }
      const last = r.events.at(-1);
      if (!r.more || !last) return;
      after = last.offset;
    }
  }

  /**
   * Create (or find, if identical) a consumer group of this topic (§8.1).
   * Groups are immutable, and their names are per fs (not per topic) and
   * visible to the server.
   */
  async group(name: Uint8Array | string, def: GroupOptions): Promise<Consumer> {
    const wire: GroupDef = {
      fs: this.fs.id,
      group: bytesOf(name),
      topic: this.id,
      mode: def.mode,
      ...(def.partitions !== undefined ? { partitions: def.partitions } : {}),
      ...(def.key !== undefined ? { key_token: this.keyToken(def.key) } : {}),
      ...(def.maxInflight !== undefined ? { max_inflight: def.maxInflight } : {}),
      ...(def.maxAttempts !== undefined ? { max_attempts: def.maxAttempts } : {}),
      ...(def.onPoison !== undefined ? { on_poison: def.onPoison } : {}),
      ...(def.start !== undefined ? { start: def.start } : {}),
    };
    await this.fs.session.call(
      '/v1/consume/groups',
      zw.encodeGroupDef,
      zw.decodeGroupCreated,
      wire,
    );
    return new Consumer(this, wire.group, def.mode);
  }

  /** A leader election on this topic: a `sequential` group whose lease is the leadership (§8.2). */
  async leader(name: Uint8Array | string, opts: LeaderOptions = {}): Promise<Leader> {
    return new Leader(await this.group(name, { mode: 'sequential' }), opts);
  }
}

/** A consumer group's definition (api.md §8.1). */
export interface GroupOptions {
  mode: Mode;
  /** `partitioned`: the partition count, 1..=256. */
  partitions?: number;
  /** `single_key`: the key. */
  key?: Uint8Array | string;
  /** Events handed out per `next` (default 1). */
  maxInflight?: number;
  /** Attempts before poison handling (default 5; 0 = unlimited). */
  maxAttempts?: number;
  /** Poison handling (default `dlq`). */
  onPoison?: OnPoison;
  /** Where the group starts (default `earliest`). */
  start?: Start;
}

/** A delivered event, decrypted, with what its consume step needs. */
export interface Delivery extends TopicEvent {
  /** The cursor the consume step presents. */
  from: Uint8Array;
  /** The lease or claim token (the fencing token). */
  token: bigint;
  /** Failed attempts so far. */
  attempts: number;
  /** The partition it was delivered from (lease modes). */
  partition?: number;
  /** Set when the envelope doesn't open (`payload` is then empty): nack or ack it. */
  error?: ZenError;
  /** The sealed event as stored. */
  envelope: Uint8Array;
}

/** Options of `Consumer.next`. */
export interface NextOptions {
  /** Max events (also capped by the group's `max_inflight` in lease modes). */
  limit?: number;
  /** Long-poll: wait up to this long (≤ 30 000 ms) for an event. */
  waitMs?: number;
  /** `partitioned`: the partition. */
  partition?: number;
}

/** Options of `Consumer.lease`. */
export interface LeaseOptions {
  partition?: number;
  /** Lease lifetime (default 10 000 ms; the server clamps it to 100..=600 000). */
  ttlMs?: number;
}

interface Held {
  token: bigint;
  ttlMs: number;
  /** When it was last acquired or renewed (local ms). */
  at: number;
}

const DEFAULT_TTL = 10_000;

/**
 * A consumer of a group. Lease modes (`sequential`, `partitioned`,
 * `single_key`) need the partition's lease (`lease`) before `next`;
 * `per_key` deliveries carry their own claim. Each delivery is acknowledged
 * by a consume step (`ack`), alone or inside a transaction, or failed
 * (`nack`); its successor isn't delivered before that.
 */
export class Consumer {
  private readonly leases = new Map<number, Held>();
  /** Dead-letter queue operations. */
  readonly dlq: Dlq;

  constructor(
    readonly topic: Topic,
    /** The group name. */
    readonly group: Uint8Array,
    readonly mode: Mode,
  ) {
    this.dlq = new Dlq(this);
  }

  private get session(): Session {
    return this.topic.fs.session;
  }

  private get fsId(): number {
    return this.topic.fs.id;
  }

  /** Whether this mode uses leases. */
  get leased(): boolean {
    return this.mode !== 'per_key' && this.mode !== 'broadcast';
  }

  /** The lease token held for a partition, if any. */
  token(partition?: number): bigint | undefined {
    return this.leases.get(partition ?? 0)?.token;
  }

  /** When the partition's lease was last acquired or renewed (local ms), if held. */
  leasedAt(partition?: number): number | undefined {
    return this.leases.get(partition ?? 0)?.at;
  }

  /** Forget a lease without releasing it (it lapses on the server). */
  forget(partition?: number): void {
    this.leases.delete(partition ?? 0);
  }

  /**
   * Acquire the partition's lease, or renew it if held. Returns the fencing
   * token. Fails with 412 `not_leader` while another device holds it (or
   * after this one lost it, which forgets the token).
   */
  async lease(opts: LeaseOptions = {}): Promise<bigint> {
    const p = opts.partition ?? 0;
    const held = this.leases.get(p);
    const ttlMs = opts.ttlMs ?? held?.ttlMs ?? DEFAULT_TTL;
    try {
      const r = await this.session.call(
        '/v1/consume/lease',
        zw.encodeLeaseRequest,
        zw.decodeLease,
        {
          fs: this.fsId,
          group: this.group,
          ...(opts.partition !== undefined ? { partition: opts.partition } : {}),
          ...(held ? { token: held.token } : {}),
          ttl_ms: ttlMs,
        },
      );
      this.leases.set(p, { token: r.token, ttlMs, at: Date.now() });
      return r.token;
    } catch (e) {
      if (isCode(e, 'not_leader')) this.leases.delete(p);
      throw e;
    }
  }

  /** Release the partition's lease, so another device can take it at once. */
  async release(partition?: number): Promise<void> {
    const p = partition ?? 0;
    const held = this.leases.get(p);
    if (!held) return;
    this.leases.delete(p);
    await this.session.call('/v1/consume/release', zw.encodeLeaseRelease, zw.decodeEmpty, {
      fs: this.fsId,
      group: this.group,
      ...(partition !== undefined ? { partition } : {}),
      token: held.token,
    });
  }

  /** The next deliveries (none if nothing is ready by the end of `waitMs`). */
  async next(opts: NextOptions = {}): Promise<Delivery[]> {
    let token: bigint | undefined;
    if (this.leased) {
      token = this.token(opts.partition);
      if (token === undefined) throw new ZenError(0, 'no_lease', 'take the lease first (`lease`)');
    }
    const r = await this.session.call(
      '/v1/consume/next',
      zw.encodeNextRequest,
      zw.decodeDeliveries,
      {
        fs: this.fsId,
        group: this.group,
        ...(opts.partition !== undefined ? { partition: opts.partition } : {}),
        ...(token !== undefined ? { token } : {}),
        ...(opts.limit !== undefined ? { limit: opts.limit } : {}),
        ...(opts.waitMs !== undefined ? { wait_ms: opts.waitMs } : {}),
      },
    );
    return r.events.map((d) => this.toDelivery(d, opts.partition));
  }

  private toDelivery(d: WireDelivery, partition?: number): Delivery {
    const meta = {
      from: d.from,
      token: d.token,
      attempts: d.attempts,
      envelope: d.envelope,
      ...(partition !== undefined ? { partition } : {}),
    };
    try {
      return { ...this.topic.decode(d), ...meta };
    } catch (e) {
      const error = e instanceof ZenError ? e : new ZenError(0, 'decrypt', String(e));
      return {
        offset: d.offset,
        ...(d.key_token ? { keyToken: d.key_token } : {}),
        sender: ZERO32,
        hlc: 0n,
        causation: NO_CAUSE,
        payload: new Uint8Array(0),
        error,
        ...meta,
      };
    }
  }

  /** The consume step that acknowledges a delivery (api.md §8.3). */
  consumeStep(d: Delivery): Consume {
    return {
      fs: this.fsId,
      group: this.group,
      ...(d.partition !== undefined ? { partition: d.partition } : {}),
      ...(this.mode === 'per_key' && d.keyToken ? { key_token: d.keyToken } : {}),
      from: d.from,
      to: d.offset,
      token: d.token,
    };
  }

  /**
   * Acknowledge a delivery: the cursor moves past it. With `tx`, the step
   * joins that transaction's commit, so the transaction's writes and appends
   * land if and only if the event is consumed (exactly-once processing); a
   * stale token or a moved cursor then fails the whole commit with 412.
   */
  async ack(d: Delivery, tx?: Transaction): Promise<void> {
    if (tx) {
      tx.extra.consume ??= [];
      tx.extra.consume.push(this.consumeStep(d));
      return;
    }
    await sendCommit(this.session, { commit_id: randomBytes(16), consume: [this.consumeStep(d)] });
  }

  /**
   * Fail a delivery: it is redelivered, or dead-lettered once it reaches
   * `max_attempts` (then the cursor moves past it).
   */
  async nack(d: Delivery): Promise<{ attempts: number; deadLettered: boolean }> {
    const r = await this.session.call('/v1/consume/nack', zw.encodeNack, zw.decodeNackResult, {
      fs: this.fsId,
      group: this.group,
      ...(d.partition !== undefined ? { partition: d.partition } : {}),
      ...(this.mode === 'per_key' && d.keyToken ? { key_token: d.keyToken } : {}),
      offset: d.offset,
      token: d.token,
    });
    return { attempts: r.attempts, deadLettered: r.dead_lettered };
  }

  /**
   * The committed cursor: of a partition, of a key (`per_key`, with `key`),
   * or a `per_key` group's start, with its low watermark.
   */
  async cursor(
    opts: { partition?: number; key?: Uint8Array | string } = {},
  ): Promise<{ cursor: Uint8Array; lowWatermark?: Uint8Array }> {
    const r = await this.session.call('/v1/consume/cursor', zw.encodeGroupRef, zw.decodeCursor, {
      fs: this.fsId,
      group: this.group,
      ...(opts.partition !== undefined ? { partition: opts.partition } : {}),
      ...(opts.key !== undefined ? { key_token: this.topic.keyToken(opts.key) } : {}),
    });
    return { cursor: r.cursor, ...(r.low_watermark ? { lowWatermark: r.low_watermark } : {}) };
  }

  /**
   * Deliveries as they come, long-polling. Ack or nack each one before
   * taking the next: until then the same event is delivered again. In lease
   * modes the lease is taken first and renewed as it goes; losing it ends
   * the iteration with 412 `not_leader`.
   */
  async *deliveries(
    opts: { waitMs?: number; partition?: number; ttlMs?: number; signal?: AbortSignal } = {},
  ): AsyncGenerator<Delivery> {
    const p = opts.partition;
    let wait = Math.min(opts.waitMs ?? 25_000, 30_000);
    if (this.leased) {
      if (this.token(p) === undefined) await this.lease({ partition: p, ttlMs: opts.ttlMs });
      // A long-poll mustn't outlive the lease.
      wait = Math.min(wait, Math.floor((this.leases.get(p ?? 0)?.ttlMs ?? DEFAULT_TTL) / 3));
    }
    while (!opts.signal?.aborted) {
      if (this.leased) {
        const held = this.leases.get(p ?? 0);
        if (!held || Date.now() - held.at > held.ttlMs / 3) await this.lease({ partition: p });
      }
      for (const d of await this.next({
        waitMs: wait,
        ...(p !== undefined ? { partition: p } : {}),
      })) {
        yield d;
      }
    }
  }
}

/** A group's dead-letter queue (api.md §8.4). */
export class Dlq {
  constructor(private readonly c: Consumer) {}

  private get session(): Session {
    return this.c.topic.fs.session;
  }

  /** Dead-lettered events, oldest first, decrypted where they open. */
  async list(opts: { after?: Uint8Array; limit?: number } = {}): Promise<DlqEntry[]> {
    const r = await this.session.call('/v1/consume/dlq/list', zw.encodeDlqList, zw.decodeDlqItems, {
      fs: this.c.topic.fs.id,
      group: this.c.group,
      ...(opts.after ? { after: opts.after } : {}),
      ...(opts.limit !== undefined ? { limit: opts.limit } : {}),
    });
    return r.items.map((it) => {
      let event: TopicEvent | undefined;
      try {
        if (equal(it.topic, this.c.topic.id)) event = this.c.topic.decode(it);
      } catch {
        // left undecoded
      }
      return { ...it, ...(event ? { event } : {}) };
    });
  }

  /** Re-append a dead-lettered event (unchanged, new offset); returns the new offset. */
  async retry(id: Uint8Array): Promise<Uint8Array> {
    const r = await this.session.call(
      '/v1/consume/dlq/retry',
      zw.encodeDlqOp,
      zw.decodeCommitResult,
      { fs: this.c.topic.fs.id, group: this.c.group, id, commit_id: randomBytes(16) },
    );
    return r.appended[0]!;
  }

  /** Delete a dead-lettered event. */
  async drop(id: Uint8Array): Promise<void> {
    await this.session.call('/v1/consume/dlq/drop', zw.encodeDlqOp, zw.decodeEmpty, {
      fs: this.c.topic.fs.id,
      group: this.c.group,
      id,
    });
  }
}

/** A DLQ entry: the stored item and, if it opens, the decrypted event. */
export interface DlqEntry extends DlqItem {
  event?: TopicEvent;
}

/** Options of a leader. */
export interface LeaderOptions {
  /** Lease lifetime (default 10 000 ms). */
  ttlMs?: number;
  /** Renewal period (default `ttlMs / 3`). */
  renewMs?: number;
  /** Called once when leadership is lost (a renewal got 412 `not_leader`, or kept failing until the lease ran out). */
  onLost?: (e: ZenError) => void;
}

/**
 * Leader election through a `sequential` group's lease (api.md §8.2,
 * DESIGN-3 §3). `acquire` or `campaign` takes the lease; it is renewed every
 * `renewMs` until `stop` releases it.
 *
 * **Fencing.** `token` is the fencing token: it increases with each new
 * holder, and the server checks it in the same storage transaction as any
 * consume step of this group. To make a leader's effects safe against a
 * stale leader (one that paused past its lease), do its writes in the
 * transaction that acknowledges the event it processes:
 *
 * ```ts
 * for (const d of await leader.next()) {
 *   await fs.transaction(async (tx) => {
 *     tx.set(['state'], apply(d.payload));
 *     leader.ack(d, tx); // consume step carrying `token`
 *   });
 * }
 * ```
 *
 * A commit by a deposed leader then fails as a whole with 412 `not_leader`.
 * Writes outside such a commit are not fenced by the server; `isLeader` is
 * only advisory (the lease may lapse at any moment).
 */
export class Leader {
  private timer: ReturnType<typeof setTimeout> | undefined;
  private lost = false;
  private stopped = false;

  constructor(
    /** The group whose lease is the leadership. */
    readonly consumer: Consumer,
    private readonly opts: LeaderOptions,
  ) {}

  private get ttlMs(): number {
    return this.opts.ttlMs ?? DEFAULT_TTL;
  }

  /** The fencing token while leader. */
  get token(): bigint | undefined {
    return this.consumer.token();
  }

  /** Whether this client believes it holds the lease. */
  get isLeader(): boolean {
    return this.token !== undefined;
  }

  /** Try once to take the lease. Returns whether this client is now the leader. */
  async acquire(): Promise<boolean> {
    if (this.isLeader) return true;
    try {
      await this.consumer.lease({ ttlMs: this.ttlMs });
    } catch (e) {
      if (isCode(e, 'not_leader')) return false;
      throw e;
    }
    this.lost = false;
    this.stopped = false;
    this.schedule();
    return true;
  }

  /** Take the lease, retrying every `ttlMs / 3` until it is free (or `signal` aborts). */
  async campaign(signal?: AbortSignal): Promise<void> {
    while (!(await this.acquire())) {
      if (signal?.aborted) throw new ZenError(0, 'aborted', 'campaign aborted');
      await sleep(Math.max(50, Math.floor(this.ttlMs / 3)));
    }
  }

  private schedule(): void {
    clearTimeout(this.timer);
    if (this.stopped || !this.isLeader) return;
    this.timer = setTimeout(() => void this.renew(), this.opts.renewMs ?? this.ttlMs / 3);
    // A renewal timer alone doesn't keep a Node process alive.
    (this.timer as { unref?: () => void }).unref?.();
  }

  /** Renew now (the timer does this every `renewMs`). Returns whether still leader. */
  async renew(): Promise<boolean> {
    if (!this.isLeader) return false;
    const held = this.token;
    try {
      await this.consumer.lease({ ttlMs: this.ttlMs });
      this.schedule();
      return true;
    } catch (e) {
      const err = e instanceof ZenError ? e : new ZenError(0, 'network', String(e));
      if (isCode(err, 'not_leader')) {
        this.loseWith(err);
        return false;
      }
      // A transient failure: retry while the lease may still hold.
      const at = this.consumer.leasedAt();
      if (this.token !== held || at === undefined || Date.now() - at >= this.ttlMs) {
        this.consumer.forget();
        this.loseWith(err);
        return false;
      }
      this.schedule();
      return true;
    }
  }

  private loseWith(e: ZenError): void {
    clearTimeout(this.timer);
    if (this.lost) return;
    this.lost = true;
    this.opts.onLost?.(e);
  }

  /** The next events of the group, delivered under this leader's lease. */
  next(opts: Omit<NextOptions, 'partition'> = {}): Promise<Delivery[]> {
    return this.consumer.next(opts);
  }

  /** Acknowledge a delivery, fenced by the token (see the class comment). */
  ack(d: Delivery, tx?: Transaction): Promise<void> {
    return this.consumer.ack(d, tx);
  }

  /** Stop renewing and release the lease. */
  async stop(): Promise<void> {
    this.stopped = true;
    clearTimeout(this.timer);
    try {
      await this.consumer.release();
    } catch (e) {
      if (!isCode(e, 'not_leader')) throw e;
    }
  }
}
