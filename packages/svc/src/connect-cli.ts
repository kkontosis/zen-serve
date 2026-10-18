#!/usr/bin/env node
// zen-connect: connect to a `tcp-auth` service of zen-serve (spec/api.md
// §14.9) as a member.
//
//   zen-connect --url URL --svc HEX [--fs N] [--listen HOST:PORT] <sign-in>
//
// Without --listen, stdin and stdout are one connection: an ssh
// ProxyCommand. With --listen, each local connection accepted gets its own.
// Secrets never come from the command line, and nothing is asked on the
// terminal: under ProxyCommand, stdin is the byte stream.
//
// It imports only @zen/client and node:*, so it bundles on its own
// (scripts/build-zen-connect.mjs).
import { readFile } from 'node:fs/promises';
import { createServer, type Socket } from 'node:net';
import type { Readable, Writable } from 'node:stream';
import { parseArgs } from 'node:util';
import { bytes, type Client, connect, type Session, type SvcSocket, zw } from '@zen/client';

const HELP = `usage: zen-connect --url URL --svc HEX [options]

Connect to a tcp-auth service of zen-serve. The member needs topic append
on the service.

  --url URL              the zen-serve URL
  --svc HEX              the service id (its [[service]] id)
  --fs ID                the service's filesystem (default 1)
  --listen HOST:PORT     accept local connections here, one service
                         connection each (default: stdin and stdout, for
                         ssh -o ProxyCommand=...)
  --origin ORIGIN        the server origin as this client sees it (default: the URL's)

Sign-in (one of; nothing is asked on the terminal):
  --api-token-file FILE  an API token (or $ZEN_API_TOKEN)
  --device-key FILE      a device key file: JSON {identity_seed, device_seed, cert} (hex)
  --user NAME            a password-derived key; the password is $ZEN_PASSWORD
  -h, --help
`;

/** Parsed command line. */
export interface Args {
  url: string;
  svc: Uint8Array;
  fs: number;
  listen?: { host: string; port: number };
  origin?: string;
  apiTokenFile?: string;
  deviceKey?: string;
  user?: string;
}

/** Sessions are signed in again this long before they expire. */
const RENEW_MS = 60_000;
/** Local input pauses while this much waits in the WebSocket. */
const HIGH_WATER = 1 << 20;

class Usage extends Error {}

export function parse(argv: string[]): Args {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      url: { type: 'string' },
      svc: { type: 'string' },
      fs: { type: 'string', default: '1' },
      listen: { type: 'string' },
      origin: { type: 'string' },
      'api-token-file': { type: 'string' },
      'device-key': { type: 'string' },
      user: { type: 'string' },
      help: { type: 'boolean', short: 'h', default: false },
    },
  });
  if (values.help) {
    process.stderr.write(HELP);
    process.exit(0);
  }
  if (positionals.length) throw new Usage(`unexpected argument ${positionals[0]}`);
  if (!values.url) throw new Usage('--url is required');
  if (!values.svc || !/^([0-9a-fA-F]{32})+$/.test(values.svc))
    throw new Usage('--svc takes the service id in hex (16·n bytes)');
  const fs = Number(values.fs);
  if (!Number.isSafeInteger(fs) || fs < 1) throw new Usage('--fs takes a filesystem id');
  let listen: Args['listen'];
  if (values.listen) {
    const m = /^(.*):(\d+)$/.exec(values.listen);
    if (!m) throw new Usage('--listen takes HOST:PORT');
    listen = { host: m[1]!.replace(/^\[(.*)\]$/, '$1'), port: Number(m[2]) };
  }
  return {
    url: values.url,
    svc: bytes.fromHex(values.svc),
    fs,
    ...(listen ? { listen } : {}),
    ...(values.origin ? { origin: values.origin } : {}),
    ...(values['api-token-file'] ? { apiTokenFile: values['api-token-file'] } : {}),
    ...(values['device-key'] ? { deviceKey: values['device-key'] } : {}),
    ...(values.user ? { user: values.user } : {}),
  };
}

interface DeviceFile {
  identity_seed: string;
  device_seed: string;
  cert: string;
}

async function signIn(client: Client, a: Args): Promise<Session> {
  if (a.apiTokenFile || process.env.ZEN_API_TOKEN) {
    const t = a.apiTokenFile
      ? (await readFile(a.apiTokenFile, 'utf8')).trim()
      : process.env.ZEN_API_TOKEN!;
    return client.withApiToken(t);
  }
  if (a.deviceKey) {
    const d = JSON.parse(await readFile(a.deviceKey, 'utf8')) as DeviceFile;
    const identity = zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed));
    const device = zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed));
    try {
      return await client.signInDevice({ identity, device, cert: bytes.fromHex(d.cert) });
    } finally {
      identity.free();
      device.free();
    }
  }
  if (a.user) {
    const pw = process.env.ZEN_PASSWORD;
    if (pw === undefined) throw new Error('--user needs the password in $ZEN_PASSWORD');
    return client.signInPassword(a.user, pw);
  }
  throw new Usage('no sign-in method: give --api-token-file, --device-key or --user');
}

/** The session, signed in again when it is about to expire. */
function sessions(client: Client, a: Args, first: Session): () => Promise<Session> {
  let current = first;
  let renewing: Promise<Session> | undefined;
  return async () => {
    const exp = current.info?.expires_unix;
    if (exp === undefined || Number(exp) * 1000 - Date.now() > RENEW_MS) return current;
    renewing ??= signIn(client, a).finally(() => {
      renewing = undefined;
    });
    current = await renewing;
    return current;
  };
}

