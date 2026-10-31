# Operating and developing zen-serve: pitfalls

Practical notes for running zen-serve and for working on it. Numbers (storage cost, retention, sizes, timings) are in [`docs/STATS.md`](../docs/STATS.md); how to deploy is in [`operations.md`](operations.md).

## 1. Building

| You want | Command | Needs |
|---|---|---|
| The default server | `cargo build -p zen-server --release` | a C compiler (ring) |
| Pure Rust | `cargo build -p zen-server --release --no-default-features` | Rust only |
| No C compiler call at all | `… --no-default-features --features pure` | Rust only |
| FoundationDB backend | add `--features fdb` | `libfdb_c` (`scripts/install-fdb.sh`) |

Pitfalls:
* **`-lfdb_c` not found** when linking an `fdb` build: `libfdb_c.so` isn't on the linker path. `scripts/install-fdb.sh` run as root links it into `/usr/lib`; otherwise set `LIBRARY_PATH` and `LD_LIBRARY_PATH` to `DEST/usr/lib`. **Running the script as root into a scratch directory re-points `/usr/lib/libfdb_c.so` there**: deleting that directory afterwards breaks every `fdb` build until you re-run the script for the real install (or fix the link).
* **Without `fdb` there are no FoundationDB commands**: `init`, `join`, `token`, `backup` and `restore` don't exist in that binary. That's intended for small embedded-only builds.
* **Debug builds compile dependencies with `opt-level = 3`** (crypto is unusably slow otherwise), so the first build is slow and the dependency artifacts are big.
* **Release builds use LTO and one codegen unit**: small and fast binaries, but a release rebuild after any change takes about 2 minutes. Use debug builds while developing.
* **Test features:** zen-server's tests depend on zen-server itself with `default-features = false`, so `--no-default-features` really tests the pure build. A test that builds its own reqwest client must call `zen_server::tls::install_default()` first.

### 1.1 The TypeScript packages

| You want | Command | Needs |
|---|---|---|
| The WASM module | `scripts/build-wasm.sh` | the `wasm32-unknown-unknown` target, `wasm-bindgen-cli` at the Cargo.lock version |
| `@zen/client` | `npm ci && npm run build -w @zen/client` | Node 22 |
| `@zen/db`, `@zen/fs`, `@zen/loro` | `npm run build -w @zen/db` (and so on); tests and apps use the sources, except the zen-mount CLI test, which runs the built `@zen/client` and `@zen/fs` |
| `zen-mount` | `npm run build -w @zen/fuse` | `libfuse-dev`, `pkg-config` and a C compiler when `npm ci` runs (`@cocalc/fuse-native` compiles against libfuse 2); `fusermount` to mount |

Pitfalls:
* **wasm-bindgen versions must match exactly.** The `wasm-bindgen` crate is pinned (`=0.2.129` in zen-wasm and zen-proto) and the CLI must be the same version; `build-wasm.sh` checks and says how to install it.
* **A stale WASM module.** After a change in zen-core, zen-proto or zen-wasm, run `scripts/build-wasm.sh` again: the tests load `packages/zen-wasm/pkg`, which isn't rebuilt by cargo.
* **The client tests spawn `target/debug/zen-serve`** (or `$ZEN_SERVE_BIN`): build it first, after server changes too.
* **`npm ci` without libfuse 2** skips `@cocalc/fuse-native` (an optional dependency): everything but `zen-mount` works, and its tests skip.
* **A FUSE mount left behind** by a killed process: `fusermount -u <dir>`.

### 1.2 TypeScript tests

| You want | Command |
|---|---|
| Every Node test | `npx vitest run` (it builds the apps and `zen-connect` once first, `scripts/vitest-setup.ts`: their workers run from `apps/dist`) |
| One file | `npx vitest run packages/db/test/txn.test.ts` |
| The browser tests | `npx playwright test` (builds the apps first); one project: `--project=apps` |
| The measurements | `ZEN_BENCH=1 npx vitest run packages/db/test/bench.test.ts packages/loro/test/bench.test.ts` (docs/STATS.md §5.4); relayed services: `ZEN_BENCH=1 npx vitest run apps/echo/test/bench.test.ts apps/mail/test/bench.test.ts` (§5.5, measured with `ZEN_SERVE_BIN` set to a release build) |
| Integrity's measurements | `ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve npx vitest run packages/client/test/integritybench.test.ts` (§6.1–§6.2; build it with `cargo build --release -p zen-server --features test-utils`) |

Pitfalls:
* **A server per test file.** Each file spawns its own zen-serve in `beforeAll`; tests in one file share it, so give each test its own database name or topic.
* **`--project=apps`, with the `=`.** In the spaced form Playwright takes the next argument as another project name.
* **Await your own work before closing.** `Db.close()` waits up to 5 s for running transactions, which then fail `closed`; a test that fires writes without awaiting them sees those failures. Keep the promises and await them (`Promise.allSettled`) before a restart.
* **Poll with a deadline** (`until`), never a fixed sleep: consumers, the indexer and the stream run in the background.
* **Kill only the processes you spawned** (app workers, servers), by PID, never `pkill -f`.
* **Several runs at once:** set `ZEN_APPS_BUILT=1` on all but one (or on all, after a build): the build empties `apps/dist/<app>` first, and a worker starting meanwhile fails with `MODULE_NOT_FOUND`. After changing an app's sources, rebuild it (`node scripts/build-apps.mjs <app>`), or its workers run the old code.

