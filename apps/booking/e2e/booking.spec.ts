// The booking app in Chromium: two members in two browser contexts. One
// books a slot; the other sees it taken and joins the waitlist; the first
// cancels; the second is promoted (its booking and a notification).
import { expect, test } from '@playwright/test';
import { openApp, startWorld, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';

let w: World;

test.beforeAll(async () => {
  w = await startWorld({
    users: ['alice', 'bob'],
    grants: GRANTS,
    server: { limits: { claim_ttl_ms: 1000 } },
  });
});

test.afterAll(async () => {
  await w?.stop();
});

test('book, waitlist, cancel, promotion', async ({ browser }) => {
  const [ca, cb] = [await browser.newContext(), await browser.newContext()];
  const [pa, pb] = [await ca.newPage(), await cb.newPage()];
  await openApp(pa, w, 'booking', 'alice');
  await openApp(pb, w, 'booking', 'bob');

  await pa.fill('#slot-resource', 'Court 1');
  await pa.click('#slot-add');
  const slotA = pa.locator('.slot[data-resource="Court 1"]');
  const slotB = pb.locator('.slot[data-resource="Court 1"]');
  await expect(slotA.locator('.status')).toHaveText('free');

  await slotA.locator('button.book').click();
  await expect(slotA.locator('.status')).toHaveText('yours');
  await expect(pa.locator('#my-bookings .mine')).toHaveCount(1);

  // Bob sees it taken: no Book button, a Join button.
  await expect(slotB.locator('.status')).toHaveText('booked by alice', { timeout: 15_000 });
  await expect(slotB.locator('button.book')).toHaveCount(0);
  await slotB.locator('button.join').click();
  await expect(slotB.locator('.wait')).toHaveText('on the waitlist (#1 of 1)');

  await expect(slotA.locator('.wait')).toHaveText('1 waiting', { timeout: 15_000 });
  await slotA.locator('button.cancel').click();

  // Bob is promoted, once: his booking and one notification.
  await expect(pb.locator('#notices .notice')).toHaveText(
    [/^promoted from the waitlist: Court 1/],
    {
      timeout: 30_000,
    },
  );
  await expect(slotB.locator('.status')).toHaveText('yours');
  await expect(slotB.locator('.wait')).toHaveText('');
  await expect(pb.locator('#my-bookings .mine')).toHaveCount(1);
  await expect(slotA.locator('.status')).toHaveText('booked by bob', { timeout: 15_000 });
  await expect(pa.locator('#my-bookings .mine')).toHaveCount(0);
  await expect(pa.locator('#notices .notice')).toHaveCount(0);

  await ca.close();
  await cb.close();
});
