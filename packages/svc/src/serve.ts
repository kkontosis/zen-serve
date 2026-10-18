// The handler runtime of a relayed service (spec/svc.md §3–§8, §10): a
// handler lease, claims, pushed input, one zen-db transaction per step,
// reveals after the commit, and re-reveals and rollback checks on a claim.
// A layer (layer.ts: raw, or TLS) sits between the bytes and the handler.
import {
  bytes,
  isCode,
  REVEAL_BAD_KEY,
  type Service,
  type Stream,
  type Subscription,
  type SvcPush,
  ZenError,
  zw,
} from '@zen/client';
import type { Db, DbTransaction, DbTxnOptions } from '@zen/db';
import type { SvcClaim, SvcInfo, SvcPeer, SvcReveal, SvcStep } from '@zen/wasm';
import { SVC_ABORT, SVC_CLOSE, SVC_FIN } from './flags.js';
import { Input } from './input.js';
import {
  type Engine,
  type Finished,
  type HandlerOut,
  type Layer,
  type LayerConn,
  type Prepared,
  raw,
  type TlsInfo,
} from './layer.js';
import { decodeState, encodeState } from './state.js';

/** Claim flags: EOF, then the step flags of `ra` shifted by one (keyspace.md §3.9). */
const C_EOF = 1;
const C_FIN = 2;
const C_CLOSE = 4;
const C_ABORT = 8;
const C_RESUMABLE = 16;

/** The bytes a sealed record adds to its raw bytes, with room to spare (api.md §14.5). */
const RECORD_ROOM = 130;

const EMPTY = new Uint8Array(0);

/** One step of a connection, as the handler sees it. */
export interface RawStep {
  readonly conn: Uint8Array;
  readonly peer: SvcPeer;
  /** The input not yet acked, from `in_acked`. */
  readonly input: Uint8Array;
  /** The peer sent FIN: `input` is all there is. */
  readonly eof: boolean;
  /** The application's state, carried across steps and takeovers (null: none). */
  state: Uint8Array | null;
  /** Consume the first `n` bytes of `input`. Ack only whole protocol units. */
  ack(n: number): void;
  /** Send bytes, after those of earlier steps. */
  write(data: Uint8Array | string): void;
  /** Shut down the write side after the writes (FIN). Input can still be acked. */
  end(): void;
  /** Close the connection after the writes. */
  close(): void;
  /** Reset the connection. */
  abort(): void;
  /** Under `tls()`: the connection's TLS parameters. */
  readonly tls?: TlsInfo;
}

/** A raw handler: runs inside the step's transaction; its db writes commit with the step. */
export type RawHandler = (c: RawStep, tx: DbTransaction) => void | Promise<void>;

/** Options of `serve`. */
export interface ServeOptions {
  /** The service's topic path, e.g. `['svc', 'echo']`: its topic id is the service id. */
  service: (string | Uint8Array)[];
  handler: RawHandler;
  /** Between the bytes and the handler: `raw()` (the default) or `tls(…)`. */
  layer?: Layer;
  /** Connections held at once (default 64). */
  maxConns?: number;
  /** Lease lifetime (default 5,000 ms): how long a takeover waits after this handler dies. */
  leaseTtlMs?: number;
  /** Lease renewal period (default 1,000 ms). */
  renewMs?: number;
  /** Errors: a failed handler, a refused claim, a lost lease. */
  onError?: (e: unknown, conn?: Uint8Array) => void;
  /** Options of each step's transaction. */
  txn?: DbTxnOptions;
}

const pause = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((r) => {
    const t = setTimeout(r, ms);
    signal?.addEventListener('abort', () => {
      clearTimeout(t);
      r();
    });
  });

