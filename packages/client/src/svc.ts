// Relayed services (spec/api.md §14, svc.md). The handler side: the
// requests a handler makes and the keys it seals with; the runtime that
// drives them is @zen/svc. The connector side: a member's connection to a
// `tcp-auth` service (§14.9).
import type { Frame, SvcClaim, SvcInfo, SvcKeys, SvcReadResult, SvcReveal } from '@zen/wasm';
import { fromB64url, utf8 } from './bytes.js';
import { ZenError } from './errors.js';
import type { UnlockedFs } from './fs.js';
import type { Topic } from './log.js';
import type { Session } from './session.js';
import { zw } from './wasm.js';

/** Codes of `/v1/svc/reveal`, one per reveal. */
export const REVEALED = 0;
/** Already revealed or written, or a plain record. */
export const REVEAL_ALREADY = 1;
/** No record at the offset, or the connection is reaped. */
export const REVEAL_NOT_COMMITTED = 2;
/** The key doesn't match the record. */
export const REVEAL_BAD_KEY = 3;

/**
 * A relayed service of an unlocked fs: its id is the topic id of its path
 * (conventionally `svc/<name>`), which the server's `[[service]]` names.
 */
export class Service {
  /** The service id. */
  readonly id: Uint8Array;
  private readonly byEpoch = new Map<number, SvcKeys>();

  constructor(
    readonly fs: UnlockedFs,
    readonly topic: Topic,
  ) {
    this.id = topic.id;
  }

  private get session(): Session {
    return this.fs.session;
  }

  private get base() {
    return { fs: this.fs.id, svc: this.id };
  }

  /** The service keys of an epoch (spec/svc.md §8). */
  keysAt(epoch: number): SvcKeys {
    let k = this.byEpoch.get(epoch);
    if (!k) {
      k = this.fs.keysAt(epoch).svcKeys(this.topic.keysAt(epoch));
      this.byEpoch.set(epoch, k);
    }
    return k;
  }

  /** The keys to seal with: the newest epoch held. */
  sealKeys(): SvcKeys {
    return this.keysAt(this.fs.sealKeys().epoch);
  }

  /** The keys that open a sealed state or escrow (by its header's epoch). */
  keysFor(sealed: Uint8Array): SvcKeys {
    return this.keysAt(zw.peekSealed(sealed).epoch);
  }

  /** Free the derived keys (the fs's keys stay; `fs.close()` frees those). */
  close(): void {
    for (const k of this.byEpoch.values()) k.free();
    this.byEpoch.clear();
    this.topic.close();
  }

  /** The service's settings (§14.1). */
  info(): Promise<SvcInfo> {
    return this.session.call('/v1/svc/info', zw.encodeSvcInfoRequest, zw.decodeSvcInfo, this.base);
  }

  /** Acquire or renew a handler lease (§14.2); returns its expiry version. */
  async lease(lease: Uint8Array, ttlMs?: number): Promise<bigint> {
    const r = await this.session.call(
      '/v1/svc/lease',
      zw.encodeSvcLeaseRequest,
      zw.decodeSvcLeaseResult,
      { ...this.base, lease, ...(ttlMs !== undefined ? { ttl_ms: ttlMs } : {}) },
    );
    return r.expires_version;
  }

  /** End a handler lease now: its connections are requeued or aborted (§14.2). */
  async releaseLease(lease: Uint8Array): Promise<void> {
    await this.session.call('/v1/svc/lease', zw.encodeSvcLeaseRequest, zw.decodeSvcLeaseResult, {
      ...this.base,
      lease,
      release: true,
    });
  }

  /** Claim ready connections under a live lease (§14.3). */
  async next(
    lease: Uint8Array,
    opts: { limit?: number; waitMs?: number } = {},
  ): Promise<SvcClaim[]> {
    const r = await this.session.call('/v1/svc/next', zw.encodeSvcNextRequest, zw.decodeSvcClaims, {
      ...this.base,
      lease,
      ...(opts.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts.waitMs !== undefined ? { wait_ms: opts.waitMs } : {}),
    });
    return r.conns;
  }

  /** Re-read a held connection (§14.4). */
  state(conn: Uint8Array, token: bigint): Promise<SvcClaim> {
    return this.session.call('/v1/svc/state', zw.encodeSvcStateRequest, zw.decodeSvcClaim, {
      ...this.base,
      conn,
      token,
    });
  }

  /** Read a held connection's input from `from` (§14.6). */
  read(
    conn: Uint8Array,
    token: bigint,
    from: bigint,
    opts: { maxBytes?: number; waitMs?: number } = {},
  ): Promise<SvcReadResult> {
    return this.session.call('/v1/svc/read', zw.encodeSvcReadRequest, zw.decodeSvcReadResult, {
      ...this.base,
      conn,
      token,
      from,
      ...(opts.maxBytes !== undefined ? { max_bytes: opts.maxBytes } : {}),
      ...(opts.waitMs !== undefined ? { wait_ms: opts.waitMs } : {}),
    });
  }

