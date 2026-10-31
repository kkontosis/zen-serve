# Milestone 6 plan: integrity (signed checkpoints, the authenticated tier) and `zen-ublk`

## Context

Today the server can't read or forge anything, but it can still **roll back or withhold**, and clients can't tell:
* serve an older valid value, index root or fs header
* hide the tail of a topic, or the newest operations of a filesystem tree
* replay an older but consistent snapshot of a relayed connection (TD-SVC-STATE-ROLLBACK)

DESIGN.md §2.6 promises freshness up to **fork consistency**. With one writer, any rollback is detected. With several writers, a server can split clients into two histories, but it can't merge them back without being caught. Every spec defers this to milestone 6:
* zendb.md §14 reserves `integrity: "authenticated"` and `zen/v1/sig/db-root`
* fs.md §9 reserves tree checkpoints
* formats.md §12 reserves a signed fs header
* DESIGN-3 §0 decides event checkpoints every `sig_every` events
* GAPS G26 puts topic heads in them
* svc.md §12 wants a handler checkpoint

The labels are registered, and the hybrid signatures and device keys exist (zen-core `sig.rs`). There are no formats, keyspace, endpoints or code yet.

Milestone 6 also builds `zen-ublk` (DESIGN.md §6): a Linux block device over an fs. `mkfs.ext4` on it gives "remote LUKS" with freshness. It has one writer at a time, enforced by a lease and fencing.

**User decisions:**
* **Scope:** all four integrity pieces:
  1. the zen-db authenticated tier
  2. event checkpoints with topic heads
  3. fs tree checkpoints and a signed fs header
  4. the svc handler checkpoint
* **Signers:** writing an authenticated object needs a device signing key. Sessions without one (passkey, OPAQUE, API token, mTLS) are offered **device enrollment**: a device key certified by the user identity, kept in the vault. Without enrollment, a session reads and verifies but doesn't write authenticated objects.
* **`zen-ublk`:**
  * a small native **Rust client** crate
  * a **block engine**: blocks as KV values, a Merkle block tree, chained roots, a single-writer lease with fencing
  * two frontends:
    * **ublk**, tested in a CI job after `modprobe ublk_drv`
    * a **FUSE single-file export** (loop device, `mkfs.ext4`, `mount`), testable here and in CI
  * This container has no ublk or NBD, but has FUSE, loop devices and root.
* **Fork detection:** checkpoints live in a per-fs log on the server. Each client pins the newest it has verified and refuses anything that doesn't extend it. A CLI/API exports and compares pins out of band. An independent witness is tech debt (TD-WITNESS).

**Defaults I chose (overridable at plan approval):**
* **One primitive for everything: the device checkpoint.**
  * A per-device, hash-chained, hybrid-signed record. It lists the heads and roots the device wrote or verified (topics, trees, namespaces, connections, header, ACL), plus the latest checkpoint hash it has seen from each other device.
  * The cross-links make a fork visible as soon as two forked clients see any checkpoint of the other side (SUNDR-style).
  * Checkpoints are written every `sig_every` changes or a few seconds after the last write (default 64 / 5 s), and on close.
* **No hybrid signature per commit.** Signing costs 3.4 KB and about 2.6 ms in WASM, and DESIGN-3 §2.5's "signed at each commit" would add both to every write.
  * Instead, each zen-db namespace root, volume root and event carries a **keyed hash chain** `(seq, prev)` that only key holders can compute. The server can't forge a successor; it can only show an older link.
  * Pins and checkpoints catch that. Signatures are on checkpoints, which gives attribution and cross-device freshness.
  * The rollback window between a write and its device's next checkpoint is bounded by `sig_every` and the delay. `sig_every = 1` closes it.
* **Authenticated zen-db namespaces use 32-byte node ids** (`PRF32`, not `PRF16`), so a member colluding with the server can't find a collision at 2^64.
* **A generic fenced lease** (`/v1/lease`, and `Commit.fence`), not one specific to volumes. Plain KV writes aren't fenced today (only consume steps are). zen-ublk uses it; zen-db writers may.
* **Volumes** are named objects inside an fs (`(fs, "blk", vol, idx)`), so an fs can hold several. Copy-on-write snapshots and clones are tech debt (TD-BLK-SNAPSHOTS). Read-only mounts read a consistent root while no writer holds the lease.

## Progress and resuming

**Rhythm (as milestone 5.5):** one sub-step per PR. Before each one:
1. restart from main if the previous PR is merged
2. plan the sub-step in detail here, and commit the plan

On the user's "go": a `spec:` commit if needed, then code and docs. Log the step in both plan copies (this file and `docs/MILESTONE-6.md`, kept equal with `cp`). Push, confirm CI is green on both runs, and pursue every red to its root cause. Stop for compaction, and confirm with the user before implementation begins after one.

**Branch:** `claude/brave-knuth-gk03mm`. Push with retries (2, 4, 8, 16 s). To restart from main: `git fetch origin main && git checkout -B claude/brave-knuth-gk03mm origin/main`, then push `--force-with-lease`.

