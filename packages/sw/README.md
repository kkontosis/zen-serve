# @zen/sw

Serving zen-fs over HTTP from a service worker on a **content origin** (spec/sw.md). Members who unlocked a filesystem can view a static website, documents or media kept in it, in a browser and offline from the worker's replica. The pages run no scripts, so content can't reach the keys the worker holds.

**What's here (milestone 9 step 3):** the core, `handle(request, site) → Response`. It covers spec/sw.md §2–§7 over `@zen/fs` replicas:
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

**Next (step 9.4):**
* the `sw.js` entry: start-up, the vault and the replica
* enrollment
* Playwright tests in the browser

Deploying a content origin is docs/SW.md, written in step 9.5.
