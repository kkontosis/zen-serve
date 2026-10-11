// The tab proxy (spec/zendb.md §9.2): only the owner holds a stream. Other
// tabs send it their subscriptions over a BroadcastChannel; it forwards log
// events sealed (each tab opens them with its own topic keys) and ephemeral
// messages opened. Transactions stay local in every tab.

import type { BroadcastChannelLike } from '@zen/client';
import {
  bytes,
  offsetAfterVersion,
  type StreamEvent,
  type StreamTarget,
  type SubscribeOptions,
  type Topic,
  ZenError,
} from '@zen/client';
import {
  DirectSource,
  type EphemeralEvent,
  pathBytes,
  type SourceSub,
  type StreamSource,
} from './broker.js';
import type { Db } from './db.js';
import { DbError } from './errors.js';

export type { BroadcastChannelLike, ChannelFactory } from '@zen/client';

/** How long a tab waits for an owner to answer (ms). */
const OWNER_WAIT = 10_000;

/** A message on the channel: `from` a tab, `to` one (or every) tab. */
// biome-ignore lint/suspicious/noExplicitAny: structured-clone payloads
type Wire = { t: string; from: string; to?: string } & Record<string, any>;

type EphOpts = { onEvent: (e: EphemeralEvent) => void; onError?: (e: ZenError) => void };

/** A subscription of this tab, whichever route it takes. */
interface Logical {
  kind: 'sub' | 'esub';
  target: StreamTarget;
  /** `sub`: the topics that open what arrives, by hex id. */
  topics: Map<string, Topic>;
  /** `sub`: the topic-id prefix, and whether only that exact topic counts. */
  prefix: Uint8Array;
  exact: boolean;
  /** `sub`: the last offset delivered (or the start). */
  last: Uint8Array | undefined;
  onEvent: (e: never) => void;
  onError: ((e: ZenError) => void) | undefined;
  /** Bumped on each (re)attach: deliveries of an older route are dropped. */
  gen: number;
  /** The direct subscription, or the owner tab and remote id. */
  inner: SourceSub | undefined;
  at: string | undefined;
  rsid: string | undefined;
  closed: boolean;
}

const isTopic = (t: StreamTarget): t is Topic => 'id' in t && 'path' in t;

/**
 * A `StreamSource` for tabs: direct while this tab owns the database,
 * through the owner otherwise. A route change re-subscribes from the last
 * offset delivered and drops what comes again, so a handover loses and
 * repeats nothing.
 */
export class TabSource implements StreamSource {
  /** This tab's id on the channel. */
  readonly id = bytes.hex(bytes.randomBytes(8));
  private owner: string | undefined;
  private direct: DirectSource | undefined;
  private host: ProxyHost | undefined;
  private readonly subs = new Set<Logical>();
  private readonly remote = new Map<string, { l: Logical; gen: number }>();
  private readonly calls = new Map<
    number,
    { to: string; ok: () => void; fail: (e: unknown) => void }
  >();
  private waiters: (() => void)[] = [];
  private nextRid = 1;
  private nextSid = 1;
  private closed = false;
  private readonly unOwner: () => void;
  private readonly onHide = () => this.bye();

  /** @internal */
  constructor(
    private readonly db: Db,
    private readonly chan: BroadcastChannelLike,
  ) {
    chan.onmessage = (ev) => this.recv(ev.data as Wire);
    this.unOwner = db.onOwner(() => this.promote());
    const g = globalThis as { addEventListener?: (t: string, f: () => void) => void };
    g.addEventListener?.('pagehide', this.onHide);
    if (!db.isOwner) this.post({ t: 'hello' });
  }

  /** Whether this tab holds a stream of its own (it owns the database). */
  get streaming(): boolean {
    return this.direct !== undefined;
  }

