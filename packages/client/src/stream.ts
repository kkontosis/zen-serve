// The WebSocket stream: subscriptions and ephemeral messages (spec/api.md §9).
import type { Frame } from '@zen/wasm';
import { concat, fromB64url, hex, utf8 } from './bytes.js';
import { ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import type { Topic, TopicEvent } from './log.js';
import type { Session } from './session.js';
import { sleep } from './transport.js';
import { zw } from './wasm.js';

/** Options of a stream. */
export interface StreamOptions {
  /** Errors that belong to no subscription: a refused `publish` (`quota`, …), an undecryptable ephemeral message. */
  onError?: (e: ZenError) => void;
  /** Reconnect backoff: the first delay and the cap, in ms (default 100 and 5 000). */
  backoffMs?: [number, number];
}

/** What to subscribe to: a topic, or every topic under a prefix of an fs. */
export type StreamTarget =
  | Topic
  | {
      fs: UnlockedFs;
      /** A topic (itself and its descendants), or a raw topic-id prefix; absent or empty = the whole fs. */
      prefix?: Topic | Uint8Array;
      /**
       * Topics whose events to decrypt. Topic ids can't be reversed into
       * paths, so events of other topics arrive unopened. A `Topic` prefix
       * is included.
       */
      topics?: Topic[];
    };

/** An event received on a subscription. */
export type StreamEvent =
  | (TopicEvent & { topic: Uint8Array; opened: true })
  /** An event of a topic the subscription doesn't know the path of, or that doesn't open. */
  | {
      topic: Uint8Array;
      offset: Uint8Array;
      keyToken?: Uint8Array;
      envelope: Uint8Array;
      opened: false;
      error?: ZenError;
    };

/** A decrypted ephemeral message. */
export interface EphemeralMessage {
  topic: Uint8Array;
  /** The publisher's device as it sealed it (32 zero bytes for an API token). */
  sender: Uint8Array;
  /** The publisher's device as the server reports it (the credential id for non-device sign-ins). */
  via: Uint8Array;
  hlc: bigint;
  data: Uint8Array;
}

/** Options of `subscribe`. */
export interface SubscribeOptions<T> {
  /** Start strictly after this offset (history, then live). Default: events committed from now on. */
  after?: Uint8Array;
  /** Receive by callback instead of iterating. */
  onEvent?: (e: T) => void;
  /** With `onEvent`: the error that ended the subscription. */
  onError?: (e: ZenError) => void;
}

/**
 * One subscription: an async iterable of what arrives (unless it was opened
 * with a callback). It ends with an error if the server ends it (e.g. the
 * caller lost read access), and finishes on `close`.
 */
export class Subscription<T> implements AsyncIterable<T> {
  private readonly queue: T[] = [];
  private waiter: { ok: (r: IteratorResult<T>) => void; fail: (e: ZenError) => void } | undefined;
  private error: ZenError | undefined;
  private done = false;

  constructor(
    private readonly stream: Stream,
    readonly id: number,
    private readonly cb: Pick<SubscribeOptions<T>, 'onEvent' | 'onError'>,
  ) {}

  /** Stop the subscription. */
  async close(): Promise<void> {
    if (this.done) return;
    this.finish();
    await this.stream.unsubscribe(this.id);
  }

  /** @internal */
  push(item: T): void {
    if (this.done) return;
    if (this.cb.onEvent) {
      this.cb.onEvent(item);
    } else if (this.waiter) {
      const w = this.waiter;
      this.waiter = undefined;
      w.ok({ value: item, done: false });
    } else {
      this.queue.push(item);
    }
  }

  /** @internal */
  fail(e: ZenError): void {
    if (this.done) return;
    this.error = e;
    this.done = true;
    this.cb.onError?.(e);
    const w = this.waiter;
    this.waiter = undefined;
    w?.fail(e);
  }

  /** @internal */
  finish(): void {
    this.done = true;
    const w = this.waiter;
    this.waiter = undefined;
    w?.ok({ value: undefined, done: true });
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        const item = this.queue.shift();
        if (item !== undefined) return Promise.resolve({ value: item, done: false });
        if (this.error) return Promise.reject(this.error);
        if (this.done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((ok, fail) => {
          this.waiter = { ok, fail };
        });
      },
      return: async () => {
        await this.close();
        return { value: undefined, done: true };
      },
    };
  }
}