**Constraints:**
* no model names in the repo; commit trailers as before
* `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push
* about 3.4 GB of disk free: prune stale `target/debug/deps` executables when low (`target/` is 21 GB)
* the local FDB cluster: `./target/debug/zen-serve serve -c /tmp/zen-ci/node.toml` (built `--features fdb`), cluster file `/tmp/zen-ci/data/fdb.cluster`
* `ZEN_APPS_BUILT=1` skips the vitest app build; don't build zen-server while vitest runs
* no skips by default: device tests skip only where the kernel lacks the device, and say so

**Log:**
* **Step 0** (plan): done. PR #52 (milestone 5.5 step 14) merged at 3378490.
  * Restart from main.
  * Write `docs/MILESTONE-6.md` (this plan).
  * Update DESIGN-3 §6 item 6 (scope and link) and the README status ("Milestone 6: in progress").
  * Commit `docs: milestone 6 plan (integrity, zen-ublk)`, then open the PR.
  * PR #53 merged.
* **Step 6.1** (spec, zen-core, vectors, wasm): planned below ("Step 6.1 in detail"). Plan committed (95ec6a8), PR #54.
  * `spec:` commit 330c7fa:
    * formats.md §4 (kind 11), §5.1 (event chains), §8, §12.1 (signed header), §14 (device checkpoints)
    * fs.md §9.1 (tree state)
    * api.md §2 (features, `checkpoint_keep`), §4.4, §6 (`fence`), §9 (`cksub`/`ckh`), §12 (`changes` ops/chain), §15, §16
    * keyspace.md §3.10 (`dk`, `dkh`, `dkd`, `dv`, `dvu`, `ls`)
    * svc.md §6 and §12, zendb.md §14
    * labels.md and `labels.rs`
  * **zen-core:**
    * new `integrity.rs`: `CheckpointBody`/`Entry`, canonical encode/decode, sign/verify, seal/open as kind 11
    * `header.rs`: `SignedHeader`, `validate_header_successor`
    * `seal.rs`: `EventBody.chain` (v2) and `event_chain_hash`
    * `fs.rs`: `StateNode`, `state_leaf`, `tree_state`, incremental `TreeState`
    * vectors in `integrity.json`; unit tests plus a test that reads the vectors back from the file
  * **zen-wasm:**
    * new `integrity.rs`: checkpoint codec, verify, seal/open, signed header, event chain, `TreeState`
    * `DeviceSecret.sign`/`signCheckpoint`, `SigningIdentity.signHeader`
    * `EventBody` v2 fields, `label()` names
  * **TS:** `packages/client/test/integrityvectors.test.ts`.
  * **Found and fixed:** a JS `JSON.parse` loses u64 hlc values above 2^53, so `integrity.json` writes hlc as decimal strings.
  * **Checks (all clean):**
    * fmt
    * clippy on the workspace, and on zen-core/zen-wasm with `--all-features`, `--no-default-features` and wasm32
    * zen-core tests
    * the TS vectors test 3×
    * `npm run check`
    * the client log, stream and kv tests, and `packages/db` (245 passed; only the opt-in bench skipped)
  * **A red on the plan-only commit (push run 377, `fdb` job), traced to its cause:**
    * `dump.rs` `relay_keys_count_as_empty` read storage directly, with no retry, and got `TooOld`.
    * The tests in that binary run at once. `export_import_round_trip` and `migrate_copies_into_another_store` jump the shared FDB cluster's version (`advance_version`), and on a fresh cluster the recovery makes reads fail for about 10 s.
    * `dump` itself retries since 2322431; the test's own reads didn't.
    * Fix: a retrying `get` helper for the three direct reads, and the migrate loop now retries `begin` too.
    * FDB here, 5×: the first run, on a fresh cluster, took 11.5 s and passed.
  * **CI on 297245b and 51e629f (runs 379–382): `client` "Lint and typecheck" red.** `rust`, `pure-rust` and `fdb` are green, so the dump fix holds on CI.
    * Cause: CI type-checks the client tests (`tsc -p packages/client/tsconfig.test.json`, `include: [src, test]`). The local `npm run check` runs `tsc -p packages/client --noEmit`, which leaves `test/` out, so the new test's type errors passed locally.
    * Errors in `packages/client/test/integrityvectors.test.ts`:
      * line 44: `flipped[8] ^= 1` gives TS2532 (index access may be undefined)
      * line 95: assigning `eventChainHash`'s `Uint8Array<ArrayBufferLike>` to `prev`, inferred as `Uint8Array<ArrayBuffer>`, gives TS2322
    * **Fix:**
      1. `flipped[8]! ^= 1;` (the repo's idiom: `local.test.ts:164`, `tree.test.ts:282`, `vault.test.ts:228`), and `let prev: Uint8Array = new Uint8Array(32);`.
      2. Make `npm run check` match CI, so this can't recur: root `package.json` `check` uses `tsc -p packages/client/tsconfig.test.json` (instead of `tsc -p packages/client --noEmit`) and adds `tsc -p packages/fuse/tsconfig.json --noEmit`, the two CI lines it lacks.
    * Done: the new `npm run check` passes. Run on the unfixed test file, it reports exactly CI's 2 errors. The integrity vectors test passes.
  * **CI green on both runs of b779c7c** (push 383, pull_request 384): rust, pure-rust, fdb and client all pass. Step 6.1 is done, waiting on review and merge of PR #54.
  * PR #54 merged at 3c8269a.
* **Step 6.2** (server): planned below ("Step 6.2 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (59d9346), PR #55.
  * `spec:` commit 5951bc9: checkpoints count toward the fs quota (429 `quota`; pruning gives bytes back); `dkd` isn't exported; `examples/zen-serve.toml` gains `checkpoint_keep`.
  * **zen-proto:** `Limits.checkpoint_keep`, `Commit.fence` (`Fence`), `Changes.ops`/`chain`, the checkpoint, device and named-lease types, `Frame::Cksub`/`Ckh`.
  * **zen-server:**
    * `integrity.rs` (checkpoint `put`/`heads`/`get`, `devices/register`/`get`), `lease.rs` (`/v1/lease`, `check_fence`), `quota.rs` (the quota check, now shared by commits and checkpoint puts)
    * `commit.rs`: fences are validated (fs `write`, 1–64-byte names, one op each) and checked first in the transaction
    * `acl.rs` `header_put`: signed headers are checked with `validate_header_successor` against the caller's ACL admins
    * `tree.rs`: `changes` carries `ops`/`chain` when complete
    * `stream.rs`: `cksub` (baseline heads, then watch `dkd` and push each moved head)
    * `dump.rs`: `dkd` is private
    * config `limits.checkpoint_keep` and `adversary`; `/v1/info` features and limit
    * `adversary.rs` (`test-utils`): the storage decorator behind `AppState.view`, the five lies, `POST /v1/test/adversary`. Read endpoints switched to `st.view`: kv get/range, log read, stream subs, header get, tree reads (`read_txn`), checkpoint heads/get, devices get, `cksub`.
  * **Tests:** new `tests/integrity.rs` (15 tests: features, the checkpoint log, ownership, quota, devices, `cksub`, the signed header chain, named leases, fences incl. the read-set conflict, `changes` ops/chain, four adversary tests and its gating). `tests/dump.rs`: the round trip now carries a checkpoint, a registered device and a lease, and checks `dkd` stays behind.
  * **Found while verifying:**
    * A POST to a missing route gets 405 from the static fallback, not 404: the gating test checks 405.
    * The disk filled (971 MB free) while building the fdb tests, and FoundationDB stopped starting transactions, so every FDB test hung. Removed `target/debug/incremental` (2.9 GB, a cache) and stale test binaries (3.8 GB).
    * The first FDB run after that recovery failed two dump tests at the new setup calls in `populated_on`. These tests run at once, and an import jumps the shared cluster's version (the 6.1 cause), so the extra calls widened the window. `populated_on` now retries its calls on `too_old`. Five plain reruns, then three with the fix, were green.
  * **Checks (all clean):**
    * fmt
    * clippy `-D warnings`: default, `--no-default-features`, `--all-features`
    * `cargo test -p zen-server -p zen-proto` (embedded)
    * on local FDB: integrity and dump 3×, then the full `-p zen-store -p zen-server --features fdb`
    * `build-wasm.sh` and `npm run check`
    * vitest `kv`, `stream`, `tree`, `log`, `integrityvectors` and `packages/fs`
  * Pushed 811a74c. **The push run (388) was red in `fdb`; the pull_request run (389) was green.**
    * `tests/dump.rs` on CI's fresh cluster: `/v1/checkpoint/put` and `/v1/devices/register` returned 409 `commit_unknown`.
    * Cause: after the version jumps these tests make, FoundationDB can lose track of a commit's outcome. Both endpoints ran the non-idempotent transaction loop, so the unknown outcome reached the client, which per api.md §1 may then only re-read.
    * Fix in the server: both are now idempotent (`txn_loop!(…, idempotent, …)`).
      * A put whose `seq` is the head and whose stored checkpoint is byte-for-byte the same returns the stored version.
      * Registering again was already a no-op.
      * api.md §1 and §15 say so.
    * Leases stay non-idempotent: two processes of one device must not share a token. So the dump setup treats `not_leader` after an unknown result as "acquired".
    * The setup helper retries `commit_unknown` as well as `too_old`.
    * `checkpoint_log` tests the resend, which returns the same version, while another checkpoint at the head gets 409.
    * Embedded, and FDB 3×: green.
  * **CI green on both runs of 43e5455** (push 390, pull_request 391): rust, pure-rust, fdb and client all pass. Step 6.2 is done, waiting on review and merge of PR #55. This line reaches `docs/MILESTONE-6.md` at the restart from main, so CI isn't rerun for a docs-only commit.
  * PR #55 merged at e3a2c5c.
* **Step 6.3** (`@zen/client`): planned below ("Step 6.3 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (854bcea), PR #56.
  * **wasm:** the 6.2 wire codecs (checkpoints, devices, named leases). The 6.1 TS type `CheckpointHead {version, hash}` is renamed `ChainHead`: exported next to zen-proto's `CheckpointHead {device, seq}`, the two interfaces merged.
  * **`@zen/client`:**
    * new `integrity.ts`: `Fs.integrity(opts)`, pins, checkpoint writing (cadence, cumulative bodies, the 409 merge, idempotent resends), `sync` (rules 1–4, the ACL and header claims), `watch`, `exportPins`/`comparePins`, alarms (`IntegrityError`)
    * new `devices.ts` (`newDevice`); `session.useIdentity`, `approveDevice`, `enrollDevice`, `devices`
    * `fs.ts`: signed headers read (verified against an admin of the verified ACL, pinned with integrity) and written (`init({signed})`, `signHeader()`, signed successors; `signer_needed`)
    * `log.ts`: chain pools, version-2 bodies, `ChainCheck` (gaps, rewrites, pins; repeats of the same body allowed, as a dead-letter retry re-appends one), the end-of-read freshness check, `verifyHead`
    * `kv.ts`: `Transaction.onSettle` (applied, failed or unknown), so a retried transaction reuses its chain position
    * `stream.ts`: the `ck` subscription (`subscribeCheckpoints`), chain checks on subscriptions
    * `acl.ts`: `pin`/`pinned`, `AclVersion.bytes`
    * the vault: `saveSigner`/`signer()`
    * testing: `spawnServer({adversary})`, `TestServer.fork()`, `testing/adversary.ts` (`lie`, `truth`); booleans in the test CBOR encoder
  * **Changes from the plan:**
    * An enrolled device signs checkpoints but doesn't sign in: method 1 still needs the cert in the ACL (auth.md §3), so the vault keeps it as `signer`, not as `device`.
    * Only device heads, the ACL and the header pins are persisted. Object floors are rebuilt from the device bodies on load (each body is cumulative), so no `obj/` records.
    * `exportPins` is a fixed binary record, not CBOR: `1 ‖ u32 fs ‖ acl ‖ header ‖ u32 n ‖ n × (fp ‖ u64 seq ‖ HC)`.
    * A non-admin's header put is refused with 403 before any signature check (the plan said 400).
  * **Found while testing:**
    * `Stream.connect` had no timeout: an attempt that met the server restarting stayed in CONNECTING forever, so the stream never came back. It now gives up after `connectTimeoutMs` (default 10 s) and the backoff loop retries.
    * `Acl.head()` didn't notice a server below the pin: an empty answer returned the pin, and a missing version gave `not_found`. Both are now `acl_chain` ("below the pinned version"), which integrity reports as an ACL rollback.
  * **Tests:** new `packages/client/test/integrity.test.ts`, 13 tests (enrollment, honest checkpoints, two processes of one device, watch, chains, gap and rewrite, signed header, header rollback, topic tail withheld, checkpoints withheld, fork, whole-store rollback, an honest mixed run). 3× green. Each attack test was run with its check disabled and failed: tail, header, gap, rewrite, fork, ACL; for checkpoints withheld, the per-device head check, the own head check and both `seen` checks had to be disabled together, since each catches it alone.
  * **Checks (all clean):**
    * `cargo fmt --check`; clippy on zen-wasm (host `--all-targets`, wasm32)
    * `scripts/build-wasm.sh`, `npm run check`
    * all `packages/client` tests (17 files, 162 tests); `packages/db`, `fs`, `svc`, `loro`, `fuse` (only the opt-in benches skip)
    * the Playwright smoke
  * **CI:** the `client` job builds `zen-server --features test-utils`.
  * Pushed 6ea8fa8. **The push run (395) was green; the pull_request run (396) was red in `client`**, at one test outside this step: `apps/chat` "who's online" (TD-TEST-OPEN-FAILURES item 4, first seen on 84c8c02 in milestone 5.5). Its diagnostic now says more: within 1.5 s, alice, carol and dave answered and bob (the asker) didn't; asked again with 10 s, alice, bob and dave answered and carol didn't. So a reply is **lost, not slow**, and a different one each time. The plan to find and fix the cause is "Step 6.3: the red on run 396" below.
  * Fixed and logged in "Step 6.3: the red on run 396" below. PR #56 merged at 883c9ba.
* **Step 6.4** (fs and svc): planned below ("Step 6.4 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (08750c1), PR #57.
  * `spec:` commit 1865927: formats.md §11.5 purge records; fs.md §6 and §9 (same ops, same state; when a replica checks; rollback and fork); api.md §12 (`tree/chain` counts purges); svc.md §12 (sync before resuming; checkpoint before release; cadence; the handler's database left for 6.5).
  * **Rust** (efd97cc): zen-core `fs::purge_bytes` and `PURGE_DEVICE`; `fs.json` gains `purge` (only that file changed on regeneration). zen-server `purge_trash` reads the tree header normally and extends `ops`/`chain` with one purge record per round that removes nodes, in the round's transaction. `tests/fs.rs` `sweeper_purges_trash_and_collects_chunks` checks the record (ops 6 → 7, the chain over `[file, dir]`, leaves first) and that a caught-up `changes` answer carries it. Negative control: with the server change reverted the test fails (`(6, …)` vs `(7, …)`).
  * **Client, fs, svc, apps** (e97489f):
    * `tree.ts`: `ChangeBatch.head`, `changes(…, {heads: true})`, `stateNodeOf`.
    * `integrity.ts`: `observe(entry, {count?, floorAt?})`. **Changed from the plan:** the rollback check uses the floor as it was *before* the answer was asked for (`floorAt`): a checkpoint that arrives while a `changes` request is in flight can honestly name a higher `ops` than the answer. The fork check (same `ops`, another chain or state) uses both floors, since it never holds honestly.
    * `replica.ts`: the feed goes through `feed()`, which takes the tree's floor before each request; the `TreeState` is built at the first check from the node table and kept by `setNode`/`dropNode` from then on (so tabs that never check never build it); `check` runs after a caught-up batch only when the saved cursor is that batch's and no full sync is under way, and after a full sync once the unseen nodes are dropped. An `IntegrityError` stops `run`, makes `sync()` rethrow, and shows in `status().alarm`. `done()` marks `wrote`, so a replica's own writes count toward the cadence.
    * `serve.ts`: `note` after each committed step (`count: true`); `verified()` syncs before resuming a batch of claims (network errors retry; another error hands the claims back; an alarm, or an fs already untrusted, aborts them); `load` calls `observe` (current floor: a handler's checkpoint can't name more than the server committed); `giveUp(conn, false)` and `stop()` checkpoint first; `resync` aborts on `integrity`.
    * `apps/shared/node.ts`: `WorkerSignIn.integrity` (pins kept in the vault directory when there is one) and `fetch`; `WorkerContext.integrity`; `close()` closes the integrity (a last checkpoint); `ZEN_INTEGRITY=1`.
  * **Tests:** `packages/fs/test/integrity.test.ts` (5: honest incl. a from-scratch replica and a reopen; purge; rollback via `server.fork()`; fork via a third client that verified S1 and follows S2; wrong merge via a tampered `changes` response) and `apps/echo/test/integrity.test.ts` (3: honest with a successor on another device; a replayed older claim with the real token; a withheld checkpoint). 3× green. **Negative controls** (each run once, then restored):
    * rollback check off (`lowerThan` skipped): the rollback test fails ("no alarm");
    * the state compared nowhere (chain only, in `divergent` and `lowerThan`): the wrong-merge test fails ("no alarm");
    * same-`ops` comparisons off entirely: the fork and wrong-merge tests fail ("no alarm"); with only `divergent` off, `lowerThan` still catches both, as a rollback;
    * the svc floor check in `load` off: the successor accepts the replayed claim silently (the test times out waiting for the report);
    * the server's purge record reverted (and the test waiting for the trash to empty instead of for `ops`): `ops` stays 5 after the purge, and a replica that held the trashed file raises a false fork ("tree state changed at the same ops").
  * **Docs:** CLIENT.md §12 (trees, relayed connections), SVC.md §6 (handler checkpoints, the window), TECH_DEBT TD-SVC-STATE-ROLLBACK narrowed to the handler's database and the window, STATS §5.2 (tree state: 100,000 nodes from scratch ~0.5–0.6 s plus 60–130 ms for the first root; a change then `root()` ~55–95 µs; measured while the full vitest run was going, so upper bounds).
  * Local: the full vitest run green (578 passed, 12 skipped: the opt-in benches); `cargo test -p zen-server --test fs --test crdt` on local FDB green.
  * Pushed ae68e8e. **The push run (406) was green; the pull_request run (407) was red in `client`**, at one test outside this step: `packages/db/test/patterns.test.ts` "retry 'delay'", `decrypt` reading the topic's events. Not reproduced in 21 local runs; reading found no cause. Recorded as TD-TEST-OPEN-FAILURES item 6, and `Topic.decode` now names the event that didn't open (offset, topic, kind, epoch, key token, size), so a recurrence says what was stored.
  * **CI green on both runs of c756b0c** (push 408, pull_request 409). Step 6.4 is done, waiting on review and merge of PR #57.
  * PR #57 merged at a166e73.
* **Step 6.5** (the zen-db authenticated tier): planned below ("Step 6.5 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (62293a4), PR #58.
  * `spec:` commit a2af30b: zendb.md §14 rewritten (§14.1–14.8: levels, the state tree, PRF32 node ids, the NsRoot, writing, reading, freshness, cost), §2.1, §3.1 (the opening rule), §5.4.2, §15, §17; formats.md §14.1 names the NsRoot; suites.md defines PRF32; labels.md gains `db-state-value` and `db-ns-root`; svc.md §12 covers the handler's database; TD-SVC-STATE-ROLLBACK closed but for the window.
  * **Changed from the plan:** the entry is `esc(suffix) ‖ 0x00 0x00 ‖ VH`, not `… ‖ 0x00 ‖ VH`. With one terminator byte the VH that follows could start with `0xFF` and sort a key after its own extension (the sort keys of §5.1 avoid it only because no field encoding starts with `0xFF`). With `0x00 0x00` the prefix is order-preserving and prefix-free for any bytes, and a stored-key bound maps to the entry bound `esc(bound)`. A unit test checks both over keys with zero bytes and prefixes.
  * **Rust and wasm** (ca4f425): `kdf::prf32`, `IndexKeys::node_id32`, `DbKeys::state`; `prolly::build` takes the id length (the existing vectors are byte-identical); new `db::auth` (esc, `value_hash`, `state_entry`, `NsRoot` with a strict decode and `NH`, the reference `state_tree`). `zendb.json` gains `authenticated` (ten values, their tree at fanout 4, NsRoot seq 1 and 2); regenerating changed only that section. wasm: `DbKeys.state`, `nodeId32`, `build(…, idLen)`, `stateEntry`, `stateValueHash`, `nsRootHash`.
  * **Client, db, svc, apps** (c21351f):
    * `@zen/client`: `Transaction.onRead` (point answers, and each range page with the range it covers); range pages are opened once.
    * `@zen/db` `auth.ts`: `NsAuth` per `Db`, `TxAuth` per attempt (NsRoot read once; `prev` checked at floor + 1; `observe` with the floor at the attempt's start; point reads through a batched `lookup`, range pages through a scan of the covered entry range; `finish` rewrites the tree over the commit's writes and clears, writes NsRoot seq + 1, and the commit notes it, counted). 32-byte ids everywhere in an authenticated database, checked on fetch. Cached rows and catalog records keep their VH; a read-only authenticated transaction checks them against the tree instead of by version. `DbOptions.integrity` and the opening rule; CRDT tables refused.
    * **Found while testing (1):** the opening rule as planned turned the checks on before the stored level was known, so `integrity: 'authenticated'` over a basic database alarmed (its DbRecord isn't in any tree) instead of giving `format`. `init` now runs a first pass that only learns the level, and an authenticated database is read again, verified, before anything is written.
    * **Found while testing (2):** a successor that opened before syncing met a frozen NsRoot whose old nodes the next commit had deleted, and retried "a private index changed" until its attempts ran out. Nodes are deleted only by the commit that replaces the NsRoot, so a missing state-tree node now goes through the mismatch path: a conflict if the NsRoot moved, else a rollback ("a node of the state tree is missing"). spec commit 62b0fd9 says so in §14.6.
    * **Found while testing (3):** the ledger's audit view (a durable `on`) stopped for good after one batch exhausted its 8 attempts: with every writer conflicting on the NsRoot that is likely, and in basic mode merely rare. A durable batch that still conflicts now runs again after a backoff; a handler that throws still stops the subscription. New test "a durable handler that keeps losing conflicts runs again later, not stopped" (attempts 1 against a busy writer); with the change reverted it times out. §11.2 says so (62b0fd9).
    * **Found while verifying (4):** in the 3× runs, one of nine runs of the authenticated ledger reported a `conflict` from the statements consumer (the data still agreed). A consumer whose handler transaction ran out of attempts was reported and nacked, which spends a delivery and can end in the dead-letter queue. §11.3 already says a conflict retries the same delivery; the code now does so after a backoff, as for network errors. New test "a handler that keeps losing conflicts is run again, not nacked" (`maxAttempts: 1`, attempts 1, against a busy writer); with the change reverted it times out. The set (new tests, echo, both ledgers) then passed 8× in a row.
    * `@zen/svc`: a step that fails with `integrity` gives the connection up (`close`).
    * `apps/ledger`: `AUTH_DB_OPTIONS`; the tests moved to `test/suite.ts`, run by `ledger.test.ts` (basic) and `ledgerauth.test.ts` (authenticated, integrity attached, no alarms at the end). `apps/echo`: "a rolled-back handler database".
  * **Tests:** `packages/db/test/authenticated.test.ts`, 13: honest (every index kind, parts, migrations with a backfill and a paged drop, a reopen from the local store with kept rows used, a second device verifying), the scheduler with parts and `gc`, state equals scratch over random puts, deletes and a paged drop, concurrent writers on two devices (sum kept, conflicts happened), a rolled-back row (silent on a basic database, caught authenticated, by `get` and by a query), a rolled-back private index root, a withheld row (scan and fast lookup), a stale cached row read again, a rolled-back NsRoot as a consistent snapshot (passes in the window, caught after a sync and by the writer itself), a rolled-back NsRoot whose tree is gone (caught without a checkpoint), a fork via `server.fork()`, opening, CRDT refused. Also: the vectors' new section in `vectors.test.ts` (the incremental tree with 32-byte ids gives the reference nodes and NsRoots), `lookup` in `prolly.test.ts` (one `get` per level).
  * **Negative controls** (each run once, then restored):
    * the point-read check off: the rolled-back row test fails ("no alarm");
    * the range check off: the withheld row test fails ("no alarm");
    * the NsRoot's `observe` off: the consistent-snapshot test fails ("no alarm"), and the echo test fails (caught later as "a value that changed", not as the root going back). A first run showed the echo test passing with `observe` off: the adversary froze only the NsRoot, and the successor caught the missing nodes instead. Both tests now freeze a consistent snapshot (the NsRoot and every state-tree node), so they test freshness alone; the missing-node case has a test of its own;
    * the `prev` check off: the fork test still passes, caught by the same-seq hash check (`observe`'s divergence), which a fork at one seq always trips; the `prev` check covers a successor seen right after a floor;
    * the node id check: no test needs it. The server can't swap sealed nodes between keys (the AAD binds the key); the check guards against a member colluding with the server.
  * **Bench** (STATS §5.4, measured side by side on two fs): reads cost the same (row get cached 1.1 / 1.1 ms, not cached 2.3 / 2.6); an insert into 1,000 rows 601 B → 8,953 B and 1 → 6 ops; a 20-row transaction 8 KB → 81 KB.
  * **Docs:** DB.md §7.1 (new) and the error table, CLIENT.md §12, SVC.md §6 and §8, EXAMPLES.md §5, STATS.md §5.4.
  * **Checks (all clean):** `cargo fmt --check`; clippy `-D warnings` on zen-core and zen-wasm (host `--all-targets`, `--all-features`, `--no-default-features`, wasm32); `cargo test -p zen-core --features test-utils`; `scripts/build-wasm.sh`; `npm run check`; the new test file, the echo file and both ledgers 8× after the consumer fix; the full vitest run after the consumer fix (600 passed, 13 skipped: the opt-in benches); the Playwright smoke (17 passed).
  * Pushed 0113929. **CI green on both runs** (push 415, pull_request 416). PR #58 merged.
* **Step 6.6** (`crates/zen-client`, Rust): planned below ("Step 6.6 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (29b23dc), PR #59.
  * `spec:` commit bd6aa12: formats.md §14.5, exported pins (the record, its decoder's refusals, how a comparison is checked).
  * **zen-core** (d458c62): `integrity::ExportedPins` with `encode` and a strict `decode`; `integrity.json` gains `pins` (regenerating changed only that file: its description and the new section); `tests/vectors.rs` reads it back.
  * **zen-client** (4902c1d, cbd94a7): the library and the `zen-client` binary, as planned: `error`, `transport`, `client`, `session`, `device`, `acl` (its own `verify_next`), `fs` (a `KeyRing` per unlock holds the epochs, so integrity binds to keys without pointing back at the `Fs`), `kv`, `lease`, `integrity`, `pins`, `main`. It builds without C (rustls on zen-tls-rustcrypto, TLS 1.3 only).
  * **Changes from the plan:**
    * The tests are two files, `tests/client.rs` (sign-in, the ACL, headers and keyslots, KV, leases, TLS) and `tests/integrity.rs` (checkpoints, the cadence, the body budget, the pins file, two processes, the five attacks, the bench), not eight: each test binary links the whole server (about 240 MB here).
    * The cadence timer is a generation counter in the state (bumping it cancels the armed timer), not a `Notify`.
    * `Integrity::checkpoint_always()` writes one even without changes; the CLI's `checkpoint` uses it, since a checkpoint also carries the newest `seen` links and claims.
    * A newer epoch than the slot's gives `epoch_ahead` at once, as the TS client does (no header re-read).
    * Forks in Rust tests: `dump::copy` into a fresh embedded store and a second server. The copy keeps the first server's pinned origin, so its clients sign that origin, and a copy taken before any sign-in has nothing pinned (the ACL test signs in first).
  * **Found while testing:** a forged checkpoint can't be made with `sign_checkpoint` (it refuses another device's body), so the `invalid` test signs the body by hand: user 2's second device E puts, under device D's fingerprint, a body E signed. The server accepts it (both devices are the user's), and a verifier reports `invalid` (the signature isn't D's).
  * **Tests:** `client.rs` 6, `integrity.rs` 10 (and the opt-in bench), the `pins` unit test, 3× green. **Negative controls** (each run once, then restored):
    * the own-head check in `sync` off: the withheld-checkpoints test still passes, since `advance` refuses a head below the pin by itself (each catches it alone, as in the TS client); with both off, A still alarms, as a fork ("head can't be produced") instead of a rollback, so the test fails on the kind. That run first panicked on `seq - p.seq` in `advance`, reachable only with the guard off; it is now `saturating_sub`;
    * `check_seen` off: the withheld-checkpoints test fails (B's `compare_pins` finds nothing);
    * `observe_at`'s `lower_than` off: the rolled-back root test fails (no alarm);
    * the header pin's rollback branch off: the header test fails;
    * the same-seq hash check in `advance` off: the fork test fails (C on the other side syncs without an alarm).
  * **Interop** (3a6cd2e): `packages/client/test/rustclient.test.ts`, 3 tests (values both ways and the signed header; checkpoints verified both ways with `seen` links, pins compared both ways; a fork: the TS device writes on a copy, the Rust client on the original compares its pins and exits 3 with a `fork` of the TS device). The TS pins codec is exported (internally) and `integrityvectors.test.ts` checks it against the vector. 3× green; the client `integrity` tests once. CI's client job builds `-p zen-client` too.
  * **Bench** (STATS §5.6, release): sign 0.19–0.23 ms, seal 0.017, open 0.016, verify 0.14 (WASM signs in 2.6 ms); a debug build is 10–50× slower, since zen-core is a workspace member and isn't optimized there.
  * **Docs:** CLIENT.md §13 (new: the Rust client, its files, the CLI and exit codes, comparing pins between a phone and a server; Security notes are now §14), README (the crate), STATS §5.6.
  * **Checks (all clean):** `cargo fmt --check`; clippy `--all-targets -D warnings` on zen-client and zen-core (default, `--no-default-features`, `--all-features`); `cargo test -p zen-core --features test-utils`; `cargo check -p zen-core -p zen-proto --target wasm32-unknown-unknown`; `cargo test -p zen-client` 3×; `npm run check`; the two TS files 3×.
  * Pushed 9f5c676. **CI green on both runs** (push 420, pull_request 421): rust, pure-rust, fdb and client all pass. Step 6.6 is done, waiting on review and merge of PR #59. This line reaches `docs/MILESTONE-6.md` at the restart from main.
  * PR #59 merged at 311012b.
* **Step 6.7** (`crates/zen-blk`): planned below ("Step 6.7 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (3f600ab), PR #60.
  * `spec:` commit cfbd5e9: new spec/blk.md (keys, leaves and the tree, the root record, writing, reading, freshness with the writer's pin and the window, leakage); formats.md §14.1 names the vol id and RH, §8 points at zendb.md §14 and blk.md; labels `blk-leaf`, `blk-node`, `blk-root`; the spec README row; TECH_DEBT TD-BLK-SNAPSHOTS (snapshots and clones, resize, delete with a tombstone, readers at a snapshot).
  * **zen-core** (aae466e): `blk.rs` (`depth`, `node_of`/`slot_of`, `leaf`, `node_hash`, node bytes, `VolumeKeys`, `RootRecord` with a strict decode, `root_hash`, the reference `tree`), unit tests (decode refusals, the tree by hand, keys); new `blk.json` (regenerating changed only that file); `tests/vectors.rs` reads it back.
  * **zen-blk** (c179f22): `pin.rs`, `tree.rs` (the verified tree: levels ≥ 2 and the top kept, level 1 in an LRU; `plan`, top-down `verify`, copy-on-write `apply`), `volume.rs` (open, reads, buffered writes and discards, flush in commits within the server's limits, the unknown-outcome path, lease renewals, the idle flush, close). zen-client gains only `pub use write_private` (the atomic file write the pins file uses).
  * **Changes from the plan:**
    * No `keys.rs`: the stored keys are zen-core's `VolumeKeys`, which the vectors use too.
    * Reads and commits share an async read-write lock on the tree: a commit holds it from fetching its paths until the new root is in place, so a read never checks a node fetched after a commit against the tree from before it. Reads run concurrently with each other.
    * Blocks and nodes are fetched raw (`/v1/kv/get`), since the leaf is over the sealed bytes; commits are built as `Commit`s and sent with `send_commit`, so the leaf can be computed before sending.
    * A writer whose read or commit finds a moved root gets `lease_lost` (another writer moved it), not `volume_changed`.
    * Read-only openers given a pin file check and raise it too, so a reader can carry a pin from one server to another (the fork test).
  * **Found while testing:** the window test first froze only the root record: the reader then fetched the current nodes under the old root and alarmed at once ("node (2, 0) doesn't match its parent"). That's right, but it isn't the window; the test now freezes a consistent snapshot (the root, its two nodes and the block), as in 6.5.
  * **Tests** (`tests/volume.rs`, the zen-client harness by `#[path]`): 12 and the opt-in bench, 3× green (about 36 s each, the model test most of it):
    * basics: create/open/read/write (zeros, aligned and unaligned, `range`, `read_only`, `exists`, `not_found`, `integrity_needed` without integrity and without a signer); 2,000 random operations over 100 MiB (depth 3, batch 16, a reopen every 400) against a model, the stored root and nodes checked against the reference tree every fourth flush and at the end, and a fresh reader over the whole volume; batches (3 × 256 blocks commit on their own; with `max_commit_bytes` 2 MB a batch of 1,000 splits into several commits); the idle flush
    * writers: a crash (500 ms lease: `busy` while it lasts; the pin file one commit behind; A and B read back, the unflushed C doesn't); a second writer (`busy`, read-only works, then it writes after the close); a stale fence (renewals paused, the lease lapses, B writes; A's commit on paths it holds gets `lease_lost`, writes nothing, and its later writes fail)
    * attacks: a reader whose root moved (`volume_changed`, no alarm); a rolled-back root as a consistent snapshot (the writer's pin catches it with no checkpoint yet; another device before the checkpoint accepts it, the window; a third after the checkpoint alarms); a rolled-back block (the leaf), then a rolled-back node with the block it lacks withheld (the node), and the writer's own read after a reopen; a fork (`Server::fork`: each side writes its own seq 3; a reader that pinned side 1's root, on side 2, alarms)
    * the checkpoint entry `[4, vol id, seq, RH]` in the writer's head body, and in another device's floor after a sync
  * **Negative controls** (each run once, then restored):
    * the pin's rollback check off: the writer side of the rolled-back root test opens without an alarm;
    * `observe_at` off: the third device opens without an alarm;
    * the leaf check off: the rolled-back block is served;
    * the node check off: first, the leaf check still caught it (the old node has no slot for block 8, which was stored); the test now also withholds block 8, so the old node and block agree, and with the node check off zeros are served;
    * the fence off: the stale writer's flush succeeds;
    * the pin's same-seq check off: the forked reader opens without an alarm.
  * **Bench** (STATS §5.7, release): sequential writes 49 MiB/s with a flush every 4 MiB; a random 4 KiB read 0.14 ms, cold or warm (a read's missing nodes come in the same request as its block); a one-block flush 1.6 ms.
  * **Docs:** README (the crate), STATS §5.7.
  * Pushed 91228b4. **Both runs red (push 429, pull_request 430), in `rust`, `pure-rust` and `fdb`, at Clippy; `client` green.**
    * Cause: CI's `stable` toolchain is Rust 1.99, this container's 1.97. Clippy 1.99 adds `chunks_exact_to_as_chunks`, which flags `b.chunks_exact(32)` in `zen-core/src/blk.rs` `node_from_bytes` (line 90). Clippy stopped at zen-core, so zen-blk wasn't linted under 1.99 at all.
    * Fix (117a05b): `node_from_bytes` uses `as_chunks::<32>()` (stable since 1.88) and refuses a rest or a count other than 128. rustup updated the toolchain here to 1.99.1, the same as CI; clippy `--all-targets -D warnings` on the whole workspace is clean in all three feature sets CI uses, and the zen-core and zen-blk tests pass on it.
  * **CI on 117a05b (push 431, pull_request 432): red in `pure-rust` only**, both runs at the same place: `a_crash_loses_only_unflushed_writes`, the first flush after `create` got `lease_lost`.
    * Cause, two bugs in the engine's lease handling:
      1. The lease was acquired before the integrity sync of the open, and renewals started only once the open finished, `ttl / 3` later. On the slower `pure-rust` runner the open outlasted the test's 500 ms lease: the fenced commit got `claim_lost`.
      2. Renewing a lease that has lapsed doesn't fail: the server grants it again with the next token (api.md §16), and `Lease::renew` takes that token, while the volume kept fencing with the old one. Between the lapse and the renewal another writer may have written, so that is a lost lease.
    * Reproduced here with a 600 ms delay after the sync of the open: the create commit got `claim_lost` (412).
    * Fix: the sync runs before the lease is acquired (it needs none), so the lease covers only the root read, the checks and the create commit; renewals are due `ttl / 3` after the last lease request that held it was sent, not after the previous sleep; a renewal that returns another token marks the volume `lease_lost`; a transient renewal failure retries after `ttl / 12`, not a whole interval later.
    * Checked: with the same 600 ms delay, the open and its commits now succeed (the test then fails only at its own "busy while the lease lasts" check, since the injected delay outlasts its lease); with a 300 ms delay inside the leased part of the open, the test passes. The crash and stale-fence tests now use 1.5 s leases and 2 s waits, so a slow runner's round trips stay well inside them.
    * The zen-blk tests 3× green, and once with `--no-default-features` (the `pure-rust` configuration).
  * **CI green on both runs of a41a906** (push 433, pull_request 434): rust, pure-rust, fdb and client all pass. Step 6.7 is done, waiting on review and merge of PR #60. This line reaches `docs/MILESTONE-6.md` at the restart from main.
  * PR #60 merged.
* **Step 6.8** (`crates/zen-ublk`, the FUSE export and ublk): planned below ("Step 6.8 in detail"). Restarted from main; the log above is now in the repo copy too. Plan committed (c0de3d3), PR #61.
  * `spec:` commit 30154a2: blk.md §4, two new bullets. A lost answer is settled before the writer reads or commits again. A delete of a block the tree shows absent is left out, and a commit that would change nothing isn't made.
  * **zen-blk:**
    * Writes and discards hold a write lock from their edge reads until their blocks are buffered. A full batch flushes after the lock is released.
    * `commit` drops deletes whose verified leaf is already zero (`Tree::leaf_in`), and a chunk left empty commits nothing.
    * **Found while implementing:** the unknown-outcome path. When `send` couldn't read the root after its replays either, it gave the error back. The flush then put its blocks back, and the next commit wrote seq + 1 again, over a commit that may have applied: two root records at one seq. Now `State.unknown` keeps `(RH before, RH written)`, and the next read or commit first settles it from the stored root (`settle`):
      * the old root: nothing applied
      * its own root: the tree restarts from it, and the pin and the checkpoint entry follow
      * anything else: `lease_lost`
  * **zen-client:** new `cli.rs`. `ConnectArgs` (a clap `Args`: the six options, with `pins_path` and `beside_key`) and its `connect` (sign in, attach, unlock, sync), plus `exit_code` and `print_error`. `main.rs` uses them, with the same behavior.
  * **zen-ublk** (new crate):
    * `disk.rs`: the adapter. Errnos, transient retries up to `--io-timeout-ms`, fail-closed `Failure {code, message}`.
    * `fuse.rs`: fuser 0.18 without libfuse, 4 threads, a task per request, `fsync`/`flush` → flush, `fallocate` → discard.
    * `ublk.rs` (feature `ublk`): libublk 0.4.8. 4 KiB blocks, discard, `VOLATILE_CACHE|FUA`, smol queues awaiting tokio `JoinHandle`s, `clear_stale`.
    * `main.rs`: `create`, `info`, `fuse DIR` and `ublk`, with `ready <path>`, SIGINT/SIGTERM, and exit codes 0/1/3.
    * libublk builds here (bindgen with libclang 18), and clippy passes with every feature.
  * **Changes from the plan:**
    * `info` prints size and root, without whether a writer holds the lease (that would need a lease request).
    * An unmount that fails (a loop device still on the file) keeps serving and says so.
    * Without C: the crate adds none. Its only C is blake3's SIMD code, which zen-client already has, and blake3's `pure` feature (as the server's) turns it off.
  * **Found while testing:** the FUSE serve loop polled the signal future again after it completed ("`async fn` resumed after completion"), so a SIGTERM exited 101. It's now polled only until the unmount succeeds.
  * **Tests:**
    * zen-blk `tests/volume.rs` adds 2: concurrent partial writes into one block all land; discarding absent blocks commits nothing. On an empty 1 GiB volume, the seq stays 1. Of 3 written blocks, one commit deletes exactly those, checked against the reference tree.
    * zen-ublk `tests/disk.rs`, 5, in the default suite without root:
      * requests and errnos: unaligned I/O, FUA, discard edges, `EINVAL`, `EROFS`
      * a lost lease fails closed (exit 1); an alarm fails closed (exit 3)
      * an outage, through a TCP proxy, is retried; with a 300 ms timeout it gives `EIO`, the disk isn't failed, and the next flush commits
      * a commit whose answer was lost, while the server stayed away for 5 s, is settled: seq 3, one record per seq, and the reader and the reopen agree
      * plus 2 unit tests: sizes, default paths
    * zen-ublk `tests/devices.rs`, 5, feature `device-tests`, as root:
      * `fuse_file`: size and mode; `pwrite` aligned and not; a short read at the end; `EPERM` on resize; `busy`; read-only `EROFS`
      * `fuse_ext4_round_trip`: loop, `mkfs.ext4` (with its discard), files, `sync`, stop, restart, `e2fsck -fn`, compare; then read-only with `ro,noload`
      * `fuse_writer_killed`: SIGKILL after a synced and an unsynced set; `busy` within the lease; then `e2fsck -fy` ≤ 1, `-fn` 0, and the synced set intact
      * `ublk_ext4_round_trip`, `ublk_writer_killed`: compiled and run here, where they skip (no `/dev/ublk-control`); CI requires them
  * **Negative controls** (each run once, then restored):
    * the write lock off: `concurrent_partial_writes` fails ("before the flush")
    * the skip of absent deletes off: `discarding_absent` fails (1,025 commits instead of 1)
    * `settle` off: the lost-answer test fails (seq 2: the writer wrote seq 2 again)
  * **CI:** a new job `devices`. It installs fuse3 and e2fsprogs, runs `modprobe ublk_drv` (falling back to `linux-modules-extra`), runs clippy with `device-tests,ublk`, then the tests as root (`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=sudo -E`) with `ZEN_TEST_REQUIRE_UBLK=1`.
  * **Docs:** new docs/BLK.md (running it, durability, integrity), the README row, CLIENT.md §13 ("Sharing it").
  * **Checks (all clean):**
    * `cargo fmt --check`; clippy on the workspace `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`)
    * `cargo test -p zen-blk -p zen-ublk` 3×; `cargo test -p zen-client`; `cargo test -p zen-ublk -p zen-blk --no-default-features`
    * `cargo test -p zen-ublk --features device-tests,ublk --test devices` 3× as root; nothing is left mounted or attached afterwards
    * `rustclient.test.ts` (the CLI refactor)
  * Pushed cf2db2b. **Both runs (push 438, pull_request 439) are red, in `devices` only**: `rust`, `pure-rust`, `fdb` (clippy `--all-features` builds libublk) and `client` are green.
    * In `devices`, `modprobe ublk_drv` worked on the hosted runner, and the three FUSE tests passed as root.
    * Both ublk tests fail at their first `zen-ublk ublk --id 4x`: the daemon exits 1 with "ublk: the control device: io_uring IO failure", after libublk prints "device reload json failed".
    * **Cause:** `clear_stale(id)` calls `UblkCtrl::new_simple(id)` to look for a device a killed daemon left. `new_simple` for an id ≥ 0 doesn't only open the control device: it reloads that device's json and **reads its info from the driver** (`handle_device_lifecycle_async` → `read_dev_info_async`). That fails when there is no such device, which is the normal case. So `new_simple` returns an error, and `clear_stale` treated it as fatal. Read in libublk 0.4.8 `src/ctrl.rs` 1180–1191.
    * **Fix (`ublk.rs` `clear_stale`):**
      1. When `/dev/ublkc<id>` doesn't exist there is no device: return `Ok`.
      2. Otherwise `new_simple(id)` (which has read the info already, so the separate `read_dev_info` call goes); a live daemon pid gives `busy`, a dead one gets `del_dev`.
    * **Not yet exercised anywhere:** what comes after the device is added. That covers the I/O path, FLUSH/FUA, the read-only export without `--id`, `kill_dev`/`del_dev` on SIGTERM, and deleting a dead device after SIGKILL. This container has no ublk driver, so the next CI run is the first test of all of it. Each red gets pursued to its cause the same way.
    * Fixed in e800684. fmt, clippy with `ublk,device-tests`, and the device tests here are clean (the FUSE ones pass; the ublk ones still skip here). Pushed; CI pending.
  * **CI on 49e9acd (push 440, pull_request 441):** `rust`, `pure-rust`, `fdb` and `client` are green. `devices` is past `modprobe` and clippy, but "Test (as root)" has been running for 8+ minutes (the whole step took 2 on cf2db2b), and its log can't be read while it runs. So the device is now added, and the I/O hangs.
    * **Cause, read in libublk 0.4.8 `src/uring_async.rs` (`wait_and_handle_io_events` → `run_uring_tasks` → `uring_poll_io_fn`):**
      * A queue thread blocks in `io_uring_enter`, waiting for a completion on the queue's ring, for up to `idle_secs` (20 s). It ticks the smol executor only after a completion arrives.
      * Our request tasks await a tokio `JoinHandle`. When the tokio task finishes, it wakes the smol task, but nothing completes on the ring, so the thread stays asleep.
      * So each request waits for some other completion, or for the 20 s timeout: `mkfs` and the copies crawl.
      * libublk's own targets never wait outside the ring: the loop target submits its I/O as SQEs on the queue's ring (`ublk_submit_sqe_async`).
    * **Fix (`ublk.rs`), so the wake-up goes through the ring:**
      * Each tag task makes an eventfd (`libc::eventfd(0, EFD_CLOEXEC)`, closed when the task ends).
      * A request spawns its `Disk` work on tokio. The result goes into a `oneshot`, and then the task writes 1 to the eventfd.
      * The smol task meanwhile awaits an `io_uring` READ of 8 bytes from that eventfd, through `q.ublk_submit_sqe(...)`. Its completion wakes the ring, and the result is then read from the `oneshot` with `try_recv`.
      * New optional dependency `io-uring = "0.7"` (feature `ublk`; 0.7.15 is already in the lock through libublk).
    * **CI guard:** the `devices` job gets `timeout-minutes: 20`, so a hang fails within 20 minutes instead of 6 hours. The test helper's `serve` also gives up after 60 s without a `ready` line.
    * The hung run can't be cancelled from here (no tool for it). It stops at GitHub's 6-hour limit unless cancelled in the Actions UI.
    * Fixed in 64e4771. fmt, clippy with `ublk,device-tests` and the zen-ublk tests here are clean (the FUSE device tests pass; the ublk ones skip). Pushed; CI pending.
  * **CI on ab1297c (push 442, pull_request 443):** `rust`, `pure-rust`, `fdb` and `client` are green. `devices` ran into its 20-minute timeout. The 3 FUSE tests passed; both ublk tests hung.
    * **Cause, from the log:** each `zen-ublk ublk` daemon panicked within a second in a tokio blocking thread: libublk `ctrl.rs:1469`, "Control ring not initialized. Call ublk_init_ctrl_task_ring() first or use UblkCtrl constructor."
      * libublk 0.4.8 keeps the control io_uring **per thread** (`thread_local! CTRL_URING`). Only the thread that ran the `UblkCtrl` constructor has one.
      * The daemon built the control device in one `spawn_blocking` thread, then ran `run_target` (its ADD/START commands) in another, and `kill_dev`/`del_dev` in more. Every thread after the first had no ring.
      * The daemon then never printed `ready`. After the 60 s watchdog's SIGKILL, the test still didn't finish: the process exit or its `wait()` never returned with the device added but not started. This is most likely in the kernel, which I can't see from the log.
    * **Fix (`ublk.rs`):** `ensure_ring()` makes the calling thread's control ring if it has none. It goes through libublk's public `ublk_init_ctrl_task_ring`, building an `IoUring<squeue::Entry128>` of depth 16 as libublk's default does. It's called at the start of every function that sends a control command from a thread: `serve` (`run_target`), `stop` (`kill_dev`), `delete` (`del_dev`) and `clear_stale`.
    * Fixed in f0b6bfb. fmt, clippy with `ublk,device-tests` and the zen-ublk tests here are clean. Pushed.
  * **CI green on both runs of 3d3eb3b** (push 444, pull_request 445): rust, pure-rust, fdb, client and devices all pass. In `devices`, the ublk tests ran over `/dev/ublkbN` for the first time: ext4 round trip, then a killed writer, its stale device deleted, `e2fsck`. Step 6.8 is done.
  * PR #61 merged.
* **Step 6.9** (docs, bench, outcome): planned below ("Step 6.9 in detail"). Restarted from main; the log above is now in the repo copy too.

## Step 6.3: the red on run 396 (`apps/chat` "who's online")

**What the code shows** (read-only so far). `whoIsOnline` is `db.requestAll(presence topic, 'ping', {}, {timeoutMs})`; each instance's `servePresence` answers with `db.reply(m, me, {ephemeral: true})`, an `epub` to the asker's inbox topic. The server path was ruled out by reading:
* the per-device rate limit (64 KB/s, 1 MB burst) is far above a few replies
* the embedded backend commits under one lock, so the ring's versionstamps become visible in order and the tailer can't skip one
* the embedded watch future is enabled when armed, so the tailer can't miss a wake-up
* the client's replay filter is per (topic, sender, stream), and each stream's clock only ticks up

Two races in `@zen/db`'s request path (`packages/db/src/txn.ts` `ask`, `packages/db/src/reply.ts` `Inbox`) can each lose a reply:
1. **The waiter is registered too late.** `ask` registers the waiter for `corr` only in `raw.onCommit`, after the commit's HTTP response. A responder gets the ping over its stream, so its reply can reach the asker before that response does. `Inbox.deliver` drops a reply with no waiter ("late, repeated or foreign"). The fastest reply is lost, and the asker answering itself is the fastest path: bob in round 1.
2. **The inbox's ephemeral subscription may not be active yet.** `ask` calls `inbox.start()` but doesn't wait for it before the commit. On a `Db`'s first request, a reply published before the `esub` is active is gone, since ephemeral messages have no history. The inbox's comment ("no reply can come before it") holds only for the log subscription.

**Fix:**
* `Inbox.expect(corr, type, all, opts)` registers the waiter at ask time and keeps any reply that arrives from then on. Its timer starts at `arm()`, called from `onCommit`, so `timeoutMs` still counts from the commit.
* A failed or abandoned attempt (`raw.onSettle` with `failed` or `unknown`, from this step) drops the waiter. The promise behaves as today for a transaction that throws (the existing test "a request in a transaction that throws is never sent").
* `DbTransaction.finish()` awaits `inbox.start()` when the transaction made a request, so both inbox subscriptions are active before the commit is sent.

**Tests that fail without the fix** (`packages/db/test/patterns.test.ts`):
* **"a reply that arrives before the asker sees its commit is kept"**: the asker's `Db` uses a client whose `fetch` holds back `/v1/commit` responses for 400 ms. A responder on another session replies ephemerally at once, so the reply arrives first. Without the fix `requestAll` returns `[]`; with it, the reply.
* **"a first request waits for the inbox's ephemeral subscription"**: a fresh asker `Db` over a client whose `WebSocket` delays outgoing `esub` frames by 400 ms. Without the fix the reply is published before the subscription and lost.
* Run each first against the unfixed code to see it fail, then with the fix. The log says so.

**Then:**
* Run the chat presence test 30× locally, and the `packages/db` and `apps` suites once.
* Remove item 4 from TD-TEST-OPEN-FAILURES (`spec/TECH_DEBT.md`), saying what it was.
* Log it here and in `docs/MILESTONE-6.md`, then commit `db: a request's replies are kept from the ask, and the inbox is live before the commit`.
* Push, and confirm CI is green on both runs.

**Done:**
* Both new tests failed on the unfixed code (`[]` instead of `['b']`) and pass with the fix.
* The chat presence test passed 30× in a row. Item 4 of TD-TEST-OPEN-FAILURES is marked resolved, with its cause.
* `DbTransaction.abort()` and a manual transaction whose `finish` fails also settle the raw transaction, so a request made in them is forgotten.

**Found while verifying (a full local vitest run):**
* **A CRDT field write could lose to an earlier one** (`packages/db/test/crdt.test.ts`, "one field, the later write wins", once under the full parallel load). Each installation's clock corrects wall time by its estimate of the server's offset (`/v1/info` round trip), so two installations' clocks differ by up to half a slow round trip. An installation that read a value and then wrote the field could stamp its write earlier than the value it read: reads didn't observe the registers' `hlc`.
  * Fix: `crdtGet` and `crdtRange` observe every `row` and `lww` register's `hlc` (`observeUnchecked`: not when over a minute ahead). zendb.md §19.4 gains a "Causality" bullet.
  * New test "a write after reading one stamped by a clock ahead still wins (§19.4)": a clock persisted 2 s ahead writes, the other installation reads then writes. It failed without the fix ('from ahead' won) and passes with it. The existing test now reads before its second write, which is what "later" means between installations.
* **The ledger sum** (TD-TEST-OPEN-FAILURES item 1) reproduced locally for the first time, in the same full run, in both ledger tests. The diagnostic: trader 3's transfer acct-3→acct-2 wrote acct-3's balance from the value before trader 3's previous transfer (165, not 136). Each trader `Db` runs 3 trade loops at once, sharing its row cache. Ruled out by reading: the server's `expect` (a `get` inside the commit's transaction at `read_version`, which adds a read-conflict range), the embedded backend's conflict check, and the client's `expectKey`. Not caused by this step: the ledger uses no CRDT tables, requests or integrity. Investigation continues after this push.
* Pushed 7c1d932 (the inbox and CRDT fixes): **CI green on both runs** (push 397, pull_request 398).
* **The ledger sum, found:** a lost update through the row cache. `put` reads its row again (`Table.put` → `read`), and each trader `Db` runs three trade loops that share the cache:
  1. T2's `get` uses the stale cached row (165, version v0) and expects v0.
  2. T1 commits (the cache drops the row), and another loop's read caches the new row (136, v1).
  3. T2's `put` reads the row again from the cache: `useCached` called `expectKey(key, v1)`, which replaced v0.
  4. T2 commits 165 − 39 = 126 expecting v1, the current version, and the commit passes.
  * Fix: the first version expected of a key in a transaction stands. `Transaction.expectKey` keeps an existing expectation (long-mode server reads already did), and `DbTransaction.useCached` keeps the first record of a key. A transaction that saw two versions of a row now conflicts and re-runs.
  * Tests that failed without the fix: `txn.test.ts` "a row read twice while the cache changes in between: the commit conflicts, no lost update" (it committed once, on stale data), and `kv.test.ts` "expectKey: the first version expected of a key stands, so two versions conflict" (it committed).
  * Item 1 of TD-TEST-OPEN-FAILURES is marked resolved, with its cause.
  * The full vitest run, 3× after the fix: 570 passed, 12 skipped (the opt-in benches).
* **CI green on both runs of 37699a0** (push 399, pull_request 400). Step 6.3 is done. PR #56 merged at 883c9ba.

## Facts the design rests on (survey)

* **Signatures** (`crates/zen-core/src/sig.rs`):
  * `SigningIdentity`, `PublicIdentity` (1,986 B) and the hybrid `sign`/`verify` (3,375 B, Ed25519 + ML-DSA-65, both must verify).
  * `SM = "zen/v1/sig" ‖ 0 ‖ lp(purpose) ‖ msg`, with purposes whitelisted in `labels::SIG_PURPOSES`.
  * `DeviceSecret` (signing seed and X-Wing key), with `issue_device_cert`/`verify_device_cert`.
  * The purposes `SIG_COMMIT`, `SIG_CHECKPOINT`, `SIG_EVENT`, `SIG_TREE_CHECKPOINT` and `SIG_DB_ROOT` are registered but unused. There is no purpose yet for an fs header or a volume.
* **WASM** (`crates/zen-wasm/src/crypto.rs`):
  * `SigningIdentity.sign(purpose, msg)` and `verifySignature`.
  * `DeviceSecret` exposes only `signSession`; there is no generic device `sign`.
  * `label()` lacks the checkpoint purposes.
* **Who holds a signing key:** device-key (method 1) and password-key (method 6) sessions only (auth.md §11).
  * The vault record is `1 ‖ lp(identity public) ‖ device seed ‖ lp(cert)` (`packages/client/src/local/vault.ts`).
  * For other sessions, `Caller.device` is a credential id (`auth.rs:89`).
* **The precedent for a signed chain:** the ACL.
  * `SignedAcl {doc, sig, signer}` with `version` and `prev_hash` (formats.md §9).
  * The server validates successors (`acl.rs` `validate_successor`) and CAS-writes `pack("acl", v)` and `acl_head`.
  * The client walks and pins the head (`packages/client/src/acl.ts` `verifyNext`).
* **The fs op chain:**
  * Computed by the server: `chain_next` in `zen-core/src/fs.rs:648`, kept in the tree header (`tree.rs` `Header {ops, chain, resync_before}`).
  * Read through `/v1/fs/tree/chain`.
  * The client recomputes it (`tree.ts` `verifyChain`).
  * There is no state hash.
* **Events:**
  * The body is `0x01 ‖ sender_fp ‖ hlc ‖ lp(causation) ‖ payload` (formats.md §5).
  * There is no device sequence, no previous hash, and nothing the server records per device.
  * Heads are `lh` per topic and `gh` per fs.
* **The zen-db prolly tree:**
  * Node ids are `PRF16(K_node, CBOR(Node))`.
  * Roots are at `D ‖ ("r", index_id, shard)`, nodes at `("n", …)`, rows at `("t", table_id, pk)` in PRF order.
  * There is no row tree and no namespace root.
  * `DbRecord.integrity` refuses anything but `"basic"` (zendb.md §3).
  * The Rust reference builder is `zen-core/src/db/prolly.rs`; the TS incremental one is `packages/db/src/index/prolly.ts`.
* **Commits:**
  * `CommitResult {commit_version, versionstamp, …}`.
  * There is no per-namespace version.
  * Fencing applies only to consume steps.
* **No Rust client exists.** zen-server has `reqwest 0.13` with `rustls-no-provider` as a dev-dependency. The device-key file is zen-mount's JSON `{identity_seed, device_seed, cert}`.
* **Paged file layout** (formats.md §11.3–§11.6): immutable random chunk ids per write, so it doesn't suit in-place blocks. zen-ublk uses KV blocks, as DESIGN-3 §1 maps them (`(fs, "blk", idx)`).
* **Environment:**
  * Here: kernel 6.18 with no ublk or NBD and no modules; FUSE, loop devices, io_uring, root, `mkfs.ext4` and `mount` all work.
  * GitHub runners: `sudo modprobe ublk_drv` (possibly after `linux-modules-extra-$(uname -r)`).
  * libublk 0.4.8 is smol-based, MSRV 1.80. Rust here is 1.97.

## Design

### Checkpoints (formats.md §14, new; api.md §15; keyspace.md §3.10)
* **`CheckpointBody`** (deterministic CBOR):
  * `{v: 1, fs, device: fp, seq, prev: H(previous body) | 0³², hlc, acl: (version, hash), header: (version, hash)}`
  * `objects: [entry]`, where an entry is one of:
    * `topic {id, head (offset), mine: (dseq, dchain)}`
    * `tree {id, ops, chain, state}`
    * `db {ns_token, seq, root_hash}`
    * `vol {vol, seq, root}`
    * `svc {svc, conn, in_acked, out_head}`
  * `seen: [(device fp, seq, H(body))]`
* **Signing and sealing:**
  * A device signs `SM(zen/v1/sig/checkpoint, body)`.
  * The body and signature are sealed under the fs key of the epoch (a new seal kind 11). The server stores ciphertext, so it learns which devices checkpoint and when, not which objects.
* **The server** stores `pack("dk", fs, device_fp, seq)` (`ck` is taken by chunks). It accepts only `seq = last + 1` for a device of the caller's user (in the ACL, or registered with a cert), and keeps `pack("dkh", fs, device_fp)` as that device's head. Step 6.1 below has the exact formats.
* **API:**
  * `POST /v1/checkpoint/put`
  * `POST /v1/checkpoint/heads`: every device's head seq
  * `POST /v1/checkpoint/get`: by device and seq range
  * a stream subscription for new heads
* **Client rules:**
  * Verify the signature against the device's cert in the pinned ACL.
  * Verify `prev`, and that `seq` extends the pinned head of that device.
  * For each object, refuse a server answer older than the newest verified head.
  * Report a fork when two bodies of one device share a seq, or when `seen` links don't extend.
* **Pins** live in the local store, per fs: own head, other devices' heads, and per-object heads. `exportPins()` / `comparePins()` give the out-of-band check.

### The signed fs header (formats.md §12)
* The admin signs `{version, prev_hash, header bytes}` with a new purpose, `zen/v1/sig/fs-header`.
* The server validates the successor as for the ACL (admin of the pinned ACL, version + 1, `prev_hash`).
* Clients walk and pin it. A header with no signature (an fs made before 6) is accepted until its first signed version, after which an unsigned one is refused.

### Event chains (formats.md §5, `body_version 2`)
* The body gains `dseq` (per device and topic) and `dprev = H(previous event's body)` of that device on that topic.
* A reader keeps each device's last `(dseq, hash)` per topic, and detects a gap or a rewrite.
* Checkpoints carry `mine` (the device's own position) and `head` (the topic head it has read through, which closes G26).
* `sig_every` is a client option per topic. Its default is the device cadence; `1` checkpoints every append.

### fs tree checkpoints (fs.md §9)
* **`state`** is a canonical hash of the materialized tree computed by the replica: sorted `(node id, parent, name token, meta digest, manifest digest)`, built as a Merkle fold so it updates incrementally.
* A device checkpoints `(ops, chain, state)` after applying ops.
* Another device at the same `ops` must compute the same `chain` and `state`. A difference is a wrong merge or a forked history.

### svc handler checkpoint (svc.md §12)
* The handler's device checkpoint includes `svc` entries for the connections it served, every `sig_every` steps and on release.
* A successor refuses a claim whose `(in_acked, out_head)` is below the newest verified entry. That closes TD-SVC-STATE-ROLLBACK up to the checkpoint window.

### The zen-db authenticated tier (zendb.md §14)
* **`integrity: "authenticated"`**, set at creation.
* **Node ids** are `PRF32`.
* **A row tree:** a prolly tree per table keyed by pk sort key, with leaf entries `(pk, H(sealed row))`, kept in the same commit as the row.
* **`NsRoot {seq, prev, catalog_hash, roots: [(index or row tree id, shard, root)], writer: fp}`**, MAC-chained with `K_db`. Every commit rewrites it, so writers of one authenticated namespace conflict on it (documented: use it for balances and permissions).
* **Reads verify** the row against the row tree path from a verified `NsRoot`. Queries verify the index path, and `NsRoot.seq` is checked against pins and checkpoints.
* **Cost:** about `height` extra node reads per uncached point read. Measured in STATS.
* **The demo:** the ledger app gains an authenticated namespace option.

### Fenced leases (api.md §16)
* `POST /v1/lease {fs, name (token), ttl_ms, release?}` returns `{token}`. A new claimant gets a higher token once the old lease lapses.
* `Commit.fence {name, token}`: a stale token gives `claim_lost` (412), atomically with the rest of the commit.

### Native client and block engine (`crates/zen-client`, `crates/zen-blk`, `crates/zen-ublk`)
* **`zen-client` (Rust):**
  * reqwest with rustls on `zen-tls-rustcrypto`
  * sign-in method 1 from the device-key file
  * the fs header and a device keyslot (type 3) unlock
  * the ACL fetch and walk
  * KV get, range and commit (with `fence`)
  * checkpoints and pins (a file)
* **`zen-blk`:**
  * **Volume record** `(fs, "blk", vol, "meta")`: size and block size 4,096.
  * **Blocks** at `(fs, "blk", vol, idx)`, sealed kind 1 with AAD binding `(fs, vol, idx, gen)`.
  * **The Merkle tree:**
    * fan-out 128, in nodes at `(fs, "blk", vol, "t", level, i)`
    * leaf `H(idx ‖ gen ‖ tag)`
    * root record `{seq, prev, root, writer}`, MAC-chained
  * **Writes** are buffered and committed on FLUSH/FUA, or when the batch reaches its size or time limit. Each commit carries the blocks, the changed tree path, the root and the fence.
  * **Reads** are verified against the cached upper tree.
  * Sparse blocks read as zeros.
  * The pinned root seq lives in a local file, and in checkpoints.
* **Frontends** (`zen-ublk` binary):
  * `zen-ublk --url --fs --vol --device-key FILE [--create SIZE] [--read-only]`
  * ublk mode: libublk, `/dev/ublkbN`
  * `--fuse DIR` mode: one file, `DIR/disk.img`, for `losetup`

## Steps (one PR each)

**6.0, plan:** above.

**6.1, spec (`spec:` commits) and zen-core** (detailed below; zendb.md §14 and spec/blk.md move to 6.5 and 6.7):
* **Spec text:**
  * formats.md §5 (event body v2), §12 (the signed header), §14 (checkpoints, kind 11)
  * labels (`sig/fs-header`, `db-ns-root`, `blk-root`, `ckpt-hash`, `event-chain`, `tree-state`)
  * keyspace.md §3.10 (`ck`, `ckh`, `ls`) and the `blk` layout
  * api.md §15 and §16, and `Commit.fence`
  * zendb.md §14, fs.md §9, svc.md §12
  * a new spec/blk.md
* **zen-core functions and vectors:**
  * checkpoint encode, hash, sign and verify
  * the header chain
  * the event chain
  * tree state folding
  * `NsRoot` and the row-tree leaf
  * block leaves and tree nodes
* **zen-wasm bindings:**
  * a generic `DeviceSecret.sign` with the purposes
  * the codecs

**6.2, server:**
* the checkpoint log (put, heads, get, stream), with per-device CAS and the device check
* signed header storage and successor validation
* fenced leases and `Commit.fence`
* `/v1/info` features
* a `test-utils` **adversary**: roll a key back to a recorded value, hide a topic tail, hide a device's newest checkpoints, or serve one session a forked view
* Rust tests on both backends

**6.3, `@zen/client`:**
* device enrollment for sessions without a key
* `Checkpointer`: the cadence, the pins in the local store, verification, fork reports, `exportPins` and `comparePins`
* signed header verification
* event body v2: writing and checking chains, and topic heads in checkpoints
* vitest against the adversary: each rollback, hidden tail and fork is detected; honest runs report nothing

**6.4, fs and svc:**
* `@zen/fs` computes `state` and checkpoints trees
* `@zen/svc` checkpoints connections, and a successor refuses an older claim (TD-SVC-STATE-ROLLBACK closed)
* tests with the adversary, and the zen-mount and echo apps unchanged

**6.5, the zen-db authenticated tier:**
* `integrity: "authenticated"`, PRF32 nodes, the row tree, `NsRoot`, verified reads and queries
* the ledger demo
* tests: rollback of a row, an index root or the `NsRoot` is detected; concurrent writers conflict correctly; overhead measured

**6.6, `crates/zen-client` (Rust):**
* sign-in, unlock, the ACL, KV, commits with fence, checkpoints and pins
* tests against the server in-process, with vectors shared with the TS client

**6.7, `crates/zen-blk`:**
* the volume record, blocks, the tree, the root chain, batching and the lease
* in-process tests:
  * random reads and writes against a model
  * a crash between commits loses only unflushed writes
  * rollback and a stale fence detected
  * a second writer refused

**6.8, `zen-ublk` frontends:**
* the FUSE file export and ublk
* tests:
  * here and in CI: FUSE, then `losetup`, `mkfs.ext4`, `mount`, writes, unmount, remount and compare; a writer killed, then `fsck` and a remount
  * a CI job `ublk` (`modprobe`, run as root): the same over `/dev/ublkbN`

**6.9, docs, bench, outcome:**
* docs/INTEGRITY.md (what is detected, the fork limit, pins, enrollment)
* docs/BLK.md
* STATS §6 (checkpoint cost, authenticated read and write overhead, block throughput and latency)
* TECH_DEBT: TD-WITNESS and TD-BLK-SNAPSHOTS; TD-SVC-STATE-ROLLBACK closed
* README, DESIGN-3 §6, and the Outcome

## Step 6.1 in detail

**Scope change from the step list:** 6.1 specifies everything that steps 6.2–6.4 build on:
* checkpoints, with all five entry types laid out
* the signed header
* event body v2
* tree state
* device registration
* fenced leases and `Commit.fence`
* the svc rule

The full texts of zendb.md §14 (the `NsRoot`, the row tree, PRF32) and spec/blk.md are written in 6.5 and 6.7, with their code. That way, three PRs of experience can change them first. For now, their checkpoint entries only name `(id, seq, hash)`.

**Simplification found while specifying:** the `NsRoot`, the volume root and the event chain need no MAC. They're inside AEAD-sealed values, so the server can't forge them anyway. `prev` is a plain labelled hash of the previous plaintext. A rollback (an older valid value) is what checkpoints and pins catch.

### Spec text (`spec:` commit)

**formats.md §14, device checkpoints (new)**

* **Body.** Deterministic CBOR (zendb.md §1) with integer keys:
  ```
  CheckpointBody = { 1: v = 1, 2: fs: u32, 3: device: bytes(32), 4: seq: u64 (1, 2, …),
                     5: prev: bytes(32)            // HC(previous body), zeros for seq 1
                     6: hlc: u64,
                     7: acl: [version: u64, hash: bytes(32)],        // the ACL head the device verified
                     8: header: [version: u64, hash: bytes(32)],     // signed header head; [0, zeros] if none
                     9: objects: [Entry],                            // sorted by (type, id), one per object
                     10: seen: [[device: bytes(32), seq: u64, hash: bytes(32)]] }   // sorted by device, never itself
  Entry = [1, topic: bytes, head: bytes(12), chains: [[chain_id: bytes(12), dseq: u64, dhash: bytes(32)]]]
        | [2, tree: bytes(16), ops: u64, chain: bytes(32), state: bytes(32)]
        | [3, ns: bytes, seq: u64, hash: bytes(32)]           // zen-db authenticated namespace (6.5)
        | [4, vol: bytes, seq: u64, hash: bytes(32)]          // block volume root (6.7)
        | [5, svc: bytes, conn: bytes(16), in_acked: u64, out_head: u64]
  HC(body) = BLAKE3.derive_key("zen/v1/checkpoint-hash", body)
  ```
* **Signed and sealed.**
  * `signed = u32(len body) ‖ body ‖ sig`, where `sig` is the device's hybrid signature with purpose `zen/v1/sig/checkpoint`.
  * `signed` is sealed as **kind 11** under the KV AEAD key, with AAD `zen/v1/aad/checkpoint` and context `u32(fs) ‖ device(32) ‖ u64(seq)`. So the server can't move a checkpoint to another device or seq.
  * The body must fit `max_value_bytes`.
* **Cumulative bodies.** Each body carries forward the device's newest entry for every object it wrote or verified. When the body would no longer fit, the entries unchanged the longest are dropped. A verifier therefore reads one head per device.
* **Verifying a checkpoint:**
  * The signature verifies against the device's cert (from the pinned ACL, or the device registry, §15), checked against the member identity in the pinned ACL.
  * `seq` is greater than the pinned seq of that device, and `prev` links when the seq is adjacent.
  * Two different bodies of one device at the same seq are a **fork**.
  * Each `seen` entry must match what the verifier knows of that device, or what it can fetch. A mismatch, or a seq the server can't produce, is a fork.
* **Freshness rule (per object):** a server answer older than the newest verified entry for the object, or than the client's own pin, is a **rollback**:
  * a topic head below `head`
  * a chain behind `dseq`
  * a tree `ops` below a checkpointed `ops` (or the same `ops` with another `chain`/`state`)
  * an `in_acked`/`out_head` below a checkpointed one
  * a namespace or volume `seq` below a checkpointed one
* **Cadence (informative):** every `sig_every` changes (default 64), 5 s after the last write, and on close.
* **Shared by a device's processes:** a put that loses the seq race re-reads the head, merges its entries in, and retries.

**formats.md §12, signed header (replaces the "planned" sentence)**

```
signed header = u8 0x80 ‖ u8 suite = 1 ‖ u32 fs ‖ u64 version ‖ prev(32) ‖ signer(32) ‖ lp(header) ‖ sig
msg  = every byte before sig;   sig = hybrid signature, purpose "zen/v1/sig/fs-header", msg
HH   = BLAKE3.derive_key("zen/v1/fs-header-chain", msg)
```

* The first byte `0x80` can't be a header version (1 or 2).
* **Acceptance (server and clients):**
  * `version` is the head's version + 1. It's 1 when the stored header is unsigned or absent.
  * `prev` is HH(head), or zeros for version 1.
  * `fs` matches.
  * `signer` is an admin of the current ACL, and its member identity verifies `sig`.
  * Once a header is signed, an unsigned put is refused.
* **Clients:**
  * pin `(version, HH)`
  * accept an unsigned header only while they hold no pin
  * refuse a version below the pin, or the same version with another hash
* History isn't kept; the signature and version suffice.
* The leakage and rollback paragraph is updated to match.

**formats.md §5, event body v2**

```
u8 2 ‖ sender_fp[32] ‖ u64 hlc ‖ lp(causation) ‖ chain_id(12) ‖ u64 dseq ‖ dprev(32) ‖ payload
dprev = HE(previous body of this chain), zeros for dseq 1;   HE(b) = BLAKE3.derive_key("zen/v1/event-chain", b)
```

* **A chain** is `(sender, chain_id, topic)`. A writer instance picks `chain_id` at random, so a restart starts a new chain and needs no stored counter.
* **Advancing:** the chain advances only once a commit is known to have applied. A failed commit reuses the `dseq`. A commit whose outcome stays unknown ends the chain.
* **In flight:** one commit at a time per chain. Several appends in one commit take consecutive `dseq`s in append order.
* **Readers:**
  * Readers of a whole topic check consecutive `dseq`s and `dprev`, and report a gap.
  * Filtered readers (`key_token`, partitioned and per-key groups) see subsets, so they check only for rewrites: the same `(chain, dseq)` with a different body.
* **Unchanged on v1:** v1 bodies still decode, and ephemeral messages stay v1.

**fs.md §9, tree state (replaces "signed checkpoints, a later milestone")**

* **Leaf.** For each node the change feed delivers with a state:
  ```
  leaf = BLAKE3.derive_key("zen/v1/tree-state", 0x00 ‖ node ‖ u8 flags ‖ parent ‖ u64 move_hlc ‖ move_dev ‖ u64 meta_hlc ‖ meta_dev ‖ lp(meta) ‖ u32 versions ‖ changed(12))
  ```
  Absent fields are zero, and `flags` marks `parent` and `meta`. Tombstones leave the set.
* **Fold.** A fixed 3-level, 256-ary radix tree over node ids:
  * bucket `N(p)`, for a 2-byte prefix `p`, is `H(0x01 ‖ ∥ over sorted ids (id ‖ leaf))`
  * `N(p)` above that is `H(0x02 ‖ ∥ over non-empty b (u8 b ‖ N(p‖b)))`
  * `state = N(empty)`, where `H` is the same derive_key
* **Why this fold:** it's canonical and costs about one bucket plus 2 × 256 child hashes per change. That makes it incremental, and keyless, because the tree is collision-resistant (a sum or XOR would be weak to generalized-birthday attacks).
* **Checkpoints:** a device checkpoints `(ops, chain, state)` at a cursor where the replica is caught up.
* **api.md §12:** `changes` gains `ops?` and `chain?`, the tree header at the read version, when `more` is false. That ties `state` to `(ops, chain)` (implemented in 6.2).

**api.md §15, checkpoints and devices (new)**

* `POST /v1/checkpoint/put {fs, device, seq, checkpoint: bytes}` → `{version: bytes(10)}`
  * Needs any right on the fs, and `device` must belong to the caller's user: a cert in the ACL, or registered.
  * A `seq` other than head + 1 → 409 `version_mismatch`. Over `max_value_bytes` → 413.
  * The server keeps the newest `limits.checkpoint_keep` (default 1,024) per device; each put deletes `seq − keep`.
* `POST /v1/checkpoint/heads {fs, devices?}` → `{heads: [{device, seq}]}`
* `POST /v1/checkpoint/get {fs, device, from, to?, limit?}` → `{items: [{seq, checkpoint}], more}`
* stream op `cksub {id, fs}` → `ckh {id, device, seq}` on every new head
* `POST /v1/devices/register {cert}` → `{}`
  * The cert must verify against the caller's member identity in the current ACL.
  * At most 64 per user (429 `quota`).
* `POST /v1/devices/get {devices}` → `{devices: [{device, user, cert?}]}`, so verifiers can find enrolled devices that aren't in the ACL.

**api.md §16, fenced leases (new), and §6 `Commit.fence`**

* `POST /v1/lease {fs, name: bytes(1..64), token?, ttl_ms?, release?}` → `{token, expires_version}`. It needs fs `write`, and works like consume leases (§8.2): empty, expired or released gives the previous token + 1; the holder renews with its token; otherwise 412 `not_leader`.
* `fence?: [{fs, name, token}]` on a commit: each lease is read normally. It must hold `token`, name the caller's device, and be unexpired, else 412 `claim_lost`.
* `/v1/info` features: `"checkpoints"`, `"signed_header"`, `"lease"`, `"devices"`.

**keyspace.md §3.10 (new)**

* `pack("dk", fs, device, seq)` → `versionstamp ‖ sealed checkpoint`
* `pack("dkh", fs, device)` → `u64 seq`
* `pack("dkd", fs)` → doorbell versionstamp
* `pack("dv", device)` → `user_fp(32) ‖ cert`
* `pack("ls", fs, name)` → `holder(32) ‖ u64 token ‖ u64 expires_version`
* `dk`/`dkh`/`dv` are data (`export` copies them); `dkd` is a doorbell. `hdr` is unchanged: it may hold the signed form.

**svc.md §12:** the handler's device checkpoints carry `[5, svc, conn, in_acked, out_head]` every `sig_every` steps and on release. A successor refuses a claim below the newest verified entry. This replaces the "fix is milestone 6" bullet.

**labels.md (and `labels.rs`)**

* **New:**
  * the purpose `zen/v1/sig/fs-header`
  * the hash labels `zen/v1/checkpoint-hash`, `zen/v1/fs-header-chain`, `zen/v1/event-chain` and `zen/v1/tree-state`
  * the AAD `zen/v1/aad/checkpoint`
* **Rewritten:**
  * `sig/checkpoint` becomes "device checkpoint (formats.md §14)"
  * `sig/tree-checkpoint`, `sig/event`, `sig/commit` and `sig/db-root` are marked reserved, since tree, event and root states ride in device checkpoints
* formats.md §4 gets the kind 11 row; §8 ("not yet specified") shrinks to the authenticated tier and other CRDT objects.

### zen-core (new module `crates/zen-core/src/integrity.rs`, plus edits)

* **`integrity.rs`:**
  * `CheckpointBody`, `Entry`, `TopicChain`, `Seen`
  * `encode`/`decode` through `db::cbor`. Decode is strict: canonical order, no duplicate objects, `v = 1`.
  * `checkpoint_hash`
  * `sign_checkpoint(&DeviceSecret, &body) -> Vec<u8>` and `verify_checkpoint(&PublicIdentity, &signed) -> CheckpointBody`
  * `seal_checkpoint` and `open_checkpoint` (kind 11, via `seal.rs`)
* **`header.rs`:**
  * `SignedHeader {fs, version, prev, signer, header}`, with `encode`/`decode`, `sign(&SigningIdentity)`, `verify(&PublicIdentity)` and `hash()`
  * `is_signed(bytes)`
  * `validate_header_successor(current: Option<&[u8]>, next: &[u8], admin: impl Fn(&[u8; 32]) -> Option<PublicIdentity>)`, which the server reuses in 6.2
* **`seal.rs`:**
  * `EventBody` gains `chain: Option<EventChain {chain_id, dseq, dprev}>`. `encode` writes v2 when it's `Some`, and `decode` accepts v1 and v2.
  * `event_chain_hash(body)`
  * kind 11 added to the kind table.
* **`fs.rs`:**
  * `state_leaf(&NodeStateFields)`
  * `TreeState` (a `BTreeMap` of leaves plus cached bucket and mid hashes; `set`, `remove`, `root`)
* **`labels.rs`:** the new constants, `ALL`, and `SIG_PURPOSES` gets `fs-header`.
* **Vectors:** a new `spec/test-vectors/integrity.json` from `vectors.rs`:
  * a checkpoint with one entry of each type and two `seen` entries: its body hex, hash, signature from a fixture device, and the sealed form under `fixture_fs`
  * a signed header v1 → v2: msg, hash and signature
  * an event v2 chain of three bodies, with their hashes
  * tree states: empty, one node, three nodes (two in one bucket), then one removed

### zen-wasm (`crates/zen-wasm/src/crypto.rs`)

* `DeviceSecret.sign(purpose, msg)` and `SigningIdentity`'s existing `sign`. `label()` learns the new names.
* `encodeCheckpoint`, `decodeCheckpoint`, `checkpointHash`, `FsKeys.sealCheckpoint` and `FsKeys.openCheckpoint`. The TS types `CheckpointBody` and `CheckpointEntry` go in `TS_TYPES`.
* `signHeader(identity, fs, version, prev, header)`, `decodeSignedHeader`, `verifySignedHeader` and `headerHash`.
* `EventBody` gets optional `chainId`, `dseq` and `dprev` (through the existing `sealEvent`/`openEvent`), plus `eventChainHash`.
* a `TreeState` class with `set(node, fields)`, `remove(node)` and `root()`, plus `stateLeaf`.
* `scripts/build-wasm.sh` is rebuilt, and `packages/zen-wasm` is regenerated.

### Tests

* **Rust (zen-core):**
  * `tests/vectors.rs` covers the new file, and `tests/labels.rs` passes.
  * Unit tests for:
    * checkpoint round trip
    * decode refuses unsorted or duplicate entries, `v ≠ 1` and trailing bytes
    * a flipped body bit or a wrong device key fails verification
    * an open with the wrong device or seq fails
    * header successor rules: wrong version, wrong `prev`, non-admin signer, unsigned after signed, wrong fs
    * event v1 and v2 round trips
    * `TreeState`: incremental equals from scratch over 2,000 random set/remove steps (`DetRng`), and the empty-tree value
* **TS:** `packages/client/test/integrityvectors.test.ts` checks the wasm bindings against `integrity.json`, like `fsvectors.test.ts`.

### Files

* `spec/{formats,api,keyspace,fs,svc,labels}.md`
* `crates/zen-core/src/{integrity.rs (new), lib.rs, header.rs, seal.rs, fs.rs, labels.rs, vectors.rs}`
* `crates/zen-wasm/src/crypto.rs`, `packages/zen-wasm/*` (generated)
* `spec/test-vectors/integrity.json`, `packages/client/test/integrityvectors.test.ts`
* `docs/MILESTONE-6.md` (the log)

### Verification

* `cargo fmt --check`
* clippy `-D warnings` on zen-core and zen-wasm (default and `--all-features`)
* `cargo test -p zen-core --features test-utils`
* regenerate the vectors, and check the diff is only `integrity.json`
* `scripts/build-wasm.sh`, `npm run check`, and `npx vitest run packages/client/test/integrityvectors.test.ts` 3×
* zen-server compiles unchanged (`cargo check -p zen-server`)
* CI green on both runs

## Step 6.2 in detail

Step 6.2 makes the server implement what 6.1 specified: api.md §15, §16, the `fence` part of §6, `cksub` (§9), §4.4's signed header, and `changes` `ops`/`chain` (§12). It also adds a test-only **adversary** that later steps test detection against. There's no client code in this step. TS types change only through zen-proto, and still have to type-check.

**Survey (what this step builds on):**
* **The router:** `crates/zen-server/src/lib.rs` `router`, and `info` (features and limits).
* **Consume leases** (`consume.rs`):
  * `LeaseRec`, `decode_lease` and `encode_lease` (`holder ‖ token ‖ expires`)
  * `ms_to_versions`
  * `lease` (`expires > now`, otherwise token + 1) and `release` (sets `expires` to 0)
  * `check_token` checks holder and token, but not expiry
* **The commit** (`commit.rs` `validate` and `execute`):
  * `validate` checks permissions and sizes per field, and counts ops.
  * In the transaction come the expects, clears, writes, consumes, `svc` steps, appends, then the tree and rows.
  * Per-fs `(bytes, keys)` quota deltas are checked against `caller.acl.limits`.
* **The fs header** (`acl.rs` `header_put`): admin only, CAS on the 10-byte stamp, `set_versionstamped_value(hdr)`. The ACL state is `AclState {admins, members: {identity, devices}}`, with `has_device(user, device)`.
* **Streams** (`stream.rs`): `run` dispatches frames; `subscription` watches a head key, then range-reads; `recheck` re-validates rights.
* **The tree** (`tree.rs`):
  * `read_changes` runs in one read transaction (`read_txn`)
  * `load_header` gives `ops`, `chain` and `resync_before`
  * every read endpoint goes through `read_txn` or `st.store.begin`
* **`dump.rs` `private()`** leaves out `meta` and relay keys; everything else is exported.
* **Tests:**
  * `tests/common` `Harness::start_with(|cfg| …)` on the embedded backend, or on FDB when `ZEN_TEST_BACKEND=fdb`
  * `User` (signing identity and device), `claim`, `sign_in`, `call`, and `connect`/`send`/`recv` for streams
* **Generated TS types:** zen-proto types derive `tsify` under `ts`, and the wasm glue isn't tracked. The vitest server is the debug binary (`packages/client/src/testing/server.ts`; CI builds it with `cargo build -p zen-server`).

### Spec touch-ups (`spec:` commit, small)

* **api.md §15.1:** checkpoints count toward the fs's `bytes` and `keys` quota (formats.md §9) like KV values; a put over quota gets 403 `quota`, and pruning gives the bytes back.
  * Why: without it, a member could store `checkpoint_keep × max_value_bytes` (about 92 MB) per device, outside any quota.
  * keyspace.md §3.10 says the same.
* **keyspace.md §3.10:** `dkd` isn't exported (it joins `meta` and the relay keys in `dump`'s private set).
* **`examples/zen-serve.toml`:** `checkpoint_keep` in `[limits]`.

### zen-proto (`crates/zen-proto/src/lib.rs`, all with `ts` derives like their neighbours)

* `Limits.checkpoint_keep: u32`, with `#[serde(default)]` so older servers decode.
* `Commit.fence: Vec<Fence {fs, name: bytes, token: u64}>`, skipped when empty. `Changes` gains `ops: Option<u64>` and `chain: Option<bytes>`.
* **Checkpoints:**
  * `CheckpointPut {fs, device, seq, checkpoint}` → `CheckpointVersion {version}`
  * `CheckpointHeads {fs, devices?}` → `Heads {heads: [Head {device, seq}]}`
  * `CheckpointGet {fs, device, from, to?, limit?}` → `Checkpoints {items: [{seq, checkpoint}], more}`
* **Devices:**
  * `DeviceRegister {cert}` → `DeviceRegistered {device}`
  * `DevicesGet {devices}` → `Devices {devices: [{device, user, cert}]}`
* **Leases:** `LeasePut {fs, name, token?, ttl_ms?, release?}` → `NamedLease {token, expires_version}`. The names avoid the consume `Lease*` types.
* **Frames:** `Frame::Cksub {id, fs}` and `Frame::Ckh {id, device, seq}`.

### Server

**Config** (`config.rs`):
* `limits.checkpoint_keep` (default 1,024, at least 1).
* A top-level `adversary: bool` (default false). If it's true in a build without `test-utils`, the config is refused ("built without test-utils").

**`/v1/info`:**
* adds the features `checkpoints`, `signed_header`, `lease` and `devices`
* adds `limits.checkpoint_keep`

**Keys** (`keys.rs`): `ckpt(fs, dev, seq)` (`dk`), `ckpt_head(fs, dev)` (`dkh`), `ckpt_heads(fs)` (prefix), `ckpt_bell(fs)` (`dkd`), `device(dev)` (`dv`), `user_device(user, dev)` (`dvu`), `named_lease(fs, name)` (`ls`).

**New module `integrity.rs`** (checkpoints and devices, api.md §15):
* **`any_right(caller, fs)`:** an fs right or any topic grant, else 403.
* **`put`:**
  * Checks: `check_fs`, `any_right`, `device` is 32 bytes, `checkpoint.len() ≤ max_value_bytes` (413), `seal::peek` gives kind 11 (400), `seq ≥ 1`.
  * Ownership: `caller.acl.has_device(user, device)`, or `dv(device)` names `caller.user`. Otherwise 403.
  * In `txn_loop!`:
    1. Read `dkh` normally. `seq ≠ head + 1` gives 409 `version_mismatch`.
    2. `set_versionstamped_value(dk, [], checkpoint)`, set `dkh = u64 seq`, ring `dkd`.
    3. If `seq > keep`, snapshot-read and clear `dk(seq − keep)`.
    4. Apply the quota delta (bytes: + new length − pruned length; keys: +1, or 0 when one was pruned) through the same check as commits. That check is factored out of `commit.rs` into `quota::apply(t, limits, delta)`.
  * Returns the stamp.
* **`heads`:**
  * With `devices`: one get each, unknown ones left out.
  * Without: a range over `dkh(fs)`, at most `max_range_items` (the device count per fs is bounded by members × (ACL devices + 64)).
* **`get`:** a range `[dk(from), dk(to + 1))` (or to the end of the device's prefix), capped by `limit` and `max_range_bytes`. Each value drops its 10-byte stamp.
* **`register`:**
  * `verify_device_cert(member identity of caller.user, cert)` must pass, else 400; the identity comes from `caller.acl.members`.
  * `fp` is the `DevicePublic` fingerprint.
  * In a transaction:
    * If `dv(fp)` exists with this user, it's a no-op. If it exists with another user, 409. That can't happen for an honest cert, since certs bind one user.
    * Otherwise, count the `dvu(user)` range: at 64 already, 429 `quota`.
    * Set `dv = user ‖ cert` and `dvu`.
* **`get` devices:** any signed-in caller; at most 1,000 devices per call; a get per device.

**Fenced leases** (`lease.rs`, api.md §16):
* **`/v1/lease`:**
  * Checks: `check_fs`, fs `write`, `name` of 1–64 bytes (400), ttl defaults to 10,000 and is clamped to 100..600,000.
  * Acquire and renew follow `consume::lease`, sharing `LeaseRec`, `decode_lease` and `encode_lease`, which become `pub(crate)`. The key is `ls`, and the holder is `caller.device`.
  * A release needs the holder and the current token; it sets `expires` to 0 and returns `{token, expires_version: 0}`.
* **`fence` in `commit.rs`:**
  * `validate`: each fence needs fs `write`, a name of 1–64 bytes, and counts as one op.
  * `execute`: fences are checked first in the transaction, before the expects. Each one `get`s `ls` normally (into the read set) and requires `holder == caller.device`, `token == f.token` and `expires > t.read_version()`, else 412 `claim_lost`.
  * A replay through `commit_id` returns the stored result without re-checking fences, as for every other step (api.md §6 rule 1).

**The signed header** (`acl.rs` `header_put`):
* Once the `expect` CAS passes, if the new header or the stored one is signed:
  * call `zen_core::header::validate_header_successor(fs, stored (without its stamp), new, admin)`
  * `admin(fp)` is the member identity of `fp` when `fp` is an admin of `caller.acl`
  * an error gives 400 `bad_request` with the reason
* Unsigned headers over unsigned ones are unchanged.

**`changes` `ops`/`chain`** (`tree.rs` `read_changes`): when `more` is false, set `ops` and `chain` from `load_header` in the same read transaction (the header is already read when `after` is given; reuse it).

**`cksub`** (`stream.rs`):
* Checks: `check_fs` and `any_right`, rechecked like `sub`.
* A task:
  1. reads every head (as `heads`)
  2. sends `ok`
  3. loops: watch `dkd(fs)`, re-read the heads, and send `ckh {id, device, seq}` for each device whose seq grew (or that is new)
* `unsub` stops it, like other subscriptions.

**`dump.rs`:** `private()` also covers the `dkd` prefix. `dk`, `dkh`, `dv`, `dvu` and `ls` export as data.

### The adversary (`adversary.rs`, `#[cfg(feature = "test-utils")]`)

Detection must be tested against a server that lies, so 6.3–6.5 need one. It sits at the storage boundary:
* **The decorator:** `Adversary` wraps `Arc<dyn Storage>` and its transactions, and filters reads by rules.
* **`AppState.view`:** an `Arc<dyn Storage>`, the decorated store when `adversary = true`, otherwise `store` itself.
* **What reads through it:** only the read endpoints:
  * kv `get`/`range`
  * `log/read` and stream `sub` reads
  * `fs/header/get`
  * the tree read endpoints (via `read_txn` and `tree_list`)
  * checkpoint `heads`/`get` and `cksub`
* **What doesn't:** commits, puts and the server's own bookkeeping use `store`. The server's internal state stays honest, and lifting a lie restores honest answers.

**Rules** are generic (built from typed lies):
* `Freeze {key, value: Option<bytes>}`: reads of `key` return `value`, including inside ranges, where the current entry is replaced, removed or inserted in order.
* `Hide {prefix, pred(key, value)}`: hides matching keys under `prefix`. A range read keeps reading past hidden items until it has `limit` visible ones or reaches the end, so `more` stays truthful about what's visible.

**Typed lies** (each "freezes X as it is now"; a test writes, lies, writes again, and the server serves the first state):

| Lie | Rules |
|---|---|
| `{kind: "kv", fs, key}` | freeze `kv(fs, key)` at its current raw value (or absent) |
| `{kind: "header", fs}` | freeze `hdr(fs)` |
| `{kind: "topic", fs, topic}` | record the head offset `h`; freeze `lh(fs, topic)`; hide log entries of the topic with offset > `h`, plus its `lk` index entries and `gl` entries (value = topic) above `h` |
| `{kind: "tree", fs, tree}` | freeze the tree header (ops, chain); hide change entries above the current tree head |
| `{kind: "checkpoints", fs, device}` | record the head `s`; freeze `dkh` at `s`; hide `dk(fs, device, seq > s)` |

**The control endpoint:** `POST /v1/test/adversary {lies: [Lie], clear?: bool}` → `{}`.
* It's routed only when `adversary = true`, and it's admin-only.
* `clear` drops every rule first.
* It's documented in the module and in `docs/MILESTONE-6.md`, not in spec/api.md (test-only).

**Forks** need no server code: in 6.3 the TS harness starts two servers on copies of one data directory (`TestServer.fork()`), and each client talks to one. That's a real split view, and it's what pins and `seen` must catch. A whole-store rollback is the same (restart on an older copy).

**CI:** the vitest binary needs the adversary from 6.3 on. In 6.3 the client job's build becomes `cargo build -p zen-server --features test-utils` (likewise locally), and `spawnServer` gains an `adversary` option. Nothing changes in CI in 6.2.

### Tests (new `crates/zen-server/tests/integrity.rs`; both backends via `Harness`)

* **`info_lists_integrity_features`:** the four features, and `checkpoint_keep` (default, and set by config).
* **`checkpoint_log`:** real checkpoints built with `zen_core::integrity` (`sign_checkpoint` and `seal_checkpoint` under a fixture `FsKeys`):
  * `put` 1, 2, 3; then a replay of 3, or 5, gives 409
  * `heads` (all and filtered, an unknown device left out)
  * `get` ranges, `limit`/`more`, and `to`
  * open and verify what comes back
  * pruning with `checkpoint_keep = 3`: after 5 puts, `get` from 1 returns 3–5
  * kind 1 sealed → 400; over `max_value_bytes` → 413; `seq` 0 → 400
* **`checkpoint_ownership`:**
  * a device of another member → 403; an unknown device → 403
  * after that user's `devices/register`, puts work
  * a user with no right on the fs → 403 for put, heads and get; a topic-only grant can put
* **`checkpoint_quota`:** with an fs `max_bytes`, puts past it → 403 `quota`; pruning frees bytes (puts under `checkpoint_keep` continue).
* **`devices_register_and_get`:**
  * register returns the fingerprint; again it's a no-op
  * a cert by another identity → 400
  * the 65th device → 429
  * `get` returns `{device, user, cert}`, and leaves out unknown ones
* **`cksub_pushes_heads`:**
  * `ok`, then `ckh` for each put of two devices, in order per device
  * no push for a 409'd put
  * a caller without rights → `err`
* **`signed_header_chain`:**
  * unsigned v0, then signed v1 (admin), then signed v2 by another admin, all accepted; `get` returns the bytes
  * refused with 400, storing nothing:
    * an unsigned put over a signed one
    * a version skip
    * a wrong `prev`
    * a wrong `fs`
    * a non-admin member's signature
    * a flipped signature byte
* **`named_lease`:**
  * acquire gives token 1; another device gets 412 `not_leader`; renew keeps the token
  * release gives `expires_version` 0; the other device then gets token 2
  * with a 100 ms ttl, after expiry, another device gets token + 1
  * a read-only user → 403; names of 0 or 65 bytes → 400
* **`fence`:**
  * a commit with a held fence applies; after another device takes the lease, the old holder's fenced commit gets 412 `claim_lost` and writes nothing
  * a released lease gets `claim_lost`
  * a short-mode commit at a read version taken before another device acquired the lease gets 409 `conflict`, since the lease is in the read set
  * a fence on an fs without `write` → 403
  * a replayed `commit_id` returns the stored result
* **`changes_carry_ops_and_chain`:**
  * after moves, `changes` (whole and after a cursor) with `more` false carries `ops`/`chain` equal to `tree/chain`
  * with `limit` cutting the list (`more` true), they're absent
* **`adversary_*`** (embedded and FDB, `adversary = true`):
  * kv: `get` and `range` serve the frozen value and version, and an absent key stays absent; `clear` restores the truth
  * topic: `log/read` and a `sub` stream stop at the frozen head; a new event is withheld until `clear`, then delivered with no gap
  * checkpoints: `heads` stays at the frozen seq, `get` hides newer ones, and `cksub` stays silent; meanwhile the device's real puts keep succeeding (the server's own state is honest)
  * header: `get` serves the frozen header
  * tree: `changes` hides new ops, and `chain` serves the old `ops`/`chain`
  * the endpoint returns 404 without `adversary = true`, and 403 for a non-admin
* **`dump`:** an existing export/import test also round-trips a checkpoint, a registered device and a lease, and `dkd` isn't exported.
* **Existing tests stay green:** the consume lease tests run unchanged (they share `decode_lease`).

### Files
* **spec:** `spec/{api,keyspace}.md`, `examples/zen-serve.toml`
* **zen-proto:** `crates/zen-proto/src/lib.rs`
* **zen-server, `src`:**
  * `{lib,config,keys,commit,consume,acl,tree,stream,dump,state,kv,log}.rs`
  * new: `{integrity,lease,quota,adversary}.rs`
* **zen-server, `tests`:** `crates/zen-server/tests/{integrity.rs (new), dump.rs}`
* **docs:** `docs/MILESTONE-6.md` (the log)

### Verification
* `cargo fmt --check`
* clippy `--all-targets -D warnings`: default, `--no-default-features`, `--all-features`
* `cargo test -p zen-server` (embedded) and `cargo test -p zen-proto`
* the integrity and dump tests on local FDB (`ZEN_TEST_BACKEND=fdb`), 3×
* `scripts/build-wasm.sh` and `npm run check` (the proto types still type-check)
* the vitest client suites that touch commits, streams and the tree, once: `kv`, `stream`, `tree`, `log`
* CI green on both runs

## Step 6.3 in detail

Step 6.3 makes `@zen/client` use what 6.1 specified and 6.2 serves:
* device enrollment
* checkpoints with pins: write, verify, detect forks, compare pins out of band
* signed fs headers
* event chains (body v2), with topic heads in checkpoints

Each detection is tested against the 6.2 adversary and against forked servers. The tree, svc and zen-db entries use the same `Integrity` object in 6.4 and 6.5; this step exposes `note(entry)` for them, but only topics call it.

**Survey (what this step builds on):**
* **Sessions** (`session.ts`, `client.ts`):
  * A device sign-in (`signInDevice`) takes `DeviceCredentials {identity, device, cert}`, but the session keeps only `info.device_fp`. `vault.signIn` frees the `DeviceSecret` after signing in.
  * Other methods have no device secret: passkey, OPAQUE, mTLS, API token and resumed sessions. For them `deviceFp` is a credential id, or absent.
  * **The user identity** (`SigningIdentity`, the ACL member identity) is held only by whoever made the member entry: tests, zen-mount's device-key file, an admin. It's not the password-derived identity of method 6, which is a separate credential (auth.md §3, "Privacy").
* **The ACL** (`acl.ts`): `Acl.head()` walks the chain from an in-memory pin (`AclVersion`) and refuses a lower or changed version (`acl_chain`). The pin isn't persisted.
* **The fs header** (`fs.ts`):
  * `Fs.header()` gets `/v1/fs/header/get` (optionally keeping a copy on a `SealedStore`) and decodes `FsHeader`.
  * `putHeader`, `init`, `editHeader` and every slot method write the unsigned form.
  * A signed stored header would fail `FsHeader.decode` today: the first byte is `0x80`.
* **Topics** (`log.ts`): `Topic.seal` builds a v1 `EventBody`, whose sender is `senderOf(session)`, the device fp or zeros. `append`/`appendIn` seal at call time; a transaction that retries calls its function again, which seals again. `read` pages `/v1/log/read` (no head in the response). `decode` opens events, and so do `Consumer.toDelivery` and the stream's `openEvent`.
* **Streams** (`stream.ts`): subscription kinds `sub`, `esub` and `svc`, re-sent after a reconnect; `Subscription<T>` is an async iterable or callback.
* **The local store** (`local/`): `SealedStore` with named stores; `memoryLocalStore`; the vault (`vault/<name>`).
* **WASM:**
  * 6.1 added the integrity bindings: `encodeCheckpoint`/`decodeCheckpoint`, `checkpointHash`, `checkpointBodyBytes`, `DeviceSecret.signCheckpoint`, `verifyCheckpoint`, `FsKeys.sealCheckpoint`/`openCheckpoint`, `SigningIdentity.signHeader`, `decodeSignedHeader`, `verifySignedHeader`, `isSignedHeader`, `validateHeaderSuccessor`, `eventChainHash`, and `EventBody.chainId`/`dseq`/`dprev`.
  * Missing: the wire codecs of the 6.2 types (`wire.rs` lists every zen-proto type; the checkpoint, device and lease types aren't in it), and a random `DeviceSecret` (only `fromSeed`; the client uses `randomBytes(32)`).
  * The device fp is `fingerprint(devicePublic)`.
* **Test harness** (`testing/server.ts`): `spawnServer` writes a config (`top`, `limits`, …) and runs `target/debug/zen-serve`. `restart` reuses the data directory. There's no fork, and no adversary option. `testing/cbor.ts` encodes CBOR (for the adversary's request, which isn't a zen-proto type).
* **CI** (`client` job): `cargo build -p zen-server`, then `scripts/build-wasm.sh`.

### WASM (`crates/zen-wasm/src/wire.rs`)

Add the 6.2 wire types to `wire_fns!`:
* `CheckpointPut`, `CheckpointVersion`, `CheckpointHeads`, `CheckpointHeadList`, `CheckpointGet`, `CheckpointList`
* `DeviceRegister`, `DeviceRegistered`, `DevicesGet`, `DeviceList`
* `NamedLeasePut`, `NamedLease` (for completeness; the TS client doesn't use leases until a later step needs them)

`Frame` already carries `cksub`/`ckh`. Rebuild with `scripts/build-wasm.sh`.

### Devices and enrollment (`session.ts`, new `devices.ts`)

* **`newDevice()`** → `{secret: DeviceSecret, seed, devicePublic, fp}`: a fresh device key (`randomBytes(32)`), for the vault.
* **`session.approveDevice(identity, devicePublic)`** → `{cert, fp}`:
  * The user identity issues a cert (`identity.issueDeviceCert(devicePublic, now)`), and it's registered (`/v1/devices/register`).
  * Any session of the same user can call it: a passkey or OPAQUE session on a laptop that holds the identity approves the phone's device key shown as a code, as in the TECH_DEBT enrolment entry.
* **`session.enrollDevice(identity, {vault?})`** → `{device: DeviceCredentials, fp}`: `newDevice` and `approveDevice` in one call. With `vault`, `saveDevice` keeps it, so the next `vault.signIn` is a device sign-in.
* **`session.devices(fps)`** → `/v1/devices/get`, as returned (unverified). Verification is `Integrity`'s job: each cert must verify against the member identity of its `user` in the verified ACL.
* **`session.useIdentity(identity)`:** the user identity for operations that need it later: signed header puts (below) and `approveDevice` without an argument. It's kept in memory, like `useClock`.

### Signed fs headers (`fs.ts`)

* **Reading** (`Fs.header`):
  * The stored bytes may be signed (`isSignedHeader`). If so, they're decoded and the signature verified against an admin of `session.acl.head()`; then the inner header is decoded as today.
  * `StoredHeader` gains `signed?: {version, hash, signer}`.
  * A signature that doesn't verify, or a signer who isn't an admin, gives `ZenError(0, 'integrity', …)` with a `header` alarm (below).
  * The offline copy keeps the signed bytes, verified again on use.
* **Pins:** with an `Integrity` attached to the `Fs`, the header pin `(version, HH)` is checked (formats.md §12.1):
  * an unsigned header once a pin exists, a lower version, or the pinned version with another hash is a **rollback**
  * a higher version raises the pin
* **Writing:**
  * `fs.init(slots, {signed: true})` writes signed version 1.
  * `UnlockedFs.signHeader()` converts an unsigned header (admins).
  * Once the stored header is signed, `editHeader` (and every slot and policy method through it) signs the successor: version + 1, `prev` = HH of the stored one. That needs `session.useIdentity(admin)`; without it, `editHeader` fails before writing, with `ZenError(0, 'signer_needed')`.
  * The server checks the same rules (6.2). Each successful put raises the pin.

### Integrity (new `integrity.ts`; `Fs.integrity`)

**Attaching:** `fs.integrity(opts)`, where `fs` is the locked `Fs`. Header pins are needed before unlocking; `unlock` hands the keys over. It returns one `Integrity` per `Fs` object. The options:
* `device?: {device: DeviceSecret, cert}`: the signer. Without it, the client verifies only, and writes no checkpoints.
* `local?: SealedStore`: where pins are kept (store `integrity/<fs>`); default memory.
* `sigEvery?` (default 64) and `delayMs?` (default 5,000): the cadence (formats.md §14.3).
* `onAlarm?(alarm)`.

**Pins** (persisted on `local`, one record per item):
* `acl`: the signed ACL bytes of the pinned version, so a new process restores `session.acl`'s pin. The `Acl` class gains `pin(bytes)` and `pinned()`.
* `header`: `(version, HH)`.
* `dev/<fp>`: each device's verified head `(seq, HC, body)`. The own device's body is kept too, to build the next cumulative body.
* `obj/<type><id>`: per object, the newest entry verified from each writer, merged into a floor: the highest topic head, the highest `dseq` per chain with its hash, and so on.

**Writing checkpoints:**
* `note(entry)` records an object entry and counts a change. After `sigEvery` changes, or `delayMs` after the last one, a checkpoint is written in the background; `checkpoint()` writes one now, and `close()` writes a last one and stops the timer.
* **The body:**
  * `seq` = own head + 1 and `prev` = its HC; `hlc` from the session clock
  * `acl` = the pinned ACL head, and `header` = the header pin or `[0, zeros]`
  * `objects` = the previous body's entries merged with the noted ones
  * `seen` = every other verified device head
* **Too big:** when the encoded body exceeds `max_value_bytes` less the seal and signature overhead (from `/v1/info`), the entries unchanged for longest are dropped.
* **Signed, sealed and put:** `signCheckpoint`, then `sealCheckpoint(fp, seq, signed)` under the current epoch, then `/v1/checkpoint/put`.
* **Races and retries:**
  * On 409, the device's head is re-read and verified (another process of the same device), its entries merged, and the put retried with the next seq (formats.md §14.3), up to a bound.
  * On `commit_unknown` or a network error, the same bytes are resent (idempotent since 6.2).
* After an alarm, no checkpoint is written: a device doesn't sign a view it knows is false.

**Verifying (`sync()`):**
1. `heads` for the fs. For each device whose head is past its pin, `get` the head checkpoint, and the pinned seq + 1 when the head is exactly one past, to check `prev`.
2. For each checkpoint (formats.md §14.4):
   * Open it (`openCheckpoint(fp, seq)`, the epoch from the sealed header).
   * Find the device's cert: among the pinned ACL's member devices, or else via `devices/get`, verified against the member identity of its `user`. Then verify the signature, and check `fs`, `device` and `seq`.
   * Check the pin rules (rule 2): a lower seq, the same seq with another HC, or a broken `prev` is a **fork**.
3. Check `seen` links (rule 3): equal to the pin at the same seq. Otherwise fetch that seq (to the device's head only), compare HC, and treat a seq the server can't produce as a **fork**.
4. Raise the object pins (rule 4); an entry below the same writer's previous one is a **fork**.
5. Freshness of the ACL and the header against the checkpoints' `acl`/`header`:
   * `session.acl.head()` must reach the version, with the same hash at it (via `acl.entries(v)`).
   * The header is re-read and checked against the raised pin.
6. Raise the own device's pins. The own device's server head below its pin is a **rollback** (its checkpoints are withheld or rolled back).

**Watching:** `watch(stream)` subscribes `cksub`, and syncs on each `ckh` and after every (re)subscription, since pushes missed while disconnected are only visible in `heads`. `stream.ts` gains the `ck` kind: it re-sends `cksub` on reconnect and yields `{type: 'heads'}` (resubscribed) or `{type: 'head', device, seq}`.

**Comparing pins out of band:**
* `exportPins()` → bytes: deterministic CBOR `{fs, acl: [v, H], header: [v, HH], devices: [[fp, seq, HC]]}`, small enough for a QR code or a chat message.
* `comparePins(bytes)` treats each device entry as a `seen` link (rule 3) and the ACL and header entries as pins (rule 5). It returns the alarms; a split view shows as a fork.

**Alarms:**
```ts
type IntegrityAlarm = {
  kind: 'rollback' | 'fork' | 'gap' | 'rewrite' | 'invalid';
  object: 'acl' | 'header' | 'checkpoint' | 'topic' | 'tree' | 'db' | 'vol' | 'svc';
  id?: Uint8Array;       // topic id, device fp, …
  detail: string;
}
```
* An alarm is recorded (`integrity.alarms`), passed to `onAlarm`, and thrown from the call that found it as `ZenError(0, 'integrity', detail)` with `.alarm`.
* From then on the fs is **untrusted**: checkpoint writes stop, and `integrity.trusted` is false.
* What the app does next is policy (docs/CLIENT.md): refuse writes, alert, compare pins.
* `invalid` is a checkpoint that doesn't open, or whose signature, cert or fields don't verify: the server served bytes no device sealed.

### Event chains (`log.ts`, `stream.ts`)

**Writing** (only when the fs has an `Integrity`; otherwise bodies stay v1 and nothing changes):
* `sender` is the integrity device's fp. That's the signer of the checkpoints that list the chains; for a device sign-in it's the same as before.
* **The chain pool.** Each `Topic` object keeps a pool of chains `{chainId (12 random bytes), dseq, dhash}`.
  * An append takes an idle chain, or starts a new one when every chain has a commit in flight, so a chain never has two commits in flight (formats.md §5.1).
  * It seals with `dseq + 1` and `dprev = dhash` (consecutive `dseq`s for several appends in one commit).
  * When the commit applies, the chain advances and goes back to the pool, and the topic entry is noted: chain `(dseq, HE)`, and `head` = the newest appended offset.
  * When it's refused or conflicts, the chain goes back unadvanced.
  * When the outcome stays unknown (`commit_unknown` after `sendCommit`'s replays), the chain is dropped.
  * Sequential appends therefore use one chain, and concurrency adds a few.
* **In transactions:** `appendIn(tx)` reserves a chain for that attempt of the transaction (`tx.onCommit` advances it; a retry or failure releases it, through a new internal hook on `Transaction` that runs at the end of each attempt).

**Reading** (with an `Integrity` attached):
* **A `ChainCheck`** per reader: each `Topic.read` call, each stream subscription, and each consumer.
  * It keeps each chain's last `(dseq, HE)` as it sees them. It starts at whatever `dseq` it first sees, since a read from an offset starts mid-chain.
  * Unfiltered readers (a `read` without `key`, `sub` without a key) report a skipped `dseq` as a **gap** and a different `dprev` as a **rewrite**.
  * Filtered readers (`key` reads and groups) check only against the pins: an event at a pinned `dseq` whose HE differs from the pinned `dhash` is a **rewrite** (fork).
* **Freshness** (formats.md §14.4):
  * **The end of a read.** When an unfiltered `Topic.read` reaches the end (`more` false, no `limit` cut), its last offset (or `after`) must be at least the pinned `head`. Each pinned chain seen in the read must reach its pinned `dseq`. Otherwise it's a **rollback** (a withheld tail, G26).
  * **`topic.verifyHead()`:** reads one event after the predecessor of the pinned head offset. It must be that offset, else a **rollback**. It's cheap enough to call after each `sync`.
* **Reads are noted:** reading through to the end notes the topic entry's `head` (the device "has read through" it). Chains are noted only by their writer.

### Test harness (`testing/`)

* **`spawnServer({adversary: true})`:** writes `adversary = true` (needs a binary built with `test-utils`).
* **New `testing/adversary.ts`:** `lie(session, lies, {clear?})` CBOR-encodes `{lies, clear}` and posts `/v1/test/adversary`. It's admin only, so an admin session is needed. It has typed helpers for the five lies.
* **`TestServer.fork()`:** stops the server, copies its data directory, restarts the original on its port, and starts the copy on a free port. It returns the copy as a `TestServer`. Sessions are server-side records, so a session token works on both sides.
* **CI:** the `client` job builds with `cargo build -p zen-server --features test-utils`. Locally, `--features fdb,test-utils`, so the running FDB cluster's binary keeps working after a restart.

### Tests (new `packages/client/test/integrity.test.ts`; the adversary on)

Every attack test first asserts that the attack works (the server really serves the stale or forked answer), then that it's detected. During development, each one is run once with its check disabled to confirm it then fails, and the log says so.

* **`enrollment`:**
  * An API-token session (no device) gets `enrollDevice(identity, {vault})`: the device is registered, and `vault.signIn` then signs in by device.
  * `session.devices` returns its cert.
  * Its checkpoints verify for another member's device.
  * `approveDevice` with another user's identity gives 400.
* **`honest_checkpoints`:**
  * Two devices (one in the ACL, one enrolled) append and checkpoint, and each syncs the other: bodies are cumulative, `seen` links each other, and there are no alarms.
  * Pins survive a new `Integrity` on the same local store, and a server restart.
  * The cadence works: `sigEvery = 3` writes after 3 appends, and `delayMs` after a pause.
* **`one_device_two_processes`:** two `Integrity`s with the same device checkpoint concurrently. Puts race on seq (409), merge, and both processes' entries end up in the head body, with no alarm.
* **`watch`:** `cksub` pushes make B sync A's new checkpoint without polling, including after the stream reconnects.
* **`chains`:**
  * Appends write v2 bodies, and an unfiltered read checks them.
  * Concurrent appends use separate chains.
  * A transaction refused by an `expect` conflict and retried reuses its `dseq` (no gap).
  * A `key` read and a consumer group see subsets without alarms.
* **`chain_gap_and_rewrite`:**
  * Events sealed by hand (`sealEvent` with chain fields), skipping `dseq` 2, give a **gap** on an unfiltered read and a subscription.
  * Two bodies at one `dseq` give a **rewrite**.
  * A pinned `dhash` that differs gives a **rewrite** on a filtered read.
* **`signed_header`:**
  * `init({signed: true})`; slot edits keep signing (versions 2 and 3); another session verifies and pins them.
  * Without `useIdentity`, an edit gives `signer_needed`.
  * A non-admin's signed put is refused by the server (400).
* **`header_rollback`:** lie `header`; the admin signs v2 (its own pin rises); its next `header()` (still v1 from the server) gives a rollback. B learns v2 from the admin's checkpoint (`header` field) and alarms too.
* **`topic_tail_withheld`:**
  * Lie `topic` at e3. A appends e4 and e5 and checkpoints.
  * B's read stops at e3 (the attack works). After B syncs A's checkpoint, the read's end check and `verifyHead` give a rollback.
* **`checkpoints_withheld`:**
  * Lie `checkpoints` for A at seq 2; A writes 3 and 4 (the server accepts them).
  * A's own `sync` gives a rollback (own head below its pin).
  * B sees nothing alone (the attack works), but `comparePins(A.exportPins())` gives a fork.
* **`fork`** (`server.fork()`, two servers on one history):
  * A writes on S1, and B on S2; each is consistent alone, with no alarms.
  * `comparePins` across gives a fork.
  * Device D writes seq k on S1. Moved to S2 with its pins, D sees its head below its pin (a rollback). D then writes a different seq k on S2, as a fresh process with no pins. A client that pinned D@k on S1 and moves to S2 sees a fork (the same seq with another HC).
* **`whole_store_rollback`:**
  * Copy the server, then on the original: update the ACL, sign a header version, and checkpoint.
  * A client with those pins, pointed at the copy, gets alarms for the ACL (`acl_chain` or rollback), the header, and the checkpoints.
* **`honest_runs_report_nothing`:** a mixed run (two devices, chains, concurrent appends, header edits, an ACL update, a server restart, watch) ends with zero alarms.

**Existing suites:** unchanged (integrity is opt-in). `kv`, `log`, `stream`, `tree`, `local`, `vault`, `unlock`, `keyslots` and `packages/db`/`packages/fs` run once to confirm.

### Docs
* **docs/CLIENT.md:** a new "Integrity" section covering enrollment, attaching, cadence, pins, alarms and policy, `exportPins`/`comparePins`, signed headers, chains, and what isn't covered (fork consistency, TD-WITNESS).
* **docs/MILESTONE-6.md:** the log, and the adversary harness.

### Files
* **WASM:** `crates/zen-wasm/src/wire.rs`
* **Client source:**
  * `packages/client/src/{session,fs,acl,log,kv,stream,index}.ts`
  * new: `{integrity,devices}.ts`
* **Testing:** `packages/client/src/testing/{server,index}.ts`, new `adversary.ts`
* **Tests:** `packages/client/test/integrity.test.ts` (new), `helpers.ts`
* **Other:** `.github/workflows/ci.yml` (the `client` build line), `docs/CLIENT.md`, `docs/MILESTONE-6.md`

### Verification
* **Rust:** `cargo fmt --check` and clippy on zen-wasm (wasm32 and host).
* **JS:**
  * `scripts/build-wasm.sh`, `npm run check`, `npx biome check`
  * `npx vitest run packages/client/test/integrity.test.ts` 3×, plus the existing client, `packages/db` and `packages/fs` suites once
  * Playwright smoke unchanged: run it once, since the wasm grows
* **Attacks:** each attack test fails with its check disabled (logged).
* **CI:** green on both runs.

**Defaults chosen (overridable):**
* Integrity is opt-in per `Fs` (`fs.integrity(...)`). Without it, nothing changes, and bodies stay v1.
* A detected attack throws from the detecting call and stops checkpoint writes; the rest is app policy.
* Streams check gaps and rewrites only; freshness is checked at the end of reads and by `verifyHead`.
* A gap in the middle of a topic is tested with hand-sealed events, not a new adversary lie: the server can only withhold tails, which the adversary covers.

## Step 6.4 in detail

Step 6.4 puts trees and relayed connections into device checkpoints:
* `@zen/fs` replicas compute the tree state (fs.md §9.1), check each caught-up point against the floors, and note `[2, tree, ops, chain, state]`.
* `@zen/svc` handlers note `[5, svc, conn, in_acked, out_head]` after each committed step. A successor syncs the fs's checkpoints before it resumes a claim, and refuses a claim below the floor (svc.md §12; TD-SVC-STATE-ROLLBACK, except for the handler's database, which waits on 6.5).

Both are opt-in, like topics in 6.3: they use the `Integrity` attached to the fs (`fs.integrity(opts)`), and nothing changes without one.

**Survey (what this step builds on):**
* **The replica** (`packages/fs/src/replica.ts`):
  * It keeps every node the change feed delivered in `nodes` (each `TreeNode` keeps its raw `NodeState`), stored under `nodes`, with the cursor under `sync`.
  * `apply` takes a batch: newer offsets win, a change without a state drops the node.
  * `catchUp` (serialized) runs a full sync when there's no cursor, or after a `resync`, and drops the nodes the full sync didn't return. `run` is the owner's long-poll loop.
  * **They aren't serialized with each other**, so one can apply changes newer than a batch the other is still handling.
  * Only the owner (Web Lock, or the store's lock file) follows the feed. Other tabs reload what it announces.
* **The feed** (`Tree.changes`): yields only batches with changes. It drops `ops`/`chain`, which the server sends when `more` is false (6.2).
* **The wasm `TreeState`** (6.1): `set(StateNode)`, `remove(node)`, `root()`, `size`. `StateNode` is camelCase (`moveHlc`, …) and `NodeState` is snake_case (`move_hlc`, …), so the client needs a mapping.
* **`Integrity`** (6.3):
  * `note(entry)` raises the floor (a different `chain`/`state` at the same `ops` from another writer is a fork) and queues the entry for the next checkpoint. `floor(entryKey(e))` reads the floor back.
  * `mergeEntry`, `lowerThan` and `divergent` already know trees and connections.
  * Nothing yet checks a server answer about a tree or a connection *against* the floor.
* **The server's trash purge** (`tree.rs` `purge_trash`, run by the sweeper): it removes node records and leaves tombstones, **without advancing the tree's op count or chain**. So two honest replicas at the same `ops`, one before a purge and one after, hold different node sets. That contradicts fs.md §9's rule that devices at the same `ops` hold the same state, and every purge would raise a false fork. Nothing else changes a node record outside an operation: undo and redo are part of applying one, and the move-log trim, tombstone drop and chunk GC don't touch records.
* **svc** (`packages/svc/src/serve.ts`):
  * `claimed` → `load` checks cursors against `seen`, the highest cursors this process saw. A fresh process, or a handler on another device, has no such floor (the TD).
  * The step commit's cursors are recorded with `see(…, b.inTo, b.outHead)`.
  * `giveUp(conn, close)` releases a connection, and `stop()` releases the lease.
  * Claims come from `/v1/svc/next`, a write transaction, so the 6.2 adversary (reads only) can't lie in a claim.
* **Test harnesses:**
  * `packages/fs/test/helpers.ts` provides `world`/`installation` (a client with a `net` switch) and `solo()` options. Sweeper tests set `limits.crdt_horizon_secs`, and `limits.sweep_interval_secs` (minimum 1 s) exists.
  * `apps/shared` provides `startWorld` and `signInWorker` (device file, passphrase), and the echo service with in-process handlers.
  * 6.3's `slowFs` showed a `fetch` wrapper on a client works for per-session tampering.

### Spec (`spec:` commit)

* **formats.md §11.5: a purge record.** A purge round (one transaction) extends the tree's op chain:
  ```
  purge = 0x05 ‖ u32(fs) ‖ tree ‖ u32(n) ‖ n × node      (the nodes in the order removed)
  chain_n = H("zen/v1/tree-op-chain", chain_{n−1} ‖ lp(purge) ‖ 0³²)   (the device is 32 zero bytes: the server)
  ```
  `ops` counts it. Clients never build one; they only compare `(ops, chain)`. The sentence on `verifyChain`-style replay says a replay from a client's own records covers a tree until its first purge.
* **fs.md §6:** the trash-purge bullet says that each round adds a purge record to the op chain, so `(ops, chain)` names every change of the node set.
* **fs.md §9 (tree state):**
  * At the same `ops`, honest replicas hold the same state, since operations and purges are all in the chain.
  * **When a replica checks:** a replica checks only where its node table is exactly the server's at a `changes` response with `more: false`. That means it has applied that response and nothing newer, and after a full sync, it has also dropped the nodes the full sync didn't return.
  * **What it checks** against the floor (formats.md §14.4):
    * an `ops` below the floor is a rollback
    * the same `ops` with another `chain` or `state` is a fork
* **api.md §12** (`tree/chain`): `ops` counts operations and purge rounds.
* **svc.md §12:**
  * A successor syncs the fs's device checkpoints before it resumes claimed connections.
  * A handler writes a checkpoint before it releases a live connection or its lease.
  * The cadence is the device's (`sig_every`): every noted step counts.

### zen-core and the server

* **zen-core `fs.rs`:** `purge_op_bytes(fs, tree, nodes) -> Vec<u8>`.
* **Vectors:** `vectors.rs` adds `purge_op` and its chain step to `fs.json` (after the existing op chain), regenerated.
* **zen-server `tree.rs` `purge_trash`:**
  * When `doomed` isn't empty, read the tree header normally (`get`, so the round conflicts with a concurrent commit on the tree, which retries).
  * Set `ops += 1` and `chain = chain_next(chain, purge_op_bytes(fs, tree, doomed ids), [0; 32])`, and write it back in the same transaction as the removal and the tombstones.
* **Tests:**
  * `crates/zen-server/tests/fs.rs`: the existing purge test also checks that `tree/chain` moved by exactly one record per round, equal to `chain_next` over the purged ids in order.
  * A `changes` response with `more: false` after the purge carries the new `(ops, chain)`.
  * Both backends (FDB once).

### `@zen/client`

* **`tree.ts`:**
  * `ChangeBatch.head?: {ops, chain}`, present when the response had `more: false`.
  * `changes(cursor, {heads: true})` also yields batches with no changes when they carry a head, so a follower sees every caught-up point. Without `heads`, nothing changes for existing callers.
  * `stateNodeOf(s: NodeState): StateNode`, the mapping for `zw.TreeState`.
* **`integrity.ts`:**
  * **`observe(entry, {count?})`:** the freshness check of a server answer about an object (formats.md §14.4), then `note`:
    * `divergent` from the floor is a **fork**
    * `lowerThan` the floor is a **rollback**
    * otherwise it's noted
  * `count` defaults to false: what a device verified rides in its next checkpoint without forcing one. Writers pass true.
  * Exported for 6.5 too (namespaces and volumes).

### `@zen/fs` (`replica.ts`)

* **The state:** a `zw.TreeState` kept with the server's node table:
  * `set` in `apply` and `load`, `remove` when `apply` drops a node and when a full sync drops the ones it didn't return
  * freed on `close`
  * only the owner keeps it (other tabs don't follow the feed)
* **The check, with an `Integrity` attached** (`fs.integrity`):
  * Both feed loops pass `{heads: true}`.
  * A batch with a `head` is checked when the replica isn't in a full sync and its saved cursor is that batch's cursor (or the request's cursor, for an empty batch): nothing newer was applied by the other loop.
  * After a full sync, the last head is checked once the unseen nodes are dropped, under the same cursor condition.
  * The check is `integrity.observe({type: 'tree', tree, ops, chain, state: treeState.root()}, {count})`, where `count` is true when this replica sent operations since its last check.
* **An alarm** (an `IntegrityError`):
  * The feed loop stops: no retries, no further applies. `sync()` rethrows the error, and `status()` gains `alarm?`.
  * Local writes still queue and send: the server can't forge them, and what to do is app policy (docs).
* **Cost:** one leaf hash per changed node, and one bucket plus two inner nodes per change at `root()`. On `load`, the state is rebuilt from the stored nodes (one leaf hash each). STATS gets a number for 100,000 nodes.

### `@zen/svc` (`serve.ts`), with `db.fs.integrity` attached

* **After each committed step**, next to `see(…)`, call `integrity.note({type: 'svc', svc, conn, inAcked, outHead}, {count: true})`. Only known-applied commits are noted; an unknown outcome goes through `resync` as today.
* **Before resuming claims:** in `claim()`, when `next` returned connections, `await integrity.sync()` once for the batch (network errors retry through `retry`). An `IntegrityError` there is reported, and the batch's connections are given up with `close` (the server is lying).
* **In `load`:**
  * The floor check: `integrity.floor(entryKey(svc entry))` with `in_acked` or `out_head` below it raises a **rollback** alarm (`object: 'svc'`, `id: svc ‖ conn`). The existing path then reports it and aborts the connection, as a regression against `seen` does today.
  * The claim's cursors are then observed (`count: false`).
* **Before a release:**
  * `giveUp(conn, false)` (a live connection handed back) awaits `integrity.checkpoint()` first, and so does `stop()` before `releaseLease`. A successor then sees the newest cursors.
  * A crash leaves the window since the last checkpoint (cadence, or `sigEvery: 1`).
  * Closing connections (`close: true`) don't checkpoint: no one resumes them.
* **Cumulative bodies** carry one entry per connection served. Ended connections' entries go first when a body is full, since they're unchanged the longest (formats.md §14.1).

### `apps/shared` (`node.ts`)

* `WorkerSignIn.integrity?: Omit<IntegrityOptions, 'device'> | true`: attaches `fs.integrity({device: {device, cert}, …})` from the device file before unlocking.
* `WorkerSignIn.fetch?`: for tests that tamper with one handler's transport.
* `workerFromEnv` reads `ZEN_INTEGRITY=1`.

### Tests

**`packages/fs/test/integrity.test.ts` (new)**, replicas with `Integrity` and signers:
* **`honest`:**
  * Two installations (two devices) write concurrently and follow each other.
  * At every `ops` both replicas checked, their states are equal, and equal to a third replica's full sync from scratch (incremental equals scratch).
  * Each device's checkpoint carries the tree entry. Syncs report nothing.
  * A replica reopened from its store rebuilds the same state, and checks again with no alarm.
* **`purge`** (`crdt_horizon_secs: 1`, `sweep_interval_secs: 1`):
  * A subtree is trashed, and A's replica checks at `ops = n` with it still there.
  * After the purge, B's replica (a full sync) is at `n + 1` with it gone, and `tree.chain().ops` is `n + 1`.
  * No alarm on either side, after both checkpoint and sync.
  * Negative control, run once by hand with the server change reverted: B is at `n` without the subtree, which gives a false fork (logged).
* **`rollback`** (whole-store copy, `server.fork()`):
  * B's replica checks at `ops = n` and closes. A writes on the original, its replica reaches `n + 2`, and A checkpoints. B syncs on the original (floor `n + 2`).
  * B reopens its replica on the copy, with the same stores (the attack works: the copy serves `n`). `sync()` throws a tree rollback.
  * `status().alarm` is set, and the feed has stopped.
* **`fork`** (`server.fork()`):
  * A writes op X on S1, and B writes op Y on S2. Each replica checks at the same `ops` with different chains, with no alarm on its own side.
  * B moves to S1 with its pins (a new session, the same local store): its sync of A's checkpoint, or its replica's next check, gives a tree fork.
* **`wrong merge`** (one session's transport tampered):
  * B's client rewrites one node's meta in its `changes` responses, so `(ops, chain)` are the server's but the state isn't.
  * The attack works (B's replica shows the old name), and B's check against A's checkpoint at the same `ops` is a fork ("tree state changed at the same ops").
  * Negative control: with the state comparison off, no alarm.
* **Each attack test fails with its check disabled** (logged).

**`apps/echo/test/integrity.test.ts` (new)**, echo handlers with `integrity: {sigEvery: 1}`:
* **`honest`:**
  * Lines echo. After each step, the device's head checkpoint has the connection's entry with the cursors the server holds.
  * `stop()` writes a checkpoint before releasing.
  * A successor (a new context, fresh pins) resumes the connection after syncing, with no alarm, and the echo continues in order.
* **`rolled-back claim`:**
  * H1 serves a connection for a few lines, then stops (checkpoint, release, requeue).
  * H2 has fresh pins and a transport that replaces its claim's cursors, state and pending with H1's first claim of the same connection, recorded earlier (an older, consistent snapshot, with the real token).
  * Without the check, H2 would accept it and echo the old lines again (the attack works with the check disabled).
  * With it, H2 reports a rollback (`object: 'svc'`) and the connection is aborted (the peer sees it close).
* **`withheld checkpoint`:** the 6.2 lie `checkpoints` on H1's device after its first checkpoint. H2 then sees H1's older entry only, which is the documented window, but H1's own next sync reports a rollback (6.3), so the handler stops trusting the fs. This test documents the limit and asserts both behaviours.

**Existing suites:** `packages/fs`, `packages/fuse`, `packages/svc`, `apps/echo` and `packages/client` (`tree`, `integrity`) unchanged and green, plus the server's `fs.rs`/`crdt.rs` on both backends.

### Docs
* **docs/CLIENT.md §12:** trees (`observe`, when replicas check, alarms stop the feed) and connections.
* **docs/SVC.md:** handler checkpoints (cadence, release, successor sync, the window, what's left for 6.5).
* **spec/TECH_DEBT.md:** TD-SVC-STATE-ROLLBACK narrowed to the handler's database (closed in 6.5).
* **docs/STATS.md:** the replica state cost.
* **docs/MILESTONE-6.md:** the log.

### Files
* **Spec:** `spec/{formats,fs,api,svc,TECH_DEBT}.md`
* **Rust:**
  * `crates/zen-core/src/{fs,vectors}.rs`, `spec/test-vectors/fs.json`
  * `crates/zen-server/src/tree.rs`, `crates/zen-server/tests/fs.rs`
* **Client and packages:**
  * `packages/client/src/{tree,integrity,index}.ts`
  * `packages/fs/src/replica.ts`, `packages/fs/test/integrity.test.ts` (new)
  * `packages/svc/src/serve.ts`
* **Apps:** `apps/shared/node.ts`, `apps/echo/test/integrity.test.ts` (new)
* **Docs:** `docs/{CLIENT,SVC,STATS,MILESTONE-6}.md`

### Verification
* **Rust:**
  * `cargo fmt --check`; clippy `-D warnings` (default, `--no-default-features`, `--all-features`)
  * `cargo test -p zen-core --features test-utils`; regenerate the vectors (only `fs.json` changes)
  * `cargo test -p zen-server --test fs --test crdt`, embedded and on local FDB
* **JS:**
  * `scripts/build-wasm.sh`, `npm run check`
  * the two new test files 3×; `packages/fs`, `packages/fuse`, `packages/svc`, `apps/echo`, and `packages/client` `tree`/`integrity` once; then the full vitest run
* **Attacks:** each attack test fails with its check disabled (logged).
* **CI:** green on both runs.

**Defaults chosen (overridable):**
* **Purge records** in the op chain rather than leaving trash out of the state. Leaving it out would make a node's leaf depend on its ancestors, so a move into the trash would rehash its whole subtree.
* **Reads don't force checkpoints** (`count: false`): only writers' changes count toward the cadence.
* **After an alarm**, a replica stops following the feed but keeps sending writes. A handler aborts the claimed connections it can't verify.
* **Tampering is per session, at the transport**, for the wrong-merge and claim tests. The 6.2 adversary lies to every reader, and can't lie in claims, which are write transactions.

## Step 6.5 in detail

Step 6.5 builds the zen-db authenticated tier (zendb.md §14, `integrity: "authenticated"`). Today a server can serve an older valid row, an older RootRecord together with its older tree, an older catalog record, or leave a row out of a scan, and nothing notices. In an authenticated namespace:
* every value the client reads is checked against a **namespace root** (`NsRoot`) read in the same transaction;
* the `NsRoot` is checked against the floors from device checkpoints (entry type 3, `[3, ns, seq, hash]`), which writers note after each commit.

The same work closes the rest of TD-SVC-STATE-ROLLBACK: a handler's database can't be rolled back beyond the checkpoint window.

**Changed from the milestone design** ("NsRoot over index roots plus a row tree per table"): **one state tree per namespace covers every value under `D`**. That means rows, parts, catalog records, unique, fast and sealed entries, RootRecords, message parts and system tables, all except index nodes. The tree is keyed by stored key, so:
* every index kind and every feature is authenticated without code of its own;
* range reads (scans, fast lookups, the catalog) can be checked for completeness;
* its write locality is the stored-key locality the server already sees.

**Survey (what this step builds on):**
* **Reads** happen only through the M4 `Transaction` (`packages/client/src/kv.ts`): `getEntries` (with `get`/`getAll`), `snapshotGet`, `rangeStored` and `snapshotRangeStored`. That's 23 call sites in `packages/db`, plus one outside a transaction: `Db.loadCatalog`, through `fs.kv.rangeStored`.
* **Writes:** `raw.writes` holds every set or delete, already sealed (`kv.open` gives the plaintext back). `raw.clears` holds the cleared ranges: paged table and index drops (`migrate.ts` `clearPage`, at most `max_range_items` keys each) and the scheduler's `gc` (`sched.ts`).
* **The prolly tree** (`index/prolly.ts` `rewrite`, `scan`; `index/node.ts`) is generic over entry bytes and `TreeKeys`. Node ids are 16 bytes everywhere: `decodeNode` checks `child.length === 16`, `decodeRoot` checks `root`, and `entryLen` counts 17 bytes for a child id. The Rust reference is `zen-core/src/db/prolly.rs` (`build`, `Entry::Child {child: [u8; 16]}`), and wasm `IndexKeys.build`/`nodeId`.
* **Caches:**
  * rows (`cachedRow`, with a version, persisted by `encodeCachedRow`), and the catalog (`catalogEntry`, with a version): both are used through `useCached` → `expectKey`, and read-only transactions check them at `finish`;
  * nodes, by id (`cacheNode`, and `keptNode`, which checks the id).
* **Transactions:** a new `DbTransaction` per attempt (`db.ts` `transaction`). `finish()` applies private-index and sealed changes, and is the last point before the commit is built.
* **Integrity** (`@zen/client`, 6.3/6.4):
  * `fs.integrity` gives the attached `Integrity`;
  * `observe(entry, {floorAt, count})` and `floor(entryKey(e))`;
  * `mergeEntry`, `lowerThan` and `divergent` already handle `db` entries (seq, and hash at the same seq);
  * `raise(alarm)` is public.
* **The opening path:** `Db.open` → `init()` runs one transaction, which reads the DbRecord and creates it with `integrity: 'basic'`; `openKept` is the offline path. Both refuse an integrity other than `basic`.
* **svc:** the step loop reports a step error and resyncs (`serve.ts`); only `resync` aborts on `integrity`. Echo writes one `lines` row per step (`tx.table('lines').insert`).
* **The adversary** `kv` lie freezes one stored key (value or absence, also inside ranges). `testing/adversary.ts` `lie(session, [{kind: 'kv', fs, key}])`.

### Spec (`spec:` commit)

**zendb.md §14 rewritten** (the authenticated tier, normative):
* **§14.1 Levels.**
  * `basic`, as today.
  * `authenticated`, chosen at creation (`DbRecord.integrity`) and never changed.
  * An authenticated namespace needs integrity attached to the fs (formats.md §14), with or without a signer. The client refuses to open one without it.
  * **Not offered** in authenticated namespaces: CRDT tables (§19), whose merged state is the server's. `createTable` refuses `merge: "crdt"` with `bad_value`.
* **§14.2 The state tree.** A prolly tree (§5.4.1), fanout 64, with the keys of the reserved index id `0¹⁶` (`K_boundary`, `K_node`), nodes at `D ‖ ("n", 0¹⁶, node_id)`. Its level-0 entries, one per stored value under `D` except `("n", …)` nodes and the `NsRoot`:
  ```
  entry      = esc(suffix) ‖ 0x00 ‖ VH            suffix = the stored key without the stored-key prefix of D
  esc(x)     = x with each 0x00 replaced by 0x00 0xFF   (as in §5.1; order-preserving and prefix-free)
  VH         = H("zen/v1/db-state-value", plaintext of the sealed value)
  ```
  * So entries sort in stored-key order, the entry of a key is the one starting with `esc(suffix) ‖ 0x00`, and a stored-key range `[b, e)` is the entry range `[esc(b′) ‖ 0, esc(e′) ‖ 0)`.
  * VH covers the plaintext, so re-sealing under a new epoch changes nothing. A Row's parts are values of their own.
* **§14.3 Node ids.** In an authenticated namespace every node id, the state tree's and private indexes', is `PRF32(K_node, CBOR(Node))`: children are `bytes(32)`, and so is `RootRecord.root`.
  * A reader checks each node it fetches against its id.
  * Why 32 bytes: a member and the server together can't find two nodes with one id, so one root names one state, even against members.
* **§14.4 NsRoot** at `D ‖ ("ns")`:
  ```
  NsRoot = { 1: v = 1, 2: seq: u64 (1, 2, …), 3: prev: bytes(32),     // NH(previous NsRoot), zeros for seq 1
             4?: tree: [root: bytes(32), height: u8, count: u64] }    // absent: no values
  NH     = H("zen/v1/db-ns-root", CBOR(NsRoot))
  ```
  No `NsRoot` means seq 0: a namespace with no values.
* **§14.5 Writing.**
  * Every commit that writes or clears a value under `D`:
    1. reads the `NsRoot` (into its read set);
    2. removes each written, deleted or cleared key's entry (found by its prefix) and adds the new one;
    3. rewrites the tree (§5.4.4);
    4. writes `NsRoot {seq + 1, prev = NH(old), tree}`.
  * So **every writer of an authenticated namespace conflicts on its `NsRoot`**: use one for balances, permissions and other state where freshness matters more than write concurrency.
  * After the commit applies, the device's checkpoints carry `[3, ns = stored-key prefix of D, seq, NH]` (formats.md §14), counted toward the cadence.
* **§14.6 Reading.**
  * A transaction reads the `NsRoot` once, at its first read under `D`. For every value the server returns under `D`:
    * **a point read:** the tree's entry for the key must exist with this VH, or be absent with the value;
    * **a range read:** the entries in the covered range must be exactly the returned items, with their VHs. The covered range is the whole range, or up to the last item when a limit or page stopped it.
  * A cached record (row or catalog) is used only when the tree's entry still has its VH; otherwise it is dropped and read again.
  * **A mismatch:**
    * The client re-reads the latest `NsRoot`. If it moved, the transaction conflicts and retries: a long-mode read, or a short read that raced, can honestly be newer than the transaction's root.
    * If it didn't move, it's a **rollback** alarm (`object: "db"`).
    * A node that doesn't match its id is `invalid`.
* **§14.7 Freshness.** The `NsRoot` read is checked against the floor of `[3, ns]` (formats.md §14.4):
  * a lower `seq` is a rollback; the same `seq` with another `NH` is a fork;
  * at the floor's `seq + 1`, `prev` must be the floor's `NH`, or it's a fork;
  * the floor used for the rollback check is the one at the start of the transaction's attempt (as fs.md §9: a checkpoint arriving mid-transaction can honestly be newer than its read version).
  * The `NsRoot` read is noted (not counted). The window between a write and its device's next checkpoint is the cadence's (`sigEvery: 1` closes it).
* **§14.8 Cost** (informative): per uncached point read, about `height` node reads, sharing the upper levels with every other read; per commit, about `2 × height` node writes and deletes per touched region, plus the `NsRoot`. STATS has the numbers.
* The paragraph on `sig/db-root` stays: no signature per commit.

**Elsewhere in the spec:**
* **zendb.md:**
  * §2.1 table: `("ns")` and the state-tree nodes.
  * §3.1: `integrity: "basic" | "authenticated"`, with the opening rule (below).
  * §5.4.2: ids are 32 bytes in authenticated namespaces.
  * §15: leakage (the state tree's node count and write locality follow stored keys; the `NsRoot` rewrite per commit shows commits, which the server sees anyway).
  * §17: the new vectors.
* **formats.md §14.1:** the type-3 sentence names zendb.md §14.4 and the 48-byte prefix.
* **labels.md and `labels.rs`:** `zen/v1/db-state-value` and `zen/v1/db-ns-root`.
* **svc.md §12:** a handler's database, when authenticated, is covered the same way. TECH_DEBT **TD-SVC-STATE-ROLLBACK closed**: what's left is the cadence window, documented in svc.md §12 and docs/SVC.md.

**Opening rule (§3.1):** `Db.open(fs, ns, {integrity})`.
* With `integrity: 'authenticated'`, the database is created that way, the open is verified from the first read, and a stored `basic` DbRecord is refused (`format`).
* Without the option, a stored `authenticated` record switches the `Db` to verified mode, and the open re-runs verified.
* A missing record with a floor for the namespace is a rollback, not a new database: an absent `NsRoot` is seq 0.

### zen-core and wasm

* **`db/mod.rs`:**
  * `IndexKeys::node_id32` (`PRF32`; add `prf32` to `kdf.rs` next to `prf16`, same construction with 32 output bytes)
  * `DbKeys::state()` = `index(&[0; 16])`
* **`db/prolly.rs`:**
  * `Entry::Child.child` becomes `Vec<u8>`
  * `build(keys, fanout, entries, id_len)` with `id_len` 16 or 32; existing callers pass 16, and the existing vectors stay byte-identical
* **New `db/auth.rs`:**
  * `state_entry(suffix, plaintext)` and `value_hash`
  * `NsRoot {seq, prev, tree}` with `encode`/`decode` (strict) and `hash`
  * `state_tree(keys, values: &[(suffix, plaintext)]) -> Tree` (fanout 64, 32-byte ids)
* **Vectors** (`zendb.json`, a new `authenticated` section; only that section changes):
  * state keys for the fixture namespace
  * ten values, then their entries
  * the tree at fanout 4: every node's bytes and 32-byte id
  * `NsRoot` seq 1 and seq 2 (after changing one value and deleting one): bytes and `NH`
* **wasm (`db.rs`):**
  * `IndexKeys.nodeId32`, and `build(entries, fanout, idLen)`
  * `stateEntry(suffix, plaintext)`, `stateValueHash`, `nsRootHash`
  * Rebuild with `scripts/build-wasm.sh`.

### `@zen/client` (`kv.ts`)

* **`Transaction.onRead(fn)`:** hooks awaited after each server answer, before own writes are overlaid. Their argument is one of:
  * `{keys, values}` for `getEntries` and `snapshotGet` (the keys asked of the server, opened values or undefined);
  * `{begin, end, reverse, items}` for each page of `rangeStored` and `snapshotRangeStored`, where `[begin, end)` is the range that page covers (to the last item when a limit or `more` stopped it).
* A hook that throws fails the read. Nothing else changes, so existing users are unaffected.

### `@zen/db`

* **`index/node.ts`:**
  * `decodeNode(b, idLen)`, `decodeRoot(b, idLen)`
  * `entryLen` counts the id's CBOR head: a 16-byte id is 17 bytes, a 32-byte one is 34
  * `Db.idLen` (16, or 32 when authenticated), passed everywhere nodes and roots are decoded
  * `treeKeys(ik, fanout, idLen)` picks `nodeId` or `nodeId32`; `TxNodeStore` checks fetched nodes against their id when authenticated (`invalid` alarm); `keptNode` uses the same check
* **`index/prolly.ts`:** `lookup(store, root, prefixes)` finds the entry starting with each prefix, descending level by level with one `store.get` per level for the whole batch.
* **New `auth.ts`:**
  * **`NsAuth`, one per `Db`:**
    * `ns` = `keys.db.prefix()`, the state `TreeKeys`, the `NsRoot` key, and the prefixes it skips: `("n")` and `("ns")`
    * `encodeNsRoot`/`decodeNsRoot` (TS CBOR, like `node.ts`)
    * `entryOf(key, plaintext)`
  * **`TxAuth`, one per attempt** (made in the `DbTransaction` constructor when authenticated):
    * `floorAt = integrity.floor(key)`, taken at construction
    * `raw.onRead(verify)`
    * **`root()`:** reads the `NsRoot` once (`raw.get`, so it's in the read set), checks `prev` against the floor, and `observe`s `{type: 'db', ns, seq, hash}` with `floorAt`
    * **`verify(read)`:** skips keys outside `D` and the skipped prefixes. Point reads go through `lookup`; ranges go through `scan` over the mapped entry range and are compared item by item. A mismatch takes the §14.6 path: a fresh `NsRoot` read through a new short `Transaction` (latest version); if it moved, `ZenError(409, 'conflict')`, else `integrity.raise({kind: 'rollback', object: 'db', id: ns, detail})`.
    * **`current(key, vh)`** for caches: whether the tree's entry still has this VH.
    * **`finish()`:** called last in `DbTransaction.finish` when `raw.writes` or `raw.clears` touch `D` (outside the skipped prefixes). It builds the changes:
      * for each cleared range, scan the tree and remove every entry in it;
      * for each write, remove the old entry (`lookup`) and add `entryOf(key, kv.open(…))` for a set.
      
      Then it `rewrite`s the tree with 32-byte ids, writes and deletes nodes, and sets the new `NsRoot`. `raw.onCommit` notes `{seq + 1, NH}` with `count: true`.
* **`txn.ts`:**
  * The constructor makes `TxAuth`; `finish()` calls `auth.finish()` after the sealed indexes and CRDT stamping, before `checkLimits`.
  * **Caches:** `useCached` gains the cached VH. When authenticated, `read` and `tableRecordOrNone` check `current(key, vh)` first, and a stale entry is dropped and read again.
  * A read-only authenticated transaction skips the end-of-transaction version check of cached records: the tree check replaced it.
* **`db.ts`:**
  * `DbOptions.integrity?: 'basic' | 'authenticated'`
  * `init()` and `openKept` follow the opening rule
  * `createTable` refuses CRDT tables in authenticated namespaces
  * `loadCatalog` reads inside a transaction when authenticated
  * `CachedRow` and catalog entries keep `vh`; `encodeCachedRow` gains an optional field, so old kept rows decode without one and are read again
  * `Db.integrity` names the level
* **`migrate.ts` and `sched.ts`:** unchanged. Their clears are paged or bounded, and `TxAuth.finish` handles them.

### `@zen/svc` and the apps

* **`serve.ts`:** a step whose error is `integrity` gives the connection up with `close` (as `resync` does), instead of reporting and resyncing again and again.
* **`apps/ledger`:** `AUTH_DB_OPTIONS = {...DB_OPTIONS, integrity: 'authenticated'}`, exported. `ledger.test.ts` runs its two balance-invariant tests over both: the authenticated run attaches integrity to each trader's fs. The UI keeps `basic`; docs/EXAMPLES.md §5 says how to switch.
* **`apps/echo`:** unchanged code. The integrity test file opens the echo database authenticated.

### Tests

**`packages/db/test/authenticated.test.ts` (new):** spawned server with the adversary on, integrity attached, two installations (devices).

* **`honest`:**
  * create; CRUD; queries over every index kind (private, unique `none`, fast, sealed); a migration that adds an index (backfill) and drops a table (paged clears)
  * the scheduler (a delayed emit and a `gc`); a large row with parts; a large message body
  * a reopen from the local store (kept rows and nodes)
  * Both devices' checkpoints carry the `db` entry, `sync` reports nothing, and there are no alarms.
* **`state equals scratch`** (property): after 300 random puts, deletes and clears, the `NsRoot`'s tree root equals the reference `IndexKeys.build` over every value under `D` (read with `snapshotRange`, nodes and `NsRoot` left out, 32-byte ids).
* **`concurrent writers`:** two devices, three loops each, move balances 30 times; the sum is unchanged, conflicts happened (counted), and there are no alarms.
* **`row rollback`:**
  * lie `kv` on a row's key, then the other device updates the row;
  * on a `basic` copy of the setup, the stale row is served silently (the attack works);
  * authenticated, a `get` and a query raise a `db` rollback.
* **`index root rollback`:** lie `kv` on a private index's RootRecord; a range query raises a rollback.
* **`withheld row`:** lie `kv` on an absent row key, then insert it elsewhere; a scan and a fast lookup raise a rollback (the item is missing from the range).
* **`stale cache`:** a cached row whose tree entry changed is read again, with no alarm, in a read-only and in a write transaction.
* **`NsRoot rollback`:**
  * lie `kv` on the `NsRoot` key; the writer commits twice and checkpoints;
  * a reader that synced raises a rollback (seq below the floor);
  * a fresh reader that didn't sync sees a consistent old state: the window, asserted and documented.
* **`fork`** (`server.fork()`): one device writes on each side. Each side alone sees no alarm; after the checkpoints cross, the same seq with another `NH`, or a broken `prev`, is a fork.
* **`opening`:**
  * `integrity: 'authenticated'` over a stored `basic` database gives `format`;
  * without integrity attached, the open is refused;
  * a database created authenticated reopens verified without the option;
  * an absent DbRecord and `NsRoot` with a floor raise a rollback, not a new database.
* **`refused`:** a CRDT table in an authenticated namespace gives `bad_value`.

**`apps/echo/test/integrity.test.ts` (one more test):** "a rolled-back handler database":
* H1 serves `a`; lie `kv` on the echo database's `NsRoot`; H1 serves `b` (its own next read alarms: the root is below its own note); H1 stops.
* The successor syncs, its `lines` insert reads the database, and it raises a rollback.
* The connection is aborted (the peer sees it close).

**Negative controls** (each run once, then restored, and logged):
* the point-read VH check off: the row rollback test fails
* the range completeness check off: the withheld row test fails
* the node id check off: no test needs it (logged as such)
* `observe` of the `NsRoot` off: the `NsRoot` rollback and echo tests fail
* the `prev` check off: the fork test still alarms by same-seq hash, so logged which check caught it

**Existing suites:** all of `packages/db` (basic is unchanged, so the existing tests stay as they are); `apps/ledger` (both runs); `apps/echo`; `packages/svc`; `packages/client` `kv`; the vectors tests (Rust and TS).

### Docs
* **docs/DB.md:** a new "Authenticated namespaces" section: what's checked, the contention rule, refused CRDT tables, needing integrity, alarms, cost.
* **docs/CLIENT.md §12:** a namespaces bullet.
* **docs/SVC.md §6:** the handler's database.
* **docs/EXAMPLES.md §5:** the ledger option.
* **spec/TECH_DEBT.md:** TD-SVC-STATE-ROLLBACK closed.
* **docs/STATS.md:** authenticated vs basic, measured with `bench.test.ts`'s harness (an opt-in bench case): point read (cold, warm), insert, a 20-row transaction (ops and bytes), and a 100-row scan.
* **docs/MILESTONE-6.md:** the log.

### Files
* **Spec:** `spec/{zendb,formats,labels,svc,TECH_DEBT}.md`
* **Rust:**
  * `crates/zen-core/src/{kdf,labels,vectors}.rs`, `crates/zen-core/src/db/{mod,prolly,auth (new)}.rs`
  * `spec/test-vectors/zendb.json`
  * `crates/zen-wasm/src/db.rs`
* **Client:** `packages/client/src/kv.ts`
* **zen-db:** `packages/db/src/{auth (new),txn,db,index/node,index/prolly,index/private}.ts`, plus `packages/db/test/{authenticated (new),vectors,bench}.test.ts`
* **svc:** `packages/svc/src/serve.ts`
* **Apps:** `apps/ledger/{src/ledger.ts,test/ledger.test.ts}`, `apps/echo/test/integrity.test.ts`
* **Docs:** `docs/{DB,CLIENT,SVC,EXAMPLES,STATS,MILESTONE-6}.md`

### Verification
* **Rust:**
  * `cargo fmt --check`; clippy `-D warnings` on zen-core and zen-wasm (default, `--all-features`, wasm32)
  * `cargo test -p zen-core --features test-utils`; regenerate the vectors (only `zendb.json`'s new section changes)
* **JS:**
  * `scripts/build-wasm.sh`, `npm run check`
  * the new test file and the echo integrity file 3×; `packages/db`, `apps/ledger`, `apps/echo`, `packages/svc` and the client `kv` tests once; then the full vitest run
* **Attacks:** the negative controls above.
* **CI:** green on both runs.

**Defaults chosen (overridable):**
* **One state tree per namespace** over every value, rather than per-table row trees and roots in the `NsRoot`: one mechanism covers every index kind and range completeness.
* **Integrity attached is required** for an authenticated namespace: without checkpoints, there's consistency but no freshness, and one alarm path is simpler.
* **CRDT tables are refused** in authenticated namespaces.
* **The ledger UI stays `basic`;** its tests run both levels.
* **A mismatch first re-reads the `NsRoot`:** if it moved, the transaction conflicts and retries rather than alarming, so long mode and racing short reads stay honest.

## Step 6.6 in detail

Step 6.6 builds `crates/zen-client`, a native Rust client: what `zen-blk` (6.7) and `zen-ublk` (6.8) need, and a CLI for checking pins out of band. It covers:
* sign-in with a device key file
* the ACL walk with a pin
* the fs header (signed or not) and unlocking with a device keyslot
* KV reads, transactions and commits, with `fence`
* named leases
* integrity: checkpoints, sync, floors, pins kept in a file, exported pins

It is a port of the matching parts of `@zen/client`, with the same rules, so a TS and a Rust device verify each other. Topics, streams, trees, svc and zen-db stay TS-only (docs say so).

**Survey (what this step builds on):**
* **No Rust client exists.** The nearest is zen-server's test harness (`tests/common/mod.rs`):
  * `call` (a CBOR POST with a Bearer token, `ErrorBody` on non-200)
  * `sign_in_at` (challenge, `session_message`, the device's `SIG_SESSION` signature, `SessionRequest`)
  * `claim`, `signed_acl`, `checkpoint_of`
* **zen-core has every primitive:**
  * keys: `FsKeys`, `token::kv_key`
  * sealing: `seal::{seal_value, open_value, peek}`
  * keyslots: `keyslot::{create_device, open(Unlock::Device)}`
  * the header: `header::{FsHeader, SignedHeader, is_signed, validate_header_successor}`, and `FsHeader::keys_at` for older epochs
  * signatures: `sig::{SigningIdentity, DeviceSecret, verify_device_cert}`, `kdf::acl_hash`
  * checkpoints: `integrity::{CheckpointBody, Entry, sign_checkpoint, verify_checkpoint, seal_checkpoint, open_checkpoint, checkpoint_hash}`
  * the clock: `fs::Clock`
* **zen-proto** has every wire type, `to_cbor`/`from_cbor`, `session_message`, `valid_origin`. The ACL chain rules are in the TS client (`acl.ts` `verifyNext`) and the server (`acl.rs` `validate_successor`), not zen-core.
* **TLS:** `zen_tls_rustcrypto::provider()` is pure Rust, with `X25519MLKEM768` first. reqwest 0.13 is in the lock with `rustls-no-provider`, `rustls-platform-verifier` and `hyper-rustls`.
* **The TS logic to port:**
  * `integrity.ts` (1,118 lines): `mergeEntry`, `lowerThan`, `divergent`, `raiseFloor`, `note`/`observe`, `checkpointNow` (with the 409 adopt-and-merge and idempotent resends), `buildBody` (cumulative, oldest dropped when too big), `advance`, `checkClaims`/`checkAcl`, `checkSeen`, `syncNow`, `encodePins`/`decodePins`
  * `kv.ts`: `Transaction` with short and long mode, `expectKey` (the first version stands), range hashing in long mode, `sendCommit`'s replays with one `commit_id`, the retry loop
  * `acl.ts`; `fs.ts`: header get with signed-header checks, `keysFor` by epoch
* **The device key file** is zen-mount's: JSON `{identity_seed, device_seed, cert}` in hex (`packages/fuse/src/cli.ts`, `apps/shared/node.ts`).
* **The exported pins record** (`1 ‖ u32 fs ‖ acl ‖ header ‖ u32 n ‖ n × (fp ‖ u64 seq ‖ HC)`) is defined only in `integrity.ts`, not in the spec.
* **Forking a server in Rust tests:** `zen_server::dump::copy(from, to, force)` copies a running store into a fresh embedded one, which a second server then opens. That's the Rust counterpart of `TestServer.fork()`.
* **CI:** `rust` and `pure-rust` run `cargo test --all` (embedded). `client` builds `zen-server --features test-utils` before vitest.

### Spec (`spec:` commit, small)

* **formats.md §14.5, exported pins (new):** the record above, moved from the TS client into the spec, so both clients read each other's.
  * Fields: version byte 1; then `acl` and `header` as `u64 version ‖ hash(32)`, with `[0, zeros]` for none; then device entries sorted by fp.
  * Comparing: each device entry is checked as a `seen` link (§14.4 rule 3), and the ACL and header as claims (rule 5).
* **integrity.json:** a `pins` vector (an fs, an ACL head, a header head, two devices) with its bytes. Only that section changes on regeneration.

### zen-core

* **`integrity.rs`:** `ExportedPins {fs, acl: Head, header: Head, devices: Vec<Seen>}` with `encode` and a strict `decode` (version 1, exact length, devices sorted and unique).
* **`vectors.rs`:** the `pins` vector; `tests/vectors.rs` reads it back.
* **The ACL chain rule** isn't moved: zen-proto can't depend on zen-core (zen-core's `test-utils` depends on zen-proto), and the server's `validate_successor` also checks put-time rules. zen-client's `acl.rs` has its own `verify_next` (formats.md §9.3 rules 1–2 and the signature), a port of TS `verifyNext`.

### `crates/zen-client` (new: library and the `zen-client` binary)

**Dependencies:**
* zen-core, zen-proto, zen-tls-rustcrypto
* `reqwest` (`rustls-no-provider`), `rustls`, `rustls-platform-verifier`, `tokio`
* `serde`, `serde_json`, `base64`, `getrandom`
* `clap` (the binary)

No C: the crate builds in the `pure-rust` job.

**Modules:**
* **`error.rs`:** `Error::Api {status, code, message, retry_after}`, plus `Network`, `Integrity(Alarm)`, `Format`, `Crypto`, `Io`. `retryable()` is true for 409 `conflict`/`too_old`, as in TS. `Alarm {kind, object, id, detail}` mirrors `IntegrityAlarm`.
* **`transport.rs`:**
  * `Transport::new(url, TlsOptions {ca: Option<pem>})`
  * reqwest with a `rustls::ClientConfig` on `zen_tls_rustcrypto::provider()`, verifying with the platform verifier, or only the given CA when `ca` is set
  * TLS 1.3 only, a 30 s request timeout
  * `call<Q, R>(path, bearer, &Q)`: a CBOR POST. Errors decode `ErrorBody`. 503, and 429 with `Retry-After`, are retried (3 attempts), as in TS.
* **`client.rs`:**
  * `Client::connect(url, opts)`: `origin` defaults to the URL's origin, checked with `valid_origin`
  * `info()`
  * `sign_in_device(&DeviceKey) -> Session`: challenge, `session_message`, `SIG_SESSION`, `SessionRequest`
  * `session_from_token(bytes)` for an API token or a resumed session
* **`device.rs`:** `DeviceKey {identity: SigningIdentity, device: DeviceSecret, cert}`.
  * `load(path)`/`save(path)` use the zen-mount JSON, saved with mode 0600.
  * `DeviceKey::new(identity_seed, device_seed)` issues the cert, for tests and setup.
  * `fp()` is the device fp; `member()` is an ACL `Member`.
* **`session.rs`:** `Session {transport, token, device: Option<DeviceKey>, clock: fs::Clock}`.
  * `call` re-signs in once on 401 when it holds a device key, then resends. That's safe: commits resend with their `commit_id`, and checkpoint puts are idempotent. A long-running `zen-ublk` outlives `session_ttl_secs`.
  * The clock's wall time is corrected by `/v1/info` `time_ms`, as `TreeClock` does.
* **`acl.rs`:** `Acl` (in the session) keeps the verified head.
  * `head()` walks from the pin with `verify_next`. A head below the pin, or a missing pinned version, gives `acl_chain`, as in TS.
  * `pin(bytes)`, `pinned()`, `entries(from)`.
  * `put_next(signer, edit)`, so tests (and admins) can update.
* **`fs.rs`:**
  * `Fs {session, id}`.
  * **`header()`:**
    * decodes the signed or unsigned form
    * a signed one is verified against an admin of the verified ACL (`SignedHeader::verify` with the admin's member identity)
    * with integrity attached, `check_header` (the formats.md §12.1 pin rules)
  * **Writing:**
    * `init(slots, signed: bool)`: `FsHeader::new`, `create_device` for the key's own device, a CAS on absence
    * `add_device_slot(&DevicePublic)`
    * `edit_header(f)`: signs the successor when the stored header is signed, with the `DeviceKey`'s identity (`signer_needed` without one), and checks with `validate_header_successor` locally first
  * **`unlock_device(&DeviceSecret) -> UnlockedFs`:** it tries each type-3 slot whose recipient is this device (`device_recipient`), else `no_slot`.
  * `UnlockedFs::keys_for(sealed)` picks the epoch from `seal::peek` via `FsHeader::keys_at`. A newer epoch than the header's re-reads the header once, then gives `epoch_ahead`, as in TS.
* **`kv.rs`:**
  * **`Kv`:** `key(path)` (`kv_key`), `get`, `get_many`, `range(prefix | [begin, end), limit, reverse)` (pages through `more`; values opened with their stored key).
  * **`Transaction` (one per attempt):**
    * `get`/`get_many`/`range` record read conflicts in short mode (one read version), or `expect`s and a range hash in long mode (zen-core's range hash, as `zw.RangeHasher`)
    * `snapshot_get`
    * `expect_key` (the first version stands)
    * `set`/`delete`/`clear_range`
    * `fence(name, token)`
    * `on_commit`
    * own writes are overlaid on reads
  * `build()` gives `Commit`.
  * **`UnlockedFs::transaction(opts, async |tx| …)`:** the TS loop, with attempts (default 8), backoff, `retryable`, and an empty transaction committing nothing. It uses Rust async closures (`AsyncFnMut`; Rust here is 1.97, edition 2024).
  * `send_commit` replays one `commit_id` on network errors and `commit_unknown` (4 attempts).
* **`lease.rs`:** `Lease::acquire(fs, name, ttl)`, `renew`, `release`, `token()`. Errors are `not_leader` and `claim_lost`. `Transaction::fence_lease(&Lease)`.
* **`integrity.rs`:** a port of `integrity.ts`.
  * **Options:** `IntegrityOptions {signer: Option<DeviceKey>, pins: Option<PathBuf>, sig_every (64), delay (5 s), on_alarm}`.
  * **Shared state:** `Arc<Mutex<State>>` with one async lock serializing `checkpoint` and `sync`, like TS's `serial`.
  * **The same functions and rules:** `note(entry, count)`, `observe(entry, floor_at, count)`, `floor(key)`, `check_header`, `checkpoint()` (cumulative body, the `max_value_bytes` budget, 409 adopt-and-merge, idempotent resend), `sync()` (rules 1–5, the ACL and header claims, `seen` links, the own head), `export_pins()`/`compare_pins(bytes)` through zen-core's `ExportedPins`, `close()`. Alarms make it untrusted and stop checkpoints.
  * **The cadence:** a tokio task armed by `note` fires `delay` after the last counted change; `sig_every` changes write at once. A `tokio::sync::Notify` cancels the task on `close`.
  * **Entries:** all five types are decoded, merged and kept, since bodies are cumulative and other devices' bodies carry topics and trees too. Only `vol` and `db` entries come from Rust writers in this milestone.
* **`pins.rs`: the pins file**, one per fs. Deterministic CBOR `{1: v = 1, 2: fs, 3?: signed ACL bytes, 4?: [version, HH], 5: [[fp, seq, HC, body]]}`.
  * Written atomically: a temporary file in the same directory, fsync, rename, mode 0600.
  * Loaded at `Integrity::attach`, raising the floors from each body, as TS `load` does.
  * A file of another fs, or one that doesn't decode, is refused (`format`), not ignored: losing pins silently would hide a rollback.
* **`main.rs`: the `zen-client` binary.**
  * Usage: `zen-client --url URL --device-key FILE --fs N [--pins FILE] [--origin O] [--ca FILE] COMMAND`.
  * **Commands:**
    * `sync`
    * `checkpoint`
    * `pins export [--out FILE]` (raw bytes, or hex on stdout)
    * `pins compare FILE|-`
    * `kv get PATH`, `kv set PATH VALUE`, `kv list PATH` (a path is `/`-separated UTF-8 elements, as TS `['a', 'b']`)
    * `header` (version, signed, slots)
  * **Exit codes:** an alarm prints it as JSON on stderr and exits 3; other errors exit 1.
  * Every command attaches integrity with the pins file and syncs first, so `kv get` is checked against the floors.

### Tests (`crates/zen-client/tests/`, the server in-process)

**The harness** (`tests/common/mod.rs`):
* `zen-server` as a dev-dependency (`test-utils`, the adversary on)
* on the embedded backend always: these test the client, and a fork needs a store copy
* `claim(admin, others)` with each user's device in the ACL
* `fork()`: `dump::copy` into a fresh embedded store, then a second server on it
* `lie(...)`: CBOR `{lies, clear}` to `/v1/test/adversary`

**Tests:**
* **`sign_in`:**
  * a device key file round trip, then sign-in, `info`, and the session's calls
  * a cert not in the ACL → 401
  * a wrong origin → refused
  * a 401 after the session is revoked (`/v1/auth/logout`) re-signs in once and the call succeeds
  * API token sessions work
* **`acl_walk`:**
  * versions 1 → 3 verified and pinned
  * `verify_next` unit cases: a version skip, a wrong `prev_hash`, a non-admin signer, a bad signature
  * a fork copy left at version 1 while the original goes to 3: a client pinned at 3 talking to the copy gets `acl_chain`
* **`header_and_unlock`:**
  * `init` unsigned, then `edit_header` adds a second device's slot, and that device unlocks
  * a device with no slot → `no_slot`
  * `init(signed)`: edits keep signing (versions 2, 3); another session verifies them
  * without an identity → `signer_needed`
  * a header rotated with zen-core's `FsHeader::rotate`: values sealed in epoch 1 still open, and new ones seal in epoch 2
* **`kv`:**
  * `get`/`range` with paging (`limit`, `more`, `reverse`), `set`/`delete`/`clear_range`
  * own writes are seen in reads
  * short mode: 2 × 20 concurrent increments end at 40, with conflicts seen
  * long mode: an `expect` conflict, and a range hash that a concurrent insert breaks
  * a commit resent with its `commit_id` applies once
  * values written by Rust open in TS, and back (in the vitest below)
* **`leases`:**
  * acquire, renew, release
  * a fenced commit applies; after another device takes the lease, the old holder's fenced commit gets `claim_lost` and writes nothing
* **`checkpoints`:**
  * device A notes `vol` entries and checkpoints; B syncs and gets A's floor; B's next body `seen`s A, and A's sync checks it
  * `sig_every = 3` writes after 3 notes, `delay` after a pause, and `close` writes the last one
  * bodies are cumulative; the budget drops the oldest entries (`max_value_bytes` set small)
  * pins survive a new `Integrity` from the same file, and a corrupt pins file is refused
  * two `Integrity`s of one device race (409), merge, and both entries reach the head with no alarm
* **`attacks`** (each first asserts the attack works, then that it is detected):
  * **`checkpoints_withheld`:** lie `checkpoints` on A at seq 2; A writes 3 and 4. A's own `sync` → rollback. B alone sees nothing, but `compare_pins(A.export_pins())` → fork.
  * **`vol_rollback`:** an app-level root record (a kv value holding `{seq, hash}`, as 6.7's volume root will). A writes seq 1 → 3, noting each, and checkpoints; lie `kv` at seq 1 earlier. B syncs, reads the frozen record and `observe`s it → rollback. Without syncing, B accepts it: the window, asserted.
  * **`header_rollback`:** lie `header`; the admin signs v2; its next `header()` → rollback, and B learns v2 from the admin's checkpoint and alarms too.
  * **`fork`:** `fork()`; device D writes checkpoint k on each side. A client pinned on side 1, talking to side 2 → fork (same seq, another HC). `compare_pins` across the sides → fork.
  * **`invalid`:** a checkpoint sealed under the fs keys but signed by another member's device for D's fp → `invalid`.
* **`tls`:** a server with `[tls]` and a test CA (`zen_tls_rustcrypto::testing::server_cert`); `TlsOptions {ca}` signs in over HTTPS; without the CA, it's refused (unknown issuer).
* **Negative controls** (each run once, then restored, and logged): the own-head check off (`checkpoints_withheld`'s A side), `check_seen` off (its B side), `observe`'s `lower_than` off (`vol_rollback`), the header pin check off, the same-seq HC check off (`fork`).

### Interop with the TS client (`packages/client/test/rustclient.test.ts`, new)

* It runs the built `target/debug/zen-client` against a spawned server (adversary on). If the binary is missing, the test fails and says how to build it (no skips).
* **Setup:** a TS admin claims the server. The ACL has a TS device and a Rust device (a device key file written by the test). The TS admin `init`s the fs with signed headers and a slot for each device.
* **Cases:**
  * Rust `kv set a/b v1` → TS `kv.get(['a', 'b'])` is `v1`; TS writes `c`, and Rust `kv get c` prints it.
  * Rust `checkpoint` → TS `integrity.sync()` verifies the Rust device's checkpoint, with no alarm. TS checkpoints → Rust `sync` exits 0. Each `seen`s the other.
  * `pins compare`: TS `exportPins()` → Rust exits 0; Rust `pins export` → TS `comparePins` returns `[]`.
  * **Fork:** after `server.fork()`, the TS device checkpoints on the copy. Rust `pins compare` of that export exits 3 with a fork.
  * The pins vector: TS's encoder gives the `integrity.json` bytes (`integrityvectors.test.ts`, through exported `encodePins`/`decodePins`).
* **CI** (`client` job): `cargo build -p zen-server --features test-utils -p zen-client`.

### Docs

* **docs/CLIENT.md §13 (new), "The Rust client":**
  * what it covers and what stays TS-only
  * the device key file
  * the pins file (keep it with the device key; losing it reopens the rollback window back to the first sync)
  * the CLI and its exit codes
  * comparing pins out of band between a phone (TS) and a server (Rust)
* **README:** the crate in the layout list.
* **docs/STATS.md:** native checkpoint cost (sign, seal, verify; vs WASM's ~2.6 ms sign), measured with an opt-in `#[ignore]` bench test.
* **docs/MILESTONE-6.md:** the log.

### Files

* **Spec:** `spec/formats.md`, `spec/test-vectors/integrity.json`
* **Rust:**
  * `crates/zen-core/src/{integrity,vectors}.rs`, `crates/zen-core/tests/vectors.rs`
  * `crates/zen-client/{Cargo.toml, src/{lib,error,transport,client,device,session,acl,fs,kv,lease,integrity,pins,main}.rs, tests/{common/mod.rs, sign_in.rs, acl.rs, header.rs, kv.rs, lease.rs, checkpoints.rs, attacks.rs, tls.rs}}`
  * root `Cargo.toml` (member)
* **TS:** `packages/client/src/integrity.ts` (export the pins codec internally), `packages/client/test/{rustclient.test.ts (new), integrityvectors.test.ts}`
* **CI:** `.github/workflows/ci.yml` (the client job's build line)
* **Docs:** `docs/{CLIENT,STATS,MILESTONE-6}.md`, `README.md`

### Verification

* **Rust:**
  * `cargo fmt --check`
  * clippy `--all-targets -D warnings`: default, `--no-default-features`, `--all-features`
  * `cargo test -p zen-client` 3×, `cargo test -p zen-core --features test-utils`
  * regenerate the vectors (only `integrity.json`'s `pins` changes)
  * `cargo check -p zen-client --no-default-features` (pure Rust)
* **JS:** `npm run check`; `rustclient.test.ts` and `integrityvectors.test.ts` 3×; the client `integrity` tests once.
* **Attacks:** the negative controls above.
* **CI:** green on both runs.

**Defaults chosen (overridable):**
* **Scope:** device sign-in and tokens only (no passkey, OPAQUE or mTLS). No topics, streams, trees, svc or zen-db. `zen-blk` needs KV, leases and checkpoints.
* **One crate** with the library and a small CLI. The CLI is also the out-of-band pin tool the milestone promised.
* **Pins in a plain file** next to the device key, not sealed: whoever can change it can also read the device key.
* **Re-sign-in on 401** with the device key, for long-running daemons.
* **Embedded backend only** in zen-client's tests: the backends are tested by zen-server's.
* **The exported pins record goes into the spec** (formats.md §14.5): it is the cross-client format.

## Step 6.7 in detail

Step 6.7 builds `crates/zen-blk`, the block engine: a fixed-size volume of 4,096-byte blocks inside an fs, on `zen-client`. Blocks are sealed KV values. A Merkle tree over the blocks and a hash-chained root record make every read verifiable, and the root's `seq` rides in device checkpoints (entry type 4). One writer at a time holds a fenced lease; read-only openers need none. The frontends (FUSE file export, ublk) come in 6.8 and only call this engine.

**Survey (what this step builds on):**
* **Stored keys** (`zen_core::token`): a path `(e1, …, en)` becomes `t1 ‖ … ‖ tn`, 16 bytes per element, each `PRF16` of the chain so far. So the token of `("blk", name)` is a 32-byte prefix of every key of the volume, the server sees neither the name nor the block index, and nearby blocks don't sort together (DESIGN.md §2.7's "shuffled index" comes free). A cached `NameChain` for `("blk", name, "b")` costs one `child()` per block key.
* **Sealing:** `seal::seal_value(keys, stored_key, value)` (kind 1, a random 24-byte nonce, the stored key in the AAD). So a block can't be moved to another index, volume or fs; it can only be replaced by an older value of the same key.
* **zen-client** (6.6): `UnlockedFs::{key, get_many, transaction}`, `Transaction::{set, delete, fence_lease, commit}` (`send_commit` replays one `commit_id`), `Lease::{acquire, renew, release}` (`not_leader`, `claim_lost`), `Integrity::{note, observe_at, floor, raise, checkpoint, sync}`, `AlarmObject::Vol`, `Error::Local{code, …}`.
* **The checkpoint entry** `Entry::Vol {vol, seq, hash}` exists (formats.md §14.1 type 4), with `vol` "the volume's stored-key prefix"; `merge_entry`, `lower_than` and `divergent` already handle it. "Volume roots are defined with volumes" is still open in the spec.
* **Server limits** (api.md §2): `max_value_bytes` 90,000; `max_commit_bytes` 8,000,000, counting 512 bytes per operation; `max_commit_ops` 10,000; a clear over more than `max_range_items` keys is refused.
* **Tests:** zen-client's `tests/common/mod.rs` (`Server::start_with` with the adversary, `fork()`, `claim`, `lie(Lie::Kv …)`, `truth`), which zen-blk's tests include with `#[path]` rather than copy. Each test binary links the whole server (about 240 MB), so one test file.
* **Labels:** nothing for blocks is registered yet.

### Spec (`spec:` commit)

**spec/blk.md (new): block volumes.**
* **A volume** is named by bytes `name` (1–64) inside an fs. Its keys are KV stored keys (formats.md §3.1):
  ```
  root record   ("blk", name, "r")
  block i       ("blk", name, "b", u64be(i))
  tree node     ("blk", name, "t", u8(level) ‖ u64be(j))
  lease name    the stored key of ("blk", name, "lease")      (48 bytes; api.md §16)
  vol id        the stored key of ("blk", name)               (32 bytes; the checkpoint entry's id)
  ```
  Every value is sealed as kind 1 (formats.md §4). The block size is 4,096; a volume has `blocks` blocks, fixed at creation (resizing is TD-BLK-SNAPSHOTS).
* **Leaves.** `leaf(i) = H("zen/v1/blk-leaf", u64(i) ‖ sealed)` over the stored bytes of block `i`, or 32 zero bytes when the block is absent. An absent block reads as zeros. The leaf covers the whole sealed value, so any older value of the block is another leaf; no per-block counter is needed (DESIGN.md §2.5's `write_counter`).
* **The tree.** Fan-out 128. Depth `D` = the least `d ≥ 1` with `128^d ≥ blocks` (a 1 TiB volume has `D = 4`). The node at level `l` (1..D), index `j`, holds the 128 child hashes of level `l − 1` (leaves at level 0), 4,096 bytes:
  ```
  node(l, j)    = child(l−1, 128j) ‖ … ‖ child(l−1, 128j + 127)        (missing children are zeros)
  hash(l, j)    = H("zen/v1/blk-node", u8(l) ‖ u64(j) ‖ node(l, j)), or zeros when every child is zero
  root          = hash(D, 0)
  ```
  A node whose children are all zero is not stored. So an empty volume stores only its root record, and storage follows what was written.
* **The root record**, deterministic CBOR (zendb.md §1):
  ```
  Root = { 1: v = 1, 2: seq: u64 (1, 2, …), 3: prev: bytes(32),     // RH(previous record), zeros for seq 1
           4: blocks: u64, 5: block_size: u32 = 4096, 6: root: bytes(32),
           7: writer: bytes(32),                                     // the device fp that wrote it
           8: hlc: u64 }
  RH   = H("zen/v1/blk-root", CBOR(Root))
  ```
  Creating a volume writes seq 1 (root zeros) with an `expect` that the record is absent. A decoder refuses another `v` or `block_size`, `blocks` 0, and trailing bytes.
* **Writing.** One writer at a time, holding the lease (api.md §16). Every commit:
  * is fenced with the lease's token (a writer that lost its lease writes nothing, 412 `claim_lost`);
  * carries the blocks it writes (or deletes, for a discard), every node on their paths whose hash changed (deleting nodes that became empty), and the next root record (`seq + 1`, `prev` = RH of the current one);
  * so each commit moves the volume from one verified state to the next, atomically.
  
  A flush returns once its commits applied. Writes not yet flushed may be lost in a crash, and any subset of them may have applied, as with a disk's write cache. After a commit applies, the writer's next checkpoint carries `[4, vol id, seq, RH]`, counted toward the cadence (formats.md §14.3).
* **Reading.** A reader holds a root record it verified. A block is accepted only when its leaf matches the path of nodes from the root, each node checked against its parent's slot. A mismatch:
  * The reader re-reads the root record. If it moved, the volume changed under the reader (`volume_changed`; a writer never sees this, since it wrote every root).
  * If not, it's a **rollback** (`object: "vol"`).
* **Freshness** (formats.md §14.4): the root record read at open is checked against the floor of `[4, vol id]`: a lower `seq` is a rollback, the same `seq` with another RH a fork, and at the floor's `seq + 1`, `prev` must be the floor's RH (a fork otherwise). A writer also keeps its own newest root in a local file, so a rollback of its own writes is caught on the next open even before a checkpoint carries them (DESIGN.md §2.6, "single writer").
* **Leakage:** the number of stored blocks and nodes, which blocks and nodes each commit touches, when, and how often the writer flushes; the server doesn't learn indexes or contents. A discard shows as deletes. The fixed block size hides write lengths.
* **Not yet:** snapshots, clones, resize and delete (TD-BLK-SNAPSHOTS).

**Elsewhere in the spec:**
* **formats.md §14.1:** the type-4 sentence names blk.md and the 32-byte vol id; "defined with volumes" goes.
* **labels.md and `labels.rs`:** `zen/v1/blk-leaf`, `zen/v1/blk-node`, `zen/v1/blk-root`.
* **spec/README.md:** the blk.md row.
* **TECH_DEBT.md:** TD-BLK-SNAPSHOTS (snapshots and clones, as DESIGN.md §5; resize; delete, which needs paged clears and a tombstone record so a deleted volume can't be resurrected; read-only openers that pin a snapshot instead of failing with `volume_changed`).

### zen-core (new `crates/zen-core/src/blk.rs`)

* `BLOCK_SIZE = 4096`, `FANOUT = 128`, `depth(blocks)`.
* `leaf(idx, sealed) -> [u8; 32]`, `node_hash(level, j, &[[u8; 32]; 128]) -> [u8; 32]` (zeros for an empty node).
* `RootRecord {seq, prev, blocks, root, writer, hlc}` with `encode`, a strict `decode`, and `hash()` (RH).
* `tree(blocks, leaves: &BTreeMap<u64, [u8; 32]>) -> Tree {root, nodes: BTreeMap<(u8, u64), [[u8; 32]; 128]>}`: the reference, for tests and vectors.
* **Vectors** (`spec/test-vectors/blk.json`, new, from `vectors.rs`): the fixture fs, volume `disk` with 20,000 blocks (depth 3); the stored keys of its root, the lease, blocks 0, 1 and 16,500 and their nodes; the three blocks' plaintexts, sealed bytes (`DetRng`) and leaves; every stored node's non-zero slots and hash; root records seq 1 (empty) and seq 2 (the three blocks): CBOR and RH. Regenerating changes only that file. `tests/vectors.rs` reads it back; unit tests cover the decode refusals and `tree` against hand-built cases (one block, a full node, the last block of a non-power-of-128 volume).

### `crates/zen-blk` (new library)

**Dependencies:** zen-client, zen-core, zen-proto, tokio, getrandom. Dev: zen-server (`test-utils`), tempfile, zen-core (`test-utils`).

**API:**
```rust
pub struct VolumeOptions {
    pub lease_ttl: Duration,            // 30 s; renewed every ttl / 3
    pub batch_blocks: usize,            // 256 (1 MiB): a commit's block budget, and the dirty limit
    pub idle_flush: Option<Duration>,   // Some(5 s): flush this long after the last write
    pub cache_nodes: usize,             // 4,096 level-1 nodes (16 MiB); levels ≥ 2 are always kept
    pub pin: Option<PathBuf>,           // the local root pin; None: in memory only (tests)
}
impl Volume {
    pub async fn create(fs: &UnlockedFs, name: &[u8], size: u64, o: VolumeOptions) -> Result<Volume>;  // the writer
    pub async fn open(fs: &UnlockedFs, name: &[u8], o: VolumeOptions) -> Result<Volume>;               // the writer
    pub async fn open_read_only(fs: &UnlockedFs, name: &[u8], o: VolumeOptions) -> Result<Volume>;
    pub fn size(&self) -> u64;
    pub fn root(&self) -> (u64, [u8; 32]);                       // seq, RH
    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    pub async fn write(&self, offset: u64, data: &[u8]) -> Result<()>;   // buffered
    pub async fn discard(&self, offset: u64, len: u64) -> Result<()>;    // whole blocks; edges are zero-written
    pub async fn flush(&self) -> Result<()>;
    pub async fn close(self) -> Result<()>;                      // flush, checkpoint, release the lease
}
```
`Volume` is `Clone` (an `Arc`), so a frontend can serve requests concurrently. Errors are `Error::Local` with codes `exists`, `not_found`, `busy` (another writer holds the lease), `lease_lost`, `read_only`, `range` (past the end), `volume_changed`, `format`; integrity failures are alarms through `Integrity::raise` (`object: Vol`). An attached `Integrity` is required (`integrity_needed` otherwise), with a signer for a writer: without checkpoints, other devices get consistency but not freshness.

**Modules:**
* **`keys.rs`:** the cached name chains and the stored keys of the spec's table.
* **`tree.rs`:** the verified tree.
  * The root hash; level-≥2 nodes kept; level-1 nodes in an LRU of clean nodes.
  * `path(idx)`: which nodes a block needs and which are missing; the missing ones are fetched together with the blocks in one `get_many`, and checked top-down against the parents.
  * `apply(changes) -> (new nodes, deleted nodes, root)`: copy-on-write of the touched nodes, so a failed commit leaves the tree as it was.
* **`pin.rs`:** the local pin file, CBOR `{1: v = 1, 2: vol id, 3: seq, 4: RH}`, written atomically (temporary file, fsync, rename, directory fsync, mode 0600) after each commit; a file of another volume, or one that doesn't decode, is refused (`format`), as zen-client's pins file.
* **`volume.rs`:**
  * **Open:** `integrity.sync()`; take the floor; read the root record; check it against the floor (`observe_at`, and the `prev` rule at floor + 1) and against the local pin (lower: rollback; same seq, other RH: fork; pin + 1 with another `prev`: fork). A writer first acquires the lease (`not_leader` → `busy`) and starts the renewal task; a renewal that fails marks the volume `lease_lost`.
  * **Writes** go to a dirty map (block → 4,096 bytes); an unaligned write reads the edge blocks first. Reaching `batch_blocks` dirty blocks flushes from inside `write`.
  * **Flush** (serialized): takes the dirty map (new writes start a new one; reads see both), splits it into commits of at most `batch_blocks` blocks and within `max_commit_bytes` and `max_commit_ops`, and for each: fetches and verifies the missing level-1 paths, seals the blocks, `tree.apply`, then one transaction (blind writes, `fence_lease`, no reads). On success: the new root, the pin file, `integrity.note(Vol, count)`. On `claim_lost`: `lease_lost` from then on. On an unknown outcome after `send_commit`'s replays: re-read the root record; ours means applied, the old one means resend, anything else is an error.
  * **Reads:** the dirty and flushing maps first; then one `get_many` for the blocks and missing nodes; each block's leaf is checked against its verified path and opened. A mismatch takes the spec's path (`volume_changed` for a reader whose root moved, else a rollback).
  * **Idle flush:** a task armed by each write, `idle_flush` after the last one (the generation counter of zen-client's cadence).
  * **Close:** flush, `integrity.checkpoint()`, release the lease.

### Tests (`crates/zen-blk/tests/volume.rs`, the server in-process, the adversary on)

* **`create_open_read_write`:** a 64 MiB volume reads as zeros; aligned and unaligned writes read back before and after `flush`; another session's writer (after `close`) reads the same; each flush advances `seq`; `create` again gives `exists`; `open` of an unknown name gives `not_found`; reading past the end gives `range`.
* **`model`** (property): 2,000 random operations (writes of 1–40 blocks, aligned or not, discards, reads, flushes, a close and reopen every few hundred) over a 100 MiB volume (depth 3, `batch_blocks` 16), seeded `DetRng`, against an in-memory model. After each flush, the stored root equals `zen_core::blk::tree` over the leaves of every stored block (read raw with `range_prefix`).
* **`batching`:** a write of 3 × `batch_blocks` with no flush commits on its own (`seq` advances); a flush of 1,000 dirty blocks with `batch_blocks` 256 takes 4 commits.
* **`idle_flush`:** with `idle_flush` 200 ms, a write is committed without a flush.
* **`crash`** (`lease_ttl` 500 ms): write A, flush, write B, then drop the volume without closing (renewals stop). A reopen after the lease lapses reads A and not B. Then the pin file left one or two commits behind (a crash after the commit, before the pin write: a copy of the older file restored) reopens without an alarm.
* **`second_writer`:** while A holds the volume, B's `open` gives `busy`; B's `open_read_only` reads A's flushed data; after A's `close`, B opens as writer (token 2).
* **`stale_fence`:** A's renewals paused (a test hook) until the lease lapses; B opens and writes; A writes and flushes: `lease_lost`, nothing of A's is stored, B's data is intact, and A's later writes fail too.
* **`volume_changed`:** a reader opened at seq k; the writer commits a change to block 5; the reader's read of block 5 gives `volume_changed` (no alarm), and a reopen reads the new data.
* **`rollback_root`:** lie `kv` on the root record at seq k; the writer commits twice and checkpoints.
  * The writer reopened (a new `Volume`, the same pin file, a fresh `Integrity` without pins) gives a rollback from the pin alone.
  * A reader on another device that synced gives a rollback; one that didn't sync accepts seq k: the window, asserted.
* **`rollback_block_and_node`:** lie `kv` on one block (its older value), then on one level-1 node: a reader's read gives a rollback each time, and so does the writer's own read of that block after a reopen (its cache dropped).
* **`fork`** (`Server::fork()`): the writer commits on S1 and checkpoints; on the copy, the same device (a fresh process, no pin file) writes other data at the same `seq`. A reader that pinned S1's root and checkpoint, moved to S2, gives a fork (same `seq`, another RH).
* **`checkpoint_entry`:** the writer's checkpoint carries `[4, vol id, seq, RH]`; another device's floor gets it after `sync`.
* **`bench`** (`#[ignore]`, release): sequential write of 64 MiB with flushes every 4 MiB, random 4 KiB reads cold and warm, a flush of one block (the latency ext4 sees for `fsync`).
* **Negative controls** (each run once, then restored, and logged): the local pin check off (`rollback_root`, writer side), `observe_at` off (reader side), the leaf check off and the node check off (`rollback_block_and_node`), the fence off (`stale_fence`), the same-`seq` RH check off (`fork`).

### Docs
* **README:** the crate row.
* **docs/STATS.md §5.7:** the bench numbers (docs/BLK.md and STATS §6 stay in 6.9).
* **docs/MILESTONE-6.md:** the log.

### Files
* **Spec:** `spec/{blk (new),formats,labels,README,TECH_DEBT}.md`, `spec/test-vectors/blk.json` (new)
* **zen-core:** `crates/zen-core/src/{blk (new),lib,labels,vectors}.rs`, `crates/zen-core/tests/vectors.rs`
* **zen-blk:** `crates/zen-blk/{Cargo.toml, src/{lib,keys,tree,pin,volume}.rs, tests/volume.rs}`; root `Cargo.toml` (member)
* **zen-client:** only if the engine needs a small addition (none expected); `tests/common/mod.rs` stays where it is
* **Docs:** `README.md`, `docs/{STATS,MILESTONE-6}.md`

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings` on zen-blk and zen-core (default, `--no-default-features`, `--all-features`)
* `cargo test -p zen-core --features test-utils`; regenerate the vectors (only `blk.json` is new, nothing else changes)
* `cargo test -p zen-blk` 3×; `cargo test -p zen-client` once (the shared harness)
* the negative controls above
* CI green on both runs (`rust` and `pure-rust` run the new tests)

**Defaults chosen (overridable):**
* **One root record** holds the size and the root, rather than a separate `meta` record: one read at open, and a future resize is just another link of the chain.
* **The leaf covers the sealed bytes,** with no per-block write counter: a counter adds nothing once the leaf binds the whole value.
* **Tree nodes are sealed** like every other value (cost ~17 µs each), so the engine needs only the KV API.
* **Writes are blind** (no read set): the fence is what excludes other writers.
* **An attached `Integrity` is required,** as for authenticated zen-db namespaces; the local pin file is per volume, next to the pins file.
* **Read-only openers fail with `volume_changed`** when a writer moves the root under them, rather than pinning a snapshot (TD).

## Step 6.8 in detail

Step 6.8 builds `crates/zen-ublk`: a daemon that serves a zen-blk volume as a Linux block device, so `mkfs.ext4` on it gives a disk whose contents the server can't read, forge or roll back unnoticed. It has two frontends over one adapter:
* **FUSE:** one file, `DIR/disk.img`, for `losetup`.
* **ublk:** `/dev/ublkbN`. It's behind a cargo feature, and tested in a new CI job.

**Survey (what this step builds on):**
* **The engine** (`zen-blk` `Volume`):
  * It's `Clone` (an `Arc`), and its `read`, `write`, `discard` and `flush` take `&self`, so requests can run concurrently.
  * A failed flush puts its blocks back under newer writes, so the next flush retries them.
  * After a lost lease, every write and flush gives `lease_lost`.
  * `close(self)` flushes, checkpoints and releases the lease.
* **Two engine gaps the frontends expose:**
  * **Partial writes race.** An unaligned `write` reads its edge blocks, then buffers them. Two concurrent partial writes into one block can each read the old block, so one write is lost. Block devices may issue such writes (512-byte sectors, `dd`, the FUSE file written directly).
  * **Discarding absent blocks costs commits.** `discard` buffers a delete for every block, and each one is committed even when the block was never written. `mkfs.ext4` discards the whole device by default: a 1 GiB volume would take 1,024 commits that change nothing.
* **The zen-client CLI** (`crates/zen-client/src/main.rs`) has the connect sequence in `run`, not in the library:
  1. sign in with the device key
  2. attach integrity with the pins file (default: the key's path + `.pins-<fs>`)
  3. unlock with the device keyslot
  4. sync
  
  Its exit codes are 3 for an alarm (as JSON on stderr) and 1 for other errors.
* **Test harness:** zen-client's `tests/common/mod.rs` provides `Server::start_with` (an HTTP URL on 127.0.0.1), `claim`, `fork`, `lie` and keys. zen-blk includes it with `#[path]`. Its tests set up fs 1 with `init_for(&devices, false)`.
* **This container:** kernel 6.18, root, `/dev/fuse`, loop devices, `fusermount3`, `losetup`, `mkfs.ext4`, `e2fsck`, libclang 18. There's no ublk and no `/lib/modules`.
* **GitHub runners:** non-root with `sudo`, clang and libclang installed. ublk may need `linux-modules-extra-$(uname -r)` before `modprobe ublk_drv`.
* **Crates:**
  * **fuser 0.18.0** (2026-07-22, MSRV 1.85). Without its `libfuse` feature it's pure Rust: it mounts through `fusermount3`. Its `Filesystem` methods take `&self`, a `Config` sets `n_threads`, and replies can be sent from another thread.
  * **libublk 0.4.8** (2026-08-29, smol and io_uring per queue). Its `libublk-rs-sys` runs bindgen at build time, which needs libclang and kernel headers. So it can't be in the default build: the `pure-rust` job and builds without clang would break.
* **CI:** `rust` and `pure-rust` run `cargo test --all` as a non-root user; root-only device tests can't run there. `fdb` runs clippy `--all-features`, which will build libublk (libclang is on the runner).

### zen-blk (two small engine changes; tests in `tests/volume.rs`)

* **Writes are ordered.**
  * `write` and `discard` take an async write lock from the edge reads until their blocks are buffered.
  * A full batch's flush runs after the lock is released, so a slow commit doesn't hold up other writes' buffering longer than it must.
  * Reads stay concurrent.
  * New test `concurrent_partial_writes`: 16 tasks write disjoint 256-byte pieces of one block at once; every piece reads back, before and after a flush. Without the lock, it fails: run once, logged.
* **Deletes of absent blocks are dropped.**
  * In `commit`, after the paths are resolved, a delete of a block whose verified leaf is already zero is left out.
  * A chunk left with no changes commits nothing: no new root, no seq.
  * New test `discard_absent_is_free`: discarding all of an empty 1 GiB volume leaves `seq` unchanged. After writing 3 blocks, discarding everything takes one commit, which deletes exactly those 3, and the stored tree matches the reference.
  * The model test still passes.

### zen-client: shared connect arguments

* **New `cli.rs`** in the library:
  * `#[derive(clap::Args)] pub struct ConnectArgs {url, device_key, fs, pins, origin, ca}`, the six options `zen-client` has today, with the same defaults.
  * `ConnectArgs::connect(IntegrityOptions) -> Connected {key, session, integrity, fs: UnlockedFs}`: sign in, attach, unlock, sync.
  * `pub fn exit_code(&Error) -> u8` (3 for an alarm, else 1), and `print_error`.
* `zen-client`'s `main.rs` uses them, with `#[command(flatten)]`. Its behavior and tests are unchanged.

### `crates/zen-ublk` (new: library and the `zen-ublk` binary)

**Features:**
* default: the FUSE frontend, pure Rust (fuser without `libfuse`)
* `ublk`: the ublk frontend (libublk)
* `device-tests`: compiles `tests/devices.rs`, the suite that needs root and the kernel's devices (as `fdb` gates FDB tests)

**Dependencies:** zen-blk, zen-client, zen-core, tokio (`signal`), clap, fuser 0.18 (default features off), libc. Optional: libublk 0.4.8, smol (`ublk`).

**The CLI:**
```
zen-ublk --url URL --device-key FILE --fs N [--pins FILE] [--origin O] [--ca FILE]     (ConnectArgs)
         --vol NAME [--vol-pin FILE] [--read-only] [--lease-ttl-ms MS] [--batch-blocks N]
         [--idle-flush-ms MS] [--io-timeout-ms MS] COMMAND
  create SIZE        create the volume (SIZE: bytes, or with K/M/G/T; a multiple of 4,096), print its root, exit
  info               size, seq, RH, whether a writer holds it
  fuse DIR           serve DIR/disk.img until unmounted or signalled
  ublk [--id N] [--queues Q] [--depth D]      serve /dev/ublkbN until signalled  (feature "ublk")
```
* **The volume pin** defaults to the device key's path + `.vol-<fs>-<hex(name)>`. It sits next to the pins file, which it shares with `zen-client`; the 409 merge covers two processes of one device.
* **Readiness:** once serving, the daemon prints `ready <path>` on stdout (the file or the device). Scripts and the tests wait for that line.
* **Stopping:**
  * SIGINT or SIGTERM: the FUSE session unmounts, or the ublk device is stopped and deleted.
  * An external `umount DIR` ends the FUSE session the same way.
  * Then `Volume::close` (flush, checkpoint, release the lease) and `integrity.close()`.
* **Exit codes:** 0 for a clean stop; 3 for an alarm, as JSON on stderr; 1 for other errors, including `busy` (another writer holds the volume) and a lost lease.

**`disk.rs`, the adapter both frontends call** (`Disk`, `Clone`):
* **Operations:**
  * `read(off, len)` returns the bytes.
  * `write(off, data, fua)`: a write with FUA is followed by a flush.
  * `flush()`.
  * `discard(off, len)`, also used for write-zeroes: discarded blocks read as zeros.
* **Errors become an errno:**
  * `range` → `EINVAL`
  * `read_only` → `EROFS`
  * `volume_changed` (a read-only export whose writer moved the root) → `EIO`, with a message to restart the export
  * the rest → `EIO`
* **Fail closed:** after an alarm or a lost lease, every request gets `EIO`. The first failure is printed once, and its exit code is kept for the end. The device stays until it's stopped: removing it under a mounted fs is worse.
* **Transient errors retry:** network errors and 5xx answers are retried with backoff for up to `--io-timeout-ms` (default 60 s) before `EIO`. A network blip shouldn't make ext4 abort its journal, and a failed flush keeps its blocks.

**`fuse.rs`, the FUSE frontend** (fuser 0.18, `n_threads` 4):
* **The tree:** the root directory (ino 1) holds `disk.img` (ino 2), with size = the volume's, mode 0600 (0400 read-only), and the daemon's uid and gid. Mount options: `fsname=zen-ublk`, `default_permissions`. As root, `allow_other` too, so loop devices and other users can open it.
* **Requests:**
  * `lookup`, `getattr`, `readdir`, `open`, `statfs`
  * `read`: a short read at the end, 0 past it
  * `write`
  * `fsync` and `flush` → `Disk::flush`. `fsync` is never `ENOSYS`: the kernel would take that as "no fsync needed".
  * `fallocate`: `PUNCH_HOLE|KEEP_SIZE` and `ZERO_RANGE` → discard, which is how loop passes DISCARD and WRITE_ZEROES on; mode 0 inside the size → ok; anything else → `EOPNOTSUPP`
  * `setattr`: a size change → `EPERM`, while times and mode are accepted and ignored
  * the rest → `ENOSYS`
* **Each request runs as a task** on the tokio runtime, which replies from the task. Writes are ordered by the engine. The kernel sends `fsync` only after the writes it covers were answered, so a flush includes them.
* **No writeback cache:** FUSE writes are write-through, so a write is in the volume's buffer when the kernel sees it done.
* **`losetup`:** docs and tests use `-b 4096`, so loop's I/O is block-aligned.

**`ublk.rs`, the ublk frontend** (feature `ublk`):
* **The control device:** `UblkCtrlBuilder`, with name `zen-ublk`, `--id` (default: the kernel picks), `--queues` (default 2), `--depth` (default 64), and 1 MiB I/O buffers.
* **Parameters:**
  * logical and physical block size 4,096; `io_min` and `io_opt` 4,096
  * `dev_sectors` = size / 512; `max_sectors` 1 MiB
  * discard: granularity 4,096, `max_discard_sectors` and `max_write_zeroes_sectors` 1 GiB, one segment
  * attributes: `UBLK_ATTR_VOLATILE_CACHE` and `UBLK_ATTR_FUA`, plus `UBLK_ATTR_READ_ONLY` when read-only. Without `VOLATILE_CACHE`, the kernel would send no flushes and ext4 would treat buffered writes as durable.
* **I/O:** each queue thread runs libublk's smol executor, with one task per tag.
  * READ: `Disk::read` into the tag's buffer
  * WRITE: copy out of the buffer, then `Disk::write`, with FUA from the request's flags
  * FLUSH: `Disk::flush`
  * DISCARD and WRITE_ZEROES: `Disk::discard`
  * The result is the byte count, or `-errno`.
  * The `Disk` futures run on the tokio runtime (`handle.spawn`). The queue awaits the `JoinHandle`, which works from any executor.
* **A killed daemon's device:** after a SIGKILL, the device stays, dead, until deleted. With `--id N`, a device N whose daemon pid is gone is deleted before N is added again, so a restart, or systemd, can reuse the id. A live one gives `busy`.

### Tests

**`tests/disk.rs`, the default suite** (no root; the in-process server; runs in `rust` and `pure-rust`):
* **The adapter:** read, write, discard and flush round trips, and unaligned edges.
* **Errnos:** `EINVAL` past the end; `EROFS` on a read-only volume; FUA (the root's seq moves before the write returns).
* **Fail closed:** after a stale fence (`pause_renewals`, another writer takes over), every request is `EIO` and the exit code is 1. After a rolled-back block (the adversary), it's `EIO` and exit code 3.
* **Retries:** with the server stopped mid-flush and started again within `io-timeout`, the flush succeeds. With a 200 ms timeout, it gives `EIO`, and the blocks stay buffered: the next flush, after the server is back, commits them.
* **The CLI:** sizes (`64M`, `1G`, `4096`, and refused: `100`, `0`, `1X`), and the default pin paths.

**`tests/devices.rs`** (feature `device-tests`; root, FUSE and loop; the server in-process, `zen-ublk` run as a child process from `CARGO_BIN_EXE_zen-ublk`):
* **Cleanup:** a guard kills the child and lazily unmounts the ext4 mount and the FUSE directory, and detaches the loop device, even when a test panics.
* **The test volumes:** 64 MiB (a debug build seals slowly); the files are a few MiB of seeded random content in nested directories.
* **`fuse_file`:**
  * `create 64M`, then `fuse DIR`; `disk.img`'s size and mode are correct
  * `pwrite`s aligned and unaligned, `fsync`, read back
  * a truncate gives `EPERM`
  * SIGTERM → exit 0, and a second run reads the same
  * while one serves, a second `fuse` of the volume exits 1 with `busy`
  * `--read-only`: opening for writing gives `EROFS`
* **`fuse_ext4_round_trip`:**
  1. `fuse`, then `losetup -b 4096 -f --show`, `mkfs.ext4 -q` (with its discard), and `mount`
  2. write the files, `sync`, `umount`, `losetup -d`, SIGTERM (exit 0)
  3. a new run: `e2fsck -fn` exits 0, then mount and compare every file
  4. a `--read-only` run on a read-only loop, mounted `-o ro,noload`: compare again
* **`fuse_writer_killed`** (`--lease-ttl-ms 2000`):
  1. set A, `sync`; then set B without a sync; SIGKILL the daemon
  2. cleanup: `umount -l`, `losetup -d`, `fusermount3 -uz`
  3. a restart before the lease lapses exits with `busy`; after it lapses, it serves
  4. `e2fsck -fy` exits 0 or 1 (it replays the journal), then `e2fsck -fn` exits 0
  5. mount: set A is intact byte for byte. Set B may be partly there: no assertion
* **`ublk_ext4_round_trip` and `ublk_writer_killed`** (also `ublk`): the same two scenarios over `/dev/ublkbN`, with mkfs directly on the device, and the restart after the kill using `--id N` (the stale device is deleted).
  * Without `/dev/ublk-control`, each prints "skipped: no /dev/ublk-control (modprobe ublk_drv)" and returns. That's the kernel lacking the device. With `ZEN_TEST_REQUIRE_UBLK=1`, as in CI, it fails instead.
* **Here:** the FUSE tests run (root, FUSE and loop are available). The ublk tests compile and clippy here, but run only in CI.

**CI: a new job `devices`:**
```yaml
  devices:
    runs-on: ubuntu-latest
    steps:
      - checkout; dtolnay/rust-toolchain@stable (clippy); Swatinem/rust-cache@v2
      - name: FUSE, e2fsprogs, the ublk driver
        run: |
          sudo apt-get update -q
          sudo apt-get install -y -q fuse3 e2fsprogs
          sudo modprobe ublk_drv || { sudo apt-get install -y -q "linux-modules-extra-$(uname -r)" && sudo modprobe ublk_drv; }
          ls -l /dev/ublk-control /dev/fuse
      - name: Clippy
        run: cargo clippy -p zen-ublk --all-targets --features device-tests,ublk -- -D warnings
      - name: Test (as root)
        env:
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER: sudo -E
          ZEN_TEST_REQUIRE_UBLK: "1"
        run: cargo test -p zen-ublk --features device-tests,ublk
```
If `modprobe` fails on hosted runners even with the extra modules, the job keeps the FUSE tests. The ublk ones become `workflow_dispatch`-only, and that's recorded in TECH_DEBT (the risk named in this plan). The log says which happened.

### Docs
* **docs/BLK.md (new; 6.9 adds the design, threats and numbers):**
  * running `zen-ublk`: create, the FUSE export with `losetup -b 4096`, ublk, read-only exports
  * flags, stopping, exit codes
  * durability: a write cache that flush empties; a crash loses only unflushed writes; one writer, with `busy`
  * alarms and fail-closed `EIO`
* **README:** the crate row.
* **docs/CLIENT.md §13:** `zen-ublk` shares the connect options and pins files.
* **docs/MILESTONE-6.md:** the log.

### Files
* **zen-blk:** `crates/zen-blk/src/volume.rs`, `tests/volume.rs`
* **zen-client:** `crates/zen-client/src/{cli (new),lib,main}.rs`
* **zen-ublk:** `crates/zen-ublk/{Cargo.toml, src/{lib,main,disk,fuse,ublk}.rs, tests/{disk,devices}.rs}`; root `Cargo.toml` (member), `Cargo.lock`
* **CI:** `.github/workflows/ci.yml` (the `devices` job)
* **Docs:** `docs/{BLK (new),CLIENT,MILESTONE-6}.md`, `README.md`

### Verification
* `cargo fmt --check`
* clippy `--all-targets -D warnings` on the workspace: default, `--no-default-features`, `--all-features` (the last builds libublk here)
* `cargo test -p zen-blk` 3×, `cargo test -p zen-client`, `cargo test -p zen-ublk` 3×
* `cargo test -p zen-ublk --features device-tests` 3× here, as root; the ublk tests report their skip here
* `cargo check -p zen-ublk --no-default-features` builds without C (`CC=false`)
* the negative control: the write lock off makes `concurrent_partial_writes` fail
* CI green on both runs, including `devices` with ublk running

**Defaults chosen (overridable):**
* **One crate, with ublk behind a feature:** libublk's bindgen needs libclang, which the default and pure-Rust builds must not need.
* **Root-only device tests are a feature-gated suite** in their own CI job, as FDB's are, not `#[ignore]`d. The only runtime skip is a missing ublk driver, and CI turns that into a failure.
* **fuser without libfuse:** pure Rust, mounting through `fusermount3`.
* **A failed lease or an alarm fails closed with `EIO`,** and the device stays until it's stopped.
* **Transient errors retry for 60 s** before `EIO`.
* **`zen-ublk` shares zen-client's pins file** by default, and keeps a volume pin next to it.

## Step 6.9 in detail

Step 6.9 closes the milestone. Everything is built, but the docs are spread across CLIENT.md §12–13, DB.md §7.1, SVC.md §6, BLK.md and the specs, and nothing says what integrity covers as a whole. This step adds:
* one guide to integrity
* the rest of BLK.md
* the measurements that are still missing: integrity in the TS client, and ext4 over the two devices
* the tech debt entries
* the milestone's Outcome

It changes no formats and no behavior. The only new code is two opt-in benches and a CI job that runs only when started by hand.

**Survey (what this step builds on):**
* **Docs:**
  * **The API is in place:**
    * CLIENT.md §12 is the TS integrity API and §13 the Rust client; DB.md §7.1, SVC.md §6 and docs/BLK.md (§1–4: running it, durability, integrity) cover their objects.
    * spec/formats.md §14 is normative. Its "What it does not catch" paragraph, and CLIENT.md §12, name **TD-WITNESS**, which TECH_DEBT doesn't have yet.
  * **What's still missing:**
    * docs/BLK.md was written in 6.8 to be finished here (the design, the threats, the numbers).
    * docs/API.md has an area section per milestone ("Relayed services (milestone 5.5)"), but none for checkpoints, devices or leases.
* **Measured already** (STATS):
  * §5.2: WASM sign 2.6 ms, verify 0.7 ms, tree state
  * §5.4: authenticated vs basic zen-db
  * §5.6: native checkpoints
  * §5.7: the block engine in process
* **Not measured:**
  * a TS checkpoint end to end (sign, seal, put) and a sync over several devices
  * the cost of event chains on an append
  * ext4 over FUSE and loop, and over ublk
  * storage per checkpoint, per chained event and per volume (§1.1)
* **Tech debt:**
  * TD-SVC-STATE-ROLLBACK is already closed (6.5, but for the window), and TD-BLK-SNAPSHOTS exists (6.7). TD-TEST-OPEN-FAILURES items 1 and 4 were resolved in 6.3; item 6 is new.
  * **Gaps found while surveying, with no entry yet:**
    * **zen-mount doesn't attach integrity** (`packages/fuse` never calls `fs.integrity`). Only `apps/shared` workers (`ZEN_INTEGRITY=1`) and the ledger do.
    * **A file's versions aren't covered by the tree state.** The leaf (fs.md §9.1) commits to a node's version **count** and change offset, not to its version records or manifests. The replica fetches those by `(node, changed)` (`replica.ts` `versionsOf`, `tree.ts` `versions`) and checks them against nothing. So a server could serve an older, still valid version record set under a node's current change offset, and the replica would show old content. It can't forge content.
* **The docs' state:**
  * README says "Milestone 6: in progress".
  * DESIGN-3 §6 item 6 still says "signed roots over a Merkle row and index tree": 6.5 built one state tree per namespace, with no signature per commit.
  * GAPS G26 has no status note.
* **CI:** `workflow_dispatch` already starts the `openssh-pq` job (`if: github.event_name == 'workflow_dispatch'`), the precedent for a job that runs only by hand. The `devices` job has the ublk driver.

### Docs

**docs/INTEGRITY.md (new): the guide**, for app developers and operators. It links to the API sections rather than repeating them.
1. **What the server can and can't do.**
   * It can't read, forge or move values.
   * Without integrity it can serve older values, withhold the newest ones, or fork clients into two histories.
   * Fork consistency (DESIGN.md §2.6) is the limit: a hard bound, not a gap in the implementation.
2. **How it works:**
   * device checkpoints: chained, signed with the hybrid signature, sealed, and cumulative
   * `seen` links between devices
   * pins and floors
   * the cadence and the window it leaves
   * a diagram of two devices' chains with their `seen` links, and where a fork shows
3. **What is covered: a table, one row per object:**
   * the ACL, the signed fs header
   * topics: chains, gaps, rewrites, withheld tails
   * trees: `ops`, chain and state
   * relayed connections
   * authenticated zen-db namespaces
   * volumes

   Each row gives what a device notes, where the check happens, the alarm, and the spec section.
4. **What isn't covered:**
   * **Basic zen-db namespaces**, and KV values outside these objects: consistency of the AEAD only, no freshness.
   * **Ephemeral messages:** replay filtering only.
   * **A file's version records** (the gap above; TD-FS-VERSIONS-INTEGRITY).
   * **`/unencrypted`.**
   * **Readers that use keys or groups** check rewrites only, not gaps.
   * **CRDT tables** are refused in authenticated namespaces.
   * **The first sync trusts on first use:** a device with no pins accepts what it's first shown.
5. **Turning it on:**
   * TS: `fs.integrity`, before `unlock`
   * Rust: `fs.integrity`, or the CLI
   * the workers: `ZEN_INTEGRITY=1`
   * zen-db: `integrity: 'authenticated'`
   * zen-ublk: always on
   * zen-mount: not yet (TD-FUSE-INTEGRITY)
6. **Signers and enrollment:**
   * a device in the ACL vs a registered one
   * `enrollDevice` and `approveDevice`
   * why an enrolled device signs checkpoints but doesn't sign in
7. **Pins:**
   * where they live (the local store, the pins file, the volume pin)
   * what losing them costs (trust on first use again)
   * keeping them with the device key
   * comparing out of band: a phone and a server, `exportPins` / `zen-client pins compare`
8. **Alarms and what to do:**
   * the five kinds
   * what each component does: the TS calls throw and stop signing, a replica stops its feed, a handler aborts the connection, zen-ublk fails closed
   * a suggested app policy
9. **Choosing the cadence:**
   * `sigEvery` and `delayMs`
   * `sigEvery: 1` for handlers and volumes that must close the window
   * the costs (STATS §6)
10. **Testing an app against a lying server:**
    * `spawnServer({adversary: true})`, `lie`, `TestServer.fork()`
    * the Rust harness's `Server::fork`

**docs/BLK.md, finished** (§1–4 stay; new sections):
* **§5 How it works.** Keys, leaves, the tree (fan-out 128), the root record chain, a commit (blocks, changed nodes, root, fence), reads checked top-down, and settling a lost answer. It summarizes spec/blk.md and links to it.
* **§6 What the server sees and can do.**
  * **It sees:** the leakage (blk.md §7), the number of blocks and nodes, write timing and locality per commit, and discards as deletes.
  * **It can't:** read, forge, or move blocks.
  * **Caught:** a rollback, by the writer's pin or by checkpoints.
  * **The window:** a read-only reader on another device, before the writer's next checkpoint.
  * **A fork:** caught when checkpoints cross.
* **§7 Numbers:** the engine (STATS §5.7) and ext4 over each frontend (STATS §6.3), with how to choose `--batch-blocks` and `--idle-flush-ms`.
* **§8 Limits:**
  * one writer
  * a fixed size, no snapshots, no delete (TD-BLK-SNAPSHOTS)
  * read-only exports fail with `volume_changed` when the writer moves the root
  * ublk needs the `ublk` feature and libclang to build, and `ublk_drv` to run
* **§9 Running it under systemd:** a unit for `zen-ublk … ublk --id 0` with `KillSignal=SIGTERM` and a `TimeoutStopSec` above the flush, and why `--id` matters on a restart.

**docs/STATS.md:**
* **§1.1, three new rows** (computed from the formats; the numbers are rough here and are worked out from the formats when written):
  * **A checkpoint:** about 4.4 KB sealed with a few entries (the signature is 3,375 B), times `checkpoint_keep` (1,024) per device at most: ~4.5 MB per device. It counts toward the quota.
  * **A chained event (v2):** +52 B (12 + 8 + 32).
  * **A volume:**
    * Per stored block: the 48-byte seal + a 10-byte versionstamp + the key (4 elements, ~72 B), about 3 % over 4 KiB.
    * Per 128 blocks: one level-1 node, about 0.8 %.
    * So a fully written volume costs about 4 %, and an empty one its root record.
* **§6 Integrity: what it costs (new).** A summary table that points to §5.2, §5.4, §5.6 and §5.7, plus the new measurements:
  * **§6.1 TS checkpoints:**
    * a `checkpoint()` end to end with 10 and with 100 entries (sign, seal, put): latency and sealed bytes
    * a `sync()` after 1 and after 10 devices moved
    * `exportPins` size for 10 devices
  * **§6.2 Event chains:** `append` latency and commit bytes with and without integrity attached (median of 50), and an unfiltered read of 1,000 events checked vs not.
  * **§6.3 ext4 on the devices** (release, `--batch-blocks` 256), for FUSE+loop and for ublk:
    * `mkfs.ext4` of 1 GiB
    * a sequential 256 MiB write with `conv=fsync`
    * a sequential read after `drop_caches`
    * 4 KiB `oflag=dsync` writes (fsync latency)
    * untarring a source tree (many small files) with a final `sync`
  * **§6.4 Reading it:**
    * where the time goes: round trips, not crypto, natively
    * choosing `sigEvery`
    * a checkpoint per handler step costs one signature and one put
  * The ublk figures come from a GitHub runner (2–4 vCPU, a different machine from the 4-core development box), and the table says so.

**spec/TECH_DEBT.md, three new entries:**
* **TD-WITNESS** (open):
  * **Context:** fork consistency. Groups that never see each other's checkpoints can't detect the split, except by comparing pins out of band.
  * **What it would take:** a witness, a log outside the server (another zen-serve, or a transparency log). Clients post their device heads `(fp, seq, HC)` to it after each checkpoint and read the others' heads from it. A head the server doesn't show is a fork. Optionally the witness signs the heads it has seen, and a client waits for that before trusting a sync. Plus the fs setting naming the witness, and a test with a forked server and an honest witness.
* **TD-FS-VERSIONS-INTEGRITY** (open): the gap above.
  * **What it would take:** a version digest in the leaf, a hash over the node's version records (dots and sealed manifests), with the spec change (fs.md §9.1, a leaf version 2 and new vectors). The replica checks fetched versions against the node's state. The tree states of 6.4 change, so it lands with a format bump.
* **TD-FUSE-INTEGRITY** (open): zen-mount attaches no integrity.
  * **What it would take:** `--integrity` (pins in the store directory, the device key as signer), the replica's alarm surfaced as `EIO` and a log line, and a test.

**Also in TECH_DEBT:**
* TD-SVC-STATE-ROLLBACK stays closed, unchanged.
* TD-BLK-SNAPSHOTS stays as written in 6.7.

**Elsewhere:**
* **docs/API.md:** an "Integrity (milestone 6)" area section, like the relayed services' one. It lists `/v1/checkpoint/{put,heads,get}`, `/v1/devices/{register,get}`, `/v1/lease`, `Commit.fence`, `cksub`/`ckh`, and the signed header, with links to api.md §15 and §16.
* **CLIENT.md §12:**
  * a pointer to INTEGRITY.md at the top
  * "volumes in a later step" becomes "volumes (zen-blk)"
* **spec/OPS.md §4:** three pitfalls:
  * **Keep the pins files.** Losing them means trust on first use again.
  * **An authenticated namespace serializes its writers.**
  * **`sigEvery: 1` in a browser** costs 2.6 ms of signing per change.
* **GAPS.md G26:** "**Done (milestone 6):** topic heads and event chains in device checkpoints (formats.md §5.1, §14)".
* **DESIGN-3 §6 item 6:** reworded to what was built (one state tree per namespace, no signature per commit), and marked done.
* **README:**
  * Milestone 6 done, with "outcome at its end"
  * two lines in the building blocks: integrity, and block devices
  * docs/INTEGRITY.md and BLK.md in the docs row
* **docs/MILESTONE-6.md, an Outcome section** in the shape of 5.5's:
  * **What was built,** per step.
  * **Differences from the plan:** collected from each step's "Changes from the plan" and "Found while …" in the log. For example: the state tree instead of row trees, the `0x00 0x00` terminator, purge records, `observe` with `floorAt`, the engine's lease fixes, settling a lost answer, libublk's per-thread ring.
  * **Tests:** counts per crate and package, taken from the runs at this step. The negative controls are summarized.
  * **CI:** the `devices` job, the `client` job's `test-utils` build, and `devices-bench`.
  * **Measurements:** STATS §5.6, §5.7 and §6.
  * **Tech debt:** resolved (TD-SVC-STATE-ROLLBACK; TD-TEST-OPEN-FAILURES items 1 and 4) and new (TD-BLK-SNAPSHOTS, TD-WITNESS, TD-FS-VERSIONS-INTEGRITY, TD-FUSE-INTEGRITY; item 6).

### Benches (opt-in; none runs in a normal CI build)

* **`packages/client/test/integritybench.test.ts`** (new):
  * It runs only with `ZEN_BENCH=1`, like `packages/db/test/bench.test.ts`, whose skip pattern it copies. It runs against a release server (`ZEN_SERVE_BIN`) and release WASM, as §5.5 did.
  * It measures the §6.1 and §6.2 numbers above, the median of 20–50 runs each.
  * Ten devices: one TS device plus nine enrolled ones, each writing one checkpoint per round.
* **`crates/zen-ublk/tests/devices.rs`, two `#[ignore]` tests, `bench_fuse` and `bench_ublk`:**
  * They reuse the suite's guards and `serve` helper, on a 1 GiB volume, and print a table.
  * They run with `cargo test --release -p zen-ublk --features device-tests[,ublk] --test devices bench -- --ignored --nocapture` as root.
  * `bench_ublk` follows the suite's rule: it skips with a message when there's no `/dev/ublk-control`, and fails when `ZEN_TEST_REQUIRE_UBLK=1`.
  * The source tree for the untar step is a tarball the test makes from `crates/` (a fixed set of files, deterministic). It needs no download.
* **CI job `devices-bench`**, which runs only when started by hand:
  * `if: github.event_name == 'workflow_dispatch'`, the same setup steps as `devices`, then both benches in release, with `ZEN_TEST_REQUIRE_UBLK=1`, `timeout-minutes: 30`.
  * I start it once on this branch (`actions_run_trigger`), read its log, and record the numbers in STATS §6.3.
  * The FUSE numbers are also measured here, on the development machine, and both sets are given with their machines.

### Verification

* **Rust:**
  * `cargo fmt --check`
  * clippy `-p zen-ublk --all-targets --features device-tests,ublk -D warnings`, plus the workspace's three feature sets (the benches only add tests)
* **The benches:**
  * `bench_fuse` here (release, as root), twice, to see the spread
  * `ZEN_BENCH=1` on the integrity bench, twice
  * `devices-bench` once in CI, green
  * Before the release builds, prune `target/debug/deps` and `target/release` as needed: about 6.6 GB is free.
* **JS:** `npm run check`. The new bench file is skipped without `ZEN_BENCH` (vitest lists it as skipped, as the other benches).
* **Docs:**
  * every link and section reference in the new and changed docs resolves (a small script over the `](…)` links and `§` references of the touched files)
  * every TD named anywhere exists in TECH_DEBT (`grep -o 'TD-[A-Z0-9-]*'` over `docs` and `spec`, compared with the headings)
* **CI:** green on both runs (push and pull_request). `devices-bench` doesn't run on them.

### Files

* **Docs:**
  * new: `docs/INTEGRITY.md`
  * `docs/{BLK,STATS,API,CLIENT,GAPS,DESIGN-3,MILESTONE-6}.md`
  * `spec/{TECH_DEBT,OPS}.md`, `README.md`
* **Benches:** `packages/client/test/integritybench.test.ts` (new), `crates/zen-ublk/tests/devices.rs`
* **CI:** `.github/workflows/ci.yml` (the `devices-bench` job)

**Defaults chosen (overridable):**
* **The two gaps found while surveying are recorded as tech debt, not fixed here.** The version digest changes fs.md §9.1's leaf and its vectors: a format change, out of place in a docs step. zen-mount's integrity is a feature.
* **ublk is benched in a CI job started by hand**, since this container has no ublk driver. FUSE is benched both here and there.
* **INTEGRITY.md is a guide that links to the API sections** (CLIENT.md §12–13, DB.md §7.1, SVC.md §6, BLK.md) rather than moving them.

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`); the touched crates' tests; FDB once.
* **JS:** `npm run check`; `npx vitest run` 3×; `npx playwright test` when the browser is touched.
* **Detection is tested against the adversary,** never only on honest runs. Each attack must fail its test with detection turned off.
* **Devices:** FUSE and loop here and in CI; ublk in its CI job.
* CI green on both runs of each PR.

## Risks
* **Scope:** the largest milestone yet. The block engine and native client come last, so the integrity pieces can't be held up by kernel availability.
* **Contention:** `NsRoot` serializes the writers of an authenticated namespace. It's opt-in and documented.
* **Checkpoint cost:** 3.4 KB and a signature per checkpoint, bounded by the cadence. Measured.
* **ublk on hosted runners:** if `modprobe` fails there, the ublk job is dispatch-only, and the FUSE export carries the device tests (TD).
* **Disk here:** 3.4 GB free; new crates add build output, so prune before each build.