  private post(m: Omit<Wire, 'from'>): void {
    if (this.closed) return;
    try {
      this.chan.postMessage({ ...m, from: this.id });
    } catch (e) {
      this.db.warn(`proxy: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  private source(): DirectSource {
    this.direct ??= new DirectSource(this.db.fs.session, (e) =>
      this.db.warn(`stream: ${e.message}`),
    );
    return this.direct;
  }

  /** Became the owner: serve the other tabs, move every subscription to the stream. */
  private promote(): void {
    if (this.closed) return;
    this.owner = undefined;
    this.host ??= new ProxyHost(this.db, this.source(), (m) => this.post(m));
    this.post({ t: 'owner' });
    this.wake();
    for (const l of this.subs) this.reattach(l);
  }

  private wake(): void {
    const ws = this.waiters;
    this.waiters = [];
    for (const w of ws) w();
  }

  /** The owner tab's id, once one answers. */
  private async ownerTab(): Promise<string | undefined> {
    if (this.db.isOwner) return undefined;
    if (this.owner) return this.owner;
    this.post({ t: 'hello' });
    await new Promise<void>((ok, fail) => {
      const t = setTimeout(() => {
        this.waiters = this.waiters.filter((w) => w !== done);
        fail(new DbError('timeout', 'no owner tab answered'));
      }, OWNER_WAIT);
      const done = () => {
        clearTimeout(t);
        ok();
      };
      this.waiters.push(done);
    });
    return this.db.isOwner ? undefined : this.owner;
  }

  /** A request to the owner, settled by its `ok` or `err`. */
  private call(to: string, m: Omit<Wire, 'from' | 'to' | 'rid'>): Promise<void> {
    const rid = this.nextRid++;
    return new Promise((ok, fail) => {
      this.calls.set(rid, { to, ok, fail });
      this.post({ ...m, to, rid });
    });
  }

  private recv(m: Wire): void {
    if (this.closed || !m || typeof m !== 'object' || m.from === this.id) return;
    if (m.to !== undefined && m.to !== this.id) return;
    switch (m.t) {
      case 'owner':
        if (this.db.isOwner) return;
        if (this.owner !== m.from) {
          this.owner = m.from;
          for (const l of this.subs) if (l.at !== m.from) this.reattach(l);
        }
        this.wake();
        return;
      case 'bye':
        this.host?.drop(m.from);
        if (this.owner === m.from) {
          this.owner = undefined;
          for (const [rid, c] of this.calls) {
            if (c.to !== m.from) continue;
            this.calls.delete(rid);
            c.fail(new Moved());
          }
        }
        return;
      case 'ok':
      case 'err': {
        const c = this.calls.get(m.rid);
        if (!c) return;
        this.calls.delete(m.rid);
        if (m.t === 'ok') c.ok();
        else c.fail(new ZenError(0, m.code ?? 'proxy', m.message ?? 'refused by the owner tab'));
        return;
      }
      case 'ev':
      case 'eev':
      case 'end': {
        const r = this.remote.get(m.rsid);
        if (!r || r.gen !== r.l.gen || r.l.closed) return;
        if (m.t === 'ev') this.remoteEvent(r.l, m);
        else if (m.t === 'eev') {
          (r.l.onEvent as (e: EphemeralEvent) => void)({
            topic: m.topic,
            sender: m.sender,
            hlc: m.hlc,
            data: m.data,
          });
        } else this.end(r.l, new ZenError(0, m.code, m.message));
        return;
      }
      default:
        this.host?.handle(m);
    }
  }

  /** A sealed event from the owner: open it here. */
  private remoteEvent(l: Logical, m: Wire): void {
    const raw = {
      topic: m.topic as Uint8Array,
      offset: m.offset as Uint8Array,
      ...(m.keyToken ? { keyToken: m.keyToken as Uint8Array } : {}),
      envelope: m.envelope as Uint8Array,
    };
    const t = l.topics.get(bytes.hex(raw.topic));
    let e: StreamEvent;
    if (!t) {
      e = { ...raw, opened: false };
    } else {
      try {
        e = {
          ...t.decode({
            offset: raw.offset,
            envelope: raw.envelope,
            ...(raw.keyToken ? { key_token: raw.keyToken } : {}),
          }),
          topic: raw.topic,
          opened: true,
        };
      } catch (err) {
        const error = err instanceof ZenError ? err : new ZenError(0, 'decrypt', String(err));
        e = { ...raw, opened: false, error };
      }
    }
    this.deliver(l, e);
  }

  /** Deliver an event once: offsets at or before the last one are repeats. */
  private deliver(l: Logical, e: StreamEvent): void {
    if (l.closed) return;
    if (l.last && bytes.compare(e.offset, l.last) <= 0) return;
    l.last = e.offset;
    (l.onEvent as (e: StreamEvent) => void)(e);
  }

  private end(l: Logical, e: ZenError): void {
    if (l.closed) return;
    this.forget(l);
    l.onError?.(e);
  }

  private forget(l: Logical): void {
    l.closed = true;
    this.subs.delete(l);
    if (l.rsid) this.remote.delete(l.rsid);
  }

  /** Move a subscription to the current route; errors end it. */
  private reattach(l: Logical): void {
    this.attach(l).catch((e) => {
      if (!(e instanceof Moved)) {
        this.end(l, e instanceof ZenError ? e : new ZenError(0, 'proxy', String(e)));
      }
    });
  }

  private async attach(l: Logical): Promise<void> {
    const gen = ++l.gen;
    // Leave the old route.
    const inner = l.inner;
    l.inner = undefined;
    if (inner) void inner.close().catch(() => {});
    if (l.at && l.rsid) {
      this.post({ t: 'unsub', to: l.at, rsid: l.rsid });
      this.remote.delete(l.rsid);
    }
    l.at = undefined;
    l.rsid = undefined;
    if (this.db.isOwner) {
      const src = this.source();
      if (l.kind === 'sub') {
        const sub = await src.subscribe(l.target, {
          ...(l.last ? { after: l.last } : {}),
          onEvent: (e) => l.gen === gen && this.deliver(l, e),
          onError: (e) => l.gen === gen && this.end(l, e),
        });
        if (l.gen !== gen || l.closed) return void sub.close().catch(() => {});
        l.inner = sub;
      } else {
        const sub = await src.subscribeEphemeral(l.target, {
          onEvent: (e) =>
            l.gen === gen && !l.closed && (l.onEvent as (e: EphemeralEvent) => void)(e),
          onError: (e) => l.gen === gen && this.end(l, e),
        });
        if (l.gen !== gen || l.closed) return void sub.close().catch(() => {});
        l.inner = sub;
      }
      return;
    }
    const owner = await this.ownerTab();
    if (l.gen !== gen || l.closed) return;
    if (owner === undefined) return this.attach(l); // became the owner meanwhile
    const rsid = `${this.id}.${this.nextSid++}`;
    l.at = owner;
    l.rsid = rsid;
    this.remote.set(rsid, { l, gen });
    if (l.kind === 'sub') {
      await this.call(owner, {
        t: 'sub',
        rsid,
        prefix: l.prefix,
        exact: l.exact,
        after: l.last,
      });
    } else {
      await this.call(owner, { t: 'esub', rsid, path: pathBytes(l.target as Topic) });
    }
  }

  private logical(kind: 'sub' | 'esub', target: StreamTarget): Logical {
    const topics = new Map<string, Topic>();
    let prefix: Uint8Array = new Uint8Array(0);
    let exact = false;
    if (isTopic(target)) {
      topics.set(bytes.hex(target.id), target);
      prefix = target.id;
      exact = true;
    } else {
      for (const t of target.topics ?? []) topics.set(bytes.hex(t.id), t);
      if (target.prefix instanceof Uint8Array) prefix = target.prefix;
      else if (target.prefix) {
        topics.set(bytes.hex(target.prefix.id), target.prefix);
        prefix = target.prefix.id;
      }
    }
    return {
      kind,
      target,
      topics,
      prefix,
      exact,
      last: undefined,
      onEvent: () => {},
      onError: undefined,
      gen: 0,
      inner: undefined,
      at: undefined,
      rsid: undefined,
      closed: false,
    };
  }

  private handle(l: Logical): SourceSub {
    return {
      close: async () => {
        if (l.closed) return;
        this.forget(l);
        if (l.at && l.rsid) this.post({ t: 'unsub', to: l.at, rsid: l.rsid });
        await l.inner?.close();
      },
    };
  }

  async subscribe(target: StreamTarget, opts: SubscribeOptions<StreamEvent>): Promise<SourceSub> {
    if (this.closed) throw new ZenError(0, 'closed', 'the database was closed');
    const l = this.logical('sub', target);
    // The start is resolved here, whichever tab owns (§9.2).
    l.last = opts.after ?? offsetAfterVersion(await this.db.fs.session.readVersion());
    l.onEvent = (opts.onEvent ?? (() => {})) as (e: never) => void;
    l.onError = opts.onError;
    this.subs.add(l);
    try {
      await this.attach(l);
    } catch (e) {
      if (!(e instanceof Moved)) {
        this.forget(l);
        throw e;
      }
    }
    return this.handle(l);
  }

  async subscribeEphemeral(target: StreamTarget, opts: EphOpts): Promise<SourceSub> {
    if (this.closed) throw new ZenError(0, 'closed', 'the database was closed');
    if (!isTopic(target)) throw new DbError('bad_value', 'ephemeral subscriptions take a topic');
    const l = this.logical('esub', target);
    l.onEvent = opts.onEvent as (e: never) => void;
    l.onError = opts.onError;
    this.subs.add(l);
    try {
      await this.attach(l);
    } catch (e) {
      if (!(e instanceof Moved)) {
        this.forget(l);
        throw e;
      }
    }
    return this.handle(l);
  }

  async publish(topic: Topic, data: Uint8Array): Promise<void> {
    const owner = await this.ownerTab();
    if (owner === undefined) return this.source().publish(topic, data);
    await this.call(owner, { t: 'epub', path: pathBytes(topic), data });
  }

  private bye(): void {
    this.post({ t: 'bye' });
  }

  close(): void {
    if (this.closed) return;
    this.bye();
    this.closed = true;
    this.unOwner();
    const g = globalThis as { removeEventListener?: (t: string, f: () => void) => void };
    g.removeEventListener?.('pagehide', this.onHide);
    for (const l of this.subs) l.closed = true;
    this.subs.clear();
    this.remote.clear();
    for (const c of this.calls.values())
      c.fail(new ZenError(0, 'closed', 'the database was closed'));
    this.calls.clear();
    this.host?.close();
    this.direct?.close();
    this.direct = undefined;
    this.chan.onmessage = null;
    this.chan.close();
  }
}

/** The owner tab went away mid-request: the next owner's announce re-attaches. */
class Moved extends Error {}

/** The owner's side: the other tabs' subscriptions, served from its stream. */
class ProxyHost {
  private readonly subs = new Map<string, { from: string; sub: Promise<SourceSub | undefined> }>();

  constructor(
    private readonly db: Db,
    private readonly src: DirectSource,
    private readonly post: (m: Omit<Wire, 'from'>) => void,
  ) {}

  handle(m: Wire): void {
    switch (m.t) {
      case 'hello':
        this.post({ t: 'owner' });
        return;
      case 'sub':
        this.open(m, () =>
          this.src.subscribe(
            { fs: this.db.fs, prefix: m.prefix as Uint8Array },
            {
              ...(m.after ? { after: m.after as Uint8Array } : {}),
              onEvent: (e) => {
                if (m.exact && !bytes.equal(e.topic, m.prefix)) return;
                if (e.opened) return; // no topics given: every event arrives sealed
                this.post({
                  t: 'ev',
                  to: m.from,
                  rsid: m.rsid,
                  topic: e.topic,
                  offset: e.offset,
                  ...(e.keyToken ? { keyToken: e.keyToken } : {}),
                  envelope: e.envelope,
                });
              },
              onError: (e) => this.ended(m, e),
            },
          ),
        );
        return;
      case 'esub':
        this.open(m, () =>
          this.src.subscribeEphemeral(this.db.topic(...(m.path as Uint8Array[])), {
            onEvent: (e) =>
              this.post({
                t: 'eev',
                to: m.from,
                rsid: m.rsid,
                topic: e.topic,
                sender: e.sender,
                hlc: e.hlc,
                data: e.data,
              }),
            onError: (e) => this.ended(m, e),
          }),
        );
        return;
      case 'unsub': {
        const s = this.subs.get(m.rsid);
        this.subs.delete(m.rsid);
        void s?.sub.then((x) => x?.close()).catch(() => {});
        return;
      }
      case 'epub':
        this.src.publish(this.db.topic(...(m.path as Uint8Array[])), m.data).then(
          () => this.post({ t: 'ok', to: m.from, rid: m.rid }),
          (e) => this.refuse(m, e),
        );
        return;
    }
  }

  private open(m: Wire, fn: () => Promise<SourceSub>): void {
    const sub = fn().then(
      (s) => {
        this.post({ t: 'ok', to: m.from, rid: m.rid });
        // Unsubscribed (or the tab left) while subscribing.
        if (!this.subs.has(m.rsid)) void s.close().catch(() => {});
        return s;
      },
      (e) => {
        this.subs.delete(m.rsid);
        this.refuse(m, e);
        return undefined;
      },
    );
    this.subs.set(m.rsid, { from: m.from, sub });
  }

  private refuse(m: Wire, e: unknown): void {
    this.post({
      t: 'err',
      to: m.from,
      rid: m.rid,
      code: e instanceof ZenError || e instanceof DbError ? e.code : 'proxy',
      message: e instanceof Error ? e.message : String(e),
    });
  }

  private ended(m: Wire, e: ZenError): void {
    if (!this.subs.delete(m.rsid)) return;
    this.post({ t: 'end', to: m.from, rsid: m.rsid, code: e.code, message: e.message });
  }

  /** A tab left: drop its subscriptions. */
  drop(tab: string): void {
    for (const [k, s] of this.subs) {
      if (s.from !== tab) continue;
      this.subs.delete(k);
      void s.sub.then((x) => x?.close()).catch(() => {});
    }
  }

  close(): void {
    for (const s of this.subs.values()) void s.sub.then((x) => x?.close()).catch(() => {});
    this.subs.clear();
  }
}
