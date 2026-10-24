// The `starttls()` layer, driven as the runtime drives it but without a
// zen server, against Node's TLS client over TCP. The client speaks
// plain, sends STARTTLS, then wraps the same socket in TLS.
// After every committed step outside the handshake the layer is dropped
// and a new one loads the committed engine state, as a successor would.
import { createServer, type Server, type Socket, connect as tcp } from 'node:net';
import { connect, type TLSSocket } from 'node:tls';
import { cbor } from '@zen/db';
import type { SvcInfo } from '@zen/wasm';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { type TestCerts, testCerts } from '../../client/src/testing/index.js';
import { SVC_CLOSE } from '../src/flags.js';
import { type Engine, type Finished, type LayerConn, type RawStep, raw } from '../src/layer.js';
import { LineReader } from '../src/lines.js';
import { starttls, tls } from '../src/tls.js';

let certs: TestCerts;
let engine: Engine<RawStep>;
let server: Server;
let port = 0;
let nextDriver: ((d: Driver) => void) | undefined;
/** The drivers of the running test: stopped after it, before the engine is closed. */
const drivers = new Set<Driver>();
const info = { resume: true, window: 1 << 18, out_window: 1 << 16 } as unknown as SvcInfo;

beforeAll(async () => {
  certs = testCerts();
  engine = await starttls({ cert: certs.chain, key: certs.key }).open(info);
  server = createServer((sock) => nextDriver?.(new Driver(sock)));
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  port = (server.address() as { port: number }).port;
});
afterEach(() => {
  for (const d of drivers) d.stop();
});
afterAll(() => {
  server?.close();
  for (const d of drivers) d.stop();
  // Late socket events find their drivers stopped: nothing uses the config now.
  engine?.close();
});

const dec = new TextDecoder();

const until = async (f: () => boolean, what: string, ms = 5000) => {
  const end = Date.now() + ms;
  while (!f()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await new Promise((r) => setTimeout(r, 5));
  }
};

/**
 * The test protocol: a line `STARTTLS` gets `220 go` and switches; other
 * plain lines get `plain: <l>`. Under TLS the server greets once with its
 * version, lines get `tls: <l>`, and `QUIT` gets `bye` and ends. The app
 * state counts lines.
 */
function handler(seen: string[]): (c: RawStep) => void {
  return (c) => {
    const st: { n: number; greeted?: boolean } = c.state
      ? JSON.parse(dec.decode(c.state))
      : { n: 0 };
    if (c.tls && !st.greeted) {
      c.write(`hello ${c.tls.version}\r\n`);
      st.greeted = true;
    }
    const r = new LineReader(c.input);
    for (let l = r.nextText(); l !== null; l = r.nextText()) {
      seen.push(l);
      st.n++;
      if (c.tls) {
        if (l === 'QUIT') {
          c.write('bye\r\n');
          c.end();
          break;
        }
        c.write(`tls: ${l}\r\n`);
      } else if (l === 'STARTTLS') {
        c.write('220 go\r\n');
        c.startTls();
        break;
      } else {
        c.write(`plain: ${l}\r\n`);
      }
    }
    c.ack(r.consumed);
    c.state = new TextEncoder().encode(JSON.stringify(st));
  };
}

/** The phase in a sealed starttls engine state (null: none yet). */
function phase(st: Uint8Array | null): number | null {
  return st ? (cbor.decode(st) as { phase: number }).phase : null;
}

/** One connection served through the layer, one step at a time. */
class Driver {
  lc: LayerConn<RawStep>;
  pending = new Uint8Array(0);
  eof = false;
  app: Uint8Array | null = null;
  /** The committed engine state. */
  sealed: Uint8Array | null = null;
  steps: Finished[] = [];
  errors: unknown[] = [];
  takeovers = 0;
  ended = false;
  private stopped = false;
  private busy = false;
  private more = false;
  readonly seen: string[] = [];

