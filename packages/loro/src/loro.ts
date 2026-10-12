// A Loro document on a zen-db topic (spec/zendb.md §20): updates are
// messages on ("zen", "loro", ns, name), a snapshot sits in the `$loro`
// row, and the document's state and unsent edits are kept on the local
// store, so it opens and is edited offline.
import { bytes, type SealedStore, type Topic, type TopicEvent, ZenError } from '@zen/client';
import { appendMsg, cbor, type Db, DbError, type OnSubscription, openMessage } from '@zen/db';
import { decodeImportBlobMeta, LoroDoc } from 'loro-crdt';

/** The message type of a Loro update (§20.1). */
export const LORO_UPDATE = 'zen.loro.update';

/** Options of `openLoro`. */
export interface LoroOptions {
  /** Compact after this many events since the stored snapshot (default 100). */
  snapshotEvery?: number;
  /** …or this many bytes of updates (default 1 MiB). */
  snapshotBytes?: number;
  /** Gather local updates this long before emitting them (ms, default 50). */
  batchMs?: number;
  /** Write the kept state this long after a change (ms, default 500). */
  keepMs?: number;
}

const ZERO12 = new Uint8Array(12);
/** At most this many bytes of updates per emit transaction. */
const BATCH_BYTES = 256 * 1024;
const backoff = (i: number) => Math.min(250 * 2 ** i, 30_000);

const isNetwork = (e: unknown) =>
  e instanceof ZenError && (e.code === 'network' || e.code === 'commit_unknown');
const isTooLarge = (e: unknown) =>
  (e instanceof DbError && e.code === 'too_large') ||
  (e instanceof ZenError && (e.status === 413 || e.code === 'too_large'));
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const newer = (a: Uint8Array | undefined, b: Uint8Array | undefined) =>
  !!a && (!b || bytes.compare(a, b) > 0);

function u32(n: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n);
  return b;
}

function u64(n: number): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n));
  return b;
}

/**
 * Open (or create) the Loro document `name` of a database. It resolves
 * once the kept state and the outbox are loaded; `ready` once the server
 * was reached (or couldn't be).
 */
export async function openLoro(db: Db, name: string, opts: LoroOptions = {}): Promise<LoroHandle> {
  const h = new LoroHandle(db, name, opts);
  await h.start();
  return h;
}

/** An open Loro document. */
export class LoroHandle {
  /** The document: edit it and `commit()` as usual. */
  readonly doc = new LoroDoc();
  /** The document's topic. */
  readonly topic: Topic;
  /** Resolves once loaded: caught up with the server, or tried and offline. */
  readonly ready: Promise<void>;

  private readyOk!: () => void;
  private readonly local: SealedStore | undefined;
  private readonly keptKey: Uint8Array;
  private readonly outPrefix: Uint8Array;
  /** The last event imported. */
  private offset: Uint8Array | undefined;
  /** The `$loro` row's offset, as last seen. */
  private snapOffset: Uint8Array | undefined;
  /** Events and bytes imported since `snapOffset`. */
  private since = { n: 0, bytes: 0 };
  /** Unsent local updates, by outbox key (hex). */
  private readonly outbox = new Map<string, Uint8Array>();
  private sub: OnSubscription | undefined;
  private closed = false;
  private isOnline = false;
  private noCompact = false;
  private compacting: Promise<void> | undefined;
  private sending: Promise<void> | undefined;
  private sendTimer: ReturnType<typeof setTimeout> | undefined;
  private keepTimer: ReturnType<typeof setTimeout> | undefined;
  private keeping: Promise<void> = Promise.resolve();
  private readonly wakers = new Set<() => void>();
  private loading: Promise<void> | undefined;
  private unLocal: (() => void) | undefined;
  private unOwner: (() => void) | undefined;
  private readonly changes = new Set<() => void>();
  private readonly flushes = new Set<{ ok: () => void; fail: (e: unknown) => void }>();
  private storeOps: Promise<unknown> = Promise.resolve();

  /** @internal */
  constructor(
    private readonly db: Db,
    readonly name: string,
    private readonly opts: LoroOptions,
  ) {
    this.topic = db.topic('zen', 'loro', db.ns, name);
    this.local = db.local;
    this.keptKey = bytes.concat(bytes.utf8('d'), bytes.utf8(name));
    const n = bytes.utf8(name);
    this.outPrefix = bytes.concat(bytes.utf8('o'), u32(n.length), n);
    this.ready = new Promise((r) => {
      this.readyOk = r;
    });
  }

  private get storeName(): string {
    return `loro/${this.db.ns}`;
  }

