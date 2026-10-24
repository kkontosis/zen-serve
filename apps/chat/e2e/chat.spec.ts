// The chat app in Chromium (EXAMPLES.md §2, the milestone 5 demo): three
// members in three browser contexts. One goes offline while the others
// talk, and sees every message exactly once, in order, when it is back.
// Typing indicators reach the others and never the log, which a Node
// session reads directly.
import { type BrowserContext, expect, type Page, test } from '@playwright/test';
import { decodeMsg } from '@zen/db';
import { openApp, startWorld, type World } from '../../shared/harness.js';
import { signInWorker } from '../../shared/node.js';
import { GRANTS } from '../test/grants.js';

let w: World;
const contexts: BrowserContext[] = [];
const pages: Record<string, Page> = {};

/** What the page exposes on `window.app` (ui/main.ts). */
type App = { texts(): string[]; offsets(): string[]; typingLog(): unknown[] };
const texts = (p: Page) => p.evaluate(() => (window as unknown as { app: App }).app.texts());
const offsets = (p: Page) => p.evaluate(() => (window as unknown as { app: App }).app.offsets());

/** Each page's typing indicators, sent and received, as a failure message. */
async function typingLogs(): Promise<string> {
  const lines = [`now ${Date.now()}`];
  for (const [name, p] of Object.entries(pages)) {
    const log = await p.evaluate(() => (window as unknown as { app: App }).app.typingLog());
    lines.push(`${name}: ${log.map((e) => JSON.stringify(e)).join('\n  ')}`);
  }
  return lines.join('\n');
}

async function send(p: Page, text: string): Promise<void> {
  await p.fill('#msg-text', text);
  await p.click('#msg-send');
  await expect.poll(() => texts(p)).toContain(text);
}

test.beforeAll(async ({ browser }) => {
  w = await startWorld({ users: ['alice', 'bob', 'carol'], grants: GRANTS });
  for (const name of ['alice', 'bob', 'carol']) {
    const c = await browser.newContext();
    contexts.push(c);
    const p = await c.newPage();
    await openApp(p, w, 'chat', name);
    pages[name] = p;
  }
});

test.afterAll(async () => {
  for (const c of contexts) await c.close();
  await w?.stop();
});

test('offline, then back: every message exactly once, in order', async () => {
  const { alice, bob, carol } = pages as Record<string, Page>;
  await send(alice!, 'hello all');
  for (const p of [bob!, carol!]) await expect.poll(() => texts(p)).toEqual(['hello all']);

  // Carol's device goes offline; Alice and Bob go on talking.
  const carolContext = contexts[2]!;
  await carolContext.setOffline(true);
  await carol!.waitForFunction(() => !navigator.onLine);
  const sent = ['a1', 'b1', 'a2', 'b2', 'a3', 'b3'];
  for (const t of sent) await send(t.startsWith('a') ? alice! : bob!, t);
  const want = ['hello all', ...sent];
  await expect.poll(() => texts(alice!)).toEqual(want);
  await expect.poll(() => texts(bob!)).toEqual(want);
  // Nothing reached Carol while offline.
  expect(await texts(carol!)).toEqual(['hello all']);

  // Back online: the history she missed, then live messages, each once.
  await carolContext.setOffline(false);
  await expect.poll(() => texts(carol!), { timeout: 30_000 }).toEqual(want);
  await send(bob!, 'welcome back');
  const all = [...want, 'welcome back'];
  for (const p of [alice!, bob!, carol!]) await expect.poll(() => texts(p)).toEqual(all);
  // Every offset once, and the same order everywhere.
  const carolOffsets = await offsets(carol!);
  expect(new Set(carolOffsets).size).toBe(carolOffsets.length);
  expect(carolOffsets).toEqual(await offsets(alice!));
});

test("typing reaches the others and never the log; who's online counts every page", async () => {
  const { alice, bob, carol } = pages as Record<string, Page>;
  await bob!.locator('#msg-text').pressSequentially('typing something', { delay: 30 });
  try {
    await expect(alice!.locator('#typing')).toHaveText('bob typing…');
    await expect(carol!.locator('#typing')).toHaveText('bob typing…');
  } catch (e) {
    throw new Error(`${(e as Error).message}\n\ntyping indicators:\n${await typingLogs()}`);
  }
  await bob!.click('#msg-send');
  await expect.poll(() => texts(alice!)).toContain('typing something');

  // A Node session reads the log: the typing topic holds no event, and the
  // room's topic holds only messages.
  const node = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  try {
    const typingEvents = [];
    for await (const e of node.fs.topic('chat', 'family', 'typing').read()) typingEvents.push(e);
    expect(typingEvents).toEqual([]);
    const types: string[] = [];
    for await (const e of node.fs.topic('chat', 'family').read()) {
      types.push(decodeMsg(e.payload).type);
    }
    expect(types.length).toBe((await texts(alice!)).length);
    expect(new Set(types)).toEqual(new Set(['msg']));
  } finally {
    await node.close();
  }

  // Who's online: the three running pages answer.
  await alice!.click('#who');
  await expect(alice!.locator('#online-list')).toHaveText('3 online: alice, bob, carol');
});

test('a reaction shows on every page', async () => {
  const { alice, bob } = pages as Record<string, Page>;
  const like = '#messages li.msg:first-child .react[data-emoji="like"]';
  await bob!.click(like);
  await expect(alice!.locator(like)).toHaveText('👍 1');
  await alice!.click(like);
  await expect(bob!.locator(like)).toHaveText('👍 2');
});
