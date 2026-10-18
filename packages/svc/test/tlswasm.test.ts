// The TLS engine's WASM binding (@zen/svc-wasm) against Node's own TLS
// client, over an in-memory duplex: the engine is the server, saved and
// restored after every input once established, as handlers do.
import { Duplex } from 'node:stream';
import { connect, type TLSSocket } from 'node:tls';
import type { TlsConfig, TlsConn, TlsProgress } from '@zen/svc-wasm';
import { beforeAll, describe, expect, it } from 'vitest';
import { type TestCerts, testCerts } from '../../client/src/testing/index.js';
import { loadSvcWasm } from '../src/tls.js';

type SW = Awaited<ReturnType<typeof loadSvcWasm>>;
let sw: SW;
let certs: TestCerts;

beforeAll(async () => {
  sw = await loadSvcWasm();
  certs = testCerts();
});

const enc = new TextEncoder();
const dec = new TextDecoder();

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

/** A server: the engine, with the input it hasn't consumed and the plaintext it got. */
class Server {
  conn: TlsConn;
  pending: Uint8Array = new Uint8Array(0);
  received: Uint8Array[] = [];
  restores = 0;
  established = 0;
  peerClosed = false;
  truncated = false;
  errors: TlsProgress['error'][] = [];
  echo = true;
  socket!: Duplex;

  constructor(readonly config: TlsConfig) {
    this.conn = sw.TlsConn.start(config);
  }

  send(b: Uint8Array): void {
    if (b.length) this.socket.push(b);
  }

  feed(bytes: Uint8Array, eof: boolean): void {
    this.pending = concat(this.pending, bytes);
    for (;;) {
      const p = this.conn.input(this.pending, eof);
      this.pending = this.pending.subarray(p.consumed);
      this.send(p.out);
      if (p.established) this.established++;
      this.peerClosed ||= p.peerClosed;
      this.truncated = p.truncated;
      if (p.error) this.errors.push(p.error);
      const plain = this.conn.plaintext();
      this.conn.consume(plain.length);
      if (plain.length) {
        this.received.push(plain);
        if (this.echo && !this.conn.writeClosed) this.send(this.conn.write(plain));
      }
      this.takeover();
      if (!p.held || p.error) break;
    }
  }

  /** Save the state, drop the connection, restore it. */
  takeover(): void {
    const st = this.conn.state();
    if (!st) return;
    this.conn.free();
    this.conn = sw.TlsConn.restore(this.config, st);
    this.restores++;
  }

  text(): string {
    return this.received.map((b) => dec.decode(b)).join('');
  }

  bytes(): number {
    return this.received.reduce((a, b) => a + b.length, 0);
  }
}

interface Pair {
  server: Server;
  client: TLSSocket;
  /** Everything the client read. */
  got: () => Buffer;
  ended: Promise<void>;
}

async function pair(
  config: TlsConfig,
  opts: { maxVersion?: 'TLSv1.2' | 'TLSv1.3'; ciphers?: string } = {},
): Promise<Pair> {
  const server = new Server(config);
  const socket = new Duplex({
    read() {},
    write(chunk: Buffer, _enc, cb) {
      server.feed(new Uint8Array(chunk), false);
      cb();
    },
    final(cb) {
      server.feed(new Uint8Array(0), true);
      cb();
    },
  });
  server.socket = socket;
  const client = connect({
    socket,
    ca: certs.ca,
    servername: 'localhost',
    ...opts,
  });
  const chunks: Buffer[] = [];
  client.on('data', (c: Buffer) => chunks.push(c));
  const ended = new Promise<void>((r) => client.on('end', () => r()));
  await secure(client, server);
  return { server, client, got: () => Buffer.concat(chunks), ended };
}

/** The client's handshake is done, and the server has seen its Finished. */
async function secure(client: TLSSocket, server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    client.once('secureConnect', () => resolve());
    client.once('error', reject);
  });
  await until(() => server.established === 1);
}

const until = async (f: () => boolean, ms = 5000) => {
  const end = Date.now() + ms;
  while (!f()) {
    if (Date.now() > end) throw new Error('timed out');
    await new Promise((r) => setTimeout(r, 5));
  }
};

function config(): TlsConfig {
  return new sw.TlsConfig(enc.encode(certs.chain), enc.encode(certs.key));
}

