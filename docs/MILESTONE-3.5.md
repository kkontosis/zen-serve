# Milestone 3.5 plan: CRDT filesystem (specs, then server side)

## Context

DESIGN-4 §2.3 and §2.7 make the filesystem a **server-merged CRDT**: a move-tree plus a multi-value content register per file plus chunks. The server merges on ids and clocks only; names, metadata and contents stay ciphertext. Yet DESIGN-4 §6 parks the "tree-CRDT filesystem" at milestone 7, after the client library (M4) and the "filesystem library over KV" (M5). That order doesn't work: the client library and zen-fs need the server primitive and its wire format first. Today `/v1/commit` answers non-empty `crdt_ops` with 501.

Milestone 3.5:
1. Specify the CRDT filesystem byte-exactly: semantics, ops, sealed formats, keyspace, API.
2. Implement what the server needs: op merging inside `/v1/commit`, reads, the change feed, chunks, garbage collection.

Client-side zen-fs logic (local replica, POSIX API) stays in M4/M5, and the KV-based filesystem of M5 is dropped.

**Decisions (user):**
* tree ops ordered by **HLC timestamps**: Kleppmann move-tree with undo/redo over a bounded move log
* **visible chunk ids** in file versions, so the server can garbage-collect chunks
* **filesystem only**, behind a generic `crdt_ops` framework that can take more object types later
* **record the device and keep an op hash chain now; signed checkpoints later**

