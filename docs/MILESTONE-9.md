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
    * fecdd1b (the log) pushed, and PR #76 retitled.
    * **CI on fecdd1b:** one run's `pure-rust` job failed `zen-server` `tests/svc.rs` `push_input` (1.160 s against its 1 s bound); the other run's `pure-rust` passed on the same commit, and every other job was green.
      * **It isn't this PR's:** the PR changes only docs, and this is TD-TEST-OPEN-FAILURES item 2.
      * **New:** it was the first push after the claim (the other nine took 7–61 ms), and no storage warning was printed, so it wasn't the reader's 1 s re-check.
      * **Not reproduced here:** 15 idle runs, 12 under CPU load and 30 under disk load all passed the bound (worst 165 ms). Under the disk load, `leases_and_takeover` failed 2 of 30 times (a 300 ms lease expired before the claim), which CI hasn't shown.
      * ba517e5 adds all this to the TD, and one PR comment says it.
    * **CI green on both runs of ba517e5** (push and pull_request), every job. PR #76 waits on review and merge.
    * **PR #76 merged** (the user). Step 9.1 is done.
* **Step 9.2** (the server): planned below ("Step 9.2 in detail").
  * On "go": restarted from main (a85090a) by fast-forward; this file copied to `docs/MILESTONE-9.md`; commit `docs: milestone 9 step 2 plan`; push; open the PR. Then implement on the next "go".
    * **Decided (the user):** add timing of the first push's path (the append's commit, the lease watch firing, the push loop's turn) to `push_input`'s failure message, in step 9.2, as its own commit, checked by a forced failure. PR #76 stays docs only.
  * Committed 085eeb1 and pushed; PR #77 opened. **CI green on both runs of 085eeb1** (push and pull_request), every job. Next, on "go": implement as "Step 9.2 in detail" below, starting by copying this file to `docs/MILESTONE-9.md` (this log line included).
  * 8739728 (the log line above) pushed.
  * **On "go" (the implementation):**
    * c48a522 `spec:` keyspace.md §3.11 (`sg`, `sf`, `sc`), api.md §2 (`content_origins`) and §10 (the sources and their ETags, the content origin's routes), operations.md §9 (`zen-serve static push` and `list`), sw.md §1 (`[content] origins` and `dir`).
    * 8b588b4 `server:` as planned:
      * `statics.rs`: streamed sources (bundled, a directory in 64 KiB pieces, the store in batches of 16 chunks), `parse_range`, `If-Range`, `Accept-Ranges` and `Content-Length` everywhere, HEAD, and the content host.
      * `fs0.rs`: push, list, open and the chunk reads.
      * `[content]` and `unencrypted_source` in the config, the content origins in `origin::explicit` and `/v1/info`, and `zen-serve static push|list`.
    * **Changed from the plan:**
      * The content host also mirrors the app host's answers for an unmatched `/v1` path (404, or 405 for other methods). Its other non-GET requests get the sandboxed 503 `starting`, as the spec says for "anything else".
      * A path below the pushed directory that `/unencrypted` can't serve (not UTF-8, `\`) refuses the push rather than being skipped. Symlinks and special files are skipped with a warning.
      * The bootstrap page missing from `[content] dir` gives the 503 `starting` (and a warning in the log), not 404.
      * operations.md and OPS.md say: one push at a time.
    * **Tests:** `tests/statics.rs`, 8 tests, and the config refusals (a unit test).
      * Ranges on all three sources: every form, 416, an empty file, `If-Range`, HEAD, 304. A 5 MiB file streams whole and by a range across 80 chunks.
      * Pushes: generations A to E, the previous one kept and older ones cleared, an interrupted push and its leftover, symlinks, an empty directory, `list`.
      * A generation cleared mid-body errors the body.
      * Export and import keep the set.
      * The content origin's routes, and its sign-in.
      * The whole zen-server suite passes; clippy is clean (default, `--no-default-features`, `--all-features`); fmt is clean. FDB is left to CI's `fdb` job, which runs these tests.
    * **Negative controls,** each run once, then restored:
      * `parse_range` always `Full`: `ranges_on_a_directory` fails.
      * `If-Range` ignored: it fails at the `If-Range` check.
      * The content-host check off: `the_content_origin_routes` fails.
      * An interrupted push that had switched: `pushes_switch_generations_whole` serves "D", not "C".
      * The previous generation cleared too: "the previous set is kept" fails.
      * The content origins left out of `explicit`: `the_content_origin_signs_in` fails.
      * The first try at the fourth control moved the switch before the files without removing the later one. It failed on "previous kept" instead, so it was redone as above.
    * 4f20e27 `test(server):` push_input's diagnostics.
      * A recorder, `svc::testing::trace`, under `test-utils`. It is off until a test starts it, and keeps the newest 100,000 events.
      * It records the reader's read, gather and append commit, and the push loop's turn start, watches, log read, wake-up (which bell), connection read and frames sent.
      * The failure message lists the slowest push's steps from the test's write.
      * **Forced failure:** a 1.2 s delay after one lease bell showed as `7.2 ms lease bell fired` then `1208.2 ms turn start`. A delay on every bell first let the test's lease expire (`not_leader`), so the check used a single one. TD-TEST-OPEN-FAILURES item 2 now says the message names the step.
    * The `docs:` commit: docs/API.md (Static), examples/zen-serve.toml (`unencrypted_source` and `[content]`, commented), a new spec/OPS.md §8 (static-file and content-origin pitfalls), and this log.
    * d0c4d00 (the `docs:` commit) pushed; PR #77 retitled `server: milestone 9 step 2: static ranges, the content origin, static push`.
    * **CI on 8739728 (the log line, docs only): the pull-request run's `pure-rust` failed,** in zen-blk `a_crash_loses_only_unflushed_writes`. Its second flush got `lease_lost` under the test's 1.5 s lease. The push run on the same commit passed every job.
      * **It isn't this PR's:** that commit is main plus one log line.
      * **Not reproduced here** (`--no-default-features`): 5 idle runs and 6 under CPU load, all passed.
      * One PR comment says so. TD-TEST-OPEN-FAILURES has a new item 7 for it, with the next step: time the renewals in its failure message.
    * **CI green on both runs of d0c4d00** (push and pull_request), every job, `fdb` included (so the new static tests pass on FoundationDB). **Step 9.2 is done;** PR #77 waits on review and merge.
    * 64d9e58 (this CI log and TD item 7) pushed: **CI green on both runs**, every job; PR #77 mergeable. This line reaches `docs/MILESTONE-9.md` at the restart from main.
    * **Next, after PR #77 merges:** restart from main, plan step 9.3 (`@zen/sw` core: `handle(request, mounts, replicas) → Response` as spec/sw.md §2–§7, in a new worker-safe `packages/sw`, tested in Node) in detail here, commit the plan, open the PR, and implement on "go".
    * **PR #77 merged** (the user). Step 9.2 is done. The PR's subscription ended with it.
* **Step 9.3** (`@zen/sw` core): planned below ("Step 9.3 in detail").
  * **On approval:**
    1. Restart from main by fast-forward.
    2. Copy this file to `docs/MILESTONE-9.md`, which brings in the log lines since 64d9e58.
    3. Commit `docs: milestone 9 step 3 plan (@zen/sw core)`, push, open the PR, and subscribe to it.
    4. Then implement on the next "go".
  * Committed ecfed87 and pushed; PR #78 opened and subscribed.
  * **On "go" (the implementation):**
    * 53cb78b `spec:` sw.md as planned:
      * §3: mounts and routing, and what a listing holds
      * §4: the policy's text is the reference
      * §6: no ETag for a local version (or a file with no version), and the body read by the version the ETag names
      * §7: the error mapping, with anything else 500
    * aca919d `fs:`:
      * `Replica.streamOf(e, v, {start, end})`, which `createReadStream` now calls, and `nodeType` exported
      * a test: the version given is read, from what was kept, offline, after a newer one lands
      * `packages/fs/tsconfig.worker.json` (WebWorker lib, no DOM or Node types, through `@zen/client`'s `.d.ts`), in `npm run check` and CI. Checked by planting `document` and then `process` in an fs source: each fails it.
      * **Changed from the plan:** the fs worker check went in this commit rather than the `sw:` one.
    * 55fa162 `spec:` two cases the code settled: a file's path with a trailing `/` is 404, and listings leave out symlinks.
    * 0289ded `sw:` the new `packages/sw`, as planned (mounts, range, mime, headers, pages, handle), wired into the workspaces, `check`, vitest, build-apps and CI.
    * **Changed from the plan:**
      * **The body is read before the response is made.** The first piece is read first, so an error there (offline, a chunk that fails to open) becomes the status, not a truncated 200. After that the body is pulled only as it is read (`highWaterMark` 0): at most one piece ahead of the reader.
      * **`If-None-Match: *`** matches any file, with or without an ETag.
      * **416** carries a short error page, as §7's errors do.
      * `Site.onError` is told of each 500.
    * 9c2e907 `test(server):` `SANDBOX_CSP` against sw.md §4's text, as `@zen/sw`'s constant is.
    * **Tests:** 12 unit tests, 14 in handle.test.ts and 1 memory test.
      * The duplicate-name fixture first assumed which of two `same.txt` nodes is shown as `same (2).txt`. fs.md §8 orders them by node id, which is random, so the first full run passed by chance; the test now reads both names from the replica.
      * A real tampered chunk (a plaintext local store, the chunk's last byte flipped, the server unreachable) gives 502 `integrity`. It surfaces as a `ZenCryptoError`, as the "crypto errors not mapped" control shows.
    * **Memory, changed from the plan:**
      * The planned bound, a 64 MiB file raising live memory by less than 32 MiB, failed: 34–63 MiB through `handle` over five runs. A direct `createReadStream` gave 22–45 MiB on the same file, and the client's `Tree.readStream` alone 19–39 MiB.
      * Before `highWaterMark` 0, `handle` added about 12 MiB over a direct read. With it, `handle` adds at most one piece, which the new test "the body is pulled" checks exactly (it counts the pulls).
      * The replica's read peak is flat with the size: 59 MiB at 128 MiB, 71–73 at 256 and 68 at 512, so nothing accumulates. But it is 5–6 times the read-ahead.
      * So `memory.test.ts` streams 256 MiB and bounds the peak at half the file, which shows it is never held whole. The replica's figure is a new TD-MEMORY item, with candidates for where it goes; step 9.5 measures the worker in the browser.
    * **Negative controls,** each run once, then restored; each made its test fail:
      * §4's headers left off error responses (8 tests failed)
      * `If-Range` ignored
      * a `..` segment allowed (the mounts and route refusals)
      * listing names not escaped
      * the body read by path rather than `streamOf` (the mid-request test)
      * `parseRange` always the whole file
      * the body pulled eagerly (`highWaterMark` 4)
      * unexpected errors as 404
      * `ZenCryptoError` not mapped (the real tampered chunk)
      * `SANDBOX_CSP` drifted, in TypeScript and then in Rust
    * **Checks:**
      * `npm run check` passes, both worker checks included.
      * `npx vitest run packages/sw packages/fs/test/replica.test.ts` passed 3 times.
      * The server's `statics` unit tests pass; clippy (`--all-targets -D warnings`) and fmt are clean.
    * The `docs:` commit: packages/sw/README.md, a docs/CLIENT.md line on `streamOf`, the TD-MEMORY item, and this log.

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
* **`push_input`'s diagnostics** (TD-TEST-OPEN-FAILURES item 2, the user's choice), its own commit `test(server): push_input times the first push's path`:
  * On failure the message names, for the slow push, when the append committed, when the lease bell's watch fired, and when the push loop's turn read the input. The server gets `test-utils` timestamps if one is needed.
  * Checked by a forced failure (a delay injected at one step), which must name that step. Then removed.

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

## Step 9.2 in detail: the server

Step 9.2 adds to zen-serve: streamed static files with single ranges, the content origin (spec/sw.md §1), the replicated static source (fs 0) with `zen-serve static push`, and `push_input`'s diagnostics. There's no client change.

**Survey:**
* **`crates/zen-server/src/statics.rs`** (188 lines):
  * `load(st, rel)` reads a whole file, from `BUNDLED` (`include_dir!`) or `unencrypted_dir`.
  * `serve(st, rel, headers, head)` sets the type (`mime_guess`), ETag, 304, `no-cache`, `nosniff`, `no-referrer`, the CSP and `Service-Worker-Allowed`.
  * `fallback` routes `/unencrypted/*`, `/`, the aliases and the SPA fallback; GET and HEAD only.
  * Mounted as the router's `.fallback` (`lib.rs:328`), so `/v1/*` routes match on any `Host` already.
* **Origins** (`origin.rs`): the accepted set is `explicit(cfg, acl_origins, pins)`, that is `public_origins`, the pins when pinning is active, and the ACL's origins.
* **Config:** flat, `deny_unknown_fields`.
* **`/v1/info`** (`lib.rs:69`): `Info` in zen-proto; new fields take `#[serde(default)]`.
* **Dump** exports every key but `meta` and the relay prefixes (`dump.rs` `private`), so static keys are data with no change.
* **The `s*` key tags** are free; only `sess` is taken.
* **No `tokio-util`:** bodies are streamed with `futures::stream` (already a dependency).
* **The CLI** (`main.rs`) opens the store with `open_store(&cfg)`, as `export` and `import` do.
* **Tests:**
  * The test binaries build `zen-server` with `test-utils`, so `#[cfg(feature = "test-utils")]` hooks reach every test, as `svc::push::testing` does.
  * `Harness::sign_in_at(user, origin, host)` signs any origin.
  * The tests run on FDB with `ZEN_TEST_FDB`.

### 1. `spec:` commit

* **keyspace.md, new §3.11 "Static files (fs 0)":**
  * `pack("sg")` → `u64` current generation
  * `pack("sf", gen, path)` → `u8 1 ‖ u64 size ‖ sha256(32)`
  * `pack("sc", gen, path, u32 i)` → chunk `i`, 65,536 bytes each but the last
  * Data (exported).
* **api.md:**
  * §2: `info.content_origins: [text]` (absent when none).
  * §10:
    * the replicated source (`unencrypted_source = "replicated"`, served from the current generation, ETag `"s<hex sha256[0..16]>"`)
    * a generation is replaced whole, and the one before it is kept until the next push, so downloads in flight finish
    * `/_zen/config.json` (`{app_origins}`)
* **operations.md:** `zen-serve static push -c CONFIG DIR` and `zen-serve static list -c CONFIG`.
  * **What it pushes:** regular files, symlinks skipped with a warning, refused if the directory holds none, and paths checked as `statics.rs` `clean` checks them.
  * **Embedded backend:** the store must be closed, so stop the server, as for `import`. On FDB it runs live from any machine with the cluster file.
  * **What it prints:** files, bytes, and the generation.
* **sw.md §1:** `[content] origins` and `dir` named exactly.

### 2. Code, `server: …` commit

* **Config** (`config.rs`):
  * `unencrypted_source: "dir" | "replicated"` (default `dir`; `replicated` with `unencrypted_dir` set is refused).
  * `[content] { origins: [text], dir: path }` (absent by default). Checked:
    * each origin valid
    * none in `public_origins`
    * `public_origins` not empty, since they are the app origins enrollment trusts
    * `dir` set
* **`statics.rs`, a source abstraction:**
  * `Source::{Bundled, Dir(root), Replicated}` → `open(rel) → Option<File {len, etag, body(range) → Body}>`.
  * Dir files are read in 64 KiB pieces from a seek. Replicated chunks are read in batches of 16 per snapshot read. Bundled files are a slice.
  * A chunk missing mid-stream (its generation cleared) errors the body.
* **`statics.rs`, ranges:**
  * `parse_range(header, len) → Full | Part(a, b) | Unsatisfiable`, as sw.md §6: one range only; `a-b`, `a-`, `-n`; `b` clamped; `-n` past the size means the whole file; several ranges or malformed ones mean the whole file.
  * `If-Range` applies the range only when it equals the ETag.
  * 206 with `Content-Range`, 416 with `bytes */len`, `Accept-Ranges: bytes` and `Content-Length` on every file response, and HEAD with the same headers.
* **`statics.rs`, the content host:** a request whose `Host` is a content origin's host (and port), checked first in `fallback`:
  * `/_zen/config.json`: `{app_origins: public_origins}`, `no-store`
  * `/_zen/<rel>`: from `[content] dir`, with the server's `csp` (the trusted pages' policy), and `Service-Worker-Allowed: /` for `/_zen/sw.js`
  * any other navigation (`Sec-Fetch-Mode: navigate`, or `Accept` with `text/html` when that header is absent): `dir/boot.html`, with the same trusted headers and `no-store`
  * anything else: 503 `Zen-Reason: starting`, a short page under sw.md §4's headers (`SANDBOX_CSP` and the rest, one constant)
  * `/unencrypted/*` and the aliases are not served there
* **`origin.rs`:** content origins join `explicit`, so a device sign-in signing one is accepted.
* **`/v1/info`:** `content_origins`.
* **`static push` and `list`:** `statics::push(store, dir) → PushStats`. In order:
  1. **The generation:** one past the highest present, whether current or left over by an interrupted push.
  2. **The files:** each one's chunks, in transactions of at most 32 chunks (2 MiB), then its `sf` record.
  3. **The switch:** one transaction sets `sg`.
  4. **Clean-up:** one transaction clears every generation but the new one and the one before it.
  * A test-only option stops before the switch, to simulate an interruption.
  * `main.rs` adds `Static { Push, List }`.

### 3. `test(server): push_input times the first push's path` (the user's choice)

* **A recorder** (`svc::testing::trace`, under `test-utils`, always cheap): `(Instant, conn or lease, step)` events at:
  * the reader's read (`conn.rs`)
  * the append's commit
  * the lease bell's watch firing (`push.rs`)
  * the turn that reads the connection's input
  * the frames sent
* **On failure,** `push_input`'s message lists, for its slowest push, those events from the push's start to its arrival, in milliseconds. Other tests in the binary run alongside, so events are filtered by its connection and lease.
* **Forced failure,** once: a test-only 1.2 s delay injected after the bell fires must show in the message as the gap before "turn read". Then removed.
* **TD-TEST-OPEN-FAILURES item 2** says the message now names the step.

### Tests (`tests/stream.rs` and a new `tests/statics.rs`; both backends where a store is involved)

* **Ranges,** over the bundled, dir and replicated sources:
  * `a-b`, `a-`, `-n`, `b` clamped, `-n` larger than the file, `0-0`, the last byte
  * 416 past the end, and 416 on an empty file
  * several ranges → 200; malformed (`bytes=x`, `items=0-1`) → 200
  * `If-Range` matching (206) and not (200)
  * HEAD with and without a range (headers, no body)
  * 304 still works
  * A 5 MiB dir file streams whole and by range, byte for byte.
* **The content host:**
  * with `Host: view…`: `/_zen/sw.js` (with `Service-Worker-Allowed: /`), `/_zen/config.json`, `/_zen/boot.html`
  * a navigation to `/site/x` → the boot page (trusted CSP, `no-store`)
  * a non-navigation → 503 `Zen-Reason: starting` with exactly sw.md §4's headers
  * `/unencrypted/style.css` there → not served
  * `/v1/info` there, listing `content_origins`
  * the app host unchanged
  * a device sign-in signing the content origin accepted, and one signing an unlisted origin refused
  * config refusals: no `public_origins`, an overlap, an invalid origin, no `dir`
* **Push:**
  * push A, then served by path, by range, with `/`, an alias and the SPA fallback
  * push B: A's files gone, B's served
  * push C: A's generation cleared, B's kept (keys counted)
  * an interrupted push leaves the current set served, and the next push uses a higher generation and clears the leftover
  * symlinks skipped; an empty directory refused
  * a chunk missing mid-stream errors the body
  * export and import keep the set
  * `static list` matches.
* **Negative controls** (each run once, then restored):
  * `parse_range` always `Full`: the range tests fail
  * `If-Range` ignored: its test fails
  * the content-host check off: the content tests fail
  * the switch before the files: the interruption test fails
  * the previous generation cleared too: "B kept" fails
  * content origins not in `explicit`: the sign-in test fails

### Docs (`docs:` commit)

* docs/API.md (§10 and info), docs/SW.md not yet (9.5)
* examples/zen-serve.toml: `[content]` and `unencrypted_source`, commented
* spec/OPS.md: a pitfall, that `static push` needs the embedded server stopped
* the log in both plan copies

### Verification

* `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`)
* `cargo test -p zen-server` (the new and touched suites 3×), and once with `ZEN_TEST_FDB` on a local cluster if the disk allows; otherwise CI's `fdb` job
* the negative controls above
* CI green on both runs

