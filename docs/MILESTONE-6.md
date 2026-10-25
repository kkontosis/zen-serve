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
  * Next: CI green on both runs.

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
