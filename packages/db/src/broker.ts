// The broker's primitives (spec/zendb.md §11): emit, `on`, consumer groups
// and ephemeral messages, over the M4 client's topics, groups and stream.
import {
  bytes,
  type Consumer,
  type Delivery,
  isCode,
  type Session,
  type Stream,
  type StreamEvent,
  type StreamTarget,
  type SubscribeOptions,
  type Topic,
  type TopicEvent,
  type ZenError,
} from '@zen/client';
import type { OnPoison, Start } from '@zen/wasm';
import { encode, type Value } from './cbor.js';
import type { Db, DbTxnOptions } from './db.js';
import { DbError } from './errors.js';
import { Keys } from './keys.js';
import { decodeMsg, encodeMsg, eventId, type Message, type Msg } from './msg.js';
import { cutParts, type Fields, joinParts, partsDigest } from './row.js';
import { scheduleMsg } from './sched.js';
import type { DbTransaction } from './txn.js';

/** Options of `emit`. */
export interface EmitOptions {
  /** The event key: order within `per_key` and `partitioned` groups (§10.1). */
  key?: Uint8Array | string;
  /**
   * A fresh random key per message: a spread work queue on a `per_key`
   * group (§12.2). Not with `key`.
   */
  spread?: boolean;
  /** The message id (default: random). Keep it across re-sends: it is the idempotency key. */
  id?: Uint8Array;
  /** App metadata. Names starting with `zen.` are reserved. */
  headers?: Record<string, string>;
  /** A large body's parts are deleted this long after the commit (§10.3). Default: kept. */
  ttlMs?: number;
}

/** Room an event's envelope needs beyond its payload (header, sender, hlc, AEAD). */
const ENVELOPE_OVERHEAD = 512;

/** Refuse a message type apps may not use (`zen.` is reserved, §10.1). */
export function checkType(type: string): void {
  if (typeof type !== 'string' || !type) throw new DbError('bad_value', 'a message needs a type');
  if (type.startsWith('zen.')) {
    throw new DbError('bad_value', `message types starting with "zen." are reserved (${type})`);
  }
}

/** Refuse app headers in the reserved `zen.` namespace (§12.2). */
export function checkHeaders(h: Record<string, string> | undefined): void {
  for (const [k, v] of Object.entries(h ?? {})) {
    if (k.startsWith('zen.')) {
      throw new DbError('bad_value', `header names starting with "zen." are reserved (${k})`);
    }
    if (typeof v !== 'string') throw new DbError('bad_value', `header ${k} must be text`);
  }
}

/** The event key of an emit: `key`, or a random one with `spread`. */
export function emitKey(opts: EmitOptions): Uint8Array | string | undefined {
  if (!opts.spread) return opts.key;
  if (opts.key !== undefined) throw new DbError('bad_value', 'spread and key exclude each other');
  return bytes.randomBytes(16);
}

/** A sealed message payload, and whether its body went to parts. */
export interface Sealed {
  payload: Uint8Array;
  parts: number;
}

/**
 * @internal Encode a Msg for an event. A payload too large for one
 * envelope puts its body in parts in this transaction (§10.3), or, with
 * `lean`, is replaced by that smaller Msg (change events, §12.7).
 * `causation` is the length of the causation the event will carry.
 */
export function sealMsg(
  tx: DbTransaction,
  m: Msg,
  lean?: Msg,
  causation = tx.causation?.length ?? 0,
): Sealed {
  const room = tx.db.limits.max_envelope_bytes - ENVELOPE_OVERHEAD - causation;
  let payload = encodeMsg(m);
  let n = 0;
  if (payload.length > room) {
    if (lean) {
      payload = encodeMsg(lean);
    } else if (m.body !== undefined) {
      const body = encode(m.body);
      const parts = cutParts(body, tx.partSize, false);
      for (const [i, p] of parts.entries()) tx.raw.set(tx.keys.msgPart(m.id, i), p);
      n = parts.length;
      const { body: _, ...rest } = m;
      payload = encodeMsg({
        ...rest,
        bodyRef: { ns: tx.db.ns, parts: n, digest: partsDigest(body) },
      });
    }
    if (payload.length > room) {
      throw new DbError('too_large', `a message over max_envelope_bytes (${payload.length} bytes)`);
    }
  }
  return { payload, parts: n };
}

