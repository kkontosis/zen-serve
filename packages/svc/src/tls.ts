// The `tls()` and `starttls()` layers (svc.md §7.1): the TLS engine of
// @zen/svc-wasm between the relay's bytes and the handler. During the
// handshake the engine lives in memory only, so a takeover then loses the
// connection; once the handshake is done, its state is sealed with every
// step and a successor continues the live connection from it. `starttls()`
// begins in plain, as `raw()`, until the handler calls `startTls()`.
import { bytes, ZenError } from '@zen/client';
import { cbor } from '@zen/db';
import type * as SW from '@zen/svc-wasm';
import { SVC_CLOSE, SVC_FIN } from './flags.js';
import {
  concat,
  type Engine,
  type Finished,
  type Layer,
  type LayerConn,
  type Live,
  type Prepared,
  type RawStep,
  Step,
  type StepContext,
  type TlsInfo,
} from './layer.js';
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
export function tls(opts: TlsLayerOptions): Layer<RawStep> {
  return tlsLayer(opts, false);
}

/**
 * Begin in plain, as `raw()`, and switch to TLS when the handler calls
 * `c.startTls()` (SMTP's STARTTLS, RFC 3207). The step that calls it sends
 * its writes in plain (the go-ahead); the input after what it acks is TLS,
 * so plaintext a client sent after the command is never read as a command
 * (CVE-2011-0411), and input sealing stops (svc.md §9). After the
 * handshake the handler runs again with `c.tls` set, as under `tls()`.
 */
export function starttls(opts: TlsLayerOptions): Layer<RawStep> {
  return tlsLayer(opts, true);
}

function tlsLayer(opts: TlsLayerOptions, plainFirst: boolean): Layer<RawStep> {
  const name = plainFirst ? 'starttls' : 'tls';
  return {
    engine: name,
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
      return new TlsEngine(name, sw, config, plainFirst);
    },
  };
}

class TlsEngine implements Engine<RawStep> {
  constructor(
    readonly name: string,
    readonly sw: SvcWasm,
    readonly config: SW.TlsConfig,
    readonly plainFirst: boolean,
  ) {}

