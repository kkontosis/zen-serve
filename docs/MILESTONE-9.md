# Milestone 9 plan: serving zen-fs over HTTP through a service worker (`@zen/sw`)

## Context

Milestone 8 is done (PR #75 merged, aa57887). DESIGN-3 §6 item 9 is next. The placeholder (docs/MILESTONE-9.md) asks for:
* **The worker:** a service worker that maps URLs in its scope to fs nodes and answers each fetch with a decrypted, streamed `Response`. It handles `Range` (206), MIME types and index pages, works offline from the replica, and leaves the HTML unchanged. An encrypted website is then viewable by the members who unlocked it.
* **Origin isolation,** so fs content never runs on the origin that holds keys: a separate content origin with its own `zen-serve` config, and `CSP: sandbox` plus `nosniff` on every worker response.
* **The server:** `zen-serve static push` (DESIGN-3 §4.2, a replicated plaintext fs 0) and `Range` in `statics.rs`.
* **The worker as the owner candidate** of TD-DB-SHAREDWORKER.
* **Playwright tests:** offline reads, seeking, the sandbox.

**Moved earlier** (from the placeholder, written during milestone 5 after step 7e):
* **7g:** `readStream`.
* **7h:** the vault, so the worker re-unlocks after the browser kills it.
* **Step 8:** a worker-safe `@zen/fs` with stream reads, the content-type xattr, and sealed chunks at rest, so the worker never caches plaintext.

DESIGN-3 §0 calls the worker "an optional cache only". This milestone makes it a serving path. Code verification stays with M11, which builds on this worker.

**Survey (read-only, this session):**
* **The server** (`crates/zen-server/src/statics.rs`, 188 lines):
  * It serves `/unencrypted/*`, `/`, the aliases, and an SPA fallback, from `unencrypted_dir` or the bundled `crates/zen-server/unencrypted/`. Each file is read whole into memory (`tokio::fs::read`).
  * Headers: ETag (len-mtime or a hash), 304 on `If-None-Match`, `Cache-Control: no-cache`, `nosniff`, `Referrer-Policy: no-referrer`, the configured CSP, and `Service-Worker-Allowed: /` for `sw.js`.
  * **Missing:** `Range`, streaming, `Host` routing, a second origin, fs 0 (only refused: `config.rs:684`, `acl.rs:154`), `unencrypted_source`, and `zen-serve static`.
  * The config is flat with `deny_unknown_fields`, and has `public_origins`, `cors_origins`, `[auth] origin_pinning`/`acl_origins` (sign-in only), `csp` and `cross_origin_isolation`.
  * Tested only by `tests/stream.rs:328` `static_files_aliases_fallback_and_safety`.
* **`@zen/fs`** is worker-safe: no DOM or Node imports, though tsc doesn't enforce it (the `lib` has DOM and no WebWorker). What the worker needs exists:
  * `openReplica(fs, tree, {local, locks, channel, owner, cacheBytes, …})`, with Web Lock owner election
  * `lookup`/`nodeAt`/`stat`/`readdir` by display names
  * `createReadStream(path, {start, end})`, with an inclusive `end`
  * the `zen.content_type` xattr (`CONTENT_TYPE`)
  * sealed chunks and pages at rest, plaintext only in memory
  * Offline, `stat` fails `offline` unless the file's versions are kept, i.e. pinned or read before.
* **`@zen/client`:**
  * `indexedDbLocalStore`, whose device key is a non-extractable `CryptoKey`
  * `sealedStore`
  * `credentialVault` (`signIn` resumes the stored session or signs in with the device, offline too; `unlock(fs, {local})` uses the stored device)
  * `session.approveDevice(devicePublic)`: another session of the user certifies and registers a device key
  * `initWasm` (the wasm-bindgen web glue; no dynamic `import()` is reached in a browser)
* **Apps** keep no vault in the browser (`apps/shared/boot.ts`: password and passphrase, every time). There is no Web Worker, SharedWorker or service worker anywhere. `apps/*/workers` are Node processes.
* **Tests and CI:**
  * Playwright 1.56, Chromium only at `/opt/pw-browsers`, with projects `smoke` and `apps`. Pages load from `http://localhost:PORT` and Node clients use `127.0.0.1`.
  * The CSP refuses inline scripts, so specs copy built files into an `unencrypted_dir`. esbuild bundles apps (`scripts/build-apps.mjs`, entry points `ui/main.ts` and `workers/*.ts`).
  * CI's `client` job runs vitest and then Playwright. A new package needs a vitest alias, a build-apps alias, a workspace, the `check` script and the CI tsc list.

**The open question the design rests on.** Content served by the worker must not reach the worker's keys, which sit in the content origin's IndexedDB (the vault). Two models are possible, and which one works depends on browser behaviour I haven't verified:
* **S (sandboxed):** every response carries `CSP: sandbox allow-scripts …` without `allow-same-origin`, so content documents get an opaque origin: no IndexedDB, no `caches`, no SW registration, no same-origin `/v1`. This works only if a document the worker answered is still controlled by it, so that its subresources (scripts, styles, images, `fetch`, media `Range`) reach the worker. Chromium's handling of opaque-origin clients is the uncertainty.
* **P (fallback):** if opaque documents aren't controlled, content must keep its origin (`allow-same-origin`). Then it can read the vault, so either content gets no scripts (`sandbox` without `allow-scripts`: static sites only), or each site gets an origin of its own holding a device with only that site's rights. The second needs scoped devices (TD-AUTH-TOKEN-SCOPES, M11).

So step 9.1 measures first, as 8.6b did, and the user picks the model before any spec or code.

## Progress and resuming

**Rhythm (as milestone 8):** one sub-step per PR. Before each one:
1. restart from main if the previous PR is merged
2. plan the sub-step in detail here, and commit the plan

On the user's "go": a `spec:` commit if needed, then code and docs. Log the step in both plan copies (this file and `docs/MILESTONE-9.md`, kept equal with `cp`). Push, confirm CI is green on both runs, and pursue every red to its root cause. Stop for compaction, and confirm with the user before implementation begins after one.

**Branch:** `claude/brave-knuth-gk03mm`. Push with retries (2, 4, 8, 16 s). To restart from main, `git fetch origin main && git merge --ff-only origin/main` when the branch holds only merged history; otherwise `git checkout -B … origin/main` and push `--force-with-lease`, which needs the user's word.

**Constraints:**
* no model names in the repo; commit trailers as before
* `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push
* `ZEN_APPS_BUILT=1` for vitest; don't build zen-server while vitest runs
* no data backward compatibility needed (the app isn't deployed)
* RAM is a first-class cost (OPS.md §6): the worker's memory is measured too
* delete only temp dirs this session made, after checking they aren't mounted

**Log:**
* **Step 0** (plan):
  * **First, on approval:**
    1. Restart from main (aa57887).
    2. Bring milestone 8's last two log lines into `docs/MILESTONE-8.md`, after the line starting "* **Done:** BLK.md §8 has the runner's table" (they were only in the old plan file):
       > * Pushed e7d6fef. **CI green on both runs** (push and pull_request), and the dispatch run 42521935996 green in full (`files-bench` and `devices-bench` included). **Step 8.8 is done;** PR #75 waits on review and merge. Open, for the user: why zen-ublk peaks at 373 MiB (TD-MEMORY), and the kanban and booking failures if they recur (their messages now name the cause).
       > * **PR #75 merged** (aa57887). Milestone 8 and its loose ends are done. **Decided (the user):** zen-ublk's 373 MiB stays a TD-MEMORY item for now, not investigated. Next: plan milestone 9 (`@zen/sw`).
    3. Write `docs/MILESTONE-9.md` (this plan, replacing the placeholder, and keeping its "Moved earlier" notes in the Context).
    4. Update DESIGN-3 §6 item 9 (the scope and the link) and the README status ("Milestone 9: in progress").
    5. Commit `docs: milestone 9 plan (@zen/sw)`, push, and open the PR.
  * Then run step 9.1's spike right away, since it changes nothing in the repo, and log its findings for the user's decision.
  * **Done:** restarted from main by fast-forward (aa57887); the two lines are in `docs/MILESTONE-8.md`; this file copied to `docs/MILESTONE-9.md`; DESIGN-3 §6 item 9 and the README updated.
  * Committed c02e92c; PR #76 opened.
* **Step 9.1, the spike** (Chromium 153, Playwright's build here; a Node server for `app.localhost` and `view.localhost`, which Chromium treats as secure contexts; the scratchpad only):

  | Question | Answer |
  |---|---|
  | 1. A document the worker answers with `CSP: sandbox allow-scripts` (opaque origin): are its subresources the worker's? | **No.** Only the navigation reached the worker. Its stylesheet, classic and module scripts, image, iframe and `<audio>` all went to the server, so the page broke (no styles, no image, the server's HTML for the scripts). `navigator.serviceWorker` throws `SecurityError`. |
  | 2. The same with `allow-same-origin` (control) | Every subresource reached the worker: script, module, style, image, `fetch`, iframe (and the iframe's script), audio. |
  | 3. What an opaque document reaches | IndexedDB, `caches`, cookies, `localStorage` and the registration all throw `SecurityError`; a same-origin iframe can't be read; `fetch('/v1/info')` fails (CORS); a `BroadcastChannel` message from it never reached the worker. So the isolation would hold, but (1) makes it unusable. |
  | 4. Media `Range` | Under the worker, `<audio>` sends `bytes=0-`, then `bytes=1310720-` for a seek to 30 s of 60. A 206 built from a stream is accepted and the seek lands (`seeked to 30.0 of 60.0`). |
  | 5. The worker's environment | A module worker with static imports works. wasm-bindgen's glue (`init({module_or_path})`) instantiates zen-wasm in 21–25 ms. **Top-level `await` is refused** (registration fails: "ServiceWorker cannot be started"), so start-up is a promise awaited per fetch. `navigator.locks` and IndexedDB work. A 70 s streamed body (1 KiB a second) arrived whole, with the worker kept alive past the 30 s idle limit and not restarted. |
  | 6. Lifecycle | A fetch to a running worker takes 2 ms. After CDP `ServiceWorker.stopAllWorkers`, the next fetch restarts it in 65 ms, or 97 ms with wasm initialised, and a navigation in 96 ms. **Before the first install, a navigation gets the server's answer**, so on the content origin the server must answer every path itself, sandboxed. |
  | 7. Escapes from an opaque document | Moot after (1). For the model below: no script runs (inline, `onerror`, a `javascript:` link, an SVG's `onload` and `<script>`, with or without `script-src 'none'`), so content can't open blob or data URLs or navigate by script. |

  * **The static model works:** `CSP: sandbox allow-same-origin; script-src 'none'; default-src 'self' data: blob:` keeps the content origin, so the worker controls the page and sees every subresource (the server saw none), with styles, images, iframes and seeking audio. With `script-src 'none'` the browser doesn't even fetch scripts. Without it, it fetches them and doesn't run them. Playwright can still read the page through CDP.
  * **What this means for the models:**
    * **S (scripts in an opaque origin, one content origin)** doesn't work in Chromium.
    * **P-static (one content origin, no scripts)** works now and isolates the vault, since content can't run code on the origin. It serves static sites, documents, images and media, but not sites that need JavaScript.
    * **P-sites (scripts allowed, `allow-same-origin`)** needs an origin per site, and content there can read that origin's vault. It's safe only if the device kept there can do no more than read that one site, which needs per-device scopes on the server (TD-AUTH-TOKEN-SCOPES, M11), plus wildcard DNS and TLS.
  * Committed 8569756 and pushed (PR #76).
  * **Decided (the user):** P-static, no scripts. The alternatives are recorded, with the spike's findings, in a low-priority TD for later study, discussion and possible revival (TD-SW-SCRIPTS, below).
  * **Decided (the user): the worker signs in as a device in the ACL.** A handed-over session gets its own TD (TD-SW-SESSION-HANDOVER, below): how it would help, how it would work, and that it's a possible later feature.
  * **On "go" (the spec):**
    * fa98059 `spec:` new `spec/sw.md` (§1–§9 as planned), indexed in spec/README.md.
    * de60039 `spec:` api.md §10 (ranges, the content origin), a DESIGN-3 §0 note, and TECH_DEBT TD-SW-SCRIPTS (low priority, the four alternatives) and TD-SW-SESSION-HANDOVER.
    * **Changed from the plan:** the hand-over can't simply pass the app's session token. The server checks the origin only at sign-in, so a token would work, but a session's credential id is "the device" for fencing, idempotency and the fs op chain (auth.md §3), and the worker and the app would then share it. The TD describes a delegated session instead: a new endpoint and a `delegated` method, whose credential id is the worker's device fingerprint.
    * **Also in sw.md beyond the plan:** a root mount (`root`), 405 with `Allow`, and `If-None-Match: *`. Bodies error mid-stream rather than send wrong bytes.

## Steps (one PR each; each planned in detail before it starts)

**9.1, the isolation spike and the spec:**
* The spike (below, done), then the user's choice of model (P-static) and of the worker's sign-in (a device in the ACL).
* Then the spec, as "Step 9.1, continued" below.
* Then a `spec:` commit, new `spec/sw.md`:
  * mounts (a scope prefix → fs, tree and base path)
  * URL → path (percent-decoding, refusals), index pages, the trailing-slash redirect, listings (off by default)
  * MIME (`zen.content_type`, else by extension, else `application/octet-stream`)
  * `Range` (a single range: 206, 416, `If-Range`; multiple ranges answered 200 in full)
  * validators (an ETag from node and dot, `If-None-Match`)
  * the headers on every response (the chosen CSP sandbox, `nosniff`, `Referrer-Policy`, CORP, `Cache-Control: no-store`)
  * errors (404, a locked or offline replica 503 with a `zen` reason)
  * the content origin, how its device is enrolled, and leakage
* api.md §10 gains `Range` and the content origin; DESIGN-3 §0 a note.

**9.2, the server:**
* **`statics.rs`:**
  * streamed files rather than read whole
  * single `Range` → 206 with `Content-Range`, 416 with `bytes */len`, `If-Range` against the ETag, and `Accept-Ranges: bytes`
  * HEAD with `Accept-Ranges`
* **The content origin:** a `[content]` table (`origins`, `csp`), routed by `Host`.
  * A request for a content host gets `/_zen/*` from `[content] dir` (the worker with `Service-Worker-Allowed: /`, the bootstrap and enrollment pages, and `config.json`) and `/v1`. Any other navigation gets the bootstrap page, and anything else 503 under sw.md §4's headers (spec/sw.md §1).
  * Nothing from `/unencrypted` is served there.
  * A content origin counts as an accepted sign-in origin.
* **fs 0 and `zen-serve static push DIR`:**
  * `unencrypted_source = "dir" | "replicated"`.
  * A push writes a new generation of files (path → meta and chunks, plaintext), then switches the current generation in one transaction, so every node serves the same set. The old generation is cleared in bounded rounds.
  * The keyspace in spec/keyspace.md; export and import carry it.
* **Tests:** `tests/stream.rs` (Range forms, 416, `If-Range`, HEAD, a large file streamed), content-host routing, push and switch, and a node serving a pushed set. On both backends.

**9.3, `@zen/sw` core** (new `packages/sw`, worker-safe, testable in Node: Node 22 has `Request`, `Response` and web streams):
* `handle(request, mounts, replicas) → Response`, exactly as spec/sw.md says: lookup, index pages, redirects, MIME, Range through `createReadStream`, HEAD, ETag/304, headers and errors.
* tsc with the `WebWorker` lib and no DOM, so worker-safety is checked. The `@zen/fs` core gets the same check.
* **Tests:** vitest over a real server and replica. URL cases (encoding, `..`, conflict names, directories), Range against a model (random ranges over v2 and v3 files, edges, 416), offline from kept content (503 for what isn't kept), headers on every path, and the memory of a large streamed response.

**9.4, the worker in the browser:**
* **The `sw.js` entry** (esbuild, a module worker with its wasm beside it):
  * install and activate (`skipWaiting`, `clients.claim`), updates, and the fetch handler over 9.3
  * start-up on every wake: wasm, the vault's `signIn` and `unlock`, then `openReplica` as owner candidate
  * re-unlocking after the browser kills it
* **Enrollment** (spec/sw.md §8):
  * The enrollment page makes a device key and hands its public part to the app origin (postMessage, origin-checked both ways).
  * There, a signed-in session approves it (`approveDevice`) and adds a device keyslot. An admin also adds it to the ACL; for other users, an admin approves it later.
  * The content origin's vault keeps the device, and the mounts come back the same way.
  * A `@zen/sw` helper does the app side (`enrollViewer`).
* **Playwright tests:**
  * offline reads
  * seeking (a `<video>`, and `fetch` with `Range`)
  * the sandbox: content can't reach IndexedDB, `caches`, the registration, or `/v1` with the worker's session
  * a stopped worker re-unlocks (CDP `ServiceWorker.stopAllWorkers`)
  * an update takes over
  * first visit before the worker is installed
* **TD-DB-SHAREDWORKER:** the serving worker lives on the content origin, so it can't own the app origin's `Db`. The TD gets that note (default below).

**9.5, benches, docs, outcome:**
* **Benches:** throughput through the worker (MiB/s in a page), time to first byte cold (worker stopped: wasm, vault, unlock) and warm, seek latency, and the worker's memory (CDP), next to `readStream`'s.
* **Docs:**
  * a new docs/SW.md guide (deploying a content origin: DNS, TLS for the second host, TD-TLS-ACME)
  * STATS, OPS (pitfalls), CLIENT.md, API.md
  * GAPS G15 (stale: the CSP and Trusted Types are already sent)
  * TECH_DEBT, README, DESIGN-3 §6 item 9 done, and the Outcome

## Step 9.1 in detail: the spike

**Goal:** answer the questions below in Chromium (Playwright's 1243 build), with figures, so the user can choose model S or P. Nothing is committed: the spike lives in the scratchpad, and its findings go in this log.

**Setup** (scratchpad, `spike/`):
* A small Node HTTP server stands in for zen-serve. It answers `app.test` and `view.test`, both mapped to 127.0.0.1 with Chromium's `--host-resolver-rules`, so they are two real origins on one port.
* `view.test` serves `sw.js` and a bootstrap page that registers it. The worker answers `/site/*` from an in-memory map, with headers set per case.
* A Playwright script runs each case in a fresh context, run with the repo's `@playwright/test` from the repo root and the script's path passed as an argument.

**Questions:**
1. **Is an opaque document controlled?** A navigation to `/site/index.html` answered by the worker with `CSP: sandbox allow-scripts` (no `allow-same-origin`): which of its subresource requests reach the worker's `fetch` handler? Script, module script, stylesheet, image, `fetch()`, an iframe navigation, and a `<video>` source. The worker counts them by `clientId` and URL.
2. **The same with `allow-same-origin`.** The expectation is that all are controlled; this is the control case for 1.
3. **What an opaque document can reach:**
   * `indexedDB.open`, `caches`, `navigator.serviceWorker`, `document.cookie`, `localStorage`
   * a `BroadcastChannel` to the worker
   * `fetch('/v1/info')`, and whether it carries anything the worker could be fooled by
   * `window.open`/top navigation
   * `postMessage` to the bootstrap page
4. **Media Range:** does the browser send `Range` to the worker for `<video>` and `<audio>`, accept a 206 the worker builds from a stream, and seek (`currentTime` jumps, with the ranges requested logged)?
5. **The worker's own environment:**
   * a module worker with static imports
   * `WebAssembly.instantiateStreaming` of a wasm beside it
   * top-level `await` (allowed or not)
   * `navigator.locks` and `indexedDB`
   * a `ReadableStream` body that outlives the `fetch` event
   * how long the browser lets a long response run
6. **Lifecycle:** after CDP `ServiceWorker.stopAllWorkers`, the next navigation and the next subresource fetch start the worker again; the time this takes, without wasm and with wasm initialised. Also what a navigation gets before the first install.
7. **Model S's escape routes:** can content in an opaque document make a top-level navigation into a non-sandboxed response on `view.test`? For example a URL the worker doesn't handle, which the server must then answer sandboxed too. A `data:` or `blob:` URL? An iframe with `srcdoc`?

**Outcome, logged here:**
* a table of each question and its answer
* the model it supports, with what each model would cost in the steps after
* the user then decides, and 9.1's `spec:` commit and plan details follow

## Step 9.1, continued: the spec (on "go", same branch and PR #76, which isn't merged yet)

Three commits on PR #76, retitled to cover the plan, the spike and the spec. No code changes.

### 1. `spec:` commit, new `spec/sw.md` ("Serving zen-fs over HTTP through a service worker")

**§1 The content origin:**
* One per deployment, configured on the server (9.2: `[content] origins`, routed by `Host`).
* It serves only fixed, trusted pages under `/_zen/`: the worker, the bootstrap and enrollment pages, and their wasm. It also serves the API at `/v1/*`, as the app origin does.
* Every other path is the worker's. Before the worker is installed (the spike's question 6), the server answers a navigation there with the bootstrap page, which registers the worker and reloads; it answers anything else with 503, sandboxed as in §4.
* The bootstrap and enrollment pages run scripts, under a strict CSP: `script-src 'self'`, Trusted Types, and no input reflected into the page. Content can navigate to them but can't run code in them.

**§2 Mounts** (kept in the content origin's sealed store, set by enrollment):
* `{name, fs, tree, base, index: ["index.html"], listing: false}`.
* `name` is `[a-z0-9-]{1,63}`, and `/<name>/<path>` maps to `base/<path>` in that tree.
* `/` lists nothing (404) unless one mount is marked the root.

**§3 URL → node:**
* The path is split on `/`, and each segment percent-decoded as UTF-8.
* Refused with 400: a segment that is empty (`//` in the middle), `.`, `..`, or holds NUL or `/` after decoding. Invalid UTF-8 or percent-encoding is 400 too.
* Lookup is by display name, so conflict copies are reachable by the names the replica shows.
* **Directories:** a directory without a trailing `/` gets 301 to the path with one, so relative links work. With one, the first index file found is served; failing that, a generated listing if `listing` (names escaped, no scripts); otherwise 404.
* **Symlinks** are 404 in M9.
* **Methods:** only GET and HEAD; others get 405.

**§4 Headers on every response from the worker** (200, 206, 304, 301, 4xx, 5xx alike):
```
Content-Security-Policy: sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-downloads;
  default-src 'self' data: blob:; script-src 'none'; object-src 'none'; style-src 'self' 'unsafe-inline';
  base-uri 'none'; form-action 'none'; frame-ancestors 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cross-Origin-Resource-Policy: same-origin
Cache-Control: no-store
```
* **Why it's safe:** no script runs (spike question 7), so content can't reach the vault even though it keeps the origin.
* **What it allows:** links that open new tabs (popups escape the sandbox, but every response on the origin is sandboxed again) and downloads.
* **What it blocks:** forms, and every load from another origin, which also keeps a page from telling a third party what is being read.
* The policy is fixed in M9. Making it configurable would make it easy to weaken; TD-SW-SCRIPTS discusses that.

**§5 Media types:**
* the `zen.content_type` xattr when it is a valid media type (RFC 9110 `type/subtype` and parameters)
* else by extension, from a table in the spec (html, css, js, json, txt, md, svg, png, jpg, gif, webp, avif, ico, mp4, webm, mp3, ogg, wav, flac, pdf, woff2, wasm, …), with `; charset=utf-8` for text types
* else `application/octet-stream`

**§6 Validators and ranges:**
* **ETag:** strong, `"<hex node>.<hex dot>"` of the version served. A version's bytes never change, so a strong validator is right. `If-None-Match` gives 304.
* **`Range`:**
  * one range only: `bytes=a-b`, `a-` and `-n`, giving 206 with `Content-Range`
  * unsatisfiable gives 416 with `Content-Range: bytes */size`
  * malformed syntax, or several ranges, gets 200 with the whole file (RFC 9110 allows ignoring Range)
  * `If-Range` naming another ETag also gets 200 in full
* `Accept-Ranges: bytes`; HEAD gives the same headers with no body.
* **Which version:** the replica's current one, the newest if there are siblings.

**§7 Errors**, each a short static HTML page under §4's headers:
* 404 (not found, or a symlink)
* 400 and 405 (§3)
* 503 with `Zen-Reason`:
  * `locked`: no device yet, or unlock failed; the page links to enrollment
  * `offline`: not kept and the server unreachable
  * `starting`: the replica isn't loaded
* A version that fails to verify (a bad chunk) is 502 with `Zen-Reason: integrity`. Once a response has started, an error mid-body aborts the stream, so the client sees a truncated body, never wrong bytes.

**§8 Credentials and enrollment** (the user's choice: a device in the ACL):
* **Making the key:** the app opens `/_zen/enroll.html` on the content origin. That page makes a device key, keeps its seed in the content origin's vault (sealed store, device layer), and posts the public key to `window.opener`, with `targetOrigin` the app origin.
* **Which app origins:** the page reads them from `/_zen/config.json`. The server writes it from `public_origins`, minus the content origins.
* **Origin checks:** each side checks `event.origin`. The content side accepts only listed app origins; the app side accepts only its configured content origin.
* **On the app:** a signed-in, unlocked session certifies the key (`approveDevice`) and adds a device keyslot on each mounted fs (`fs.addDevice`). If the user is an admin, it also adds the certificate to the member's ACL entry. Otherwise the enrollment page shows the device's fingerprint for an admin to approve, and the worker serves only kept content until then.
* **Completing:** the app sends back `{cert, identityPublic, mounts}`. The page stores them, registers the worker and closes. It asks for confirmation first, naming the app origin and the mounts.
* **On each wake:** the worker does `vault.signIn` (device), then `vault.unlock(fs, {local})`, then `openReplica`.
* **Revoking:** remove the device's certificate from the ACL, and its keyslot. The device has the member's rights; with no scripts on the origin, nothing but the worker can use it.

**§9 Leakage:** the server sees what any replica shows (which tree, which chunks are fetched, when), plus that this device belongs to the content origin. The page's requests never reach the server: the worker answers them all.

### 2. `spec:` the rest

* **api.md §10:** `Range` on `/unencrypted` (as §6), the content origin (as §1), and a pointer to sw.md.
* **DESIGN-3 §0:** a note that M9 makes the worker a serving path on a separate origin, without scripts.
* **spec/TECH_DEBT.md**, two new entries:
  * **TD-SW-SCRIPTS**, status open, low priority (for later study, discussion and possible revival):
    * **Context:** M9 serves content with no scripts. The spike: a document in an opaque origin (`sandbox allow-scripts`) isn't controlled by the worker in Chromium, so its subresources skip the worker and the page breaks.
    * **The alternatives:**
      1. **Per-site origins with scripts:** an origin per mount (wildcard DNS and TLS), each holding a device scoped to read that one fs. It needs per-device scopes on the server (cf. TD-AUTH-TOKEN-SCOPES).
      2. **Opt-in trusted scripts per mount:** scripts on the content origin, where they could read the vault, so only for content the user wrote.
      3. **A keyless site origin bridged to the app origin:** a site origin with no keys, whose worker gets plaintext from the app origin over a `MessageChannel` scoped by `event.origin`. The caveat is storage partitioning between sites.
      4. **Opaque sandboxing:** revisit if browsers start controlling opaque clients. Firefox and WebKit are untested.
    * **What it would take**, for each.
  * **TD-SW-SESSION-HANDOVER**, status open (a possible later feature):
    * **How it helps:** a member who isn't an admin could set up viewing with no admin step. Today their worker serves only kept content until an admin adds its device.
    * **How it would work:** at enrollment the app mints and passes a session for the content origin, bound to that origin. Sessions bind to an origin at sign-in, so it would need either a sign-in on the content origin through the app, or a new "delegate a session to origin X" endpoint. The worker would keep it in the vault and resume it.
    * **Its limits:** it expires with `session_ttl_secs`, and then the worker needs the hand-over again. Offline reads keep working meanwhile.
    * **What it would take:** the endpoint or flow, the vault's session record (it exists: `saveSession`), and tests.
    * **Combined:** with the ACL device, the device would be preferred and the session used while an admin hasn't approved.

### 3. `docs:` the log

The log in both plan copies; PR #76 retitled `docs, spec: milestone 9 plan, isolation spike and spec/sw.md`.

### Verification

* Links between the spec files resolve (a grep for each anchor named).
* CI green on both runs (docs only).

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`); the touched crates' tests; FDB once for server changes.
* **JS:** `scripts/build-wasm.sh`; `npm run check`; the touched suites 3×, then the full `npx vitest run`; `npx playwright test` when the browser is touched.
* **Negative controls:** each new check (Range bounds, the sandbox header, the origin check in enrollment, the content-host routing, the generation switch) is run once disabled, and its test must fail. The log says so.
* **CI:** green on both runs of each PR, with Playwright in the `client` job.

**Defaults chosen (overridable):**
* **Measure the isolation model first;** no design is committed until the user picks it.
* **One content origin per deployment,** with sites as mounts (path prefixes), and no scripts (decided).
* **The content origin's API** is the same server's `/v1`, routed by `Host`. The worker and enrollment bundle are served from a `[content] dir` (the built `packages/sw/dist`), not bundled into the binary, so `cargo build` doesn't depend on npm.
* **fs 0 and `static push` are in this milestone,** as the placeholder says, in the server step.
* **TD-DB-SHAREDWORKER** stays open, with a note: the serving worker is on the content origin, and an app-origin owner worker is a separate piece.
* **Single ranges only:** a multi-range request gets the whole file (200), which RFC 9110 allows.
* **Directory listings off by default.**

## Risks
* **Sites that need JavaScript don't work in M9** (decided; TD-SW-SCRIPTS).
* **A non-admin's worker** serves only kept content until an admin adds its device (TD-SW-SESSION-HANDOVER).
* **Only Chromium is installed here and on CI,** so Firefox and WebKit behaviour isn't tested. The docs will say so.
* **The worker's memory and wake time** (the wasm, and the replica loading every node at start) could be large for big trees. Measured in 9.5, with TD-MEMORY.
