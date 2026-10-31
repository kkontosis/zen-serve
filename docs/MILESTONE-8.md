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
