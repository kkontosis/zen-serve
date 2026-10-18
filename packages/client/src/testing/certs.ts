// Test certificates made with the openssl CLI: a CA and a server
// certificate for localhost and 127.0.0.1, for TLS tests of relayed
// services (packages/svc, apps/echo).
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/** The server key's algorithm. */
export type TestKeyKind = 'p256' | 'p384' | 'ed25519' | 'rsa';

/** A CA and a server certificate it signed (PEM). */
export interface TestCerts {
  /** The CA certificate, for the client's trust. */
  ca: string;
  /** The server's chain: its certificate, then the CA's. */
  chain: string;
  /** The server's key, PKCS#8. */
  key: string;
}

const KEY_ARGS: Record<TestKeyKind, string[]> = {
  p256: ['-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256'],
  p384: ['-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-384'],
  ed25519: ['-newkey', 'ed25519'],
  rsa: ['-newkey', 'rsa:2048'],
};

/** Make a CA (P-256) and a server certificate for localhost and 127.0.0.1. Needs `openssl`. */
export function testCerts(kind: TestKeyKind = 'p256'): TestCerts {
  const dir = mkdtempSync(join(tmpdir(), 'zen-certs-'));
  const openssl = (...args: string[]) => {
    try {
      execFileSync('openssl', args, { cwd: dir, stdio: 'pipe' });
    } catch (e) {
      const code = (e as { code?: string }).code;
      if (code === 'ENOENT') throw new Error('these tests need the openssl CLI');
      throw e;
    }
  };
  try {
    openssl(
      'req',
      '-x509',
      ...KEY_ARGS.p256,
      '-nodes',
      '-keyout',
      'ca.key',
      '-out',
      'ca.pem',
      '-days',
      '2',
      '-subj',
      '/CN=zen test CA',
      '-addext',
      'basicConstraints=critical,CA:TRUE',
      '-addext',
      'keyUsage=critical,keyCertSign',
    );
    openssl(
      'req',
      ...KEY_ARGS[kind],
      '-nodes',
      '-keyout',
      'leaf.key',
      '-out',
      'leaf.csr',
      '-subj',
      '/CN=localhost',
    );
    writeFileSync(
      join(dir, 'leaf.ext'),
      'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\n' +
        'keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n',
    );
    openssl(
      'x509',
      '-req',
      '-in',
      'leaf.csr',
      '-CA',
      'ca.pem',
      '-CAkey',
      'ca.key',
      '-CAcreateserial',
      '-out',
      'leaf.pem',
      '-days',
      '2',
      '-extfile',
      'leaf.ext',
    );
    const read = (f: string) => readFileSync(join(dir, f), 'utf8');
    return { ca: read('ca.pem'), chain: read('leaf.pem') + read('ca.pem'), key: read('leaf.key') };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
