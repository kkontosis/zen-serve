// The `tls()` layer (svc.md §7.1): the TLS engine of @zen/svc-wasm between
// the relay's bytes and the handler. During the handshake the engine lives
// in memory only, so a takeover then loses the connection; once the
// handshake is done, its state is sealed with every step and a successor
// continues the live connection from it.
import { bytes, ZenError } from '@zen/client';
import type * as SW from '@zen/svc-wasm';
import { SVC_CLOSE, SVC_FIN } from './flags.js';
import type { Engine, Finished, HandlerOut, Layer, LayerConn, Prepared, TlsInfo } from './layer.js';
import { loadSvcWasm, type SvcWasm, type WasmSource } from './wasm.js';

/** Options of `tls()`. */
export interface TlsLayerOptions {
  /** PEM: the certificate chain, leaf first. */
  cert: string | Uint8Array;
  /** PEM: its key, PKCS#8 or SEC1 (ECDSA P-256 or P-384, or Ed25519; not RSA). */
  key: string | Uint8Array;
  /** ALPN protocols, preferred first. */
  alpn?: string[];
  /** The versions offered (default both). */
  versions?: ('1.2' | '1.3')[];
  /** Where @zen/svc-wasm's module is, if not in the default place. */
  wasm?: WasmSource;
  /** @internal Tests: records per key before the limit's action. */
  keyLimit?: number;
}

/**
 * Terminate TLS 1.3 and 1.2 in the handler: the handler sees plaintext
 * through the same `RawStep`, with `c.tls` set. A bad certificate or key
 * is reported through `onError` when `serve()` starts, and nothing is
 * claimed.
 */
export function tls(opts: TlsLayerOptions): Layer {
  return {
    engine: 'tls',
    open: async () => {
      const sw = await loadSvcWasm(opts.wasm);
      const versions = opts.versions ?? ['1.2', '1.3'];
      const pem = (p: string | Uint8Array) => (typeof p === 'string' ? bytes.utf8(p) : p);
      const config = new sw.TlsConfig(pem(opts.cert), pem(opts.key), {
        alpn: opts.alpn?.map((a) => bytes.utf8(a)),
        tls12: versions.includes('1.2'),
        tls13: versions.includes('1.3'),
        ...(opts.keyLimit === undefined ? {} : { keyLimit: opts.keyLimit }),
      });
      return new TlsEngine(sw, config);
    },
  };
}

class TlsEngine implements Engine {
  readonly name = 'tls';

  constructor(
    readonly sw: SvcWasm,
    readonly config: SW.TlsConfig,
  ) {}

  conn(): LayerConn {
    return new TlsConn(this);
  }

  close(): void {
    this.config.free();
  }
}

/** A failure the engine reported, as an error for `onError`. */
function failure(f: SW.TlsFailure): ZenError {
  return new ZenError(0, `tls_${f.code}`, `TLS: ${f.detail}`);
}

const EMPTY = new Uint8Array(0);

const latin1 = new TextDecoder('latin1');

