// The echo app's database: its namespace, tables, the certificate row, the
// SSH host key and the users' SSH keys. Kept apart from echo.ts so that
// code without @zen/svc (the Playwright spec, which puts the keys) can use
// it.
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

/** A user's SSH key, as stored: `user` may log in with it. */
export type SshKey = {
  user: string;
  /** `SHA256:…`, as `ssh-keygen -l` prints it. */
  fingerprint: string;
  /** The public line, `ssh-ed25519 AAAA…`. */
  line: string;
};

/** The `certs` row that holds the SSH host key: `key` is its PEM, `chain` its public line. */
export const SSH_HOST_KEY = 'ssh';

export const DB_OPTIONS: DbOptions = {
  schema: 3,
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
    3: async (m) => {
      await m.createTable('ssh_keys', { pk: ['user', 'fingerprint'] });
    },
  },
};

/** Store the certificate `name` (a row, sealed like every row). */
export async function putCert(db: Db, cert: Cert): Promise<void> {
  await db.table<Cert>('certs').put(cert);
}

/** Store the SSH host key (OpenSSH PEM, Ed25519) with its public line. */
export async function putHostKey(db: Db, pem: string, line: string): Promise<void> {
  await putCert(db, { name: SSH_HOST_KEY, chain: line, key: pem });
}

/** A public line's fingerprint, `SHA256:…` as `ssh-keygen -l` prints it. */
export async function sshFingerprint(line: string): Promise<string> {
  const b64 = line.trim().split(/\s+/)[1];
  if (!b64) throw new Error('not a public key line');
  const blob = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
  const h = new Uint8Array(await crypto.subtle.digest('SHA-256', blob));
  return `SHA256:${btoa(String.fromCharCode(...h)).replace(/=+$/, '')}`;
}

/** Let `user` log in over SSH with the key of `line` (`ssh-ed25519 AAAA…`). */
export async function putSshKey(db: Db, user: string, line: string): Promise<void> {
  const l = line.trim().split(/\s+/).slice(0, 2).join(' ');
  await db.table<SshKey>('ssh_keys').put({ user, fingerprint: await sshFingerprint(l), line: l });
}
