// The service worker of the content origin (spec/sw.md §1, §8): it answers
// every request of the origin but `/_zen/*` and `/v1/*` from the mounts'
// replicas. Built by scripts/build-sw.mjs into /_zen/sw.js, a module worker
// with the wasm beside it. No top-level await: a worker that has one isn't
// started, so the start is a promise each request awaits (`startSite`).
import { indexedDbLocalStore } from '@zen/client';
import { handle, startSite } from '../src/index.js';

declare const self: ServiceWorkerGlobalScope;

/** Changes with each build that should replace a running worker (the update test). */
const BUILD = 'zen-sw-build-1';
/** The IndexedDB database the pages and the worker share (sw.md §8). */
const DB = 'zen-sw';

/** The worker's own files, kept at install so it starts offline (sw.md §1). */
const CACHE = `zen-sw-${BUILD}`;
const WASM = new URL('zen_wasm_bg.wasm', self.location.href).href;

const viewer = startSite({
  origin: self.location.origin,
  local: indexedDbLocalStore(DB),
  wasm: async () => (await caches.match(WASM, { cacheName: CACHE })) ?? WASM,
  onError: (e, req) => console.error(`zen-sw: 500 for ${req.url}:`, e),
});

self.addEventListener('install', (e) => {
  e.waitUntil(
    (async () => {
      await (await caches.open(CACHE)).add(new Request(WASM, { cache: 'no-store' }));
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    (async () => {
      for (const k of await caches.keys()) {
        if (k.startsWith('zen-sw-') && k !== CACHE) await caches.delete(k);
      }
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', (e) => {
  const d = e.data as { type?: unknown } | null;
  if (!(e.source instanceof Client) || new URL(e.source.url).origin !== self.location.origin)
    return;
  if (d?.type === 'zen-restart') e.waitUntil(viewer.restart());
  else if (d?.type === 'zen-build') e.ports[0]?.postMessage({ build: BUILD });
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/_zen/') || url.pathname.startsWith('/v1/')) return;
  e.respondWith(viewer.site().then((site) => handle(e.request, site)));
});
