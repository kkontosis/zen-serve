// The echo app in Chromium: Alice's tab serves the service; a Node `net`
// peer sends a line, gets its reply, and the line shows in the tab. Over
// TLS and SSH too: the tab loads @zen/svc-wasm next to its bundle.
import { connect } from 'node:net';
import { connect as tlsConnect } from 'node:tls';
import { expect, test } from '@playwright/test';
import {
  runSsh,
  type TestSshKeys,
  testCerts,
  testSshKeys,
} from '../../../packages/client/src/testing/index.js';
import { openApp, startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker } from '../../shared/node.js';
import { DB_OPTIONS, NS, putCert, putHostKey, putSshKey } from '../src/schema.js';
import { GRANTS } from '../test/grants.js';

let w: World;
let ca: string;
let keys: TestSshKeys;

test.beforeAll(async () => {
  w = await startWorld({
    users: ['alice'],
    grants: GRANTS,
    services: [
      { name: 'echo', path: ['svc', 'echo'] },
      { name: 'echo-tls', path: ['svc', 'echo-tls'] },
      { name: 'echo-ssh', path: ['svc', 'echo-ssh'], more: { window: 1_048_576 } },
    ],
  });
  // The certificate and the SSH keys go into Alice's tables, where the tab
  // reads them.
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
    keys = testSshKeys();
    await putHostKey(db, keys.hostKey, keys.hostLine);
    await putSshKey(db, 'test', keys.userLine);
  } finally {
    await alice.close();
  }
});

test.afterAll(async () => {
  await w?.stop();
  keys?.cleanup();
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

test('a browser tab serves the echo service over SSH', async ({ page }) => {
  await openApp(page, w, 'echo', 'alice');
  await page.click('#serve-ssh-toggle');
  await expect(page.locator('#serve-ssh-status')).toHaveText('serving');

  const r = await runSsh([...keys.sshArgs(w.servicePort('echo-ssh')), 'lines'], 'over SSH\n');
  expect(r.stderr).toBe('');
  expect(r.code).toBe(0);
  expect(r.stdout.toString()).toBe('1: over SSH\nbye\n');
  await expect(page.locator('#lines .line')).toContainText([/1: over SSH$/]);
  await expect(page.locator('#serve-ssh-status')).toHaveText('serving');
});
