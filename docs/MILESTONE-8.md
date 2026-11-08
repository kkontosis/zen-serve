# Milestone 8 plan: large files (the rest)

## Context

Milestone 5 (steps 7f, 7g and 8) built the paged layout:
* immutable, server-visible chunk-id pages, listed with their digests in a sealed version-2 manifest (formats.md §11.3–§11.6), about 640 GiB at the defaults
* per-file keys
* `readStream`, resumable `Tree.upload` within `chunk_grace_secs`
* the replica's queued writes

The placeholder (docs/MILESTONE-8.md) leaves upload sessions past the grace period, repacking, multi-GiB performance and the interplay with zen-ublk. Milestone 7 is the roadmap's "later" bucket (fips, ORAM), so 8 is next.

**A probe at 256 MiB** (release server, Node, embedded, loopback; not committed):
* **Speeds:** `writeFile` 22–24 MiB/s, in 34 commits, sent one at a time. `readStream` 112–131 MiB/s.
* **Appending needs a full read:** `chunkIndex` takes 3.0 s. Any append or edit to a version without a kept index reads the whole file, about 20 minutes at 100 GiB.
* **The replica holds too much memory:** a file written through a `FileHandle` stays whole in memory until the flush, then is sealed whole into the store. Peak RSS was 810 MiB for 256 MiB.
* **A bug:** `FileHandle.close({wait: true})` throws once the write is already sent (`replica.ts:1687`, `ov.files.get(node)` is gone).

**Found by reading:**
* **O(n) records:** the replica keeps each version's `ChunkIndex` (every id and SHA-256) as JSON, and queue entries list every chunk id. At 100 GiB that's about 100 MB of JSON per append.
* **Quadratic progress state:** `UploadState` copies every id and hash at each commit (`tree.ts` `state()`).
* **Chunks are fixed-size**, so an insertion re-uploads every chunk after it.
* **Repacking is moot** as built: pages are always full but the last, so nothing fragments. An edit costs one page, its chunks and the manifest.

**User decisions:**
* **Content-defined chunking** instead of repacking: a new manifest version with variable chunks.
* **Add-ons:** TD-FUSE-INTEGRITY (`zen-mount --integrity`) and TD-LORO-LARGE-SNAPSHOT (snapshots as zen-fs files). Not TD-FS-VERSIONS-INTEGRITY.
* **zen-ublk:** `import`/`export` of raw images, and a measured comparison of an image as a file and as a volume, documented as when to use which. Plus a TD that discusses volume snapshots to files (pros, cons, how it could be opened later).

