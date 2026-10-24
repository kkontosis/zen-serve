// Layers: what sits between a connection's bytes and the handler (svc.md
// §7). `raw()` hands the bytes through; `tls()` and `starttls()` (tls.ts)
// terminate TLS and `ssh()` (ssh.ts) SSH. The runtime prepares each step
// outside its transaction, then opens it inside, where it may run again on
// a conflict: the layer gives the handler its view of the step (a
// `RawStep`, an `SshStep`) and turns what the handler did into the step's
// output.
import { bytes } from '@zen/client';
import type { SvcInfo, SvcPeer } from '@zen/wasm';
import { SVC_ABORT, SVC_CLOSE, SVC_FIN } from './flags.js';

/** What the handler learns of a TLS connection (`RawStep.tls`). */
export interface TlsInfo {
  /** `TLSv1.3` or `TLSv1.2`. */
  readonly version: string;
  /** The cipher suite's IANA name. */
  readonly suite: string;
  /** The client's server name, if it sent one. */
  readonly sni: string | null;
  /** The negotiated application protocol, if any. */
  readonly alpn: string | null;
  /** The input ended without close_notify (`eof` is set too). */
  readonly truncated: boolean;
}

/** One step of a connection, as a raw or TLS handler sees it. */
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
  /**
   * Stop sealing this connection's input from now on, on a service with an
   * inbox (svc.md §9): for example after STARTTLS, when the input becomes
   * TLS ciphertext. Sticky; input stored sealed before the server notices
   * is still opened.
   */
  unseal(): void;
  /**
   * Under `starttls()`, in the plain phase: switch to TLS after this step
   * (svc.md §7.1). The step's writes go out in plain first (the protocol's
   * go-ahead); the input after what the step acks is TLS, and input sealing
   * stops. Throws under the other layers, after the switch, and with
   * `end()`, `close()` or `abort()`.
   */
  startTls(): void;
  /**
   * Be woken by `ServeHandle.wake(key)` for these keys: the handler is then
   * called even without new input, for example to tell an idling client
   * of a change made elsewhere. They replace the keys of earlier steps;
   * `wakeOn()` clears them. They live in this process only: a successor
   * calls the handler once when it claims the connection, which registers
   * them again.
   */
  wakeOn(...keys: string[]): void;
  /**
   * Step again at once, without new input: for output larger than one
   * step can hold (`room`).
   */
  again(): void;
  /** The bytes this step may still write. */
  readonly room: number;
  /** Under `tls()`, and `starttls()` after the switch: the connection's TLS parameters. */
  readonly tls?: TlsInfo;
}

/** @internal A step prepared from the input, before the transaction. */
export interface Prepared {
  /** The connection can be resumed from this step on (TLS: its handshake is done). */
  established: boolean;
  /** Call the handler. */
  call: boolean;
  /** The connection failed: reported; the layer's step sends what it must and closes. */
  error?: unknown;
  /** Preparing changed something in memory that can't be redone (a handshake moved on). */
  fragile: boolean;
}

/** @internal What a step's view is opened with. */
export interface StepContext {
  conn: Uint8Array;
  peer: SvcPeer;
  /** The application's state, as committed. */
  state: Uint8Array | null;
  /** The handler ended its side (FIN) in an earlier step. */
  finSent: boolean;
  /** Most bytes the step's output may hold. */
  outWindow: number;
}

/** @internal A step's output, inside the transaction. */
export interface Finished {
  /** Input bytes acked, from `in_acked`. */
  inAck: number;
  /** Bytes to send. */
  bytes: Uint8Array;
  /** SVC_FIN, SVC_CLOSE, SVC_ABORT or 0. */
  endFlag: number;
  /** The engine state to seal (null: none). */
  engineState: Uint8Array | null;
  /** It differs from the committed one. */
  engineChanged: boolean;
  /**
   * Step again at once, without new input: the layer's (the handler made
   * room for held input) or the handler's (`again()`).
   */
  again: boolean;
  /** The handler called `unseal()`. */
  unseal?: boolean;
  /** The handler's wake keys, if it set them (`wakeOn`). */
  wakeOn?: string[];
}

/** @internal One attempt at a step, inside the transaction. */
export interface Live<S> {
  /** What the handler gets. */
  readonly view: S;
  /** The application's state after the handler. */
  app(): Uint8Array | null;
  /** Turn the handler's step into output. */
  finish(): Finished;
  /** Free what the attempt holds (it ran, or the handler threw). */
  dispose(): void;
}

/** @internal A layer's side of one connection. */
export interface LayerConn<S> {
  /**
   * The committed engine state, from a claim or a re-read (null: none).
   * `same`: the cursors are those this side last saw. Throws when the
   * connection can't continue here.
   */
  load(engineState: Uint8Array | null, same: boolean): void;
  /**
   * Process input (`eof`: the peer's FIN follows it). `finSent`: the
   * handler ended its side. `wake`: call the handler even without new
   * input (a wake, `again()`, or a claim).
   */
  prepare(input: Uint8Array, eof: boolean, finSent: boolean, wake: boolean): Prepared;
  /** Open an attempt at the step (may run more than once). */
  open(p: Prepared, ctx: StepContext): Live<S>;
  /** The step committed (or needed no commit). */
  committed(p: Prepared, f: Finished, live: Live<S>): void;
  free(): void;
}