### 1.3 Relayed-service tests

The relayed services' tests (`crates/zen-svc`, `apps/echo`, `apps/mail`, `packages/svc`) run real clients, and say which one is missing:

| Tool | Used by | Install (Ubuntu) |
|---|---|---|
| `openssl` | `s_client` against the TLS engine; the test certificates (`testCerts`) | `openssl` |
| `ssh`, `ssh-keygen` | OpenSSH against the SSH engine and the `ssh()` layer; `zen-connect` as a `ProxyCommand` | `openssh-client` |
| `python3` | `smtplib` and `imaplib` against the mail app (run as `python3 -I`) | `python3` |
| `git` | `git ls-remote` against the echo app's upload-pack stub | `git` |

* **OpenSSH 9.6** (Ubuntu 24.04) has no `mlkem768x25519-sha256`. `tests/openssh_pq.rs` is ignored unless `ZEN_TEST_SSH` names an OpenSSH 9.9 or later: `scripts/build-openssh.sh <prefix>` builds 10.6p1 from source in about 10 s. CI's `openssh-pq` job does this, on demand only (`workflow_dispatch`).
* **Ports.** Each test world gives its services free ports on 127.0.0.1; nothing listens on a fixed port.
* **Takeover tests** SIGKILL handler workers and wait out a 1 s lease. A loaded machine stretches them; their bounds leave room for that, so a failure there is worth reading, not re-running.

### 1.4 Block-device tests

