// The hello app in Chromium: two members in two browser contexts.
import { expect, test } from '@playwright/test';
import { openApp, startWorld, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';

let w: World;

test.beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob'], grants: GRANTS });
});

test.afterAll(async () => {
  await w?.stop();
});

test('a note and the shared text reach the other browser', async ({ browser }) => {
  const [ca, cb] = [await browser.newContext(), await browser.newContext()];
  const [pa, pb] = [await ca.newPage(), await cb.newPage()];
  await openApp(pa, w, 'hello', 'alice');
  await openApp(pb, w, 'hello', 'bob');

  await pa.fill('#note-text', 'from alice');
  await pa.click('#note-add');
  await expect(pb.locator('#notes .note')).toHaveText(['from alice']);

  await pb.fill('#shared-text', 'written by bob');
  await expect(pa.locator('#shared-text')).toHaveValue('written by bob');

  await ca.close();
  await cb.close();
});
