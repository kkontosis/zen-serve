// The worker in Chromium (spec/sw.md): the first visit, enrollment and its
// refusal, §4's headers, the sandbox, seeking, a stopped worker, an update,
// and offline reads. One server; each test has a fresh browser context
// (its own storage, so its own device), and the offline test, which stops
// the server, runs last.
import { type BrowserContext, expect, type Page, test } from '@playwright/test';
import { ADMIN, editWorker, type SwWorld, startSwWorld } from './world.js';

let w: SwWorld;
test.describe.configure({ mode: 'serial' });
test.beforeAll(async () => {
  w = await startSwWorld();
});
test.afterAll(() => w?.stop());

const mounts = () => [{ name: 's', fs: 1, tree: w.tree, base: '/site' }];

/** Enroll this context's content origin from the test app (sw.md §8). */
async function enroll(ctx: BrowserContext, admin = true): Promise<{ fp: string; inAcl: boolean }> {
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
      mounts: mounts(),
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
async function controlled(ctx: BrowserContext, path = '/s/'): Promise<Page> {
  const page = await ctx.newPage();
  await page.goto(`${w.content}${path}`);
  await page.waitForFunction(() => !!navigator.serviceWorker.controller);
  return page;
}

/** `fetch` from the page: status, headers and the body's bytes. */
function fetchIn(page: Page, path: string, headers: Record<string, string> = {}) {
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

/** sw.md §4's set. */
function sandboxed(h: Record<string, string>, what: string): void {
  expect(h['content-security-policy'], what).toContain('sandbox allow-same-origin');
  expect(h['content-security-policy'], what).toContain("script-src 'none'");
  expect(h['x-content-type-options'], what).toBe('nosniff');
  expect(h['referrer-policy'], what).toBe('no-referrer');
  expect(h['cross-origin-resource-policy'], what).toBe('same-origin');
  expect(h['cache-control'], what).toBe('no-store');
}

test('the first visit installs the worker, which answers 503 locked', async ({ browser }) => {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  const locked = page.waitForResponse(
    (r) => r.url() === `${w.content}/s/` && r.status() === 503 && r.fromServiceWorker(),
  );
  const first = await page.goto(`${w.content}/s/`);
  // The server's bootstrap page, before the worker.
  expect(first?.fromServiceWorker()).toBe(false);
  const r = await locked;
  expect(r.headers()['zen-reason']).toBe('locked');
  await expect(page.locator('h1')).toHaveText('Locked');
  await expect(page.locator('a')).toHaveAttribute('href', '/_zen/enroll.html');
  await ctx.close();
});

test('enrollment: the site is served by the worker; an untrusted origin is ignored', async ({
  browser,
}) => {
  // An origin that isn't trusted opens the page and offers mounts.
  const evil = await browser.newContext();
  const ep = await evil.newPage();
  await ep.goto(w.appPage(w.evil));
  const popupP = evil.waitForEvent('page');
  await ep.evaluate(
    ([content, tree]) => {
      const pop = window.open(`${content}/_zen/enroll.html`, 'zen-enroll', 'popup')!;
      const t = Uint8Array.from(tree.match(/../g)!, (b) => Number.parseInt(b, 16));
      const offer = {
        type: 'zen-enroll-offer',
        mounts: [{ name: 's', fs: 1, tree: t, base: '/' }],
      };
      setInterval(() => {
        pop.postMessage(offer, '*');
        pop.postMessage(
          { type: 'zen-enroll-done', cert: new Uint8Array(8), identityPublic: t },
          '*',
        );
      }, 100);
    },
    [w.content, w.tree] as const,
  );
  const pop = await popupP;
  await expect(pop.locator('#status')).toHaveText('Waiting for the app…');
  await pop.waitForTimeout(1_000);
  await expect(pop.locator('#offer')).toBeHidden();
  await expect(pop.locator('#status')).toHaveText('Waiting for the app…');
  await pop.close();
  // Nothing was kept: the origin is still locked.
  const check = await controlled(evil, '/s/');
  await expect(check.locator('h1')).toHaveText('Locked');
  await evil.close();

  const ctx = await browser.newContext();
  const r = await enroll(ctx);
  expect(r.inAcl).toBe(true);
  expect(r.fp).toMatch(/^[0-9a-f]{64}$/);
  const page = await ctx.newPage();
  const seen: { url: string; sw: boolean }[] = [];
  page.on('response', (res) => {
    if (res.url().startsWith(`${w.content}/s/`))
      seen.push({ url: res.url(), sw: res.fromServiceWorker() });
  });
  await page.goto(`${w.content}/s/`);
  await expect(page.locator('#h')).toHaveText('home');
  await expect(page.locator('#h')).toHaveCSS('color', 'rgb(1, 2, 3)');
  await expect
    .poll(() => page.locator('#img').evaluate((i: HTMLImageElement) => i.naturalWidth))
    .toBe(1);
  expect(seen.map((s) => s.url).sort()).toEqual(
    [`${w.content}/s/`, `${w.content}/s/img/dot.png`, `${w.content}/s/style.css`].sort(),
  );
  expect(seen.every((s) => s.sw)).toBe(true);
  await ctx.close();
});

test("§4's headers on every response", async ({ browser }) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  const page = await controlled(ctx);
  const ok = await fetchIn(page, '/s/style.css');
  expect(ok.status).toBe(200);
  expect(ok.headers['content-type']).toBe('text/css; charset=utf-8');
  sandboxed(ok.headers, '200');
  const part = await fetchIn(page, '/s/style.css', { Range: 'bytes=0-3' });
  expect(part.status).toBe(206);
  sandboxed(part.headers, '206');
  const etag = ok.headers.etag!;
  const not = await fetchIn(page, '/s/style.css', { 'If-None-Match': etag });
  expect(not.status).toBe(304);
  sandboxed(not.headers, '304');
  const missing = await fetchIn(page, '/s/nothing.txt');
  expect(missing.status).toBe(404);
  sandboxed(missing.headers, '404');
  // The redirect of a directory without its slash, seen as the navigation follows it.
  const nav = await page.goto(`${w.content}/s/docs`);
  expect(nav?.url()).toBe(`${w.content}/s/docs/`);
  const moved = await nav!.request().redirectedFrom()!.response();
  expect(moved!.status()).toBe(301);
  sandboxed(moved!.headers(), '301');
  await ctx.close();
});

test('the sandbox: no script runs; same-origin styles and images load, others not', async ({
  browser,
}) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  await controlled(ctx);
  const page = await ctx.newPage();
  const requested: string[] = [];
  page.on('request', (r) => requested.push(r.url()));
  await page.goto(`${w.content}/s/scripts.html`);
  await expect(page.locator('#h')).toHaveCSS('color', 'rgb(1, 2, 3)');
  await expect
    .poll(() => page.locator('#local').evaluate((i: HTMLImageElement) => i.naturalWidth))
    .toBe(1);
  await page.locator('#js').click();
  await page.waitForTimeout(500);
  expect(await page.title()).toBe('sandboxed');
  expect(await page.evaluate(() => (window as never as { ran?: string }).ran)).toBeUndefined();
  expect(await page.locator('#remote').evaluate((i: HTMLImageElement) => i.naturalWidth)).toBe(0);
  // `script-src 'none'`: the external script isn't even fetched.
  expect(requested.some((u) => u.endsWith('/s/x.js'))).toBe(false);
  await ctx.close();
});

