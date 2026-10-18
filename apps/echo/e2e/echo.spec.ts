// The echo app in Chromium: Alice's tab serves the service; a Node `net`
// peer sends a line, gets its reply, and the line shows in the tab. Over
// TLS too: the tab loads @zen/svc-wasm next to its bundle.
import { connect } from 'node:net';
import { connect as tlsConnect } from 'node:tls';
import { expect, test } from '@playwright/test';
import { testCerts } from '../../../packages/client/src/testing/index.js';
import { openApp, startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker } from '../../shared/node.js';
import { DB_OPTIONS, NS, putCert } from '../src/echo.js';
import { GRANTS } from '../test/grants.js';

let w: World;
let ca: string;

test.beforeAll(async () => {
  w = await startWorld({
    users: ['alice'],
    grants: GRANTS,
    services: [
      { name: 'echo', path: ['svc', 'echo'] },
      { name: 'echo-tls', path: ['svc', 'echo-tls'] },
    ],
  });
  // The certificate goes into Alice's `certs` table, where the tab reads it.
  const alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  try {
    const db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
    const c = testCerts();
    ca = c.ca;
    await putCert(db, { name: 'echo', chain: c.chain, key: c.key });
  } finally {
    await alice.close();
  }
});

test.afterAll(async () => {
  await w?.stop();
});

test('a browser tab serves the echo service', async ({ page }) => {
  await openApp(page, w, 'echo', 'alice');
  await page.click('#serve-toggle');
  await expect(page.locator('#serve-status')).toHaveText('serving');

  const s = connect(w.servicePort('echo'), '127.0.0.1');
  let got = '';
  s.on('data', (d) => {
    got += d.toString();
  });
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  s.write('from the browser\n');
  await until(() => got === '1: from the browser\n', 30_000, 'the reply');
  await expect(page.locator('#lines .line')).toContainText([/1: from the browser$/]);
  s.end();
  await until(() => got.endsWith('bye\n'), 30_000, 'bye');
  s.destroy();
});

test('a browser tab serves the echo service over TLS', async ({ page }) => {
  await openApp(page, w, 'echo', 'alice');
  await page.click('#serve-tls-toggle');
  await expect(page.locator('#serve-tls-status')).toHaveText('serving');

  const s = tlsConnect({
    host: '127.0.0.1',
    port: w.servicePort('echo-tls'),
    ca,
    servername: 'localhost',
  });
  let got = '';
  s.on('data', (d) => {
    got += d.toString();
  });
  await new Promise<void>((ok, fail) => s.once('secureConnect', ok).once('error', fail));
  s.write('over TLS\n');
  await until(() => got === '1: over TLS\n', 30_000, 'the reply');
  await expect(page.locator('#lines .line')).toContainText([/1: over TLS$/]);
  await expect(page.locator('#serve-tls-status')).toHaveText('serving');
  s.end();
  await until(() => got.endsWith('bye\n'), 30_000, 'bye');
  s.destroy();
});