**Defaults chosen (overridable):**
* **Keep the previous generation** until the next push, rather than clearing it at the switch.
* **The content host serves `/v1`,** so the worker calls its own origin.
* **The trusted pages use the server's `csp`.**
* **`static push` talks to the store directly,** like `import`, not through an admin endpoint. On the embedded backend that means the server is stopped, so replicated is for clusters.
* **An empty directory is refused.**

## Step 9.3 in detail: `@zen/sw` core

Step 9.3 adds a new package, `packages/sw` (`@zen/sw`). Its core is `handle(request, site) → Response`: spec/sw.md §2–§7 as code, over a `@zen/fs` replica.
* It is worker-safe and typechecked as such, but runs no worker yet: the `sw.js` entry, enrollment and the browser come in 9.4.
* It is tested in Node (Node 22 has `Request`, `Response` and web streams) against a real zen-serve.

**Survey (read-only, this session):**
* **`@zen/fs` `Replica`** (`packages/fs/src/replica.ts`):
  * **Lookup:** `lookup(path) → Entry {node, sibling?}` walks shown names from the root, so conflict copies and display names resolve. It throws `not_found` / `not_dir`.
  * **Listing:** `entries(dir) → Map<name, Entry>`.
  * **The version:** `shown(e) → ShownVersion | undefined` gives the conflict copy's version, else the newest, with `.dot` (12 bytes) and `.size`. A queued local version has a zero dot (`isLocal`); the worker never writes, so it sees none.
  * **Content type:** `stat(path).contentType` comes from the node meta's `zen.content_type` xattr (`CONTENT_TYPE`).
  * **Streams:** `createReadStream(path, {start, end})` (with `end` inclusive) looks the path up again and reads `shown`. The version could change between the handler's lookup, which computes the ETag, and the read. Its stream rethrows `network` as `offline`.
  * **`typeOf`** is private (line 198).