interface SubState {
  kind: 'sub' | 'esub';
  fs: number;
  target: { topic: Uint8Array } | { prefix: Uint8Array };
  /** Topics that can open what arrives, by hex id. */
  topics: Map<string, Topic>;
  /** `sub`: the last offset received (or the start). */
  cursor?: Uint8Array;
  // biome-ignore lint/suspicious/noExplicitAny: events or ephemeral messages
  sub: Subscription<any>;
  /** Settles on the first `ok` or `err`. */
  started?: { ok: () => void; fail: (e: ZenError) => void };
  /** `esub`: the newest hlc per sender and topic (replay protection, G21). */
  seen?: Map<string, bigint>;
}

/** u64be(v) ‖ 0xFFFF ‖ 0xFFFF: an offset after every event committed at or before version v. */
function offsetAfterVersion(v: bigint): Uint8Array {
  const out = new Uint8Array(12).fill(0xff);
  new DataView(out.buffer).setBigUint64(0, v);
  return out;
}

function isTopic(t: StreamTarget): t is Topic {
  return 'id' in t && 'path' in t;
}

/**
 * An open stream (one WebSocket). Subscriptions survive a dropped
 * connection: the stream reconnects with backoff and resubscribes each log
 * subscription from the last offset it received, so nothing is lost or
 * repeated (G9). Ephemeral subscriptions resume too, but miss what was
 * published while disconnected.
 */
export class Stream {
  private ws: WebSocket | undefined;
  private readonly subs = new Map<number, SubState>();
  private nextId = 1;
  private closed = false;
  private reconnecting: Promise<void> | undefined;

  private constructor(
    readonly session: Session,
    private readonly opts: StreamOptions,
  ) {}

  /** Open and authenticate a stream. */
  static async open(session: Session, opts: StreamOptions = {}): Promise<Stream> {
    const s = new Stream(session, opts);
    await s.connect();
    return s;
  }

  /** Whether the socket is up and authenticated. */
  get connected(): boolean {
    return this.ws !== undefined && this.ws.readyState === 1 && !this.reconnecting;
  }

  private authToken(): Uint8Array {
    const t = this.session.token;
    return t.startsWith('zen_at_') ? utf8(t) : fromB64url(t);
  }

