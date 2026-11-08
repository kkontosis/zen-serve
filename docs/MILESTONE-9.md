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

## Steps (one PR each; each planned in detail before it starts)

**9.1, the isolation spike and the spec:**
* The spike (below), then the user's choice of model.
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
  * A request for a content host gets the bundled worker bootstrap (`/sw.js` with `Service-Worker-Allowed`, and a bootstrap page for every other navigation, itself sandboxed as the model says) plus `/v1`.
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
* **Enrollment:** a bootstrap page on the content origin makes a device key and hands its public part to the app origin (postMessage, with an origin check), where a signed-in session approves it (`approveDevice`) and adds a device keyslot. The content origin's vault keeps the device. Mounts are configured the same way.
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

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`); the touched crates' tests; FDB once for server changes.
* **JS:** `scripts/build-wasm.sh`; `npm run check`; the touched suites 3×, then the full `npx vitest run`; `npx playwright test` when the browser is touched.
* **Negative controls:** each new check (Range bounds, the sandbox header, the origin check in enrollment, the content-host routing, the generation switch) is run once disabled, and its test must fail. The log says so.
* **CI:** green on both runs of each PR, with Playwright in the `client` job.

**Defaults chosen (overridable):**
* **Measure the isolation model first;** no design is committed until the user picks it.
* **One content origin per deployment** under model S, with sites as mounts (path prefixes). Per-site origins only if model P is chosen.
* **fs 0 and `static push` are in this milestone,** as the placeholder says, in the server step.
* **TD-DB-SHAREDWORKER** stays open, with a note: the serving worker is on the content origin, and an app-origin owner worker is a separate piece.
* **Single ranges only:** a multi-range request gets the whole file (200), which RFC 9110 allows.
* **Directory listings off by default.**

## Risks
* **The isolation model** may force a static-only or per-site design (model P), which changes 9.2–9.4. Hence the spike first.
* **Only Chromium is installed here and on CI,** so Firefox and WebKit behaviour isn't tested. The docs will say so.
* **The worker's memory and wake time** (the wasm, and the replica loading every node at start) could be large for big trees. Measured in 9.5, with TD-MEMORY.
