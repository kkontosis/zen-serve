// The kanban app in Chromium: two members on one board, in two browser
// contexts (docs/EXAMPLES.md §1, the milestone 5 demo).
import { type Browser, type BrowserContext, expect, type Page, test } from '@playwright/test';
import { openApp, startWorld, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';

let w: World;

test.beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob'], grants: GRANTS });
});

test.afterAll(async () => {
  await w?.stop();
});

/** The contexts a test opened: closed after it, passed or failed, before the server stops. */
const open: BrowserContext[] = [];
test.afterEach(async () => {
  for (const c of open.splice(0)) await c.close();
});

/** Alice and Bob on the same board, each in a browser context of their own. */
async function two(browser: Browser, query: string) {
  const [ca, cb] = [await browser.newContext(), await browser.newContext()];
  open.push(ca, cb);
  const [pa, pb] = [await ca.newPage(), await cb.newPage()];
  // Alice first: she creates the board.
  await openApp(pa, w, 'kanban', 'alice', { query });
  await openApp(pb, w, 'kanban', 'bob', { query });
  return { pa, pb };
}

const column = (p: Page, n: number) => p.locator(`.column:nth-child(${n})`);
const card = (p: Page, title: string) => p.locator('.card', { hasText: title });

async function addCard(p: Page, title: string) {
  await p.fill('#new-title', title);
  await p.click('#new-add');
  await expect(column(p, 1).locator('.card .title', { hasText: title })).toHaveCount(1);
}

test('a drag and an attachment in one browser show in the other', async ({ browser }) => {
  const { pa, pb } = await two(browser, 'board=e2e-drag');
  await addCard(pa, 'Buy milk');
  await addCard(pa, 'Fix the bike');
  await expect(column(pb, 1).locator('.card .title')).toHaveText(['Buy milk', 'Fix the bike']);

  // Drag "Fix the bike" to "Doing" (HTML5 drag and drop).
  await card(pa, 'Fix the bike').dragTo(column(pa, 2).locator('.cards'));
  await expect(column(pb, 2).locator('.card .title')).toHaveText(['Fix the bike']);
  await expect(column(pb, 1).locator('.card .title')).toHaveText(['Buy milk']);
  // Drop "Buy milk" onto it: it goes before.
  await card(pa, 'Buy milk').dragTo(card(pa, 'Fix the bike'));
  await expect(column(pb, 2).locator('.card .title')).toHaveText(['Buy milk', 'Fix the bike']);
  await expect(column(pb, 1).locator('.card')).toHaveCount(0);
  // And the buttons.
  await card(pb, 'Fix the bike').locator('.move-right').click();
  await expect(column(pa, 3).locator('.card .title')).toHaveText(['Fix the bike']);

  // Attach a file in Alice's browser.
  await card(pa, 'Buy milk').click();
  await pa.setInputFiles('#attach-input', {
    name: 'list.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('milk, eggs, bread'),
  });
  await expect(card(pb, 'Buy milk').locator('.clip')).toHaveText(/1/);
  await card(pb, 'Buy milk').click();
  await expect(pb.locator('#attachments .attachment')).toHaveText(['list.txt (17 B)']);
  // Bob reads it.
  const download = pb.waitForEvent('download');
  await pb.locator('#attachments .attachment a').click();
  const d = await download;
  expect(d.suggestedFilename()).toBe('list.txt');
  const chunks: Buffer[] = [];
  for await (const c of (await d.createReadStream())!) chunks.push(c as Buffer);
  expect(Buffer.concat(chunks).toString()).toBe('milk, eggs, bread');

  // Bob deletes it; it goes from Alice's board.
  await pb.click('#card-delete');
  await expect(column(pa, 2).locator('.card')).toHaveCount(0);
  for (const p of [pa, pb]) {
    expect(
      await p.evaluate(() => (window as unknown as { app: { error(): string } }).app.error()),
    ).toBe('');
  }
});

test('the card text (Loro) merges edits made at once', async ({ browser }) => {
  const { pa, pb } = await two(browser, 'board=e2e-text');
  await addCard(pa, 'Shopping');
  await card(pa, 'Shopping').click();
  await card(pb, 'Shopping').click();
  await pa.locator('#card-text').pressSequentially('milk');
  await expect(pb.locator('#card-text')).toHaveValue('milk');
  // At the same time: Alice types at the start, Bob at the end.
  await pa.locator('#card-text').press('Control+Home');
  await pb.locator('#card-text').press('Control+End');
  await Promise.all([
    pa.locator('#card-text').pressSequentially('buy ', { delay: 20 }),
    pb.locator('#card-text').pressSequentially(' and eggs', { delay: 20 }),
  ]);
  for (const p of [pa, pb]) await expect(p.locator('#card-text')).toHaveValue('buy milk and eggs');
});

test('the offline variant: votes add up and labels merge across browsers', async ({ browser }) => {
  const { pa, pb } = await two(browser, 'board=e2e-crdt&crdt=1');
  await addCard(pa, 'Picnic');
  await expect(column(pb, 1).locator('.card .title')).toHaveText(['Picnic']);
  await card(pa, 'Picnic').click();
  await card(pb, 'Picnic').click();
  await pa.click('#card-vote');
  await pb.click('#card-vote');
  await pa.fill('#label-text', 'outdoor');
  await pa.click('#label-add');
  await pb.fill('#label-text', 'food');
  await pb.click('#label-add');
  for (const p of [pa, pb]) {
    await expect(card(p, 'Picnic').locator('.votes')).toHaveText(/▲2/);
    await expect(card(p, 'Picnic').locator('.labels')).toHaveText(/\[food, outdoor\]/);
  }
  await card(pb, 'Picnic').locator('.move-right').click();
  await expect(column(pa, 2).locator('.card .title')).toHaveText(['Picnic']);
  await expect(column(pa, 1).locator('.card')).toHaveCount(0);
});