/** @internal Append a Msg in a transaction (§11.1), with the transaction's causation. */
export function appendMsg(
  tx: DbTransaction,
  topic: Topic,
  m: Msg,
  key: Uint8Array | string | undefined,
  lean?: Msg,
): Sealed {
  const s = sealMsg(tx, m, lean);
  topic.appendIn(tx.raw, s.payload, {
    ...(key !== undefined ? { key } : {}),
    ...(tx.causation ? { causation: tx.causation } : {}),
  });
  return s;
}

/** @internal The Msg of a received message (its body resolved), to send again. */
export function msgOf(m: Message, headers?: Record<string, string>): Msg {
  return {
    type: m.type,
    id: m.id,
    ...(m.corr ? { corr: m.corr } : {}),
    ...(m.replyTo ? { replyTo: m.replyTo } : {}),
    ...(m.saga ? { saga: m.saga } : {}),
    ...(m.step !== undefined ? { step: m.step } : {}),
    headers: headers ?? m.headers,
    body: m.body,
  };
}

/** A topic path as bytes (how system rows and `reply_to` store it). */
export const pathBytes = (t: Topic): Uint8Array[] =>
  t.path.map((s) => (typeof s === 'string' ? bytes.utf8(s) : s));

/** Turn an event into a Message, fetching a large body's parts. `poisoned` if it can't be read. */
export async function openMessage<B = Value>(
  db: Db,
  topic: Topic,
  e: TopicEvent,
): Promise<Message<B>> {
  const m = decodeMsg(e.payload);
  let body = m.body;
  if (m.bodyRef) body = await readBody(db, m.id, m.bodyRef);
  return {
    type: m.type,
    id: m.id,
    body: (body ?? null) as B,
    ...(m.corr ? { corr: m.corr } : {}),
    ...(m.replyTo ? { replyTo: m.replyTo } : {}),
    ...(m.deliverAt !== undefined ? { deliverAt: m.deliverAt } : {}),
    ...(m.saga ? { saga: m.saga } : {}),
    ...(m.step !== undefined ? { step: m.step } : {}),
    headers: m.headers ?? {},
    topic,
    offset: e.offset,
    ...(e.keyToken ? { keyToken: e.keyToken } : {}),
    sender: e.sender,
    hlc: e.hlc,
    causation: e.causation,
    eventId: eventId(topic.id, e.offset),
  };
}

async function readBody(db: Db, id: Uint8Array, ref: NonNullable<Msg['bodyRef']>) {
  const other = ref.ns === db.ns ? undefined : new Keys(db.fs.keys.db(ref.ns));
  try {
    const k = other ?? db.keys;
    const keys = Array.from({ length: ref.parts }, (_, i) => k.msgPart(id, i));
    const got = await db.transaction((tx) => tx.raw.snapshotGet(keys));
    if (got.some((e) => !e)) throw new DbError('poisoned', "the message body's parts are gone");
    try {
      return joinParts(
        got.map((e) => e!.value),
        ref.digest,
      );
    } catch (e) {
      if (e instanceof DbError && e.code === 'corrupt') throw new DbError('poisoned', e.message);
      throw e;
    }
  } finally {
    other?.db.free();
  }
}

// ------------------------------------------------------------ the stream

/** A subscription of a `StreamSource`. */
export interface SourceSub {
  close(): Promise<void>;
}

/** An ephemeral message as a stream source hands it over (sealed data, opened). */
export interface EphemeralEvent {
  topic: Uint8Array;
  sender: Uint8Array;
  hlc: bigint;
  data: Uint8Array;
}

/**
 * Where subscriptions come from: the session's stream, or (in a tab that
 * isn't the owner) the owner's, forwarded (§9.2).
 */
export interface StreamSource {
  subscribe(target: StreamTarget, opts: SubscribeOptions<StreamEvent>): Promise<SourceSub>;
  subscribeEphemeral(
    target: StreamTarget,
    opts: { onEvent: (e: EphemeralEvent) => void; onError?: (e: ZenError) => void },
  ): Promise<SourceSub>;
  publish(topic: Topic, data: Uint8Array): Promise<void>;
  close(): void;
}

/** A stream of this session, opened on first use. */
export class DirectSource implements StreamSource {
  private stream: Promise<Stream> | undefined;