  /** Give up a held connection: requeued if it may be, else aborted; with `close`, aborted (§14.8). */
  async release(lease: Uint8Array, conn: Uint8Array, token: bigint, close = false): Promise<void> {
    await this.session.call('/v1/svc/release', zw.encodeSvcReleaseRequest, zw.decodeEmpty, {
      ...this.base,
      lease,
      conn,
      token,
      ...(close ? { close: true } : {}),
    });
  }

  /** Reveal record keys of committed records (§14.7); one code per reveal. */
  async reveal(reveals: SvcReveal[]): Promise<number[]> {
    const r = await this.session.call(
      '/v1/svc/reveal',
      zw.encodeSvcRevealRequest,
      zw.decodeSvcRevealResult,
      { ...this.base, reveals },
    );
    return [...r.results];
  }
}

// ---------------------------------------------------------------- connectors

/** The largest frame on `/v1/svc/connect`, both ways (§14.9). */
export const SVC_FRAME = 65_536;

/** What a connector does with a `tcp-auth` connection. */
export interface SvcConnectOptions {
  /** The fs of the service. */
  fs: number;
  /** The service id: its `[[service]] id`. */
  svc: Uint8Array;
  /** Bytes from the service, in order. */
  onData?: (data: Uint8Array) => void;
  /** The service's half-close: no more data comes. */
  onFin?: () => void;
  /**
   * The connection ended: `reset` false after the service ended it
   * normally (close 1000), true for a reset, a lost WebSocket or `reset()`.
   */
  onClose?: (reset: boolean) => void;
  /** A `WebSocket` constructor; else the transport's, else the global one. */
  WebSocket?: typeof WebSocket;
}

const STATUS: Record<string, number> = {
  unauthorized: 401,
  forbidden: 403,
  not_found: 404,
  quota: 429,
  bad_request: 400,
};

/**
 * A connection to a `tcp-auth` service (§14.9): a byte stream to whichever
 * handler holds it. Rights are checked once, when it opens.
 *
 * Writes queue in the WebSocket: pause the source while `bufferedAmount`
 * is high. Reads can't be paused (the standard WebSocket has no way to),
 * so a slow consumer buffers what arrives (TD-SVC-CONNECT-BACKPRESSURE).
 */
export class SvcSocket {
  private ended = false;

  private constructor(
    private readonly ws: WebSocket,
    /** The connection id. */
    readonly conn: Uint8Array,
  ) {}

  /** Connect and authenticate (with a first frame, which browsers can send). */
  static open(session: Session, opts: SvcConnectOptions): Promise<SvcSocket> {
    const t = session.client.transport;
    const Ctor = opts.WebSocket ?? t.opts.WebSocket ?? globalThis.WebSocket;
    const hex = [...opts.svc].map((b) => b.toString(16).padStart(2, '0')).join('');
    const ws = new Ctor(t.wsUrl(`/v1/svc/connect?fs=${opts.fs}&svc=${hex}`));
    ws.binaryType = 'arraybuffer';
    const token = session.token.startsWith('zen_at_')
      ? utf8(session.token)
      : fromB64url(session.token);
    return new Promise<SvcSocket>((resolve, reject) => {
      let sock: SvcSocket | undefined;
      ws.onopen = () => ws.send(zw.encodeFrame({ op: 'auth', token }));
      ws.onmessage = (m: MessageEvent) => {
        if (sock) {
          if (typeof m.data === 'string') {
            if (m.data === 'fin') opts.onFin?.();
          } else {
            opts.onData?.(new Uint8Array(m.data as ArrayBuffer));
          }
          return;
        }
        let f: Frame;
        try {
          f = zw.decodeFrame(new Uint8Array(m.data as ArrayBuffer));
        } catch (e) {
          reject(new ZenError(0, 'format', `bad frame: ${String(e)}`));
          ws.close(4000);
          return;
        }
        if (f.op === 'ok' && f.conn) {
          sock = new SvcSocket(ws, f.conn);
          resolve(sock);
        } else if (f.op === 'err') {
          reject(new ZenError(STATUS[f.code] ?? 0, f.code, f.message));
        } else {
          reject(new ZenError(0, 'format', `unexpected frame ${f.op}`));
          ws.close(4000);
        }
      };
      ws.onclose = (e: CloseEvent) => {
        if (!sock) {
          reject(new ZenError(0, 'network', `the connection closed (${e.code})`));
          return;
        }
        if (sock.ended) return;
        sock.ended = true;
        opts.onClose?.(e.code !== 1000);
      };
      ws.onerror = () => {
        // onclose follows.
      };
    });
  }

  /** Bytes written and not yet sent. */
  get bufferedAmount(): number {
    return this.ws.bufferedAmount;
  }

  /** Whether the connection is still open. */
  get open(): boolean {
    return !this.ended && this.ws.readyState === 1;
  }

  /** Send bytes, in frames of at most 64 KiB. */
  write(data: Uint8Array): void {
    for (let at = 0; at < data.length; at += SVC_FRAME) {
      this.ws.send(data.subarray(at, Math.min(at + SVC_FRAME, data.length)));
    }
  }

  /** Half-close: nothing more is sent; data still arrives. */
  fin(): void {
    this.ws.send('fin');
  }

  /** Reset the connection (any close from the connector is one). */
  reset(): void {
    if (this.ended) return;
    this.ended = true;
    this.ws.close(4000);
  }
}
