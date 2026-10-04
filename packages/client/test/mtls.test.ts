// Sign-in method 5: TLS client certificates, natively (auth.md §10), with a
// test PKI made by openssl and an undici dispatcher that presents the
// client certificate.
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Agent } from 'undici';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { claim, connect, fullGrants, isCode, member } from '../src/index.js';
import { spawnServer, type TestServer, testUser } from '../src/testing/index.js';
import { initWasm } from '../src/wasm.js';

let dir: string;
let server: TestServer;
let pem: Record<string, string>;

function openssl(...args: string[]) {
  execFileSync('openssl', args, { cwd: dir, stdio: 'pipe' });
}

/** A CA, a server certificate for 127.0.0.1/localhost, and two client certificates. */
function pki() {
  const ec = ['-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes'];
  openssl(
    'req',
    '-x509',
    ...ec,
    '-keyout',
    'ca.key',
    '-out',
    'ca.pem',
    '-days',
    '2',
    '-subj',
    '/CN=zen test CA',
  );
  const leaf = (name: string, ext: string) => {
    openssl('req', ...ec, '-keyout', `${name}.key`, '-out', `${name}.csr`, '-subj', `/CN=${name}`);
    openssl(
      'x509',
      '-req',
      '-in',
      `${name}.csr`,
      '-CA',
      'ca.pem',
      '-CAkey',
      'ca.key',
      '-CAcreateserial',
      '-out',
      `${name}.pem`,
      '-days',
      '2',
      '-extfile',
      `${name}.ext`,
    );
    void ext;
  };
  execFileSync(
    'sh',
    [
      '-c',
      [
        'printf "subjectAltName=IP:127.0.0.1,DNS:localhost\\nextendedKeyUsage=serverAuth\\n" > server.ext',
        'printf "extendedKeyUsage=clientAuth\\n" > alice.ext',
        'printf "extendedKeyUsage=clientAuth\\n" > mallory.ext',
      ].join(' && '),
    ],
    { cwd: dir },
  );
  leaf('server', 'server.ext');
  leaf('alice', 'alice.ext');
  leaf('mallory', 'mallory.ext');
}

beforeAll(async () => {
  await initWasm();
  dir = await mkdtemp(join(tmpdir(), 'zen-pki-'));
  pki();
  pem = {};
  for (const f of ['ca.pem', 'alice.pem', 'alice.key', 'mallory.pem', 'mallory.key']) {
    pem[f] = await readFile(join(dir, f), 'utf8');
  }
  const trust = new Agent({ connect: { ca: pem['ca.pem'] } });
  server = await spawnServer({
    https: true,
    probeInit: { dispatcher: trust },
    extra: `[tls]\ncert = "${join(dir, 'server.pem')}"\nkey = "${join(dir, 'server.key')}"\nclient_ca = "${join(dir, 'ca.pem')}"\n`,
  });
});

afterAll(async () => {
  await server?.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

const agent = (who?: 'alice' | 'mallory') =>
  new Agent({
    connect: {
      ca: pem['ca.pem'],
      ...(who ? { cert: pem[`${who}.pem`], key: pem[`${who}.key`] } : {}),
    },
  });

describe('5: TLS client certificates', () => {
  it("registers the connection's certificate and signs in with it", async () => {
    const origin = `https://localhost:${server.port}`;
    const admin = testUser(1);
    const asAlice = await connect(server.url, {
      origin,
      fetchInit: { dispatcher: agent('alice') },
    });
    await claim(asAlice, server.claimToken, admin.identity, {
      members: [member(admin.identity, [admin.cert])],
      grants: fullGrants(1, admin.fp),
    });
    // Bind alice's certificate (the one this connection presents) to the admin.
    const s = await asAlice.signInDevice(admin);
    const id = await s.registerMtls({ label: 'alice laptop' });
    expect(id).toHaveLength(32);

    const m = await asAlice.signInMtls();
    expect(m.info?.method).toBe('mtls');
    expect(m.deviceFp).toEqual(id);
    expect((await m.fsList()).map((f) => f.id)).toContain(1);

    // Another certificate of the same CA isn't registered.
    const asMallory = await connect(server.url, {
      origin,
      fetchInit: { dispatcher: agent('mallory') },
    });
    await expect(asMallory.signInMtls()).rejects.toSatisfy((e) => isCode(e, 'unauthorized'));
    // No certificate at all.
    const plain = await connect(server.url, { origin, fetchInit: { dispatcher: agent() } });
    await expect(plain.signInMtls()).rejects.toMatchObject({ status: 401 });
  });
});