/** A connection held under the lease. */
interface Conn {
  readonly key: string;
  readonly conn: Uint8Array;
  token: bigint;
  peer: SvcPeer;
  input: Input;
  outHead: bigint;
  /** Claim flags, as last known. */
  flags: number;
  app: Uint8Array | null;
  /** A state is stored: every cursor move must carry a new one. */
  hasState: boolean;
  /** The EOF's offset, once pushed. */
  eofAt: bigint | undefined;
  /** What the handler saw last: input up to here, and whether the EOF. */
  seenTo: bigint;
  seenEof: boolean;
  /** A step is due even without new input (a fresh connection: the server may speak first). */
  due: boolean;
  busy: boolean;
  gone: boolean;
  /** Since when input past a gap has waited (local ms; 0: no gap). */
  gapAt: number;
  /** When the connection last changed (local ms). */
  at: number;
  /** Reveals of this connection, in order. */
  reveals: Promise<void>;
  /** The layer's side of the connection. */
  layer: LayerConn;
  /** A step prepared and not committed yet (reused when it runs again). */
  prep: Step0 | undefined;
}

/** A prepared step, with the input it was prepared from. */
interface Step0 extends Prepared {
  inEnd: bigint;
  rawEof: boolean;
}

/** Input pushed for a connection whose claim hasn't arrived yet. */
interface Early {
  at: number;
  /** The offsets the chunks span (pushes repeat bytes, so not their sum). */
  lo: bigint;
  hi: bigint;
  chunks: Extract<SvcPush, { type: 'input' }>[];
}

/** The outcome of a handler run, built into a step. */
interface Built {
  step: SvcStep | undefined;
  reveals: SvcReveal[];
  inTo: bigint;
  outHead: bigint;
  app: Uint8Array | null;
  sealed: boolean;
  flags: number;
}

class HandlerError extends Error {
  constructor(override readonly cause: unknown) {
    super(cause instanceof Error ? cause.message : String(cause));
  }
}

class Step implements RawStep {
  acked = 0;
  readonly out: Uint8Array[] = [];
  outBytes = 0;
  endFlag = 0;

  constructor(
    readonly conn: Uint8Array,
    readonly peer: SvcPeer,
    readonly input: Uint8Array,
    readonly eof: boolean,
    public state: Uint8Array | null,
    private readonly finSent: boolean,
    private readonly outWindow: number,
    readonly tls: TlsInfo | undefined,
  ) {}

  ack(n: number): void {
    if (!Number.isInteger(n) || n < 0 || n > this.input.length) {
      throw new RangeError(`ack(${n}): ${this.input.length} input bytes`);
    }
    this.acked = Math.max(this.acked, n);
  }

  private open(what: string): void {
    if (this.endFlag) throw new Error(`${what} after end, close or abort`);
    if (this.finSent) throw new Error(`${what} after the connection's FIN`);
  }

  write(data: Uint8Array | string): void {
    this.open('write');
    const d = typeof data === 'string' ? bytes.utf8(data) : data;
    if (this.outBytes + d.length > this.outWindow) {
      throw new RangeError(`a step writes at most ${this.outWindow} bytes (out_window)`);
    }
    if (d.length) {
      this.out.push(d);
      this.outBytes += d.length;
    }
  }

  end(): void {
    this.open('end');
    this.endFlag = SVC_FIN;
  }

  close(): void {
    this.closing('close');
    this.endFlag = SVC_CLOSE;
  }

  abort(): void {
    this.closing('abort');
    this.endFlag = SVC_ABORT;
  }

  /** Close or abort may replace this step's FIN, not an earlier step's. */
  private closing(what: string): void {
    if (this.endFlag && this.endFlag !== SVC_FIN) throw new Error(`${what} after close or abort`);
    if (this.finSent) throw new Error(`${what} after the connection's FIN`);
  }
}

/** A running service handler: `serve(db, opts)`. */
export class ServeHandle {
  private ctl: AbortController | undefined;
  private stopped = false;
  private readonly unOwner: () => void;
  private readonly svc: Service;
  private engine: Engine | undefined;
  private info: SvcInfo | undefined;
  private maxRaw = 0;
  private stream: Stream | undefined;
  private sub: Subscription<SvcPush> | undefined;
  private lease: Uint8Array | undefined;
  private readonly held = new Map<string, Conn>();
  private readonly early = new Map<string, Early>();
  /** The highest cursors seen per connection in this process (svc.md §6, rollback). */
  private readonly seen = new Map<string, { inAcked: bigint; outHead: bigint }>();
  private readonly busy = new Set<Promise<unknown>>();
  private claiming = false;
  private claimAgain = false;
  /** Ends the current lease's generation. */
  private lost: ((e: unknown) => void) | undefined;