  /** Connect and authenticate once; rejects on failure. */
  private connect(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const Ctor = this.session.client.transport.opts.WebSocket ?? globalThis.WebSocket;
      const ws = new Ctor(this.session.client.transport.wsUrl('/v1/stream'));
      ws.binaryType = 'arraybuffer';
      this.ws = ws;
      let authed = false;
      ws.onopen = () => ws.send(zw.encodeFrame({ op: 'auth', token: this.authToken() }));
      ws.onmessage = (m: MessageEvent) => {
        if (ws !== this.ws) return;
        let f: Frame;
        try {
          f = zw.decodeFrame(new Uint8Array(m.data as ArrayBuffer));
        } catch (e) {
          this.report(new ZenError(0, 'format', `bad frame: ${String(e)}`));
          return;
        }
        if (authed) {
          this.dispatch(f);
        } else if (f.op === 'ok') {
          authed = true;
          resolve();
        } else if (f.op === 'err') {
          reject(new ZenError(f.code === 'unauthorized' ? 401 : 0, f.code, f.message));
          ws.close();
        }
      };
      ws.onclose = () => {
        if (!authed) reject(new ZenError(0, 'network', 'the stream closed'));
        else if (ws === this.ws) this.dropped();
      };
      ws.onerror = () => {
        // onclose follows.
      };
    });
  }

  private dropped(): void {
    this.ws = undefined;
    if (this.closed || this.reconnecting) return;
    const loop = this.reconnectLoop();
    this.reconnecting = loop;
    void loop.finally(() => {
      if (this.reconnecting === loop) this.reconnecting = undefined;
      // It dropped again while the loop was finishing.
      if (!this.closed && !this.ws) this.dropped();
    });
  }

  private async reconnectLoop(): Promise<void> {
    const [base, cap] = this.opts.backoffMs ?? [100, 5000];
    for (let i = 0; !this.closed; i++) {
      await sleep(Math.min(cap, base * 2 ** i) * (0.5 + Math.random() / 2));
      if (this.closed) return;
      try {
        await this.connect();
      } catch (e) {
        const err = e instanceof ZenError ? e : new ZenError(0, 'network', String(e));
        if (err.status === 401) {
          this.terminate(err);
          return;
        }
        continue;
      }
      if (this.closed) return;
      for (const [id, s] of this.subs) this.sendSub(id, s);
      return;
    }
  }

  /** End every subscription with an error and stop. */
  private terminate(e: ZenError): void {
    this.closed = true;
    for (const s of this.subs.values()) {
      s.started?.fail(e);
      s.sub.fail(e);
    }
    this.subs.clear();
    this.report(e);
  }

  /** Drop the connection and reconnect (for example after a network change). */
  reconnect(): void {
    const ws = this.ws;
    if (!ws) return;
    // The close event arrives later; detach it first so the reconnection starts now.
    this.ws = undefined;
    ws.close();
    this.dropped();
  }

  /** Close the stream; every subscription finishes. */
  close(): void {
    this.closed = true;
    for (const s of this.subs.values()) {
      s.started?.fail(new ZenError(0, 'closed', 'the stream was closed'));
      s.sub.finish();
    }
    this.subs.clear();
    this.ws?.close();
    this.ws = undefined;
  }

  private report(e: ZenError): void {
    this.opts.onError?.(e);
  }

  private send(f: Frame): boolean {
    if (this.ws?.readyState !== 1) return false;
    this.ws.send(zw.encodeFrame(f));
    return true;
  }

  private sendSub(id: number, s: SubState): void {
    if (s.kind === 'sub') {
      this.send({ op: 'sub', id, fs: s.fs, ...s.target, ...(s.cursor ? { after: s.cursor } : {}) });
    } else {
      this.send({ op: 'esub', id, fs: s.fs, ...s.target });
    }
  }

  /** A connection, waiting out a reconnection in progress. */
  private async ready(): Promise<void> {
    if (this.closed) throw new ZenError(0, 'closed', 'the stream was closed');
    while (this.reconnecting) await this.reconnecting;
    if (this.closed) throw new ZenError(0, 'closed', 'the stream was closed');
  }

  private dispatch(f: Frame): void {
    switch (f.op) {
      case 'ok': {
        const s = f.id !== undefined ? this.subs.get(f.id) : undefined;
        s?.started?.ok();
        if (s) s.started = undefined;
        return;
      }
      case 'err': {
        const e = new ZenError(0, f.code, f.message);
        const s = f.id !== undefined ? this.subs.get(f.id) : undefined;
        if (!s) {
          this.report(e);
          return;
        }
        this.subs.delete(f.id!);
        s.started?.fail(e);
        s.sub.fail(e);
        return;
      }
      case 'ev': {
        const s = this.subs.get(f.id);
        if (s?.kind !== 'sub') return;
        s.cursor = f.offset;
        s.sub.push(openEvent(s, f));
        return;
      }
      case 'eph': {
        const s = this.subs.get(f.id);
        if (s?.kind !== 'esub') return;
        const m = this.openEphemeral(s, f);
        if (m) s.sub.push(m);
        return;
      }
      default:
        this.report(new ZenError(0, 'format', `unexpected frame ${f.op}`));
    }
  }

  private openEphemeral(
    s: SubState,
    f: Extract<Frame, { op: 'eph' }>,
  ): EphemeralMessage | undefined {
    const t = s.topics.get(hex(f.topic));
    if (!t) return undefined;
    let body: ReturnType<Topic['open']>;
    try {
      body = t.open(t.ephemeralToken(), f.data);
    } catch (e) {
      this.report(e instanceof ZenError ? e : new ZenError(0, 'decrypt', String(e)));
      return undefined;
    }
    // A replayed message has an hlc its sender already used.
    const k = hex(concat(f.topic, body.sender));
    const last = s.seen!.get(k);
    if (last !== undefined && body.hlc <= last) return undefined;
    s.seen!.set(k, body.hlc);
    return {
      topic: f.topic,
      sender: body.sender,
      via: f.sender,
      hlc: body.hlc,
      data: body.payload,
    };
  }

  private resolveTarget(target: StreamTarget): Pick<SubState, 'fs' | 'target' | 'topics'> {
    const topics = new Map<string, Topic>();
    if (isTopic(target)) {
      topics.set(hex(target.id), target);
      return { fs: target.fs.id, target: { topic: target.id }, topics };
    }
    for (const t of target.topics ?? []) topics.set(hex(t.id), t);
    let prefix: Uint8Array = new Uint8Array(0);
    if (target.prefix instanceof Uint8Array) {
      prefix = target.prefix;
    } else if (target.prefix) {
      topics.set(hex(target.prefix.id), target.prefix);
      prefix = target.prefix.id;
    }
    return { fs: target.fs.id, target: { prefix }, topics };
  }

  private async start<T>(
    kind: 'sub' | 'esub',
    target: StreamTarget,
    opts: SubscribeOptions<T>,
  ): Promise<Subscription<T>> {
    await this.ready();
    const base = this.resolveTarget(target);
    const id = this.nextId++;
    const sub = new Subscription<T>(this, id, opts);
    const s: SubState = { kind, ...base, sub };
    if (kind === 'sub') {
      // Without `after`, start from the current version rather than letting
      // the server pick its head: the cursor is then known from the start,
      // and a reconnection before the first event still misses nothing.
      s.cursor =
        opts.after ??
        offsetAfterVersion(
          (await this.session.call('/v1/grv', zw.encodeEmpty, zw.decodeReadVersion, {}))
            .read_version,
        );
    } else {
      s.seen = new Map();
    }
    const started = new Promise<void>((ok, fail) => {
      s.started = { ok, fail };
    });
    this.subs.set(id, s);
    this.sendSub(id, s);
    await started;
    return sub;
  }

  /**
   * Subscribe to the log: events after `after`, then live ones, in order,
   * decrypted (G9). Resolves once the server accepted the subscription.
   */
  subscribe(
    target: StreamTarget,
    opts: SubscribeOptions<StreamEvent> = {},
  ): Promise<Subscription<StreamEvent>> {
    return this.start('sub', target, opts);
  }

  /** Subscribe to ephemeral messages published from now on (best effort, no history). */
  subscribeEphemeral(
    target: StreamTarget,
    opts: Omit<SubscribeOptions<EphemeralMessage>, 'after'> = {},
  ): Promise<Subscription<EphemeralMessage>> {
    return this.start('esub', target, opts);
  }

  /** Stop a subscription. */
  async unsubscribe(id: number): Promise<void> {
    const s = this.subs.get(id);
    if (!s) return;
    this.subs.delete(id);
    s.sub.finish();
    this.send({ op: 'unsub', id });
  }

  /**
   * Publish an ephemeral message (§9.1): not stored in the log, delivered to
   * current subscribers only. It is sealed as an event body of the topic
   * under a reserved key token, so only holders of the topic key read it,
   * and its hlc lets receivers drop replays. A refusal (`quota` when over
   * the per-device rate) arrives through `onError`.
   */
  async publish(topic: Topic, data: Uint8Array): Promise<void> {
    await this.ready();
    const sealed = topic.seal(topic.ephemeralToken(), data);
    if (!this.send({ op: 'epub', fs: topic.fs.id, topic: topic.id, data: sealed })) {
      throw new ZenError(0, 'network', 'the stream is not connected');
    }
  }
}

function openEvent(s: SubState, f: Extract<Frame, { op: 'ev' }>): StreamEvent {
  const raw = {
    topic: f.topic,
    offset: f.offset,
    ...(f.key_token ? { keyToken: f.key_token } : {}),
    envelope: f.envelope,
  };
  const t = s.topics.get(hex(f.topic));
  if (!t) return { ...raw, opened: false };
  try {
    return { ...t.decode(f), topic: f.topic, opened: true };
  } catch (e) {
    const error = e instanceof ZenError ? e : new ZenError(0, 'decrypt', String(e));
    return { ...raw, opened: false, error };
  }
}