**Defaults I chose (overridable at plan approval):**
* **Chunk boundaries are keyed:**
  * Content-defined chunking (FastCDC-style gear hash, normalized) with a gear table derived from a key the server doesn't hold, so it can't predict a known file's boundaries.
  * Chunk lengths are visible to the server already (a sealed chunk's length is its plaintext plus a constant), so lengths in pages add nothing.
* **Manifest version 3** for new versions:
  * Inline entries are `id ‖ u32 len`. Paged versions list pages `id ‖ digest ‖ u64 bytes`.
  * A new page form carries `id ‖ u32 len` per chunk, so a reader finds an offset by prefix sums.
  * v1 and v2 stay readable.
  * The fs policy gains `chunking: cdc | fixed`, default `cdc`. `fixed` writes v2.
  * A CDC version holds about 220 GiB at the defaults: entries grow from 16 to 20 bytes, and chunks average about 32 KiB under the `max_value_bytes` cap. Larger files use `fixed` (v2, 640 GiB). The writer picks it when it knows the size beforehand, and otherwise fails `too_large` with the hint.
  * Two-level pages for larger CDC files are a TD.
* **Upload sessions:**
  * A server record that references every chunk and page put in its commits, with an expiry (default 7 days, at most 30). It's renewable and closable by its user.
  * The sweeper releases expired sessions in bounded rounds, after which normal grace applies.
* **The write path works by pages:** a source may yield whole known pages, the index's hashes are optional, and an append or edit fetches only the pages it touches.
* **Pipelined commits:** up to 3 uploads in flight. Reads fetch several windows in parallel.
* **The replica stays offline-first:** queued content stays local. But a handle keeps at most 64 MiB of dirty plaintext, sealing the rest into the store as it goes, and records are binary and per page.

## Progress and resuming

**Rhythm (as milestone 6):** one sub-step per PR. Before each one:
1. restart from main if the previous PR is merged
2. plan the sub-step in detail here, and commit the plan

On the user's "go": a `spec:` commit if needed, then code and docs. Log the step in both plan copies (this file and `docs/MILESTONE-8.md`, kept equal with `cp`). Push, confirm CI is green on both runs, and pursue every red to its root cause. Stop for compaction, and confirm with the user before implementation begins after one.

**Branch:** `claude/brave-knuth-gk03mm`. Push with retries (2, 4, 8, 16 s). To restart from main: `git fetch origin main && git checkout -B claude/brave-knuth-gk03mm origin/main`, then push `--force-with-lease`.

**Constraints:**
* no model names in the repo; commit trailers as before
* `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push
* about 5 GB of disk free: prune `target/` before release builds and multi-GiB runs; the server's data goes in the scratchpad
* Rust 1.99.1, as on CI
* `ZEN_APPS_BUILT=1` for vitest; don't build zen-server while vitest runs
* no skips by default

**Log:**
* **Step 0** (plan): PR #63 (milestone 6 step 9) merged at 7c85a25. Restarted from main; `docs/MILESTONE-6.md` gained its last two log lines (fc843c9, not yet pushed).
  * **First, after approval:**
    1. delete the uncommitted probe `packages/fs/test/zzprobe.test.ts`
    2. write `docs/MILESTONE-8.md` (this plan, replacing the placeholder)
    3. update DESIGN-3 §6 item 8 (the scope and the link) and the README status ("Milestone 8: in progress")
    4. commit `docs: milestone 8 plan (large files, the rest)`
    5. push and open the PR
  * Done: ee840a5, PR #64. CI green; PR #64 merged.
* **Step 8.1** (spec, zen-core, wasm, vectors): planned below ("Step 8.1 in detail").
  * **First:**
    1. restart from main
    2. `cp` this file to `docs/MILESTONE-8.md`
    3. commit `docs: milestone 8 step 1 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (0dd69c0), PR #65.
  * `spec:` commit f95a431:
    * formats.md §11.3 (version 3), §11.6 (page form 2), §11.7 (the chunker, new), §12 (`chunking`, key 3)
    * fs.md §4, §4.1 (chunking, upload sessions), §6, §10
    * api.md §2 (features `fs_cdc`, `upload_sessions`; limits `upload_ttl_secs`, `upload_max_ttl_secs`), §6 (`upload`), §12.1 (upload sessions, new)
    * keyspace.md §3.6 (`us`, `ur`, `ux`)
    * labels `fs-cdc` and `fs-cdc-file`
  * **zen-core:**
    * `fs.rs`: `CdcParams` (`default_for`, `check`), `ManifestV3`/`BodyV3`/`PageRefV3` with a strict decode, `AnyManifest::V3`, `encode_page2`/`decode_page2`/`decode_any_page`/`page_capacity2`
    * new `cdc.rs`: `Chunker` (streaming `push` → cut offsets in the piece, `finish`, `lengths`), `gear_table`, `fs_cdc_key`/`file_cdc_key`
    * `FsKeys::cdc_key`
    * `FsPolicy.chunking` (`Chunking::{Cdc, Fixed}`, key 3 written only for `Fixed`, so existing policies encode byte for byte as before)
  * **Vectors:** `fs.json` gains `manifest_v3_inline`, `manifest_v3_paged`, `pages_v2` and `cdc`. Regenerating changed only that file, by additions.
  * **zen-wasm:**
    * TS types `CdcParams`, `ChunkEntry`, `PageRefV3`, `ManifestV3`, `AnyPage`; `FsPolicy.chunking?`
    * bindings: `sealManifestV3` (checked as a reader would), `openAnyManifest` v3, `FsKeys.chunker`, `fileChunker`, the `Chunker` class, `encodePage2`, `decodeAnyPage`, `pageCapacity2`, `cdcDefaults`
  * **Client:** `tree.ts`'s `AnyManifest` is versions 1–2 until 8.3, and opening a version-3 manifest gives `format` ("this client reads versions 1 and 2").
  * **Changes from the plan:**
    * **Chunker inputs:** `Chunker.push` returns cut offsets within the piece pushed, not in the stream, so no 64-bit offsets cross into JS.
    * **Vector inputs:** the chunker's vector inputs are xorshift32 streams (`vectors::xorshift_bytes`), not `DetRng`: the TS test regenerates them, and TS has no BLAKE3 XOF.
    * **Negative control:** with one mask (normalization off) the mean stays near 40 KiB, inside the 25–45 KiB check, so that check alone doesn't catch it. What normalization changes is how often a chunk is cut at `max`, so the test now also requires fewer than 0.5 % of chunks cut there. With one mask, 149 of 1,763 were.
  * **Found while testing:** `UnlockedFs.setPolicy` decided "nothing changed" from `fileKeys` and `rekeyAfterRotate` only, so `setPolicy({chunking: 'fixed'})` would have written nothing. It now compares `chunking` too. New test "chunking: set to fixed and back, kept in the header" (the header goes to version 2 and back to 1); with the old check it fails ("expected 'cdc' to be 'fixed'").
  * **Measured:** the 1 MiB vector input gives 28 chunks, a mean of 37,449 B. A CDC version at the defaults then holds about 5.8 M × 37 KiB ≈ 200 GiB.
  * **Tests:**
    * zen-core units: `cdc` 6 (pieces of 1, 7 and 65,536 bytes give the same cuts; bounds; resync after an insertion and a deletion, ≤ 2 chunks changed; the mean and the forced cuts over 64 MiB; keys change the cuts; parameter checks) and the policy test with key 3
    * `tests/fs.rs` 3 new (v3 round trips and sealing, every v3 decode refusal, page forms)
    * `tests/vectors.rs`: the new sections read back, including ≤ 2 changed chunks after the insertion
    * `fsvectors.test.ts` 2 new (v3 manifests and form-2 pages; the chunker over the vector input whole and in pieces, after the insertion, under the file key)
  * **Negative controls** (each run once, then restored):
    * one mask: the mean test fails ("149 of 1763 cut at max")
    * the inline sum check removed: the refusal test fails
    * the old `setPolicy` check: the chunking test fails
  * **Checks (all clean):**
    * `cargo fmt`; clippy `--all-targets -D warnings` on the workspace (default, `--no-default-features`, `--all-features`) and zen-core and zen-wasm on wasm32
    * `cargo test -p zen-core --features test-utils`; `cargo test -p zen-server --test fs`
    * `scripts/build-wasm.sh`, `npm run check`, biome
    * `fsvectors` 3×; `large`, `tree`, `keyslots`
    * the full vitest run (607 passed, 15 skipped: the opt-in benches)
  * Pushed 617b00b. **CI green on both runs.** PR #65 merged. Step 8.1 is done.
  * The scheduled CI check-in for PR #65 was cancelled.
* **Step 8.2** (server): planned below ("Step 8.2 in detail").
  * **First:**
    1. restart from main (94b5d76)
    2. `cp` this file to `docs/MILESTONE-8.md`
    3. commit `docs: milestone 8 step 2 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (9323508), PR #66.
  * `spec:` commit d3fffff:
    * keyspace.md §3.6: `us` without `refs`; new `uu`; close moves `ux` to 0; `cr`/`pr` count sessions too
    * api.md §12.1: `list`; another user's session reads as unknown; close needs no fs right; `ttl_secs` 0 is 400; `upload` with no puts only checks
    * api.md §1: `upload_gone` in the error table
  * **zen-proto:** `UploadRef`, `UploadOpen`, `UploadSession`, `UploadRenew`, `UploadExpiry`, `UploadClose`, `UploadList`, `UploadSessions`; `Commit.upload`; `Limits.upload_ttl_secs`/`upload_max_ttl_secs`.
  * **Server:**
    * new `upload.rs`: `open`, `renew`, `close`, `list`, `check`, `reference`, `sweep` (release rounds of 5,000, at most 100 per fs and pass)
    * `tree.rs`: `decode_any_page` in `put_page` and `collect_pages`; the engine's session, referencing each chunk and page once per commit (and once per session through `ur`); `release_page`; `release_chunk` and `add_quota` `pub(crate)`
    * `commit.rs`: the `upload` checks (16 bytes, fs `write`, puts of that fs only, 64 B per put toward `max_commit_bytes`) and `upload::check` before the puts
    * `config.rs`: the two limits, checked; `error.rs`: `upload_gone`; `keys.rs`: the four key families; `lib.rs`: routes, features `fs_cdc` and `upload_sessions`, the limits in `Info`, `upload::sweep` before `tree::sweep`
  * **Changes from the plan:**
    * **Every non-final release round rewrites `us`.** Planning had only the final round touch it, but then a renew or commit that read `us` before the expiry and committed after a middle round would still succeed and keep referencing while the release ran. Rewriting the record makes those conflict, and their retries (at a read version past the expiry) get 412.
    * **The final round is the one whose `ur` read comes back short** (fewer than the budget, so the conflicting read covered the whole range), not a separate empty read.
    * **`list` needs fs `read`.**
    * **Tests went in `tests/fs.rs`, not a new binary,** to save disk.
  * **Tests:**
    * `upload.rs` unit: 30 references released 7 per round, and only the last round clears the record and indexes
    * `config.rs` unit: the ttl checks
    * `tests/fs.rs` 7 new: `page_form_2_counts_and_releases`, `upload_session_keeps_content_past_grace`, `upload_session_expiry_and_renew`, `upload_session_rights`, `upload_session_cap_and_quota`, `upload_session_parallel_puts_count_once`, `upload_session_big_release` (12,000 references)
    * `tests/dump.rs`: an open session with a chunk survives export and import, still accepts a commit on the copy, and its reference holds after a sweep
    * `tests/api.rs`: the features and limits in `/v1/info`
  * **Negative controls** (each run once, then restored):
    * without the `ur` check: the parallel test fails (`cr` 8, not 1)
    * without `reference`: the keeps-content test fails (`kept a1…`)
    * without the user check: the rights test fails
    * clearing the record after any round: the unit test fails
    * `decode_page` in `put_page`: the form-2 test fails
  * **Checks:**
    * `cargo fmt`; clippy `--all-targets -D warnings` on the workspace (default, `--no-default-features`, `--all-features`)
    * `cargo test -p zen-server` on embedded: all suites pass
    * `scripts/build-wasm.sh` and `npm run check`: clean
  * Pushed e9a4b9d (d3fffff before it).
  * **CI:**
    * `rust`, `pure-rust`, `fdb` and `client` green on both runs
    * `devices` red on the push run, green on the pull_request run of the same commit
    * The red: zen-ublk's `fuse_writer_killed` and `fuse_ext4_round_trip` lost the writer's 2 s lease (`lease_lost`, then EIO, then ext4 read-only)
    * The cause is in the test, not this PR: `crates/zen-ublk/tests/devices.rs` runs the server in-process on the test's tokio runtime, and the test bodies call blocking code (`losetup`, `mkfs`, `mount`, `std::fs::write` into the FUSE mount, `sync`) from async code. A blocked worker can delay a renewal past the 1.33 s of slack.
    * Explained in a PR comment, and the failed job re-run once: green. PR #66 merged. Step 8.2 is done.
  * **Local FDB run:** hung because the local cluster ran out of disk (`DDTeamPivotAvailableSpaceTooSmall` in its logs), not because of the code; CI's `fdb` job covers it. The cluster was stopped and its data removed.
  * **Pending for the user:** fix the devices test (the proposed fix: start the test server on its own runtime thread in `World`, or `block_in_place` around the blocking helpers), in its own PR or folded into 8.3.
  * **Decided at the 8.3 plan:** the devices fix rides in 8.3 as its own commit.
* **Step 8.3** (`@zen/client` write and read paths): planned below ("Step 8.3 in detail").
  * **First:**
    1. restart from main (bc37ca9)
    2. `cp` this file to `docs/MILESTONE-8.md`
    3. commit `docs: milestone 8 step 3 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (4432212), PR #67.
  * `spec:` commit 2da7ba3 (formats.md §11.3; zen-core; wasm; `fs.json`'s v3 paged vector, 2 lines):
    * v3 pages hold 1 to `page_size` chunks, and each page reference carries its `count` (56 B a page)
    * the decoder refuses a count of 0 or above `page_size`, counts not summing to `n`, and a page's `bytes` below its count or above `count × max`
    * the fill rule for writers, and the repack
  * The devices fix: 16d1c67 (`world()` starts the server on a runtime of its own); the FUSE device tests pass locally.
  * **wasm:** encoders for the seven upload-session types (`wire.rs`).
  * **Client:**
    * `tree.ts`:
      * reading v1–v3 (`layout`, `spans`, `entries`, `pagesOf` with per-form checks, `readStream` in 4 MiB windows with `readAhead`)
      * `writeFile`/`upload`/`seal` through the new stager
      * `edit`, `editUpload`, `append`, `uploads`, `closeUpload`
      * `send(…, upload)`
      * the types: `Chunking`, `FileManifest`, `KnownPage`, `Edit`, `TailEntry`, `UploadCommit`, `UploadSessionInfo`, `EditOptions`, the compact `UploadState`, and optional `ChunkIndex.hashes`/`WrittenFile.index`
    * new `writer.ts`:
      * the fixed and CDC cutters
      * known chunks and pages, aligned or fetched from the base
      * pages with the fill rule and the realigning of pages with `previous`'s
      * pipelined commits reported in order
      * sessions
      * `editSource`
  * **Callers:**
    * `@zen/fs`: `sealWrite` writes `chunking: 'fixed'`; a v3 base gets the default chunk size; optional hashes in the handle
    * kanban's worker prints `onProgress`'s second argument
  * **Changes from the plan:**
    * **Page references carry a `count`.** Without it a writer taking a page over unread doesn't know `n`, and a page wholly inside a removed range would have to be fetched to count it.
    * **The form follows `base` or `previous`** (unless `chunking` or `chunkSize` is given), so a write with a fixed index under the CDC default keeps reusing its chunks; the policy applies to writes with neither.
    * **A commit carrying pages waits for the commits before it:** the server refuses a page whose chunks it hasn't applied (`unknown chunk in page`).
    * **Progress is reported through a promise chain,** so a commit is reported as soon as it and every earlier one are done, not when the pipeline next fills.
    * **A known chunk off a cut is fetched and cut again** (it was `bad_value`), also with only `previous` (its key opens it). `large.test.ts`'s mid-block case now checks that.
    * **`KnownChunk.last`** marks the base's last chunk, so the writer doesn't fetch the last page to recognise it.
    * **The writeFile path closes a page where a page of `previous` ends** (under the fill rule), so an insertion re-uploads at most 2 pages.
    * **`WriteOptions.size`** takes precedence over a `Uint8Array`'s length for the capacity check.
  * **Tests:**
    * new `cdc.test.ts`, 12:
      * round trips
      * an insertion (≤ 3 chunks, ≤ 2 pages, at 3 places)
      * an append (1 page and 1 chunk read, v3 and v2)
      * edits (touched pages only; random multi-edit rounds keep the fill property; truncation; refusals)
      * the fill rule's merge (deterministic, chunk-aligned cuts)
      * random reads and streams over v1, v2, v3 and after a repack
      * refusals (page bytes or count off, a swapped page, foreign known items)
      * pipelining (order, a failure at commit 4, resume)
      * the policy and its fallbacks (capacity, `size`, no parameters, no `fs_cdc`)
      * sessions (a resume past the grace with one and without one, an expired one is 412, `writeFile` closes its own)
    * `tree.test.ts` pins its chunk-counting tests to `chunking: 'fixed'`
    * `large.test.ts` (typing, the mid-block case)
    * a new `@zen/fs` writes test: a v3 file read and edited through the replica, flushed as v2, then reused
  * **Negative controls** (each run once, then restored):
    * pages located by division: the random-reads test fails
    * known chunks taken over off a cut: the edit test reads back wrong
    * the fill rule's merge off: the fill-rule test fails (5 pages, not 4); with random edits alone it was caught only sometimes, hence the deterministic test
    * `upload` left out of commits: the session test fails (`unknown chunk in page` on resume)
    * progress reported as commits finish: the pipelining test fails (`[2, 3, 1, 5, 6]`)
  * **Checks:** `cargo fmt`; clippy on zen-core, zen-wasm (wasm32) and zen-ublk (`device-tests`); `cargo test -p zen-core --features test-utils`; vectors regenerated; `scripts/build-wasm.sh`; `npm run check` (biome and every tsc project); `cdc`, `large`, `tree`, `fsvectors` and `stream` suites.
  * **Full vitest run:** 619 passed, 15 skipped, 1 failed. The failure was zen-mount's vault test (EIO on a read): the CLI runs `packages/fuse/dist/cli.js`, and this checkout's `dist/` builds dated from Oct 25, so an old client read a v3 file. After `npm run build` the mount suite passes (11/11). CI builds fresh.
  * **The insertion test was flaky (1 in 3 runs: 4 or 5 chunks re-uploaded, bound 3).**
    * The writer isn't at fault: in a probe of 30 writes it uploaded exactly the chunks the chunker changed.
    * The chunker's own resync over 300 random 100-byte insertions (avg 1 KiB, min 256) changed 1 chunk 64 % of the time, 2 21 %, 3 7 %, 4 4 %, 5 2.7 %, 6 1 %, 7 0.3 %. Normalized cutting depends on the position within a chunk, so an insertion can move a cut, and a few chunks follow before the cuts line up again.
    * The test now requires the uploads to equal the chunks whose bytes changed (cut with the same chunker), which is exact; the edit and append bounds allow that tail.
    * Then `cdc` passed 3× in full, and its randomized tests 10× more.
    * The plan's "≤ 2 chunks" (from 8.1's single vector input) holds for most insertions, not all; STATS gets the distribution in 8.7.
  * **CI on 9f0fba8: red on both runs, two different tests.**
    * **The pull_request run** failed `filled()` after the three edits: a short page and its neighbour held 408 entries, not over 409. The writer is at fault: a short page of the base was taken over unread while an edit shrank the page after it, and nothing re-examined the pair.
      * Fix: when the writer closes a short page right after a short base page taken over unread, it fetches that page and makes the two one page, if they fit.
      * Progress snapshots now copy the page list, since a page can be taken back.
      * The fill-rule test gained a deterministic case (a short base page whose next page an edit shrinks); with the merge disabled it fails (5 pages, not 4).
    * **The push run** failed kanban's "deleting a card moves its attachment directory to TRASH": the directory's parent read back as ROOT.
      * It didn't reproduce locally (6 of 6 passed).
      * The cause is a clock race in `deleteCard`: the move is stamped by a session that never read the directory, so it isn't ordered after the directory's creation (by the other session, in `addCard`). If that session's clock estimate trails the other's by more than the time between them, which can happen under CI load, the move sorts before the `mkdir` and loses.
      * Fix: `deleteCard` reads the directory first (`tree.getOne`), which observes its timestamps.
      * No test change: the race needs the clocks skewed.
  * Pushed cf82ad1 and ccfca39. **CI green on both runs of each** (push and pull_request), every job. PR #67 merged. Step 8.3 is done.
  * The scheduled CI check-in for ccfca39 fired and was handled.
  * These two lines are only in this file; they reach `docs/MILESTONE-8.md` at the next restart from main.
* **Step 8.4** (the replica and zen-mount): planned below ("Step 8.4 in detail").
  * **Decided at planning:** past the dirty bound a handle queues a write of its own (the user's choice), rather than sealing into the store unqueued.
  * **First:**
    1. restart from main
    2. `cp` this file to `docs/MILESTONE-8.md`
    3. commit `docs: milestone 8 step 4 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (879eca6), PR #68.
  * **CI on 879eca6:** the pull_request run was green. The push run's `fdb` job failed 3 of 5 zen-server `dump` tests (`export_import_round_trip`, `a_started_server_counts_as_empty`, `a_lagging_store_is_read_again`), all at `dump.rs:94`: the upload-session commit in `populated_on` got 412 `upload_gone`.
    * **The cause is in the test (8.2's), not the code:**
      * Session expiry is counted in commit versions (`read_version + ttl × VERSIONS_PER_SEC`).
      * Embedded versions are unix microseconds (about 1.8e15). The shared FDB test cluster's versions are small.
      * `migrate_copies_into_another_store` copies an embedded store into that cluster and calls `advance_version` (`dump.rs:362`), which jumps the cluster about 56 years ahead, far past a 7-day ttl (6e11 versions).
      * The tests in this binary run at once. A session another test opened just before the jump has expired by its next commit. Whether the jump lands in that gap is down to timing, which is why the pull_request run passed.
      * The binary's other helpers already retry `too_old` and `commit_unknown` for the same jump.
    * **The fix** (`crates/zen-server/tests/dump.rs` only, its own commit `test(server): dump tests survive a concurrent version jump`):
      1. In `populated_on`, open the session and commit under it in a bounded loop (50 tries, 200 ms apart, as `call` does). On 412 `upload_gone`, a concurrent jump expired the session, so open a new one and commit again.
      2. `session_of` takes the session with the highest expiry (the last `ux` entry, since the index sorts by expiry), not the first, so an expired earlier one isn't picked.
    * **Verification:**
      1. **Reproduce first:** with a temporary line in `populated_on` that advances the harness's own store by 1e15 versions between the open and the commit (embedded), `cargo test -p zen-server --test dump` fails at `dump.rs:94` with `upload_gone`, as on CI.
      2. **With the fix:** the same hacked run passes, and the old session is skipped by `session_of`.
      3. Remove the hack. `cargo test -p zen-server --test dump` passes on embedded, and on local FDB if its cluster starts (disk permitting). `cargo fmt --check`; clippy `--all-targets -D warnings` on zen-server.
    * Push; CI green on both runs.
    * **Done:**
      * **Reproduced:** with the jump forced on embedded (1e15 versions between the open and the commit), 4 of 5 dump tests failed at `dump.rs:94` with `upload_gone`, as on CI.
      * **The fix:** with the jump still forced, all 5 pass.
      * **The `session_of` control:** with `find` in place of `rfind` and the jump, `export_import_round_trip` fails at `dump.rs:266`, picking the expired session.
      * **After removing the hack:** the 5 dump tests pass on embedded; `cargo fmt --check` and clippy `-p zen-server --all-targets -D warnings` are clean.
      * **Not run:** local FDB, given about 2.7 GB of disk free; CI's `fdb` job runs it.
      * Pushed d2b5202. **CI green on both runs** (push and pull_request), `fdb` included.
  * **PR #68 merged** (the plan and the dump-test fix).
  * **The session's container was replaced** while it was idle: the repo was kept at d2b5202, and this file was restored from `docs/MILESTONE-8.md` with the lines since. Disk now has about 30 GB free; `target/`, `node_modules` and every `dist/` are gone. Before implementing: `npm ci`, `scripts/build-wasm.sh`, `npm run build`, and a zen-server build for the tests (cold).
  * **The implementation goes in a new PR.** On the user's "go":
    1. restart from main
    2. `cp` this file to `docs/MILESTONE-8.md` (bringing in these log lines)
    3. implement "Step 8.4 in detail"
    4. push and open the PR
    * **A note for 8.7's OPS pitfalls** (no code change): a cross-backend `migrate` or `import` changes the version scale, so upload sessions (like leases) may read as expired on the target, and their uploads start over.
  * **Implementation** (on "go"): restarted from main (416b662), this file copied in.
    * **Setup** for the new container: Rust 1.99.1 with the wasm32 target, wasm-bindgen-cli 0.2.129, `npm ci`, `scripts/build-wasm.sh`, `npm run build`, a zen-server build; `fuse` and `libfuse-dev` installed, and `@cocalc/fuse-native` built, for the mount tests.
    * **Spec:** fs.md §11.1, "Content" (only the chunks around what changed; untouched pages taken over) and a new "File handles" bullet. It rides in the implementation commit, since it only describes the replica.
    * **Client:**
      * `Edit.zeros`, streamed in 1 MiB pieces (`editBytes`, `inserted`); `stageEdit`'s no-base branch yields the parts and passes `size`
      * exports of the 8.3 types the replica needs (`AnyManifest`, `Chunking`, `Edit`, `EditOptions`, `FileManifest`, `KnownPage`, `UploadCommit`, `UploadSessionInfo`)
    * **`@zen/fs`:**
      * `handle.ts` rewritten over extents (base, memory up to 4 MiB each and doubling, zeros), with `dirtyBytes` and spills past the bound
      * a flush with base extents left is an edit (`queueEdit` → `sealEdit` → `tree.editUpload`); otherwise a whole rewrite (`queueFlush`, `previous` from `indexOf`, none for empty content)
      * `queueSealed` takes the local version before awaiting the send (`store` returns `{sent}`)
      * new `records.ts`: the binary per-page and inline records, written in `sealed()` from `Upload.index`; `indexOf` reads legacy, then records, then reads once and keeps the records
      * `ReplicaOptions.dirtyBytes` and `cdc`; `sealWrite` without `chunking: 'fixed'`; `chunkSize()` and the handle's `chunkSize`/`index` gone
      * `QWrite.index` is legacy only; `done` keeps writing a legacy entry's JSON index (only those entries carry one)
    * **zen-mount:** `MountOptions.integrity` (every op, read and write included, checks `trusted`; release drops the handle; unmount drops open files unflushed); the CLI's `--integrity` (`refuse` exits 3 on an alarm at attach, unlock or the first sync; `stop` skips the drain once untrusted, then `integrity.close()`; exit 3 if untrusted).
    * **Changes from the plan:**
      * **A handle copies what it's given.** FUSE hands `write` a `Buffer`, whose `slice` shares memory; the first mount run failed with "Cannot perform Construct on a detached ArrayBuffer". The handle now copies with `new Uint8Array(data)`.
      * **The memory test measures growth past the middle.** With `gc()` before each sample (exposed through `v8.setFlagsFromString` and `vm`, since vitest workers don't expose it), RSS at 128 MiB was about 113 MiB over the start, most of it fixed: wasm memory, the page memo and caches, and the HTTP client. Past the middle it grew by only 10 to 15 MiB. With the bound off, it grew linearly (+61 MiB over the second half). So the test asserts a total under 160 MiB and growth past the middle under 32 MiB, alongside `dirtyBytes` ≤ bound + one write, instead of one 96 MiB bound.
      * **The memory test uses a session that isn't watched:** the `watch` helper keeps every request, chunk bytes included (245 MiB in the first run).
      * **A flush with `wait` of an unchanged handle doesn't wait for earlier entries.** The random-operations test's first failures were a close without wait followed by a flush with wait; the test now also awaits `r.flush()`.
      * **The legacy v1-entry case checks `indexOf` returns the entry's index and that no read happens,** rather than counting `chunks/get`: the content of a queued write is local, so a read would make no request either. The record tests count `tree.chunkIndex` calls for the same reason.
      * **The in-process integrity test** follows the fs rollback test (Alice writes and checkpoints on `self`, Bob verifies there, then Bob's replica is served the copy), and first mounts without integrity to show the stale content reads.
    * **Tests:**
      * new `packages/fs/test/handles.test.ts`, 9: the memory bound; random operations against a model (CDC and fixed, 300 ops each, files of 1.5–3 MB, a 64 KiB bound); an edit's cost (a 1-byte overwrite: ≤ 2 pages, chunks sent equal to those the chunker changes, chunks fetched equal to those it removes; a 1-byte append: 1 page); `close({wait})`; index records (a rewrite with no read; after an edit, one read, then none; a legacy JSON record); a v1 queue entry with its index; zeros (64 MiB by `truncate`, `dirtyBytes` 4, then grown to 80 MiB by an edit); a crash mid-write (server killed, 3 spills queued, the replica closed with the handle open, then restarted: the file is the bytes up to the last spill)
      * `cdc.test.ts`: zeros in an edit, into a base and with none, and a negative count refused
      * `writes.test.ts`: the v3 case now stays v3 and sends at most 2 pages
      * `mount.test.ts`: the 1-byte test's fs set to `fixed`; "a rolled-back tree fails the mount closed" (in-process); the CLI with `--integrity` (exit 1 without pins storage; on a fork, exit 0 and the device's checkpoint naming the tree on `self`, then exit 3 with a `rollback` alarm and "not mounting" on the copy)
    * **Negative controls** (each run once, then restored):
      * the bound off (`dirtyBytes` Infinity): the memory test fails (`dirtyBytes` 134,217,728 > 4,325,376; RSS grew linearly)
      * every flush a whole rewrite: the edit test fails (1,717 chunks sent, not 1)
      * the old `queueFlush` (reading the overlay after the send): the `close({wait})` test fails ("reading 'local'")
      * the per-page lookup off: the records test fails (`chunkIndex` called once, expected 0)
      * the `trusted` check off in `mount.ts`: the in-process test fails (the read resolved with "v1")
    * **Docs:** CLIENT.md §8 (handles, records, chunking) and §9 (the mapping, `--integrity`); INTEGRITY.md §5 and §8; TECH_DEBT TD-FUSE-INTEGRITY resolved.
    * **Known, not changed:** index records are never evicted (nor were the JSON ones). A whole rewrite of a large file without records reads it once, as planned; in zen-mount that is the first spill of a `w` open over a large file.
    * **Checks:** `npm run check` and `npm run build` clean; `handles` 3× (RSS 112–115 MiB over the start, 10–17 past the middle); the full `npx vitest run`: 613 passed, 34 skipped, none failed, but 4 files didn't load (`apps/echo` connect and ssh, `packages/svc` sshlayer and sshwasm) for want of OpenSSH in this container. With `openssh-client` installed they pass, with the fs and fuse suites (75 tests).
    * **Found in the rerun:** `writes.test.ts`'s v3 case failed 3 runs in 4 (6 pages, all 583 chunks sent). Its content was `blob()`, whose bytes repeat every 256: after an edit, content-defined cuts are shifted by a phase and never realign in periodic data (the writer's `pending` stayed the same at every page), so the whole file is cut again. It passed only when the edit happened to leave the chunker in phase. The writer is right: the test now uses random content (6 of 6 runs pass). Periodic content (and long zero runs, cut at `max`) is where CDC can't resync after a length-changing edit; STATS notes it in 8.7.
    * Committed 1cb1440 (and the test fix after it); pushed.
    * PR #69 opened. **CI green on both runs** of 6abe026 (push and pull_request): `rust`, `pure-rust`, `fdb`, `devices` and `client`; the benches and `openssh-pq` skipped as usual. A safety-net check-in is scheduled; the PR waits on review and merge.
    * **PR #69 merged.** Step 8.4 is done. The check-in was cancelled.
    * These lines are only in this file; they reach `docs/MILESTONE-8.md` at the next restart from main.
* **Step 8.5** (Loro snapshots as files, TD-LORO-LARGE-SNAPSHOT): planned below ("Step 8.5 in detail").
  * **First:**
    1. restart from main (f6b7e98)
    2. `cp` this file to `docs/MILESTONE-8.md` (bringing in the 8.4 log lines)
    3. commit `docs: milestone 8 step 5 plan`
    4. push and open the PR
  * Then implement on "go", in a new PR.
  * Plan committed (2953c99), PR #70.
  * **Decided at "go":** PR #70 wasn't merged yet, so the implementation goes on the same branch, on top of the plan commit. PR #70 is retitled to cover the plan and the implementation.
  * **On "go":**
    1. Set up the container if needed: `npm ci`, `scripts/build-wasm.sh`, `npm run build`, and a zen-server build for the tests.
    2. The `spec:` commit (zendb.md §20).
    3. Implement and test "Step 8.5 in detail", then the docs.
    4. Log the step in both copies.
    5. Push, retitle PR #70, and confirm CI is green on both runs.
  * **Implementation** (on "go"):
    * **Setup:** this container had `node_modules`, the wasm package and the builds; only `zen-serve` was rebuilt.
    * **`spec:` commit** 86114a3 (zendb.md §20.1, §20.2, §20.5, §20.6).
    * **`@zen/loro` (`loro.ts`):**
      * `LoroOptions.fileBytes`
      * the row helpers `rowOffset` and `fileOf`, which checks a file row's shape (ids of 16 bytes, a hash of 32, a safe integer size)
      * `writeInline`, which removes a file row's node
      * `writeFile`: upload, then one transaction. It starts over, up to 3 times, when the row no longer names the node it uploaded to.
      * `removeIn`: reads the node first, so the move sorts after its creation (the kanban clock race of 8.3)
      * `snapshotOf` and `readFile` for loading. A version whose size differs from the row's is refused (`poisoned`).
    * **Changes from the plan:**
      * **`writeFile` starts over when its node is stale.** That covers a node taken from the row before the upload that the row no longer names by the transaction: the file went back inline (the node is in `TRASH`), or another client replaced it. Writing there would put the snapshot in a removed node, so the attempt starts over with the row read again. After 3 attempts nothing is written (the next compaction tries again).
      * **Test 3 was split in two.** A real race of two owners mostly ends with equal offsets, so the loser writes nothing. A second, deterministic test therefore commits another client's first file (at an earlier offset) between our transaction's row read and its commit, and checks that the transaction's re-run removes that node and writes its own.
    * **Tests:** the "too large" describe became "a snapshot larger than a commit goes to a file", with 9 cases:
      * on the small world (64 KiB commits, 16 KiB values), with the snapshot uploaded in several session commits:
        1. the compaction to a file
        2. the load (exactly the 5 tail events), then the next compaction replacing the version
      * on the main world, with `fileBytes: 0`:
        3. two owners at once
        4. another client's new file removed
        5. a version replaced between the reads (0 events read)
        6. a row naming no version (one warning, the whole log read)
        7. the hash picking the row's version over a newer sibling
        8. back to inline (the node in `TRASH`)
        9. no upload sessions (no `/v1/fs/upload/` call)
    * **Negative controls** (each run once, then restored):
      * the newest version instead of the hash match: test 7 fails (the document reads back empty)
        * The first form of this control compared objects from two `versions()` calls, so it never matched and the loader fell back to the log, and the test passed. The fixed control takes the last version.
      * `replaces: []` on every write: test 2 fails (2 versions)
      * no reread: test 5 fails (2 events read, not 0)
      * the session left open: test 1 fails
      * no remove of the other node: test 4 fails
    * **Docs:** DB.md (Loro limits); TECH_DEBT TD-LORO-LARGE-SNAPSHOT resolved.
    * **Checks:** `npm run check` and `npm run build` clean; `loro` 3× (17 passed).
    * **Full `npx vitest run`:** 63 files passed and 3 failed (`apps/echo` integrity, client integrity, db authenticated). They failed only because this container's `zen-serve` had been rebuilt without `test-utils` ("adversary: this zen-serve is built without test-utils"). Rebuilt as CI builds it (`cargo build -p zen-server --features test-utils -p zen-client`), those files and the db suite pass (239 tests).
    * Committed a0e56bc (the spec, 86114a3, before it) and pushed. PR #70 retitled to cover the implementation.
    * **CI green on both runs** of a0e56bc (push and pull_request): `rust`, `pure-rust`, `fdb`, `devices` and `client`; the benches and `openssh-pq` skipped as usual. A safety-net check-in is scheduled; the PR waits on review and merge.
    * The check-in at 20:05 found nothing new, so check-ins stopped.
    * **PR #70 merged.** Step 8.5 is done.
    * These lines are only in this file; they reach `docs/MILESTONE-8.md` at the next restart from main.
* **Step 8.6** (zen-ublk import and export, files or volumes, TD-BLK-SNAPSHOT-FILES): planned below ("Step 8.6 in detail").
  * **First:**
    1. restart from main (1ceeccc)
    2. `cp` this file to `docs/MILESTONE-8.md` (bringing in the 8.5 log lines)
    3. commit `docs: milestone 8 step 6 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (eac2689), PR #71.
  * **CI on eac2689 (docs only):** the pull_request run's `devices` job failed: `fuse_ext4_round_trip` lost its writer's 2 s lease, as in 8.2. The push run was green.
    * **Measured locally** (timing in the renewal loop):
      * a volume commit of about 260 writes takes up to 320 ms on the debug server
      * a renewal queued behind such commits took 899 ms
      * the renewal timer fired 667 ms late
      * a pin save's fsyncs on a runtime worker took up to 397 ms under disk load
      * A 2 s lease leaves 1.33 s of slack for all of that.
    * **Fix, its own commit 98c151c** (not pushed yet):
      * the device tests' lease is 6 s; the killed-writer tests wait 6.5 s
      * zen-blk saves the root pin on the blocking pool (`save_pin`)
    * **Checks:** the device tests passed 8 runs. One run hung with no output (killed at 600 s) and wasn't reproduced in 6 more.
  * **Implemented so far (uncommitted):**
    * zen-blk: `blocks_per_commit()`, `root_record()`, and the re-export of `RootRecord`
    * zen-ublk:
      * `src/image.rs`: import (one commit per window, the progress file, the resume check) and export (`.part`, holes)
      * `main.rs`: the `import` and `export` commands (hidden `--stop-after`, `--pause-after`/`--pause-file`)
    * `tests/image.rs`: the 5 tests, passing 3×
    * `tests/devices.rs`: `image_import_serve_export`, passing; the benches `bench_file_cdc`, `bench_file_fixed` and `bench_image`; `bench_fuse` and `bench_ublk` now report the stop time, peak RSS and stored bytes
    * ci.yml: `devices-bench` builds zen-mount (wasm, Node, `npm run build`)
  * **Negative controls,** all failing as required:
    * zeros written instead of skipped: test 1
    * no discard on an existing volume: test 2
    * any seq accepted: the resume refusal
    * no `seq + 1` acceptance: the crash case
    * no cleanup of a failed export: test 5
    * **Changes from the plan:**
      * The "zeros skipped" control first made zeros *discarded*, which stores nothing on a new volume, so it passed. It now writes zeros as data.
      * The "no .part" control is now "no cleanup": writing straight to `IMAGE` with the cleanup kept still removes the file. The test's wait for the export to start now has a deadline (it hung).
  * **The volume baseline** (`bench_fuse`, release, this container, one run):
    * 25.6 MiB/s write, 149 MiB/s read, 8.3 ms per dsync 4 KiB write, untar 0.21 s
    * stop 0.02 s, peak RSS 62 MiB, 279 MiB stored
  * **Found by the file benches** (zen-mount + loop):
    1. **fuse-native's per-operation timeout:** zen-mount ran with the default 15 s, so a flush or close of a large file answered `ETIMEDOUT` while it went on. `mount.ts` now passes `timeout: false` (uncommitted).
    2. **A sparse file's first flush** is a whole write, so `truncate -s 1G` seals and stores 1 GiB of zeros.
    3. **Queued writes refused** (`unknown chunk in page`, then `queued content … is missing` for the edits built on it):
       * The replica's sender puts a queued write's chunks and pages in commits of their own, then the write, with no upload session. Content a slow upload put in an early commit can be collected by the grace sweep before the write lands.
       * The bench's short grace (0, then 20 s) made it certain. The default grace (1 day) only bounds it.
       * Every later entry that edits that local version is then refused too.
  * **Decided:** fix (3) in 8.6 (the user's choice), below ("Step 8.6, continued").
  * Failed bench runs left dead zen-mount mounts that made `ps` and `losetup -a` hang; they were cleaned up (`umount -l`, `losetup -D`).
  * **On "continue" (plan approved):**
    * **Commit 2007fdc:** zen-ublk import and export. The benches use the default grace and report stored bytes at the stop (superseded content included). `bench_file` fails on a refused queued write and unmounts a dead zen-mount.
    * **Commit 4fa20a5:** the sender's sessions and zen-mount's `timeout: false`.
      * `Tree.openUpload` and `renewUpload` (the writer uses them)
      * `Inflight.session`
      * `upload_gone` mid-upload starts over under a new session
    * **Tests:** new `packages/fs/test/sessions.test.ts`, 2 cases, passing 3×; zen-mount "an operation that takes over 15 s finishes".
    * **Negative controls:**
      * without `upload` on the content commits, the session test fails: the bench's `unknown chunk in page`, then `not_found: /big` on another replica
        * At first the test checked the result only after a later rewrite, which re-uploads what it needs and passed. It now reads the first write back on another replica at once.
      * without `timeout: false`, the mount test fails: the kernel read returned **other bytes than the file's**, not just an error
        * The first form of the test delayed the fetch during the replica's `sync` (prefetch) and passed. The delay now starts after mounting, and the file is written afterwards.
    * **Checks:** `npm run check` and `npm run build` clean; `@zen/fs` (all 5 files), client `cdc` and `mount`: 72 passed.
    * **Docs:**
      * BLK.md §2 ("Moving images in and out")
      * CLIENT.md §8 (sending under sessions) and §9 (no per-operation timeout; zeros written)
      * TECH_DEBT: TD-BLK-SNAPSHOT-FILES and TD-FS-SPARSE
      * README
    * Pushed 4fa20a5, then the docs (8c874c6). CI green on both runs; **PR #71 merged.** The figures follow in a new PR.
  * **The benches, from main (566cfe2), PR #72:**
    * **The disk filled during a CDC run.** The bench's server store and zen-mount cache take several GB, and two earlier runs had left 4.8 GB of temp dirs. Those three dirs were deleted (a broader sweep was refused, rightly).
    * **The fixed-chunk run lost data:** queued writes were refused (`queued content … is missing`, then `unknown page`), and the file read EIO.
      * **The cause:** `Replica.evict` built its keep set before awaiting the store's flush and listing. What was sealed and queued meanwhile wasn't in it. Once queued content outgrows `cacheBytes`, eviction removes all it may, the newest included.
      * **The fix, commit 63e4d66:** `Content.evict` takes `keep` as a function, asked once the content is listed, with nothing awaited from there to the deletes.
      * **Test** (`writes.test.ts`): offline, a 1,000-byte cache, a handle spilling while another file is rewritten. It failed 6 runs of 6 on main and 0 of 6 with the fix; with only the snapshot restored, 5 of 6.
      * **Not changed:** `store()`'s global `protect.clear()`. Every seal and its queueing run in `serial()`, so none overlaps. A test of overlapping seals deadlocked on that, and the per-call variant was reverted.
    * **The file bench reports unsent writes** (commit 9505ee5). zen-mount's stop waits 10 s for its queue and leaves the rest in the cache dir.
    * **CI on 9505ee5:** the pull_request run's `client` job failed `handles.test.ts`'s memory bound (34.4 MiB past the middle, the bound 32); the push run passed.
      * The test took the largest gap between any one sample and the one sample at the middle, so a single noisy sample could fail it. Locally: 5.5–12.1 MiB.
      * **Commit 7dd7ac6:** it now compares the medians of the last quarter and the second quarter: 14.8 and 18.1 MiB, and 64.1 MiB (failing) with the bound off.
    * **The figures** (release, this container, one run each), in BLK.md §8 "Files or volumes":
      * **volume:** 25.6 MiB/s write, 149 MiB/s read, 8.3 ms dsync, untar 0.21 s; 62 MiB RSS; 279 MiB stored
      * **file, fixed:** 9.6 MiB/s, 45 MiB/s, 126 ms, 0.96 s; 1,822 MiB RSS; 1,333 MiB stored; 447 writes unsent at stop
      * **file, CDC:** 0.7 MiB/s, 10.5 MiB/s, 2,172 ms, 30 s; 1,544 MiB RSS; 8,182 MiB stored; 402 unsent
      * **import:** 37 MiB/s of data; **export:** 108 MiB/s; a 294 MiB-allocated 1 GiB image exports to 262 MiB allocated
    * **Not resolved:** zen-mount's 1.5–1.8 GB peak RSS under this load, against the handle's 64 MiB bound. Not investigated; BLK.md says so.
    * **Not run:** `bench_ublk` (no `/dev/ublk-control` here).
    * Pushed be9f8aa (the docs and this log).
    * **CI green on both runs** of be9f8aa (push and pull_request): `rust`, `pure-rust`, `fdb`, `devices` and `client`; the benches and `openssh-pq` skipped as usual. PR #72 waits on review and merge (check-in set for 00:40 UTC).
    * The check-in at 00:40 found nothing new, so check-ins stopped.
    * **PR #72 merged.** Step 8.6 is done; "plan 8.7" waits on the user's word. These lines are only in this file; they reach `docs/MILESTONE-8.md` at the next restart from main.
* **Step 8.6b** (zen-mount's memory): planned below ("Step 8.6b in detail").
  * **Decided at "plan 8.7":** this session investigates and fixes zen-mount's 1.5–1.8 GB peak RSS (the user's choice), and 8.7 is planned in another session.
  * **The user, on RAM:** RAM use is a big issue in general, so it gets noted in the docs as well. It becomes a standing rule (OPS.md §6), a TD listing the known hot spots, and a priority for 8.7's memory figures.
  * Plan committed (15fc9d8), PR #73.
  * **Measured** (on "go"): `bench_file_fixed` in release, with zen-mount under `NODE_OPTIONS=--require memlog.cjs` (a `memoryUsage` line every 500 ms, and heap snapshots at 700 and 1,300 MiB RSS). Peak RSS was 1,882 MiB (1,879 by the bench), with 408 writes unsent at stop. There are **two causes, at two different times:**
    1. **At 10–13 s (mkfs and the first writes): the records' write-behind backlog.**
       * ArrayBuffers reached 951 MB while the JS heap held 36 MB.
       * In the 700 MiB snapshot, 492 of the 594 MiB of buffers were 7,860 records of about 64 KiB, held by the `batch` arrays of `Records.flush` in its promise chain (`store.ts`), waiting to be written one file and one fsync at a time (`nodeDiskLocalStore.put` → `writeAtomic`).
       * Nothing makes the writer wait for the disk.
       * Once the backlog drained, ArrayBuffers fell to about 90 MB but RSS stayed at 1.2 GB: the allocator keeps freed memory, so the peak stays as it was.
    2. **From about 110 s (the 4 KiB dsync writes): the client's page memo.**
       * The JS heap grew from 116 to more than 674 MB.
       * The 1,300 MiB snapshot (taken after a full GC) holds 126 MiB live: 1.3 M objects, most of them the chunk ids of decoded pages in `Tree.pageMemo` (`tree.ts:939`). That's 5,624 separate `Uint8Array`s per v2 page, about 190 B each with headers, so about 1 MB of heap per 90 KB page.
       * The memo keeps 256 pages by digest. Each 4 KiB fsync makes a new version with 1–2 new pages, so the memo fills with stale pages: up to about 256 MB live, and that much churn again, which V8 collects late (the rest of the 674 MB was garbage).
       * The plan's survey figure ("the page memo, at most about 23 MB") counted raw page bytes, not decoded objects. It was about 10× low.
    * **Ruled out by the snapshots:** the handle (bounded), the sender, eviction, fuse-native buffers, and wasm memory (25 MiB).
    * **Not run:** the tmpfs cross-check. The retainers name the cause directly.
  * **The fix, for approval** (cause 2 is in `@zen/client`, outside `@zen/fs` and zen-mount):
    1. **`Records` bound** (`@zen/fs`), as planned: pending bytes (`pending` plus the batches not yet written) of at most `ReplicaOptions.pendingBytes` (32 MiB). Then `room()`, awaited by the replica's content sink and at the start of `net()` (every fetch that may cache content).
    2. **The page memo bounded by entries, not pages** (`@zen/client` `tree.ts`): at most 65,536 chunk ids memoized (about 12 MB of objects, 11 v2 pages or 16 full v3 pages), alongside the 256-page cap. A test counts the memo's ids after reading a file of many pages, and a negative control with the old cap must fail it.
  * **Decided:** fix both (the user's choice), and make every cap a setting.
  * **Implemented:**
    * **`@zen/fs` `store.ts`:** `Records` counts its backlog (the values pending and in batches not yet written), `room()` waits past `maxPending`, and a failed batch no longer stops the later ones: the chain recovers, since a stuck backlog would block `room()` forever.
    * **`replica.ts`:**
      * `ReplicaOptions.pendingBytes` (32 MiB) and `pageMemoIds` (passed to the tree)
      * the content sink and `net()` await `room()`
      * pinned content fetched in windows of 256 chunks (`FETCH_WINDOW`), each waiting for room: found by reading, since `fetchLoop` fetched a whole file's chunks in one `fetchChunks`
      * `replica.pendingBytes` and `status().pendingBytes`
    * **`@zen/client` `tree.ts`:** `TreeOptions.pageMemoIds` (65,536) and `pageMemoPages` (256); `memoPut` and `memoTrim` count ids; `tree.pageMemoSize`.
    * **zen-mount:** `--dirty-size`, `--pending-size` and `--page-memo`. `dirtyBytes` had no flag before.
  * **Tests:**
    * **new `packages/fs/test/memory.test.ts`:** a store whose puts take 2 ms; 48 MiB written as one `writeFile` (sealed whole before it's queued, like zen-mount's first flush of a large file), then read back in 1 MiB reads on a second replica, with `pendingBytes` sampled every 5 ms.
      * Writing peaks at 8.9 MiB (the bound of 2 MiB plus one commit's content), reading at 3.1 MiB.
    * **`cdc.test.ts`,** "the page memo keeps at most its bounds": the defaults hold all pages, `pageMemoIds: 818` holds at most 818, `pageMemoPages: 2` holds 2, and 0 holds none.
  * **Changes from the plan:** the write side is tested with one large write, not through a handle in 128 KiB writes. A handle's spill already awaits `rec.flush()` when it queues, so writes through a handle were bounded by `dirtyBytes` already. Without the bound, the first form of the test passed on the write side (0.007 MiB), and only its read side failed. RSS isn't asserted: the backlog is the direct measure.
  * **Negative controls** (each run once, then restored):
    * `room()` never waiting: the memory test fails, with 44.7 MiB pending while writing and 47.4 reading
    * `memoTrim` with only the 256-page cap: the memo test fails (1,699 ids, bound 818)
  * **Checks:** `npm run check`, `npm run build`; `@zen/fs` (all), client `cdc` and `mount`: 75 passed.
  * **Re-measured (release, this container):**
    * **`bench_file_fixed`:** peak RSS 1,879 → **426 MiB**
      * write 7.7 MiB/s (8.7 before), cold read 21.7 (23.5), dsync 139 ms (188), untar and sync 2.85 s (0.60: writes now wait for the cache dir), stop 15.9 s, 430 writes unsent
      * What's left at the peak (about 65 s) is about 190 MB of ArrayBuffers; a snapshot run follows.
    * **`bench_file_cdc`:** peak RSS 1,544 → **560 MiB**
      * write 0.8 MiB/s, dsync 2,087 ms, untar 34 s, stop 43 s, 373 unsent
      * but the cold read fell from 10.5 to 3.0 MiB/s: reads waited for fetched content to be written to the cache dir (one file and fsync per record, many small CDC chunks)
    * **A second fixed run with snapshots** (peak 621 MiB this time; peaks vary by run). At 380 MiB RSS, 135 MiB was live, all within the designed bounds:
      * 52 MiB: a handle's flush holding its memory extents (`dirtyBytes`)
      * 12 MiB: the handle's next writes
      * 30 MiB: records waiting (`pendingBytes`)
      * 25 MiB: wasm memory
      * The RSS beyond that is the allocator keeping freed memory, and garbage.
  * **Changed after the re-measure: fetched content is a cache, so it's dropped rather than waited for.**
    * `Content`'s tree cache `put` doesn't keep anything while the backlog is past `maxPending`, unless `keeping`.
    * Sealed content goes through `Content.keep` (always stored, and the sink waits with `room()`).
    * `net()` no longer waits.
    * The pinned fetch loop waits for room before each window and sets `keeping` during it, so a pin is kept whole, with at most one window past the bound per fetcher.
    * **The memory test gained:**
      * the reader keeps less than half the file (1.06 of 48 MiB)
      * a third replica pins the file: it's kept whole (48.06 MiB) and pending peaks at 11.2 MiB (the bound plus one window)
    * **Negative controls** (each run once, then restored):
      * `room()` never waiting: fails (45.5 MiB writing)
      * the drop off: fails (47.5 MiB reading)
      * `keeping` not raised in the pinned loop: fails (41.2 of 48 MiB kept)
  * **The final figures** (release, this container), in BLK.md §8:
    * **fixed:** peak RSS **446 MiB** (1,822 before); mkfs 5.0 s, write 7.6 MiB/s, cold read 24 MiB/s, dsync 146 ms, untar 0.86 s, stop 15.9 s with 428 unsent, 1,338 MiB stored
    * **CDC:** peak RSS **575 MiB** (1,544 before); mkfs 75 s (3.5 before), write 0.8 MiB/s, cold read 16.4 MiB/s (10.5 before, 3.0 while reads waited), dsync 2,905 ms, untar 32 s, stop 39.1 s with 314 unsent, 10,246 MiB stored
    * **The price of the bound:** writes go at the cache disk's pace. Under CDC, the first flush of the image's gigabyte of zeros now waits for the cache dir, which is most of mkfs's 75 s. `--pending-size` trades memory back for it.
  * **The first CDC re-run failed with ENOSPC** near its end, after 1,168 s. The test server stores about 10 GB under CDC, plus the store's overhead and the cache dir, which took more than the 20 GB free.
    * A second run with the disk watched grew about 1 GB a minute through the write phase, as CDC re-cuts long zero runs at every edit (the periodic-content case of 8.4).
    * `target/debug/incremental` and `deps` were deleted (no Rust change in this step), and the run finished.
    * OPS.md §2.2 now says what the CDC bench needs (up to about 20 GB).
  * **Docs:**
    * spec/OPS.md §6 "Memory": the rules, the settings table, how to measure
    * spec/TECH_DEBT.md TD-MEMORY
    * docs/CLIENT.md §8 (the bounds, `pendingBytes`, `pageMemoIds`) and §9 (the flags)
    * docs/BLK.md §8 (the figures, the cause and the price)
  * **Checks:**
    * `npm run check` and `npm run build` clean
    * the memory test passed 4 runs, beside the controls
    * the full `npx vitest run`: 646 passed, 15 skipped (the opt-in benches), none failed
  * Pushed 2491d86 (the fix) and 95d303b (the docs); PR #73 retitled to cover the implementation.
  * **CI green on both runs** of 95d303b (push and pull_request). PR #73 waits on review and merge; a check-in is set for 15:22 UTC.
  * These two lines are only in this file; they reach `docs/MILESTONE-8.md` at the next restart from main.
  * The check-in at 15:22 found nothing new, so check-ins stopped.
  * **PR #73 merged.** Step 8.6b is done.
  * **Next:** "plan 8.7", in another session (the user's word). 8.7's memory figures (the replica, zen-mount, the server) are required, per OPS.md §6 and TD-MEMORY.
* **Step 8.7** (benches, docs, outcome): planned below ("Step 8.7 in detail").
  * **First:**
    1. restart from main (57b052e)
    2. `cp` this file to `docs/MILESTONE-8.md` (bringing in the 8.6b log lines)
    3. commit `docs: milestone 8 step 7 plan`
    4. push and open the PR
  * Then implement on "go".
  * Plan committed (f68d66b), PR #74.
  * **On "go":**
    * Release zen-server built. FoundationDB 7.3.79 installed in `/opt/foundationdb`; its `--features fdb` build waits until the embedded benches finish, since it replaces the binary they run.
    * Commit 23aa632 `test: large-file benches (milestone 8 step 7)`: `benchkit.ts` (shared), the client, replica and zen-mount benches, the Loro case, and ci.yml `files-bench`. Pushed.
    * **Fixed in the benches during the 128 MiB smoke run:**
      * The 16 writers are one device, so they share one `TreeClock`. Each session on its own clock gave `operation timestamp reused`.
      * The resync count compares cut positions rather than hex-encoding every chunk: 196 s down to 3 s.
  * **The client bench at 4 GiB** (embedded, release, this container):
    * **CDC:**
      * Write: 56 MiB/s in 550 commits. 114,722 chunks (mean 36.6 KiB), 26 pages; the largest upload state 566 KiB.
      * `readStream`: 113 MiB/s at readAhead 3, 122 at 1.
      * Random 4 KiB reads: median 1.1 ms, p99 7–8 ms.
      * Append 1 MiB: 85 ms, 1 page and 1 chunk fetched, 29 chunks sent. 4 KiB overwrite: 218 ms. 100-byte insertion: 91 ms, 1 chunk and 1 page each.
      * A resume at half after 12 s (grace 5 s) sent only the rest. Without a session: refused, 400.
      * Pipelining at 1 GiB: 39.5 MiB/s with 1 in flight, 58.4 with 3.
      * 16 writers: 68 MiB/s in all. The client peaked at 1.4 GiB: 16 × 3 commits in flight.
      * The chunker: 389 MiB/s, mean 36.4 KiB, 195 GiB per version full and 97 GiB at the fill rule's worst. Insertions of 1–100 B changed 1 chunk 99.1 % of the time, 2 chunks 0.7 %, 5 chunks 0.2 %.
    * **Fixed:**
      * Write: 69 MiB/s. Reads: 112–127 MiB/s.
      * **The run failed at the 100-byte insertion** (`unknown chunk in page`). The bench's fault: the edit re-uploads half of a 1 GiB file, about 9 s, and ran without a session against a grace of 5 s. The edits now pass `session: true` (uncommitted).
    * **The server:** 2.2–4.1 GiB peak RSS (`VmHWM`) with one writer; redb's cache is 1 GiB by default.
    * **The data dir grew** 8 GiB under CDC and 10 GiB under fixed, for 4 GiB.
  * **A probe** (not committed): the allocated disk (`du -B1`) per GiB written, one world each.
    * Fixed 64 KiB: 2.04 GiB. Fixed 64 KiB − 256 B: 1.05. Fixed 32 KiB: 2.07. CDC: 1.66.
    * The apparent file size is twice the allocated size: redb grows its file sparse.
    * **The cause:** redb's buddy allocator rounds each page allocation up to a power of two (`page_manager.rs` `ceil_log2`). A sealed 64 KiB chunk (65,584 B) needs 17 pages of 4 KiB, so it gets 32.
    * The benches now report allocated bytes (uncommitted). STATS §1.2's "1.1–1.5×" was an estimate; it's wrong for large values.
  * **The replica bench at 4 GiB:**
    * write 9.2 MiB/s (the cache dir's fsync per record), with RSS at most 579 MiB, `dirtyBytes` 65 MiB and `pendingBytes` 39 MiB
    * a 1-byte edit 4.4 s
    * cold read 60 MiB/s
    * pin 11.9 MiB/s, with RSS 810 MiB
    * the server 2.2–3.1 GiB
  * **Decided (the user):**
    1. The default fixed chunk becomes 65,280 B (64 KiB − 256), kept even with the split below: about 6 % less in the replica's disk cache.
    2. Fix the embedded store's large values in 8.7.
    3. Investigate and fix the server's memory in 8.7.
    4. No data compatibility is needed between revisions: the app isn't deployed.
    * Planned below ("Step 8.7, continued").
  * **On "go" (after the compaction):**
    * **1. The embedded store splits large values** (`crates/zen-store/src/embedded.rs`):
      * **Changes from the plan:**
        * **A split value's tail stays in its `kv` record** (`0x01 ‖ u32 len ‖ u32 pieces ‖ tail`), and only whole pieces go to `parts`. The first form put the tail in `parts` too: it sat between two full pieces in key order, so it got a leaf of its own, and blocks (4,144 B) still took 2 pages (2.05×). A tail is shorter than a piece, so the record still fits one page.
        * **The page size isn't set:** redb's `Builder::set_page_size` exists only in its own tests. `PAGE` is its default, 4 KiB, and the disk test catches drift.
        * **The marker is `meta/store_format`**, so it can't meet a server `meta` name.
      * **Measured** (`large_values_cost_their_size_on_disk`, allocated growth over the bytes stored): CDC lengths 1.062×, 65,584 B 1.049×, 65,328 B 1.050×, blocks of 4,144 B 1.075×.
      * **Tests:**
        * `conformance.rs` gains `large_values` (both backends): 236 lengths from 0 to 100,000 B, densely around one and two pages; points, ranges forward and reverse with limits, overwrites (small, 100,000, 9,000, empty), an older read version, a clear and a range clear over split and inline values, a key written again, and versionstamped values and keys past a page.
        * `tests/embedded.rs`: the disk test; `no_pieces_left_behind` (exact piece counts through overwrites, clears, a range clear, a set and clear in one transaction, a versionstamped value over a split one, and 0 after clearing everything); `another_layout_is_refused` (no marker, and format 3).
      * **Negative controls** (each run once, then restored):
        * pieces 16 B larger: the disk test fails for every set (2.02–2.05×)
        * old pieces kept on an overwrite: the pieces test fails
        * no splitting: the disk test fails for CDC (1.49×), 65,584 B (2.01×) and blocks (2.00×), but not for 65,328 B (1.01×): a 65,280 B chunk fits 16 pages without the split. So the plan's "fails for every set" holds for three of the four.
      * **Checks:** `cargo fmt --check`; clippy `--all-targets -D warnings` on the workspace (default and `--no-default-features`) and zen-store (`--all-features`); `cargo test -p zen-store`, and `-p zen-server -p zen-blk -p zen-ublk` (26 suites, all passing).
      * Committed d51bfc3.
    * **2. Fixed chunks of 65,280 B** (`@zen/client`):
      * `CHUNK_SIZE` is defined once, in `writer.ts`, and re-exported from `tree.ts` (the other way round would make the two modules import each other's values).
      * spec: fs.md §4.1 (the default named), formats.md §11.3 and §11.5 (about 640 GiB a version and about 350 MiB a page at 65,280 B), glossary "chunk". STATS waits for the docs commit.
      * **Test:** `tree.test.ts`, "fixed chunks are 64 KiB − 256 by default, sealed within 64 KiB": 4 chunks sealed at most 65,536 B, the manifest's `chunkSize` 65,280. With 65,536 it fails ("expected 65584 to be less than or equal to 65536").
      * The existing chunk-counting tests still hold: `tree.test.ts`'s 11-chunk file is 11 chunks at either size.
      * **Checks:** `npm run check`, `npm run build`; the full `npx vitest run`: 647 passed, 22 skipped (the opt-in benches), none failed. Committed 4402e78.
    * **3. The server's memory, measured** (release, embedded, this container; a probe not committed: one 4 GiB CDC write under a session and a `readStream`, or 16 writers of 256 MiB at once; the server's `VmHWM` per phase, and `smaps_rollup` every second):

      | redb cache | Allocator | 1 writer: write | read | 16 writers |
      |---|---|---|---|---|
      | 1 GiB (the default) | glibc as is | 1,927 MiB | 2,668 MiB | 3,071 MiB |
      | 1 GiB | `MALLOC_ARENA_MAX=2` | 1,349–1,401 | 1,613–1,667 | |
      | 1 GiB | jemalloc (preloaded) | 1,750 | 1,815 | |
      | 256 MiB | glibc as is | 1,362 | 1,358 | |
      | 256 MiB | `MALLOC_ARENA_MAX=2` | 637 | 639 | 908 |
      | 64 MiB | `MALLOC_ARENA_MAX=2` | 273 | 274 | |

      * **Throughput doesn't depend on the cache here:** 61–65 MiB/s writing and 117–124 MiB/s reading at every size, and 69–72 MiB/s for the 16 writers.
      * **All of it is anonymous memory** (the file-backed part is 10 MB).
      * **Cause 1, redb's cache:** a commit's pages go into the read cache once written (`cached_file.rs` `flush_write_buffer`), so a steady write fills it to `cache_bytes`, 1 GiB by default, whether or not anything reads them again.
      * **Cause 2, glibc's arenas:** about 0.6–1 GiB more is memory freed but kept per arena. Commits run on tokio's blocking pool, and its threads get arenas of their own. `MALLOC_ARENA_MAX=2` gives it back; jemalloc did less well at 1 GiB.
      * **Cause 3, the commits in flight** (heaptrack, 16 writers at 2 GiB, cache 256 MiB; peak heap 630 MB):
        * redb's pages: about 355 MB (the cache, and the write buffer of the commit being applied)
        * request bodies being read: about 140 MB
        * the decoded commits (zen-proto): about 140 MB
        * the transaction's two copies (`overlay` and `ops`): about 59 MB
        * Embedded commits apply one at a time, and up to 48 (16 × 3 in flight) wait their turn, each holding its body or its decoded form. So the server's memory grows with clients × `parallel` × the commit size, and nothing bounds it.
      * **Decided (the user):**
        1. **The allocator:** zen-serve calls `mallopt(M_ARENA_MAX, 2)` at start, before any thread (glibc only, no new dependency), unless `MALLOC_ARENA_MAX` or `GLIBC_TUNABLES` is set.
        2. **The cache:** `[storage] cache_bytes`, 256 MiB by default (a hedge for metadata-heavy loads, which weren't measured; 64 MiB was as fast for large files).
        3. **The commits:** one shared buffer per value in a transaction, plus a byte budget for commit bodies in flight.
      * **Implemented:**
        * `zen-store`: `Options.cache_bytes` (`DEFAULT_CACHE_BYTES` 256 MiB) → `Builder::set_cache_size`; `Op::Set` and the overlay share one `Arc<[u8]>`.
        * `zen-server`:
          * `[storage] cache_bytes` (at least 1 MiB), used by `open_store` and `migrate`
          * `main` builds its runtime by hand, after `cap_malloc_arenas()`
          * `limits.commit_buffer_bytes` (64 MiB; at least `max_commit_bytes`, below 4 GiB; not sent to clients)
          * `commit::Budget`, an extractor that takes the request's Content-Length (or `max_commit_bytes` + 1 MB when absent, at most the budget) from a semaphore and holds it until the commit is answered, on `/v1/commit` and `/v1/log/append`
      * **Change from the first form:** the budget was first a middleware on the route, before authentication. A test showed a request without a session is answered 401 before its body is read, so it held nothing, but it still queued for the budget. It's now an extractor after `Caller`, so only a signed-in caller waits for it or holds it.
      * **Known, not changed:** a signed-in caller that stalls its bodies holds its share until the connection drops, so enough stalled commits can hold the whole budget and make other commits on that node wait. zen-serve has no body read timeout. OPS §6 and TD-MEMORY say so.
      * **Tests:**
        * `embedded::tests::a_set_value_is_buffered_once` (a test-only `buffered_bytes` counts each allocation once): 1 MiB + 3 B. With two copies: 2,097,158.
        * `config` units: `storage_cache_bytes`, `commit_buffer_bytes_is_checked`.
        * `tests/api.rs` `commits_past_the_buffer_budget_wait` (budget 1,000,000): a stalled body without a session is answered 401; a signed-in stalled body of 900,000 makes another 900,000 wait (still waiting after 1.5 s) while 50,000 goes through; once the stalled connection drops, the waiting one is answered. Without `Budget` on the handler it fails ("B waits while A holds the budget").
        * The arena cap has no test (glibc doesn't report its arenas); the benches measure it.
      * **Re-measured with the defaults** (no environment set): 1 writer 613 MiB writing (57.1 MiB/s) and 621 reading (116.7 MiB/s); 16 writers 712 MiB (64.9 MiB/s). Throughput is 8–10 % below the 1 GiB runs in this one run each; the benches re-run below.
      * **Checks:** `cargo fmt --check`; clippy on the workspace and zen-server `--no-default-features`; `cargo test -p zen-store -p zen-server` (22 suites). Committed d6d066c.
    * Pushed 509c8bd (the three commits and this log).
    * **CI on 509c8bd: red, `rust`, `pure-rust` and `fdb` (both runs, but `rust` only on the pull_request run).** All three were `commits_past_the_buffer_budget_wait`, timing out at the small commit.
      * **The cause is in the test:** the budget is a fair semaphore (tokio's), so a commit that would fit waits behind one queued before it. The test sent the 900,000-byte commit and the 50,000-byte one at once, and on CI the large one reached the budget first. Locally the small one happened to win.
      * **Reproduced** by queueing a large one 300 ms before the small one: the same `Elapsed`.
      * **Fix** (df2a690): the small one is sent and answered before the large one. Passed 5×; without `Budget` it still fails ("B waits while A holds the budget"). `Budget`'s comment says the budget is fair.
    * **The older red, on the pull_request run of 23aa632:** `devices` failed `ublk_ext4_round_trip` at `umount` ("target is busy"), right after its files were written, synced and closed. That commit changed no Rust, and it doesn't reproduce here (no ublk). `umount` in the device tests now retries a busy mount for up to 5 s and reports what holds it from `/proc` (2ea6253), so a next occurrence names its cause. The FUSE device tests pass here with it.
    * **CI on 2ea6253:** the pull_request run green. The push run's `client` job failed `writes.test.ts`'s v3 case (3 pages sent, bound 2); the dispatch run (files-bench) failed in the build steps of `files-bench` and `devices-bench`.
      * **The v3 case:** reproduced 2 runs in 12. An edit near the start of a full page (102 entries, its capacity) gave it a 103rd chunk, so the writer cut a full page and a page of 1 (the fill rule allows it: 1 + 102 > 102); with the appended last page, 3. The writer is right and the bound was wrong. The test now checks that every page but the first and the last is taken over unchanged, and that exactly the new pages are sent (at most 3): 16 of 16 passed (48d729a).
        * Its negative control (every flush a whole rewrite) passes too: a rewrite with `previous` reuses the unchanged pages as well (8.3), so pages can't tell the two apart. Chunks do, and `handles.test.ts`'s edit-cost test has that control.
      * **The build steps:** `npm run build` builds the workspaces in the order package.json lists them, and `@zen/fuse` came before `@zen/fs`, which it imports. A fresh checkout failed (`Cannot find module '@zen/fs'`); locally an older `dist/` hid it, since 8.6's `devices-bench` step. Reproduced with the old order and every `dist/` removed; fuse is now listed after fs (49b64d3).
    * **The zen-mount bench ran forever** (stopped after 40 min): it looked for `/big` by `node.name`, which a `TreeNode` doesn't have (`meta.name`), so its wait for the version never ended; zen-mount had sent everything at 22:57. tsc covered only `packages/fuse/src`. A `packages/fuse/tsconfig.test.json` now covers the tests, in `npm run check` and CI; with the old lookup it fails (TS2339). It also found one type error in `mount.test.ts` (fixed).
    * **A leftover:** the heaptrack run's server (its stop signal reached heaptrack's wrapper, not the server) kept running with 1.1 GB until found; stopped by PID. It used 1.4 % CPU, so the figures taken beside it stand.
    * **The client bench at 4 GiB, embedded, again** (`target/largebench-4g-2.log`; the CDC case re-run alone, since a clippy build overlapped its reads the first time):

      | | CDC | Fixed |
      |---|---|---|
      | Write 4 GiB (session) | 51–54 MiB/s, 550 commits | 66.9 MiB/s, 546 commits |
      | Data dir, allocated, for 4.0 GiB sent | +4,560–4,593 MiB (1.11×) | +4,428 MiB (1.08×) |
      | `readStream`, readAhead 3 / 1 | 107.6 / 115.6 MiB/s | 121.1 / 121.8 MiB/s |
      | 1,000 random 4 KiB reads, cold / warm | median 1.61 / 1.44 ms | 1.55 / 1.38 ms |
      | Append 1 MiB | 137 ms | 318 ms |
      | 4 KiB overwrite | 302 ms | 457 ms |
      | 100-byte insertion | 250 ms (1 page, 1 chunk) | 24.7 s on 1 GiB (8,209 chunks, 512 MiB) |
      | Resume at half, past the grace | 51.9 MiB/s, only the rest sent | 63.4 MiB/s |
      | Pipelining 1 GiB, 1 / 3 in flight | 27.0 / 52.6 MiB/s | 35.1 / 63.0 MiB/s |
      | Server peak | 587–637 MiB | 605–616 MiB |

      * 16 writers of 256 MiB: 60.2 MiB/s in all; the server 691 MiB (3.1–4.1 GiB before); the client 2,076 MiB (1.4 GiB of buffers: 16 × 3 commits in flight).
      * The chunker: 425.5 MiB/s, mean 36.5 KiB, 196 GiB a version full and 98 GiB at the fill rule's worst; 1–100 B insertions changed 1 chunk 98.1 %, 2 1.4 %, 3 0.2 %, 4 0.1 %, 6 0.2 %.
      * **Against the first 4 GiB run:** the data dir went from 2× and 2.4× the bytes sent (8 and 10 GiB, with superseded content) to 1.11× and 1.08×; the server from 2.2–4.1 GiB to under 0.7. Writes are 5–10 % slower, and random 4 KiB reads 1.4–1.6 ms against 1.1: a split chunk is a lookup in each table, and the cache is a quarter of the size.
    * **The replica bench at 4 GiB:** write 9.9 MiB/s (until queued and until on the server alike), RSS 564 MiB, `dirtyBytes` at most 65.0 MiB, `pendingBytes` 39.2 MiB, the cache dir at most 1,699 MiB; a 1-byte overwrite 385 ms (1 page and 1 chunk each way; 4.4 s in the first run); a cold read 58.2 MiB/s, RSS 541 MiB, 393 MiB kept; a pin 13.8 MiB/s, RSS 831 MiB, 4,102 MiB kept. The server 499–541 MiB (2.2–3.1 GiB before).
    * **The zen-mount bench at 4 GiB** (after the fix): write 9.7 MiB/s until closed and 9.6 until on the server, zen-mount 465 MiB, the server 507 MiB, the cache dir at most 1,559 MiB, stop 0.2 s; a cold read on a fresh cache 39.9 MiB/s with zen-mount at 296 MiB; a 1-byte write and fsync 292 ms, stop with it sent 0.4 s.
    * **The Loro case:** a 31 MiB snapshot compacts to a file in 618 ms and loads from it in 305 ms.
    * **The resume control:** a resume whose source starts one byte late (same length) fails the bench at its read-back hash (`largebench.test.ts:273`).
    * **FoundationDB at 1 GiB** (a release `--features fdb` build, a single-process cluster from `zen-serve init` here):
      * **The first run failed** the 16-writer case with `version_mismatch: ACL version must be 2`: every world of the bench served the cluster's root keyspace, so the second world found the first one's ACL. The bench's fault: each FDB world now has a `key_prefix` of its own, and `ZEN_BENCH_FDB_DATA` names the cluster's data dir for the disk row (it measured the server's empty data dir).
      * **The second run failed** "the same at 64 MiB without a session": it was written, not refused. It ran after two 1 GiB files were dropped, so about 57,000 chunks were ahead of its chunks in the collection queue, and FoundationDB's sweep rounds didn't reach them within the bench's 12 s. Not a server fault (content is kept longer, never shorter, than the grace). The check now runs in a world of its own.


## Steps (one PR each; each planned in detail before it starts)

**8.1, spec, zen-core, wasm, vectors:**
* **formats.md:**
  * §11.3 manifest version 3 (inline and paged forms with chunk lengths and page byte spans)
  * §11.6 page form 2 (`u32 0x8000_0000 | n ‖ n × (id ‖ u32 len)`)
  * §11.7 the chunker (new): gear table `KDF("zen/v1/fs-cdc", key)`, min, average and max lengths (the max is capped by `max_value_bytes`), and normalized cuts
  * §12 the policy's `chunking`
  * leakage
* **fs.md:** §4 and §4.1 (variable chunks, upload sessions, grace), §6 (session expiry in GC), §10 leakage.
* **api.md §12:** `fs/upload/{open,renew,close}`, the commit field `upload`, the limits `upload_ttl_secs` and `upload_max_ttl_secs`, the features `fs_cdc` and `upload_sessions`.
* **keyspace.md §3.6:** `us` (session record), `ur` (its references) and `ux` (expiry index).
* **zen-core:** `ManifestV3`, encode and a strict decode; page form 2; `cdc::Chunker` (streaming `push` → cuts); `page_capacity_v2`.
* **Vectors:** `fs.json` gains `manifest_v3` (inline and paged), `page_v2`, and chunker cuts over a deterministic input under a fixture key. Regenerating changes only those sections.
* **zen-wasm:** `sealManifestV3` and `openAnyManifest` (v1–v3), `encodePage2`/`decodePage2`, and a `Chunker` class.
* **Tests:** Rust units (decode refusals; the chunker resyncs after an insertion, with cuts that depend only on the bytes since the last cut) and the TS vectors test.

**8.2, server:**
* **Pages:** page form 2 accepted, with chunk references counted per entry and `pages/get` unchanged.
* **Upload sessions:** open, renew, close and the commit field. Each chunk and page put under a session gets one `ur` reference (`cr`/`pr` + 1, once per id). Closing or expiry marks the session, and the sweeper releases it in bounded rounds (the idempotency sweep is the template).
* **Rights and limits:** only the opening user may use, renew or close a session. At most 64 open per user and fs. `ur` keys count toward the quota.
* **Dump:** `us`, `ur` and `ux` export as data.
* `/v1/info` lists the features and limits.
* **Tests (both backends):** a session's chunks survive past the grace period while it's open and go one grace period after it closes or expires; a resume after the grace (a test clock: `chunk_grace_secs` 1); another user is refused; the quota; a close of a big session over several rounds; the dump round trip.

**8.3, `@zen/client` write and read paths:**
* **Reading v3:** offsets by prefix sums over pages, then over chunk lengths. Windows are counted in bytes, with several in flight.
* **Writing:**
  * The CDC writer in `stage`, with reuse by SHA-256 so an insertion re-uploads only the chunks around it.
  * The source may yield `KnownPage` (a whole unchanged page) as well as `KnownChunk`, so an append or edit costs O(touched pages).
  * `ChunkIndex` hashes are optional, so building an index fetches pages, not content.
  * Up to 3 upload commits in flight.
* **Resuming:**
  * `UploadState` becomes compact (the session, the pages done, the tail since the last full page, the file key), with no per-commit copies.
  * `upload(…, {session: true})` opens, renews and closes a session, so a resume works after the grace period.
* **Policy:** `fs.setPolicy({chunking})`; a known size over the CDC capacity picks fixed.
* **Tests** (scaled down with a small `max_value_bytes`, so 100,000-chunk files are a few hundred MB):
  * an insertion re-uploads ≤ 3 chunks
  * an append fetches only the last page
  * random reads match a model across v1, v2 and v3
  * a resume after the grace with a session, and one without it that fails
  * pipelining keeps commit order
  * the old manifests still read

**8.4, the replica and zen-mount:**
* **`FileHandle`:**
  * Dirty data is byte ranges over the base, with at most 64 MiB of plaintext, then sealed into the store as it goes.
  * The flush re-chunks only the dirty regions, until cuts resync with the base's.
  * `close({wait})` fixed.
* **Records:** binary indexes per page instead of JSON of every id and hash, and queue entries that name content lists instead of embedding them. Old records still load.
* **zen-mount `--integrity`** (TD-FUSE-INTEGRITY):
  * signs with the device key, or the vault's `signer()`
  * keeps pins in the vault or the cache dir
  * an alarm makes the mount's operations `EIO`, with a log line
  * a last checkpoint on unmount
* **Tests:**
  * a 1 GiB-equivalent (scaled) file written through a handle stays under a memory bound
  * random writes against a model, with CDC and fixed
  * the queue survives a restart mid-way
  * the `close({wait})` regression
  * a FUSE test of a rolled-back tree giving `EIO` (adversary)
  * the existing replica, fs and fuse suites

**8.5, Loro snapshots as files** (TD-LORO-LARGE-SNAPSHOT):
* **The row:** `$loro` rows hold `{offset, snapshot}` inline or `{offset, file: {tree, node, dot}}`, past a threshold (half `max_commit_bytes`).
* **Writing:** compaction uploads the snapshot (a session) and commits the row and the file's write in one transaction, then deletes the old version.
* **Loading:** streams it.
* **Spec:** zendb.md §20.5.
* **Tests:** the existing "too large" test now compacts; a load from a file snapshot plus the tail; two owners compacting; a missing file version reloads the row.

**8.6, zen-ublk import and export, files and volumes:**
* **`zen-ublk import IMAGE`:** aligned reads; zero blocks discarded or skipped; batched; resumable by seq. **`export IMAGE`:** read-only, writes holes for absent blocks (sparse).
* **Tests:** a round trip of a sparse image with holes; a refused size mismatch; device tests unchanged.
* **The comparison:** a measured ext4 image as a zen-fs file (zen-mount + loop) against a volume, giving docs/BLK.md "files or volumes".
* **TD-BLK-SNAPSHOT-FILES (new):** volume snapshots to zen-fs files, discussed:
  * **For:** a backup and transport format, readable without zen-ublk, dedupe through CDC.
  * **Against:** it needs the snapshots of TD-BLK-SNAPSHOTS, and a Rust tree writer in zen-client (none exists). It doubles storage, and the volume's freshness doesn't carry over to the file without a checkpoint entry.
  * **How it could open later.**

**8.7, benches, docs, outcome:**
* **Benches:**
  * an opt-in multi-GiB bench (`ZEN_BENCH=1`): write, read, append, insert, resume; 4 GiB here, embedded and on local FDB
  * a `files-bench` CI job started by hand, at 16 GiB on a runner
  * memory figures for the replica and zen-mount
* **Docs:**
  * STATS §1.3 (CDC capacity), §7 (large files: throughput, reuse after inserts, memory, sessions), §1.1 (session and page-2 overhead)
  * CLIENT.md §8–§9, DB.md or LORO notes, BLK.md
  * TECH_DEBT: TD-FS-LARGE-FILES fully resolved; TD-FUSE-INTEGRITY and TD-LORO-LARGE-SNAPSHOT resolved; new TD-FS-CDC-HUGE (two-level pages)
  * OPS pitfalls (sessions hold storage until they expire)
  * README, DESIGN-3 §6 item 8 done, and the Outcome

## Step 8.1 in detail

Step 8.1 specifies what 8.2–8.6 build on, and adds the zen-core codecs, the chunker and the vectors. It covers:
* manifest version 3 (variable chunks)
* page form 2
* the keyed content-defined chunker
* the policy's `chunking`
* upload sessions

The server is unchanged in this step: it keeps decoding pages with `decode_page`, form 1 only, and switches to form 2 in 8.2. No client behavior changes either: writers keep writing v2 until 8.3.

**Survey:**
* **zen-core `fs.rs`** (lines 136–380): `Manifest` (v1), `ManifestV2 {size, chunk_size, n, file_key, body: Inline | Paged {page_size, pages: (id, digest)}}`, `AnyManifest::{V1, V2}`, `page_len`, `page_capacity`, `encode_page`/`decode_page`, `page_digest`. They are sealed as kind 5 through `FsKeys` (`sealManifestV2`).
* **The server** decodes pages only in `tree.rs` (`put_page`, :767, and the GC, :1942), with `zfs::decode_page`. It treats manifests as opaque.
* **The key tree** (formats.md §2): NK is random and never rotated; MK_e is per epoch; `kdf(label, key, info)` gives 32 bytes (`kdf.rs`). File keys use `KDF("zen/v1/fs-file", file_key, u32(fs))`.
* **The policy** (`header.rs` `FsPolicy`, CBOR keys 1 `file_keys` and 2 `rekey_after_rotate`):
  * Unknown keys are kept.
  * The default policy writes a version-1 header.
  * wasm's `FsPolicy` TS type is `{fileKeys, rekeyAfterRotate}`.
* **Vectors:** `fs.json` has `manifest`, `manifest_v2_{inline,fs_keys,paged}`, `pages`, `write_paged`, `purge`, … (`vectors.rs` `fs_vectors`, lines 269–410). They're read back by `crates/zen-core/tests/vectors.rs` and `packages/client/test/fsvectors.test.ts`.
* **wasm (`crypto.rs`):**
  * TS types by hand in `TS_TYPES` (`Manifest`, `PageRef`, `ManifestV2`, `AnyManifest`)
  * the serde structs `JsManifestV2` etc. (:290–401)
  * the sealing bindings (:522–582)
  * `encodePage`/`decodePage`/`pageDigest`/`pageCapacity` (:1464–1489)
  * the policy getter and setter (:968–990)

### Spec (`spec:` commit)

**formats.md §11.3, manifest version 3 (new; v1 and v2 unchanged and still read):**
```
u8 3 ‖ u64 size ‖ u64 n ‖ u32 min ‖ u8 avg_bits ‖ u32 max ‖ u8 key_mode ‖ keys ‖ u8 form ‖ body
form 1, inline:  body = n × (chunk_id(16) ‖ u32 len)
form 2, paged:   body = u32 page_size ‖ u32 m ‖ m × (page_id(16) ‖ page_digest(32) ‖ u32 bytes)
```
* **Fields:**
  * `min`, `avg_bits` and `max` are the chunker's parameters (§11.7). Readers don't need them; a writer resyncs with a base only under the same ones.
  * `key_mode` and `keys` are as in v2.
  * `bytes` is the plaintext a page's chunks hold, so a reader finds an offset's page by prefix sums over the manifest, then its chunk by prefix sums over the page.
* **A decoder refuses:**
  * any `len` of 0 or above `max`
  * lengths (inline) or page `bytes` (paged) whose sum isn't `size`
  * `n = 0` with `size > 0`, or the reverse
  * `m ≠ ceil(n / page_size)`, or `page_size = 0` with `n > 0`
  * parameters with `min ≥ 2^avg_bits`, `2^avg_bits ≥ max`, `avg_bits ∉ 8..=24`, or `min < 64`
  * another `form`, or trailing bytes
* **A reader** checks each fetched page against its digest, its expected count and its `bytes`.
* **Capacity at the defaults:** a page holds 4,499 chunks, and a version about 1,300 pages (68 B each: 52 in the manifest and 16 in the stored version), so about 5.8 million chunks. That is roughly 180 GiB at the measured average chunk length (STATS gives the figure in 8.7). A file past it uses `chunking: fixed`, and v2's 640 GiB.
* **Changed from the milestone plan:** a page's `bytes` is `u32`, not `u64`, since a page holds at most 4,499 × 128 KiB < 4 GiB. That keeps 4 B per page.

**formats.md §11.6, page form 2:**
```
page2 = u32 (0x8000_0000 | n) ‖ n × (chunk_id(16) ‖ u32 len)      1 ≤ n, 4 + 20n ≤ max_value_bytes
```
* Form 1's `n` is below 2^31, so the top bit tells the forms apart.
* `page_digest` is unchanged, over all the bytes.
* The server reads the ids for its reference counts, and the lengths are in the clear. That leaks nothing new: a sealed chunk's stored length is its plaintext length plus a constant.
* Form 2 pages are listed only by v3 manifests, and form 1 only by v2.

**formats.md §11.7, the chunker (new):**
```
K_cdc = KDF("zen/v1/fs-cdc", NK, u32(fs))                 key_mode 0 (stable across rotations, like the naming keys)
K_cdc = KDF("zen/v1/fs-cdc-file", file_key, u32(fs))      key_mode 1
G[i]  = u64le(BLAKE3.keyed_hash(K_cdc, u8(i))[0..8])      for i in 0..256
```
* **A chunk** starts with `h = 0`. For its byte at position `p` (0-based in the chunk), from `p = min` on:
  1. `h = (h << 1) + G[b]` (mod 2^64)
  2. With `len = p + 1`, cut after it when `len = max`, or when the top `avg_bits + 2` bits of `h` are zero while `len < 2^avg_bits`, or when the top `avg_bits − 2` bits are zero from `2^avg_bits` on (normalized chunking).
* The rest of the input is the last chunk.
* So cuts depend only on the bytes since the last cut and on the key: an insertion changes the chunks around it, and the cuts resync after it.
* **Defaults:** `min` 8,192, `avg_bits` 15 (32 KiB), `max = min(131,072, max_value_bytes − 64)`, which is 89,936 at the defaults.
* **Why keyed:** without `K_cdc` the server can't compute where a known plaintext would be cut.

**formats.md §12, the policy:** key 3 `chunking`, 0 `cdc` (the default) or 1 `fixed`.
* An absent key is `cdc`, so the default policy still writes a version-1 header.
* With `cdc` writers write v3; with `fixed`, v2.
* The rollback paragraph: a server serving another policy (unsigned header) changes only how new files are cut.

**formats.md leakage (§11 and fs.md §10):**
* **Under CDC** the server sees every chunk's length, not only the last one's. Two files with shared content in one fs show matching runs of lengths: the same key gives the same cuts.
* **The key** keeps boundaries unpredictable to anyone without NK or the file key. A revoked member keeps NK, since it isn't rotated, so for files under the fs keys they can still predict cuts. Files under file keys don't have that exposure.
* **Padding** of lengths is milestone 11's metadata padding.

**fs.md:**
* **§4:** versions list chunks with lengths (v3).
* **§4.1:**
  * chunks are 1 B to `max_value_bytes` sealed
  * clients cut by content (formats.md §11.7) or at fixed sizes, by the policy
  * **upload sessions:** a chunk or page put under a session is referenced by it until the session closes or expires, then released normally (grace)
* **§6:** sessions in GC.

**api.md:**
* **§2:** the features `fs_cdc` (page form 2 accepted) and `upload_sessions`, and the limits `upload_ttl_secs` (604,800) and `upload_max_ttl_secs` (2,592,000).
* **§6:** the commit field `upload?: {fs, session: bytes(16)}`, and a step-8 sentence on it.
* **§12.1, upload sessions (new):**
  ```
  POST /v1/fs/upload/open  {fs, ttl_secs?}            → {session: bytes(16), expires_ms: u64}
  POST /v1/fs/upload/renew {fs, session, ttl_secs?}   → {expires_ms}
  POST /v1/fs/upload/close {fs, session}              → {}
  ```
  * **Open** needs fs `write`. `ttl_secs` defaults to `upload_ttl_secs` and is capped at `upload_max_ttl_secs`. At most 64 sessions per user and fs are open (429 `quota`).
  * **A commit with `upload`:**
    * The session must be the caller's user's and unexpired, else 412 `upload_gone`.
    * Every chunk and page the commit puts for that fs gets one reference from the session, unless it has one already (`cr`/`pr` + 1).
    * A put for another fs in such a commit is 400.
  * **Renew** sets the expiry from now (412 `upload_gone` once expired). **Close** marks it, and is idempotent.
  * **Releasing:** the sweeper releases a closed or expired session's references in bounded rounds. Each release is an ordinary one (a GC candidate whose grace restarts), so a write committed just before the close still holds its content.
  * **Expiry** runs on the server's clock. Each reference counts as one key toward the fs quota.
  * **Leakage:** which puts belong to one upload, which commit grouping and the device already show.

**keyspace.md §3.6:**
* `pack("us", fs, session)` → `user_fp(32) ‖ u64 expires_version ‖ u64 refs`
* `pack("ur", fs, session, u8 kind, id)` → empty (kind 1 chunk, 2 page)
* `pack("ux", fs, expires_version, session)` → empty: the expiry index, moved on renew and close
* All three are data (`export` copies them).

**labels.md and `labels.rs`:** `zen/v1/fs-cdc` and `zen/v1/fs-cdc-file`.

### zen-core

* **`fs.rs`:**
  * `ManifestV3 {size, n, cdc: CdcParams {min, avg_bits, max}, file_key, body: BodyV3::{Inline(Vec<(Id, u32)>), Paged {page_size, pages: Vec<PageRefV3 {id, digest, bytes}>}}}`, with `encode` and a strict `decode`
  * `AnyManifest::V3`
  * `encode_page2(&[(Id, u32)])`, `decode_page2`, `decode_any_page` (→ ids and optional lengths), `page_capacity2(max_value_bytes)`
  * `CdcParams::default_for(max_value_bytes)` and `CdcParams::check()`
* **New `cdc.rs`:**
  * `Chunker::new(key: &Key32, params)`, with the gear table computed once
  * `push(&mut self, data: &[u8]) -> Vec<usize>`: cut positions within the stream so far, streaming across calls
  * `finish()`
  * `cdc_key_fs(&FsKeys)`, from NK through the existing `kdf`, and `cdc_key_file(&[u8; 32], fs)`
* **`FsPolicy`** gains `chunking: Chunking::{Cdc, Fixed}`. Key 3 is decoded and encoded, an absent key is `Cdc`, and the default stays version 1.
* **Vectors (`vectors.rs`, `fs.json`):**
  * `manifest_v3_inline` and `manifest_v3_paged` (plaintext and sealed, as the v2 ones), and `pages_v2` (bytes and digests)
  * `cdc`:
    * the fixture fs's `K_cdc` and the first 4 `G` entries
    * a 1 MiB `DetRng` input and its cuts
    * the cuts after inserting 100 bytes at offset 300,000, showing at most 2 chunks differ before the cuts resync
    * a file key's cuts over the same input
  * Regenerating changes only these sections (and the description).
* **Tests:**
  * unit tests:
    * v3 round trips
    * every decode refusal above
    * page form 2 round trip, and form 1 and form 2 told apart
    * the chunker: cuts identical whether the input is pushed whole or in pieces of 1, 7 and 65,536 bytes; every chunk within `min..=max` but the last; resync after an insertion and a deletion
    * a fixed-key distribution check: the mean chunk length over 64 MiB within 25–45 KiB
    * the policy round trip, with key 3 and without it
  * `tests/vectors.rs` reads the new sections back

### zen-wasm (`crypto.rs`)

* **TS types:**
  * `ManifestV3 {version: 3; size; n; cdc: {min, avgBits, max}; keyEpoch?; fileKey?; chunks?: {id, len}[]; pageSize?; pages?: {id, digest, bytes}[]}`
  * `AnyManifest` gains it
  * `FsPolicy` gains `chunking: "cdc" | "fixed"`, optional on input, defaulting to `cdc`
* **Bindings:**
  * `FsKeys.sealManifestV3`; `openAnyManifest` decodes v3
  * `encodePage2`, `decodeAnyPage` (→ `{ids, lens?}`), `pageCapacity2`, `cdcDefaults(maxValueBytes)`
* **A `Chunker` class:** made by `FsKeys.chunker(params)` or `fileChunker(fileKey, fs, params)`, so K_cdc never reaches JS. It has `push(bytes) → Uint32Array` (cut offsets in the stream) and `finish()`.
* Rebuild with `scripts/build-wasm.sh`.

### TS

* **`packages/client/test/fsvectors.test.ts`:** v3 open and seal against the vectors; page form 2; the chunker's cuts over the vector input (whole and in pieces); the insertion's cuts.
* No other client change: `tree.ts` still writes v2, and v3 reading is 8.3.
* The `FsPolicy` type change compiles through `fs.ts`'s `policy` and `setPolicy`, with the new field passed through.

### Files

* **Spec:** `spec/{formats,fs,api,keyspace,labels}.md`
* **zen-core:** `crates/zen-core/src/{fs,cdc (new),header,labels,lib,vectors}.rs`, `crates/zen-core/tests/vectors.rs`, `spec/test-vectors/fs.json`
* **zen-wasm:** `crates/zen-wasm/src/crypto.rs`
* **TS:** `packages/client/test/fsvectors.test.ts`, `packages/client/src/fs.ts` (only if the policy type needs it)
* **Docs:** `docs/MILESTONE-8.md` (the log)

### Verification

* `cargo fmt --check`
* clippy `--all-targets -D warnings` on zen-core and zen-wasm (default, `--all-features`, `--no-default-features`, wasm32), and on the workspace once, since zen-server uses `FsPolicy` and `decode_page`
* `cargo test -p zen-core --features test-utils`; regenerate the vectors (only `fs.json`'s new sections change); `cargo test -p zen-server --test fs` (unchanged behavior)
* `scripts/build-wasm.sh`, `npm run check`, `fsvectors.test.ts` 3×; the client `large`, `tree` and `keyslots` tests once (policy and manifests unchanged for them)
* **Negative controls:** with the chunker's normalization switched off (one mask), the distribution test fails; with the decode's sum check removed, the refusal test fails
* CI green on both runs

**Defaults chosen (overridable):**
* **Gear key under the fs keys:** from NK, so cuts stay the same across rotations and chunks are reused across them, as under v2.
* **Chunker:** `min` 8 KiB and an average of 32 KiB, capped by `max_value_bytes`.
* **Lengths in page form 2 in the clear,** since sealed sizes already show them.
* **Upload-session expiry on the server's clock,** shown to clients as `expires_ms`.

## Step 8.2 in detail

Step 8.2 makes the server accept page form 2 and adds upload sessions (api.md §12.1). The client is unchanged until 8.3: no client sends form 2 or `upload` yet. The tsify types in zen-proto grow, but no TS code uses them.

**Survey:**
* **Pages:**
  * `tree.rs` `put_page` (:765) and `collect_pages` (:1905) decode with `zfs::decode_page`, which is form 1 only.
  * A page's first upload adds one `cr` per entry, and its collection releases one per entry (`release_chunk`: `cr − 1` plus a GC candidate with a `cp` pointer).
  * `validate` (`commit.rs`) estimates a page's cost as `48 × (data / 16)`. For form 2 (20 B per entry) that over-counts, which is safe.
* **Commit:** `execute` runs `Engine::put_chunk` for every chunk, then `put_page`, then the CRDT ops, and then `quota::apply(delta)`, where the delta is (bytes, keys) per fs.
* **Expiry:** leases (`lease.rs`) measure expiry in commit versions: `t.read_version()` plus `ttl × VERSIONS_PER_SEC`.
* **The sweeper:**
  * `sweep_once` (`lib.rs`) runs `commit::sweep`, which pages through 1,000 records a transaction, up to 100 transactions a pass.
  * Next it runs `tree::sweep`: pages before chunks, each `collect_*` bounded by a release budget.
* **Dump** exports every fs key except the relay prefixes, so `us`, `ur` and `ux` are data without any change.
* **Info** (`lib.rs` :69): a fixed feature list, and the limits from the config. New `Limits` fields carry `#[serde(default)]`, as `checkpoint_keep` does.
* **Tests** (`tests/fs.rs`, run on embedded, and on FDB when `ZEN_TEST_FDB` is set):
  * `two_devices(cfg)` harness
  * grace tests with `chunk_grace_secs` of 0 or 3 and `sweep_once` called by hand
  * helpers `chunk_put`, `page_put`, `upload`, `quota_bytes`, `sweep_until`

### Spec fixes (`spec:` commit)

Found while planning. The 8.1 text has two problems:
1. **`refs` in `us` makes commits conflict:** a commit into a session would rewrite its `us` record, so two commits in flight under one session (8.3 pipelines up to 3) would always conflict. Commits now only read `us`.
2. **No way to count a user's open sessions:** the 64-per-user cap would need a scan of every session on the fs.

The fixes:
* **keyspace.md §3.6:**
  * `us` → `user_fp(32) ‖ u64 expires_version`, where 0 means closed. `refs` is dropped.
  * New `pack("uu", fs, user_fp, session)` → `u64 expires_version`, the user's sessions. Open counts the user's live entries there, and close and the release clear it.
  * `ux`: a close moves the entry to `expires_version` 0, so the sweeper finds closed and expired sessions with one range read up to now.
  * All four keys are data.
* **api.md §12.1:**
  * New `POST /v1/fs/upload/list {fs} → {sessions: [{session, expires_ms}]}`, the caller's open sessions on the fs. A client that lost its upload state can then close them instead of waiting up to 30 days to get under the cap.
  * Another user's session is unknown to the caller: a commit or renew naming it gets 412 `upload_gone`, and a close of it is a no-op, so the endpoint doesn't reveal whether it exists.
  * Close needs no fs right beyond being the session's user, so a user who loses `write` can still release what their sessions hold.
  * `ttl_secs` of 0 is 400.
  * The `upload` field may come with no puts: it then only checks the session.
* **api.md §1:** `upload_gone` joins the 412 row of the error table ("not for this session; open a new one").

### zen-proto

* **New types:**
  * `UploadOpen {fs, ttl_secs?}` → `UploadSession {session, expires_ms}`
  * `UploadRenew {fs, session, ttl_secs?}` → `UploadExpiry {expires_ms}`
  * `UploadClose {fs, session}` → `Empty`
  * `UploadList {fs}` → `UploadSessions {sessions: Vec<UploadSession>}`
  * `UploadRef {fs, session}`
* **Changed types:**
  * `Commit.upload: Option<UploadRef>`, with serde default and skipped when `None`. Existing struct literals use `..Default::default()` or get the field.
  * `Limits.upload_ttl_secs` and `upload_max_ttl_secs`, with `#[serde(default)]`.

### Server

* **Config** (`config.rs`):
  * `limits.upload_ttl_secs` (604,800) and `upload_max_ttl_secs` (2,592,000)
  * checked: both at least 1, and the default at most the maximum
  * the per-user cap is the constant `MAX_UPLOAD_SESSIONS = 64`
* **Keys** (`keys.rs`):
  * `upload_session(fs, s)`
  * `upload_refs(fs, s)` (prefix) and `upload_ref(fs, s, kind, id)`
  * `upload_expiry(fs)` (prefix) and `upload_expiry_entry(fs, v, s)`
  * `upload_user(fs, user)` (prefix) and `upload_user_entry(fs, user, s)`
* **New `upload.rs`:**
  * **`open`:**
    * needs fs `write`
    * reads the `uu` range of the caller with conflicts and counts the entries whose expiry is past the read version: 64 or more gives 429 `quota`
    * makes a random 16-byte session id
    * sets `us`, `ux` and `uu` with `expires = read_version + ttl × VERSIONS_PER_SEC`
    * returns `expires_ms = now_ms + ttl × 1000`
  * **`renew`:**
    * reads `us` with conflicts; a missing session, another user's, a closed one or an expired one gives 412 `upload_gone`
    * moves `ux`, and rewrites `us` and `uu`
  * **`close`:** for the user's own open session, sets `us` expiry 0, moves `ux` to version 0 and clears `uu`. Anything else is `{}` with nothing written.
  * **`list`:** the `uu` range of the caller, live entries only. `expires_ms` comes from the version difference.
  * **`check(t, caller, r) -> Session`:** called in `execute` before the puts. It reads `us` with conflicts (a close or renew that commits meanwhile makes the commit retry), and applies the same refusal rules as renew.
  * **`reference(t, fs, s, kind, id, delta)`:** if `ur` is absent (a conflicting read, so two commits putting one id under one session can't both count it), it sets `ur`, adds 1 to `cr`/`pr`, and adds 1 key to the delta.
  * **`sweep(st, now)`:** per fs, page through `ux` up to `now`. For each session, transactions of at most `RELEASE_BUDGET` (5,000) `ur` entries:
    * clear each one, release it the ordinary way (`cr`/`pr` − 1, a GC candidate) and subtract its key from the quota
    * in the transaction that reads an empty `ur` range (with conflicts), clear `us`, `ux` and `uu`
    * up to 100 transactions per pass, as the other sweeps
* **Why the sweep's race is safe:**
  * A commit whose read version is past the expiry is refused.
  * A commit that read `us` before the expiry and commits later either:
    * adds its `ur` before the sweeper's final empty read, so that round sees it, or
    * conflicts with the final clear of `us` and retries into 412.
  * Releases are ordinary, so whatever a `write` committed before the close lists stays referenced by the version.
* **`tree.rs`:**
  * `put_page` and `collect_pages` use `decode_any_page`: form 2 counts and releases per entry as form 1 does.
  * Form-2 pages are refused 400 unless the feature is on (always on here; the check documents the rule).
  * `Engine` gains `session: Option<(u32, Id)>`. `put_chunk` and `put_page` call `upload::reference` for it, chunks with kind 1 and pages with kind 2.
  * `release_chunk` and `page_gc_candidate` become `pub(crate)` for the upload sweep.
* **`commit.rs`:**
  * `validate`: with `upload`, the session is 16 bytes, its fs is checked, and every chunk and page put is for that fs (400 otherwise). Each put costs 64 more bytes toward `max_commit_bytes` (the `ur` key and the count).
  * `execute`: `upload::check`, then the engine's session is set.
* **`lib.rs`:**
  * routes `/v1/fs/upload/{open,renew,close,list}`
  * features `fs_cdc` and `upload_sessions`; the two limits in `Info`
  * `sweep_once` calls `upload::sweep` before `tree::sweep`, so a released chunk becomes a candidate in the same pass, and its grace starts then
* **Docs:**
  * `docs/API.md`: the four endpoints and the commit field
  * the log in both plan copies
  * STATS and OPS figures wait for 8.7

### Tests (`tests/fs.rs`, embedded and FDB)

1. **`page_form_2_counts_and_releases`:**
   * form-2 pages are stored and served unchanged by `pages/get`
   * a paged write over them works
   * `cr` is +1 per entry (read from the store), including a repeated id
   * after the version is replaced, with grace 0, the page and then its chunks go
   * malformed form 2 is refused 400: a short body, length 0, and the flag with `n = 0`
   * form 1 still works alongside
2. **`upload_session_keeps_content_past_grace`** (grace 1 s):
   * open, then put chunks under the session with no `write`, and sweep past the grace: they stay
   * a second commit under the session puts more chunks, a page and the `write`
   * close and sweep: what the write lists stays; an abandoned chunk (put, never listed) goes one grace after the close, not before
   * without `upload`, the same chunks go after the grace (the control already in `chunk_grace_restarts…`)
3. **`upload_session_expiry_and_renew`:**
   * open with `ttl_secs` 2, renew to 4, and a commit at 3 s still works
   * after the expiry: a commit with `upload` is 412 `upload_gone` with nothing applied; renew is 412; close is a no-op
   * the sweep releases it, and the chunk is collected after the grace
4. **`upload_session_rights`:**
   * another user's commit, renew and close: 412, 412, and a no-op that leaves the session working for its owner
   * a read-only member can't open (403)
   * a put of another fs in an `upload` commit is 400
   * `ttl_secs` 0 is 400; a ttl over the max is capped (`expires_ms` checked)
   * `list` shows only the caller's live sessions
5. **`upload_session_cap_and_quota`:**
   * 64 opens work and the 65th is 429; after a close the next open works; an expired, unswept session doesn't count
   * the fs `keys` quota rises by one per reference (one per id, not per put) and returns to its baseline after the release
   * a `max_keys` limit refuses a commit whose references would pass it (429)
6. **`upload_session_parallel_puts_count_once`:**
   * 8 commits in parallel under one session put the same chunk
   * `cr` ends at exactly 1, and after the close and sweep the chunk goes after the grace
7. **`upload_session_big_release`:**
   * 12,000 one-byte chunks under one session (3 commits)
   * after the close, one `sweep_once` releases them all over several transactions
   * no `ur` keys remain, `us`, `ux` and `uu` are cleared, and the quota keys are back
   * a unit test in `upload.rs` calls the release with a budget of 7 and checks that each call releases at most 7, and that the last clears the record
8. **`tests/dump.rs`:** an export with an open session, imported into a fresh store; the session still accepts a commit and holds its content past the grace there.
9. **`/v1/info`** lists the features and limits (in `tests/api.rs`, beside the existing info test); config validation refuses a default over the maximum.

**Negative controls** (each run once, then restored):
* without the `ur` check, test 6 fails (`cr` > 1)
* without `reference`, test 2 fails (chunks collected)
* without the user check in `check`, test 4 fails
* without the final empty read before clearing `us`, test 7's unit test fails (the record is cleared with refs left)
* `decode_page` instead of `decode_any_page`, test 1 fails

### Files

* **Spec:** `spec/{api,keyspace}.md`
* **zen-proto:** `crates/zen-proto/src/lib.rs`
* **Server:**
  * `crates/zen-server/src/{upload (new),tree,commit,keys,config,lib}.rs`
  * `crates/zen-server/tests/{fs,dump,api}.rs`
  * struct literals of `Commit`, if any lack `..Default::default()`
* **Docs:** `docs/API.md`, `docs/MILESTONE-8.md`

### Verification

* `cargo fmt --check`; clippy `--all-targets -D warnings` on the workspace (default, `--no-default-features`, `--all-features`), and zen-proto with `ts` on wasm32
* `cargo test -p zen-server` (fs, dump, api, and the units), on embedded and once with `ZEN_TEST_FDB`
* `scripts/build-wasm.sh` and `npm run check`, since the proto types reach TS; the client `fs`, `large` and `tree` tests once
* the negative controls above
* CI green on both runs

**Defaults chosen (overridable):**
* **Keep the cap at 64 and add `list`,** rather than raising the cap or letting admins close sessions. Sessions of a user removed from the ACL simply expire, which becomes an OPS pitfall in 8.7.
* **Close needs no fs right,** only being the session's user.
* **Each reference counts as a quota key, but the session records don't,** since there are at most 64 per user and fs.

## Step 8.3 in detail

Step 8.3 makes `@zen/client` read version 3 and write it, the default under `chunking: cdc`. It also makes edits and appends cost the pages they touch, makes uploads resumable past the grace period through upload sessions, and pipelines commits and reads.

The replica (`@zen/fs`) and zen-mount keep writing fixed chunks until 8.4. They read v3 files through the client unchanged.

**Survey (`packages/client/src/tree.ts`, 1,596 lines):**
* **Reading:**
  * `read` locates chunks by `offset / chunkSize`. `chunkIds(v, first, last)` maps chunk indexes through pages (`pageSize` ids each). `fetchPages` checks each page's digest and its expected count.
  * `readStream` reads one window of 64 chunks per pull, which is one `chunks/get` request in flight. That's why the probe measured 112–131 MiB/s.
  * `openVersions` refuses v3 with `format`, and `opener` takes a file key only from v2.
* **Writing:**
  * `stage` cuts fixed blocks (`blocks()`), reuses previous chunks by SHA-256, cuts full pages as ids accumulate, and flushes a commit whenever `max_commit_bytes` or `max_commit_ops` would be passed. Each flush awaits its commit before the next one starts.
  * `UploadState` copies every id and hash at each `onProgress`, which is quadratic over an upload.
  * `KnownChunk` must start at a block boundary and be whole.
  * `ChunkIndex` lists every id and hash.
* **Users of these types:**
  * `@zen/fs` `handle.ts` and `replica.ts`: a fixed `chunkSize` handle, `KnownChunk` sources, `indexOf` (JSON records), and `base.manifest.chunkSize`
  * kanban's `attach` (`chunkSize`, `onProgress`); its test worker prints `state.chunks` after each commit
  * `large.test.ts` (fixed chunks of 1,984 B at a `max_value_bytes` of 2,048)
* **wasm already has** (8.1): `ManifestV3`, `sealManifestV3`, `openAnyManifest` v3, `encodePage2`, `decodeAnyPage`, `pageCapacity2`, `cdcDefaults`, `FsKeys.chunker`, `fileChunker`, and `Chunker.push`/`finish`/`pending`. It lacks encoders for the four upload-session requests.
* **The server's default `max_value_bytes` (90,000)** gives CDC defaults of min 8,192, avg 2^15 and max 89,936. At 2,048 (the large tests) `cdcDefaults` is undefined.

### A spec change: v3 pages need not be full

**The problem:** formats.md §11.3 has every v3 page full but the last (`m = ceil(n / page_size)`). Under CDC an edit can change the chunk count by a few, so every page after the edit shifts and must be fetched, re-encoded and uploaded again. That's about 1,300 pages (117 MB each way) for one small edit of a 200 GiB file. Under v2 it doesn't arise: fixed chunks don't shift unless bytes are inserted.

**The change (`spec:` commit):**
* **formats.md §11.3, v3 paged:**
  * Each page holds 1 to `page_size` entries, and `ceil(n / page_size) ≤ m ≤ n` (`m = 0` exactly when `n = 0`). `page_size` is then the writer's capacity per page.
  * A reader finds an offset through the pages' `bytes`, and checks a fetched page against its digest, its `bytes`, and an entry count of at most `page_size`.
  * The digest is in the sealed manifest, so swapping, reordering or truncating a page is still detected; the count check was redundant with it.
  * The decoder's rule becomes "`m` outside `ceil(n / page_size)..=n`".
* **Why now:** no v3 manifest exists yet (8.3 writes the first), and v2 is unchanged.
* **zen-core:** `ManifestV3::decode`'s check, and its refusal test, which gains `m = n + 1`, `m` below the ceiling and `m > 0` with `n = 0`. The vectors are unchanged, since their full pages stay valid.
* **Fill rule (writer, below):** a page is cut short only where an edit ends, and a short page is merged with its successor when the two fit in one page. So any two neighbours hold more than `page_size` entries, every page is more than half full on average, and capacity is at worst half of 8.1's figure (about 100 GiB at the defaults).
* **Repacking:** when a write would need more pages than one manifest holds, `edit` writes again with every page re-cut full (`repack`), which costs fetching all the pages, and fails `too_large` only if that doesn't fit either.

### Reading (v1–v3)

* **Types:**
  * `AnyManifest` is v1–v3, and `WrittenFile.manifest` and `Upload.manifest` are `ManifestV2 | ManifestV3`.
  * `FileVersion` gains `chunking: 'fixed' | 'cdc'` (v1 and v2 are fixed).
  * `opener` takes v3's file key too.
  * `openVersions` checks v3's inline entries and pages against the write's lists, as for v2.
* **Locating:**
  * A private `spans(v, offset, end)` gives the chunks a byte range falls in (`{id, start, len}`), fetching only their pages.
    * Fixed: by division, as now.
    * v3 inline: prefix sums over the entries' lengths, computed once per version and memoized.
    * v3 paged: a binary search over the pages' cumulative `bytes`, then over the page's lengths.
  * Decoded pages keep ids and lengths in the memo (`PAGE_MEMO` 256) and in the `pageCache` path.
  * `read` checks each chunk's plaintext length against its entry (v3) or the fixed size.
* **`readStream`:**
  * Windows are counted in bytes (4 MiB, aligned to chunk ends).
  * Up to `readAhead` windows are fetched ahead (default 3, an option), so several `chunks/get` requests are in flight. Order is kept, and memory stays at `readAhead + 1` windows.
  * Cancelling stops the prefetches.
* **`chunkIds(v, first, last)`** keeps working by chunk index. For v3 paged it walks the pages from the start (page counts vary), so the replica's prefetch of every id fetches each page once.

### Writing

**Choosing the form:**
* **What decides:** `WriteOptions.chunking` if given, else `chunkSize` given implies `fixed` (kanban and the replica keep their behavior), else the fs policy (`cdc` by default).
* **When CDC falls back to fixed:**
  * the server lacks the `fs_cdc` feature
  * there are no parameters: no `WriteOptions.cdc` or `TreeOptions.cdc`, and `cdcDefaults(max_value_bytes)` is undefined (small values)
  * the size is known (a `Uint8Array`, or `WriteOptions.size`) and exceeds the CDC capacity: `pageCapacity2 × max pages × 2^avg_bits`, conservatively
* **An unknown size that passes the capacity** fails `too_large` with the hint to write with `chunking: 'fixed'`.
* **Parameters:** a write with a v3 `previous` or base uses that version's parameters, so its cuts resync with it.

**The CDC writer (in `stage`):**
* **Cutting:** source bytes go through a `Chunker` from `keys.chunker(params)` or `zw.fileChunker(fs, fileKey, params)`. Bytes since the last cut are buffered (at most `max`), and each cut gives one chunk.
* **Reuse by SHA-256:** a chunk's plaintext SHA-256 is looked up among `previous`'s hashes, as now. Reuse needs only the same key, not the same parameters, since equal hashes mean equal bytes. So an insertion re-uploads only the chunks around it: the cuts resync within about 2 chunks (8.1's vectors).
* **Pages:** entries `id ‖ len` (form 2), cut into pages of up to `pageCapacity2(max_value_bytes)`, with the fill rule above. Inline up to what one value holds (`(max_value_bytes − sealed empty manifest) / 20`), paged past it.
* **Index:** `ChunkIndex` gains `lens?: number[]` and `cdc?: CdcParams`. `chunkSize` becomes optional (fixed only).

**Known content in a source (fixed and CDC):**
* **`KnownChunk`:** `{id, size, hash?}`, where `hash` is now optional.
* **New `KnownPage`:** `{page: PageRef | PageRefV3}`, a whole page of the base.
* **Base and validation:**
  * The base is `WriteOptions.base`, a `FileVersion`, or `previous`'s version.
  * Known items must be of a base under the write's key, else `bad_value` as now. Known chunks are checked against `previous.chunks` when it lists them; known pages against the base manifest's pages.
* **Taken over as is when aligned:**
  * Fixed: at a block boundary and whole. A known page also needs a page boundary, and must be full or last.
  * CDC: a known chunk when the chunker is at a cut (`pending = 0`). A known page also closes the open page, under the fill rule: when the open page is under half full, the known page is fetched and merged if the two fit.
  * In either form, the base's last chunk (cut by its end, not its content) is taken over only if it stays last.
* **Otherwise the writer fetches the item's plaintext from the base** and feeds it through, until a cut lands on a known item's start; from there items pass through again. Today an unaligned item fails `bad_value`.
* **This is the resync:** the source only says which bytes are new and which are the base's, and the writer fetches the base's bytes only around an edit.

**`Tree.edit(node, base, edits, opts)` (new) → `WrittenFile`:**
* **Edits:** sorted, non-overlapping `{offset, remove, insert}`, giving a truncate, an overwrite, an insertion, a deletion or an append.
* **It builds the source:**
  * known pages for pages untouched by any edit
  * known chunks for the untouched chunks of a touched page
  * the inserted bytes, and nothing for the removed ones
* **Cost:** the touched pages, plus the chunks from the start of the one holding each edit to where the cuts resync (CDC) or to the end of the edit's blocks (fixed). Nothing scales with the file's size.
* **No index needed:** everything comes from the manifest and the touched pages, so no `previous` or hashes. `Tree.append(node, data, opts)` is `edit` with one edit at the end.
* **Options:** as `WriteOptions` (`replaces`, `session`, and `repack` when the page count overflows, above). `editUpload` is the same without the write (as `upload`), which 8.4's replica handle uses with a sink.
* **Fixed form:** an insertion or deletion not a multiple of the chunk size shifts every later block, so the writer reads the base from there on. That's correct but costs the rest of the file, and the docs say so.

**`ChunkIndex` hashes are optional:** `chunkIndex(node, v, {hashes: false})` builds ids and lengths from the pages alone (no content read). A write whose `previous` lacks hashes reuses only through known items.

**Pipelining:**
* Upload commits are sent with up to `MAX_IN_FLIGHT = 3` in flight. The write commit waits for all of them, and `WriteOptions.parallel` (1–8) overrides the default.
* `onProgress` is called in commit order, each time with the state after the longest finished prefix. A failed commit fails the upload after the others settle, and the last state reported doesn't include it.
* Memory: about `parallel × max_commit_bytes` of sealed data.

### Resuming and upload sessions

**`UploadState`, compact and resumable:**
```
{ node, chunking, chunkSize? | cdc, size, pages: (PageRef | PageRefV3)[],
  tail: {id, len, hash?}[], fileKey?, keyEpoch?, session?, expiresMs?,
  uploaded, pagesUploaded, commits }
```
* **`tail`** is the entries since the last full page (at most one page), with hashes when known.
* **Building it** costs O(pages + tail) per report, not O(n).
* **Resuming** works for both forms. The source is the rest from `state.size`, which is at a cut (CDC) or a block boundary (fixed), and the chunker restarts there, since cuts depend only on the bytes since the last cut.
* **Hashes:** a resumed upload's index holds hashes only when every one is known (they aren't kept for full pages). Otherwise it has none.
* **Breaking change:** `chunks` and `hashes` leave the state. `onProgress` gains a second argument `{chunks, pages}`, the ids that commit put. The kanban uploader worker prints those instead.

**`upload(…, {session: true})` and `writeFile(…, {session: true})`:**
* **Session lifecycle:**
  * Open a session (`fs/upload/open`, `ttl_secs` from `opts.sessionTtlSecs`), and send every commit with `upload: {fs, session}`.
  * Renew between commits once less than half the ttl is left.
  * `writeFile` closes the session after its write commit. `upload` leaves it open, since the write comes later; it's in `Upload.session`, and the caller closes it with `tree.closeUpload(session)` after the write.
* **Resuming:** with a state that has a session, the session is renewed first. On 412 `upload_gone` the resume fails with that code: the content may be gone and the upload starts over.
* **Without a session**, resuming keeps working within `chunk_grace_secs`, as now.
* **The cap:** `tree.uploads()` lists the caller's open sessions (`fs/upload/list`), so a client that lost its state can close them and get under the cap of 64.
* **Server support:** `session: true` on a server without `upload_sessions` fails with `unsupported`.
* **wasm:** encoders and decoders for `UploadOpen`, `UploadSession`, `UploadRenew`, `UploadExpiry`, `UploadClose`, `UploadList` and `UploadSessions` (`wire.rs`).

### `@zen/fs` and other callers (kept working, no new behavior)

* **The replica stays fixed until 8.4:**
  * `sealWrite` passes `chunking: 'fixed'`.
  * A v3 base gives the handle the default chunk size: `base.manifest.chunkSize` exists only on fixed versions, now read through `FileVersion.chunking`.
  * The handle's index of a v3 base isn't usable (no `chunkSize`), so a flush based on a v3 file seals the whole file once, and later flushes reuse as now. Reads of v3 go through `tree.read`, by byte offsets, which is unchanged.
* **kanban** passes `chunkSize`, so it stays fixed, and its worker uses `onProgress`'s second argument.
* **Existing suites:**
  * `large.test.ts` sets fs 1's policy to `fixed`, so it keeps testing v2 as written.
  * Assertions elsewhere on `version 2` or `chunkSize` are updated where the default now gives v3.

### The devices test (folded in, its own commit)

zen-ublk's `tests/devices.rs` lost a writer lease on CI in 8.2: the in-process server shares the test's runtime with blocking calls (`losetup`, `mkfs`, `mount`, `sync`, writes into the FUSE mount).

The fix: `world()` starts the server on its own thread with its own multi-thread runtime, so the test's blocking calls can't delay lease renewals. CI's `devices` job runs it.

### Tests

**New `packages/client/test/cdc.test.ts`:**
* **The setup:**
  * `max_value_bytes` of 8,192 and `TreeOptions.cdc` `{min: 256, avgBits: 10, max: 8,128}`
  * A page then holds 409 entries, and a CDC version reaches tens of MB with thousands of chunks, so every test runs in seconds.
* **The cases:**
  1. **Round trip and form:** inline and paged v3 from bytes, an async generator and a web stream. Entries are within the bounds, and the server stores form-2 pages.
  2. **An insertion re-uploads ≤ 3 chunks:** 100 bytes inserted mid-file through `writeFile` with `previous`. It also re-uploads at most 2 pages, given the merge under the fill rule.
  3. **An append fetches only the last page:** `append` on a paged v3 file makes one `pages/get` of the last page and one `chunks/get` of its last chunk, counted with `calls()`. It does the same for v2 (the last page, and the last block if short).
  4. **`edit` costs touched pages:** an overwrite, an insertion and a deletion mid-file each fetch and upload only the touched pages. Pages are not all full afterwards, and the file reads back. A repeated edit at many places stays within the fill rule's bound (every adjacent pair over `page_size`).
  5. **Random reads match a model across v1, v2 and v3:** 200 random `read(offset, length)` calls and `readStream` ranges, against the plaintext. Also a forced `repack`.
  6. **Resuming:**
     * with a session after the grace: `chunk_grace_secs` 1, the upload killed mid-way, sweeps past the grace, then resumed; it uploads only the rest and reads back
     * without a session: the same fails 400 (a chunk missing)
     * an expired session: 412 `upload_gone`
     * `closeUpload` and `uploads()`
     * the state stays O(pages + tail): its JSON size after 30 commits is under a bound
  7. **Pipelining:** with 3 in flight (commits delayed by an edited `session.call`), `onProgress` sizes are non-decreasing and reported in commit order. A failure injected in commit 4 rejects the upload; the last state has 3 commits, and resuming from it reads back.
  8. **Policy:** `setPolicy({chunking: 'fixed'})` writes v2. A known size over the capacity picks fixed, and an unknown one fails `too_large` with the hint. A server without `fs_cdc` gets fixed (`info` stubbed through `TreeOptions.info`).
  9. **Refusals:** a page swapped, truncated or with lengths not summing to its `bytes` (the response edited) is refused `format`; so is a known item of another key.

**Updated:**
* `large.test.ts` (fs 1 fixed; the resume test uses the compact state and the second argument)
* `tree.test.ts` and `fsvectors.test.ts` where they assert v2
* kanban's attach test (worker output)
* `@zen/fs` replica tests: unchanged, run, plus one new case where a replica reads and edits a v3 file written by `writeFile`

**zen-core:** the refusal test for the relaxed `m`.

**Negative controls** (each run once, then restored):
* Inline v3 located by fixed division: the model test fails.
* Known chunks taken over unaligned: the insertion test reads back wrong.
* The fill rule's merge off: test 4's bound fails.
* `upload` left out of the commits: the session resume test fails (chunks collected).
* `onProgress` reported as commits finish rather than by prefix: test 7 fails.

### Files

* **Spec:** `spec/formats.md` (§11.3 v3 paged)
* **zen-core:** `crates/zen-core/src/fs.rs`, `crates/zen-core/tests/fs.rs`
* **zen-wasm:** `crates/zen-wasm/src/wire.rs`
* **Client:**
  * `packages/client/src/{tree,index}.ts`
  * `packages/client/test/{cdc (new),large,tree,fsvectors}.test.ts`
* **Callers:**
  * `packages/fs/src/{replica,handle}.ts` and a replica test
  * `apps/kanban/workers/uploader.ts` and its test
* **zen-ublk:** `crates/zen-ublk/tests/devices.rs`
* **Docs:** `docs/CLIENT.md` (§8–§9: CDC, `edit`/`append`, sessions, the compact state), `docs/MILESTONE-8.md` (the log)

### Verification

* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` on zen-core, zen-wasm and zen-ublk; `cargo test -p zen-core --features test-utils`
* **Build and lint:** `scripts/build-wasm.sh`, `npm run check`, biome
* **Suites:**
  * `cdc` 3×
  * `large`, `tree`, `fsvectors`, `stream` and the `@zen/fs` replica suites once
  * the kanban attach test
  * the full `npx vitest run`
* **Negative controls:** the five above.
* **CI:** green on both runs, including `devices`.

**Defaults chosen (overridable):**
* **Relaxed v3 page fill** (the spec change above), rather than accepting O(pages) work per CDC edit.
* **`chunkSize` given implies `fixed`,** so existing callers keep their chunks.
* **Small servers fall back to fixed** rather than scaling CDC parameters down on their own. Tests pass parameters explicitly.
* **Up to 3 upload commits and 3 read windows in flight.**
* **The replica stays fixed until 8.4,** and the devices fix rides in this PR as its own commit.

## Step 8.4 in detail

Step 8.4 makes the `@zen/fs` replica write large files in bounded memory and edit them by page (under CDC by default), fixes `FileHandle.close({wait})`, and adds `zen-mount --integrity` (TD-FUSE-INTEGRITY). No Rust or wasm change.

**Survey:**
* **`handle.ts` (218 lines):**
  * A `FileHandle` keeps dirty data as whole fixed chunks (`Map<index, bytes>`), with no bound: a file written through a handle stays whole in memory until the flush (810 MiB peak RSS for 256 MiB).
  * `flush` streams every chunk, the clean ones as `KnownChunk`s from the base's `ChunkIndex` (`replica.indexOf`), into `replica.queueFlush` → `sealWrite` (`tree.seal`, `chunking: 'fixed'`, a sink into the content store) → one queue entry.
  * A v3 base gets the default chunk size, so its first flush re-seals the whole file.
* **`replica.ts`:**
  * **`queueFlush` (:1676)** returns `this.ov.files.get(node)!.local` after `enqueue`. With `wait`, `enqueue` awaits the send, and by then the entry has left the overlay, so `close({wait: true})` throws. That's the bug.
  * **`indexOf` (:1307)** reads a JSON `ChunkIndex` record (`index`, key `node ‖ dot`, every id and hash), or builds one with a full read (`tree.chunkIndex`) and stores it.
  * **`done` (:1132)** writes the JSON index of every write it sends, from `QWrite.index`.
  * **`open` (:1636)** creates the handle with a chunk size (`chunkSize()`, the only user of that method).
  * The tree is made with `chunkCache`/`pageCache` on the content store, so the client reads a local version's chunks and pages from there.
* **`queue.ts`:**
  * Entries are JSON `{v: 1, key, ops, chunks, pages}`.
  * `QWrite.index` embeds the whole index, so a queued write of a 100 GiB file carries about 100 MB of JSON.
  * Eviction (`evict`, :995) keeps queued entries' `chunks`/`pages` and `protect`.
* **The client (8.3):**
  * `editUpload(node, base, edits, {sink})` builds the source from known pages and known chunks, and fetches only touched pages and the chunks around each edit.
  * `Edit.insert` is a `Uint8Array`. The no-base branch of `stageEdit` concatenates the inserts.
  * Several edits at one offset are already allowed (each needs `offset ≥` the previous one's end).
* **zen-mount:**
  * `cli.ts` signs in, unlocks (through `session.fs(id)`, which returns a new `Fs` each call), opens the replica, syncs, mounts, and on a signal unmounts, drains, closes.
  * `mount.ts` maps `ZenError` codes to errno (`EIO` otherwise).
  * Integrity is attached by `fs.integrity({device, local, onAlarm})` before unlocking; `integrity.trusted` turns false at the first alarm, and `integrity.close()` writes a last checkpoint.
  * The workers (`apps/shared/node.ts`) attach it with the vault store as `local`; `vault.signer()` gives the signer.
* **Tests:**
  * `packages/fs/test/{replica,writes,integrity}.test.ts`; the fork adversary is `TestServer.fork()` → `{self, copy}`; `restart()` and `kill()` exist.
  * `packages/fuse/test/mount.test.ts` mounts in-process (`canMount`) and runs `dist/cli.js` with a vault and a cache dir.

### Spec (`spec:` commit)

**fs.md §11.1, "Content" and a new "File handles" bullet:**
* Only the chunks around what changed are sealed: under CDC, until the cuts resync with the base's (§4.1). Untouched pages are taken over.
* **File handles:** a handle keeps its writes in memory up to a bound (the client's, 64 MiB by default).
  * Past it, the handle queues what it has as a write, as a flush does.
  * So a file being written may show partial content to other devices before the handle closes, as a local reader of a file being written sees, and a restart keeps what was queued.
  * Each flush is one entry and one version.

### Client (`@zen/client`)

**`Edit.zeros?: number`:**
* The bytes inserted after `insert` are zeros, streamed in 1 MiB pieces and never allocated whole.
* Counted in the edit's size.
* `stageEdit`'s no-base branch yields the parts in turn rather than concatenating them.
* Lets a handle that was extended (`truncate` up, or a write past the end) flush without allocating the gap.

### `@zen/fs`

**`FileHandle`, rewritten over extents:**
* **Extents:**
  * The content is a sorted list of extents `[start, end)`, each one of:
    * the base's bytes at the same offsets (a base extent, never moved: POSIX writes overwrite in place)
    * bytes in memory
    * zeros
  * `write`, `truncate` and growing split and replace extents by binary search.
  * Writes that continue a memory extent append to it (a buffer that doubles, up to 4 MiB per extent), so the kernel's 128 KiB writes don't make thousands of extents.
  * `dirtyBytes` (a getter) is the bytes held in memory.
* **`read`** walks the extents: base ranges through `replica.readVersion` (byte offsets, so any form), memory copied, zeros filled.
* **The bound:**
  * After a `write` or `truncate`, when `dirtyBytes` exceeds `ReplicaOptions.dirtyBytes` (default 64 MiB), the handle flushes.
  * The new local version is its base from then on, and the extents become one base extent.
* **`flush`:**
  * Nothing changed: nothing is queued.
  * **No base, or no base extent left** (a new file, or a whole rewrite such as `w` or `truncate(0)`): the content streams into `sealWrite` as today, with `previous = indexOf(base)` when there is a base, so unchanged chunks are reused by hash.
  * **Otherwise an edit:**
    * The edits come from the gaps between base extents: each gap removes the base range it covers and inserts its memory and zero extents. Each part is its own edit at the gap's end (`insert` or `zeros`), since edits at one offset are allowed.
    * The new replica method `sealEdit(node, base, edits)` calls `tree.editUpload(node, base, edits, {sink})` with the same sink and op building as `sealWrite`.
    * So a flush fetches and seals only the touched pages and the chunks around each edit (8.3).
  * **The form:** the base's form is kept (`editUpload` and `sealWrite`'s `previous`); a new file takes the fs policy (CDC by default). `sealWrite` no longer passes `chunking: 'fixed'`.
* **The handle drops `chunkSize` and `index`.** `Replica.chunkSize()` goes too, since `open` was its only caller.

**`queueFlush` and the `close({wait})` fix:**
* `enqueue` returns once the entry is stored and shown, with a promise for its send.
* `queueFlush` takes the local version from the overlay before it awaits that promise.
* `flush({wait})` and `close({wait})` then resolve once the server has the write, and the handle stays usable after a `flush({wait})`.

**Index records: binary, per page, content-addressed** (store `index`):
* **The records:**
  * `0x70 ‖ page_id` → `u8 1 ‖ n × (chunk_id ‖ u32 len ‖ sha256)`, one per page.
  * `0x6d ‖ sha256(sealed manifest)[0..16]` → `u8 1 ‖ n × sha256`, for an inline version (ids and lengths are in its manifest).
  * Neither depends on the dot, so they're written when the content is sealed, valid for the local version and the sent one alike, and nothing is rewritten on `done`. An edit writes records only for the pages it made.
* **Written** from `Upload.index` when it has hashes (writes from bytes: `writeFile`, whole rewrites, new files), split by the manifest's pages (v3 `count`; v2 `pageSize`, the last page short). An edit's upload has no hashes (pages taken over unread), so it writes none.
* **`indexOf(node, v)`:**
  1. A legacy JSON record (`node ‖ dot`), or a legacy local version's `index`, as before.
  2. Else the per-page or inline records, when every page has one: ids, lengths and hashes, with the form, keys and `pages` from the manifest.
  3. Else, as today, one full read (`tree.chunkIndex`), whose records are then written; offline, undefined.
* **`done`** writes no index records.

**Queue entries:**
* New entries no longer carry `QWrite.index` (the records hold it). Entries with it still load, and their index is used as before.
* Content arrays stay in entries: a handle's entry holds at most the dirty bound of new content, plus what its edit re-cut.
* Old JSON index records and v1 entries load unchanged.

**`ReplicaOptions`:**
* `dirtyBytes` (default 64 MiB)
* `cdc` (passed to the tree as `TreeOptions.cdc`, so small-`max_value_bytes` tests and servers can cut by content)

### zen-mount `--integrity` (TD-FUSE-INTEGRITY)

* **Setup:**
  * **Pins:** `--integrity` needs `--vault` or `--cache-dir`, else it exits 1 ("pins kept in memory would trust whatever the server shows at each mount"). The pins go in the vault's store when there is one (with the device key, as INTEGRITY.md §7 advises), else in the cache dir's.
  * **Signer:** `--device-key` (its device seed and cert), else `vault.signer()`. Without either, it verifies only, with a line saying so.
  * **Attaching:** on the one `Fs` object that `unlock` then unlocks (`session.fs()` makes a new object each call). `onAlarm` writes `zen-mount: integrity alarm: <kind> (<object>): <detail>; every operation now fails with EIO` to stderr.
* **Failing closed:**
  * **`MountOptions.integrity?: Integrity`:** every FUSE operation first checks `integrity.trusted`; once it's false, the operation answers `EIO`. Unmounting still works.
  * **At startup:** an alarm while attaching, unlocking or in the first sync prints the alarm and exits 3. No mount is made, as the Rust CLI does.
  * **Mounted:** the replica's feed raises the alarm, the mount fails closed, and `stop` exits 3.
* **Stopping:** unmount, drain, `replica.close()`, then `integrity.close()` (the last checkpoint; none once untrusted), then `ufs.close()`.
* **`HELP`** gains the flag.

### Docs

* **CLIENT.md:**
  * §8: the replica's handles (the bound, flushes by edit, CDC by default, `dirtyBytes`, partial versions while writing)
  * §9: `zen-mount --integrity`
* **INTEGRITY.md:** §5 (zen-mount turned on) and §8 (zen-mount fails closed: EIO, exit 3)
* **TECH_DEBT:** TD-FUSE-INTEGRITY resolved
* **The plan log,** in both copies

### Tests

**`@zen/fs`, new `test/handles.test.ts`:**
1. **The memory bound:**
   * The setup: a disk store in a temp dir, `cacheBytes` 16 MiB, `dirtyBytes` 4 MiB, and the server's default `max_value_bytes` (CDC).
   * 128 MiB written through one handle in 128 KiB writes.
   * `dirtyBytes` never passes 4 MiB plus one write.
   * The peak RSS growth stays under 96 MiB, sampled at each write after a `gc()` where exposed.
   * Several entries were queued.
   * A fresh replica reads it back (SHA-256 compared).
2. **Random operations against a model, CDC and fixed:**
   * The setup: `max_value_bytes` 8,192, `cdc` `{256, 10, 8,128}` (8.3's parameters), `dirtyBytes` 64 KiB, so files of about 2 MB are paged and spill.
   * 300 random writes, truncates (down and up), appends, reads and flushes, over handles reopened at random.
   * After each flush, the file reads back through a second replica and `tree.read`.
   * Run once under the default policy and once with `setPolicy({chunking: 'fixed'})`.
3. **An edit costs the pages it touches:**
   * The setup: a paged v3 file.
   * A 1-byte overwrite mid-file through a handle queues at most 2 pages. Its new chunks are exactly those whose bytes changed as the fs's chunker cuts them (as `cdc.test.ts`).
   * A 1-byte append queues 1 page and the re-cut tail.
   * No `chunks/get` beyond the touched chunks (calls counted).
4. **The queue survives a restart mid-way:**
   * The server is killed.
   * A write of 3 × the bound through a handle queues 3 entries offline.
   * The replica is closed without closing the handle (a crash: the in-memory tail is lost).
   * The server restarts and the replica reopens: the entries are sent, and the file is exactly the bytes up to the last spill.
5. **`close({wait: true})`:**
   * It resolves with the write on the server. Before the fix it threw reading `local` of undefined.
   * After a `flush({wait: true})`, the same handle writes and flushes again.
6. **Records:**
   * A `writeFile` after a `writeFile` of a paged file reuses its chunks from the per-page records, with no content read (no `chunks/get`).
   * After an edit, a `writeFile` reads once (incomplete records), then reuses.
   * An old JSON index record and a v1 queue entry with `index`, written raw into the store, still load and are used.
7. **Zeros:** a handle extended from 0 to 64 MiB by `truncate` and flushed holds no zero buffer (`dirtyBytes` 0), and reads back zeros.

**Updated:**
* `writes.test.ts`'s 8.3 case: a v3 file edited through the replica now stays v3, with only touched pages queued.
* The replica suite runs unchanged otherwise.

**zen-mount (`mount.test.ts`):**
* **In-process, "a rolled-back tree fails the mount closed":**
  * The fork setup of `integrity.test.ts`: a write and checkpoint on `self`, verified there.
  * Bob's replica on the copy is mounted with `integrity`. Its `sync` raises the rollback.
  * `readFile`, `readdir`, `stat` and a write then give `EIO`, where they gave the stale content before the alarm; unmount works.
* **The CLI (a world of its own):**
  * `--integrity` without `--vault` or `--cache-dir` exits 1 with the message.
  * With `--vault`, after a fork: a mount on `self` writes a file and stops (exit 0), and the device's checkpoint head is on the server (written at close).
  * A mount on the copy, with the same vault and cache, exits 3 and prints a `rollback` alarm.
* **"A 1-byte write through the kernel sends one chunk and one page"** keeps its exact count with its fs set to `fixed`. The CDC count is test 3 above.

**Negative controls** (each run once, then restored):
* the bound off (`dirtyBytes` Infinity): test 1 fails (`dirtyBytes` and RSS)
* every flush a whole rewrite (no edits): test 3 fails (all pages queued)
* the `trusted` check off in `mount.ts`: the in-process fuse test fails (stale reads succeed)
* the old `queueFlush`: test 5 fails
* the per-page lookup off: test 6 fails (a `chunks/get`)

### Files

* **Spec:** `spec/fs.md` (§11.1)
* **Client:** `packages/client/src/{tree,writer}.ts` (`Edit.zeros`) and a case in `cdc.test.ts`
* **`@zen/fs`:**
  * `packages/fs/src/{handle,replica,queue,index}.ts`
  * `packages/fs/test/{handles (new),writes}.test.ts`
* **zen-mount:** `packages/fuse/src/{cli,mount}.ts`, `packages/fuse/test/mount.test.ts`
* **Docs:** `docs/CLIENT.md`, `docs/INTEGRITY.md`, `spec/TECH_DEBT.md`, `docs/MILESTONE-8.md`

### Verification

* `npm run check` (biome and every tsc project); `npm run build` (the CLI tests run `dist/`)
* **Suites:** `handles` 3× and `writes`/`replica`/`integrity` once; `mount` (FUSE, in-process and CLI); client `cdc`, `tree` and `large` once; then the full `npx vitest run`
* **Negative controls:** the five above
* **CI:** green on both runs

**Defaults chosen (overridable):**
* **The bound:** 64 MiB per handle (`dirtyBytes`). Past it the handle queues a write (decided).
* **A whole rewrite reuses by hash** and reads the base once when it has no complete records, as `writeFile` does today; an edit never reads beyond its pages.
* **Content arrays stay in queue entries,** since the bound limits them, rather than new content-list records (a change from the milestone plan's "entries name content lists").
* **zen-mount:** an alarm at startup exits 3 without mounting; pins are kept in the vault, else the cache dir, and one of them is required.
* **TD-FUSE-INTEGRITY is resolved in this step,** with the code, not in 8.7.

## Step 8.5 in detail

Step 8.5 lets a Loro document's snapshot be larger than one commit: past a threshold, compaction stores it as a zen-fs file. The `$loro` row then names the file's version instead of holding the bytes, and loading reads the snapshot from that file (TD-LORO-LARGE-SNAPSHOT). The change is in `@zen/loro` and the spec, with no Rust, wasm or zen-db change.

**Survey:**
* **`packages/loro/src/loro.ts` (552 lines):**
  * `writeSnapshot(explicit)` exports a snapshot. One transaction reads the `$loro` row and, if its offset is older, puts `{name, offset, snapshot}`.
  * On `too_large` it warns once (the TD), sets `noCompact` and keeps the old row; `compact()` rejects with `too_large`.
  * `catchUp()` imports `row.snapshot` when the row's offset is newer than the kept one, then reads the tail and subscribes.
  * `isTooLarge` and `isNetwork` classify errors.
* **zen-db has what is needed:**
  * A `Db` carries its `UnlockedFs` (`db.fs`), so `db.fs.tree(id)` gives a `Tree`. It also carries `db.limits` and `db.features`.
  * `tx.fs(tree)` (`packages/db/src/files.ts`) adds a tree's ops and an uploaded file's `write` to the transaction's commit (zendb.md §7.6).
  * kanban's `attachFile` is the pattern: `tree.batch().create(parent, name)` builds the node first, so the upload is sealed for it; `tree.upload(node, data)`; then one transaction carries `ops(create)`, `write(up, {replaces: []})` and the row.
* **Rows take nested objects** (`cbor.ts` `Value`), so the row can hold the file's reference.
* **The client (8.3):**
  * `upload(…, {session: true})` keeps the content past the grace period until `closeUpload(session)`.
  * A version's dot is assigned by the server at commit, so a row written in the same commit can't name it. A `FileVersion` carries its `sealedManifest`, though, and the upload has the same bytes (`Upload.sealedManifest`).
  * `TreeBatch.remove(node)` moves a node to `TRASH`, and the sweeper purges it and releases its content.
* **Tests** (`packages/loro/test/loro.test.ts`, 467 lines):
  * a world with `claim_ttl_ms` 1000
  * `installation()`, `open()`, and `net(s)`, which wraps `session.call` to count commits, events read and row reads, and to go offline
  * "a snapshot too large for a commit" runs on a world with `max_commit_bytes` 64 KiB, types 100 × 512 random hex characters, and expects `too_large` with one warning

### Spec (`spec:` commit)

**zendb.md §20.1, "Snapshot":**
* The `$loro` row holds `{name, offset, snapshot}` (inline) or `{name, offset, file}`.
* `file` is `{tree, node, manifest, size}`:
  * `tree` and `node` are 16-byte ids
  * `manifest` is the SHA-256 of the version's sealed manifest (32 bytes)
  * `size` is the snapshot's length
* The row is sealed and authenticated like every row, and the hash binds it to one version. A server can't serve the row with another version of the file, an older one included, without the reader noticing.

**§20.2, step 2 (loading), for a file row:**
1. Read the node's versions, and take the one whose sealed manifest hashes to `manifest`.
2. Read it whole (`Tree.read`) and import it.
3. **When no version matches**, read the row again: a compaction may have replaced the file between the two reads. If the reread names another version, go back to step 1.
4. **When the reread names the same missing version**, warn and go on without the snapshot: from the kept offset if there is one, else the whole log. That is slower, but complete.

**§20.5 (compaction), "How":**
* **The threshold.** A snapshot of at most `max_commit_bytes / 2` (the threshold) is written inline, as before. A larger one goes to a file, and so does an inline write that fails `too_large`.
* **The file:**
  * **Which file.** The row's file's tree and node when it has one. Otherwise a new tree of the document's own, with the node created at its root under the name `snapshot`.
  * **The upload.** Uploaded before the transaction, under an upload session when the server has `upload_sessions` (fs.md §4.1), so a slow upload outlives the grace period.
  * **One transaction** reads the row. Only if the row's offset is older than the client's does it write three things: the `create` (for a new node), the file's `write` replacing the node's current versions, and the row `{name, offset, file}`. The old version goes in the same commit, so the node has one version at a time.
  * **The session** is closed after the transaction, whether it wrote or not. An upload that wasn't written (a newer row, or an error) is released one grace period later.
* **Switching form:**
  * **From a file to inline:** when a row with a file is replaced by an inline one, the same transaction removes the file's node (to `TRASH`).
  * **Another client's node:** when the row read in the transaction names a node other than the one this client uploaded to, the transaction removes that node and writes its own. This happens when two clients make a document's first file at once and the other one committed first.
* **Two compacting clients** still need no leader. The row read conflicts, and the re-run either finds nothing to do or rewrites with the node's versions read again.

**§20.5, "Too large":**
* Only a snapshot past what one file holds (fs.md §4.1) is too large now.
* The rest of the paragraph stays: the old row is kept, the client warns once and stops compacting, and `compact()` fails `too_large`.

**§20.6 (leakage):**
* For a file row, the server sees the document's snapshot tree.
* It sees the file's size and chunk lengths, as for any file (fs.md §10).
* It sees that one commit carries the row and the file's write.
* That is what an inline row's size already showed, plus the tree id, which links the document's compactions to each other. They were linked already through the row's key.

**Older clients:**
* **Loading.** A client from before this step finds no `snapshot` in a file row, so it loads from its kept offset or the whole log: correct, just slower.
* **Compacting.** If it writes an inline row over a file row, the file's node stays behind (not removed). The spec notes that as harmless but wasted storage.

### `@zen/loro`

**`LoroOptions.fileBytes?: number`:** the threshold, in snapshot bytes. It defaults to `max_commit_bytes / 2` (from `db.limits`). Tests set it to force either form.

**`writeSnapshot`:**
1. Export the snapshot.
2. If it is at most the threshold, run the inline transaction as now. If that fails `too_large`, go on as for a file.
3. **For a file:**
   1. Read the row outside the transaction (`db.table('$loro').get`). If its offset isn't older, stop.
   2. **Pick the target:** the row's `file.tree` and `file.node`, or a new tree (`Tree.newId()`) and a node made by `tree.batch().create(ROOT, 'snapshot')`, keeping the batch's ops.
   3. Upload with `tree.upload(node, snapshot, {session})`. `session` is true when `db.features` has `upload_sessions`.
   4. **Run the transaction:**
      * Re-read the row; if its offset isn't older, return.
      * If it names a node other than ours, `tx.fs(theirTree).ops([remove])`.
      * If our node is new, `ops(create)` and `write(up, {replaces: []})`.
      * Otherwise `write(up)`, which replaces `'current'`, read in each run.
      * Put `{name, offset, file: {tree, node, manifest: sha256(up.sealedManifest), size}}`.
   5. In a `finally`, `tree.closeUpload(session)`, ignoring errors.
4. **An inline write over a file row** adds `tx.fs(tree).ops([remove(file.node)])` in the same transaction.
5. **Errors:**
   * `too_large` from the file path (past a file's capacity) warns once, as now, with the message updated.
   * Other errors are handled as now: rejected for an explicit `compact()`, warned otherwise, and silent offline.

**`catchUp`:**
* An inline row is imported as now.
* **A file row** whose offset is newer than the kept one goes through `loadFile(row)`, which follows §20.2's steps:
  1. `tree.versions(node)`, then the version matching the hash.
  2. `tree.read(node, {version})`, checked against `size`, then `doc.import`.
  3. If none matches: one reread of the row, and a retry if it names another version.
  4. If the reread names the same missing version: a warning, and the row's snapshot is skipped; `snapOffset` still moves.
* A read that fails with `not_found` (the version was purged while being read) counts as no match.

**Unchanged:**
* The kept state (`d ‖ name`) still holds the whole snapshot on the device.
* The outbox, sending, and the tab behavior are as before.

### Docs

* **docs/DB.md (Loro limits):** large snapshots go to a file, with `fileBytes`. Only a single update too large for a commit is still dropped.
* **spec/TECH_DEBT.md:** TD-LORO-LARGE-SNAPSHOT resolved, by the 8.5 commits.
* **The plan log,** in both copies.

### Tests (`packages/loro/test/loro.test.ts`)

The "too large" describe becomes "a snapshot larger than a commit (§20.5)". Its world gets `max_commit_bytes` 64 KiB and `max_value_bytes` 16 KiB, so a snapshot of about 100 KiB is several chunks and commits; CDC falls back to fixed chunks at that value size. Each case uses a namespace of its own.

1. **The old "too large" test now compacts:**
   * The same 100 KiB of random text: `compact()` resolves.
   * The row has `file` and no `snapshot`, and its offset is the handle's.
   * The node has exactly one version, whose sealed manifest hashes to `file.manifest`.
   * No `TD-LORO-LARGE-SNAPSHOT` warning, and `tree.uploads()` is empty (the session was closed).
2. **A load is the file snapshot plus the tail:**
   * After the compaction, 5 more edits are flushed.
   * A fresh installation's load (counted with `net`) reads only those 5 events, and its document equals the writer's.
   * A second compaction replaces the version: still one version on the node, and the row names it.
3. **Two owners compacting at once (file form):**
   * Two installations, both owners, compact the same document at once, both for a document's first file.
   * The row's offset is the larger one, and only the row's node holds a version. The other node is in `TRASH`, or was never created.
   * No session stays open for either.
   * A fresh load reads the document whole.
4. **A missing file version reloads the row:**
   * Installation A's `session.call` is wrapped (as `net` does) so that its first version listing waits while installation B compacts further, replacing the version A's row named.
   * A then finds no match, rereads the row, and loads B's version. Its document is whole, and it read only the tail after B's offset.
5. **A row naming no version** (a `$loro` row put by the test with a wrong `manifest`):
   * The load warns, skips the snapshot, reads the whole log, and the document is whole.
6. **The hash picks the row's version:**
   * The test writes a sibling version to the file's node (`writeFile` with `replaces: []`, a newer dot) holding another document's snapshot.
   * A fresh load imports the row's version, so the document is the writer's, without the other document's text.
7. **Back to inline:**
   * A handle with a large `fileBytes` compacts a file-form document.
   * The row is inline again, and the old node is under `TRASH`.
8. **A server without `upload_sessions`** (`db.features` with it removed): the file path uploads without a session and still compacts.

The other Loro suites run unchanged: inline rows, two installations, offline, resends and tabs.

**Negative controls** (each run once, then restored):
* **The newest version instead of the hash match:** test 6 fails (the other document's text appears).
* **`replaces: []` on every write:** tests 1 and 2 fail (two versions on the node).
* **No reread of the row:** test 4 fails (A reads the whole log).
* **The session left open:** test 1 fails (`uploads()` not empty).
* **No remove of the other client's node:** test 3 fails (two nodes with versions).

### Files

* **Spec:** `spec/zendb.md` (§20.1, §20.2, §20.5, §20.6)
* **Code:** `packages/loro/src/loro.ts`, and `packages/loro/src/index.ts` if the options type is re-exported
* **Tests:** `packages/loro/test/loro.test.ts`
* **Docs:** `docs/DB.md`, `spec/TECH_DEBT.md`, `docs/MILESTONE-8.md`

### Verification

* `npm run check` (biome, and every tsc project, `packages/loro/tsconfig.test.json` included); `npm run build`
* **Suites:** `loro` 3×; the zen-db `txn` and `files` tests, and kanban's attach test, once (shared paths, unchanged); then the full `npx vitest run`
* **Negative controls:** the five above
* **CI:** green on both runs

**Defaults chosen (overridable):**
* **One tree per large document,** made at its first file snapshot and named in the row, rather than one tree per database. All clients find it through the row, with no ids to agree on in advance.
* **The row names the version by its sealed manifest's hash,** since the dot isn't known before the commit.
* **The threshold is half of `max_commit_bytes`,** as the milestone plan has it, overridable with `fileBytes`.
* **No chunk reuse between snapshot versions:** each compaction uploads the whole snapshot. Reusing chunks would need the previous version's index kept per document; 8.7's STATS can measure whether that's worth it.
* **The kept state stays a single record of the whole snapshot.**

## Step 8.6 in detail

Step 8.6 adds two zen-ublk commands, `import IMAGE` and `export IMAGE`, that copy a raw disk image into a volume and back out. It measures an ext4 image kept as a zen-fs file (zen-mount + loop) against the same image as a volume, and turns the result into docs/BLK.md "Files or volumes". It also writes up TD-BLK-SNAPSHOT-FILES. There's no format change, so no `spec:` commit.

**Survey:**
* **`crates/zen-ublk/src/main.rs` (278 lines):**
  * clap `Cli {connect, vol, vol_pin, read_only, lease_ttl_ms, batch_blocks, idle_flush_ms, io_timeout_ms, command}`, with the commands `create SIZE`, `info`, `fuse DIR` and `ublk`
  * `options()` builds `VolumeOptions` (pin from `vol_pin_path`); `run()` connects (`ConnectArgs::connect` → `Connected {fs, integrity}`), and `integrity.close()` writes the last checkpoint
  * `parse_size` and `vol_pin_path` are in `lib.rs`
* **`zen-blk` `Volume` (`volume.rs`):**
  * `read(offset, buf)` fills absent blocks with zeros, and fetches no block the tree shows absent (`plan.absent`), only the nodes above it.
  * `write` buffers blocks and flushes itself once `dirty ≥ batch_blocks`. `discard` buffers `None` per block, so a discard over a large range holds one map entry per block.
  * `flush` splits into commits of `per_commit()` blocks (private): `batch_blocks` within the server's limits.
  * `commit` drops deletes of blocks already absent, so a chunk of nothing else commits nothing. Each commit is one root record (`seq + 1`, `prev` = the old RH, `writer` = the device fp).
  * `idle_flush` spawns a timer that flushes on its own.
* **Tests:**
  * `tests/disk.rs` runs the in-process server, with no root needed.
  * `tests/devices.rs` (feature `device-tests`, root) runs the CLI against an in-process server on its own runtime. `bench_fuse` and `bench_ublk` (`#[ignore]`) run `bench_device` (mkfs, 256 MiB written with `conv=fsync`, a cold read, 256 synchronous 4 KiB writes, an untar and `sync`) on a 1 GiB volume.
* **zen-mount** (`packages/fuse`):
  * `fsync` and `flush` are `h.flush()` without `wait`: the write is queued locally (sealed into the store) and sent later. So an fsync through zen-mount is durable on the device, not on the server, unlike zen-ublk's.
  * The CLI takes the same device key file (`--device-key`, `{identity_seed, device_seed, cert}`) and `--new-tree`, and prints `mounted fs … on …` to stderr once mounted.
  * Neither the CLI nor the Rust client sets an fs policy, so a run under `chunking: fixed` needs a small `@zen/client` script.

### zen-blk (small additions)

* **`Volume::blocks_per_commit()`:** the existing `per_commit()`, made public.
* **`Volume::root_record() -> RootRecord`:** the record the tree holds (`seq`, `prev`, `writer`, …), for the import's resume check.

### `zen-ublk import IMAGE`

```
zen-ublk CONNECT --vol NAME import IMAGE [--create] [--progress FILE] [--restart]
```

**The image:**
* A regular file or a block device, read with `pread` at 4,096-byte-aligned offsets.
* Its size (seek to its end) must be a non-zero multiple of 4,096.
* **Without `--create`,** the volume must exist and be exactly the image's size. Otherwise the import exits 1 ("the image is N bytes and the volume M; sizes must match") without writing.
* **With `--create`,** the volume is created at the image's size (`exists` if it already does, unless a progress file resumes it).

**Writing:**
* **Opening:** the import opens the volume as its writer (the lease) with `idle_flush: None`.
* **Windows:** the image is copied in windows of `blocks_per_commit()` blocks (at most `--batch-blocks`, 1 MiB by default). Each window is buffered and flushed as **exactly one commit**, so each window moves the volume at most one `seq`.
* **Holes** in the image (`lseek(SEEK_DATA)`) are found without reading. All-zero blocks are found by reading.
  * **On a volume this import created,** both are skipped: the volume holds nothing there yet.
  * **On an existing volume,** they're discarded, window by window. Deletes of blocks already absent commit nothing, so the volume ends equal to the image.
* **A block device** has no holes to find, so `SEEK_DATA` failing just means every window is read.

**Resuming (by seq):**
* **The progress file:**
  * Default: the volume pin's path + `.import`; `--progress FILE` overrides it.
  * After each window it is rewritten (a temporary file, `fsync`, rename) with `{image: {size, mtime_ns, dev, ino}, created, offset, seq, rh}`.
  * It's removed when the import finishes.
* **On a start with a progress file,** the import continues from `offset` when both hold:
  * the image is the same (size, mtime, device and inode)
  * the volume's root is the recorded `(seq, rh)`, or exactly one commit past it: `seq + 1`, with `prev = rh` and `writer` this device. That's a crash after a window's commit and before its record.
* **Redoing a window:** the window from `offset` is written again, which is harmless since it's the same bytes.
* **Anything else refuses** (exit 1, nothing written), with the reason: "the image changed", or "the volume changed since the import stopped (seq …)". The message ends "--restart to start over".
* **`--restart`** ignores the progress file. On a volume it created, it then discards zeros as for an existing one.
* **Output:** progress goes to stderr every 64 MiB and at a resume ("resuming at offset X, seq S"). At the end, the volume is closed (a flush, a checkpoint, the lease released), and the root JSON is printed as `info` prints it.
* **Hidden `--stop-after N`** (for tests): exit 0 after N windows, leaving the progress file.

### `zen-ublk export IMAGE`

```
zen-ublk CONNECT --vol NAME export IMAGE [--force]
```

* **Read-only:** it opens the volume read-only (no lease), so it can run while a writer serves it. It reads the root it opened, checked against the tree as every read is.
* **The output** is a regular file (`IMAGE` must not exist, unless `--force` replaces it):
  * It's written as `IMAGE.part`, `set_len(size)` first, so every block starts as a hole.
  * It's read in windows of 256 blocks, and only blocks that aren't all zeros are written (`pwrite`). Absent blocks and blocks written with zeros stay holes, and absent blocks cost no fetch.
  * Then `fsync`, a rename to `IMAGE`, and an `fsync` of the directory.
  * It prints the root JSON of the root exported, so an image names the `(seq, root)` it holds.
* **When the writer moves the root** mid-export, reads of changed blocks fail `volume_changed`. The export then removes `IMAGE.part` and exits 1, with a message saying to stop the writer (or `fsfreeze` its filesystem) and retry. A consistent export of a live volume needs TD-BLK-SNAPSHOTS.
* **Block devices** as the target are out of scope: export to a file, then `dd` it. BLK.md says so.

### Tests

**New `crates/zen-ublk/tests/image.rs`** (no root; the CLI against an in-process server, with `disk.rs`'s world and `devices.rs`'s `cmd`):
1. **A sparse round trip:**
   * A 64 MiB image: random blocks at scattered offsets (a run at the start, single blocks, the last block), a run of blocks written as zeros, and holes between.
   * `import --create`, then the stored blocks (read with `VolumeKeys::block(i)`) are exactly the non-zero blocks.
   * `export` gives the same bytes. It's sparse: `SEEK_HOLE` finds the holes, and the allocated size (`st_blocks × 512`) is under the non-zero data plus 1 MiB.
2. **Into an existing volume:**
   * A 64 MiB volume filled with non-zero blocks through `Volume`, then `import` of the image from test 1.
   * The export equals the image, and the image's zero and hole blocks are now absent.
3. **Refusals:**
   * an image one block larger than the volume (exit 1, the message, the volume's `seq` unchanged)
   * an image of 64 MiB + 100 bytes with `--create` (no volume created)
   * `export` onto an existing file without `--force`
4. **Resuming:**
   1. **A clean resume:** `--stop-after 2` leaves the progress file at window 2 (a copy is kept). A resume with `--stop-after 1` reaches window 3.
   2. **A crash between the commit and the record:** the window-2 copy is put back (as if the process died after window 3's commit), and the next resume is accepted. It redoes window 3 (stderr names the offset), finishes, and the export equals the image.
   3. **Refusals:**
      * a volume written by another writer after a stop: refused, the message names `--restart`; `--restart` then finishes and the export equals the image
      * an image whose mtime changed: refused
5. **An export while the root moves:**
   * The export runs with a hidden `--pause-after N`: after N windows it waits until a file named by the test appears.
   * Meanwhile a writer `Volume` changes a block in a later window and flushes, and then the test creates the file.
   * The export exits 1 and leaves neither `IMAGE` nor `IMAGE.part`.

**`tests/devices.rs` (root), one new test, `image_import_serve_export`:**
* `mkfs.ext4 -d DIR` makes a 64 MiB image of 40 files without mounting it, and `import --create` copies it in.
* `fuse` serves it; with `losetup` and `mount -o ro,noload`, the files are checked; then it's mounted read-write, one file added, and stopped.
* `export`, then `e2fsck -fn` on the exported image is clean, and `debugfs -R 'cat /added'` reads the added file.
* The other device tests are unchanged.

**Negative controls** (each run once, then restored):
* **Zeros written, not skipped:** test 1 fails (stored blocks include the zero run).
* **No discard on an existing volume:** test 2 fails (the export shows the old blocks).
* **The resume check accepting any seq:** test 4's refusal fails.
* **No acceptance of `seq + 1`:** test 4's crash case fails (refused).
* **No `.part`:** test 5 fails (an `IMAGE` left behind).

### The comparison: an image as a file and as a volume

**The benches** (`tests/devices.rs`, `#[ignore]`, release, as root):
* **The server:** the existing `bench_world`, with `chunk_grace_secs` 0, so stored bytes can be read after a sweep.
* **New `bench_file_cdc` and `bench_file_fixed`:**
  * `zen-mount` (`node packages/fuse/dist/cli.js`, or `$ZEN_MOUNT`) runs with the writer's key file, `--new-tree`, `--foreground` and a `--cache-dir` in the temp dir.
  * `truncate -s 1G disk.img` in the mount, then `losetup -b 4096` on it, then the same `bench_device`.
  * For `fixed`, a small `node --input-type=module` script (inline in the test) first signs in with the key file and calls `fs.setPolicy({chunking: 'fixed'})`.
* **`bench_device` gains, for every frontend:**
  * the time from the stop signal to the daemon's exit (for zen-mount, the drain: everything sent to the server)
  * the serving process's peak RSS (`VmHWM`, read before the stop)
  * the fs's stored bytes after a sweep (`quota(1, "bytes")` read from the store), next to the 256 MiB file plus the untarred tree written
* **New `bench_image`:**
  * a 1 GiB ext4 image made with `mkfs.ext4 -d` (the source tarball unpacked, plus 256 MiB of random data)
  * `import --create`, then `export`, each in MiB/s of the image
  * the exported file's allocated size
* **Runs:** each bench runs twice here (this container, release), plus `bench_fuse` for a same-machine baseline. The figures go in BLK.md, with the machine named.

**docs/BLK.md:**
* **§2:** `import` and `export` in the usage block, and a "Moving images in and out" paragraph covering resuming, `--restart`, export while serving, and that a block device as the target is out of scope.
* **A new §8, "Files or volumes"** (Limits and systemd move to §9 and §10). It has the measured table and when to use which:
  * **A volume:**
    * an fsync is on the server when it returns
    * one writer, fenced by the lease
    * reads are checked per block against a Merkle root, with freshness through checkpoints
    * a fixed size, no history
    * use it for a filesystem a host mounts and syncs
  * **A file** (zen-mount + loop):
    * an fsync is durable on the device only; the server has it once the queue drains
    * it works offline
    * every flush is a version, so older ones read until collected
    * two writers become siblings, with no lease
    * any zen-fs client can fetch it
    * CDC and fixed chunks dedupe it against other files
    * memory is bounded by the handle's 64 MiB
    * use it to carry an image (a VM disk to hand around, a backup) more than to run a filesystem on it
  * **Fixed or CDC for images,** from the measured cost of a 4 KiB in-place write.
  * **The cost of an fsync** in each, from the bench (an edit's pages and chunks against one block and its path).
* **"Limits"** (now §9) gains "an export of a live volume fails if the writer moves the root".

**TD-BLK-SNAPSHOT-FILES (new, `spec/TECH_DEBT.md`):** volume snapshots written to zen-fs files.
* **Context:** `export` writes a raw image to the local disk. A snapshot kept in zen-fs, as a file of the fs, would be a server-side backup and a transport format.
* **For:**
  * readable without zen-ublk, by any zen-fs client (and mountable through zen-mount + loop)
  * deduplicated across snapshots by CDC or fixed chunks, so a second snapshot of a mostly unchanged volume stores only what changed
  * versions and the tree's own integrity (signed checkpoints) cover it
* **Against:**
  * a consistent snapshot of a live volume needs TD-BLK-SNAPSHOTS: a pinned root whose blocks the writer doesn't overwrite or delete meanwhile
  * zen-client (Rust) has no tree writer: one would be needed, or the snapshot made by a TS tool
  * it stores the data twice until the snapshot file is removed: the blocks are sealed per stored key, so the file's chunks can't share them
  * the volume's freshness (its root `seq` in checkpoints) doesn't carry over to the file: the snapshot file needs the root it came from recorded (in its name or meta), and a checkpoint entry linking the two, for a reader to know it is the volume as of that root
* **How it could open later:**
  * after TD-BLK-SNAPSHOTS, a `zen-ublk snapshot --to-tree TREE PATH` that pins a root, streams the blocks into a Rust `Tree::write` with fixed 4 KiB-aligned chunks (64 KiB, so in-place block writes keep chunk boundaries), and records `(vol_id, seq, RH)` in the file's meta
  * `import` taking a zen-fs file as its source
  * restoring is then `import` from the file

**Other docs:**
* **README:** the zen-ublk line names `import` and `export`.
* **The plan log,** in both copies.

### Files

* **zen-blk:** `crates/zen-blk/src/volume.rs` (`blocks_per_commit`, `root_record`)
* **zen-ublk:**
  * `crates/zen-ublk/src/{main.rs, lib.rs, image.rs (new: import and export, the progress file)}`
  * `crates/zen-ublk/tests/{image.rs (new), devices.rs}`
* **Docs:** `docs/BLK.md`, `spec/TECH_DEBT.md`, `README.md`, `docs/MILESTONE-8.md`

### Verification

* **Rust:**
  * `cargo fmt --check`
  * clippy `--all-targets -D warnings` on zen-blk and zen-ublk (default, `--features device-tests`, `--features ublk` if libclang is here)
  * `cargo test -p zen-blk`; `cargo test -p zen-ublk` (with `image` 3×)
* **The device tests** as root (`--features device-tests`): all of them, including the new one.
* **The benches,** after pruning `target/` for the release build: `bench_fuse`, `bench_file_cdc`, `bench_file_fixed` and `bench_image`, twice each, with `npm run build` first for `dist/cli.js`.
* **Negative controls:** the five above.
* **CI:** green on both runs. The `devices` job runs `image_import_serve_export`, and the `rust` job runs `tests/image.rs`.

**Defaults chosen (overridable):**
* **One commit per import window,** so a resume is decided by `seq` alone (the recorded one, or one past it by this device).
* **Zeros skipped on a volume the import created, discarded on an existing one.**
* **Export is read-only and fails if the root moves,** rather than taking the lease, so it never stops a writer. A consistent live export waits for TD-BLK-SNAPSHOTS.
* **Export writes regular files only.**
* **The comparison runs in this container,** with the figures in BLK.md; STATS gets them in 8.7, with the CI bench job.

## Step 8.6, continued: the replica's uploads under sessions, and zen-mount

The file benches found three things outside zen-ublk (see the 8.6 log). The user chose to fix the replica's uploads in this step. The work below goes on the same branch and PR (#71), after the work already done.

### 1. The replica's sender uploads under an upload session (`@zen/fs`, `sender.ts`)

**The problem:**
* `Sender.upload(entries)` puts a batch's chunks, then its pages, in commits of their own, and then `once()` sends the write commit.
* Nothing references that content until the write lands, so a slow upload (larger than the grace allows, or slowed by the network) can lose its early chunks to the grace sweep. The page commit then fails `unknown chunk in page`, or the write fails.
* Every later queued entry that edits the refused local version is refused in turn.

**The fix:**
* **Opening:** when the server has `upload_sessions` and the batch has content, `upload` opens a session (`/v1/fs/upload/open`, the server's default ttl).
* **Using it:** every content commit carries `upload: {fs, session}`, renewed between commits once half the ttl has passed, as `writer.ts` does.
* **Closing:** the session is closed after the write commit's outcome is final, whether written or refused. A close that fails is ignored, since the session then expires.
* **Across a restart:** the session is kept in the stored commit in flight (`Inflight.session`, optional, so old records still load).
  * A restart renews it before uploading again. On `upload_gone` it opens a new one and uploads everything again (the puts are idempotent).
  * `uploaded` (in memory) is still what skips a second upload within one process.
* **Without `upload_sessions`:** as today.
* **Code reuse:** `Tree` gains `openUpload(ttlSecs?)` and `renewUpload(session, ttlSecs?)`, next to `closeUpload`. `writer.ts`'s `sessionCall` uses them too.
* **Cost:** each put under a session costs `SESSION_COST` (64 B) more toward `max_commit_bytes`, so the sender's packing in `upload` counts it.

**Test (new case in `packages/fs/test/writes.test.ts`, or `handles.test.ts`):**
* **The setup:** a world with `chunk_grace_secs: 1`, `sweep_interval_secs: 1` and a small `max_commit_bytes`, so a 2 MB file's queued write takes several content commits.
* **The delay:** the replica's `session.call` is wrapped so that, once the first content commit has applied, the next commit waits 3 s (past the grace, with sweeps running).
* **The checks:**
  * the write lands, and the file reads back on a second replica
  * a later queued edit of the same file also lands
  * `tree.uploads()` is empty afterwards: the session was closed
* **A restart case:** the replica is closed after the content commits and before the write. It's reopened past the grace, and the write still lands, because the session was renewed from `Inflight.session`.
* **Negative control:** with `upload` left out of the content commits, the test fails (`unknown chunk in page`).

### 2. zen-mount without fuse-native's per-operation timeout (`packages/fuse/src/mount.ts`)

* `timeout: false` (done, uncommitted). Without it, an operation over 15 s (the flush of a large file, a read waiting on the network) answers `ETIMEDOUT` while its work goes on.
* **Test:** an in-process mount (`mount.test.ts`) whose replica's `session.call` delays the commit of a flush by 16 s. `close` (via `fs.closeSync` in a worker, or `fs.promises` with the handle) succeeds, where it gave `ETIMEDOUT` before.
* **Negative control:** without `timeout: false`, the test fails with `ETIMEDOUT`.
* **Cost:** about 16 s added to the mount suite.

### 3. A sparse file's zeros (no code change in 8.6)

* The first flush of a file that `truncate` grew writes its zeros as content, so a 1 GiB sparse image file costs 1 GiB sealed, uploaded and stored. CDC cuts long zero runs at `max`.
* The benches measure it, and BLK.md "Files or volumes" says so: an image kept as a file stores its whole size, and a volume stores only what was written.
* **New TD-FS-SPARSE:** holes in zen-fs files (a zero-extent entry in v3 manifests, or a shared all-zero chunk per key), and the replica's handle keeping a hole as one. A format change, out of place here.

### Then, as planned

1. **The file benches**, with the default grace. Stored bytes are read after the stop: the server holds superseded versions' chunks until their grace passes, and the table says so. With sessions, a short grace would also work; the default keeps the benches realistic.
2. **`bench_image`, `bench_fuse` and `bench_ublk`** (ublk only where `/dev/ublk-control` exists; not in this container).
3. **The docs:**
   * BLK.md §2, the new §8 and the limits
   * TECH_DEBT: TD-BLK-SNAPSHOT-FILES and TD-FS-SPARSE
   * README
   * CLIENT.md §8–§9: the replica's sender under sessions, and zen-mount without the timeout
4. **The log** in both copies; then commit, push and retitle PR #71.

### Commits

1. 98c151c (the device tests' lease; done)
2. `ublk: import and export of raw images (milestone 8 step 6)`: zen-blk, zen-ublk, tests, ci.yml
3. `fs: the replica's queued uploads under an upload session; zen-mount without the per-operation timeout`: `@zen/client` (`openUpload`/`renewUpload`), `@zen/fs` sender, `@zen/fuse`, tests
4. `docs: files or volumes (milestone 8 step 6)`: BLK.md and the bench figures, TECH_DEBT, README, CLIENT.md, the log

### Verification (added to 8.6's)

* `npm run check`; `npm run build`
* **Suites:** `@zen/fs` `writes`, `handles`, `replica` and `integrity`; the client `cdc`; `mount` (FUSE); each new test 3×; then the full `npx vitest run`
* **Negative controls:** the two above
* **The file benches** run to completion: no refused writes in zen-mount's stderr (the bench fails on a `queued writes refused` line)
* **CI:** green on both runs

## Step 8.6b in detail: zen-mount's memory under the image workload

**The problem:** in 8.6's file benches, zen-mount (fixed and CDC) peaked at 1,544–1,822 MiB RSS (`VmHWM`) while serving an ext4 image through loop. A `FileHandle` holds at most 64 MiB dirty, and the volume frontend took 62 MiB under the same workload. Nobody looked into why.

**The intended outcome:**
* the cause found by measurement, not guessed
* fixed in `@zen/fs` or zen-mount (or the client's local store, if that's where it is), with a test and a negative control
* the figure re-measured
* RAM recorded as a first-class cost in the docs (the user's note)

### Survey (read so far)

* **The handle has backpressure:** `write` and `truncate` await the current flush (`handle.ts:150–224`). Past the bound, `spill` awaits a flush, so the kernel's writes wait. So the handle alone can't hold more than about 64 MiB plus one flush.
* **The sender is bounded:** `Sender.upload` (`sender.ts:207`) reads one put at a time from the records and sends at most `max_commit_bytes` (8 MB) at once.
* **The records' write-behind has no backpressure. This is the first suspect.**
  * `Records.put` (`store.ts`) adds to `pending` and schedules a flush every 100 ms.
  * `flush()` moves the batch into a promise chain (`this.flushing.then(…)`), which writes its records one at a time.
  * zen-mount's cache dir is `nodeDiskLocalStore`, whose `put` is `writeAtomic`: a temp file, `writeFile`, `fsync`, `close` and a rename, per record.
  * Each sealed chunk costs one content record plus an `lru` use record (`Content.use`), and reads add `lru` records too.
  * If records arrive faster than one fsync each can drain them, the batches wait in the chain with their values, so memory grows with the backlog. Nothing makes a handle's flush or the kernel wait for it.
  * The sealed store adds a copy per value (AES-GCM through WebCrypto).
  * The 447 writes left unsent at stop show the queue was far behind, which fits.
* **Other candidates, to be ruled in or out by the measurement:**
  * eviction: `Content.evict` lists every `lru` record, reading each file, at every `cacheBytes / 16` added
  * the client's page memo (256 pages, at most about 23 MB)
  * wasm linear memory, which never shrinks
  * fuse-native's write buffers, plus the handle's copy of each
  * the sender's in-flight record (`Inflight.pending`, the batch's ops as JSON)
  * the overlay and the queue entries in memory

### 1. Measure (no repo change)

**The run:** `bench_file_fixed` (the 1,822 MiB case, about 4 min in release), with zen-mount started through `NODE_OPTIONS`. The bench runs `node CLI`, and `Command` passes the environment through, so nothing in the repo changes for the profiling.
* `--require $SCRATCH/memlog.cjs`: a preload that writes `process.memoryUsage()` (`rss`, `heapUsed`, `external`, `arrayBuffers`) every 500 ms to a log under `target/`
* `--heapsnapshot-signal=SIGUSR2`: the bench's runner script sends it at about 1 GB RSS and again near the peak
* `--max-old-space-size` left alone

**Splitting the bytes:**
* **JS heap against ArrayBuffers** (`external` and `arrayBuffers`) against the rest (wasm memory, native).
* **Retainers:** a small script in the scratchpad parses the `.heapsnapshot` (nodes, edges and the retainer path of the largest ArrayBuffer and Uint8Array owners, summed by path). It gives the top retainers by retained size.
  * If they're `Records.pending` or the closures in the `flushing` chain, the first suspect is confirmed.
* **A cross-check:** the same run with the cache dir on tmpfs. Fsyncs then cost nothing, so a backlog peak would drop sharply if write-behind is the cause.

**What gets logged:** the breakdown, the top 5 retainers, and the tmpfs figure, all in this log. The fix's design goes here before any code, and the code waits for the user's "go".

### 2. Fix (the likely shape, if the write-behind is confirmed)

* **`Records` gets a bound on what's pending:** bytes waiting to be written, counted over `pending` and the batches in the chain. The default is 32 MiB, and `ReplicaOptions.pendingBytes` overrides it.
* **Callers wait once past it:** a new `Records.room(): Promise<void>` resolves once the backlog is under the bound. Puts stay synchronous, since many call sites rely on that.
  * Content sealing awaits it (the sinks in `sealWrite`/`sealEdit`, and the client cache's `put` via `Content`), so a handle's flush slows to what the disk takes.
  * The kernel's writes then wait through the handle's existing backpressure.
* **Fewer fsyncs, if the measurement shows they're the bottleneck:**
  * The `lru` use records are coalesced: one per id per flush (already true through the `pending` map). Hits on content that was just written don't rewrite its record within a few seconds.
  * `nodeDiskLocalStore` gets a way to write a batch with one directory fsync rather than one fsync per file? That's a change in `@zen/client`'s local store. It's considered only if the bound alone leaves throughput too low, and if so it's described here first.
* **If the cause is something else** (eviction, wasm, fuse-native): the fix is designed here from the retainers, under the same rule. If it's local to `@zen/fs` or zen-mount, fix it; otherwise investigate further before deciding.

### 3. Tests

* **New `packages/fs/test/memory.test.ts`** (or a case in `handles.test.ts`):
  * a replica over a deliberately slow store: a `LocalStore` wrapper whose `put` takes 5 ms
  * 256 MiB written through one handle in 128 KiB writes
  * the replica's pending bytes (a getter for tests) never pass the bound plus one record
  * RSS growth past the middle stays bounded, as the handles test measures it (quarter medians, `gc()` exposed)
  * the file reads back whole on another replica
* **Negative control:** with the bound off (`pendingBytes` Infinity), the test fails: pending grows to most of the file, and RSS grows linearly.
* **The existing suites:** `@zen/fs` (all), client `cdc` and `mount`, then the full `npx vitest run`.

### 4. Re-measure and document

* **The re-runs:** `bench_file_fixed` and `bench_file_cdc`, with peak RSS, throughput (the bound may slow writes to the disk's pace), stop time and unsent writes.
* **BLK.md §8:** the table updated, the "not investigated" line replaced with the cause and the fix.
* **RAM as a first-class cost (the user's note), in the docs:**
  * **spec/OPS.md, a new §6 "Memory":**
    * Every buffer, queue, cache and backlog has a bound and a figure.
    * Producers wait on slow consumers, never buffer for them.
    * How to measure: `VmHWM`, the `memoryUsage` split, heap snapshots via `NODE_OPTIONS`, and the retainer script's method.
    * What never shrinks (wasm memory).
  * **spec/TECH_DEBT.md, new TD-MEMORY:**
    * the known hot spots and their bounds: the handle (64 MiB), the records' backlog (new), the page memo, wasm memory, the server's commit buffers, and the Loro kept state (a whole snapshot in memory)
    * those not yet measured
    * kept open as a standing list
  * **docs/MILESTONE-8.md:** 8.7's memory figures for the replica, zen-mount and the server become a required part of 8.7, not an extra.
* **The log** in both copies.

### Branch, PR and commits

* **On approval:**
  1. restart from main (e4b11ca)
  2. `cp` this file to `docs/MILESTONE-8.md` (bringing in the 8.6 log lines)
  3. commit `docs: milestone 8 step 6b plan (zen-mount's memory)`
  4. push and open the PR
  5. run step 1, the measurement, right away: it changes nothing in the repo, and its findings go in the log
* **On "go":** the fix, tests and docs:
  * `fs: bound the replica's pending records (zen-mount's memory)`
  * `docs: memory (milestone 8 step 6b)`

### Verification

* `npm run check`, `npm run build`
* the new test 3×, with the negative control run once
* the `@zen/fs` suites, client `cdc`, `mount`, and the full vitest run
* **The benches:** `bench_file_fixed` and `bench_file_cdc` in release, as root. Clear the bench temp dirs between runs, and keep logs and snapshots under `target/` (snapshots can be GBs: delete them after analysis).
* **CI:** green on both runs.

## Step 8.7 in detail: benches, docs, outcome

Step 8.7 is the milestone's last step. It measures what 8.1–8.6b built, at multi-GiB sizes. Memory is part of every figure, as OPS.md §6 and TD-MEMORY require: the client, the replica, zen-mount and the server. Then it writes the docs and the Outcome.

There's no format change and no `spec:` commit. The code is the benches and a CI job, plus whatever the benches find wrong. A small, local fix goes in with a test and a negative control. Anything larger goes to the user first.

**Survey:**
* **The harness** (`packages/client/src/testing/server.ts`):
  * `spawnServer`/`world()` give `pid` (for `/proc/PID/status`), `dataDir`, `limits`, and `extra` TOML. A `[storage]` table with `backend = "fdb"` and `cluster_file` selects FoundationDB.
  * `ZEN_SERVE_BIN` picks the release binary.
* **vitest uses `pool: 'forks'`,** so each file runs in its own process, and `/proc/self/status` `VmHWM` is that bench's alone.
* **The opt-in bench pattern** (`integritybench.test.ts`, `loro/test/bench.test.ts`): `describe.skipIf(!process.env.ZEN_BENCH)`, and markdown table lines printed at the end. 8.0's 256 MiB probe wasn't committed.
* **What gets measured:**
  * `Tree.writeFile`/`upload` (`session`, `parallel`, `onProgress`, `resume`), `readStream` (`readAhead`), `read`, `edit`, `append`, `uploads`
  * `@zen/fs`: `openReplica`, `FileHandle`, `status()` (`pendingBytes`, `cacheBytes`), `sealedStore(nodeDiskLocalStore(dir))` (zen-mount's store)
  * zen-mount: `packages/fuse/dist/cli.js`, run as `mount.test.ts` runs it
* **The TS client has no quota read,** so stored bytes are the data dir's size on disk after a sweep (`du`), beside the logical figure computed from the formats.
* **CI:** `devices-bench` (`workflow_dispatch` only) is the template. No TS bench job exists.
* **Here:** 27 GB of disk free, no FoundationDB installed (`scripts/install-fdb.sh`). STATS has §1–§6, so §7 is new.

### 1. The client bench (`packages/client/test/largebench.test.ts`, new)

**Running it:**
* Started with `ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve npx vitest run packages/client/test/largebench.test.ts`.
* The size is `ZEN_BENCH_GIB` (4 by default).
* Before anything else it checks free disk under the temp dir. It refuses to start below 3 × the size, naming what it needs (8.6b's ENOSPC).

**The source:**
* AES-128-CTR keystream over zeros (`node:crypto`). It's random-looking, so it's not periodic and CDC resyncs, and it's fast.
* It's seekable (counter = offset / 16), so a resume can restart the source at any offset.
* Read-backs compare SHA-256 computed while streaming. No file is kept on disk.

**The world:**
* The release server with `chunk_grace_secs` 5 and `sweep_interval_secs` 1, so superseded content goes quickly and a resume happens past the grace.
* One world per form: CDC (the default), then `fixed` (`setPolicy`). Each is stopped and its data deleted before the next.

**Per form:**
1. **Write:** `writeFile(…, {session: true})` from the stream.
   * MiB/s and commits
   * the last `UploadState`'s JSON size (it should stay compact)
   * the client's peak RSS, and its `memoryUsage` split (sampled every 200 ms)
   * the server's `VmHWM`
   * stored bytes after a sweep
2. **Cold read:** `readStream` of the whole file at `readAhead` 3, and again at 1, in MiB/s, with the hash checked.
3. **Random reads:** 1,000 reads of 4 KiB at random offsets, median and p99 ms. First on a fresh `Tree` (cold page memo), then again (warm).
4. **Append** 1 MiB (`tree.append`): ms, plus pages and chunks fetched and sent, counted through `session.call`, as `cdc.test.ts` counts them.
5. **Edits:**
   * **A 4 KiB overwrite and a 100-byte insertion at the middle** (`tree.edit`), counted the same way.
   * **Fixed form:** the insertion shifts every later block, so it's measured on a separate 1 GiB file to bound the time. The docs say why CDC is the default.
6. **Resume:**
   1. A second file's source throws at half the size.
   2. Wait 2 × the grace, with sweeps running.
   3. Resume from the last reported state. It sends only the rest (the bytes counted), and the file reads back.
   4. **Without a session:** the same at 64 MiB fails, showing what the session buys.
7. **Pipelining:** a 1 GiB write at `parallel` 1 and at 3.

**Once (in the CDC world):**
8. **The server under many writers:** 16 clients, each with its own session, write size / 16 each at once. Aggregate MiB/s, and zen-serve's `VmHWM` (TD-MEMORY's open server figure).
9. **The chunker at the defaults:**
   * wasm `Chunker.push` in MiB/s
   * the mean chunk length, giving CDC capacity at the defaults and in the half-full worst case of the fill rule
   * **the resync distribution:** 1,000 random insertions of 1–100 bytes into 4 MiB random buffers, with a histogram of chunks changed, counted exactly as in `cdc.test.ts` (8.3's figures were at small parameters)

**FoundationDB:**
* `ZEN_BENCH_BACKEND=fdb` with `ZEN_BENCH_CLUSTER_FILE` adds the `[storage]` table.
* Locally: run at 1 GiB, CDC only, with a release `--features fdb` server and a cluster from `install-fdb.sh` and `zen-serve init` (OPS §3).
* If the cluster doesn't fit the disk, the log says so.

**Output:** a table per form and backend, printed at the end.

### 2. The replica bench (`packages/fs/test/bench.test.ts`, new)

**The setup:**
* The replica runs over `sealedStore(nodeDiskLocalStore(tmp))`, as zen-mount runs it, with the defaults: `dirtyBytes` 64 MiB, `pendingBytes` 32 MiB, `cacheBytes` 1 GiB, CDC.
* The size is `ZEN_BENCH_GIB`.
* The same source and disk check as the client bench.

**What it measures:**
1. **Write:** one `FileHandle`, 1 MiB writes, then close.
   * MiB/s until the last write is queued, then the time until the queue drains (`idle()`), in MiB/s
   * sampled every 50 ms: the peaks of RSS, `heapUsed`, `arrayBuffers`, `dirtyBytes` and `pendingBytes`
   * the cache dir's size at its peak, since queued content stays local until sent
2. **Edit:** a 1-byte overwrite at the middle through a handle. The flush time, plus pages and chunks sent.
3. **Cold read:** on a second replica with a fresh cache dir, in 1 MiB reads. MiB/s, peak RSS, and `cacheBytes` kept at the end.
4. **Pin:** a third replica pins the file. MiB/s and peak `pendingBytes`.

### 3. The zen-mount bench (`packages/fuse/test/bench.test.ts`, new)

* **Setup:** `ZEN_BENCH=1`, and FUSE available (`canMount`). zen-mount runs as a child, as `mount.test.ts` runs it, with `--cache-dir` and the defaults.
* **Write:** the test writes a file of the size into the mount in 1 MiB writes, then fsyncs and closes it. It reports MiB/s, zen-mount's `VmHWM` (read before the stop), and the stop time (the drain).
* **Read:** remount with a fresh cache dir and read the file back, hashed, in MiB/s, with `VmHWM`.
* **Edit:** a 1-byte `pwrite` at the middle plus `fsync`, in ms.
* 8.6b's image benches (BLK.md §8) cover zen-mount's other workload. These add a large single file.

### 4. Loro (one case in `packages/loro/test/bench.test.ts`)

* A document whose snapshot is about 16 MiB: the compaction to a file, and a load from it.
* In ms, beside the inline figures.

### 5. CI: a `files-bench` job, started by hand

* **The trigger:** `workflow_dispatch` gains an input `files_gib` (default 16). The job runs only on dispatch, as `devices-bench` does.
* **Disk:**
  * It frees runner disk (`/usr/share/dotnet`, `/usr/local/lib/android`, `/opt/ghc`, the CodeQL cache) and prints `df`.
  * It sets `TMPDIR` to the larger of `/` and `/mnt`, so the servers' data goes there.
* **Build:** the release zen-server, the wasm, `npm ci`, `npm run build`, and FUSE (`fuse3`).
* **Runs:**
  * the client bench at `files_gib`
  * the replica and zen-mount benches at 4 GiB, to bound the time
  * `timeout-minutes: 150`
* **Not gating:** it measures. It's started once on this PR's branch (`actions_run_trigger`), and its figures go in STATS with the runner named.

### 6. Docs

* **docs/STATS.md:**
  * **§1.1:**
    * rows for CDC content: per chunk at the measured mean, 20 B in a page, 56 B per page reference
    * an upload session: its records, plus one `ur` key per chunk and page while it's open
  * **§1.3:**
    * CDC capacity, measured (the mean chunk, the fill rule's worst case)
    * the v3 inline limit
    * that fixed keeps 640 GiB, and when the writer picks it
  * **§2:** a row for editing a large file (CDC: the touched pages; fixed: an insertion costs the rest of the file).
  * **New §7, "Large files"** (measured):
    * throughput: write, read, random reads, append, edits, resume and pipelining, for CDC and fixed, embedded and FDB, here and on the runner
    * reuse after inserts: the resync distribution, plus periodic content and long zero runs (8.4, 8.6b)
    * memory: the client, the replica, zen-mount and the server under 16 writers
    * sessions: overhead and sweep rounds
    * the Loro file snapshot
* **spec/OPS.md:**
  * **§2.1:** open upload sessions hold storage until they're closed or expire (7 days by default, at most 30), at most 64 per user and fs. A user removed from the ACL keeps theirs until they expire.
  * **§2.2:** the large-file benches' disk needs.
  * **A new §7, "Large-file pitfalls":**
    * a cross-backend `migrate` or `import` changes the version scale, so upload sessions and leases may read as expired, and uploads start over (8.4's note)
    * CDC re-cuts periodic content and long zero runs after a length-changing edit, so use `fixed` for disk images and sparse-heavy files (TD-FS-SPARSE)
    * a whole rewrite without index records reads the file once
    * zen-mount's fsync is durable on the device, not on the server
  * **§6:** the measured figures linked from STATS §7.
* **spec/TECH_DEBT.md:**
  * TD-FS-LARGE-FILES fully resolved (its status names the 8.x commits)
  * new TD-FS-CDC-HUGE: two-level pages for CDC files past the v3 capacity
  * TD-MEMORY: the measured figures, with the server's no longer "no figure"
  * a check that TD-FUSE-INTEGRITY and TD-LORO-LARGE-SNAPSHOT read as resolved
* **docs/CLIENT.md §8–§9:** the figures linked, and what to set for large files (`parallel`, `readAhead`, sessions, `chunking`).
* **docs/BLK.md §8 and docs/DB.md §20:** a link each to STATS §7.
* **The milestone closes:**
  * README: "Milestone 8: done"
  * DESIGN-3 §6 item 8: "Done"
  * `docs/MILESTONE-8.md`: the log, and an **Outcome** in milestone 6's form (what was built, the differences from the plan, tests, measurements)

### Commits

1. `test: large-file benches (milestone 8 step 7)`: the three bench files, the Loro case, and ci.yml
2. any fix a bench finds, as its own commit with its test and control
3. `docs: large files measured, and the milestone 8 outcome`

### Verification

* **Checks:** `npm run check` and `npm run build`. The full `npx vitest run` without `ZEN_BENCH` gives 646 passed and the skipped count up by the new benches, none failed.
* **The benches,** each once here:
  * client, replica and zen-mount at 4 GiB, embedded, with `target/debug` pruned first for the release build
  * the client bench at 1 GiB on local FDB
  * a run's temp dirs deleted after it (only this session's, checked not mounted)
* **CI:** `files-bench` once, at 16 GiB.
* **The benches' own checks:** hashes equal, the resume sends only the rest, an edit's counts.
  * **Negative control:** a resume from an offset one byte off must fail the bench at its hash check.
* **CI green on both runs** (push and pull_request).

**Defaults chosen (overridable):**
* **Sizes:** 4 GiB here (the disk), 16 GiB on the runner. FDB here at 1 GiB, CDC only.
* **A 5 s grace in the bench worlds,** so superseded content goes and each resume runs past the grace with only a session keeping it.
* **The fixed form's insertion on a 1 GiB file.**
* **The CI job measures and doesn't gate.**
* **Fixes the benches find:** small and local ones go in this PR with a test; anything larger goes to the user.

## Step 8.7, continued: the embedded store's large values, fixed chunks, the server's memory

The 4 GiB benches found three things in zen-serve itself:
1. The embedded backend stores a large value at up to twice its size.
2. 64 KiB fixed chunks land just past a power of two.
3. The server peaks at 2–4 GiB RSS with one writer.

The user chose to fix all three in this PR (#74), before the benches are re-run and the docs written. No data compatibility is needed, so the store's format changes without a migration.

### 1. Large values in the embedded backend (`crates/zen-store/src/embedded.rs`)

**The cause:**
* redb (4.3) keeps an entry larger than its 4 KiB page in a leaf of its own.
* The buddy allocator rounds that leaf up to a power of two of pages. A sealed 64 KiB chunk (65,584 B) needs 17 pages and gets 32.
* CDC chunks of 8–88 KiB round up to 16 or 32 pages: 1.66× measured.
* A sealed zen-blk block (4,144 B) is just past one page, so it gets 2 (computed: to be measured in the test).

**The fix:** a value whose entry doesn't fit one page is split into pieces that each fill one leaf page exactly.
* **Tables:**
  * `kv`: key → tagged value. `0x00 ‖ bytes` is inline. `0x01 ‖ u32 len ‖ u32 pieces` is split.
  * New `parts`: `u32be(len(key)) ‖ key ‖ u32be(i)` → piece `i`.
  * The length prefix keeps one key's pieces contiguous and apart from every other key's.
* **The piece size** is what fits a one-entry leaf: `4096 − 12 − len(parts key)`. The 12 bytes are redb's leaf header: 4 B, plus a `u32` key end and a `u32` value end per entry (`btree_base.rs` `LeafAccessor`).
  * A value is split when its own entry wouldn't fit one page: `12 + len(key) + 1 + len(value) > 4096`.
  * Smaller values stay inline and share leaves as now.
  * So every piece but the last fills a page. The last one shares a leaf with small entries, and the record in `kv` is 9 B.
  * The page size is set explicitly (`Builder::set_page_size(4096)`), so the arithmetic can't drift from redb's default.
* **Reads:**
  * `get` and range reads decode the tag. A split value is read from `parts` in the same read transaction, the `Snapshot` holding both tables.
  * The piece count and total length are checked; a mismatch is `Error::Io`, naming the key.
* **Writes:**
  * In `commit_blocking`, `Set`, `VsKey` and `VsValue` go through one `put`, which:
    1. reads the old record
    2. removes its pieces if it was split
    3. writes the new record and its pieces
  * `Clear` removes the pieces likewise.
  * `ClearRange` collects the split keys in its `retain_in` closure, which sees each value, and removes their pieces.
  * `Add` decodes and writes inline (counters are 8 B).
  * The meta version key is tagged like any value.
* **A format marker:**
  * `meta/format` = 2, written at create.
  * `open` refuses a file that holds data but no marker: "zen.redb was written by an older zen-serve; start from an empty data_dir". That's cheaper than a silent misread, since no migration is needed.
* **Unchanged:** the `Storage`/`Txn` trait and everything above it, the FoundationDB backend, and `export`/`import`. Those go through the trait, so dumps carry plain values.

**Tests:**
* **`conformance.rs`** (both backends; values up to 100,000 B, within FoundationDB's limit):
  * sizes 0, 1, piece − 1, piece, piece + 1, 3 pieces and 100,000
  * each through `get`, `get_range` forward and reverse with limits, and `snapshot_get`
  * overwrites large → small → large
  * `clear`, and a `clear_range` spanning split and inline values
  * an old read version still seeing the large value it read, after an overwrite
  * a versionstamped value past a page
* **`tests/embedded.rs`:**
  1. **Disk:**
     * 1,500 random values in four sets: CDC-like lengths of 8,192–89,984 B, 65,584 B, 65,328 B and 4,144 B (blocks).
     * Each set's allocated growth of the file (`st_blocks × 512`) must be at most 1.12 × its logical bytes. With the fix it's about 1.03–1.06×; today it's about 2× for the fixed sizes.
  2. **No pieces left behind:** after overwrites (large → small, large → larger), clears and a clear range, a test-only count of `parts` entries (`Embedded::part_count`, under the `testing` feature) is what the live values need.
  3. **An old file is refused:** a redb file with a `kv` table and a version key but no marker.
* **Negative controls** (each run once, then restored):
  * the piece size 16 B larger (each piece then spills into a second page): test 1 fails at about 2×
  * no removal of the old pieces on an overwrite: test 2 fails
  * splitting off: test 1 fails for every set

### 2. Fixed chunks of 65,280 B (`@zen/client`)

* **The change:**
  * `CHUNK_SIZE` becomes 65,280 (64 KiB − 256), so a sealed chunk (65,328 B) is under 64 KiB on any store that rounds to 4 KiB blocks: the replica's disk cache (one file per record), and filesystems generally.
  * One constant, exported from `tree.ts` and used by `writer.ts` (it's duplicated today).
* **Existing versions** keep their chunk size through `previous`/`base`, as before. No data needs carrying over anyway.
* **Updated:**
  * tests that assume 65,536: those that don't pass `chunkSize` explicitly
  * docs that say "64 KiB chunks": STATS §1.1 and §1.3, CLIENT.md, fs.md/formats.md where they give the default
  * v2 capacity: 5,624 × 1,870 × 65,280 ≈ 638 GiB
* **Test:** `tree.test.ts` asserts a default fixed write's `chunkSize` is 65,280 and every sealed chunk is at most 65,536 B. With 65,536 it fails.

### 3. The server's memory

**Measure first, as in 8.6b:** the client bench's CDC write at 4 GiB (`-t cdc`, with the write and the first read), with the server under:
1. **As built:** `VmHWM`, plus `smaps_rollup` (anonymous against file-backed) every second.
2. **`MALLOC_ARENA_MAX=2`:** if the peak falls sharply, the cause is glibc keeping freed memory per arena. Commits run on tokio's blocking pool, and each thread can get an arena.
3. **jemalloc preloaded** (`LD_PRELOAD=libjemalloc.so.2`, installed here):
   * `MALLOC_CONF=stats_print:true` gives live, resident and retained bytes at exit.
   * If live bytes are large, `prof:true` with jeprof (or `heaptrack`, installable) names the allocation sites.
4. **redb's cache** at 1 GiB (the default) against 256 MiB (a temporary build with the setting below).

**Suspects from reading:**
* redb's cache: 1 GiB for the read cache and write buffer together.
* The embedded transaction copies every value twice: `overlay` and `ops` each own a `Vec`. That's on top of the request body and its decoded form, so a commit of 8 MB holds about 4 copies while it runs.
* glibc arenas across the blocking pool's threads.
* hyper's body buffers.

**The fix, by what the measurement shows** (the findings and the design go in the log before any code; anything beyond the items below goes to the user first):
* **`[storage] cache_bytes`** for the embedded backend, in any case (OPS §6: every cache gets a setting). It's `Options.cache_bytes` → `Builder::set_cache_size`. The default is the smallest that keeps the bench's write and read throughput within 10 % of 1 GiB's.
* **One copy per value in a transaction,** if the copies show up: `ops` and `overlay` share one `Arc<[u8]>`.
* **The allocator,** only if fragmentation shows: `mallopt(M_ARENA_MAX, …)` at start-up, or jemalloc as the server's global allocator (a new dependency). The user decides which.
* **Tests:**
  * a config test for `cache_bytes` (parsed, checked at least 1 MiB, reaching `Options`)
  * for the copies, a unit test that a transaction's buffered bytes are counted once (a test-only `EmbeddedTxn::buffered_bytes`)
  * RSS itself isn't asserted in a test (too noisy); the benches re-measure it

### Then, as planned

1. Re-run the client, replica and zen-mount benches at 4 GiB (embedded), and the client bench at 1 GiB on local FoundationDB (`--features fdb` release build, a cluster from `zen-serve init`).
2. Run the negative control for a resume one byte off.
3. Start `files-bench` once on CI, at 16 GiB.
4. The docs, as "Step 8.7 in detail" §6, plus:
   * STATS §1.2: measured multipliers before and after the split, for CDC, fixed and blocks
   * OPS §2: measure redb's file with `du`, not `ls` (it's sparse)
   * OPS §6: `[storage] cache_bytes` in the settings table
   * TD-MEMORY: the server's figures and their cause
   * spec/keyspace.md or OPS §1.2: the embedded store's two tables
5. The Outcome.

### Commits (after 23aa632)

1. `test: the benches' edits under sessions; disk as allocated`
2. `store: split large values across whole pages in the embedded backend`
3. `client: fixed chunks of 64 KiB − 256 B`
4. `server: …` (memory, by cause; `[storage] cache_bytes` at least)
5. `docs: large files measured, and the milestone 8 outcome`

### Verification

* **Rust:**
  * `cargo fmt --check`
  * clippy `--all-targets -D warnings` on the workspace (default, `--no-default-features`, `--all-features`)
  * `cargo test -p zen-store` (embedded; conformance on FoundationDB once, with the local cluster)
  * `cargo test -p zen-server` (dump, fs, api)
  * `cargo test -p zen-blk -p zen-ublk`
* **JS:** `npm run check`, `npm run build`, then the full `npx vitest run`: 646 passed plus the new skips, none failed.
* **The negative controls above.**
* **The benches re-run:** the data dir's allocated growth near 1.05× of the bytes sent under fixed and CDC alike, and the server's peak against the figures above.
* **CI:** green on both runs, `fdb` included (conformance on FoundationDB).

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`); the touched crates' tests; FDB once for server changes; vectors regenerate with only the intended diff.
* **JS:** `scripts/build-wasm.sh`; `npm run check`; the touched suites 3×, then the full `npx vitest run`; `npx playwright test` when the browser is touched.
* **Negative controls:** each new check (offset lookup, resync, session pin, the memory bound, `EIO` on an alarm) is run once disabled, and its test must fail. The log says so.
* **CI:** green on both runs of each PR.

## Risks
* **CDC is a format change across the writer, reader, replica and vectors.** v1 and v2 stay readable, and `fixed` stays a policy, so it can be backed out per fs.
* **CDC capacity** is smaller than v2's at the defaults (about 220 against 640 GiB). The writer falls back to fixed when the size is known; two-level pages are a TD.
* **Disk here:** about 5 GB free limits the local bench to about 4 GiB. Larger runs are in the CI job started by hand.
* **Memory bounds in Node** are measured by RSS, which is noisy, so the bounds have margin.

## Outcome

Done. Milestone 8 makes large files ordinary: a file of hundreds of GiB is written from any stream in bounded memory, resumed days later, edited or appended to at the cost of the pages it touches, and read back at disk-like speed. Content is cut by a keyed content-defined chunker by default, so an insertion re-uploads a chunk or two, and the server still can't predict where a known file would be cut. The milestone measured all of it at 4 and 16 GiB, and the measurements found and fixed three things in zen-serve itself: the embedded store's large values (2× on disk), the fixed chunk size (just past a power of two) and the server's memory (up to 3 GiB).

What was built:
* **The spec** (8.1, then each step's `spec:` commit):
  * formats.md §11.3 (manifest version 3: chunk lengths, pages with byte spans and counts, the fill rule), §11.6 (page form 2), §11.7 (the keyed chunker, new), §12 (the policy's `chunking`)
  * fs.md §4, §4.1 (variable chunks, upload sessions), §6, §10, §11.1 (the replica's handles); zendb.md §20 (Loro snapshots as files)
  * api.md §2 (`fs_cdc`, `upload_sessions`, the upload limits), §6 (`upload` in a commit), §12.1 (upload sessions, new); keyspace.md §3.6 (`us`, `ur`, `ux`, `uu`)
  * vectors: `fs.json` gains version-3 manifests, form-2 pages and the chunker
* **zen-core and zen-wasm** (8.1): `ManifestV3` with a strict decoder, page form 2, `cdc::Chunker` (streaming, keyed under the fs's or the file's key), `FsPolicy.chunking`; bound in wasm with a `Chunker` class that keeps the key out of JS.
* **The server** (8.2): form-2 pages counted per entry; upload sessions (open, renew, close, list, the commit field), each put referenced once per session, released by the sweeper in bounded rounds; the features and limits in `/v1/info`.
* **`@zen/client`** (8.3): reading versions 1–3 by byte offsets; the CDC writer with reuse by hash, known chunks and pages, and the fill rule; `edit`, `append`, `editUpload`; up to 3 commits in flight with progress in order; `readStream` read-ahead; a compact resumable `UploadState`; sessions. Fixed chunks became 65,280 B (8.7).
* **The replica and zen-mount** (8.4, 8.6, 8.6b): file handles over extents with at most 64 MiB in memory, flushed as edits; binary per-page index records; `Edit.zeros`; the `close({wait})` fix; `zen-mount --integrity` (TD-FUSE-INTEGRITY); the sender's uploads under sessions; no per-operation timeout in zen-mount; the records' backlog bounded (`pendingBytes`), the page memo bounded by ids, pinned content in windows, fetched content dropped past the bound.
* **Loro** (8.5): a snapshot past half a commit is a zen-fs file in a tree of the document's own, named in the `$loro` row by its sealed manifest's hash (TD-LORO-LARGE-SNAPSHOT).
* **zen-ublk** (8.6): `import` (resumable by seq, zeros skipped or discarded) and `export` (sparse, read-only) of raw images; a measured comparison of an image as a file and as a volume (docs/BLK.md §8).
* **The benches** (8.7): the client, the replica, zen-mount and Loro at multi-GiB sizes, embedded and on FoundationDB, with memory for every process; CI's `files-bench` job started by hand.
* **The store and the server** (8.7): the embedded backend splits a value larger than a page across whole pages (1.05–1.08× on disk, from up to 2×), behind a layout marker; `[storage] cache_bytes` (256 MiB), 2 malloc arenas, `limits.commit_buffer_bytes` (64 MiB), and one copy of each value in a transaction (the server at 0.6–0.7 GiB, from 1.9–3.1).
* **The guides** (8.7): STATS §1.1–§1.3, §2 and a new §7; OPS §2, §6 and a new §7 (large-file pitfalls); operations.md §1 (the embedded file's layout); CLIENT.md §8–§9; BLK.md §2 and §8; DB.md (Loro); TECH_DEBT (TD-FS-LARGE-FILES resolved fully, TD-FUSE-INTEGRITY and TD-LORO-LARGE-SNAPSHOT resolved, TD-FS-CDC-HUGE, TD-FS-SPARSE, TD-BLK-SNAPSHOT-FILES and TD-MEMORY new).

Differences from the plan above:
* **v3 pages need not be full** (8.3, a `spec:` change to 8.1): with every page full but the last, an edit that changes the chunk count by one would shift every later page. Each page reference carries its `count`, and the fill rule keeps any two neighbours over a page, so capacity is at worst half.
* **Repacking became the fill rule** and `edit`'s repack when pages would overflow, rather than a background repack; nothing fragments.
* **Upload sessions** have a per-user index (`uu`) and no reference count in their record (8.2), so commits under one session don't conflict; every non-final release round rewrites the record, so a renew racing the release retries into `upload_gone`.
* **A handle past its bound queues a write of its own** (8.4, the user's choice), so other devices can see a file partly written.
* **Two causes of zen-mount's memory** (8.6b), both outside the handle: the records' write-behind and the page memo counted in pages. A step of its own.
* **The replica's sender uploads under sessions** (8.6): found by the file benches, where a slow upload's early chunks were collected before its write landed.
* **The benches found three server issues** (8.7), fixed in the milestone rather than left as debt: the embedded store's large values, the fixed chunk size and the server's memory (the user's choice each time, and of the allocator, the cache default and the commit budget).
* **Capacity:** about 196 GiB a CDC version, measured, against the plan's estimate of 220 (the mean chunk is 36.5 KiB under the 88 KiB cap).
* **Fixed along the way:** the `close({wait})` throw (8.4); `setPolicy` deciding "nothing changed" without `chunking` (8.1); a clock race in kanban's `deleteCard` (8.3); the devices test's server sharing the test's runtime (8.3), its lease (8.6), and a busy unmount now named (8.7); the dump tests under a concurrent version jump (8.4); eviction's keep set taken before the store was listed (8.6); the package build order and the fuse tests' typecheck (8.7).

Tests (at step 7):
* **Rust:**
  * zen-core: `cdc` (6), version-3 manifests and form-2 pages (`tests/fs.rs`), the vectors read back.
  * zen-store: the conformance case `large_values` (both backends), the embedded disk, pieces and layout tests, `a_set_value_is_buffered_once`.
  * zen-server: 7 upload-session tests and the form-2 page test in `tests/fs.rs`, the dump round trip with a session, `/v1/info`, the commit budget (`tests/api.rs`), the config checks.
  * zen-ublk: `tests/image.rs` (5) and `image_import_serve_export`.
* **TypeScript:** `cdc.test.ts` (12), `handles.test.ts` (9), `memory.test.ts`, `sessions.test.ts`, the replica's `writes.test.ts` cases, the mount's integrity and timeout tests, the Loro file-snapshot cases (9), and the fixed chunk size in `tree.test.ts`. The full vitest run: 647 passed, 22 skipped (the opt-in benches).
* **Negative controls,** each run once and logged per step: every new check failed with its mechanism off.

Measurements (docs/STATS.md §7):
* **Throughput, 4 GiB, embedded, here:** CDC writes 51–54 MiB/s and reads 108–116; fixed 67 and 121. 1,000 random 4 KiB reads at a median of 1.4–1.6 ms. An append, a 4 KiB overwrite and a 100-byte insertion into 4 GiB each cost one page and one chunk, in 137–302 ms (CDC). Pipelining doubles a single upload.
* **Reuse:** 98.1 % of small insertions change one chunk.
* **Disk:** 1.08–1.11× the bytes sent, every key included.
* **Memory:** the server 0.6–0.7 GiB with one writer or 16; the replica 564 MiB writing 4 GiB; zen-mount 465 MiB.
* **FoundationDB, 1 GiB, one process here:** writes 27–33 MiB/s, reads 80–116, edits about 1 s each (one page and one chunk); zen-serve 206–324 MiB with one writer and 1,091 MiB with 16 (FoundationDB's client library).
* **CI's runner (4 cores, 16 GB) at 16 GiB:** CDC writes 51 MiB/s and reads 75, fixed 72 and 97; 1.05–1.08× on disk; every edit one page and one chunk; the server at most 726 MiB (16 writers). The client's own memory grows with the file (1.2–1.9 GiB at 16 GiB), a TD-MEMORY entry.

### Remaining (decided: wait for the runner)
1. At the check-in (~01:25 UTC), read the `files-bench` Bench step's tables from dispatch run 42428472910 (client at 16 GiB, replica and zen-mount at 4 GiB, on the runner).
2. If it failed, root-cause it before anything else.
3. Fill in STATS §7.1's runner paragraph and the Outcome's runner line. Log the figures here, then `cp` this file to `docs/MILESTONE-8.md`.
4. Commit `docs: the 16 GiB figures from CI's runner`, push, and confirm CI green on both runs. PR #74 then waits on review and merge.

**At the 01:25 check-in:** `files-bench` was still in its Bench step (started 00:16; the job's limit is 240 min). Another check-in is due in about 60 min.

**Found:** `devices-bench` (8.6's image benches, dispatch only) was cancelled at its 45-minute job limit. Its Bench step ran from 00:12 to 00:54; `bench_file_cdc` alone took more than 20 minutes here.
* **Fix** (`.github/workflows/ci.yml`): `timeout-minutes: 120`, its own commit `ci: devices-bench gets 120 minutes`. It rides in the push with the runner figures, so no extra dispatch is needed until its figures are wanted.
* Done: 2bde53c, pushed.

**At the 02:27 check-in, `files-bench` had passed** (dispatch run 42428472910 on 53ff0e4; the Bench step 00:16–01:43, 87 min; every bench's tests passed). The runner is GitHub's `ubuntu-latest`, 4 cores and 16 GB:
* **Client, 16 GiB, embedded:**

  | | CDC | Fixed |
  |---|---|---|
  | Write, under a session | 51.2 MiB/s, 2,196 commits | 71.7 MiB/s, 2,179 commits |
  | The version | 459,627 chunks (mean 36.5 KiB), 103 pages | 263,173 chunks, 47 pages |
  | Allocated on disk, for 16.0 GiB sent | +17.3 GiB (1.08×) | +16.9 GiB (1.05×) |
  | `readStream`, readAhead 3 / 1 | 75.0 / 75.5 MiB/s | 96.9 / 98.5 MiB/s |
  | 1,000 random 4 KiB reads, cold / warm | median 2.9 / 2.3 ms | 3.0 / 2.4 ms |
  | Append 1 MiB / overwrite 4 KiB / insert 100 B | 110 ms / 1,558 ms / 345 ms, 1 page and 1 chunk each | 550 ms / 3,292 ms / 18.3 s on 1 GiB (8,209 chunks) |
  | Resume at half, past the grace | 26.7 MiB/s, only the rest sent; without a session refused | 44.2 MiB/s; refused |
  | 1 GiB, 1 / 3 in flight | 15.9 / 26.1 MiB/s | 28.1 / 38.5 MiB/s |
  | Server peak | 601–622 MiB | 602–624 MiB |
  | Largest upload state | 576 KiB | 707 KiB |

  * 16 writers of 1 GiB: 49.9 MiB/s in all, the server 726 MiB, the client 3,597 MiB (1.45 GiB buffers).
  * The chunker: 406 MiB/s; insertions changed 1 chunk 99.5 %, 2 0.3 %, 3 0.2 %.
  * The client's own RSS grew with the file (1.2–1.9 GiB at 16 GiB, against 0.5–1.0 at 4 GiB; heap up to 762 MiB at the insertion). Not bounded by anything of ours yet: a TD-MEMORY line.
* **Replica, 4 GiB:** write 13.0 MiB/s, RSS 571 MiB, `dirtyBytes` ≤ 65.0, `pendingBytes` ≤ 39.4, cache dir ≤ 1,811 MiB; a 1-byte edit 318 ms; a cold read 63.1 MiB/s (RSS 491); a pin 16.1 MiB/s (RSS 773); the server 558–564 MiB.
* **zen-mount, 4 GiB:** write 11.2 MiB/s until closed and 11.1 until on the server, zen-mount 456 MiB, the server 496 MiB, cache dir ≤ 1,568 MiB, stop 0.6 s; a cold read 47.8 MiB/s (zen-mount 301 MiB); a 1-byte write and fsync 647 ms.

**Done** (the steps below), committed with the figures:
1. **STATS §7.1:** replace the runner placeholder sentence with a "CI's runner (4 cores, 16 GB), 16 GiB" table: the client rows above, the replica and zen-mount rows, and the 16-writer line.
2. **STATS §7.3:** add a client row at 16 GiB (1.2–1.9 GiB RSS).
3. **TD-MEMORY:** add a line on the client's RSS growing with a file's size (heap 762 MiB at an insertion into 16 GiB), not yet bounded or explained.
4. **The Outcome:** replace "CI's runner at 16 GiB: in progress" with a summary line.
5. Copy this file to `docs/MILESTONE-8.md`.
6. Commit `docs: the 16 GiB figures from CI's runner` and push.
7. Confirm CI is green on both runs of it, and that it was on 2bde53c. Then PR #74 waits on review and merge.
