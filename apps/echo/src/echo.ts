// The echo app: a relayed line service (spec/svc.md) whose handler runs on
// a member's client. Each line `l` a peer sends gets the reply `<n>: l`,
// with `n` counting the connection's lines, and a row `{conn, n, line}` in
// `lines`, committed in one transaction with the reply.
import { bytes } from '@zen/client';
import type { Db, DbOptions } from '@zen/db';
import { type RawHandler, type ServeHandle, type ServeOptions, serve } from '@zen/svc';

/** The database namespace. */
export const NS = 'echo';

/** The service's topic path: its topic id is the service id. */
export const SERVICE = ['svc', 'echo'];

/** A line, as stored. */
export type Line = {
  /** The connection id, hex. */
  conn: string;
  n: number;
  line: string;
};

export const DB_OPTIONS: DbOptions = {
  schema: 1,
  migrations: {
    1: async (m) => {
      await m.createTable('lines', { pk: ['conn', 'n'] });
      await m.createIndex('lines', 'by_conn_n', {
        fields: [
          ['conn', 'text'],
          ['n', 'int'],
        ],
      });
    },
  },
};

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

/** Serve the echo service from `db` (until `stop()`). */
export function serveEcho(db: Db, opts: Partial<Omit<ServeOptions, 'handler'>> = {}): ServeHandle {
  return serve(db, { service: SERVICE, handler: echo, ...opts });
}

/** Every stored line, by connection (then count, through the index). */
export function lines(db: Db): Promise<Line[]> {
  return db.table<Line>('lines').query().orderBy('conn').all();
}
