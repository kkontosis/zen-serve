// The `ssh()` layer (svc.md §7.2): the SSH engine of @zen/svc-wasm between
// the relay's bytes and the handler. The engine's whole state is sealed
// with every step, a key exchange in progress included, so a successor
// continues the live session from any step. The handler sees an
// `SshStep`: the login question to answer, the step's events, and the
// channels with their data.
import { bytes, ZenError } from '@zen/client';
import type * as SW from '@zen/svc-wasm';
import type { SvcInfo, SvcPeer } from '@zen/wasm';
import { SVC_CLOSE } from './flags.js';
import {
  concat,
  type Engine,
  type Finished,
  type Layer,
  type LayerConn,
  type Live,
  type Prepared,
  type StepContext,
} from './layer.js';
import { loadSvcWasm, type SvcWasm, type WasmSource } from './wasm.js';

/** Options of `ssh()`. */
export interface SshLayerOptions {
  /** The host key: OpenSSH PEM, Ed25519, unencrypted (`ssh-keygen -t ed25519 -N ''`). */
  hostKey: string;
  /**
   * The window each channel is offered. Default: (the service's `window`
   * − 64 KiB) / 10, so the 10 channels' data in flight fits the relay's
   * input window (svc.md §7.2).
   */
  window?: number;
  /** Bytes under one key before the server rekeys (default 1 GiB). */
  rekeyBytes?: number;
  /** Seconds since the last key exchange before the server rekeys (default 3,600). */
  rekeySecs?: number;
  /** Key-exchange methods, preferred first (default `mlkem768x25519-sha256`, `curve25519-sha256`). */
  kex?: string[];
  /** Ciphers, preferred first (default `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`). */
  ciphers?: string[];
  /** Where @zen/svc-wasm's module is, if not in the default place. */
  wasm?: WasmSource;
}

/** A user's public key. */
export interface SshKey {
  /** `ssh-ed25519` or `ssh-rsa`. */
  readonly algorithm: string;
  /** `SHA256:…`, as `ssh-keygen -l` prints it. */
  readonly fingerprint: string;
  /** The key in its SSH encoding: equal blobs are the same key. */
  readonly blob: Uint8Array;
}

/** May `key` log in as `user`? */
export interface SshQuestion {
  readonly user: string;
  readonly key: SshKey;
  /** The client proved it holds the key: a yes logs it in. Without it, a yes lets it sign. */
  readonly signed: boolean;
}

/** What happened in this step's input. */
export type SshEvent =
  | { readonly type: 'open'; readonly ch: number }
  | { readonly type: 'exec'; readonly ch: number; readonly command: string }
  | { readonly type: 'shell'; readonly ch: number }
  | { readonly type: 'subsystem'; readonly ch: number; readonly name: string }
  | { readonly type: 'data'; readonly ch: number }
  | { readonly type: 'eof'; readonly ch: number }
  | { readonly type: 'close'; readonly ch: number };

/** A session channel. */
export interface SshChannel {
  readonly id: number;
  /** What the client asked it to run (null: not yet). */
  readonly request: 'exec' | 'shell' | 'subsystem' | null;
  /** The client sent EOF. */
  readonly eof: boolean;
  /** The client closed it: nothing more can be written. */
  readonly closed: boolean;
  /** What the client sent and the handler hasn't acked. */
  readonly data: Uint8Array;
  /** Consume the first `n` bytes of `data` (the client's window grows). */
  ack(n: number): void;
  /**
   * Send to standard output: the bytes taken, which may be fewer than
   * given (the client's window, the engine's queue, the step's output).
   * Keep the rest for a later step.
   */
  write(d: Uint8Array | string): number;
  /** Send to standard error, like `write`. */
  stderr(d: Uint8Array | string): number;
  /** Send `exit-status`, after what is queued. */
  exit(code: number): void;
  /** Send EOF, after what is queued. */
  end(): void;
  /** Close the channel, after what is queued. */
  close(): void;
}

