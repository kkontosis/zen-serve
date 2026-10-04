// Browser smoke test: Chromium with a virtual authenticator (CTAP2, PRF)
// runs browser/smoke.js against a zen-serve that serves the page itself.
// Build first: scripts/build-wasm.sh, npm run build -w @zen/client.
import { cp, mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { expect, test } from '@playwright/test';
import { claim, connect, fullGrants, member, zw } from '../dist/index.js';
import { spawnServer, type TestServer, testUser } from '../dist/testing/index.js';
import { initWasm } from '../dist/wasm.js';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../../..');

/** Copy the page, the built client and the WASM glue into one static directory. */
async function site(): Promise<string> {
  const dir = join(await mkdtempDir(), 'site');
  await mkdir(dir, { recursive: true });
  await cp(join(here, 'index.html'), join(dir, 'index.html'));
  await cp(join(here, 'smoke.js'), join(dir, 'smoke.js'));
  await cp(join(root, 'packages/zen-wasm/pkg'), join(dir, 'wasm'), { recursive: true });
  await cp(join(root, 'packages/client/dist'), join(dir, 'client'), { recursive: true });
  // The bare specifier '@zen/wasm' has no import map here (inline scripts are
  // refused by the default CSP): point it at the copied glue.
  for (const f of await walk(join(dir, 'client'))) {
    if (!f.endsWith('.js')) continue;
    const depth = f.slice(join(dir, 'client').length).split('/').length - 2;
    const rel = `${'../'.repeat(depth + 1)}wasm/zen_wasm.js`;
    const src = await readFile(f, 'utf8');
    await writeFile(f, src.replaceAll("from '@zen/wasm'", `from '${rel}'`));
  }
  return dir;
}

async function walk(d: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(d, { withFileTypes: true })) {
    const p = join(d, e.name);
    if (e.isDirectory()) out.push(...(await walk(p)));
    else out.push(p);
  }
  return out;
}

async function mkdtempDir(): Promise<string> {
  const { mkdtemp } = await import('node:fs/promises');
  return mkdtemp(join(tmpdir(), 'zen-site-'));
}

let server: TestServer;
const cfg = { name: 'web', password: 'browser pw', passphrase: 'admin passphrase' };

test.beforeAll(async () => {
  await initWasm();
  const dir = await site();
  server = await spawnServer({ top: { unencrypted_dir: dir } });
  const origin = `http://localhost:${server.port}`;
  const client = await connect(server.url, { origin });
  const admin = testUser(1);
  await claim(client, server.claimToken, admin.identity, {
    members: [member(admin.identity, [admin.cert])],
    grants: fullGrants(1, admin.fp),
  });
  const s = await client.signInDevice(admin);
  await s.setPassword(cfg.name, cfg.password);
  const ufs = await s
    .fs(1)
    .init((k) => [
      zw.createPassphraseSlot(k, new TextEncoder().encode(cfg.passphrase), 65536, 1, 1),
    ]);
  ufs.close();
});

test.afterAll(() => server?.stop());

test('sign in, passkey with PRF unlock, KV', async ({ page }) => {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
      hasPrf: true,
    },
  });
  page.on('console', (m) => console.log(`[page] ${m.text()}`));
  await page.goto(`http://localhost:${server.port}/unencrypted/index.html`);
  await page.waitForFunction(() => (window as unknown as { smokeReady?: boolean }).smokeReady);
  const r = await page.evaluate(
    (c) =>
      (
        window as unknown as { runSmoke: (c: unknown) => Promise<Record<string, unknown>> }
      ).runSmoke(c),
    cfg,
  );
  console.log(r);
  expect(r.ok, String(r.error)).toBe(true);
  expect(r.origin).toBe(`http://localhost:${server.port}`);
  expect(r.prf).toBe(true);
  expect(r.passkeyMethod).toBe('passkey');
  expect(r.kv).toBe('from the browser');
});
