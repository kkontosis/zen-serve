// RequestMsg/reply (spec/zendb.md §12.3): the inbox, waiters, replies, and
// the dedup rows of responders and consumers (§12.6).
import { bytes, type Topic } from '@zen/client';
import { appendMsg, type EphemeralMsg, type OnSubscription, type SourceSub } from './broker.js';
import { encode, isPlainObject, type Value } from './cbor.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';
import type { Message, Msg } from './msg.js';
import { scheduleGc } from './sched.js';
import type { DbTransaction } from './txn.js';

/**
 * Thrown by a `serve` handler: the requester gets an error reply (type
 * `<type>.error`, body `{message, data?}`) and the request is consumed.
 * `request` rejects with one built from that reply.
 */
export class ReplyError extends Error {
  override name = 'ReplyError';

  constructor(
    message: string,
    readonly data?: Value,
  ) {
    super(message);
  }
}

/** A reply: an event on the inbox, or an ephemeral message. */
export type Reply<B = Value> = EphemeralMsg<B> | Message<B>;

/** Options of `request`. */
export interface RequestOptions {
  /** How long to wait for the reply, from the commit (default 10,000 ms). */
  timeoutMs?: number;
  /** The request's event key. */
  key?: Uint8Array | string;
  /** The correlation id (default: random). Pass the same one to retry after a timeout (dedup). */
  corr?: Uint8Array;
}

/** Options of `requestAll`. */
export interface RequestAllOptions extends RequestOptions {
  /** Resolve as soon as this many replies arrived. */
  max?: number;
}

/** @internal The reply a responder sends: its type and body. */
export interface ReplyBody {
  type: string;
  body: Value;
}

interface Waiter {
  type: string;
  all: boolean;
  max: number;
  got: Reply[];
  resolve: (r: never) => void;
  reject: (e: unknown) => void;
  timer?: ReturnType<typeof setTimeout>;
}

/** The body of an error reply. */
export const errorBody = (e: ReplyError): Value =>
  e.data === undefined ? { message: e.message } : { message: e.message, data: e.data };

/**
 * Each `Db`'s inbox (§12.3): the topic `("zen", "inbox", instance)` with
 * an `on` subscription from its start, so no reply can come before it, and
 * an ephemeral one.
 */
export class Inbox {
  /** 16 random bytes per `Db`. */
  readonly instance = bytes.randomBytes(16);
  readonly topic: Topic;
  private readonly waiting = new Map<string, Waiter>();
  private started: Promise<void> | undefined;
  private subs: (OnSubscription | SourceSub)[] = [];
  private closed = false;

  constructor(private readonly db: Db) {
    this.topic = db.topic('zen', 'inbox', this.instance);
  }

  /** The topic path requests carry as `reply_to`. */
  get path(): Uint8Array[] {
    return [bytes.utf8('zen'), bytes.utf8('inbox'), this.instance];
  }

  /** Subscribe (once). */
  start(): Promise<void> {
    this.started ??= (async () => {
      this.subs.push(
        await this.db.on(this.topic, (m) => this.deliver(m), { after: new Uint8Array(12) }),
        await this.db.onEphemeral(this.topic, (m) => this.deliver(m)),
      );
    })();
    return this.started;
  }

  /**
   * Wait for the replies to `corr`: the first (`all` false), or every one
   * until the timeout or `max`. The timer starts now.
   */
  wait(corr: Uint8Array, type: string, all: boolean, opts: RequestAllOptions): Promise<never> {
    return new Promise((resolve, reject) => {
      if (this.closed) return reject(new DbError('closed', 'the database was closed'));
      const k = bytes.hex(corr);
      const w: Waiter = { type, all, max: opts.max ?? Infinity, got: [], resolve, reject };
      w.timer = setTimeout(() => {
        this.waiting.delete(k);
        if (all) resolve(w.got as never);
        else
          reject(new DbError('timeout', `no reply to ${type} in ${opts.timeoutMs ?? 10_000} ms`));
      }, opts.timeoutMs ?? 10_000);
      this.waiting.set(k, w);
    });
  }