/** @internal A started layer. */
export interface Engine<S> {
  readonly name: string;
  conn(): LayerConn<S>;
  close(): void;
}

/** A layer between the relay's bytes and the handler: `raw()`, `tls(…)` or `ssh(…)`. */
export interface Layer<S = RawStep> {
  /** The engine's name in the connection state (svc.md §7). */
  readonly engine: string;
  /**
   * @internal Load what it needs (a WASM module, a key), given the
   * service's settings; throws when it can't.
   */
  open(info: SvcInfo): Promise<Engine<S>>;
}

const EMPTY = new Uint8Array(0);

/** Join byte strings. */
export function concat(parts: Uint8Array[]): Uint8Array {
  if (parts.length === 1) return parts[0]!;
  const n = parts.reduce((a, p) => a + p.length, 0);
  const out = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** @internal The `RawStep` of raw and TLS handlers. */
export class Step implements RawStep {
  acked = 0;
  readonly out: Uint8Array[] = [];
  outBytes = 0;
  endFlag = 0;
  unsealed = false;
  /** `startTls()` was called. */
  starting = false;
  /** `again()` was called. */
  asked = false;
  /** The keys of `wakeOn()`, if it was called. */
  keys: string[] | undefined;

  constructor(
    readonly conn: Uint8Array,
    readonly peer: SvcPeer,
    readonly input: Uint8Array,
    readonly eof: boolean,
    public state: Uint8Array | null,
    private readonly finSent: boolean,
    private readonly limit: number,
    readonly tls: TlsInfo | undefined,
    /** `startTls()` is allowed (`starttls()`'s plain phase). */
    private readonly startable = false,
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
    if (this.outBytes + d.length > this.limit) {
      throw new RangeError(`a step writes at most ${this.limit} bytes (out_window)`);
    }
    if (d.length) {
      this.out.push(d);
      this.outBytes += d.length;
    }
  }

  end(): void {
    this.open('end');
    if (this.starting) throw new Error('end after startTls');
    this.endFlag = SVC_FIN;
  }

  close(): void {
    this.closing('close');
    if (this.starting) throw new Error('close after startTls');
    this.endFlag = SVC_CLOSE;
  }

  abort(): void {
    this.closing('abort');
    if (this.starting) throw new Error('abort after startTls');
    this.endFlag = SVC_ABORT;
  }

  unseal(): void {
    this.unsealed = true;
  }

  get room(): number {
    return this.limit - this.outBytes;
  }

  wakeOn(...keys: string[]): void {
    this.keys = keys;
  }

  again(): void {
    this.asked = true;
  }

  startTls(): void {
    if (!this.startable) {
      throw new Error(
        this.tls
          ? 'startTls: the connection is TLS already'
          : 'startTls needs the starttls() layer',
      );
    }
    this.open('startTls');
    this.starting = true;
  }

  /** Close or abort may replace this step's FIN, not an earlier step's. */
  private closing(what: string): void {
    if (this.endFlag && this.endFlag !== SVC_FIN) throw new Error(`${what} after close or abort`);
    if (this.finSent) throw new Error(`${what} after the connection's FIN`);
  }

  /** The writes, joined. */
  writes(): Uint8Array {
    return this.out.length ? concat(this.out) : EMPTY;
  }
}

/** A raw step: the input as it is. */
interface RawPrepared extends Prepared {
  input: Uint8Array;
  eof: boolean;
}

class RawConn implements LayerConn<RawStep> {
  load(): void {}

  prepare(input: Uint8Array, eof: boolean): RawPrepared {
    return { established: true, call: true, input, eof, fragile: false };
  }

  open(p: RawPrepared, ctx: StepContext): Live<RawStep> {
    const s = new Step(
      ctx.conn,
      ctx.peer,
      p.input,
      p.eof,
      ctx.state,
      ctx.finSent,
      ctx.outWindow,
      undefined,
    );
    return {
      view: s,
      app: () => s.state,
      finish: () => ({
        inAck: s.acked,
        bytes: s.writes(),
        endFlag: s.endFlag,
        engineState: null,
        engineChanged: false,
        again: s.asked,
        unseal: s.unsealed,
        ...(s.keys ? { wakeOn: s.keys } : {}),
      }),
      dispose: () => {},
    };
  }

  committed(): void {}

  free(): void {}
}

/** The handler sees the connection's bytes as they are. */
export function raw(): Layer<RawStep> {
  const engine: Engine<RawStep> = { name: 'raw', conn: () => new RawConn(), close: () => {} };
  return { engine: 'raw', open: async () => engine };
}