describe('the TLS engine (WASM) and Node TLS clients', () => {
  const suites13 = [
    'TLS_AES_128_GCM_SHA256',
    'TLS_AES_256_GCM_SHA384',
    'TLS_CHACHA20_POLY1305_SHA256',
  ];
  for (const suite of suites13) {
    it(`TLS 1.3 ${suite}: data both ways, restored after every input`, async () => {
      const cfg = config();
      const { server, client, got } = await pair(cfg, { ciphers: suite });
      expect(client.getProtocol()).toBe('TLSv1.3');
      expect(server.conn.version).toBe('TLSv1.3');
      expect(server.conn.suite).toBe(suite);
      expect(client.getCipher().standardName).toBe(suite);
      expect(server.conn.sni).toBe('localhost');
      client.write('hello\n');
      await until(() => got().toString() === 'hello\n');
      expect(server.text()).toBe('hello\n');
      expect(server.established).toBe(1);
      expect(server.restores).toBeGreaterThan(0);
      client.destroy();
      cfg.free();
    });
  }

  const suites12: [string, string][] = [
    ['ECDHE-ECDSA-AES128-GCM-SHA256', 'TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256'],
    ['ECDHE-ECDSA-AES256-GCM-SHA384', 'TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384'],
    ['ECDHE-ECDSA-CHACHA20-POLY1305', 'TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256'],
  ];
  for (const [openssl, iana] of suites12) {
    it(`TLS 1.2 ${openssl}: data both ways`, async () => {
      const cfg = config();
      const { server, client, got } = await pair(cfg, { maxVersion: 'TLSv1.2', ciphers: openssl });
      expect(client.getProtocol()).toBe('TLSv1.2');
      expect(server.conn.version).toBe('TLSv1.2');
      expect(server.conn.suite).toBe(iana);
      client.write('over 1.2\n');
      await until(() => got().toString() === 'over 1.2\n');
      client.destroy();
      cfg.free();
    });
  }

  it('1 MB each way, in both versions', async () => {
    const cfg = config();
    for (const maxVersion of ['TLSv1.3', 'TLSv1.2'] as const) {
      const { server, client, got } = await pair(cfg, { maxVersion });
      const data = Buffer.alloc(1 << 20);
      for (let i = 0; i < data.length; i++) data[i] = (i * 7 + (i >> 9)) & 255;
      client.write(data);
      await until(() => got().length >= data.length, 20_000);
      expect(got().equals(data)).toBe(true);
      expect(server.bytes()).toBe(data.length);
      client.destroy();
    }
    cfg.free();
  });

  it('P-384 and Ed25519 certificates', async () => {
    for (const kind of ['p384', 'ed25519'] as const) {
      const c = testCerts(kind);
      const cfg = new sw.TlsConfig(enc.encode(c.chain), enc.encode(c.key));
      const server = new Server(cfg);
      const socket = new Duplex({
        read() {},
        write(chunk: Buffer, _e, cb) {
          server.feed(new Uint8Array(chunk), false);
          cb();
        },
      });
      server.socket = socket;
      const client = connect({ socket, ca: c.ca, servername: 'localhost' });
      await secure(client, server);
      expect(server.conn.established).toBe(true);
      client.destroy();
      cfg.free();
    }
  });

  it('a server KeyUpdate, with and without asking the client to update', async () => {
    const cfg = config();
    const { server, client, got } = await pair(cfg);
    for (const ask of [false, true]) {
      server.send(server.conn.keyUpdate(ask));
      server.takeover();
      client.write(`after ${ask}\n`);
      await until(() => got().toString().endsWith(`after ${ask}\n`));
    }
    expect(server.errors).toEqual([]);
    client.destroy();
    cfg.free();
  });

  it('close_notify both ways', async () => {
    const cfg = config();
    const { server, client, ended } = await pair(cfg);
    client.end();
    await until(() => server.peerClosed);
    expect(server.truncated).toBe(false);
    server.send(server.conn.close());
    server.socket.push(null);
    await ended;
    cfg.free();
  });

  it('ALPN', async () => {
    const cfg = new sw.TlsConfig(enc.encode(certs.chain), enc.encode(certs.key), {
      alpn: [enc.encode('imap'), enc.encode('h2')],
    });
    const server = new Server(cfg);
    const socket = new Duplex({
      read() {},
      write(chunk: Buffer, _e, cb) {
        server.feed(new Uint8Array(chunk), false);
        cb();
      },
    });
    server.socket = socket;
    const client = connect({
      socket,
      ca: certs.ca,
      servername: 'localhost',
      ALPNProtocols: ['h2'],
    });
    await secure(client, server);
    expect(client.alpnProtocol).toBe('h2');
    expect(dec.decode(server.conn.alpn)).toBe('h2');
    client.destroy();
    cfg.free();
  });

  it('errors have stable codes', () => {
    const code = (f: () => unknown) => {
      try {
        f();
      } catch (e) {
        expect((e as Error).name).toBe('ZenTlsError');
        return (e as Error).message;
      }
      throw new Error('no error');
    };
    expect(code(() => new sw.TlsConfig(enc.encode('not a pem'), enc.encode(certs.key)))).toBe(
      'config',
    );
    expect(code(() => new sw.TlsConfig(enc.encode(certs.chain), enc.encode('nope')))).toBe(
      'config',
    );
    const rsa = testCerts('rsa');
    expect(code(() => new sw.TlsConfig(enc.encode(rsa.chain), enc.encode(rsa.key)))).toBe('config');
    // A key that isn't the certificate's.
    const other = testCerts();
    expect(code(() => new sw.TlsConfig(enc.encode(certs.chain), enc.encode(other.key)))).toBe(
      'config',
    );
    expect(
      code(
        () =>
          new sw.TlsConfig(enc.encode(certs.chain), enc.encode(certs.key), {
            tls12: false,
            tls13: false,
          }),
      ),
    ).toBe('config');
    const cfg = config();
    const c = sw.TlsConn.start(cfg);
    expect(code(() => c.write(enc.encode('x')))).toBe('handshaking');
    expect(code(() => c.keyUpdate(false))).toBe('handshaking');
    expect(c.state()).toBeUndefined();
    expect(c.close().length).toBe(0);
    c.free();
    expect(code(() => sw.TlsConn.restore(cfg, enc.encode('garbage')))).toBe('state');
    cfg.free();
  });

  it('KeyUpdate is refused in TLS 1.2; garbage from a client is an alert', async () => {
    const cfg = config();
    const { server, client } = await pair(cfg, { maxVersion: 'TLSv1.2' });
    try {
      server.conn.keyUpdate(false);
      expect.unreachable();
    } catch (e) {
      expect((e as Error).message).toBe('unsupported');
    }
    client.destroy();
    const c = sw.TlsConn.start(cfg);
    const p = c.input(enc.encode('GET / HTTP/1.1\r\n\r\n'), false);
    expect(p.error?.code).toBe('tls');
    expect(c.established).toBe(false);
    c.free();
    cfg.free();
  });

  it('a truncated stream is reported, not a close', async () => {
    const cfg = config();
    const { server, client } = await pair(cfg);
    client.write('x');
    await until(() => server.bytes() === 1);
    server.feed(new Uint8Array(0), true);
    expect(server.truncated).toBe(true);
    expect(server.peerClosed).toBe(false);
    client.destroy();
    cfg.free();
  });

  it('the plaintext held is bounded: input stops, then resumes after consuming', async () => {
    const cfg = config();
    const { server, client } = await pair(cfg);
    server.echo = false;
    // Feed records without consuming: the engine holds at most plainHeld().
    const held = sw.plainHeld();
    const orig = server.feed.bind(server);
    let stalls = 0;
    server.feed = (bytes: Uint8Array, eof: boolean) => {
      server.pending = concat(server.pending, bytes);
      const p = server.conn.input(server.pending, eof);
      server.pending = server.pending.subarray(p.consumed);
      if (p.held) stalls++;
      expect(server.conn.plaintext().length).toBeLessThan(held + 16_384 + 1);
      server.takeover();
    };
    client.write(Buffer.alloc(100_000, 1));
    await until(() => stalls > 0);
    server.feed = orig;
    // Consuming lets it continue.
    server.feed(new Uint8Array(0), false);
    await until(() => server.bytes() === 100_000);
    client.destroy();
    cfg.free();
  });
});