  private deliver(m: Reply): void {
    if (!m.corr) return;
    const k = bytes.hex(m.corr);
    const w = this.waiting.get(k);
    // Late, repeated or foreign: dropped.
    if (!w) return;
    if (w.all) {
      w.got.push(m);
      if (w.got.length < w.max) return;
      this.settle(k, w);
      w.resolve(w.got as never);
      return;
    }
    this.settle(k, w);
    if (m.type === `${w.type}.error`) {
      const b = m.body;
      const msg = isPlainObject(b) && typeof b.message === 'string' ? b.message : 'error reply';
      w.reject(new ReplyError(msg, isPlainObject(b) ? b.data : undefined));
    } else {
      w.resolve(m as never);
    }
  }

  private settle(k: string, w: Waiter): void {
    clearTimeout(w.timer);
    this.waiting.delete(k);
  }

  /** Stop: pending requests fail with `closed`. */
  close(): void {
    this.closed = true;
    for (const [k, w] of this.waiting) {
      this.settle(k, w);
      w.reject(new DbError('closed', 'the database was closed'));
    }
    for (const s of this.subs) void s.close().catch(() => {});
    this.subs = [];
  }
}

/** What a reply needs of its request. */
export type RequestMsg = Pick<Message<unknown>, 'type' | 'id' | 'corr' | 'replyTo'> &
  Partial<Pick<Message<unknown>, 'saga' | 'step'>>;

/** @internal The reply Msg to a request; a saga command's reply carries its `saga` and `step` (§12.5). */
export function replyMsg(req: RequestMsg, r: ReplyBody): Msg {
  return {
    type: r.type,
    id: bytes.randomBytes(16),
    corr: req.corr ?? req.id,
    ...(req.saga ? { saga: req.saga } : {}),
    ...(req.step !== undefined ? { step: req.step } : {}),
    body: r.body,
  };
}

/**
 * @internal Send a reply to `reply_to`: an event in `tx` (keyed by the saga
 * id for a saga command, so its orchestrator sees it in order), or
 * ephemeral after its commit.
 */
export function sendReply(
  tx: DbTransaction,
  req: RequestMsg,
  r: ReplyBody,
  ephemeral = false,
): void {
  if (!req.replyTo) return;
  const to = tx.db.topic(...req.replyTo);
  const m = replyMsg(req, r);
  if (ephemeral) tx.after(() => void tx.db.publishMsg(to, m).catch((e) => tx.db.warn(String(e))));
  else appendMsg(tx, to, m, req.saga);
}

// ------------------------------------------------------------ dedup

/** Default lifetime of a dedup row (7 days, §12.6). */
export const DEDUP_TTL = 7 * 24 * 3600_000;

interface InboxRow {
  [field: string]: Value;
  scope: string;
  id: Uint8Array;
  at: number;
}

/** @internal A dedup row seen before: its stored reply, if any (§12.6). */
export async function dedupSeen(
  tx: DbTransaction,
  scope: string,
  id: Uint8Array,
): Promise<{ reply?: ReplyBody } | undefined> {
  const row = await tx.table<InboxRow>('$inbox').get([scope, id]);
  if (!row) return undefined;
  const r = row.reply;
  return isPlainObject(r) && typeof r.type === 'string'
    ? { reply: { type: r.type, body: r.body ?? null } }
    : {};
}

/**
 * @internal Record a dedup row (with the reply to send again) and its `gc`
 * job after `ttlMs`.
 */
export async function dedupRecord(
  tx: DbTransaction,
  scope: string,
  id: Uint8Array,
  ttlMs: number,
  reply?: ReplyBody,
): Promise<void> {
  const now = Math.floor(tx.db.fs.session.clock.now());
  await tx
    .table<InboxRow>('$inbox')
    .insert({ scope, id, at: now, ...(reply ? { reply: { ...reply } } : {}) });
  const t = await tx.tableRecord('$inbox');
  const pk = encode([scope, id]);
  await scheduleGc(
    tx,
    [
      ['t', t.id, pk],
      ['o', t.id, pk],
    ],
    now + ttlMs,
  );
}