  /** @internal */
  constructor(
    private readonly db: Db,
    private readonly opts: ServeOptions,
  ) {
    this.svc = db.fs.service(...opts.service);
    this.unOwner = db.onOwner(() => this.start());
  }

  /** The service id. */
  get id(): Uint8Array {
    return this.svc.id;
  }

  /** Connections held now. */
  get conns(): number {
    return this.held.size;
  }

  /** Whether it runs (this instance owns the database and it wasn't stopped). */
  get running(): boolean {
    return this.ctl !== undefined && !this.ctl.signal.aborted;
  }

  /**
   * Stop: no more claims, wait for the steps and reveals in flight, then
   * release the lease (the server requeues or aborts its connections).
   */
  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    this.unOwner();
    this.ctl?.abort();
    while (this.busy.size) await Promise.allSettled([...this.busy]);
    await this.sub?.close().catch(() => {});
    this.stream?.close();
    if (this.lease) await this.svc.releaseLease(this.lease).catch(() => {});
    for (const c of this.held.values()) c.layer.free();
    this.held.clear();
    this.engine?.close();
    this.engine = undefined;
    this.svc.close();
  }

  private get maxConns(): number {
    return this.opts.maxConns ?? 64;
  }

  private get ttl(): number {
    return this.opts.leaseTtlMs ?? 5000;
  }

  private report(e: unknown, conn?: Uint8Array): void {
    if (this.opts.onError) this.opts.onError(e, conn);
    else {
      const msg = e instanceof Error ? e.message : String(e);
      this.db.warn(`svc ${this.opts.service.join('/')}: ${msg}`);
    }
  }

  /** Keep a task until it settles (`stop` waits for them); it never rejects. */
  private track(p: Promise<void>): Promise<void> {
    const q = p.catch((e) => this.report(e));
    this.busy.add(q);
    void q.finally(() => this.busy.delete(q));
    return q;
  }

  private start(): void {
    if (this.stopped || this.ctl) return;
    this.ctl = new AbortController();
    void this.run(this.ctl.signal);
  }

  /** Retry `fn` on network errors (and `retry`), with backoff, until it succeeds or the signal fires. */
  private async retry<T>(
    signal: AbortSignal,
    fn: () => Promise<T>,
    retry: (e: unknown) => boolean = () => false,
  ): Promise<T | undefined> {
    for (let backoff = 100; !signal.aborted; backoff = Math.min(backoff * 2, 5000)) {
      try {
        return await fn();
      } catch (e) {
        if (signal.aborted) return undefined;
        if (!isCode(e, 'network') && !retry(e)) throw e;
        await pause(backoff, signal);
      }
    }
    return undefined;
  }

  private async run(signal: AbortSignal): Promise<void> {
    try {
      // A layer that can't start (a bad certificate) claims nothing.
      this.engine ??= await (this.opts.layer ?? raw()).open();
      if (signal.aborted) return;
      const setup = await this.retry(signal, async () => {
        const info = await this.svc.info();
        const limits = (await this.db.fs.session.client.info()).limits;
        const stream = await this.db.fs.session.stream({ onError: (e) => this.report(e) });
        return { info, limits, stream };
      });
      if (!setup) return;
      this.info = setup.info;
      this.maxRaw = setup.limits.max_value_bytes - RECORD_ROOM;
      this.stream = setup.stream;
      if (signal.aborted) return;
      const sweep = setInterval(() => this.sweep(signal), 1000);
      (sweep as { unref?: () => void }).unref?.();
      try {
        while (!signal.aborted) await this.generation(signal);
      } finally {
        clearInterval(sweep);
      }
    } catch (e) {
      // Nothing above heals by retrying (a missing service, no rights).
      if (!signal.aborted) this.report(e);
    }
  }

  /** One handler lease: claims and steps until the lease is lost or the signal fires. */
  private async generation(signal: AbortSignal): Promise<void> {
    const lease = zw.randomBytes(16);
    let lose!: (e: unknown) => void;
    const ended = new Promise<unknown>((r) => {
      lose = r;
    });
    this.lost = lose;
    const got = await this.retry(signal, () => this.svc.lease(lease, this.ttl));
    if (got === undefined) return;
    this.lease = lease;
    let sub: Subscription<SvcPush> | undefined;
    try {
      sub = await this.retry(signal, () =>
        this.stream!.subscribeSvc(this.db.fs, this.svc.id, lease, {
          onEvent: (p) => this.pushed(p, signal),
          onError: lose,
        }),
      );
    } catch (e) {
      this.report(e);
      this.lease = undefined;
      await this.svc.releaseLease(lease).catch(() => {});
      await pause(1000, signal);
      return;
    }
    if (!sub) return;
    this.sub = sub;
    const renew = this.renewLoop(lease, signal);
    void this.claim(signal);
    const why = await Promise.race([
      ended,
      new Promise((r) => signal.addEventListener('abort', r)),
    ]);
    if (signal.aborted) {
      await renew.catch(() => {});
      return;
    }
    // The lease is gone: its connections are requeued or aborted on the server.
    this.report(
      why instanceof Error ? why : new ZenError(0, 'not_leader', 'the handler lease was lost'),
    );
    for (const c of this.held.values()) {
      c.gone = true;
      c.layer.free();
    }
    this.held.clear();
    this.lease = undefined;
    this.sub = undefined;
    await sub.close().catch(() => {});
    await renew.catch(() => {});
  }

  private async renewLoop(lease: Uint8Array, signal: AbortSignal): Promise<void> {
    let ok = Date.now();
    const every = this.opts.renewMs ?? 1000;
    while (!signal.aborted && this.lease === lease) {
      await pause(every, signal);
      if (signal.aborted || this.lease !== lease) return;
      try {
        await this.svc.lease(lease, this.ttl);
        ok = Date.now();
      } catch (e) {
        if (isCode(e, 'not_leader') || Date.now() - ok > this.ttl) {
          this.lost?.(e);
          return;
        }
      }
    }
  }

  // ---------------------------------------------------------------- claims

  private async claim(signal: AbortSignal): Promise<void> {
    if (this.claiming) {
      this.claimAgain = true;
      return;
    }
    this.claiming = true;
    try {
      do {
        this.claimAgain = false;
        for (;;) {
          const lease = this.lease;
          const room = Math.min(this.maxConns - this.held.size, 64);
          if (!lease || room <= 0 || signal.aborted) break;
          let got: SvcClaim[];
          try {
            got = await this.svc.next(lease, { limit: room });
          } catch (e) {
            if (isCode(e, 'not_leader')) {
              if (lease === this.lease) this.lost?.(e);
            } else if (!signal.aborted) this.report(e);
            break;
          }
          if (lease !== this.lease) break;
          for (const c of got) this.claimed(c, signal);
          if (got.length < room) break;
        }
      } while (this.claimAgain && !signal.aborted);
    } finally {
      this.claiming = false;
    }
  }

  /** Take a claimed connection: check, open its state, re-reveal, then step. */
  private claimed(c: SvcClaim, signal: AbortSignal): void {
    const key = bytes.hex(c.conn);
    const conn: Conn = {
      key,
      conn: c.conn,
      token: c.token,
      peer: c.peer,
      input: new Input(c.in_acked),
      outHead: c.out_head,
      flags: c.flags,
      app: null,
      hasState: false,
      eofAt: undefined,
      seenTo: c.in_acked,
      seenEof: false,
      due: c.in_acked === 0n && c.out_head === 0n && !c.state,
      busy: false,
      gone: false,
      gapAt: 0,
      at: Date.now(),
      reveals: Promise.resolve(),
      layer: this.engine!.conn(),
      prep: undefined,
    };
    this.held.set(key, conn);
    try {
      this.load(conn, c, c.in_acked === 0n && c.out_head === 0n);
    } catch (e) {
      this.report(e, c.conn);
      void this.giveUp(conn, true);
      return;
    }
    const e = this.early.get(key);
    this.early.delete(key);
    for (const p of e?.chunks ?? []) this.input(conn, p);
    this.schedule(conn, signal);
  }

  /**
   * Apply a claim (or `/v1/svc/state`) to a connection; `same`: its cursors
   * are those this side last saw. Throws when it can't be served.
   */
  private load(conn: Conn, c: SvcClaim, same: boolean): void {
    const seen = this.seen.get(conn.key);
    if (seen && (c.in_acked < seen.inAcked || c.out_head < seen.outHead)) {
      throw new ZenError(
        0,
        'rollback',
        `the connection's cursors went back (${c.in_acked}, ${c.out_head} < ${seen.inAcked}, ${seen.outHead})`,
      );
    }
    this.see(conn.key, c.in_acked, c.out_head);
    conn.prep = undefined;
    conn.token = c.token;
    conn.flags = c.flags;
    conn.outHead = c.out_head;
    conn.input.advance(c.in_acked);
    if (conn.input.base !== c.in_acked) conn.input = new Input(c.in_acked);
    if (conn.seenTo < c.in_acked) conn.seenTo = c.in_acked;
    if (c.state) {
      const pt = this.svc.keysFor(c.state).openState(conn.conn, c.in_acked, c.out_head, c.state);
      const st = decodeState(pt);
      const engine = this.engine!.name;
      if (st.engine !== engine)
        throw new ZenError(0, 'format', `a ${st.engine} state on a ${engine} handler`);
      conn.layer.load(st.engineState.length ? st.engineState : null, same);
      conn.app = st.app;
      conn.hasState = true;
    } else {
      conn.layer.load(null, same);
    }
    if (c.flags & (C_CLOSE | C_ABORT)) {
      // Closing already: nothing more to do but the reveals.
      conn.gone = true;
      this.held.delete(conn.key);
    }
    if (c.pending.length) {
      const reveals = c.pending.map((p) => ({
        conn: conn.conn,
        off: p.off,
        key: this.svc.keysFor(p.escrow).openKeyEscrow(conn.conn, p.off, p.escrow),
      }));
      this.reveal(conn, reveals);
    }
  }

  private see(key: string, inAcked: bigint, outHead: bigint): void {
    const s = this.seen.get(key);
    this.seen.set(key, {
      inAcked: s && s.inAcked > inAcked ? s.inAcked : inAcked,
      outHead: s && s.outHead > outHead ? s.outHead : outHead,
    });
  }

  /** Release a connection (with `close`: abort it) and forget it. */
  private async giveUp(conn: Conn, close: boolean): Promise<void> {
    conn.gone = true;
    conn.layer.free();
    this.held.delete(conn.key);
    const lease = this.lease;
    if (lease) {
      await this.svc.release(lease, conn.conn, conn.token, close).catch((e) => {
        if (!isCode(e, 'claim_lost') && !isCode(e, 'conn_closed')) this.report(e, conn.conn);
      });
    }
    if (this.ctl) void this.claim(this.ctl.signal);
  }

  private forget(conn: Conn): void {
    conn.gone = true;
    conn.layer.free();
    if (this.held.get(conn.key) === conn) this.held.delete(conn.key);
    if (this.ctl) void this.claim(this.ctl.signal);
  }

  // ---------------------------------------------------------------- input

  private pushed(p: SvcPush, signal: AbortSignal): void {
    if (p.type === 'ready') {
      void this.claim(signal);
      return;
    }
    const key = bytes.hex(p.conn);
    const conn = this.held.get(key);
    if (!conn) {
      // Input for a claim whose response hasn't arrived: keep it briefly.
      // A push starts at `in_acked` and holds at most `window` bytes, but
      // pushes repeat bytes and may start a little before the claim's
      // `in_acked`: the bound is on the span. A chunk dropped here leaves a
      // gap, which `repair` reads again.
      const end = p.off + BigInt(p.data.length);
      let e = this.early.get(key);
      if (!e) {
        e = { at: Date.now(), lo: p.off, hi: end, chunks: [] };
        this.early.set(key, e);
      }
      const lo = p.off < e.lo ? p.off : e.lo;
      const hi = end > e.hi ? end : e.hi;
      if (hi - lo <= BigInt(2 * (this.info?.window ?? 0))) {
        e.chunks.push(p);
        e.lo = lo;
        e.hi = hi;
      }
      return;
    }
    this.input(conn, p);
    this.schedule(conn, signal);
  }

  private input(conn: Conn, p: Extract<SvcPush, { type: 'input' }>): void {
    if (p.sealed) {
      this.report(
        new ZenError(0, 'unsupported', 'sealed input (an inbox) is not supported yet'),
        conn.conn,
      );
      void this.giveUp(conn, true);
      return;
    }
    conn.at = Date.now();
    if (p.data.length) conn.input.add(p.off, p.data);
    if (p.flags & C_EOF && p.data.length === 0) conn.eofAt = p.off;
    if (conn.input.pending === 0) conn.gapAt = 0;
    else if (!conn.gapAt) conn.gapAt = Date.now();
  }

  /**
   * Read input that a lost push left missing (input past a gap for over a
   * second): the server keeps everything from `in_acked`.
   */
  private async repair(conn: Conn): Promise<void> {
    const r = await this.svc.read(conn.conn, conn.token, conn.input.end, {
      maxBytes: this.info?.window ?? 0,
    });
    if (conn.gone) return;
    for (const c of r.chunks) {
      this.input(conn, {
        type: 'input',
        conn: conn.conn,
        off: c.off,
        data: c.data,
        sealed: c.sealed,
        flags: 0,
      });
    }
    // Pushes resume past what was read; a gap left now is new.
    if (conn.input.pending === 0) conn.gapAt = 0;
    else conn.gapAt = Date.now();
  }

  /** Drop stale early input, and check on connections that went quiet. */
  private sweep(signal: AbortSignal): void {
    const now = Date.now();
    for (const [k, e] of this.early) if (now - e.at > 10_000) this.early.delete(k);
    for (const c of this.held.values()) {
      if (!c.busy && c.gapAt && now - c.gapAt > 1000) {
        c.busy = true;
        void this.track(
          this.repair(c)
            .catch((e) => {
              if (!isCode(e, 'claim_lost') && !isCode(e, 'conn_closed')) throw e;
              this.forget(c);
            })
            .finally(() => {
              c.busy = false;
              this.schedule(c, signal);
            }),
        );
        continue;
      }
      if (c.busy || now - c.at < 10_000) continue;
      // The server reaps a connection (a reset, a timeout) without telling
      // the holder: ask, so its slot frees.
      c.at = now;
      c.busy = true;
      void this.track(
        this.resync(c, signal).finally(() => {
          c.busy = false;
          this.schedule(c, signal);
        }),
      );
    }
  }

  // ---------------------------------------------------------------- steps

  private schedule(conn: Conn, signal: AbortSignal): void {
    if (conn.busy || conn.gone || signal.aborted) return;
    const eof = conn.eofAt !== undefined && conn.input.end >= conn.eofAt;
    if (!(conn.due || conn.input.end > conn.seenTo || (eof && !conn.seenEof))) return;
    conn.busy = true;
    void this.track(
      this.stepLoop(conn, signal).finally(() => {
        conn.busy = false;
        this.schedule(conn, signal);
      }),
    );
  }

  private async stepLoop(conn: Conn, signal: AbortSignal): Promise<void> {
    for (let backoff = 20; !conn.gone && !signal.aborted; ) {
      try {
        await this.step(conn);
        return;
      } catch (e) {
        if (conn.gone) return;
        if (e instanceof HandlerError) {
          this.report(e.cause, conn.conn);
          await this.giveUp(conn, true);
          return;
        }
        if (isCode(e, 'claim_lost') || isCode(e, 'conn_closed')) {
          this.forget(conn);
          return;
        }
        // A handshake step whose outcome isn't known can't be redone: the
        // handshake has moved on in memory (svc.md §7.1).
        if (
          conn.prep?.fragile &&
          (isCode(e, 'cursor_moved') || isCode(e, 'commit_unknown') || isCode(e, 'network'))
        ) {
          this.report(e, conn.conn);
          await this.giveUp(conn, true);
          return;
        }
        if (isCode(e, 'window')) {
          await pause(backoff, signal);
          backoff = Math.min(backoff * 2, 500);
          continue;
        }
        if (e instanceof ZenError && (e.status === 400 || e.status === 413)) {
          this.report(e, conn.conn);
          await this.giveUp(conn, true);
          return;
        }
        // cursor_moved, or an outcome left unknown (`commit_unknown`,
        // `network`): the server's view is the truth. If the commit did
        // apply, its records come back as pending and are revealed then.
        if (!isCode(e, 'cursor_moved') && !isCode(e, 'commit_unknown') && !isCode(e, 'network')) {
          this.report(e, conn.conn);
        }
        await pause(backoff, signal);
        backoff = Math.min(backoff * 2, 500);
        await this.resync(conn, signal);
      }
    }
  }

  /** Re-read a held connection from the server and continue from it. */
  private async resync(conn: Conn, signal: AbortSignal): Promise<void> {
    try {
      const c = await this.retry(signal, () => this.svc.state(conn.conn, conn.token));
      if (!c || conn.gone) return;
      this.load(conn, c, c.in_acked === conn.input.base && c.out_head === conn.outHead);
    } catch (e) {
      if (!(isCode(e, 'claim_lost') || isCode(e, 'conn_closed'))) this.report(e, conn.conn);
      if (isCode(e, 'rollback') || isCode(e, 'decrypt') || isCode(e, 'format')) {
        await this.giveUp(conn, true);
      } else {
        this.forget(conn);
      }
    }
  }

  /** Prepare a step from the input, outside the transaction (the layer's part). */
  private prepare(conn: Conn): Step0 {
    const inEnd = conn.input.end;
    const rawEof = conn.eofAt !== undefined && inEnd >= conn.eofAt;
    const p = conn.layer.prepare(conn.input.bytes(), rawEof, (conn.flags & C_FIN) !== 0);
    if (p.error) this.report(p.error, conn.conn);
    return { ...p, inEnd, rawEof };
  }

  /** Run the handler and commit its step, in one transaction. */
  private async step(conn: Conn): Promise<void> {
    conn.prep ??= this.prepare(conn);
    const p = conn.prep;
    const finSent = (conn.flags & C_FIN) !== 0;
    let built: Built | undefined;
    let fin: Finished | undefined;
    let h: HandlerOut = { acked: 0, writes: EMPTY, endFlag: 0 };
    let committed = false;
    await this.db.transaction(async (tx) => {
      built = undefined;
      committed = false;
      h = { acked: 0, writes: EMPTY, endFlag: 0 };
      let app = conn.app;
      if (p.call) {
        const s = new Step(
          conn.conn,
          conn.peer,
          p.input,
          p.eof,
          conn.app,
          finSent,
          conn.layer.room(p, this.info!.out_window),
          p.tls,
        );
        try {
          await this.opts.handler(s, tx);
        } catch (e) {
          throw new HandlerError(e);
        }
        app = s.state;
        h = {
          acked: s.acked,
          writes: s.out.length === 1 ? s.out[0]! : concatAll(s.out, s.outBytes),
          endFlag: s.endFlag,
        };
      }
      try {
        fin = conn.layer.finish(p, h);
      } catch (e) {
        throw new HandlerError(e);
      }
      const b = this.build(conn, fin, app, p.established);
      built = b;
      if (b.step) {
        const raw = tx.raw;
        raw.extra.svc = [...(raw.extra.svc ?? []), b.step];
        raw.onCommit(() => {
          committed = true;
        });
      }
    }, this.opts.txn);
    conn.seenTo = p.inEnd;
    conn.seenEof = p.rawEof;
    conn.due = false;
    const b = built!;
    if (b.step && !committed) return;
    conn.prep = undefined;
    conn.layer.committed(p, fin!, h);
    conn.app = b.app;
    // Records held back behind the plaintext: the handler made room.
    if (p.held && h.acked > 0) conn.due = true;
    if (b.step) {
      conn.at = Date.now();
      this.see(conn.key, b.inTo, b.outHead);
      conn.input.advance(b.inTo);
      conn.outHead = b.outHead;
      conn.hasState ||= b.sealed;
      conn.flags |= b.flags;
      if (b.reveals.length) this.reveal(conn, b.reveals);
    }
    const done =
      b.flags & (C_CLOSE | C_ABORT) ||
      (conn.flags & C_FIN && p.rawEof && conn.input.base >= (conn.eofAt ?? -1n));
    if (done) this.forget(conn);
  }

  /** Turn a step's output into the commit's step: records, the ack, the state. */
  private build(conn: Conn, f: Finished, app: Uint8Array | null, resumable: boolean): Built {
    const info = this.info!;
    const inTo = conn.input.base + BigInt(f.inAck);
    const writes = f.bytes;
    const out: Uint8Array[] = [];
    const reveals: SvcReveal[] = [];
    let off = conn.outHead;
    const keys = info.resume ? this.svc.sealKeys() : undefined;
    const record = (flags: number, raw: Uint8Array) => {
      if (keys) {
        const r = keys.sealRecord(conn.conn, off, flags, raw);
        out.push(r.record);
        reveals.push({ conn: conn.conn, off, key: r.key });
      } else {
        out.push(zw.plainRecord(flags, raw));
      }
      off += BigInt(raw.length);
    };
    for (let at = 0; at < writes.length; at += this.maxRaw) {
      const raw = writes.subarray(at, Math.min(at + this.maxRaw, writes.length));
      record(at + raw.length >= writes.length ? f.endFlag : 0, raw);
    }
    if (writes.length === 0 && f.endFlag) record(f.endFlag, new Uint8Array(0));
    const moved = inTo !== conn.input.base || off !== conn.outHead;
    const changed = !sameState(app, conn.app) || f.engineChanged;
    // TLS: resumable once the handshake is done, with the state it gives.
    const firstResumable = info.resume && resumable && !(conn.flags & C_RESUMABLE);
    const needState = conn.hasState || app !== null || f.engineState !== null;
    const sealState = needState && (moved || changed || !conn.hasState);
    const flags =
      (f.endFlag === SVC_FIN ? C_FIN : 0) |
      (f.endFlag === SVC_CLOSE ? C_CLOSE : 0) |
      (f.endFlag === SVC_ABORT ? C_ABORT : 0) |
      (firstResumable ? C_RESUMABLE : 0);
    if (!(moved || out.length || sealState || firstResumable)) {
      return {
        step: undefined,
        reveals,
        inTo,
        outHead: off,
        app,
        sealed: false,
        flags: 0,
      };
    }
    const step: SvcStep = {
      fs: this.db.fs.id,
      svc: this.svc.id,
      conn: conn.conn,
      token: conn.token,
      in_from: conn.input.base,
      in_to: inTo,
      out_from: conn.outHead,
      ...(out.length ? { out } : {}),
      ...(sealState
        ? {
            state: this.svc.sealKeys().sealState(
              conn.conn,
              inTo,
              off,
              encodeState({
                engine: this.engine!.name,
                engineState: f.engineState ?? EMPTY,
                app,
              }),
            ),
          }
        : {}),
      ...(firstResumable ? { resumable: true } : {}),
    };
    return { step, reveals, inTo, outHead: off, app, sealed: sealState, flags };
  }

  // ---------------------------------------------------------------- reveals

  /** Reveal record keys, in order per connection, retried while the network fails. */
  private reveal(conn: Conn, reveals: SvcReveal[]): void {
    const signal = this.ctl?.signal;
    conn.reveals = this.track(
      conn.reveals.then(async () => {
        for (let backoff = 100; ; backoff = Math.min(backoff * 2, 5000)) {
          try {
            const codes = await this.svc.reveal(reveals);
            const bad = codes.indexOf(REVEAL_BAD_KEY);
            if (bad >= 0) {
              this.report(
                new ZenError(0, 'bad_key', `reveal at ${reveals[bad]!.off} refused`),
                conn.conn,
              );
            }
            return;
          } catch (e) {
            // Keep trying through a stop: these bytes are committed. A
            // successor would re-reveal them from their escrows otherwise.
            if (isCode(e, 'network')) {
              if (signal?.aborted && backoff >= 5000) return;
              await pause(backoff);
              continue;
            }
            this.report(e, conn.conn);
            return;
          }
        }
      }),
    );
  }
}

function concatAll(parts: Uint8Array[], n: number): Uint8Array {
  const out = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

function sameState(a: Uint8Array | null, b: Uint8Array | null): boolean {
  if (a === null || b === null) return a === b;
  return bytes.equal(a, b);
}

/**
 * Serve a relayed service (spec/svc.md) from this database: claim its
 * connections and run `handler` for each step, in one transaction with the
 * step, so the handler's db writes and its output commit together. Runs
 * while the database is the owner (one tab of a browser serves), until
 * `stop()`.
 */
export function serve(db: Db, opts: ServeOptions): ServeHandle {
  return new ServeHandle(db, opts);
}
