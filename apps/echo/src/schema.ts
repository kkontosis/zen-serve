// The echo app's database: its namespace, tables and the certificate row.
// Kept apart from echo.ts so that code without @zen/svc (the Playwright
// spec, which puts the certificate) can use it.
import type { Db, DbOptions } from '@zen/db';

/** The database namespace. */
export const NS = 'echo';

/** A line, as stored. */
export type Line = {
  /** The connection id, hex. */
  conn: string;
  n: number;
  line: string;
};

/** A certificate, as stored (PEM). */
export type Cert = {
  name: string;
  /** The chain, leaf first. */
  chain: string;
  key: string;
};

export const DB_OPTIONS: DbOptions = {
  schema: 2,
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
    2: async (m) => {
      await m.createTable('certs', { pk: ['name'] });
    },
  },
};

/** Store the certificate `name` (a row, sealed like every row). */
export async function putCert(db: Db, cert: Cert): Promise<void> {
  await db.table<Cert>('certs').put(cert);
}