/** One step of an SSH connection, as the handler sees it. */
export interface SshStep {
  readonly conn: Uint8Array;
  readonly peer: SvcPeer;
  /** The application's state, carried across steps and takeovers (null: none). */
  state: Uint8Array | null;
  /** The authenticated user, once logged in. */
  readonly user: string | null;
  /** A login question: `answer` it in this step (unanswered, it is refused). */
  readonly question: SshQuestion | null;
  answer(ok: boolean): void;
  /** This step's events; a command or subsystem name comes only here, once. */
  readonly events: readonly SshEvent[];
  /** The open channels. */
  readonly channels: readonly SshChannel[];
  channel(id: number): SshChannel | undefined;
  /** End the connection (DISCONNECT, "by application"). */
  disconnect(message?: string): void;
}

/** Bytes a step keeps free for the engine's own messages (window adjusts, EOF, exit, CLOSE). */
const RESERVE = 1024;
/** The data an SSH packet carries (32 KiB at most), against what it adds around it. */
const PACKET = 32_768;
const OVERHEAD = 64;

const EMPTY = new Uint8Array(0);
const text = new TextDecoder();

/** Unix seconds. */
const now = () => Math.floor(Date.now() / 1000);

/**
 * Serve SSH from the handler: the engine of svc.md §7.2, with `publickey`
 * logins the handler decides and `session` channels (exec, shell,
 * subsystem). A bad host key is reported through `onError` when
 * `serve()` starts, and nothing is claimed.
 */
export function ssh(opts: SshLayerOptions): Layer<SshStep> {
  return {
    engine: 'ssh',
    open: async (info: SvcInfo) => {
      const sw = await loadSvcWasm(opts.wasm);
      const window = opts.window ?? Math.max(1024, Math.floor((info.window - 65_536) / 10) || 1024);
      const config = new sw.SshConfig(opts.hostKey, {
        window,
        ...(opts.rekeyBytes === undefined ? {} : { rekeyBytes: opts.rekeyBytes }),
        ...(opts.rekeySecs === undefined ? {} : { rekeySecs: opts.rekeySecs }),
        ...(opts.kex ? { kex: opts.kex } : {}),
        ...(opts.ciphers ? { ciphers: opts.ciphers } : {}),
      });
      return new SshEngine(sw, config);
    },
  };
}

class SshEngine implements Engine<SshStep> {
  readonly name = 'ssh';

  constructor(
    readonly sw: SvcWasm,
    readonly config: SW.SshConfig,
  ) {}

  conn(): LayerConn<SshStep> {
    return new SshLayerConn(this);
  }

  close(): void {
    this.config.free();
  }
}

/** An SSH step prepared from the input. */
interface SshPrepared extends Prepared {
  /** Input bytes the engine consumed (whole packets): the step's ack. */
  consumed: number;
  /** What the engine sends from the input: key exchange, replies, queued output. */
  out: Uint8Array;
  events: SshEvent[];
  /** Reading stopped with complete input left (a question, unread data). */
  held: boolean;
  /** The engine ended the connection, or the client did. */
  ended: boolean;
  /** The engine state after the input. */
  engineState: Uint8Array;
}

function event(e: SW.SshEngineEvent): SshEvent | null {
  switch (e.type) {
    case 'authorize':
      return null;
    case 'exec':
      return { type: 'exec', ch: e.ch, command: text.decode(e.command) };
    default:
      return e;
  }
}

class SshLayerConn implements LayerConn<SshStep> {
  /** The committed engine state (null: a fresh connection). */
  private state: Uint8Array | null = null;

  constructor(private readonly e: SshEngine) {}