  constructor(
    private readonly session: Session,
    private readonly onError: (e: ZenError) => void,
  ) {}

  private get(): Promise<Stream> {
    this.stream ??= this.session.stream({ onError: this.onError });
    return this.stream;
  }

  async subscribe(target: StreamTarget, opts: SubscribeOptions<StreamEvent>): Promise<SourceSub> {
    return (await this.get()).subscribe(target, opts);
  }

  async subscribeEphemeral(
    target: StreamTarget,
    opts: { onEvent: (e: EphemeralEvent) => void; onError?: (e: ZenError) => void },
  ): Promise<SourceSub> {
    return (await this.get()).subscribeEphemeral(target, opts);
  }

  async publish(topic: Topic, data: Uint8Array): Promise<void> {
    await (await this.get()).publish(topic, data);
  }

  /** @internal Drop the connection; the stream reconnects and resumes (tests). */
  async reconnect(): Promise<void> {
    (await this.stream)?.reconnect();
  }

  close(): void {
    this.stream?.then((s) => s.close()).catch(() => {});
    this.stream = undefined;
  }
}

// ------------------------------------------------------------ on

/** What `on` subscribes to: a topic, or every topic under a prefix (§11.2). */
export type OnTarget = Topic | { prefix: Topic; topics?: Topic[] };

/** Options of `on` (§11.2). */
export interface OnOptions {
  /** Start after this offset: history first, then live. Default: new events only. */
  after?: Uint8Array;
  /**
   * Where to start without `after` or a stored cursor: `now` (default, new
   * events only) or `earliest` (the whole history, then live).
   */
  start?: 'now' | 'earliest';
  /**
   * A durable cursor (`$cursors`, per device): the handler runs in a
   * transaction that advances it, so its database effects happen exactly
   * once per device and name.
   */
  cursor?: string;
  /** With `cursor`: events per transaction (default 1). */
  batch?: number;
  /** With `cursor`: the transaction options. */
  txn?: DbTxnOptions;
  /**
   * Errors: an unreadable message (skipped), a plain handler's throw
   * (skipped), a durable handler's throw or the stream's end (the
   * subscription stops). Default: the database's `warn`.
   */
  onError?: (e: unknown, msg?: Message) => void;
}

/** A plain handler gets the message; a durable one (`cursor`) also its transaction. */
export type OnHandler<B = Value> = (msg: Message<B>, tx: DbTransaction) => unknown;

const isTopic = (t: OnTarget): t is Topic => 'id' in t;

/** The offset before every event. */
const EARLIEST = new Uint8Array(12);

/** An `on` subscription. */
export class OnSubscription {
  private readonly queue: StreamEvent[] = [];
  private readonly topics = new Map<string, Topic>();
  private sub: SourceSub | undefined;
  private running = false;
  private stopped = false;
  private idle: Promise<void> = Promise.resolve();

  /** @internal */
  constructor(
    private readonly db: Db,
    private readonly target: OnTarget,
    private readonly handler: OnHandler<never>,
    private readonly opts: OnOptions,
  ) {
    if (isTopic(target)) {
      this.topics.set(bytes.hex(target.id), target);
    } else {
      this.topics.set(bytes.hex(target.prefix.id), target.prefix);
      for (const t of target.topics ?? []) this.topics.set(bytes.hex(t.id), t);
    }
  }

  /** @internal */
  async start(): Promise<void> {
    let after = this.opts.after ?? (this.opts.start === 'earliest' ? EARLIEST : undefined);
    if (this.opts.cursor !== undefined) {
      const row = await this.db.table('$cursors').get([this.db.device, this.opts.cursor]);
      if (row?.offset instanceof Uint8Array) after = row.offset;
    }
    const t = this.target;
    const st: StreamTarget = isTopic(t)
      ? t
      : { fs: this.db.fs, prefix: t.prefix, topics: [...this.topics.values()] };
    this.sub = await this.db.source.subscribe(st, {
      ...(after ? { after } : {}),
      onEvent: (e) => {
        if (this.stopped) return;
        this.queue.push(e);
        this.pump();
      },
      onError: (e) => {
        if (!this.stopped) this.report(e);
        this.stopped = true;
      },
    });
  }

  /** Whether the subscription still runs. */
  get active(): boolean {
    return !this.stopped;
  }