test('seeking: <audio> gets 206 from the worker, and Range fetches give the bytes', async ({
  browser,
}) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  await controlled(ctx);
  const page = await ctx.newPage();
  const ranges: { range?: string; status: number; sw: boolean }[] = [];
  page.on('response', async (res) => {
    if (!res.url().endsWith('/tone.wav')) return;
    ranges.push({
      range: (await res.request().allHeaders()).range,
      status: res.status(),
      sw: res.fromServiceWorker(),
    });
  });
  await page.goto(`${w.content}/s/media.html`);
  // The page runs no scripts, so its event listeners never fire: poll the element.
  const audio = page.locator('#a');
  await expect.poll(() => audio.evaluate((a: HTMLAudioElement) => a.readyState)).toBeGreaterThan(0);
  await audio.evaluate((a: HTMLAudioElement) => {
    a.currentTime = a.duration / 2;
  });
  await expect.poll(() => audio.evaluate((a: HTMLAudioElement) => a.seeking)).toBe(false);
  const landed = await audio.evaluate((a: HTMLAudioElement) => ({
    at: a.currentTime,
    duration: a.duration,
  }));
  expect(landed.duration).toBeCloseTo(72, 0);
  expect(landed.at).toBeCloseTo(36, 0);
  expect(ranges.some((r) => r.status === 206 && r.sw)).toBe(true);

  const model = w.files.get('tone.wav')!;
  const size = model.length;
  const cases: [string, number, number][] = [
    ['bytes=0-99', 0, 99],
    [`bytes=${size - 10}-`, size - 10, size - 1],
    ['bytes=-5', size - 5, size - 1],
    [`bytes=1500000-${1500000 + 70000}`, 1500000, 1570000],
  ];
  for (const [range, a, b] of cases) {
    const r = await fetchIn(page, '/s/tone.wav', { Range: range });
    expect(r.status, range).toBe(206);
    expect(r.headers['content-range'], range).toBe(`bytes ${a}-${b}/${size}`);
    expect(r.body.length, range).toBe(b - a + 1);
    expect(Buffer.from(r.body).equals(Buffer.from(model.subarray(a, b + 1))), range).toBe(true);
  }
  const full = await fetchIn(page, '/s/tone.wav', { Range: 'bytes=0-0' });
  const etag = full.headers.etag!;
  const same = await fetchIn(page, '/s/tone.wav', { Range: 'bytes=10-19', 'If-Range': etag });
  expect(same.status).toBe(206);
  const other = await fetchIn(page, '/s/tone.wav', { Range: 'bytes=10-19', 'If-Range': '"x"' });
  expect(other.status).toBe(200);
  expect(other.body.length).toBe(size);
  await ctx.close();
});