/**
 * Join a local byte stream to a service connection. The local side's EOF
 * is a `fin`; the service's `fin` ends the local output. A reset on
 * either side resets the other.
 */
function join(
  s: SvcSocket,
  input: Readable,
  events: { data: (d: Uint8Array) => void; fin: () => void; close: (reset: boolean) => void },
  local: { write: (d: Uint8Array) => void; end: () => void; reset: () => void },
): Promise<boolean> {
  return new Promise<boolean>((done) => {
    events.data = (d) => local.write(d);
    events.fin = () => local.end();
    events.close = (reset) => {
      if (reset) local.reset();
      else local.end();
      done(reset);
    };
    let paused = false;
    input.on('data', (d: Buffer) => {
      if (!s.open) return;
      s.write(new Uint8Array(d.buffer, d.byteOffset, d.length));
      if (s.bufferedAmount > HIGH_WATER && !paused) {
        paused = true;
        input.pause();
        const t = setInterval(() => {
          if (!s.open || s.bufferedAmount <= HIGH_WATER / 2) {
            clearInterval(t);
            paused = false;
            input.resume();
          }
        }, 10);
      }
    });
    input.on('end', () => {
      if (s.open) s.fin();
    });
    input.on('error', () => s.reset());
  });
}

/** Connect, with handlers that can be set once the socket exists. */
async function open(session: Session, a: Args) {
  const events = {
    data: (_: Uint8Array) => {},
    fin: () => {},
    close: (_: boolean) => {},
  };
  const s = await session.svcConnect({
    fs: a.fs,
    svc: a.svc,
    onData: (d) => events.data(d),
    onFin: () => events.fin(),
    onClose: (reset) => events.close(reset),
  });
  return { s, events };
}

/** How long a hung-up ProxyCommand waits for the service to close. */
const HUP_WAIT_MS = 5_000;

/** stdin and stdout as one connection; resolves to the exit code. */
async function stdio(session: Session, a: Args): Promise<number> {
  // ssh hangs up its ProxyCommand as it exits, after its last write: the
  // rest of stdin, then its EOF, still go to the service, which then
  // closes the connection normally.
  process.on('SIGHUP', () => {
    setTimeout(() => process.exit(0), HUP_WAIT_MS).unref();
  });
  const { s, events } = await open(session, a);
  const out: Writable = process.stdout;
  // The reader may be gone: what it would have read is dropped.
  out.on('error', () => {});
  const reset = await join(s, process.stdin, events, {
    write: (d) => out.write(d),
    end: () => {},
    reset: () => {},
  });
  // Everything received is written before the exit.
  await new Promise<void>((r) => out.write('', () => r()));
  if (reset) process.stderr.write('zen-connect: the connection was reset\n');
  return reset ? 1 : 0;
}

/** Accept local connections, one service connection each. */
async function listen(client: Client, a: Args, first: Session): Promise<void> {
  const session = sessions(client, a, first);
  const open_ = new Set<Socket>();
  const server = createServer({ allowHalfOpen: true, pauseOnConnect: true }, (sock) => {
    open_.add(sock);
    sock.on('close', () => open_.delete(sock));
    sock.on('error', () => {});
    void (async () => {
      let c: Awaited<ReturnType<typeof open>>;
      try {
        c = await open(await session(), a);
      } catch (e) {
        process.stderr.write(`zen-connect: ${message(e)}\n`);
        sock.resetAndDestroy();
        return;
      }
      sock.on('close', (hadError) => {
        if (hadError) c.s.reset();
      });
      const done = join(c.s, sock, c.events, {
        write: (d) => {
          if (!sock.destroyed) sock.write(d);
        },
        end: () => {
          if (!sock.destroyed && !sock.writableEnded) sock.end();
        },
        reset: () => {
          if (!sock.destroyed) sock.resetAndDestroy();
        },
      });
      sock.resume();
      await done;
    })();
  });
  await new Promise<void>((ok, fail) => {
    server.once('error', fail);
    server.listen(a.listen!.port, a.listen!.host, ok);
  });
  const addr = server.address();
  const at = typeof addr === 'string' ? addr : `${addr!.address}:${addr!.port}`;
  process.stderr.write(`zen-connect: listening on ${at}\n`);
  const stop = () => {
    server.close();
    for (const s of open_) s.resetAndDestroy();
    process.exit(0);
  };
  process.once('SIGTERM', stop);
  process.once('SIGINT', stop);
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export async function main(argv: string[]): Promise<number> {
  let a: Args;
  try {
    a = parse(argv);
  } catch (e) {
    process.stderr.write(`zen-connect: ${message(e)}\n\n${HELP}`);
    return 2;
  }
  try {
    const client = await connect(a.url, a.origin ? { origin: a.origin } : {});
    const session = await signIn(client, a);
    if (a.listen) {
      await listen(client, a, session);
      return -1;
    }
    return await stdio(session, a);
  } catch (e) {
    process.stderr.write(`zen-connect: ${message(e)}\n`);
    return e instanceof Usage ? 2 : 1;
  }
}

main(process.argv.slice(2)).then((code) => {
  if (code >= 0) process.exit(code);
});