* **Errors:**
  * `ZenError` (`status`, `code`; `isCode`) covers `not_found`, `not_dir`, `is_dir`, `offline`, `network`, `format` and `closed`.
  * `IntegrityError` has code `integrity`.
  * A chunk that fails to open is a plain `Error` named `ZenCryptoError` (message `decrypt` or `format`), not a `ZenError`.
* **Hex:** `bytes.hex` from `@zen/client`.
* **Wiring a package** (the list is explicit everywhere, nothing is discovered):
  * the root `package.json`: `workspaces` and the `check` chain
  * `vitest.config.ts` aliases
  * `scripts/build-apps.mjs` aliases
  * `apps/tsconfig.json` paths, if apps import it
  * the CI `client` job, whose tsc lines duplicate `check`
* **Package model:** `packages/fs` has `package.json` (`exports` → `dist`, `build: tsc -p .`), `tsconfig.json` (extends `../client/tsconfig.json`, which has lib `ES2023, DOM, DOM.Iterable` and types `node`) and `tsconfig.test.json` (`paths` to the client's sources).
* **No worker typecheck exists.** A `WebWorker` lib check that resolves `@zen/client` to its sources would fail:
  * `passkey.ts` uses DOM-only WebAuthn types
  * `wasm.ts` uses `process`
  * Resolving `@zen/client` and `@zen/fs` to their built `.d.ts` instead (`exports` → `dist`, with `skipLibCheck`) checks only our own sources. CI already builds both before typechecking.
* **Test harness:**
  * `world()` (`packages/client/test/helpers.ts`) spawns the server, claims it and signs in.
  * `installation(w)` and `watch(session)` come from `packages/fs/test/helpers.ts`; setting `net.down` makes the calls fail with `network`.
  * `solo()` replica options, as in `packages/fs/test/replica.test.ts`.

### 1. `spec:` commit (sw.md)

* **§3,** made precise:
  * A mount's own path without the slash (`/docs`) is its base directory, so it gets 301 to `/docs/`.
  * `/` goes to the root mount, or is 404 without one.
  * The first segment picks a mount by name. Otherwise the root mount gets the whole path, or 404 without one.
* **§3, listings:**
  * entries sorted by name, the directories first
  * each name HTML-escaped, with its link percent-encoded per segment and a trailing `/` for a directory
  * a `..` link except at a mount's base
  * no ETag, under §4's headers
* **§6, the ETag:** only a version the server holds gets one. A queued local version, which a worker that doesn't write never has, gets none, so `If-Range` never matches it.
* **§7,** how the replica's errors map to responses:
  * `not_found`, `not_dir` → 404
  * `offline` and `network` → 503 `offline`
  * `integrity`, `format` and a chunk that fails to open (`ZenCryptoError`) → 502 `integrity`
  * `closed` (the replica is restarting) → 503 `starting`
  * anything else → 500, with no reason, so an unexpected error never looks like a missing file
* **§4:** the policy's text is the one that both the server's `SANDBOX_CSP` and `@zen/sw`'s constant are tested against (below).

### 2. `fs:` commit (`@zen/fs`, small)

* **`Replica.streamOf(e: Entry, v: ShownVersion | undefined, {start, end})`:** the stream `createReadStream` builds now, for a version the caller already holds. `createReadStream` then calls it, so its behaviour doesn't change. The handler reads exactly the version its ETag names.
* **`typeOf`** is exported (as `nodeType(n)`).
* **Test:** `streamOf` on a version the path no longer shows still reads that version's bytes, as long as the version is kept.

### 3. `sw:` commit, the new `packages/sw`

**Files:**
* `package.json`: `@zen/sw`, deps `@zen/client` and `@zen/fs`, `exports` → `dist`, `build: tsc -p .`, `sideEffects: false`.
* `tsconfig.json`, extending the client's.
* `tsconfig.test.json`: `paths` to the client's and fs's sources, including tests.
* **`tsconfig.worker.json`:**
  * lib `ES2023, WebWorker`, types `[]`, `noEmit`, with only `src` included
  * `@zen/client` and `@zen/fs` resolve through the workspace to their `dist`
  * so it needs `npm run build -w @zen/client -w @zen/fs` first, as CI already does
* **`packages/fs/tsconfig.worker.json`:** the same check for `@zen/fs`'s own sources, so its "worker-safe" comment is enforced.

**Wired into:**
* the root `workspaces` and `check`: `tsc -p packages/sw/tsconfig.test.json`, plus the two worker checks after a build of client and fs
* the `vitest.config.ts` alias
* the `build-apps.mjs` alias
* the CI `client` job's tsc lines

**`src/`:**
* **`mounts.ts`:**
  * the `Mount` type of §2
  * `parseMounts(value: unknown): Mount[]`, which refuses:
    * a name outside `[a-z0-9-]{1,63}`
    * a repeated name
    * more than one `root`
    * a `base` that isn't absolute, or has an empty, `.` or `..` segment
    * an `index` name with `/`
    * a `fs` or `tree` of the wrong type
  * `route(mounts, url) → {mount, path, trailing} | 400 | 404 | redirect`, as §3
* **`range.ts`:** `parseRange(header, size)` as the server's `parse_range`, with the same table of cases in the tests.
* **`mime.ts`:**
  * the §5 table
  * `validMediaType` (RFC 9110 `type "/" subtype *(OWS ";" OWS parameter)`, tokens and quoted strings)
  * `contentType(name, xattr?)`
* **`headers.ts`:** `SANDBOX_CSP` and the §4 header set, applied to every response.
* **`pages.ts`:**
  * the error pages: short static HTML, with the 503 `locked` page linking `/_zen/enroll.html`
  * the listing page
* **`handle.ts`:**
  * **Signature:** `handle(req: Request, site: Site): Promise<Response>`.
    * `Site = { mounts: readonly Mount[]; replica(m: Mount): Promise<Replica> }`.
    * A `replica` that rejects with code `locked` or `starting` (9.4's start-up, faked in the tests here) gives that 503.
  * **Methods:** GET and HEAD, else 405 with `Allow`.
  * **Routing:** `route`, then `replica`, then `lookup(base + path)`.
  * **Directories:** the 301 when the slash is missing; else the index; else the listing; else 404.
  * **Symlinks:** 404.
  * **Files:** `shown` and the ETag `"<hex node>.<hex dot>"`, then:
    * `If-None-Match` (the ETag, a list, or `*`) → 304
    * `If-Range`, then `parseRange` → 206 or 416, else 200
    * `Content-Length`, `Accept-Ranges` and `Content-Type`
    * HEAD opens no stream
    * the body is `streamOf(e, v, {start, end})`
  * **Errors:** mapped as §7 (step 1). An error once the body has started errors the stream (the replica's stream already does).
* **`index.ts`:** exports `handle`, `parseMounts`, `Mount`, `Site`, `SANDBOX_CSP`, `parseRange` and `contentType`.

### Tests (`packages/sw/test/`; vitest over a real server, as `packages/fs/test`)

* **`unit.test.ts`** (no server):
  * `parseRange`: the server's table of cases, plus random ranges against a model
  * `validMediaType` and `contentType`: the table, case, unknown extensions, a bad xattr ignored
  * `parseMounts`: each refusal
  * `route`: the encodings and the 400s, the mount's own path, `/` with and without a root mount, a root mount's fallthrough
  * **`SANDBOX_CSP` equals spec/sw.md §4's policy,** read from the file with its whitespace folded. A Rust unit test in `statics.rs` does the same for the server's constant, so the two can't drift apart.
* **`handle.test.ts`** (one server):
  * **The site:** a tree with an index page, nested directories, a name with spaces and `%`, a duplicate display name (`foo (2).txt`), a conflict copy (two devices write one file), a symlink, a `zen.content_type` xattr (good and bad), an empty file, and files of one chunk, several chunks and enough to have pages.
  * **Per path:**
    * the status and headers, with §4's set on every response (200, 206, 301, 304, 4xx, 5xx)
    * the body against `readFile`
  * **Ranges:** random ranges against a model, the edges, 416, `If-Range` matching and not, HEAD, 304.
  * **Directories:** the redirect (keeping the query), the index, the listing (escaping, links, no script), 404 without either.
  * **Errors:**
    * **Offline:** `net.down` gives 503 `offline` for what isn't kept, and 200 for what is.
    * **The site's replica rejecting** with `locked` gives 503 `locked`, and with `starting` gives 503 `starting`.
    * **Integrity:** the test adversary can't corrupt chunks (it withholds and freezes; `adversary.rs`), so a wrapped replica throws instead:
      * an `IntegrityError` from `lookup`, and a `ZenCryptoError` from `streamOf` before its first byte, each give 502 `integrity`
      * a stream that errors after some bytes gives an errored body
      * also once for real: a chunk kept in the replica's local store, tampered, gives 502
    * **Methods:** 405.
  * **A version replaced mid-request:** the ETag's version is the one streamed (`streamOf`).
  * **Memory:** a 64 MiB file through `handle`, read whole, raises `heapUsed` + `arrayBuffers` by less than 32 MiB (OPS.md §6).
* **Negative controls** (each run once, then restored):
  * the §4 headers left off error responses
  * `If-Range` ignored
  * a `..` segment allowed
  * listing names not escaped
  * the read by path instead of `streamOf` (the mid-request test fails)
  * `parseRange` always the whole file

### Docs (`docs:` commit)

* the log in both plan copies
* a short README for packages/sw (what it is, what's next)
* docs/CLIENT.md: a line on `streamOf`
* The deploy guide (docs/SW.md) is 9.5.

### Verification

* `scripts/build-wasm.sh`; `npm run build -w @zen/client -w @zen/fs`
* `npm run check`, which includes the two worker checks
* `npx vitest run packages/sw packages/fs` 3×, then the full `npx vitest run` with `ZEN_APPS_BUILT=1`
* the Rust test for `SANDBOX_CSP`
* the negative controls above
* CI green on both runs

**Defaults chosen (overridable):**
* **The worker typecheck goes through the built `.d.ts`,** not the client's sources, which have DOM- and Node-only code behind feature checks.
* **`streamOf` in `@zen/fs`** rather than looking up and reading by path, to close the race between the ETag and the bytes.
* **An unexpected error is 500,** not 404.
* **Listings put directories first.**

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