test('a stopped worker starts again from the vault', async ({ browser }) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  const page = await controlled(ctx);
  await expect(page.locator('#h')).toHaveText('home');
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('ServiceWorker.enable');
  for (let i = 0; i < 3; i++) {
    await cdp.send('ServiceWorker.stopAllWorkers');
    const t0 = Date.now();
    const res = await page.goto(`${w.content}/s/docs/a/b/deep.txt`);
    console.log(`a stopped worker answered in ${Date.now() - t0} ms`);
    expect(res?.status()).toBe(200);
    expect(res?.fromServiceWorker()).toBe(true);
    expect(await res?.text()).toBe('deep');
  }
  await ctx.close();
});

test('an updated worker takes over', async ({ browser }) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  await controlled(ctx);
  // Content pages run no scripts, so messages go from a trusted page the worker controls.
  const page = await ctx.newPage();
  await page.goto(`${w.content}/_zen/boot.html`);
  await page.waitForFunction(() => !!navigator.serviceWorker.controller);
  const build = () =>
    page.evaluate(
      () =>
        new Promise<string>((ok) => {
          const ch = new MessageChannel();
          ch.port1.onmessage = (e) => ok((e.data as { build: string }).build);
          navigator.serviceWorker.controller!.postMessage({ type: 'zen-build' }, [ch.port2]);
        }),
    );
  expect(await build()).toBe('zen-sw-build-1');
  await editWorker(w, 'zen-sw-build-1', 'zen-sw-build-2');
  try {
    await page.evaluate(async () => {
      const reg = (await navigator.serviceWorker.getRegistration())!;
      const changed = new Promise((ok) =>
        navigator.serviceWorker.addEventListener('controllerchange', ok, { once: true }),
      );
      await reg.update();
      await changed;
    });
    expect(await build()).toBe('zen-sw-build-2');
    const site = await ctx.newPage();
    const res = await site.goto(`${w.content}/s/`);
    expect(res?.fromServiceWorker()).toBe(true);
    await expect(site.locator('#h')).toHaveText('home');
    await page.reload();
    await page.waitForFunction(() => !!navigator.serviceWorker.controller);
    expect(await build()).toBe('zen-sw-build-2');
  } finally {
    await editWorker(w, 'zen-sw-build-2', 'zen-sw-build-1');
  }
  await ctx.close();
});

test('offline: what was read is served with the server and the worker stopped', async ({
  browser,
}) => {
  const ctx = await browser.newContext();
  await enroll(ctx);
  const page = await controlled(ctx);
  await expect(page.locator('#h')).toHaveCSS('color', 'rgb(1, 2, 3)');
  // Let the replica write what it fetched.
  await page.waitForTimeout(500);
  await w.server.kill();
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('ServiceWorker.enable');
  await cdp.send('ServiceWorker.stopAllWorkers');
  const res = await page.goto(`${w.content}/s/`);
  expect(res?.status()).toBe(200);
  expect(res?.fromServiceWorker()).toBe(true);
  await expect(page.locator('#h')).toHaveText('home');
  await expect(page.locator('#h')).toHaveCSS('color', 'rgb(1, 2, 3)');
  const never = await page.goto(`${w.content}/s/never.html`);
  expect(never?.status()).toBe(503);
  expect(never?.headers()['zen-reason']).toBe('offline');
  await ctx.close();
});