  constructor(readonly sock: Socket) {
    drivers.add(this);
    this.lc = engine.conn();
    this.lc.load(null, true);
    sock.on('data', (chunk: Buffer) => {
      const n = new Uint8Array(this.pending.length + chunk.length);
      n.set(this.pending);
      n.set(chunk, this.pending.length);
      this.pending = n;
      this.kick();
    });
    sock.on('end', () => {
      this.eof = true;
      this.kick();
    });
    sock.on('error', () => {});
    this.kick();
  }

  /** Run the steps due; a failure is recorded, for the test to see. */
  private kick(): void {
    this.run().catch((e) => this.errors.push(e));
  }

  /** Stop serving: the socket is destroyed and the layer freed. */
  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.ended = true;
    drivers.delete(this);
    this.sock.destroy();
    this.lc.free();
  }

  async run(): Promise<void> {
    if (this.stopped) return;
    if (this.busy) {
      this.more = true;
      return;
    }
    this.busy = true;
    try {
      for (let again = true; again && !this.ended; ) {
        this.more = false;
        const p = this.lc.prepare(this.pending, this.eof, false);
        if (p.error) this.errors.push(p.error);
        const live = this.lc.open(p, {
          conn: new Uint8Array(16),
          peer: { addr: '127.0.0.1', port: 0 } as never,
          state: this.app,
          finSent: false,
          outWindow: info.out_window,
        });
        let f: Finished;
        try {
          if (p.call) handler(this.seen)(live.view);
          f = live.finish();
          this.app = live.app();
        } finally {
          live.dispose();
        }
        this.lc.committed(p, f, live);
        this.steps.push(f);
        if (f.engineState) this.sealed = f.engineState;
        this.pending = this.pending.subarray(f.inAck);
        if (f.bytes.length) this.sock.write(f.bytes);
        if (f.endFlag) {
          this.ended = true;
          this.sock.end();
        }
        // A successor, outside the handshake: drop the layer, load the state.
        if (!this.ended && phase(this.sealed) !== 2) {
          this.lc.free();
          this.lc = engine.conn();
          this.lc.load(this.sealed, false);
          this.takeovers++;
        }
        again = f.again || this.more;
      }
    } finally {
      this.busy = false;
    }
  }
}

/** A client connected to a new driver; `got()` is what it read in plain. */
async function open(): Promise<{ d: Driver; sock: Socket; got: () => string; stop: () => void }> {
  const accepted = new Promise<Driver>((r) => {
    nextDriver = r;
  });
  const sock = tcp(port, '127.0.0.1');
  await new Promise<void>((ok, fail) => sock.once('connect', ok).once('error', fail));
  const d = await accepted;
  let got = '';
  const on = (b: Buffer) => {
    got += b.toString('latin1');
  };
  sock.on('data', on);
  return { d, sock, got: () => got, stop: () => sock.off('data', on) };
}

/** Wrap the client's socket in TLS. */
async function upgrade(
  sock: Socket,
  maxVersion: 'TLSv1.2' | 'TLSv1.3' = 'TLSv1.3',
): Promise<{ client: TLSSocket; got: () => string }> {
  const client = connect({ socket: sock, ca: certs.ca, servername: 'localhost', maxVersion });
  let got = '';
  client.on('data', (b: Buffer) => {
    got += b.toString();
  });
  await new Promise<void>((ok, fail) => {
    client.once('secureConnect', () => ok());
    client.once('error', fail);
  });
  return { client, got: () => got };
}

