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
    * **A note for 8.7's OPS pitfalls** (no code change): a cross-backend `migrate` or `import` changes the version scale, so upload sessions (like leases) may read as expired on the target, and their uploads start over.

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
