# Milestone 2 plan: zen-server + embedded backend

## Context

Milestone 1 (merged in kkontosis/zen-serve#2) delivered `docs/` (design), `spec/` (normative formats, labels, suites, glossary, test vectors) and `crates/zen-core` (client crypto). Milestone 2 builds the **keyless server**: the HTTP/WebSocket API from `docs/API.md`, running on an **embedded single-node backend** behind a `Storage` trait that mirrors FoundationDB semantics, so the FoundationDB backend in milestone 3 is a thin adapter. Outcome: one `zen-serve` binary that a client can authenticate to, read and commit KV through, append, subscribe to and consume events from, and load `/unencrypted` from.

**After compaction, re-read before coding:**
* `docs/DESIGN-3.md`: primitives, commit, KV modes, leases/fencing, `/unencrypted`, cross-origin isolation
* `docs/DESIGN-4.md` §1: event keys and consumer modes
* `docs/API.md`
* `docs/GAPS.md`: G1, G4, G7, G9, G12, G13, G15, G19
* `spec/` (all of it)
* `crates/zen-core/src/{sig,labels}.rs`

Branch: `claude/brave-knuth-gk03mm`, which already equals `main`. Open a **new** PR when done (short description).

## Scope

**In:**
* auth (device challenge/session)
* signed ACL + membership chain
* `/v1/info`
* fs headers/keyslots
* KV get/range
* `/v1/commit` (short + long modes, idempotency, writes, appends, consume)
* event log (keys, cursors, WebSocket push, gap-free)
* consumer groups (`broadcast`, `sequential`, `partitioned`, `per_key`, `single_key`) with leases/claims + fencing
* DLQ (G7)
* basic quotas/limits (G12)
* GC sweeper (G13)
* `/unencrypted` static serving
* ephemeral pub/sub over the WebSocket

**Out (later milestones):**
* FoundationDB backend and supervisor (M3)
* server-side CRDT objects (`crdt_ops` returns 501)
* event retention/compaction and snapshots
* ACME/TLS (M2 serves plain HTTP for use behind a reverse proxy; an optional rustls cert/key in config is fine)
* client TS library

## Step 0: persist this plan
Copy this plan to `docs/MILESTONE-2.md` and commit it first, so it survives container loss.

## Step 1: spec additions (`spec/`)
* **`spec/api.md`:** wire format of every endpoint.
  * Bodies are **CBOR** (`application/cbor`, via `ciborium` + `serde_bytes`); byte fields are raw.
  * Errors: `{code, message}` with HTTP status: 409 conflict, 412 `cursor_moved`/`not_leader`/`claim_lost`, 413 too large, 403 ACL, 429 quota, 501 crdt.
  * `GET /v1/info` returns version, suites, limits, features, `cross_origin_isolation` (G4).
* **`spec/keyspace.md`:** the server's storage layout, shared by both backends:
  * KV
  * value version metadata
  * log
  * per-key index
  * consumer cursors / ready / claims / leases
  * idempotency records
  * ACL history
  * fs headers
  
  Keys use the **FoundationDB tuple encoding** (subset: bytes, int, string, nested tuple), so the FDB backend stores identical keys.
* **ACL document** (`spec/formats.md` §9, deterministic CBOR):
  * `{version, prev_hash, admins: [public identity], members: [{user public identity, device certs}], grants: [{subject_fp, fs_id, rights: [read|write]}, {subject_fp, fs_id, topic_prefix, rights: [read|append|consume]}], limits, retention}`
  * signed with `zen/v1/sig/acl` by an admin identity
  * `prev_hash` chains versions, which makes it the G1 membership log
* **Bootstrap:** on first start the server prints a one-time **claim token**. The first `PUT /v1/acl` must carry it, and it pins the first admin. After that, only an admin signature is accepted.
* **New label** `zen/v1/sig/session` (device signs `challenge ‖ server_origin`). Add it to `labels.rs` `ALL`/`SIG_PURPOSES` and `spec/labels.md` (the registry test enforces this).
* **`expect_ranges` hash** = `BLAKE3(lp(key) ‖ versionstamp …)` over the range in key order.

## Step 2: `crates/zen-store`
* **Storage trait** (async, FDB-shaped):
  * `get_read_version()`
  * `Txn { get, get_range(begin, end, limit, reverse), set, clear, clear_range, set_versionstamped_key/value, add_read_conflict_range, commit() -> Result<Version, Conflict | TooOld> }`
  * `watch(key)`
  * a background `now_version()`
* **Version clock:** like FDB, ≈1,000,000 versions/s: `version = max(last+1, micros_since_epoch)`. A versionstamp is 10 bytes, `u64 version ‖ u16 batch order`. Lease expiry uses versions (DESIGN-3 §3.1).
* **Embedded backend on `redb`:**
  * Commits are serialized under one writer.
  * **Backward OCC validation:** an in-memory recent-writes log covers the last ~5 s of committed key writes. A commit whose read conflict ranges intersect writes after its read version → `Conflict`. A read version older than the window → `TooOld`.
  * Reads are served from the latest snapshot, which is safe because the validation above aborts any transaction that observed a newer write.
  * Watches use `tokio::sync::watch`/`Notify` per key.
* **`tuple.rs`:** an FDB tuple-encoding subset, with unit tests against known FDB encodings.
* **Tests:** conflict detection, phantom detection on ranges, versionstamp ordering, TooOld, watch wakeups, persistence across reopen.

## Step 3: `crates/zen-proto`
Serde request/response types for every endpoint, shared with future clients. Keep it wasm-compatible: no tokio.

## Step 4: `crates/zen-server` (binary `zen-serve`)
Stack: axum + tokio + tower-http. The CLI is `zen-serve serve --config zen-serve.toml`. Config fields:
* listen
* data_dir
* `[[fs]] id`
* unencrypted_dir / source
* aliases
* spa_fallback
* cross_origin_isolation
* limits
* CORS allowlist

Modules:
* `auth.rs`:
  * challenge → session: verify the device cert chain against the current ACL with `zen_core::sig::{PublicIdentity::verify, verify_device_cert}`
  * random 32-byte session tokens with a TTL
* `acl.rs`: load/verify/CAS the signed ACL, plus the claim token. Enforcement helpers: `can(subject, fs, right)` and `can_topic(subject, fs, topic_prefix, right)`.
* `kv.rs`:
  * `grv`, `get`, `range`
  * values are stored as `versionstamp(10) ‖ sealed_value` (version metadata)
* `commit.rs`: the single write path, in one storage transaction:
  1. idempotency check (`commit_id` → stored result, TTL 24 h)
  2. ACL + limits
  3. short-mode read conflicts / long-mode `expect` + `expect_ranges`
  4. writes
  5. appends (log entry + per-key index + `ready` insertion for `per_key` groups)
  6. consume (cursor/claim/lease token checks, advance, re-queue)
  7. store the result
* `log.rs`: `GET /v1/log/{topic}?after=&limit=`, and `append` as commit shorthand.
* `consume.rs`:
  * `POST /v1/consume/groups`: modes are immutable (G8)
  * lease acquire/renew/release
  * dispatcher for `sequential` (delivery gate, `max_inflight`), `per_key` (ready list + claims, oldest first), `single_key`, `partitioned`
  * `nack` → attempts++, and at `max_attempts` atomically append to `<topic>.dlq` and advance (G7)
  * `on_poison: block` option
* `stream.rs` (WebSocket `/v1/stream`):
  * `subscribe {topic|prefix, after}` reads history from storage, then switches to live without a gap (G9). Watches only wake; data always comes from range reads.
  * `consume {group}` pushes ready events + claim tokens
  * ephemeral `publish/subscribe` is in-memory fan-out, ACL-checked
* `statics.rs`:
  * `/unencrypted/*` from a dir, or bundled defaults via `include_dir`
  * root aliases
  * SPA fallback for `GET` + `Accept: text/html`
  * path canonicalisation, symlink escape refused
  * `Service-Worker-Allowed: /` on `sw.js`
  * default CSP + Trusted Types headers (G15)
  * optional COOP/COEP/CORP (DESIGN-3 §4.2)
* `sweeper.rs`: background expiry of idempotency records, sessions, claims and leases (G13).
* **Logging:** never log tokens/ids (G19), only counts.

## Step 5: tests and CI
* **Integration tests** in `crates/zen-server/tests/`: start the server on `127.0.0.1:0` with a tempdir. A small test client (reqwest + tokio-tungstenite + zen-core) signs in as fixture devices. Cases:
  * claim + ACL CAS
  * unauthorized → 403
  * commit idempotent replay (same `commit_id` → same result, applied once)
  * concurrent conflicting commits → exactly one 409
  * long-mode `expect` stale → 409
  * phantom via `expect_ranges`
  * append + cursor read order
  * WebSocket subscribe gap-free across reconnect
  * sequential gate (e+1 withheld until e commits)
  * per_key parallel across keys / serial within a key
  * lease takeover + stale token → 412
  * DLQ after `max_attempts`
  * events published in an aborted commit are never visible
  * static: alias, fallback, traversal refused, COOP/COEP only when enabled
* **CI:** add `cargo test --all` (already present). Keep the wasm32 check for `zen-core` and `zen-proto` only.

## Commits
1. plan doc
2. spec
3. zen-store
4. zen-proto
5. zen-server (may be split)
6. integration tests + CI

Push after each green step. Open a new PR at the end with a short description.

## Verification
* `cargo fmt --check`
* `cargo clippy --all-targets --all-features -- -D warnings`
* `cargo test --all`
* `cargo check -p zen-core -p zen-proto --target wasm32-unknown-unknown`
* **Manual smoke test:**
  1. `cargo run -p zen-server -- serve --config examples/zen-serve.toml`
  2. `curl -i localhost:8080/` → index.html with security headers
  3. `curl localhost:8080/v1/info`
  4. run the integration test client against it
