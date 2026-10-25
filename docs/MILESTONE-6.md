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
