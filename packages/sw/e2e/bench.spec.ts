// The worker's bench (docs/STATS.md §7.6), opt-in:
//
//   ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve \
//     npx playwright test --project sw packages/sw/e2e/bench.spec.ts
//
// `ZEN_BENCH_GIB` sets the large file's size (default 1). One enrolled
// context reads through the worker: throughput, cold and again, next to
// `Tree.readStream` in Node on the same server; the time to the first byte
// on the first start, from a running worker and from a stopped one; seeks
// by `Range` and by `<audio>`; and the worker's memory, through CDP on its
// target. It checks only that what it read is right, never a time.
//
// Content pages run no scripts, so timing runs inside `page.evaluate`
// (CDP), whose promises resolve there, and media is polled from Node.
import { createHash } from 'node:crypto';
import { type Browser, expect, type Page, test } from '@playwright/test';
import type { FileSource } from '../../client/src/index.js';
import { benchBytes, keystream, mib, prng, quantile, rate } from '../../client/test/benchkit.js';
import { controlledPage, enrollIn } from './pages.js';
import { type SwWorld, startSwWorld, wav } from './world.js';

const ON = !!process.env.ZEN_BENCH;
const N = benchBytes(1);
const SEED = 7;
const WAV_SECONDS = 600;
const RANGE = 64 << 10;