  /** Stop: events not yet handled are dropped (a durable cursor keeps its place). */
  async close(): Promise<void> {
    this.stopped = true;
    this.queue.length = 0;
    await this.sub?.close();
    await this.idle;
  }

  private report(e: unknown, msg?: Message): void {
    if (this.opts.onError) this.opts.onError(e, msg);
    else this.db.warn(`on: ${e instanceof Error ? e.message : String(e)}`);
  }

  private pump(): void {
    if (this.running) return;
    this.running = true;
    this.idle = this.drain().finally(() => {
      this.running = false;
    });
  }

  private async drain(): Promise<void> {
    const n = this.opts.cursor !== undefined ? Math.max(1, this.opts.batch ?? 1) : 1;
    let conflicts = 0;
    while (this.queue.length && !this.stopped) {
      const batch = this.queue.splice(0, n);
      try {
        if (this.opts.cursor !== undefined) await this.durable(batch);
        else await this.plain(batch[0]!);
        conflicts = 0;
      } catch (e) {
        // Writers that kept winning (an authenticated database's writers all
        // conflict, §14.5) aren't a failed handler: run the batch again later.
        if (isCode(e, 'conflict') && !this.stopped) {
          this.queue.unshift(...batch);
          const ms = Math.min(20 * 2 ** conflicts++, 1000) * (0.5 + Math.random());
          await new Promise((r) => setTimeout(r, ms));
          continue;
        }
        // A durable handler failed: stop here, so a restart retries from the cursor.
        this.stopped = true;
        this.queue.length = 0;
        this.report(e);
        void this.sub?.close();
      }
    }
  }

  /** Open an event; undefined (reported) if it can't be read. */
  private async open(e: StreamEvent): Promise<Message | undefined> {
    const t = this.topics.get(bytes.hex(e.topic));
    if (!e.opened || !t) {
      if (e.opened === false && e.error) this.report(e.error);
      return undefined;
    }
    try {
      return await openMessage(this.db, t, e);
    } catch (err) {
      this.report(err);
      return undefined;
    }
  }

  private async plain(e: StreamEvent): Promise<void> {
    const msg = await this.open(e);
    if (!msg) return;
    try {
      await this.handler(msg as Message<never>, undefined as never);
    } catch (err) {
      this.report(err, msg);
    }
  }

  private async durable(batch: StreamEvent[]): Promise<void> {
    const items = await Promise.all(batch.map(async (e) => ({ e, msg: await this.open(e) })));
    const name = this.opts.cursor!;
    const path = pathBytes(isTopic(this.target) ? this.target : this.target.prefix);
    await this.db.transaction(async (tx) => {
      const cursors = tx.table('$cursors');
      const cur = await cursors.get([this.db.device, name]);
      const from = cur?.offset instanceof Uint8Array ? cur.offset : undefined;
      let last: Uint8Array | undefined;
      for (const { e, msg } of items) {
        if (from && bytes.compare(e.offset, from) <= 0) continue;
        if (msg) await this.handler(msg as Message<never>, tx);
        last = e.offset;
      }
      if (last) await cursors.put({ device: this.db.device, name, topic: path, offset: last });
    }, this.opts.txn);
  }
}

// ------------------------------------------------------------ consume