  /** @internal Load what is kept, then start loading from the server. */
  async start(): Promise<void> {
    if (this.local) {
      try {
        const k = await this.local.get(this.storeName, this.keptKey);
        if (k) {
          const m = cbor.decode(k);
          if (!(m instanceof Map) || !(m.get(2) instanceof Uint8Array))
            throw new Error('bad record');
          const off = m.get(1);
          this.doc.import(m.get(2) as Uint8Array);
          if (off instanceof Uint8Array) this.offset = off;
        }
        for await (const [key, u] of this.local.list(this.storeName)) {
          if (!startsWith(key, this.outPrefix)) continue;
          this.doc.import(u);
          this.outbox.set(bytes.hex(key), u);
        }
      } catch (e) {
        this.db.warn(`loro ${this.name}: the kept state: ${message(e)}`);
      }
    }
    this.unLocal = this.doc.subscribeLocalUpdates((u) => this.edited(u));
    this.unOwner = this.db.onOwner(() => this.maybeCompact());
    this.loading = this.load();
    if (this.outbox.size) this.later();
  }

  /** Whether the last call to the server got through. */
  get online(): boolean {
    return this.isOnline;
  }

  /** The offset of the last event imported (undefined before any). */
  get lastOffset(): Uint8Array | undefined {
    return this.offset;
  }

  /** Local updates not yet emitted. */
  get pending(): number {
    return this.outbox.size;
  }

  /** Call `fn` after updates from elsewhere were imported. Returns an unsubscribe. */
  onChange(fn: () => void): () => void {
    this.changes.add(fn);
    return () => this.changes.delete(fn);
  }

  /**
   * Resolves once every local update made so far is emitted. Offline it
   * rejects with `offline` (the updates stay in the outbox).
   */
  flush(): Promise<void> {
    if (!this.outbox.size) return Promise.resolve();
    const p = new Promise<void>((ok, fail) => this.flushes.add({ ok, fail }));
    this.kick();
    return p;
  }

  /** Write a snapshot to the `$loro` row now (§20.5). */
  async compact(): Promise<void> {
    await this.compacting;
    const p = this.writeSnapshot(true);
    this.compacting = p.then(
      () => {},
      () => {},
    );
    try {
      await p;
    } finally {
      this.compacting = undefined;
    }
  }

  /** Stop: unsent updates stay in the outbox of the local store. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.unLocal?.();
    this.unOwner?.();
    clearTimeout(this.sendTimer);
    this.wakeAll();
    for (const f of this.flushes) f.fail(new DbError('closed', 'the document was closed'));
    this.flushes.clear();
    await this.sub?.close().catch(() => {});
    await Promise.allSettled([this.loading, this.sending, this.compacting]);
    clearTimeout(this.keepTimer);
    this.keepTimer = undefined;
    await this.keep();
    await this.storeOps;
  }

  // ------------------------------------------------------------ loading

  private async load(): Promise<void> {
    for (let i = 0; !this.closed; i++) {
      try {
        await this.catchUp();
        this.setOnline(true);
        this.readyOk();
        return;
      } catch (e) {
        if (this.closed) break;
        if (isNetwork(e)) this.setOnline(false);
        else this.db.warn(`loro ${this.name}: loading: ${message(e)}`);
        this.readyOk();
        await this.sleep(backoff(i));
      }
    }
    this.readyOk();
  }

  /** The `$loro` row, the tail, then live events (§20.2). */
  private async catchUp(): Promise<void> {
    const row = await this.db.table('$loro').get(this.name);
    const off = row?.offset instanceof Uint8Array ? row.offset : undefined;
    if (off && row?.snapshot instanceof Uint8Array && newer(off, this.offset)) {
      this.doc.import(row.snapshot);
      this.offset = off;
      this.since = { n: 0, bytes: 0 };
      this.notify();
      this.later('keep');
    }
    if (newer(off, this.snapOffset)) this.snapOffset = off;
    for await (const e of this.topic.read(this.offset ? { after: this.offset } : {})) {
      if (this.closed) return;
      await this.imported(e, await this.open(e));
    }
    if (this.closed) return;
    this.sub = await this.db.on(this.topic, (m) => this.imported(m, m), {
      after: this.offset ?? ZERO12,
      onError: (e) => {
        if (!(e instanceof DbError && e.code === 'poisoned')) {
          this.db.warn(`loro ${this.name}: ${message(e)}`);
        }
        // The stream ended (not just an unreadable message): load again.
        setTimeout(() => {
          if (this.closed || !this.sub || this.sub.active) return;
          this.sub = undefined;
          this.loading = this.load();
        }, 0);
      },
    });
    // Closed while subscribing: `close` saw no subscription to stop.
    if (this.closed) await this.sub.close();
  }

  private async open(e: TopicEvent) {
    try {
      return await openMessage(this.db, this.topic, e);
    } catch (err) {
      if (!(err instanceof DbError && err.code === 'poisoned')) throw err;
      this.db.warn(`loro ${this.name}: an unreadable update: ${err.message}`);
      return undefined;
    }
  }

