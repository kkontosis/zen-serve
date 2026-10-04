# Milestone 3 plan: FoundationDB backend, supervisor, backup/PITR

## Context

Milestone 2 (merged in kkontosis/zen-serve#3) delivered `zen-serve` on the embedded redb backend. The storage sits behind the FDB-shaped `Storage`/`Txn` trait (`crates/zen-store/src/lib.rs`), and the keyspace is in FDB tuple encoding (`spec/keyspace.md`).

Milestone 3 makes FoundationDB the production backend. One binary supervises its own `fdbserver` processes (no docker-compose, DESIGN-2 §6), several zen-serve nodes can serve one cluster, and backup/PITR (G14) is provided.

**Decisions taken:**
* ephemeral pub/sub goes through a short-lived FDB ring
* backup = FDB native continuous backup + a ciphertext logical export/import
* FDB binaries come from a pinned official 7.3.x release, checked by sha256

**After compaction, re-read before coding:**
* `docs/MILESTONE-2.md` (Outcome section)
* `docs/DESIGN-2.md` §6
* `docs/DESIGN.md` §5
* `docs/GAPS.md` G13, G14, G18, G20, G22
* `spec/keyspace.md`, `spec/api.md`
* `crates/zen-store/src/{lib,embedded}.rs`
* `crates/zen-server/src/{lib,state,auth,stream,txn,commit}.rs`
* `crates/zen-server/tests/common/mod.rs`

Branch: `claude/brave-knuth-gk03mm`, which equals `main`. Open a **new** PR when done (short description).

## Scope

**In:**
* FDB `Storage` backend
* trait adjustments
* a backend conformance suite run on both backends
* a key-prefix adapter (test isolation, restore-into-prefix)
* shared sessions and challenges
* the cross-node ephemeral ring
* the `fdbserver` supervisor: `init`, `join`, `status`, auto redundancy and coordinators
* FDB native backup/restore wrappers
* logical `export`/`import`/`migrate`
* CI job with real FDB

**Out:**
* automatic TLS between FDB nodes (operator-supplied TLS files are passed through; auto-TLS comes with ACME later)
* building FDB from source
* event retention/compaction
* server-side CRDTs
* rolling FDB upgrades (G22, only the version pin now)

## Step 0: persist this plan
Copy it to `docs/MILESTONE-3.md` and commit first.

## Step 1: trait adjustments (`zen-store`)
* `fn watch(&self, key) -> Watch` becomes `async fn watch(&self, key) -> Result<Watch>`. In FDB a watch is only armed once its transaction commits. Callers keep the pattern "await watch, then read" (`stream.rs` subscription, `consume.rs::next`, `acl::follow`).
* `fn now_version()` becomes `async fn now_version() -> Result<Version>`. FDB: the cached GRV, refreshed when more than 100 ms old.
* New `Error::CommitUnknown` (FDB 1021). `commit.rs` treats it like conflict/too_old: look the commit up by `commit_id` and replay, otherwise retry. `txn_loop!` retries it only when a `commit_id` record makes the retry safe; otherwise it surfaces 409.
* **`prefixed.rs`: `Prefixed<S>`**, which prepends a root prefix to every key, strips it on reads, and shifts versionstamp placement. Used for test isolation on a shared FDB, and for serving a restored clone (`storage.key_prefix` config).
* **`conformance.rs`** (feature `testing`): the M2 `tests/embedded.rs` cases rewritten as `async fn run_all(make: impl Fn() -> S)`, plus a randomized concurrent "bank transfer" invariant test (concurrent conflicting commits keep the total constant). Run for embedded, Prefixed(embedded) and FDB.

## Step 2: FDB backend (`zen-store/src/fdb.rs`, feature `fdb`)
* Crate `foundationdb = "0.11"`, features `fdb-7_3`, `embedded-fdb-include`. The `fdb` feature is off by default, so plain `cargo test` and the wasm checks don't need libfdb_c.
* `Fdb::open(cluster_file)`. `foundationdb::boot()` runs once per process; its guard is held in a `OnceLock` / returned to `main`.
* **Mapping:**
  * `begin(rv)` → `create_trx` + `set_read_version`
  * `get` / `snapshot_get`
  * `get_range` with `RangeOption{limit, reverse, mode: WantAll}`; FDB adds exact conflict ranges itself
  * `add_read_conflict_range`, `set`, `clear`, `clear_range`
  * versionstamped key/value: `MutationType::SetVersionstampedKey/Value` with the 4-byte LE offset suffix
  * `atomic_add` → `MutationType::Add` (same i64 LE)
  * `commit`
* **Error mapping:** 1020 → `Conflict`; 1007 / 1009 / 1037 (too old, future version, process behind) → `TooOld`; 1021 → `CommitUnknown`; 2015 → `Unreadable`; else `Io`.
* **Watch hub:** one FDB watch per key per process, shared by every waiter, re-armed after it fires. This keeps the process under FDB's 10k watch limit (DESIGN-2 §2.4).
* Limits already fit: value ≤ 90 kB < 100 kB, commit ≤ 8 MB < 10 MB, transactions < 5 s.

## Step 3: multi-node server state (`zen-server`)
* **Config:** `[storage] backend = "embedded" | "fdb"`, `cluster_file`, `key_prefix`. The `fdb` feature passes through to zen-store. `start()` picks the backend.
* **Sessions → keyspace:** `pack("sess", BLAKE3(token))` → `user_fp ‖ device_fp ‖ u64 expires_unix`, stored hashed so a dump leaks no bearer tokens. Each node keeps a ≤30 s in-memory cache; every request is still checked against the ACL.
* **New endpoint** `POST /v1/auth/logout`.
* **Challenges → keyspace:** `pack("chal", challenge)` → expiry, consumed in the same transaction that creates the session (single use across nodes).
* **Ephemeral ring:** `pack("eph", fs, vs)` → `topic ‖ sender ‖ data`, plus `pack("eh", fs)` as head. Each node runs one tailer per fs with live ephemeral subscribers (watch head, range-read after its cursor, fan out to local `esub`s). The sweeper clears entries older than 60 s.
  * New starts only (no history).
  * Wording in `spec/api.md` §9 changes to "kept at most ~60 s, never in backups beyond that window".
* **Sweeper** additions: sessions, challenges, the eph ring, and expired `km` claims. It runs on every node; sweeps are idempotent transactions.
* **Spec:** keyspace.md (sess, chal, eph, eh), api.md (logout, ephemeral wording).

## Step 4: supervisor (`zen-server/src/supervisor/`)
* **Binary discovery:** config `[fdb] bin_dir` (default `/usr/sbin`, `/usr/bin`, `/usr/lib/foundationdb`), plus `processes` (default 1 per node; 1 per core is suggested in docs), `listen_ip`, `data_dir/fdb/<port>`, and `public_ip`.
* **`scripts/install-fdb.sh`:** pinned 7.3.x client + server `.deb`/tarball from Apple's GitHub releases with sha256 verification. Used by CI and documented for installs.
* **`zen-serve init`:**
  1. generate the cluster file `zen:<random>@<ip>:<port>`
  2. spawn the `fdbserver` children
  3. `fdbcli --exec "configure new single ssd"`
  4. start the API (or `--no-api`)
* **`zen-serve join <token>`:**
  * The token is base64url of the cluster-file contents. It is not secret against network peers: inter-node trust = private network or operator TLS (documented, G18).
  * The node writes the cluster file, spawns its children, then the redundancy policy runs.
* **Redundancy policy** (one leader via a `pack("meta","supervisor")` lease in FDB, the same lease code idea as consumers): count machines from `\xff\xff/status/json`.
  * < 3 machines → `single`; ≥ 3 → `double`; ≥ 5 → `triple`
  * then `coordinators auto`
  * applied through `fdbcli --exec`
* **Process supervision:** restart with exponential backoff (max 30 s), log child stderr, kill children on shutdown.
* **`zen-serve status`:** a summary from `status json` (machines, processes, redundancy, health). It's also exposed admin-only as `POST /v1/admin/status`.
* **TLS passthrough:** optional `[fdb] tls_cert/tls_key/tls_ca` → `fdbserver --tls_*` args and client network options.

## Step 5: backup, PITR, export/migrate
* **Native backup:**
  * The supervisor also runs `backup_agent` children when `[backup] enabled`.
  * `zen-serve backup start --dest <file:///…|blobstore://…>` wraps `fdbbackup start -z -d`; `backup status|stop`.
  * `zen-serve restore --source … [--timestamp T | --version V] [--add-prefix P]` wraps `fdbrestore`. Restore unit = the whole keyspace (G14).
  * **"Clone at time T"** = restore with `--add-prefix`, then run a second zen-serve with `storage.key_prefix = P`.
* **Logical export/import** (works on any backend):
  * `zen-serve export --out file.zen` streams `pack`ed key/values, framed `lp(k) ‖ lp(v)` with a header `{format 1, keyspace 1, source version}`. It's ciphertext only, so the operator needs no keys.
  * Embedded: one consistent snapshot. FDB: documented as non-atomic across transactions unless the server is stopped (use native backup for consistency).
  * `zen-serve import --in file.zen` writes in ≤ 5 MB transactions, refusing a non-empty target unless `--force`.
* **`zen-serve migrate --from-data-dir <embedded> --to fdb`:** offline export → import, versionstamps preserved byte-for-byte (they are values and key elements). It then **bumps the FDB version past the max embedded version** (`\xff/minRequiredCommitVersion`, via `fdbcli advanceversion`), so new versionstamps stay larger than migrated ones.

## Step 6: tests and CI
* `zen-store`: the conformance suite on embedded + Prefixed in the default CI job, on FDB in the new job.
* `zen-server` integration tests: the harness reads `ZEN_TEST_BACKEND=fdb` + `ZEN_TEST_CLUSTER_FILE`; each test gets a random `key_prefix`, so tests run in parallel against one cluster. All 19 M2 tests must pass on both backends.
* **New tests:**
  * two zen-serve nodes on one FDB: a session from node A works on node B; ephemeral across nodes; a subscription on node B sees appends via node A; a consumer lease is fenced across nodes
  * export → import round-trip equals the source (embedded→embedded, embedded→fdb)
  * supervisor: `init` in a tempdir starts a working single-node cluster, and killing the `fdbserver` child gets it restarted (FDB job only)
* **CI:** a new `fdb` job: `scripts/install-fdb.sh` (client + server), then `cargo test --features fdb` for zen-store + zen-server, with the cluster started by `zen-serve init --no-api` in the background. The existing job stays unchanged.

## Commits
1. plan doc
2. trait + Prefixed + conformance
3. FDB backend
4. multi-node state + spec
5. supervisor + install script
6. backup/export/migrate
7. tests + CI

Push after each green step. A new PR at the end.

## Verification
* Default job: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` (with `--all-features` once FDB is installed locally), `cargo test --all`, and the wasm32 check for zen-core + zen-proto.
* Locally: install FDB with the script, then:
  1. `cargo run -p zen-server --features fdb -- init --config examples/zen-serve-fdb.toml`
  2. `curl localhost:8080/v1/info`
  3. `ZEN_TEST_BACKEND=fdb cargo test --features fdb`
  4. `zen-serve status`
  5. `zen-serve export` / `import` round-trip
  6. `zen-serve backup start --dest file:///tmp/zb`, write data, then `zen-serve restore --timestamp … --add-prefix`, and check the clone with a second server

## Outcome

Done. Differences from the plan above:

* **FoundationDB release.** Pinned to **7.3.79**, the newest 7.3 release (`scripts/install-fdb.sh`, sha256 of Apple's `.deb` packages). The script unpacks the packages, and starts no service.
* **`Txn::commit` returns the versionstamp**, not the version. FoundationDB's batch order is not always 0, so the API's `versionstamp` comes from the backend.
* **New trait method `advance_version`.** It makes `import`/`migrate` correct on both backends. On FoundationDB it writes `\xff/minRequiredCommitVersion` from the client, the same as `fdbcli advanceversion`, so no `fdbcli` is needed.
* **Challenges are stateless:** a MAC under a cluster-wide key (`meta/"challenge_key"`), plus a "consumed" record written when the session is created. Unauthenticated requests no longer write to storage.
* **The session cache is 10 s**, not 30 s, so logout reaches the other nodes faster.
* **Leader for the redundancy policy.** The node that owns the lowest process address in `status json` acts. No lease in FoundationDB: the policy only raises the mode, so a rare double action is harmless.
* **Lease expiry inside transactions uses the transaction's read version**, not the cached clock. On an idle FoundationDB cluster versions advance in ~2 s steps, so expiry can be late by that much, never early (api.md §8.2).
* **Retryable storage errors.**
  * `txn_loop!` also retries `TooOld` and the transient errors FoundationDB maps to it.
  * `commit_unknown` is retried only where the body detects its own earlier commit: `/v1/commit`, ACL put and session creation. Everything else returns 409 `commit_unknown`.
* **Expired `km` claims** still need no sweep: a claim exists only while its key is in the ready list, and the next claim overwrites it.
* **`[storage] backend` is optional.** It defaults to `fdb` on a node set up by `init`/`join`.
* **Not done:** clearing a restored clone is a documented `fdbcli clearrange` (spec/operations.md §5.3), not a zen-serve command.

Tests:
* the zen-store conformance suite (13 cases) on embedded, Prefixed(embedded) and FoundationDB
* all server tests on both backends
* new tests: multi-node (sessions, challenges, logout, ephemeral, subscriptions, fencing across nodes), export/import/migrate round trips, and the supervisor (init, crash restart)

The CI `fdb` job starts its cluster with `zen-serve init --no-api`.

Checked by hand:
* three nodes (`init` + 2 × `join`) went to `double` with 3 coordinators automatically
* a killed `fdbserver` was restarted
* a backup restored at a timestamp into a prefix held exactly the data written before that time