**After compaction, re-read:** `docs/DESIGN-4.md` §2, `docs/GAPS.md` G10/G11/G23, `spec/api.md` §6, `spec/keyspace.md`, `spec/formats.md` §4/§8, `spec/labels.md`, `crates/zen-server/src/{commit,keys,consume,log,lib}.rs`, `crates/zen-proto/src/lib.rs`, `crates/zen-core/src/{seal,labels,vectors}.rs`, `crates/zen-server/tests/common/mod.rs`. Branch `claude/brave-knuth-gk03mm`: restart it from `origin/main` (PR #5 is merged). Open a new short PR at the end.

## Step 0: persist the plan
Write `docs/MILESTONE-3.5.md` (this plan) and commit.

## Step 1: specs (normative)

### New `spec/fs.md`: the CRDT filesystem

**Objects**
* A **tree** belongs to an fs, with a random 16-byte `tree` id; an fs may hold several trees.
* **Nodes** have random 16-byte ids. Two are reserved: `ROOT = 00…00` and `TRASH = FF…FF`. They always exist and never move.
* A node carries:
  * a parent, with its move timestamp
  * sealed **meta** (name, mode, mtime, xattrs) as an LWW register
  * for files, a **content register**: a multi-value register of **versions**

**Timestamps**
* `ts = (hlc: u64, device_fp: 32 B)`, totally ordered.
* `hlc = unix_ms << 16 | counter`.
* The client keeps HLC rules: `max(wall, last_seen) + tick`, and observes every `hlc` the server returns.
* `device_fp` is set by the server from the session, so a device can't impersonate another replica.

**Ops**
* **`move {node, parent, hlc, meta?}`**: create, move, rename+move, or delete (= move to TRASH).
  * The server applies moves in `ts` order. A move that would make a node its own ancestor is a no-op.
  * A late op undoes the logged moves with greater `ts`, applies itself, then redoes them (Kleppmann et al. 2021).
  * A `meta` inside a move is applied as a separate LWW write with the same `ts`. So a concurrent rename and move both survive.
* **`meta {node, hlc, meta}`**: LWW on `ts`. It needs no log.
* **`write {node, replaces: [dot], chunks: [chunk_id], manifest}`**: a dotted MV-register.
  * The new version's `dot` is `versionstamp ‖ u16(i)`, assigned by the server.
  * The versions listed in `replaces` (the ones the writer has seen) are removed. Concurrent writes remain as **siblings**, shown by the client as conflict copies.
  * `write` with empty `chunks` and `manifest` = truncate to empty.
* **Chunks** are uploaded separately, through the commit field `chunks: [{fs, id, data}]`, before the `write` that references them.

**Rules**
* The parent must exist (ROOT, TRASH or an applied node). The node of a `meta`/`write` must exist.
* **Clock skew:** `hlc` more than `limits.crdt_max_skew_ms` (default 60 s) ahead of server time → 409 `clock_skew`.
* **Horizon (G10):**
  * an op with `hlc` older than `now − limits.crdt_horizon_secs` (default 7 d) → 409 `stale_op`
  * an op whose undo/redo would touch more than `limits.crdt_max_redo` (default 1000) logged moves → 409 `stale_op`
  * the client rebases: it re-issues the op with a fresh HLC, so the op takes arrival order
* **Garbage collection:**
  * moves older than the horizon leave the log
  * nodes in TRASH whose move is older than the horizon are purged with their subtree and content
  * unreferenced chunks are deleted after a grace period (`limits.chunk_grace_secs`, 24 h)
* **Isolation (G11):** in long mode (no `read_version`) the server retries conflicts itself, so CRDT ops never fail with a conflict. In short mode they conflict like any read.
* **Duplicate names:** the server can't see names. Clients display `foo (2).txt` (DESIGN-4 §2.3). The optional name token is reserved, not implemented.
* **Change feed:** every node change gets a new change versionstamp. Clients sync with `/v1/fs/tree/changes` from a cursor: full sync from none, partial after.
* **Integrity, now and later:**
  * Now: a per-tree **op chain**, `chain_i = H("zen/v1/tree-op-chain", chain_{i−1} ‖ lp(canonical op ‖ device_fp))`, in arrival order, readable through the API.
  * Later: signed checkpoints over (state hash, chain) (§10 of the doc, reserved label `zen/v1/sig/tree-checkpoint`).
* **Leakage section** (DESIGN-4 §2.6): tree shape, move/rename timing, chunk counts and sharing between versions, which device changed what.

### `spec/formats.md`
New sealed kinds (§4 table) and the plaintext layouts:

| kind | what | label | context |
|---|---|---|---|
| 4 | node meta | `zen/v1/aad/fs-meta` | `u32(fs) ‖ tree ‖ node` |
| 5 | file manifest | `zen/v1/aad/fs-manifest` | `u32(fs) ‖ tree ‖ node` |
| 6 | chunk | `zen/v1/aad/fs-chunk` | `u32(fs) ‖ chunk_id` |

* All three are sealed under one new key, `KDF("zen/v1/fs-data", MK_e, u32(fs)‖u32(e))`.
* Plaintext layouts:
  * meta: `u8 v ‖ lp(name) ‖ u32 mode ‖ u64 mtime_ms ‖ lp(xattrs CBOR)`
  * manifest: `u8 v ‖ u64 size ‖ u32 chunk_size ‖ lp(chunk hashes…)`
* Remove "CRDT op encodings" from §8.

### Other spec files
* **`spec/labels.md`:** the new labels (`fs-data`, three AAD labels, `tree-op-chain`, `sig/tree-checkpoint` reserved).
* **`spec/api.md`:**
  * Commit gains `crdt_ops: [{fs, tree, op: "move"|"meta"|"write", …}]` and `chunks`, which replaces the 501. The response gains `dots: [bytes(12)]`.
  * New error codes: `clock_skew`, `stale_op`.
  * New §12 FS endpoints:
    * `tree/list {fs}`
    * `tree/get {fs, tree, nodes}`
    * `tree/children {fs, tree, parent, after?, limit}`
    * `tree/changes {fs, tree, after?, limit, wait_ms?}` (long-poll like `consume/next`)
    * `tree/chain {fs, tree}`
    * `file/get {fs, tree, node}` → versions
    * `chunks/get {fs, ids}`
  * Rights: fs `read` / `write`.
* **`spec/keyspace.md`, §3.6:**
  * `("tr", fs, tree)` → tree header `{chain, op_count}`
  * `("th", fs, tree)` → versionstamp, watched
  * `("tn", fs, tree, node)` → `{parent, move_ts, meta, meta_ts, changed_vs}`
  * `("tc", fs, tree, parent, node)` → children index
  * `("tm", fs, tree, hlc, device)` → move log `{node, parent, old_parent?}`
  * `("tv", fs, tree, vs, node)` → change index
  * `("tf", fs, tree, node, dot)` → version `{chunks, manifest, device}`
  * `("ck", fs, chunk)` → `vs ‖ sealed`
  * `("cr", fs, chunk)` → refcount, atomic add
  * `("cz", fs, vs, chunk)` → GC candidate index
  * The idempotency record gains `u16 dots` (old 44-byte records are still read).
* **`spec/glossary.md`:** tree, node, HLC, ts, dot, version/sibling, chunk, horizon, rebase.
* **`docs/DESIGN-4.md` §6, `docs/GAPS.md` G10/G11/G23:** note milestone 3.5, mark G10/G11 decided, and the replacement of the KV filesystem.

### zen-core
Implement the spec'd client formats so the vectors stay code-generated:
* the `fs-data` key
* seal/open for kinds 4–6 with their contexts
* meta and manifest plaintext encoders
* HLC helpers

Add `spec/test-vectors/fs.json` through the existing `gen_vectors` example, checked byte-exactly in tests. Pattern: `crates/zen-core/src/seal.rs`, `vectors.rs`.

## Step 2: wire types (`zen-proto`)
* `CrdtOp` (serde-tagged `op`: Move/Meta/Write)
* `ChunkPut`
* `Commit.crdt_ops` becomes typed, plus `chunks`; `CommitResult.dots`
* request/response types for the §12 endpoints
* new limits in `Info.limits`

The wasm32 check must still pass.

## Step 3: server (`zen-server/src/tree.rs`, `chunks.rs`)

**Engine: `apply_ops(t, caller, ops, stamp_index) -> dots`, inside the commit transaction** (`commit.rs`, after consumes and appends; validation in `validate`).
* `move`:
  * check skew/horizon
  * reject a duplicate `ts`
  * read the log range `("tm", …, > ts)` (≤ max_redo+1)
  * undo (reverse order: restore `old_parent`, fix the `tc` index)
  * do_op: cycle check by walking ancestors through `tn` reads
  * log `{node, parent, old_parent}`
  * redo the later entries (recompute their `old_parent` and cycle checks)
  * every node touched gets a new `changed_vs` (versionstamped key in `tv`, old entry cleared)
* `meta`: compare and set by `ts`.
* `write`:
  * remove the `replaces` dots that exist, and decrement their chunk refcounts (`atomic_add`), adding `cz` candidates
  * insert the new version at `dot = vs ‖ i`, and increment refcounts
* Chunks: `set_versionstamped_value(("ck",…))`, plus a `cz` candidate. Refcount starts at 0.
* Every op:
  * extends the tree chain (`tr`)
  * bumps `th` (versionstamped)
  * counts toward the fs quotas (`q` bytes/keys)
* Reuse:
  * `txn_loop!` (idempotent form, already used by `/v1/commit`)
  * `keys.rs` builders: new `tree_*`, `chunk_*`
  * `ids::offset` for dots
  * the `consume::next` long-poll pattern for `tree/changes`
  * `Storage::watch` on `th`

**Reads:** the §12 handlers in `tree.rs`, all `POST` + CBOR, using `Caller::require_fs`.

**Sweeper** (`lib.rs::sweep_once`), per fs and tree, in bounded transactions:
* trim the move log older than the horizon
* purge TRASH children older than the horizon (subtree, versions, `tc` and `tv` entries, refcount decrements)
* process `cz` candidates older than the grace period (refcount 0 → delete chunk and refcount)

**Config `[limits]`:** `crdt_max_skew_ms`, `crdt_horizon_secs`, `crdt_max_redo`, `chunk_grace_secs`.

## Step 4: tests
* **Convergence property test (G23)** in `zen-server/tests/fs.rs`:
  * an in-memory reference Kleppmann model (sequential apply in `ts` order)
  * random ops from 3–4 simulated devices, including cycles and trash, submitted through the API in several random arrival orders on fresh servers
  * every final `(node → parent, meta)` equals the reference
* **API tests:**
  * create/move/rename/delete
  * cycle no-op
  * concurrent rename+move both kept
  * MV siblings and resolution via `replaces`
  * chunks round trip
  * `clock_skew`, `stale_op` (horizon and max_redo)
  * permissions (read vs write)
  * change feed (cursor, `wait_ms` wakes on a write)
  * idempotent replay returns the same dots
  * sweeper: trash purge, log trim, chunk GC after grace (tiny limits in config)
* **zen-core:** vectors for kinds 4–6 and the encoders.
* **Every server test** runs on both backends (`ZEN_TEST_BACKEND=fdb`).
* **Multinode (FDB):** ops through node A, changes long-poll on node B.

## Commits
1. plan doc
2. specs + zen-core formats + vectors
3. zen-proto types
4. server engine + endpoints + sweeper
5. tests + docs touch-ups

Push after each green step, then a new short PR.

## Verification
* `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, the wasm32 check for zen-core + zen-proto
* on the local FDB cluster: `ZEN_TEST_BACKEND=fdb cargo test -p zen-server --features fdb`
* `cargo run -p zen-core --example gen_vectors --features test-utils` leaves `spec/test-vectors` unchanged after commit (deterministic)
* CI green on both jobs