  /** Import one event's update, in log order; the offset moves past it either way. */
  private imported(e: { offset: Uint8Array }, m: { type: string; body: unknown } | undefined) {
    if (this.closed || !newer(e.offset, this.offset)) return;
    this.offset = e.offset;
    if (m?.type === LORO_UPDATE && m.body instanceof Uint8Array) {
      try {
        this.doc.import(m.body);
        this.since.n++;
        this.since.bytes += m.body.length;
        this.notify();
      } catch (err) {
        this.db.warn(`loro ${this.name}: an update that doesn't import: ${message(err)}`);
      }
    }
    this.later('keep');
    this.maybeCompact();
  }

  private notify(): void {
    for (const f of this.changes) {
      try {
        f();
      } catch (e) {
        this.db.warn(`loro ${this.name}: onChange: ${message(e)}`);
      }
    }
  }

  // ------------------------------------------------------------ the outbox

  private edited(u: Uint8Array): void {
    if (this.closed) return;
    const key = bytes.concat(this.outPrefix, u64(Date.now()), bytes.randomBytes(4));
    this.outbox.set(bytes.hex(key), u);
    this.storeOp((l) => l.put(this.storeName, key, u));
    this.later();
    this.later('keep');
  }

  /** Run a store write, in order with the others. */
  private storeOp(f: (l: SealedStore) => Promise<unknown>): void {
    const l = this.local;
    if (!l) return;
    this.storeOps = this.storeOps.then(() =>
      f(l).catch((e) => this.db.warn(`loro ${this.name}: the local store: ${message(e)}`)),
    );
  }

  private later(what: 'send' | 'keep' = 'send'): void {
    if (this.closed) return;
    if (what === 'keep') {
      if (!this.local || this.keepTimer) return;
      this.keepTimer = setTimeout(() => {
        this.keepTimer = undefined;
        void this.keep();
      }, this.opts.keepMs ?? 500);
      return;
    }
    if (this.sendTimer || this.sending) return;
    this.sendTimer = setTimeout(() => {
      this.sendTimer = undefined;
      this.kick();
    }, this.opts.batchMs ?? 50);
  }

  /** Start the sender now (or wake it from its backoff). */
  private kick(): void {
    clearTimeout(this.sendTimer);
    this.sendTimer = undefined;
    this.wakeAll();
    if (this.sending || this.closed) return;
    this.sending = this.send().finally(() => {
      this.sending = undefined;
      if (this.outbox.size && !this.closed) this.later();
    });
  }

  private async send(): Promise<void> {
    let single = false;
    for (let i = 0; this.outbox.size && !this.closed; ) {
      const batch = take(this.outbox, single ? 1 : Number.POSITIVE_INFINITY);
      const bodies = this.merge(batch.map(([, u]) => u));
      try {
        await this.db.transaction(async (tx) => {
          for (const body of bodies) {
            appendMsg(
              tx,
              this.topic,
              { type: LORO_UPDATE, id: bytes.randomBytes(16), body },
              undefined,
            );
          }
        });
      } catch (e) {
        if (this.closed) return;
        if (isTooLarge(e) && batch.length > 1) {
          single = true;
          continue;
        }
        if (isTooLarge(e)) {
          // One update too large for any commit can never be sent.
          this.db.warn(`loro ${this.name}: an update too large to send was dropped`);
          this.sent(batch.map(([k]) => k));
          continue;
        }
        if (isNetwork(e)) this.setOnline(false);
        else this.db.warn(`loro ${this.name}: sending: ${message(e)}`);
        for (const f of this.flushes) f.fail(isNetwork(e) ? new DbError('offline', message(e)) : e);
        this.flushes.clear();
        await this.sleep(backoff(i++));
        continue;
      }
      i = 0;
      single = false;
      this.setOnline(true);
      this.sent(batch.map(([k]) => k));
    }
  }

  /** Emitted: forget the outbox records. */
  private sent(keys: string[]): void {
    for (const k of keys) this.outbox.delete(k);
    this.storeOp((l) =>
      l.delete(
        this.storeName,
        keys.map((k) => bytes.fromHex(k)),
      ),
    );
    if (!this.outbox.size) {
      for (const f of this.flushes) f.ok();
      this.flushes.clear();
    }
  }

