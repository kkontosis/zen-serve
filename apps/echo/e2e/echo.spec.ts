// The echo app in Chromium: Alice's tab serves the service; a Node `net`
// peer sends a line, gets its reply, and the line shows in the tab.
import { connect } from 'node:net';
import { expect, test } from '@playwright/test';
import { openApp, startWorld, until, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';

let w: World;

test.beforeAll(async () => {
  w = await startWorld({
    users: ['alice'],
    grants: GRANTS,
    services: [{ name: 'echo', path: ['svc', 'echo'] }],
  });
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