  load(engineState: Uint8Array | null, same: boolean): void {
    if (engineState?.length) {
      // Check it restores now, so a bad state releases the connection.
      try {
        this.e.sw.SshConn.restore(this.e.config, engineState).free();
      } catch (e) {
        throw new ZenError(0, 'format', `an SSH state that doesn't restore (${String(e)})`);
      }
      this.state = engineState;
      return;
    }
    // Every step of an SSH connection carries its state (svc.md §7.2).
    if (this.state || !same) throw new ZenError(0, 'format', 'an SSH connection without a state');
  }

  prepare(input: Uint8Array, eof: boolean): SshPrepared {
    const sw = this.e.sw;
    const c = this.state
      ? sw.SshConn.restore(this.e.config, this.state)
      : sw.SshConn.start(this.e.config, now());
    try {
      const p = c.input(input, eof, now());
      const ended = c.ended;
      // The client's DISCONNECT, or its FIN, is how sessions end.
      const error =
        p.error && p.error.code !== 'disconnected'
          ? new ZenError(0, `ssh_${p.error.code}`, `SSH: ${p.error.detail}`)
          : undefined;
      return {
        established: true,
        call: !ended,
        fragile: false,
        ...(error ? { error } : {}),
        consumed: p.consumed,
        out: p.out,
        events: p.events.map(event).filter((e): e is SshEvent => e !== null),
        held: p.held,
        ended,
        engineState: c.state(),
      };
    } finally {
      c.free();
    }
  }

  open(p: SshPrepared, ctx: StepContext): Live<SshStep> {
    if (p.ended) {
      // Send what the engine has (its DISCONNECT), then close.
      const view = new Closed(ctx);
      return {
        view,
        app: () => view.state,
        finish: () => ({
          inAck: p.consumed,
          bytes: p.out,
          endFlag: SVC_CLOSE,
          engineState: p.engineState,
          engineChanged: !this.same(p.engineState),
          again: false,
        }),
        dispose: () => {},
      };
    }
    const c = this.e.sw.SshConn.restore(this.e.config, p.engineState);
    const s = new Step(c, p, ctx);
    return {
      view: s,
      app: () => s.state,
      finish: () => {
        s.done();
        const state = c.state();
        return {
          inAck: p.consumed,
          bytes: concat([p.out, ...s.out]),
          endFlag: c.ended ? SVC_CLOSE : 0,
          engineState: state,
          engineChanged: !this.same(state),
          again: !c.ended && s.again(),
        };
      },
      dispose: () => {
        s.dead = true;
        c.free();
      },
    };
  }

  private same(s: Uint8Array): boolean {
    return this.state !== null && bytes.equal(this.state, s);
  }

  committed(_p: SshPrepared, f: Finished): void {
    if (f.engineState) this.state = f.engineState;
  }

  free(): void {
    this.state = null;
  }
}

/** The view of a step after the connection ended: the handler isn't called. */
class Closed implements SshStep {
  readonly conn: Uint8Array;
  readonly peer: SvcPeer;
  state: Uint8Array | null;
  readonly user = null;
  readonly question = null;
  readonly events: SshEvent[] = [];
  readonly channels: SshChannel[] = [];

  constructor(ctx: StepContext) {
    this.conn = ctx.conn;
    this.peer = ctx.peer;
    this.state = ctx.state;
  }

  answer(): void {}
  channel(): undefined {
    return undefined;
  }
  disconnect(): void {}
}

class Step implements SshStep {
  readonly conn: Uint8Array;
  readonly peer: SvcPeer;
  state: Uint8Array | null;
  readonly events: readonly SshEvent[];
  readonly channels: Channel[];
  /** The step's output, after the engine's from the input. */
  readonly out: Uint8Array[] = [];
  /** The attempt is over: the engine is freed. */
  dead = false;
  private outBytes = 0;
  private readonly room: number;
  private q: SshQuestion | null;
  private answered = false;
  private consumed = false;
  /** A write was cut short by the step's output, not by the client's window. */
  cut = false;