/** Options of `consume` (§11.3); the group's are api.md §8.1's. */
export interface ConsumeOptions {
  /** The app's group name; the server sees a per-topic token of it. */
  group: string;
  topic: Topic;
  mode: 'sequential' | 'partitioned' | 'per_key' | 'single_key';
  /** `partitioned`: the partition count (1..256); one lease per partition. */
  partitions?: number;
  /** `single_key`: the key. */
  key?: Uint8Array | string;
  /** `per_key`: deliveries handled at once (default 1). */
  maxInflight?: number;
  /** Attempts before the DLQ (default 5; 0 = unlimited). */
  maxAttempts?: number;
  /** `dlq` (default) or `block`. */
  onPoison?: OnPoison;
  /** Where a new group starts (default `earliest`). */
  start?: Start;
  /** The handler's transaction options (default short). */
  txn?: DbTxnOptions;
  /** Lease lifetime in lease modes (default 10,000 ms). */
  ttlMs?: number;
  /** Long-poll wait (default 25,000 ms; capped at a third of the lease). */
  waitMs?: number;
  /**
   * What a handler's throw does (§12.2): `nack` (default) redelivers at
   * once, in order; `delay` consumes the message and schedules a copy
   * after `backoff(attempt)`, with header `zen.attempt` counting attempts,
   * and after `maxAttempts` emits it to `<topic>/zen.dlq`.
   */
  retry?: 'nack' | 'delay';
  /** `retry: 'delay'`: ms before attempt `attempt + 1` (default `min(1000·2^(attempt−1), 300000)`). */
  backoff?: (attempt: number) => number;
  /**
   * Skip messages handled before (§12.6): a `$inbox` row per message id
   * (per `corr` for `serve`), kept `dedupTtlMs`.
   */
  dedup?: boolean;
  /** How long dedup rows are kept (default 7 days). */
  dedupTtlMs?: number;
  /** Errors: a handler's throw (nacked), an unreadable message (nacked), polling errors (retried). */
  onError?: (e: unknown, msg?: Message) => void;
}

/** The attempt a delivery is (`zen.attempt`, from 1). */
export function attemptOf(m: Message): number {
  const n = Number(m.headers['zen.attempt'] ?? 1);
  return Number.isSafeInteger(n) && n >= 1 ? n : 1;
}

const defaultBackoff = (a: number) => Math.min(1000 * 2 ** (a - 1), 300_000);

/**
 * @internal With `retry: 'delay'`: schedule the next attempt of `m`, or,
 * after `maxAttempts`, emit it to the class DLQ `<topic>/zen.dlq` (§12.2).
 */
async function retryLater(tx: DbTransaction, o: ConsumeOptions, m: Message): Promise<void> {
  const a = attemptOf(m);
  const max = o.maxAttempts ?? 5;
  if (max > 0 && a >= max) {
    appendMsg(
      tx,
      o.topic.child('zen.dlq'),
      msgOf(m, { ...m.headers, 'zen.attempt': String(a) }),
      undefined,
    );
    return;
  }
  // A consumer knows the event key's token only: the copy goes out with
  // the group's key (`single_key`) or a fresh one.
  const key = o.mode === 'single_key' ? o.key : bytes.randomBytes(16);
  const at = Math.ceil(tx.db.fs.session.clock.now() + (o.backoff ?? defaultBackoff)(a));
  await scheduleMsg(tx, o.topic, msgOf(m, { ...m.headers, 'zen.attempt': String(a + 1) }), key, at);
}

/**
 * @internal How a consumer handles a handler's throw before `nack`: in a
 * fresh transaction that also consumes the message, `true` if handled.
 */
export type FailHook = (msg: Delivered, e: unknown, tx: DbTransaction) => Promise<boolean>;

/** A consumed message: `ack` it without writes, or bind it with `db.begin({consumes})`. */
export interface Delivered<B = Value> extends Message<B> {
  /** Failed attempts before this delivery. */
  attempts: number;
  /** Acknowledge without writes: a commit with only the consume step. */
  ack(): Promise<void>;
}

/** @internal What a delivered message is bound to. */
export interface Binding {
  consumer: Consumer;
  delivery: Delivery;
  acked: boolean;
}

const bindings = new WeakMap<object, Binding>();

/** @internal The delivery a message came from. */
export function bindingOf(msg: object): Binding | undefined {
  return bindings.get(msg);
}

const pause = (ms: number, signal: AbortSignal) =>
  new Promise<void>((r) => {
    const t = setTimeout(r, ms);
    signal.addEventListener('abort', () => {
      clearTimeout(t);
      r();
    });
  });

const dropped = (e: unknown) => isCode(e, 'cursor_moved') || isCode(e, 'claim_lost');

/**
 * A running consumer (§11.3). It runs while its database is the owner
 * (§9.2), from `onOwner` until `stop()` or `db.close()`.
 */
export class ConsumerHandle {
  private ctl: AbortController | undefined;
  private consumer: Consumer | undefined;
  private readonly busy = new Set<Promise<void>>();
  private stopped = false;
  private readonly unOwner: () => void;

  /** @internal */
  constructor(
    private readonly db: Db,
    private readonly opts: ConsumeOptions,
    private readonly handler: (msg: Delivered<never>, tx: DbTransaction) => unknown,
    private readonly manual: boolean,
    private readonly onFail?: FailHook,
  ) {
    this.unOwner = db.onOwner(() => this.start());
  }

