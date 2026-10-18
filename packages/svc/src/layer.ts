// Layers: what sits between a connection's bytes and the handler (svc.md
// §7). `raw()` hands the bytes through; `tls()` (tls.ts) terminates TLS.
// The runtime prepares each step outside its transaction, then finishes it
// inside, where it may run again on a conflict.

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

/** @internal A step prepared from the input, before the transaction. */
export interface Prepared {
  /** Input bytes the layer consumed itself (TLS records); 0 when the handler's ack counts. */
  consumed: number;
  /** Bytes to send before the handler's: handshake flights, alerts, KeyUpdates. */
  out: Uint8Array;
  /** The connection can be resumed from this step on (its handshake is done). */
  established: boolean;
  /** Call the handler. */
  call: boolean;
  /** The handler's input and EOF. */
  input: Uint8Array;
  eof: boolean;
  tls?: TlsInfo;
  /** The connection failed: send `out`, then close, without the handler. */
  error?: unknown;
  /** Complete records wait behind the plaintext held: once the handler acks, step again. */
  held: boolean;
  /** The engine state after the input (null: none). */
  engineState: Uint8Array | null;
  /** Preparing changed something in memory that can't be redone (a handshake moved on). */
  fragile: boolean;
}

/** @internal What the handler did, for the layer to turn into bytes. */
export interface HandlerOut {
  acked: number;
  writes: Uint8Array;
  /** SVC_FIN, SVC_CLOSE, SVC_ABORT or 0. */
  endFlag: number;
}

/** @internal A step's output, inside the transaction. */
export interface Finished {
  /** Input bytes acked, from `in_acked`. */
  inAck: number;
  /** Bytes to send. */
  bytes: Uint8Array;
  endFlag: number;
  /** The engine state to seal (null: none). */
  engineState: Uint8Array | null;
  /** It differs from the committed one. */
  engineChanged: boolean;
}

/** @internal A layer's side of one connection. */
export interface LayerConn {
  /**
   * The committed engine state, from a claim or a re-read (null: none).
   * `same`: the cursors are those this side last saw. Throws when the
   * connection can't continue here.
   */
  load(engineState: Uint8Array | null, same: boolean): void;
  /** Process input (`eof`: the peer's FIN follows it). `finSent`: the handler ended its side. */
  prepare(input: Uint8Array, eof: boolean, finSent: boolean): Prepared;
  /** Turn the handler's step into output (may run more than once). */
  finish(p: Prepared, h: HandlerOut): Finished;
  /** The step committed (or needed no commit). */
  committed(p: Prepared, f: Finished, h: HandlerOut): void;
  /** Most bytes the handler may write in a step whose output may be `outWindow`. */
  room(p: Prepared, outWindow: number): number;
  free(): void;
}

/** @internal A started layer. */
export interface Engine {
  readonly name: string;
  conn(): LayerConn;
  close(): void;
}

/** A layer between the relay's bytes and the handler: `raw()` or `tls(…)`. */
export interface Layer {
  /** The engine's name in the connection state (svc.md §7). */
  readonly engine: string;
  /** @internal Load what it needs (a WASM module, a certificate); throws when it can't. */
  open(): Promise<Engine>;
}

const EMPTY = new Uint8Array(0);

class RawConn implements LayerConn {
  load(): void {}

  prepare(input: Uint8Array, eof: boolean): Prepared {
    return {
      consumed: 0,
      out: EMPTY,
      established: true,
      call: true,
      input,
      eof,
      held: false,
      engineState: null,
      fragile: false,
    };
  }

  finish(_p: Prepared, h: HandlerOut): Finished {
    return {
      inAck: h.acked,
      bytes: h.writes,
      endFlag: h.endFlag,
      engineState: null,
      engineChanged: false,
    };
  }

  committed(): void {}

  room(_p: Prepared, outWindow: number): number {
    return outWindow;
  }

  free(): void {}
}

/** The handler sees the connection's bytes as they are. */
export function raw(): Layer {
  const engine: Engine = { name: 'raw', conn: () => new RawConn(), close: () => {} };
  return { engine: 'raw', open: async () => engine };
}
