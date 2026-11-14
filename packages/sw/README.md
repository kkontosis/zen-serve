# @zen/sw

Serving zen-fs over HTTP from a service worker on a **content origin** (spec/sw.md). Members who unlocked a filesystem can view a static website, documents or media kept in it, in a browser and offline from the worker's replica. The pages run no scripts, so content can't reach the keys the worker holds.

**The core (milestone 9 step 3):** `handle(request, site) → Response`. It covers spec/sw.md §2–§7 over `@zen/fs` replicas:
* mounts and routing
* index files, redirects and listings
* media types
* `ETag`, `If-None-Match`, `Range` and `If-Range`
* the headers every response carries (`SANDBOX_CSP`)
* the error mapping

It is worker-safe (`tsconfig.worker.json` checks it with the WebWorker lib and no DOM) and tested in Node against a real server.

```ts
import { handle, parseMounts, type Site } from '@zen/sw';

const site: Site = {
  mounts: parseMounts([{ name: 'docs', fs: 1, tree, base: '/site', listing: true }]),
  replica: async (m) => replicaFor(m), // rejects with code `locked` or `starting` while not ready
};
const res = await handle(request, site);
```

**The worker in the browser (step 9.4):**
* `startSite({origin, local})`: the worker's start (sw.md §8). Once per worker instance it loads the wasm, signs in with the device kept in the content origin's vault, and unlocks each mounted fs; each mounted tree's replica opens on its first request and catches up before it serves. `viewer.site()` gives the `Site` for one request: while the start fails or runs past 15 s, every path answers `locked`, `offline` or `starting`.
* `enrollPage` (the content origin) and `enrollViewer` (the app): enrollment, the conversation of sw.md §8. Nothing changes on the server until the user confirms on the content origin.
* `web/`: the deployable files: the module worker `sw.ts`, the bootstrap page and the enrollment page.

**Building the content origin's files:**

```sh
scripts/build-wasm.sh
node scripts/build-sw.mjs            # → packages/sw/content/ (or --out DIR)
```

The directory holds `sw.js`, `boot.html`/`boot.js`, `enroll.html`/`enroll.js`, `pages.css` and `zen_wasm_bg.wasm`. Point the server's `[content] dir` at it (or a copy), with `[content] origins` and `public_origins` set (api.md §10).

**The app side**, with the member's signing identity (TD-SW-IDENTITY) and, to add the device to the ACL, an admin's:

```ts
import { enrollViewer } from '@zen/sw';

const { fp, inAcl } = await enrollViewer({
  session, identity, admin: identity, fs: [ufs],
  mounts: [{ name: 'docs', fs: 1, tree, base: '/site' }],
  contentOrigin: 'https://view.example.com',
});
```

**Tests:** `npx vitest run packages/sw` (Node: the core, the start and enrollment); `npx playwright test --project sw` (Chromium: the first visit, enrollment, the headers, the sandbox, seeking, a stopped worker, an update, offline).

**Next (step 9.5):** benches through the worker, and docs/SW.md, the guide to deploying a content origin.