`crates/zen-ublk/tests/devices.rs` runs `zen-ublk` as a real block device, so it is a suite of its own behind `--features device-tests` (as FDB's tests are behind `fdb`), and runs as root:

```sh
# cargo builds as you; only the test binary runs under sudo:
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test -p zen-ublk --features device-tests,ublk --test devices
```

* **Needs:** `/dev/fuse` and `fusermount3` (`fuse3`), loop devices, `mkfs.ext4` and `e2fsck` (`e2fsprogs`); for ublk, `modprobe ublk_drv` (Ubuntu: `linux-modules-extra-$(uname -r)`) and libclang at build time.
* **Without `/dev/ublk-control`** the ublk tests say so and return; `ZEN_TEST_REQUIRE_UBLK=1` (CI's `devices` job) makes that a failure.
* **A test that panics** unmounts and detaches what it set up (its `Cleanup` guard). After a SIGKILLed run, check `losetup -a` and `mount | grep zen-ublk`.
* **The benches** (docs/STATS.md §6.3): `cargo test --release -p zen-ublk --features device-tests[,ublk] --test devices bench -- --ignored --nocapture --test-threads 1`, as root. CI's `devices-bench` job runs them when started by hand (`workflow_dispatch`).

## 2. What fills the disk

### 2.1 In production

From fastest-growing to slowest (rates and retention in STATS.md §2–3):
1. **The event log: forever.** Appends are never deleted (`TD-LOG-RETENTION`). A busy topic grows without bound.
2. **Continuous backups: forever.** FoundationDB backups keep snapshots and mutation logs until you run `fdbbackup expire`.
3. **File rewrites:** up to 2× the rewritten bytes for 24 h (the chunk grace period), more if clients don't reuse unchanged chunk ids.
4. **Deletes:** nothing is freed for about 8 days (7-day horizon, then 24 h chunk grace). Deleting to make room doesn't help quickly.
5. **Unresolved sibling versions:** concurrent writes keep every sibling until a client resolves them.
6. **The embedded file never shrinks**: freed pages are reused, but the file keeps its peak size (`TD-STORE-COMPACTION`).
7. **FoundationDB trace logs:** about 100 MB per `fdbserver` process.
8. **Replication:** `double` and `triple` store every byte two or three times across nodes.
9. **Relayed connections:** input waits in storage until a handler acks it, output until it is written. Each connection holds at most `window` plus `out_window` bytes, so a service holds at most `max_conns × (window + out_window)` per node (1,024 × 1.25 MiB, about 1.3 GB, by default), counted in the fs quota. It is freed as the bytes move: a handler that stops acking holds its connections' share until they time out.

**FoundationDB stops accepting writes when a disk is nearly full.** Everything stalls: reads and status calls time out (`fdbcli status` hangs), and transactions in flight end with `commit_unknown`. Free space and it recovers by itself within seconds. **Watch free disk, and alert well before 10 %.**

### 2.2 While developing

| Consumer | Size (measured) |
|---|---|
| `target/` with debug, clippy, `fdb`, pure and wasm builds | **20–25 GB** |
| One extra git worktree with its own `target/` | **~8 GB** per worktree |
| Release builds of all variants | ~1.5 GB |
| A FoundationDB test cluster | ~300 MB data + ~110 MB trace logs |
| Each `scripts/install-fdb.sh` test install | 144–277 MB |

* **Use `CARGO_INCREMENTAL=0`**: incremental caches are large and rarely worth it with LTO-free debug builds of this size.
* **Delete `target/debug/incremental`** when space runs low; it is the first thing to go.
* **Stale test executables pile up in `target/debug/deps`**: each integration test binary is ~200 MB, and every feature set or dependency change links a new one beside the old. Deleting the ones not built today (`find target/debug/deps -maxdepth 1 -type f -perm -u+x ! -name '*.*' ! -newermt today -delete`) freed 13 GB once; cargo relinks what it needs.
* **Don't build in several worktrees at once**; each one needs its own ~8 GB `target/`.
* **A full disk kills the FoundationDB test cluster**, and the next test run then **hangs silently** on a database that isn't there (the test process sits idle at 0 % CPU). Check `df` and `fdbcli status` before suspecting the code.

## 3. FoundationDB test cluster pitfalls

* **It dies quietly** (full disk, container restart). A hung `cargo test --features fdb` with no CPU use almost always means the cluster is down. Restart it with `zen-serve serve -c node.toml` (or `init` for a new one) and wait until `fdbcli --exec status` shows Healthy.
* **`advance_version` (export/import tests, `zen-serve migrate`) forces a recovery.** Parallel tests then see `commit_unknown` or `too_old`. Every transaction that creates or reads state at start-up must retry those (the `txn_loop!` macro, with `idempotent` for create-if-absent writes).
* **The version clock lags on an idle cluster** (up to ~2 s per step). Tests that wait for a lease, a claim or a sweeper age must **poll with a deadline**, never sleep a fixed time.
* **Never `pkill -f zen-serve` or `pkill -f cargo`**: it can match your own shell. Kill explicit PIDs.

## 4. Advice for the next milestones: don't pay for slow paths you don't need

The milestones so far spent most wall-clock time on rebuilds, full test matrices and a dead test cluster, not on code. To keep the next ones fast:

1. **Iterate on the embedded backend only.** Every server test runs on both backends with the same semantics. Run the FoundationDB suite **once, before the final commit** of a piece of work, and let CI's `fdb` job be the gate.
2. **Run only the tests you touch:** `cargo test -p zen-server --test fs`, or a single test by name. The full workspace suite takes minutes; one test file takes seconds once built.
3. **Let CI cover the feature matrix.** Locally, check the default build. CI runs the pure-Rust build, the no-C-compiler check and FoundationDB in parallel, in 2–4 minutes. Running all five clippy variants locally costs far more.
4. **Use `cargo check` while writing code**, not `cargo build` or `cargo test`. Build once you need to run something.
5. **Never build release while developing.** LTO makes every release rebuild ~2 minutes. Measure sizes once, at the end.
6. **Keep sweeper and lease tests short:** set tiny horizons, grace periods and sweep intervals in the test config (existing tests use 1–8 s), and poll for the result.
7. **Keep test Argon2id at the floor** (64 MiB, t=1). Production parameters (1 GiB) would make every sign-in test take seconds.
8. **Reuse expensive test fixtures:** generated RSA keys, TLS certificates and FoundationDB clusters. Never generate them per test.
9. **One working tree, one `target/`.** Parallel agents or worktrees each pay for a full build and ~8 GB of disk. If work must run in parallel, give it separate files and let one build verify both.
10. **Check the environment first when something hangs:** free disk, then the FoundationDB cluster, then the code.
11. **The client milestones (4–5) don't need FoundationDB at all.** Iterate with `npx vitest run packages/client/test/<file>`: one file spawns its own embedded server in milliseconds. Develop the client library against an embedded server. The server already behaves identically on both backends, which its tests prove.

## 5. Integrity pitfalls

Integrity is described in [`docs/INTEGRITY.md`](../docs/INTEGRITY.md); these are the ways to get it wrong in production.

* **Keep the pins.** A device's pins (the TS client's `local` store, the Rust client's pins file, zen-ublk's volume pin) are what it verified. Lose them and the next run trusts whatever the server shows at its first sync, and a rollback to before that sync goes unnoticed. Back them up with the device key, keep them on persistent storage (not a container's ephemeral disk), and never delete them to silence an alarm: they are the evidence.
* **An authenticated namespace serializes its writers.** Every commit rewrites its NsRoot, so concurrent writers of one authenticated namespace conflict and retry (zendb.md §14.5). Put balances, permissions and other state where freshness matters there, and keep high-rate, independent writes (logs, chat messages, telemetry) in a basic namespace.
* **`sigEvery: 1` in a browser** signs a checkpoint after every change: 2.6 ms of WASM per change, plus a put of about 4.4 KB (docs/STATS.md §6). Natively it's 0.2 ms. Use it for handlers and volumes that need the window closed, and the default cadence elsewhere.
* **Checkpoints count toward the fs quota:** up to `checkpoint_keep` (1,024) per device, about 4.5 MB per device that keeps writing. Lower `limits.checkpoint_keep` for fs with many devices and small quotas; a verifier needs only each device's newest.
* **`adversary = true` is for tests only.** The server refuses it unless built with `test-utils`, and production builds shouldn't have that feature.