  conn(): LayerConn<RawStep> {
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

function handshakeLost(): ZenError {
  return new ZenError(0, 'format', 'a TLS connection taken over during its handshake');
}

const EMPTY = new Uint8Array(0);

const latin1 = new TextDecoder('latin1');

/** `starttls()`'s phases (svc.md §7.1); `tls()` begins at SWITCHED. */
const PLAIN = 0;
const SWITCHED = 1;
const HANDSHAKE = 2;
const ESTABLISHED = 3;

/** A step prepared from the input. */
interface TlsPrepared extends Prepared {
  /** The phase after the input (PLAIN: the input is handed over as it is). */
  phase: number;
  /** Input bytes the engine consumed (whole records): the step's ack. */
  consumed: number;
  /** Bytes to send before the handler's: handshake flights, alerts, KeyUpdates. */
  out: Uint8Array;
  /** The handler's input (plaintext) and EOF. */
  input: Uint8Array;
  eof: boolean;
  tls?: TlsInfo;
  /** Complete records wait behind the plaintext held: once the handler acks, step again. */
  held: boolean;
  /** The TLS engine's state after the input (null: plain or handshaking). */
  engineState: Uint8Array | null;
}

/** An attempt at a step. */
interface TlsLive extends Live<RawStep> {
  readonly view: Step;
  /** What the attempt's `finish()` made: the phase after the step, and the TLS state. */
  next?: { phase: number; tls: Uint8Array | null };
}

class TlsConn implements LayerConn<RawStep> {
  /** The handshake, in memory until it is done. */
  private hs: SW.TlsConn | undefined;
  /** The committed TLS state, once established. */
  private state: Uint8Array | null = null;
  /** The committed phase. */
  private phase: number;
  /** The committed engine state, as sealed (`tls()`: the TLS state itself). */
  private sealed: Uint8Array | null = null;
  /** What the handler saw last: the plaintext it left unacked, and the EOF (-1: nothing yet). */
  private seenPlain = -1;
  private seenEof = false;

  constructor(private readonly e: TlsEngine) {
    this.phase = e.plainFirst ? PLAIN : SWITCHED;
  }

  load(engineState: Uint8Array | null, same: boolean): void {
    this.seenPlain = -1;
    this.seenEof = false;
    const st = engineState?.length ? engineState : null;
    let phase: number;
    let tlsState: Uint8Array | null;
    if (this.e.plainFirst) {
      ({ phase, tls: tlsState } = decodeOuter(st));
    } else if (st) {
      phase = ESTABLISHED;
      tlsState = st;
    } else {
      // `tls()` keeps no state until the handshake is done.
      if (this.state || !same) throw handshakeLost();
      phase = this.phase;
      tlsState = null;
    }
    // svc.md §7.1: a handshake can't be continued elsewhere.
    if (phase === HANDSHAKE && !(same && this.phase === HANDSHAKE)) throw handshakeLost();
    if (tlsState) {
      // Check it restores now, so a bad state releases the connection.
      try {
        this.e.sw.TlsConn.restore(this.e.config, tlsState).free();
      } catch (e) {
        throw new ZenError(0, 'format', `a TLS state that doesn't restore (${String(e)})`);
      }
    }
    if (phase !== HANDSHAKE) this.dropHandshake();
    this.phase = phase;
    this.state = tlsState;
    this.sealed = st;
  }

  prepare(input: Uint8Array, eof: boolean, finSent: boolean): TlsPrepared {
    if (this.phase === PLAIN) {
      return {
        phase: PLAIN,
        established: true,
        call: true,
        input,
        eof,
        consumed: 0,
        out: EMPTY,
        held: false,
        engineState: null,
        fragile: false,
      };
    }
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
      // Once it has consumed or sent anything, the handshake can't be redone.
      const begun = this.phase === HANDSHAKE || p.consumed > 0 || p.out.length > 0;
      return {
        phase: begun ? HANDSHAKE : this.phase,
        consumed: p.consumed,
        out: p.out,
        established: false,
        call: false,
        input: EMPTY,
        eof: false,
        ...(p.error ? { error: failure(p.error) } : {}),
        held: false,
        engineState: null,
        fragile: begun || p.error !== undefined,
      };
    }
    const q = this.after(this.hs, p, finSent, true);
    this.dropHandshake();
    return q;
  }

  /** A step of an established connection, from the engine after its input. */
  private after(c: SW.TlsConn, p: SW.TlsProgress, finSent: boolean, fragile: boolean): TlsPrepared {
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
      phase: ESTABLISHED,
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

  open(p: TlsPrepared, ctx: StepContext): TlsLive {
    const plain = p.phase === PLAIN;
    const s = new Step(
      ctx.conn,
      ctx.peer,
      p.input,
      p.eof,
      ctx.state,
      ctx.finSent,
      plain ? ctx.outWindow : room(p, ctx.outWindow),
      p.tls,
      plain,
    );
    const live: TlsLive = {
      view: s,
      app: () => s.state,
      finish: () => {
        const f = plain ? this.finishPlain(p, s) : this.finish(p, s);
        live.next = f.next;
        return f.f;
      },
      dispose: () => {},
    };
    return live;
  }

  /** The engine state to seal: `tls()`'s is the TLS state; `starttls()` wraps it with the phase. */
  private outer(phase: number, tlsState: Uint8Array | null): Uint8Array | null {
    if (!this.e.plainFirst) return tlsState;
    return cbor.encode(tlsState ? { v: 1, phase, tls: tlsState } : { v: 1, phase });
  }

  private changed(s: Uint8Array | null): boolean {
    return s !== null && !(this.sealed !== null && bytes.equal(this.sealed, s));
  }

  /** A plain step: the handler's bytes as they are, and the switch if it asked for it. */
  private finishPlain(p: TlsPrepared, s: Step): { f: Finished; next: TlsLive['next'] } {
    const engineState = s.starting ? this.outer(SWITCHED, null) : null;
    return {
      f: {
        inAck: s.acked,
        bytes: s.writes(),
        endFlag: s.endFlag,
        engineState,
        engineChanged: this.changed(engineState),
        // Input after the command is TLS: hand it to the engine at once,
        // so a handshake that begins with plaintext fails now.
        again: s.starting && s.acked < p.input.length,
        unseal: s.unsealed || s.starting,
      },
      next: { phase: s.starting ? SWITCHED : PLAIN, tls: null },
    };
  }

  private finish(p: TlsPrepared, s: Step): { f: Finished; next: TlsLive['next'] } {
    if (p.error || !p.engineState) {
      const engineState = this.outer(p.phase, p.engineState);
      return {
        f: {
          inAck: p.consumed,
          bytes: p.out,
          endFlag: p.error ? SVC_CLOSE : 0,
          engineState,
          engineChanged: this.changed(engineState),
          again: false,
        },
        next: { phase: p.phase, tls: p.engineState },
      };
    }
    const c = this.e.sw.TlsConn.restore(this.e.config, p.engineState);
    try {
      c.consume(s.acked);
      const parts = [p.out];
      let endFlag = s.endFlag;
      const writes = s.writes();
      if (writes.length) {
        try {
          parts.push(c.write(writes));
        } catch (e) {
          // TLS 1.2 at its key's record limit: close (svc.md §7.1).
          if (!(e instanceof Error && e.message === 'key_limit')) throw e;
          endFlag = SVC_CLOSE;
        }
      }
      if (endFlag === SVC_FIN || endFlag === SVC_CLOSE) parts.push(c.close());
      const tlsState = c.state() ?? null;
      const engineState = this.outer(ESTABLISHED, tlsState);
      return {
        f: {
          inAck: p.consumed,
          bytes: concat(parts),
          endFlag,
          engineState,
          engineChanged: this.changed(engineState),
          // Records held back behind the plaintext: the handler made room.
          again: p.held && s.acked > 0,
          unseal: s.unsealed,
        },
        next: { phase: ESTABLISHED, tls: tlsState },
      };
    } finally {
      c.free();
    }
  }

  committed(p: TlsPrepared, f: Finished, live: TlsLive): void {
    if (live.next) {
      this.phase = live.next.phase;
      if (live.next.tls) this.state = live.next.tls;
    }
    if (f.engineState) this.sealed = f.engineState;
    if (p.call && p.phase !== PLAIN) {
      this.seenPlain = p.input.length - live.view.acked;
      this.seenEof = p.eof;
    }
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

/** `starttls()`'s engine state (svc.md §7.1); none is the plain phase. */
function decodeOuter(st: Uint8Array | null): { phase: number; tls: Uint8Array | null } {
  if (!st) return { phase: PLAIN, tls: null };
  let m: Record<string, unknown> | null;
  try {
    m = cbor.decode(st) as Record<string, unknown> | null;
  } catch {
    m = null;
  }
  const phase = m?.phase;
  const tls = m?.tls;
  if (
    !m ||
    typeof m !== 'object' ||
    m.v !== 1 ||
    typeof phase !== 'number' ||
    !Number.isInteger(phase) ||
    phase < PLAIN ||
    phase > ESTABLISHED ||
    (phase === ESTABLISHED) !== tls instanceof Uint8Array ||
    (tls !== undefined && !(tls instanceof Uint8Array))
  ) {
    throw new ZenError(0, 'format', 'not a version-1 starttls state');
  }
  return { phase, tls: (tls as Uint8Array | undefined) ?? null };
}

/** Most plaintext a step may write so its records fit `outWindow`. */
function room(p: TlsPrepared, outWindow: number): number {
  // A record adds at most 29 bytes (1.2 GCM) to 16 KiB; keep room for a
  // KeyUpdate and close_notify.
  const n = outWindow - p.out.length - 256;
  return n <= 0 ? 0 : Math.floor((n * 16384) / (16384 + 32));
}
