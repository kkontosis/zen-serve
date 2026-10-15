// The checkout app in Chromium: the customer places orders in the page;
// the services and the saga's orchestrator run as Node workers.
import { expect, type Page, test } from '@playwright/test';
import { openApp, startWorld, type WorkerProcess, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';
import { Services, startProvider, USERS } from '../test/services.js';

let w: World;
let provider: { p: WorkerProcess; url: string };
let services: Services;
let page: Page;

test.beforeAll(async ({ browser }) => {
  w = await startWorld({
    users: USERS,
    grants: GRANTS,
    server: { limits: { claim_ttl_ms: 1000 } },
  });
  provider = await startProvider();
  services = new Services(w, { PROVIDER_URL: provider.url });
  await services.startAll();
  page = await (await browser.newContext()).newPage();
  await openApp(page, w, 'checkout', 'alice');
  await page.click('#restock');
  await expect(page.locator('#stock-apple .available')).toHaveText('10');
});

test.afterAll(async () => {
  await page?.context().close();
  await services?.stop();
  await provider?.p.kill();
  await w?.stop();
});

const available = async (sku: string) =>
  Number(await page.locator(`#stock-${sku} .available`).textContent());

test('an order goes through to done', async () => {
  await page.fill('#customer', 'carol');
  await page.fill('#qty-apple', '2');
  await page.fill('#qty-cheese', '1');
  await page.click('#place');
  const order = page.locator('#orders .order').first();
  await expect(order.locator('.customer')).toHaveText('carol');
  await expect(order.locator('.status')).toHaveText('done', { timeout: 30_000 });
  await expect(page.locator('#stock-apple .available')).toHaveText('8');
  await expect(page.locator('#stock-cheese .available')).toHaveText('9');
});

test('a declined payment ends failed, and its stock is released', async () => {
  const before = await available('bread');
  await page.fill('#customer', 'decline-dave');
  await page.fill('#qty-bread', '3');
  await page.click('#place');
  const order = page.locator('#orders .order').first();
  await expect(order.locator('.customer')).toHaveText('decline-dave');
  // Reserved by the first step, then released by its compensation.
  await expect(order.locator('.status')).toHaveText('failed', { timeout: 30_000 });
  await expect(page.locator('#stock-bread .available')).toHaveText(String(before));
  const reserved = await page.evaluate(async () => {
    const app = (
      window as unknown as {
        app: { db: { table(n: string): { query(): { all(): Promise<unknown[]> } } } };
      }
    ).app;
    return (await app.db.table('reservations').query().all()).length;
  });
  // Only the done order's two lines are still reserved.
  expect(reserved).toBe(2);
});