function concat(parts: Uint8Array[]): Uint8Array {
  const n = parts.reduce((a, p) => a + p.length, 0);
  if (parts.length === 1) return parts[0]!;
  const out = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

class TlsConn implements LayerConn {
  /** The handshake, in memory until it is done. */
  private hs: SW.TlsConn | undefined;
  /** The committed engine state, once established. */
  private state: Uint8Array | null = null;
  /** What the handler saw last: the plaintext it left unacked, and the EOF (-1: nothing yet). */
  private seenPlain = -1;
  private seenEof = false;

  constructor(private readonly e: TlsEngine) {}

  load(engineState: Uint8Array | null, same: boolean): void {
    this.seenPlain = -1;
    this.seenEof = false;
    if (engineState?.length) {
      // Check it restores now, so a bad state releases the connection.
      try {
        this.e.sw.TlsConn.restore(this.e.config, engineState).free();
      } catch (e) {
        throw new ZenError(0, 'format', `a TLS state that doesn't restore (${String(e)})`);
      }
      this.state = engineState;
      this.dropHandshake();
      return;
    }
    if (this.state || !same) {
      // svc.md §7.1: a handshake can't be continued elsewhere.
      throw new ZenError(0, 'format', 'a TLS connection taken over during its handshake');
    }
  }

  prepare(input: Uint8Array, eof: boolean, finSent: boolean): Prepared {
    if (this.state) {
      const c = this.e.sw.TlsConn.restore(this.e.config, this.state);
      try {
        return this.after(c, c.input(input, eof), finSent, false);
      } finally {
        c.free();
      }
    }
    this.hs ??= this.e.sw.TlsConn.start(this.e.config);
    const p = this.hs.input(input, eof);
    if (p.error || !p.established) {
      if (p.error) this.dropHandshake();
      return {
        consumed: p.consumed,
        out: p.out,
        established: false,
        call: false,
        input: EMPTY,
        eof: false,
        ...(p.error ? { error: failure(p.error) } : {}),
        held: false,
        engineState: null,
        fragile: true,
      };
    }
    const q = this.after(this.hs, p, finSent, true);
    this.dropHandshake();
    return q;
  }

  /** A step of an established connection, from the engine after its input. */
  private after(c: SW.TlsConn, p: SW.TlsProgress, finSent: boolean, fragile: boolean): Prepared {
    const input = c.plaintext();
    const eof = c.peerClosed || p.truncated;
    const alpn = c.alpn;
    const info: TlsInfo = {
      version: c.version ?? '',
      suite: c.suite ?? '',
      sni: c.sni ?? null,
      alpn: alpn ? latin1.decode(alpn) : null,
      truncated: p.truncated,
    };
    // The handler runs when the handshake just finished (a server-first
    // protocol greets then), on new plaintext, and on an EOF it hasn't
    // seen, unless it has ended its side and there's nothing to read.
    const fresh =
      p.established || input.length > this.seenPlain || (eof && !this.seenEof && !finSent);
    return {
      consumed: p.consumed,
      out: p.out,
      established: true,
      call: !p.error && fresh && !(finSent && eof && input.length === 0),
      input,
      eof,
      tls: info,
      ...(p.error ? { error: failure(p.error) } : {}),
      held: p.held,
      engineState: c.state() ?? null,
      fragile,
    };
  }

  finish(p: Prepared, h: HandlerOut): Finished {
    if (p.error || !p.engineState) {
      return {
        inAck: p.consumed,
        bytes: p.out,
        endFlag: p.error ? SVC_CLOSE : 0,
        engineState: p.engineState,
        engineChanged: p.engineState !== null && !this.same(p.engineState),
      };
    }
    const c = this.e.sw.TlsConn.restore(this.e.config, p.engineState);
    try {
      c.consume(h.acked);
      const parts = [p.out];
      let endFlag = h.endFlag;
      if (h.writes.length) {
        try {
          parts.push(c.write(h.writes));
        } catch (e) {
          // TLS 1.2 at its key's record limit: close (svc.md §7.1).
          if (!(e instanceof Error && e.message === 'key_limit')) throw e;
          endFlag = SVC_CLOSE;
        }
      }
      if (endFlag === SVC_FIN || endFlag === SVC_CLOSE) parts.push(c.close());
      const state = c.state() ?? null;
      return {
        inAck: p.consumed,
        bytes: concat(parts),
        endFlag,
        engineState: state,
        engineChanged: state !== null && !this.same(state),
      };
    } finally {
      c.free();
    }
  }

  private same(s: Uint8Array): boolean {
    return this.state !== null && bytes.equal(this.state, s);
  }

  committed(p: Prepared, f: Finished, h: HandlerOut): void {
    if (f.engineState) this.state = f.engineState;
    if (p.call) {
      this.seenPlain = p.input.length - h.acked;
      this.seenEof = p.eof;
    }
  }

  room(p: Prepared, outWindow: number): number {
    // A record adds at most 29 bytes (1.2 GCM) to 16 KiB; keep room for a
    // KeyUpdate and close_notify.
    const n = outWindow - p.out.length - 256;
    return n <= 0 ? 0 : Math.floor((n * 16384) / (16384 + 32));
  }

  private dropHandshake(): void {
    this.hs?.free();
    this.hs = undefined;
  }

  free(): void {
    this.dropHandshake();
    this.state = null;
  }
}