let w: SwWorld;
test.describe.configure({ mode: 'serial' });
test.skip(!ON, 'opt-in: ZEN_BENCH=1');
test.beforeAll(async () => {
  if (!ON) return;
  test.setTimeout(30 * 60_000);
  const t0 = Date.now();
  w = await startSwWorld({
    extra: new Map<string, () => FileSource>([
      ['big.bin', () => keystream(SEED, 0, N)],
      ['long.wav', () => wav(WAV_SECONDS)],
      [
        'long.html',
        () =>
          new TextEncoder().encode(
            '<!doctype html><body><audio id="a" src="long.wav" preload="metadata"></audio></body>',
          ),
      ],
    ]),
  });
  console.log(`world: ${mib(N)} MiB written in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
});
test.afterAll(() => w?.stop());

const sha = (parts: Iterable<Uint8Array>) => {
  const h = createHash('sha256');
  for (const p of parts) h.update(p);
  return h.digest('hex');
};
const ms = (x: number) => x.toFixed(x < 10 ? 1 : 0);
const summary = (xs: number[]) =>
  `median ${ms(quantile(xs, 0.5))} ms, p90 ${ms(quantile(xs, 0.9))}, max ${ms(Math.max(...xs))} (${xs.length})`;
const lines: string[] = [];
const report = (s: string) => {
  lines.push(s);
  console.log(s);
};

/** Read `path` from the page to its end: the bytes, the time to the headers and to the end. */
function readAll(page: Page, path: string) {
  return page.evaluate(async (p) => {
    const t0 = performance.now();
    const r = await fetch(p, { cache: 'no-store' });
    const head = performance.now() - t0;
    const reader = r.body!.getReader();
    let n = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      n += value.length;
    }
    return { status: r.status, n, head, total: performance.now() - t0 };
  }, path);
}

/** One `Range` read from the page: the time to the headers and to the end, and the body's SHA-256. */
function rangeRead(page: Page, path: string, a: number, b: number) {
  return page.evaluate(
    async ([p, range]) => {
      const t0 = performance.now();
      const r = await fetch(p, { cache: 'no-store', headers: { Range: range } });
      const head = performance.now() - t0;
      const body = await r.arrayBuffer();
      const total = performance.now() - t0;
      const d = new Uint8Array(await crypto.subtle.digest('SHA-256', body));
      return {
        status: r.status,
        head,
        total,
        sha: Array.from(d, (x) => x.toString(16).padStart(2, '0')).join(''),
      };
    },
    [path, `bytes=${a}-${b}`] as const,
  );
}

/**
 * The worker's target over CDP: Playwright's sessions take a page or a
 * frame, so the browser's session attaches to it and relays messages.
 */
async function workerTarget(browser: Browser, origin: string) {
  const b = await browser.newBrowserCDPSession();
  const { targetInfos } = await b.send('Target.getTargets');
  const t = targetInfos.find((x) => x.type === 'service_worker' && x.url.startsWith(origin));
  if (!t) throw new Error('no service worker target');
  const { sessionId } = await b.send('Target.attachToTarget', {
    targetId: t.targetId,
    flatten: false,
  });
  let id = 0;
  const waiting = new Map<number, (r: { result?: unknown; error?: unknown }) => void>();
  b.on('Target.receivedMessageFromTarget', (e) => {
    if (e.sessionId !== sessionId) return;
    const m = JSON.parse(e.message) as { id?: number; result?: unknown; error?: unknown };
    if (m.id !== undefined) waiting.get(m.id)?.(m);
  });
  const send = async <T>(method: string, params: object = {}): Promise<T> => {
    const mine = ++id;
    const reply = new Promise<{ result?: unknown; error?: unknown }>((ok) => waiting.set(mine, ok));
    await b.send('Target.sendMessageToTarget', {
      sessionId,
      message: JSON.stringify({ id: mine, method, params }),
    });
    const r = await reply;
    waiting.delete(mine);
    if (r.error) throw new Error(`${method}: ${JSON.stringify(r.error)}`);
    return r.result as T;
  };
  return { send };
}

interface HeapUsage {
  usedSize: number;
  totalSize: number;
  embedderHeapUsedSize?: number;
  backingStorageSize?: number;
}

/** The worker's memory: its JS heap plus array buffers' backing stores. */
const used = (h: HeapUsage) => h.usedSize + (h.backingStorageSize ?? 0);

/** Sample `send` every `everyMs` until `stop`, after a collection when `gc`; the peak over `base`. */
function sampler(
  send: <T>(m: string, p?: object) => Promise<T>,
  base: number,
  everyMs: number,
  gc: boolean,
) {
  let peak = 0;
  let stopped = false;
  const done = (async () => {
    while (!stopped) {
      if (gc) await send('HeapProfiler.collectGarbage');
      const h = await send<HeapUsage>('Runtime.getHeapUsage');
      peak = Math.max(peak, used(h) - base);
      await new Promise((ok) => setTimeout(ok, everyMs));
    }
  })();
  return {
    async stop() {
      stopped = true;
      await done;
      return peak;
    },
  };
}

test('the worker, measured', async ({ browser }) => {
  test.setTimeout(60 * 60_000);
  const ctx = await browser.newContext();
  await enrollIn(w, ctx);
  report(`## The worker, ${mib(N)} MiB (Chromium ${browser.version()})`);

  // The first start after enrollment: an empty replica catches up with the server.
  const first = await ctx.newPage();
  let t0 = Date.now();
  const r0 = await first.goto(`${w.content}/s/docs/a/b/deep.txt`);
  const firstMs = Date.now() - t0;
  expect(r0?.status()).toBe(200);
  expect(r0?.fromServiceWorker()).toBe(true);
  report(`first start after enrollment: ${firstMs} ms (a navigation)`);

  const page = await controlledPage(w, ctx);
  const cdp = await ctx.newCDPSession(page);
  await cdp.send('ServiceWorker.enable');

  // From a running worker: small fetches.
  const warmHead: number[] = [];
  for (let i = 0; i < 20; i++) {
    const r = await rangeRead(page, '/s/docs/a/b/deep.txt', 0, 3);
    expect(r.status).toBe(206);
    warmHead.push(r.head);
  }
  report(`a running worker, a small file: to the headers ${summary(warmHead)}`);

  // From a stopped worker: wasm from the cache, the session resumed, the fs unlocked, the replica loaded.
  const cold: number[] = [];
  for (let i = 0; i < 10; i++) {
    await cdp.send('ServiceWorker.stopAllWorkers');
    t0 = Date.now();
    const res = await page.goto(`${w.content}/s/docs/a/b/deep.txt`);
    cold.push(Date.now() - t0);
    expect(res?.status()).toBe(200);
    expect(res?.fromServiceWorker()).toBe(true);
    expect(await res?.text()).toBe('deep');
  }
  report(`a stopped worker, a navigation: ${summary(cold)}`);

  // The large file, read whole: cold (not kept), then again.
  await page.goto(`${w.content}/s/`);
  const target = await workerTarget(browser, w.content);
  await target.send('HeapProfiler.enable');
  await target.send('HeapProfiler.collectGarbage');
  const h0 = await target.send<HeapUsage>('Runtime.getHeapUsage');
  report(
    `the worker at rest: heap ${mib(h0.usedSize)} MiB used of ${mib(h0.totalSize)}` +
      (h0.backingStorageSize !== undefined
        ? `, array buffers ${mib(h0.backingStorageSize)} MiB`
        : ', array buffers not reported'),
  );
  const base = used(h0);

  const sCold = sampler(target.send, base, 100, false);
  const c1 = await readAll(page, '/s/big.bin');
  const peakCold = await sCold.stop();
  expect(c1.status).toBe(200);
  expect(c1.n).toBe(N);
  report(
    `read whole, cold: ${rate(N, c1.total)} MiB/s, the headers in ${ms(c1.head)} ms; ` +
      `the worker's heap and buffers at most +${mib(peakCold)} MiB (sampled every 100 ms, garbage included)`,
  );

  const sLive = sampler(target.send, base, 250, true);
  const c2 = await readAll(page, '/s/big.bin');
  const peakLive = await sLive.stop();
  expect(c2.n).toBe(N);
  report(
    `read whole, again (collected every 250 ms, which slows it): ${rate(N, c2.total)} MiB/s; ` +
      `live at most +${mib(peakLive)} MiB`,
  );
  const c3 = await readAll(page, '/s/big.bin');
  expect(c3.n).toBe(N);
  report(`read whole, a third time: ${rate(N, c3.total)} MiB/s`);

  // The same file through `Tree.readStream` in Node, at the default read-ahead.
  const ufs = await w.unlock();
  try {
    const t = ufs.tree(Uint8Array.from(Buffer.from(w.tree, 'hex')));
    t0 = performance.now();
    let n = 0;
    const reader = t.readStream(w.nodes.get('big.bin')!).getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      n += value.length;
    }
    expect(n).toBe(N);
    report(`Tree.readStream in Node, the same file: ${rate(N, performance.now() - t0)} MiB/s`);
  } finally {
    ufs.close();
  }

  // Seeks by `Range`: random 64 KiB reads, checked against the source.
  const rnd = prng(42);
  const spans = Array.from({ length: 50 }, () => {
    const a = Math.floor(rnd() * (N - RANGE));
    return [a, a + RANGE - 1] as const;
  });
  for (const pass of ['cold', 'again']) {
    const head: number[] = [];
    const total: number[] = [];
    for (const [a, b] of spans) {
      const r = await rangeRead(page, '/s/big.bin', a, b);
      expect(r.status).toBe(206);
      expect(r.sha).toBe(sha(keystream(SEED, a, b + 1)));
      head.push(r.head);
      total.push(r.total);
    }
    report(`64 KiB ranges, ${pass}: to the headers ${summary(head)}; to the end ${summary(total)}`);
  }

  // `<audio>` seeks: from setting `currentTime` until `seeking` is false (polled from Node).
  const media = await ctx.newPage();
  await media.goto(`${w.content}/s/long.html`);
  const audio = media.locator('#a');
  await expect.poll(() => audio.evaluate((a: HTMLAudioElement) => a.readyState)).toBeGreaterThan(0);
  expect(await audio.evaluate((a: HTMLAudioElement) => a.duration)).toBeCloseTo(WAV_SECONDS, 0);
  const seeks: number[] = [];
  for (let i = 0; i < 10; i++) {
    const to = Math.floor(rnd() * (WAV_SECONDS - 10));
    t0 = performance.now();
    await audio.evaluate((a: HTMLAudioElement, x) => {
      a.currentTime = x;
    }, to);
    while (await audio.evaluate((a: HTMLAudioElement) => a.seeking)) {
      if (performance.now() - t0 > 30_000) throw new Error('the seek never landed');
    }
    seeks.push(performance.now() - t0);
    expect(await audio.evaluate((a: HTMLAudioElement) => a.currentTime)).toBeCloseTo(to, 0);
  }
  report(
    `<audio> seeks in a ${WAV_SECONDS} s WAV: ${summary(seeks)} (polled, a few ms of resolution)`,
  );

  await ctx.close();
  console.log(`\n${lines.join('\n')}\n`);
});
