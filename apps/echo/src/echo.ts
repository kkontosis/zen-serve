// The echo app: a relayed line service (spec/svc.md) whose handler runs on
// a member's client. Each line `l` a peer sends gets the reply `<n>: l`,
// with `n` counting the connection's lines, and a row `{conn, n, line}` in
// `lines`, committed in one transaction with the reply. The same handler
// serves `echo-tls` behind `tls()`, with the certificate from `certs`; and
// `mirror`, which sends back whatever arrives, serves `mirror-tls`. Over
// SSH (ssh.ts), `sshEcho` serves `echo-ssh`.
import { bytes } from '@zen/client';
import type { Db } from '@zen/db';
import {
  type Layer,
  type RawHandler,
  type ServeHandle,
  type ServeOptions,
  serve,
  tls,
} from '@zen/svc';
import type { Cert, Line } from './schema.js';

export {
  type Cert,
  DB_OPTIONS,
  type Line,
  NS,
  putCert,
  putHostKey,
  putSshKey,
  type SshKey,
} from './schema.js';
export { SSH_SERVICE, STUB_COMMIT, STUB_REFS, sshEcho, sshLayer } from './ssh.js';

/** The service's topic path: its topic id is the service id. */
export const SERVICE = ['svc', 'echo'];

/** The echo service over TLS. */
export const TLS_SERVICE = ['svc', 'echo-tls'];

/** The mirror service over TLS (bulk transfers). */
export const MIRROR_SERVICE = ['svc', 'mirror-tls'];

const text = new TextDecoder();

/**
 * The handler. Input is acked up to the last newline, so a partial line
 * waits for the rest; the line count is the connection's state. At the
 * peer's EOF, a last partial line counts too, then `bye` and FIN.
 */
export const echo: RawHandler = async (c, tx) => {
  let n = c.state ? Number(text.decode(c.state)) : 0;
  const input = c.input;
  const conn = bytes.hex(c.conn);
  const line = async (raw: Uint8Array) => {
    const l = text.decode(raw);
    n++;
    c.write(`${n}: ${l}\n`);
    await tx.table('lines').insert({ conn, n, line: l });
  };
  let at = 0;
  for (let nl = input.indexOf(10); nl >= 0; nl = input.indexOf(10, at)) {
    await line(input.subarray(at, nl));
    at = nl + 1;
  }
  if (c.eof) {
    if (at < input.length) await line(input.subarray(at));
    c.ack(input.length);
    c.write('bye\n');
    c.end();
  } else {
    c.ack(at);
  }
  c.state = bytes.utf8(String(n));
};

/** The mirror: every byte that arrives goes back; the peer's EOF ends it. */
export const mirror: RawHandler = (c) => {
  if (c.input.length) c.write(c.input);
  c.ack(c.input.length);
  if (c.eof) c.end();
};

/** A `tls()` layer with the stored certificate `name`. */
export async function tlsLayer(db: Db, name = 'echo'): Promise<Layer> {
  const c = await db.table<Cert>('certs').get(name);
  if (!c) throw new Error(`no certificate ${name} in certs`);
  return tls({ cert: c.chain, key: c.key });
}

/** Serve the echo service from `db` (until `stop()`). */
export function serveEcho(db: Db, opts: Partial<ServeOptions> = {}): ServeHandle {
  return serve(db, { service: SERVICE, handler: echo, ...opts });
}

/** Every stored line, by connection (then count, through the index). */
export function lines(db: Db): Promise<Line[]> {
  return db.table<Line>('lines').query().orderBy('conn').all();
}