  constructor(
    readonly c: SW.SshConn,
    private readonly p: SshPrepared,
    ctx: StepContext,
  ) {
    this.conn = ctx.conn;
    this.peer = ctx.peer;
    this.state = ctx.state;
    this.events = p.events;
    this.room = ctx.outWindow - p.out.length - RESERVE;
    this.q = c.pendingAuthorize() ?? null;
    this.channels = c
      .channels()
      .map((i) => new Channel(this, i.ch, i.request, i.eof, i.closed, i.unread));
  }

  get user(): string | null {
    this.live();
    return this.c.user ?? null;
  }

  get question(): SshQuestion | null {
    return this.q;
  }

  /** @internal */
  live(): void {
    if (this.dead) throw new Error('an SSH step used after its transaction');
  }

  /** @internal Bytes for the client. */
  send(b: Uint8Array): void {
    if (b.length) {
      this.out.push(b);
      this.outBytes += b.length;
    }
  }

  /** @internal Most data a write may offer now. */
  offer(): number {
    const left = this.room - this.outBytes;
    return left <= 0 ? 0 : Math.floor((left * PACKET) / (PACKET + OVERHEAD));
  }

  /** @internal The handler consumed data. */
  consumedSome(): void {
    this.consumed = true;
  }

  answer(ok: boolean): void {
    this.live();
    if (!this.q) throw new Error('answer() without a question');
    this.send(this.c.authorize(ok));
    this.q = null;
    this.answered = true;
  }

  channel(id: number): Channel | undefined {
    return this.channels.find((c) => c.id === id);
  }

  disconnect(message = ''): void {
    this.live();
    this.send(this.c.disconnect(11, message));
  }

  /** @internal Before the state is taken: a question left unanswered is refused. */
  done(): void {
    if (this.q && !this.c.ended) this.answer(false);
  }

  /** @internal Step again at once: held input can be read now, or output was cut short. */
  again(): boolean {
    return (this.p.held && (this.answered || this.consumed)) || this.cut;
  }
}

class Channel implements SshChannel {
  private cached: Uint8Array | undefined;
  private unread: number;

  constructor(
    private readonly s: Step,
    readonly id: number,
    readonly request: 'exec' | 'shell' | 'subsystem' | null,
    readonly eof: boolean,
    readonly closed: boolean,
    unread: number,
  ) {
    this.unread = unread;
  }

  get data(): Uint8Array {
    this.s.live();
    if (this.unread === 0) return EMPTY;
    this.cached ??= this.s.c.channelData(this.id);
    return this.cached;
  }

  ack(n: number): void {
    this.s.live();
    if (!Number.isInteger(n) || n < 0 || n > this.unread) {
      throw new RangeError(`ack(${n}): ${this.unread} bytes unread on channel ${this.id}`);
    }
    if (n === 0) return;
    this.s.send(this.s.c.consume(this.id, n));
    this.unread -= n;
    this.cached = this.cached?.subarray(n);
    this.s.consumedSome();
  }

  write(d: Uint8Array | string): number {
    return this.put(d, false);
  }

  stderr(d: Uint8Array | string): number {
    return this.put(d, true);
  }

  private put(d: Uint8Array | string, err: boolean): number {
    this.s.live();
    const data = typeof d === 'string' ? bytes.utf8(d) : d;
    if (data.length === 0) return 0;
    const n = Math.min(data.length, this.s.offer());
    if (n === 0) {
      this.s.cut = true;
      return 0;
    }
    const part = n < data.length ? data.subarray(0, n) : data;
    const w = err ? this.s.c.writeStderr(this.id, part) : this.s.c.write(this.id, part);
    this.s.send(w.out);
    if (n < data.length && w.taken === n) this.s.cut = true;
    return w.taken;
  }

  exit(code: number): void {
    this.s.live();
    this.s.send(this.s.c.exitStatus(this.id, code >>> 0));
  }

  end(): void {
    this.s.live();
    this.s.send(this.s.c.eof(this.id));
  }

  close(): void {
    this.s.live();
    this.s.send(this.s.c.close(this.id));
  }
}