describe('starttls()', () => {
  for (const maxVersion of ['TLSv1.3', 'TLSv1.2'] as const) {
    it(`plain, the switch, then ${maxVersion} both ways; a successor at every step but the handshake's`, async () => {
      const { d, sock, ...plain } = await open();
      sock.write('one\r\n');
      await until(() => plain.got() === 'plain: one\r\n', 'the plain reply');
      sock.write('STARTTLS\r\n');
      await until(() => plain.got().endsWith('220 go\r\n'), 'the go-ahead');
      plain.stop();
      const switching = d.steps.at(-1)!;
      expect(switching.unseal).toBe(true);
      expect(switching.again).toBe(false);
      expect(phase(d.sealed)).toBe(1);
      const before = d.steps.length;
      const { client, got } = await upgrade(sock, maxVersion);
      await until(() => got() === `hello ${maxVersion}\r\n`, 'the greeting under TLS');
      // The handshake's steps weren't taken over; the rest were.
      expect(d.steps.slice(before).some((f) => phase(f.engineState) === 2)).toBe(true);
      client.write('two\r\nthree\r\n');
      await until(() => got().endsWith('tls: three\r\n'), 'the TLS replies');
      const ended = new Promise<void>((r) => client.once('end', () => r()));
      client.write('QUIT\r\n');
      await ended;
      expect(got()).toBe(`hello ${maxVersion}\r\ntls: two\r\ntls: three\r\nbye\r\n`);
      expect(d.seen).toEqual(['one', 'STARTTLS', 'two', 'three', 'QUIT']);
      expect(JSON.parse(dec.decode(d.app!)).n).toBe(5);
      expect(d.takeovers).toBeGreaterThan(4);
      expect(d.errors).toEqual([]);
      client.destroy();
    });
  }

  it('plaintext pipelined after STARTTLS goes to TLS: never a command, and the connection closes', async () => {
    const { d, sock, ...plain } = await open();
    const closed = new Promise<void>((r) => sock.once('close', () => r()));
    sock.on('error', () => {});
    sock.write('STARTTLS\r\nNOOP\r\n');
    await closed;
    expect(plain.got().startsWith('220 go\r\n')).toBe(true);
    expect(plain.got()).not.toContain('NOOP');
    expect(plain.got()).not.toContain('plain:');
    expect(d.seen).toEqual(['STARTTLS']);
    // The switching step asked to run again for the input after it.
    expect(d.steps.find((f) => f.unseal)!.again).toBe(true);
    expect(d.errors.length).toBe(1);
    expect(d.steps.at(-1)!.endFlag).toBe(SVC_CLOSE);
  });

  it('a state in the handshake is refused by a successor; a switched one is not', async () => {
    const { d, sock, ...plain } = await open();
    sock.write('STARTTLS\r\n');
    await until(() => plain.got() === '220 go\r\n', 'the go-ahead');
    plain.stop();
    const switched = d.sealed!;
    expect(phase(switched)).toBe(1);
    const c1 = engine.conn();
    c1.load(switched, false);
    c1.free();
    // Start the handshake, then look at the state its first step sealed.
    const client = connect({ socket: sock, ca: certs.ca, servername: 'localhost' });
    client.on('error', () => {});
    await until(() => phase(d.sealed) === 2, 'a handshake step');
    for (const same of [false, true]) {
      const c = engine.conn();
      expect(() => c.load(d.sealed, same)).toThrow(/during its handshake/);
      c.free();
    }
    client.destroy();
  });

  it('bad states are refused', () => {
    for (const st of [
      cbor.encode({ v: 2, phase: 0 }),
      cbor.encode({ v: 1, phase: 4 }),
      cbor.encode({ v: 1, phase: 3 }),
      cbor.encode({ v: 1, phase: 1, tls: new Uint8Array(4) }),
      new Uint8Array([0xff]),
    ]) {
      const c = engine.conn();
      expect(() => c.load(st, false)).toThrow(/starttls state|restore/);
      c.free();
    }
  });

  it('startTls() only under starttls(), in its plain phase', async () => {
    const tryStart = async (e: Engine<RawStep>) => {
      const c = e.conn();
      c.load(null, true);
      const p = c.prepare(new TextEncoder().encode('x'), false, false);
      const live = c.open(p, {
        conn: new Uint8Array(16),
        peer: { addr: '127.0.0.1', port: 0 } as never,
        state: null,
        finSent: false,
        outWindow: info.out_window,
      });
      try {
        live.view.startTls();
      } finally {
        live.dispose();
        c.free();
      }
    };
    await expect(tryStart(await raw().open(info))).rejects.toThrow(/starttls\(\) layer/);
    const t = await tls({ cert: certs.chain, key: certs.key }).open(info);
    // tls()'s handshake: the handler isn't called, but its step can't start TLS either.
    await expect(tryStart(t)).rejects.toThrow(/starttls\(\) layer/);
    t.close();
    await expect(tryStart(engine)).resolves.toBeUndefined();
  });
});
