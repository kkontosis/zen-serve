// Relayed services, the handler side (spec/api.md §14, svc.md): the
// requests a handler makes and the keys it seals with. The runtime that
// drives them is @zen/svc.
import type { SvcClaim, SvcInfo, SvcKeys, SvcReadResult, SvcReveal } from '@zen/wasm';
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