  /** The group's consumer, once created (its DLQ, cursors). */
  get group(): Consumer | undefined {
    return this.consumer;
  }

  /** Whether the consumer runs (this instance owns the database and it wasn't stopped). */
  get running(): boolean {
    return this.ctl !== undefined && !this.ctl.signal.aborted;
  }

  /** Stop consuming: waits for the handlers running now, then releases the leases. */
  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    this.unOwner();
    this.ctl?.abort();
    await Promise.allSettled([...this.busy]);
    const c = this.consumer;
    if (c?.leased) {
      for (const p of this.partitions()) await c.release(p).catch(() => {});
    }
  }

  private partitions(): (number | undefined)[] {
    return this.opts.mode === 'partitioned'
      ? Array.from({ length: this.opts.partitions ?? 1 }, (_, i) => i)
      : [undefined];
  }

  private report(e: unknown, msg?: Message): void {
    if (this.opts.onError) this.opts.onError(e, msg);
    else this.db.warn(`consume ${this.opts.group}: ${e instanceof Error ? e.message : String(e)}`);
  }

  private start(): void {
    if (this.stopped || this.ctl) return;
    this.ctl = new AbortController();
    void this.run(this.ctl.signal);
  }

  private get ttl(): number {
    return this.opts.ttlMs ?? 10_000;
  }

  private async run(signal: AbortSignal): Promise<void> {
    const o = this.opts;
    while (!this.consumer && !signal.aborted) {
      try {
        const id = o.topic.keysAt(o.topic.fs.keys.epoch).groupId(o.group);
        this.consumer = await o.topic.group(id, {
          mode: o.mode,
          ...(o.partitions !== undefined ? { partitions: o.partitions } : {}),
          ...(o.key !== undefined ? { key: o.key } : {}),
          ...(o.maxInflight !== undefined ? { maxInflight: o.maxInflight } : {}),
          ...(o.maxAttempts !== undefined ? { maxAttempts: o.maxAttempts } : {}),
          ...(o.onPoison !== undefined ? { onPoison: o.onPoison } : {}),
          ...(o.start !== undefined ? { start: o.start } : {}),
        });
      } catch (e) {
        if (signal.aborted) return;
        this.report(e);
        // A changed definition (`group_exists`) won't heal: stop.
        if (isCode(e, 'group_exists')) return;
        await pause(1000, signal);
      }
    }
    if (signal.aborted) return;
    await Promise.all(this.partitions().map((p) => this.loop(this.consumer!, p, signal)));
  }

  private async loop(c: Consumer, p: number | undefined, signal: AbortSignal): Promise<void> {
    let backoff = 100;
    while (!signal.aborted) {
      try {
        if (c.leased) {
          for await (const d of c.deliveries({
            ...(p !== undefined ? { partition: p } : {}),
            ttlMs: this.ttl,
            waitMs: this.opts.waitMs ?? 25_000,
            signal,
          })) {
            if (signal.aborted) break;
            await this.track(this.one(c, d, signal));
            backoff = 100;
          }
        } else {
          const ds = await c.next({
            limit: this.opts.maxInflight ?? 1,
            waitMs: Math.min(this.opts.waitMs ?? 25_000, 30_000),
          });
          if (signal.aborted) break;
          await Promise.all(ds.map((d) => this.track(this.one(c, d, signal))));
          backoff = 100;
        }
      } catch (e) {
        if (signal.aborted) break;
        if (isCode(e, 'not_leader')) {
          // Another instance holds the lease: campaign again later.
          c.forget(p);
          await pause(Math.max(50, this.ttl / 3), signal);
          continue;
        }
        this.report(e);
        await pause(backoff, signal);
        backoff = Math.min(backoff * 2, 5000);
      }
    }
  }

  private async track(p: Promise<void>): Promise<void> {
    this.busy.add(p);
    try {
      await p;
    } finally {
      this.busy.delete(p);
    }
  }

  /** Nack a delivery. Retried while the network fails: until it lands, the claim holds the key. */
  private async nack(c: Consumer, d: Delivery, signal: AbortSignal): Promise<void> {
    for (let backoff = 100; ; backoff = Math.min(backoff * 2, 5000)) {
      try {
        await c.nack(d);
        return;
      } catch (e) {
        if (isCode(e, 'not_leader')) throw e;
        if (isCode(e, 'network') && !signal.aborted) {
          await pause(backoff, signal);
          continue;
        }
        if (!dropped(e)) this.report(e);
        return;
      }
    }
  }

  /**
   * Handle one delivery: one transaction with the handler and the consume
   * step. A network failure is the transport's, not the handler's: the
   * claim still holds the key, and a nack would fail as well, so the key's
   * later events would wait for the claim to expire. The delivery runs
   * again once the network is back instead; if an earlier attempt did
   * commit, the retry's consume step is `claim_lost` or `cursor_moved`.
   */
  private async one(c: Consumer, d: Delivery, signal: AbortSignal): Promise<void> {
    if (d.error) {
      this.report(d.error);
      return this.nack(c, d, signal);
    }
    let msg: Message;
    try {
      msg = await openMessage(this.db, this.opts.topic, d);
    } catch (e) {
      this.report(e);
      return this.nack(c, d, signal);
    }
    const b: Binding = { consumer: c, delivery: d, acked: false };
    const m = msg as Delivered;
    m.attempts = d.attempts;
    m.ack = async () => {
      await c.ack(d);
      b.acked = true;
    };
    bindings.set(m, b);
    for (let backoff = 100; ; backoff = Math.min(backoff * 2, 5000)) {
      try {
        if (this.manual) {
          await this.handler(m as Delivered<never>, undefined as never);
          if (!b.acked) throw new DbError('bad_value', 'the handler neither acked nor committed');
        } else {
          await this.db.transaction(async (tx) => {
            tx.causation = m.eventId;
            await this.handler(m as Delivered<never>, tx);
            await c.ack(d, tx.raw);
          }, this.opts.txn);
        }
        return;
      } catch (e) {
        if (b.acked || dropped(e)) return;
        if (isCode(e, 'not_leader')) throw e;
        if (isCode(e, 'network') && !signal.aborted) {
          this.report(e, m);
          await pause(backoff, signal);
          continue;
        }
        if (!this.manual && (await this.failed(c, d, m, e))) return;
        this.report(e, m);
        await this.nack(c, d, signal);
        return;
      }
    }
  }

  /**
   * A handler threw: an error reply (`serve`) or a delayed retry, in a
   * fresh transaction with the consume step. False: `nack` instead.
   */
  private async failed(c: Consumer, d: Delivery, m: Delivered, e: unknown): Promise<boolean> {
    const delay = this.opts.retry === 'delay';
    if (!delay && !this.onFail) return false;
    try {
      return await this.db.transaction(async (tx) => {
        tx.causation = m.eventId;
        if (!(this.onFail && (await this.onFail(m, e, tx)))) {
          if (!delay) return false;
          this.report(e, m);
          await retryLater(tx, this.opts, m);
        }
        await c.ack(d, tx.raw);
        return true;
      }, this.opts.txn);
    } catch (e2) {
      if (dropped(e2)) return true;
      if (isCode(e2, 'not_leader')) throw e2;
      this.report(e2, m);
      return false;
    }
  }
}

/** A message sent with `publishEphemeral` (§11.4). */
export interface EphemeralMsg<B = Value> {
  type: string;
  id: Uint8Array;
  body: B;
  /** The request a reply answers (§12.3). */
  corr?: Uint8Array;
  topic: Topic;
  sender: Uint8Array;
  hlc: bigint;
}

/** Decode an ephemeral message; undefined if it isn't a Msg. */
export function openEphemeral<B>(topic: Topic, e: EphemeralEvent): EphemeralMsg<B> | undefined {
  try {
    const m = decodeMsg(e.data);
    return {
      type: m.type,
      id: m.id,
      body: (m.body ?? null) as B,
      ...(m.corr ? { corr: m.corr } : {}),
      topic,
      sender: e.sender,
      hlc: e.hlc,
    };
  } catch {
    return undefined;
  }
}

/** The body of a change event (§12.7). */
export interface ChangeBody<T extends Fields = Fields> {
  op: 'put' | 'delete';
  table: string;
  pk: Value;
  /** With `image: 'full'`, when it fits one envelope. */
  row?: T;
}
