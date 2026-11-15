// What the browser tests (sw.spec.ts) and the bench (bench.spec.ts) do on
// pages: enroll a context's content origin from the test app, open a
// controlled page, and fetch from one.
import { type BrowserContext, expect, type Page } from '@playwright/test';
import { ADMIN, type SwWorld } from './world.js';

/** The site's mount: `/s/` is `/site` of fs 1. */
export const siteMounts = (w: SwWorld) => [{ name: 's', fs: 1, tree: w.tree, base: '/site' }];

/** Enroll this context's content origin from the test app (sw.md §8). */
export async function enrollIn(
  w: SwWorld,
  ctx: BrowserContext,
  admin = true,
): Promise<{ fp: string; inAcl: boolean }> {
  const page = await ctx.newPage();
  await page.goto(w.appPage());
  const popup = ctx.waitForEvent('page');
  const result = page.evaluate(
    (a) =>
      (
        globalThis as never as { zenEnroll(a: unknown): Promise<{ fp: string; inAcl: boolean }> }
      ).zenEnroll(a),
    {
      ...ADMIN,
      contentOrigin: w.content,
      mounts: siteMounts(w),
      admin,
    },
  );
  const p = await popup;
  await expect(p.locator('#origin')).toHaveText(w.app);
  await p.locator('#confirm').click();
  const r = await result;
  await page.close();
  return r;
}

/** A page on the content origin, controlled by the worker. */
export async function controlledPage(w: SwWorld, ctx: BrowserContext, path = '/s/'): Promise<Page> {
  const page = await ctx.newPage();
  await page.goto(`${w.content}${path}`);
  await page.waitForFunction(() => !!navigator.serviceWorker.controller);
  return page;
}

/** `fetch` from the page: status, headers and the body's bytes. */
export function fetchIn(page: Page, path: string, headers: Record<string, string> = {}) {
  return page.evaluate(
    async ([p, h]) => {
      const r = await fetch(p, { headers: h, cache: 'no-store' });
      return {
        status: r.status,
        headers: Object.fromEntries(r.headers),
        body: Array.from(new Uint8Array(await r.arrayBuffer())),
      };
    },
    [path, headers] as const,
  );
}