  /**
   * One update holding the ops of these (exported from the document), or
   * the updates themselves when they don't merge.
   */
  private merge(us: Uint8Array[]): Uint8Array[] {
    if (us.length < 2) return us;
    try {
      const have = this.doc.oplogVersion().toJSON();
      const ranges = new Map<string, [number, number][]>();
      for (const u of us) {
        const meta = decodeImportBlobMeta(u, false);
        const end = meta.partialEndVersionVector.toJSON();
        for (const [peer, start] of meta.partialStartVersionVector.toJSON()) {
          const e = end.get(peer) ?? start;
          if ((have.get(peer) ?? 0) < e) return us;
          if (e > start) ranges.set(peer, [...(ranges.get(peer) ?? []), [start, e]]);
        }
      }
      const spans: { id: { peer: `${number}`; counter: number }; len: number }[] = [];
      for (const [peer, rs] of ranges) {
        rs.sort((a, b) => a[0] - b[0]);
        let [s, e] = rs[0]!;
        for (const [a, b] of rs.slice(1)) {
          if (a <= e) e = Math.max(e, b);
          else {
            spans.push({ id: { peer: peer as `${number}`, counter: s }, len: e - s });
            [s, e] = [a, b];
          }
        }
        spans.push({ id: { peer: peer as `${number}`, counter: s }, len: e - s });
      }
      return [this.doc.export({ mode: 'updates-in-range', spans })];
    } catch {
      return us;
    }
  }

  // ------------------------------------------------------------ kept state

  /** Write the snapshot and the offset it covers (§20.4). */
  private keep(): Promise<void> {
    const l = this.local;
    if (!l) return this.keeping;
    const snapshot = this.doc.export({ mode: 'snapshot' });
    const rec = new Map<number, Uint8Array>([[2, snapshot]]);
    if (this.offset) rec.set(1, this.offset);
    const b = cbor.encode(rec);
    this.storeOp((s) => s.put(this.storeName, this.keptKey, b));
    this.keeping = this.storeOps.then(() => {});
    return this.keeping;
  }

  // ------------------------------------------------------------ compaction

  private maybeCompact(): void {
    if (this.closed || this.noCompact || this.compacting || !this.db.isOwner) return;
    const every = this.opts.snapshotEvery ?? 100;
    const max = this.opts.snapshotBytes ?? 1024 * 1024;
    if (this.since.n < every && this.since.bytes < max) return;
    this.compacting = this.writeSnapshot(false).finally(() => {
      this.compacting = undefined;
    });
  }

  /** Put a snapshot at the current offset in `$loro`, if the row's is older (§20.5). */
  private async writeSnapshot(explicit: boolean): Promise<void> {
    if (this.noCompact && !explicit) return;
    const offset = this.offset;
    if (!offset) return;
    const snapshot = this.doc.export({ mode: 'snapshot' });
    const since = { ...this.since };
    try {
      const wrote = await this.db.transaction(async (tx) => {
        const t = tx.table('$loro');
        const row = await t.get(this.name);
        const at = row?.offset instanceof Uint8Array ? row.offset : undefined;
        if (!newer(offset, at)) return at;
        await t.put({ name: this.name, offset, snapshot });
        return offset;
      });
      if (wrote && newer(wrote, this.snapOffset)) {
        this.snapOffset = wrote;
        this.since = {
          n: Math.max(0, this.since.n - since.n),
          bytes: Math.max(0, this.since.bytes - since.bytes),
        };
      }
    } catch (e) {
      if (isTooLarge(e)) {
        if (!this.noCompact) {
          this.db.warn(
            `loro ${this.name}: the snapshot is too large for one commit; the stored one stays (TD-LORO-LARGE-SNAPSHOT)`,
          );
        }
        this.noCompact = true;
        if (explicit) throw new DbError('too_large', 'the snapshot is too large for one commit');
        return;
      }
      if (explicit) throw e;
      if (!isNetwork(e)) this.db.warn(`loro ${this.name}: compacting: ${message(e)}`);
    }
  }

  // ------------------------------------------------------------ helpers

  private setOnline(v: boolean): void {
    this.isOnline = v;
  }

  /** Sleep, woken early by `kick` (a flush or a new edit) or `close`. */
  private sleep(ms: number): Promise<void> {
    return new Promise((r) => {
      const done = () => {
        clearTimeout(t);
        this.wakers.delete(done);
        r();
      };
      const t = setTimeout(done, ms);
      this.wakers.add(done);
    });
  }

  private wakeAll(): void {
    for (const w of [...this.wakers]) w();
  }
}

function startsWith(b: Uint8Array, p: Uint8Array): boolean {
  return b.length >= p.length && bytes.equal(b.subarray(0, p.length), p);
}

/** The first outbox records, up to `n` and `BATCH_BYTES` (at least one). */
function take(m: Map<string, Uint8Array>, n: number): [string, Uint8Array][] {
  const out: [string, Uint8Array][] = [];
  let size = 0;
  for (const [k, u] of [...m].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))) {
    if (out.length >= n || (out.length && size + u.length > BATCH_BYTES)) break;
    out.push([k, u]);
    size += u.length;
  }
  return out;
}
