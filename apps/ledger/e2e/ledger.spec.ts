// The ledger in Chromium: two members in two browser contexts.
import { type Browser, expect, type Page, test } from '@playwright/test';
import { openApp, startWorld, type World } from '../../shared/harness.js';
import { GRANTS } from '../test/grants.js';

let w: World;

test.beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob'], grants: GRANTS });
});

test.afterAll(async () => {
  await w?.stop();
});

async function two(browser: Browser, query?: string) {
  const [ca, cb] = [await browser.newContext(), await browser.newContext()];
  const [pa, pb] = [await ca.newPage(), await cb.newPage()];
  await openApp(pa, w, 'ledger', 'alice', query ? { query } : {});
  await openApp(pb, w, 'ledger', 'bob', query ? { query } : {});
  return {
    pa,
    pb,
    close: async () => {
      await ca.close();
      await cb.close();
    },
  };
}

const balance = (p: Page, id: string) => p.locator(`#accounts tr[data-id="${id}"] .balance`);

async function open(p: Page, id: string, deposit: number) {
  await p.fill('#open-id', id);
  await p.fill('#open-deposit', String(deposit));
  await p.click('#open-add');
  await expect(balance(p, id)).toHaveText(String(deposit));
}

async function send(p: Page, from: string, to: string, amount: number, memo = '') {
  await expect(p.locator(`#tr-from option[value="${from}"]`)).toHaveCount(1);
  await expect(p.locator(`#tr-to option[value="${to}"]`)).toHaveCount(1);
  await p.selectOption('#tr-from', from);
  await p.selectOption('#tr-to', to);
  await p.fill('#tr-amount', String(amount));
  await p.fill('#tr-memo', memo);
  await p.click('#tr-send');
}

test('transfers in both browsers update the balances in both, and an overdraft is refused', async ({
  browser,
}) => {
  const { pa, pb, close } = await two(browser);
  await open(pa, 'alice', 100);
  await open(pb, 'bob', 100);
  for (const p of [pa, pb]) {
    await expect(balance(p, 'alice')).toHaveText('100');
    await expect(balance(p, 'bob')).toHaveText('100');
  }

  await send(pa, 'alice', 'bob', 30, 'lunch');
  for (const p of [pa, pb]) {
    await expect(balance(p, 'alice')).toHaveText('70');
    await expect(balance(p, 'bob')).toHaveText('130');
  }

  await send(pb, 'bob', 'alice', 45, 'rent');
  for (const p of [pa, pb]) {
    await expect(balance(p, 'alice')).toHaveText('115');
    await expect(balance(p, 'bob')).toHaveText('85');
  }

  await send(pb, 'bob', 'alice', 1000, 'too much');
  await expect(pb.locator('#tr-error')).toContainText('overdraft');
  await expect(balance(pb, 'bob')).toHaveText('85');

  // Each device's audit view has both transfers, and only those.
  for (const p of [pa, pb]) {
    await expect(p.locator('#audit .audit')).toHaveCount(2);
    await expect(p.locator('#audit .audit').first()).toContainText('bob → alice: 45');
  }
  // The statement projection (run by either page) counts both entries of bob's month.
  await pa.selectOption('#hist-account', 'bob');
  await expect(pa.locator('#stmt-in')).toHaveText('in 130');
  await expect(pa.locator('#stmt-out')).toHaveText('out 45');
  await close();
});

test('an account history pages newest first, older and newer', async ({ browser }) => {
  const { pa, pb, close } = await two(browser, 'page=5');
  await open(pa, 'pager-a', 100);
  await open(pa, 'pager-b', 0);
  for (let i = 1; i <= 12; i++) {
    await send(pa, 'pager-a', 'pager-b', 1, `t${i}`);
    await expect(balance(pa, 'pager-a')).toHaveText(String(100 - i));
  }
  await expect(balance(pb, 'pager-b')).toHaveText('12');

  const pages = [
    ['t12', 't11', 't10', 't9', 't8'],
    ['t7', 't6', 't5', 't4', 't3'],
    ['t2', 't1', 'opening deposit'],
  ];
  for (const p of [pa, pb]) {
    await p.selectOption('#hist-account', 'pager-a');
    const memos = p.locator('#history .entry .memo');
    const check = async (i: number) => {
      await expect(memos).toHaveText(pages[i]!);
      await expect(p.locator('#hist-page')).toHaveText(`page ${i + 1}`);
      await expect(p.locator('#hist-newer')).toBeEnabled({ enabled: i > 0 });
      await expect(p.locator('#hist-older')).toBeEnabled({ enabled: i < pages.length - 1 });
      const ats = (await p
        .locator('#history .entry')
        .evaluateAll((es) => es.map((e) => Number(e.getAttribute('data-at'))))) as number[];
      for (let k = 1; k < ats.length; k++) expect(ats[k]!).toBeLessThan(ats[k - 1]!);
      return ats;
    };
    const first = await check(0);
    await p.click('#hist-older');
    const second = await check(1);
    expect(second[0]!).toBeLessThan(first.at(-1)!);
    await p.click('#hist-older');
    const third = await check(2);
    expect(third[0]!).toBeLessThan(second.at(-1)!);
    await p.click('#hist-newer');
    await check(1);
    await p.click('#hist-newer');
    await check(0);
  }
  await expect(pa.locator('#history .entry .amount').first()).toHaveText('-1');
  await close();
});
