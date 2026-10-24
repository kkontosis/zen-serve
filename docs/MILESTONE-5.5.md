# Milestone 5.5 plan: relayed services (blind TCP relay, handlers on member clients, seamless takeover)

## Context

zen-serve can't run classic internet protocols (git over SSH, inbound mail, IMAP, TLS services) itself, because it never sees plaintext. Milestone 5.5 makes the server a **thin, blind TCP relay**:
* zen-serve accepts connections on public ports and stores each connection's bytes in a durable, ordered, per-connection queue.
* **Handlers** run on authenticated member clients (Node workers or browser tabs) that hold the fs keys. A handler claims a connection the way a consumer claims a key, reads its input, writes its output and serves the protocol.
* Several handlers share a service, as consumers share a group.
* **When a handler dies, another one continues the same live connection.**
* The step that consumes input and produces output commits in the **same transaction** as the handler's zen-db rows and broker events.
* Data isn't retained: input is deleted once acked, and output once written to the socket.

This is a sibling of the broker. It reuses the claim, fencing and commit machinery, with its own data path.

**User decisions:**
* **Placement:** a full plan now, built **before milestone 6**, as milestone 5.5 (`docs/MILESTONE-5.5.md`), so milestones 6–11 keep their numbers.
* **Mid-connection takeover:** built and **on by default** (opt-out per service), also for TLS and SSH sessions that end in the handler.
* **`https-auth` dropped:** the server never terminates a client's TLS. The modes are `tcp` (public) and `tcp-auth` (a WebSocket on zen-serve's own TLS, with a zen token).
* **Certificates and host keys:** brought by the admin, stored sealed in the fs; ACME is tech debt (TD-SVC-ACME).
* **Git over SSH:** the SSH transport, complete and tested with OpenSSH (exec, shell, subsystem, rekey, takeover), plus an exec demo; real `git-upload-pack` and `receive-pack` come with milestone 10.
* **TLS 1.2 is included** (some inbound MTAs), next to 1.3, resumable the same way.

**Defaults I chose (overridable):**
* Takeover TTL 5 s, with 1 s heartbeats on the handler lease.
* Relay bytes held in storage count against the fs quota; per-service caps in the config.
* Resume on: an output step costs a commit plus a reveal, about 6–10 ms locally.

## Progress and resuming

**Rhythm (as milestone 5):** one sub-step per PR. Before each one: restart from main if the previous PR is merged, plan the sub-step in detail here, and commit the plan. On the user's "go": a `spec:` commit if needed, then code and docs. Log the step in both plan copies (this file and `docs/MILESTONE-5.5.md`, kept equal with `cp`), push, confirm CI is green, stop for compaction.

**Branch:** `claude/brave-knuth-gk03mm`. Push with retries (2, 4, 8, 16 s). Restart from main: `git fetch origin main && git checkout -B claude/brave-knuth-gk03mm origin/main`, then push `--force-with-lease`.

**Constraints:** no model names in the repo; commit trailers as before; `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push; about 10 GB of disk free (prune stale `target/debug/deps` executables when low). The local FDB cluster for tests: `./target/debug/zen-serve serve -c /tmp/zen-ci/node.toml` (built with `--features fdb`), cluster file `/tmp/zen-ci/data/fdb.cluster`.

**Log:**
* **Step 0** (plan): done. Restarted from main after PR #35 (milestone 5 step 11) merged; wrote `docs/MILESTONE-5.5.md` (this plan), added 5.5 to DESIGN-3 §6 and the README status; commit `docs: milestone 5.5 plan (relayed services)`. Next: plan step 5.5.1 (spec) in detail, on "go".
* **Step 5.5.1** (spec): done, PR #37. Commits: `docs: milestone 5.5 step 1 plan (spec)`; `spec: relayed services (svc.md, api §14, keyspace §3.9)`; `spec: seal kinds 9 and 10, output records, sealed inbox`; `core: svc seals and vectors`; the docs and this log.
  * Spec as planned, plus details settled while writing:
    * `rz` abort requests (reasons 1 bad record, 2 not resumable, 3 released with close) let any node abort; the owner resets and reaps.
    * A closed socket is reaped at once (no lingering `peer_gone` or RST flags): `rh` flags are EOF only, and `rw` is `written_to` only.
    * `ra` adds `u32 state_len`, for quota.
    * `rq(fs, svc, vs) = conn`; a queued `rm` carries its `rq` versionstamp.
    * `rli` is the lease's input log for stream pushes.
    * The state rule: once a connection has a state, a cursor-moving step must carry a new one, because the AAD binds the cursors.
    * Record flags are 1 FIN, 2 close, 4 plain, 8 abort; FIN still lets a handler ack input.
    * `/v1/svc/state` re-syncs a holder; `Claim.flags` = EOF | `ra` flags << 1.
    * Delegation carries `K_svc`, or `D_svc`, per epoch.
  * zen-core `svc.rs`:
    * `SvcKeys`
    * `seal_state`/`open_state`, `seal_record`/`plain_record`/`parse_record`/`open_escrow`/`reveal`
    * `inbox_public`/`inbox_encaps`/`inbox_decaps`/`InboxKey`
  * `spec/test-vectors/svc.json` and its file test; 5 unit tests.
  * Docs: spec README, glossary, TD-SVC-STATE-ROLLBACK.
  * (planned next) step 5.5.2 (server relay core, input side) on "go", after PR #37 merges.
* **Step 5.5.2** (relay core, input side): done, PR #38. Commits: `docs: milestone 5.5 step 2 plan`; `server: relayed services, input side (config, accept, leases, claims, read)`; the docs and this log.
  * As planned, plus details settled while building:
    * The reader caps each batch at the window's remaining room, so `in_head − in_acked` never exceeds `window`.
    * Input held back by the quota or a storage error stays in memory, and nothing more is read until it's stored.
    * An ended lease's doorbell `rld` is deleted (which wakes its watchers) rather than rung. A reap rings a holder's doorbell only while the lease exists.
    * `reap_node` stops if the node renews meanwhile; the renewer then sees the lapse and aborts its sockets.
    * `per_ip_rate = 0` turns the bucket off; admission refusals reset the socket.
    * `Svc::local_addr(name)` gives the bound address, so tests listen on port 0.
  * Docs:
    * operations.md §4: every node needs the same service table, and the ports must be reachable.
    * OPS.md §2.2: stale test executables in `target/debug/deps`.
  * Verified: fmt; clippy default, `--all-features` and `--no-default-features`; `tests/svc.rs` (9 tests) 3× on embedded and on FDB; the full zen-store and zen-server suites on FDB and embedded.
  * CI fix: CI's newer stable toolchain deprecates `AtomicUsize::fetch_update` (renamed `try_update`), and `-D warnings` failed the Rust jobs on it. `Service::open` now uses `fetch_add`, then `fetch_sub` when over `max_conns`. Commit `server: max_conns count without fetch_update`.
* **Step 5.5.3** (the step, reveal, the writer): done, PR #39. Commits: `docs: milestone 5.5 step 3 plan`; `spec: svc step and writer details`; `spec: an empty end record sits at out_head`; `server: svc commit step, reveal, writer, timers`; the docs and this log.
  * As planned, plus details settled while building:
    * An empty record (FIN, close or abort alone) sits at the new `out_head` itself. So `pending`, reveals and the writer look at records from `written_to` to the end of `ro`, not only below `out_head`. The writer acts on such a record without clearing it; the reap does.
    * A sealed empty record still needs its reveal before the writer acts on it, like any other sealed record.
    * zen-core gains `open_key_escrow`: a claim lists bare escrows, and `open_escrow` needed a whole record.
    * A committed step also wakes a local connection at once (the same fast path as a reveal); other nodes are woken through their doorbell.
    * The writer keeps its own `written_to` after the first pass. Advancing `rw` after a write retries until it applies, and after `commit_unknown` it re-reads `rw`, so the quota is never given back twice.
    * A close record shuts down the write side before draining input for 1 s, so the peer sees EOF at once.
    * A step's 400 checks that depend on the service (`resumable`, sealed or plain records) run both in `validate`, against the config, and in the step, against `rc`.
  * Verified:
    * fmt; clippy default, `--no-default-features` and `--all-features`
    * `tests/svc.rs` (18 tests) 3× on embedded and once on FDB
    * `tests/multinode.rs` on FDB, including the new two-node echo
    * the full zen-core, zen-proto and zen-server suites on embedded
    * zen-server on FDB: all passed but `dump.rs` `a_started_server_counts_as_empty`, which once got `too_old` in `dump::is_empty` (two one-item range reads, so a stalled transaction on the loaded local cluster); `tests/dump.rs` then passed 3× on FDB
    * the dev wasm build declares `SvcStep` for `Commit.svc`
* **Step 5.5.4** (stream push): done, PR #40. Commits: `docs: milestone 5.5 step 4 plan`; `spec: svc stream push details`; `server: svc stream push (svc_sub, svc_in, svc_reveal)`; `server: an import target with relay keys counts as empty`; the docs and this log.
  * As planned, plus details settled while building:
    * `svc_sub`'s checks (the service, `consume`, the lease) run before the `ok`, so a refusal is an `err` with the subscription's `id` and nothing is spawned.
    * A subscription keeps each doorbell watch armed until it fires, rather than re-arming every turn, so a queue ring is never lost while input is being pushed. With input left over (more than `max_range_bytes` for a connection, or 1,000 log entries), the next turn starts at once, and a fired queue watch still sends its `svc_ready`.
    * The rescan reads the log's newest key before paging `rx`, so input logged during the rescan is read on the next turn.
    * A socket runs at most 8 `svc_reveal` requests at once; more wait, which slows the socket's reads.
    * `log_input` in `svc/mod.rs` writes an `rli` entry; the reader and `claim_some` share it.
  * Measured locally (embedded): push latency from the peer's write, worst of 10, about 10 ms; 100 connections' lines pushed in about 120 ms.
  * Verified:
    * fmt; clippy default, `--no-default-features` and `--all-features`
    * `tests/svc.rs` (24 tests) 3× on embedded and once on FDB; `tests/multinode.rs` and `tests/dump.rs` on FDB
    * the full zen-server suite on embedded and on FDB, and zen-proto
    * the dev wasm build (the `Frame` type gains the five variants) and `npm run check`
  * CI green on PR #40's head (all 8 checks).
* **Step 5.5.5** (`@zen/svc`, raw): done, PR #41 (the plan and the CI fix) and PR #42 (the code). Restarted from main after PR #41 merged. Commits: `docs: milestone 5.5 step 5 plan`; `db: a delivery whose commit failed on the network is retried, not nacked`; `wasm: svc seals and the svc request codecs`; `client: svc_sub on the stream; a svc step makes a transaction non-empty`; `svc: @zen/svc, the raw handler runtime`; `apps: echo, a relayed line service`; the docs and this log.
  * As planned, plus details settled while building:
    * The stream's svc subscription is `Stream.subscribeSvc(fs, svc, lease, {onEvent, onError})`, delivering `SvcPush` items (`input` or `ready`) like `subscribe`, rather than its own `{input, ready, end}` callbacks.
    * `@zen/client` gains `Service` (`fs.service(...path)`): the `/v1/svc/*` requests, and the service keys per epoch (`keysAt`, `keysFor`, `sealKeys`). The plan had these inside `@zen/svc`; the client tests need them too.
    * The flag constants (`SVC_FIN`, `SVC_CLOSE`, `SVC_ABORT`) live in `@zen/svc`, not the wasm. `peekSealed` reads an escrow's epoch (tested), so no `escrowEpoch` binding.
    * A fresh connection (no step yet) gets a first step even without input, for protocols where the server speaks first. On a `resume` service that step carries `resumable`, so each connection's first step commits.
    * A connection is forgotten after a close or abort record, or after its FIN once the peer's EOF is in and all input is acked. The server reaps a connection without telling its holder, so one quiet for 10 s is checked with `/v1/svc/state`, which frees its slot when it is gone.
    * The test server restarts in place (`TestServer.restart`), with `[[service]]` tables from `serviceToml`; `startWorld({services})` uses it. Members get `consume` on `svc/` through the app's grants, not a harness default.
    * The echo table has an index on `(conn, n)`, so listing lines doesn't scan.
    * `resume: false` test: the server's sweeper runs every 60 s, so the test starts a second handler, whose lease calls reap the lapsed lease. It never receives the connection, which is aborted.
  * CI fix done before "go": one `client` run on the plan commit failed the kanban offline test (the index didn't follow a move within 15 s). Commit `db: a delivery whose commit failed on the network is retried, not nacked`, as in "CI fix first" below.
    * `packages/db/test/network.test.ts` failed on the old code (the second event of the key didn't arrive within 10 s) and passes with the fix in about 3 s.
    * Verified: `npm run check`; `packages/db` and `apps/kanban`; the kanban offline test 5×; the whole vitest suite 3×.
    * Seen once, in the first of those three full runs: the kanban test "deleting a card moves its attachment directory to TRASH" failed an assertion in 55 ms. It involves no consumer and no network error, so this fix doesn't reach it. It didn't recur in 3 kanban-only runs, 4 parallel runs of that test, or the 2 other full runs. Not yet investigated: watch CI for it.
  * Verified:
    * `cargo fmt --check`; clippy on zen-wasm (wasm32, `-D warnings`); the dev wasm build
    * `npm run check`, and `tsc -p packages/client/tsconfig.test.json`
    * `npx vitest run` 3× (437 tests, all passed each time), including the new `packages/client/test/svcvectors.test.ts` and `svc.test.ts`, `packages/svc/test/input.test.ts` and `apps/echo/test/echo.test.ts`
    * `npx playwright test` (smoke and apps, 14 specs), including `apps/echo/e2e/echo.spec.ts`
    * `cargo test -p zen-server --test svc` (24 tests; the server is unchanged)
  * CI green on PR #42's head (all 8 checks); merged.
* **Step 5.5.6** (the shared TLS provider crate): done, PR #43. Restarted from main after PR #42 merged. Commits: `docs: milestone 5.5 step 6 plan`; `tls: the RustCrypto rustls provider in its own crate (zen-tls-rustcrypto)`; the docs and this log.
  * As planned, plus details settled while building:
    * On wasm32, rustls's `std` feature needs a clock (`UnixTime::now`), which rustls-pki-types has there only with its `web` feature (web-time, JavaScript's `Date.now`). The crate turns it on for wasm32, next to getrandom's `wasm_js`. So rustls's default time provider works in the wasm build too.
    * One moved unit test built an `Iv` with `Iv::new`, which rustls has only with its `tls12` feature; another workspace crate turned that on, so it built inside zen-server and not alone. It uses `Iv::from` now.
    * zen-server drops aes-gcm, hmac, ml-kem and x25519-dalek; p384 becomes a dev-dependency (`tests/common/pki.rs`); chacha20poly1305 stays (the sealed OPAQUE login state).
    * `webauthn::soft::rsa_key` calls `rsakey::testing::rsa_key` (same label, so the same keys); `tls/ring.rs`'s own `rsa_key_with_exp` stays.
  * Docs: the README crates table, TECH_DEBT (the hybrid and provider-review entries), operations.md §8's provider table, this plan's survey line.
  * Verified:
    * `cargo fmt --check`; clippy `--all-targets -D warnings` default, `--no-default-features` and `--all-features`; clippy on the crate alone, native and wasm32
    * `cargo test -p zen-tls-rustcrypto` (10 tests: the provider's 6 and the key policy's 4)
    * `tests/tls.rs` and `tests/mtls.rs` unchanged: default (14 and 8) and `--no-default-features` (10 and 8)
    * the full zen-server suite (16 test binaries, 178 tests)
    * `cargo check -p zen-tls-rustcrypto --target wasm32-unknown-unknown`, also with `test-utils`
    * `cargo tree -p zen-server --no-default-features -i ring` is empty
    * `npx vitest run apps/echo packages/client/test/mtls.test.ts` against the rebuilt binary (6 tests)
  * CI green on PR #43's head (all 8 checks); merged.
* **Step 5.5.7** (the TLS engine, native): done, PR #44. Restarted from main after PR #43 merged. Commits: `docs: milestone 5.5 step 7 plan`; `spec: the TLS engine (svc.md §7.1)`; `tls: TLS 1.2 suites and a record API in zen-tls-rustcrypto`; `svc: the TLS engine (crates/zen-svc), native`; the docs and this log.
  * As planned, plus details settled while building:
    * The 1.2 AES-GCM explicit-nonce prefix is drawn for each batch of records the engine sends (a write, a close, an alert), not once per connection, and isn't in the state. With the record IV `salt ‖ prefix ‖ 0⁴` XOR the sequence number, the explicit nonce is the prefix and the low 32 bits of the sequence number. svc.md §7.1 and §12 say "per batch".
    * The handshake feeds rustls one record at a time. A header rustls would refuse (unknown type, too long) is handed over alone, so it fails at once instead of waiting for a body.
    * After a handshake error the engine drops rustls's input buffer and takes the alert rustls queued. Re-parsing the bad bytes would make rustls send a second fatal alert, which it debug-asserts against.
    * `UnbufferedServerConnection` has no SNI accessor: a per-connection certificate resolver records the client's server name, next to the per-connection key log.
    * `Conn::restore(config, state)` takes the config (the test-only key-limit knob lives there). `Session::restore` and `Conn::from(Session)` exist too; `Conn::session()` gives the `Clone`able session.
    * A peer's KeyUpdate requests are answered once per input call, in that call's output.
    * `Progress.truncated` is false while the engine holds back records because of the plaintext bound.
    * The test certificates are `zen_tls_rustcrypto::testing` (feature `test-utils`, rcgen on RustCrypto keys), so zen-svc needs no rcgen of its own.
    * `record` also has in-place opens (`open13_in_place`, `open12_in_place`), which the provider's decrypters use. `RecordKey` is `Clone`.
    * OpenSSL findings:
      * In 1.2 the certificate's curve must be among the client's groups (RFC 8422 §5.1), so the 1.2 `s_client` runs use `-groups X25519:P-256` and `P-256`.
      * `s_client` takes a stdin read that starts with Q, R, k or K as a command, so the large-transfer test sends digits.
      * OpenSSL 3.0 takes `Options = -ExtendedMasterSecret` from `OPENSSL_CONF`, so the EMS refusal is tested against OpenSSL itself.
      * OpenSSL answers the engine's `no_renegotiation` warning with a fatal `handshake_failure`; the test checks exactly that.
  * Verified:
    * `cargo fmt --all --check`; clippy `--all-targets -D warnings` default, `--no-default-features` and `--all-features`; clippy on zen-svc and zen-tls-rustcrypto for wasm32
    * `cargo test -p zen-tls-rustcrypto --all-features` (14 unit tests, including RFC 8448 and independent known answers, and 3 handshake tests)
    * `cargo test -p zen-svc` 3× (23 rustls-client tests and 6 `s_client` tests)
    * zen-server `tests/tls.rs` and `tests/mtls.rs`, default (14 and 8) and `--no-default-features` (10 and 8), alone and built together with zen-svc (rustls's `tls12` unified)
    * `cargo check -p zen-svc --target wasm32-unknown-unknown`; `cargo tree -p zen-svc -e normal -i ring` is empty
  * **CI fix:** on head 73404e6 the `rust` job (both runs) and one `pure-rust` run failed `crates/zen-svc/tests/openssl.rs` `s_client_large_lines` at `assert!(c.stdout.contains(&want))`. Commit `svc: s_client tests wait for the echo before closing stdin`.
    * The assertions before it passed: the engine received all 120,002 bytes and echoed them. Only `s_client`'s stdout was short.
    * Root cause, a race in the test: the `s_client` helper closed stdin 300 ms after the last line, and `s_client` shuts the connection down as soon as it reads EOF on stdin, dropping echo still in flight. 300 ms was enough for 120 KB locally, not on the CI runners.
    * The failure didn't reproduce locally, even on one loaded CPU. The mechanism was confirmed against OpenSSL directly: with a Python TLS echo server that waits 1 s, `s_client` with stdin closed after 0.3 s printed no echo, and with stdin open for 2 s it did.
    * Fix: a thread reads `s_client`'s stdout as it arrives, and the helper keeps stdin open until stdout holds `wait_for` (or 30 s pass). The data tests pass their expected echo; the renegotiation and no-EMS tests pass `None`.
    * Verified: clippy on zen-svc; `cargo test -p zen-svc --test openssl` 5×, and 3 rounds of three copies at once.
  * CI green on PR #44's head (all 8 checks); merged.
* **Step 5.5.8** (`@zen/svc-wasm` and the `tls()` layer): done, PR #45. Restarted from main after PR #44 merged. Commits: `docs: milestone 5.5 step 8 plan`; `svc: the TLS engine for wasm (crates/zen-svc-wasm, @zen/svc-wasm)`; `svc: the tls() layer in @zen/svc`; `apps: echo over TLS`; `svc: early input is bounded by its span, and a gap is read again`; the docs and this log. No `spec:` commit: svc.md §7.1 already says what the runtime does.
  * As planned, plus details settled while building:
    * wasm-bindgen exports no constants, so the binding has `plainHeld()`. `ZenTlsError`'s message is its code, with the details in `detail`. A connection's failure in `input` is `{code, alert?, detail}`, with the codes `tls`, `alert_sent`, `alert_received` and `internal` besides the thrown ones. `version` is `TLSv1.3` or `TLSv1.2`, and `suite` the IANA name (rustls's `TLS13_…` becomes `TLS_…`), so both compare with Node's `getCipher().standardName`.
    * The layer interface (`packages/svc/src/layer.ts`): `prepare` (outside the transaction), `finish` (inside, may run again), `committed`, `room` (how much plaintext a step may write so its records fit `out_window`). The prepared step is kept until it commits, for established connections too, so a `window` retry or a conflict reuses it.
    * The handler runs on the step that finishes the handshake even without plaintext, so a server-first protocol (IMAPS, SMTPS) can greet; then on new plaintext, or an EOF it hasn't seen. After its own FIN, an EOF with nothing to read doesn't call it.
    * A claim of a TLS connection with no state whose cursors moved (a handshake begun elsewhere) is released with `close`, like a state that doesn't restore.
    * In TLS 1.2 a write that reaches the key's record limit closes the connection (close_notify, then close).
    * `serve()` opens the layer before anything else: a bad certificate is one `onError` (`ZenTlsError: config`), and nothing is claimed.
    * `testCerts(kind)` is in `@zen/client/testing` rather than `apps/shared`, so the svc package's tests use it too.
    * The echo app: `serveEcho(db, opts)` takes any `ServeOptions` (handler and layer); `tlsLayer(db, name)` reads a `certs` row; the worker takes `ECHO_LAYER` = `raw`, `tls` or `mirror`. The Playwright case puts the certificate through a Node session of Alice, and the tab reads it.
    * Node's TLS client takes 1.3 suites in `ciphers` (there is no `ciphersuites` option), and `secureConnect` fires before the engine has read the client's Finished, so the in-memory tests wait for the engine. Node has no client KeyUpdate API: the binding tests send the server's KeyUpdate both ways; client KeyUpdates were tested against `s_client` natively in 5.5.7.
    * Sizes (release, no wasm-opt): `zen_svc_wasm_bg.wasm` 1.05 MB, 403 KB gzipped; `zen_wasm_bg.wasm` is now 2.42 MB, 645 KB gzipped (STATS §4.1 said 2.14 MB).
    * Measured locally: 20 MB through `mirror-tls` with two kills in about 14 s.
    * **A runtime bug from 5.5.5, found by the 20 MB test:**
      * In one of three full vitest runs, the transfer stalled after the first kill, at about 36%, with no error on either side.
      * Reproduced about once in ten with three copies of the test running at once. The workers' debug output showed the successor dropping a pushed chunk before its claim response arrived. Input pushed early was kept up to `window` bytes, counted as the sum of the chunks. But pushes repeat bytes and here started 352 bytes before the claim's `in_acked`, so the sum passed 256 KiB with a full input window. The dropped chunk left a gap no later push filled.
      * Raw handlers had the same bug; their takeover tests never had a full window at the kill.
      * Fix: the early buffer is bounded by the offsets it spans (twice the window), and input that has waited past a gap for over a second is read again with `/v1/svc/read`.
      * After the fix, 46 runs of three copies at once all passed. Two more runs failed only because the copies rebuilt `apps/dist/echo` at the same moment.
      * The test now times out its sender and reports the workers' output.
  * Verified:
    * `cargo fmt --all --check`; clippy on zen-svc-wasm, `--all-targets -D warnings`, default and `--no-default-features`, and for wasm32; `cargo test -p zen-svc-wasm`
    * `scripts/build-wasm.sh`, release and `--dev`
    * `npm run check`
    * `npx vitest run` 3× after the fix: 458 passed each time; the 7 skipped are the opt-in benches. This includes `packages/svc/test/tlswasm.test.ts` (15 tests) and `apps/echo/test/tls.test.ts` (6 tests).
    * `npx playwright test`: smoke and apps, 15 specs, including the TLS case in `apps/echo/e2e/echo.spec.ts`
    * `cargo test -p zen-svc` (unchanged: 23 and 6 tests)
  * **CI fix:** on head a041fc4 both `client` runs failed. Vitest passed; then Playwright stopped before any test with `Cannot find module …/node_modules/@zen/db/dist/index.js imported from packages/svc/src/state.ts`. Commit `apps: the echo e2e spec doesn't load @zen/svc`.
    * Cause: the new TLS case in `apps/echo/e2e/echo.spec.ts` imported `apps/echo/src/echo.ts`, which imports `@zen/svc`.
    * Playwright maps `@zen/svc` to its sources through `apps/tsconfig.json`. Imports inside `packages/svc/src` resolve against that package's own tsconfig, which has no paths, so `@zen/db` resolves to its build. CI doesn't build `@zen/db`; locally a stale build hid this.
    * Fix: the database parts (`NS`, `DB_OPTIONS`, `Line`, `Cert`, `putCert`) move to `apps/echo/src/schema.ts`. `echo.ts` re-exports them, and the spec imports `schema.ts`.
    * Verified with the local `packages/db/dist` moved aside, as in CI. The error reproduced before the fix; after it, `npx playwright test` (15) passed, and so did `npx vitest run apps/echo` (11) and `npm run check`.
  * CI green on PR #45's head (all 8 checks); merged.
* **Step 5.5.9** (the SSH engine, native): done, PR #46. Restarted from main after PR #45 merged. Commits: `docs: milestone 5.5 step 9 plan`; `spec: the SSH engine (svc.md §7.2)`; `svc: the SSH engine (crates/zen-svc), native`; `ci: OpenSSH from source for the hybrid key exchange`; the docs and this log; `ci: the OpenSSH-from-source job runs on demand only`.
  * Decided with the user: implement `mlkem768x25519-sha256` too. OpenSSH has it only from 9.9, and Ubuntu 24.04 (here and on the runners) ships 9.6. So a CI job builds OpenSSH 10.6p1 from source to test the hybrid against it. Once that job has passed on the PR, it is switched to run on demand only.
  * As planned, plus details settled while building (all in svc.md §7.2):
    * **Bounds, sized to the state budget.** The plan's per-channel figures would allow 10 × 32 KiB of queued output, beyond `max_value_bytes`. So the bounds are across channels:
      * Unread data: at most 32,768 bytes. A packet is read only when nothing is unread, or when what is unread plus the packet's length stays within that.
      * Queued output: at most 16,384 bytes, in at most 64 pieces.
      * Held replies during the server's own key exchange: at most 32 messages and 4,096 bytes.
      * A full state measured about 47 KB; the OpenSSH transfers peaked at about 34 KB.
    * **Packets up to 35,000 bytes** (RFC 4253 §6.1's minimum), not 256 KiB: a packet larger than the relay's input window could never be read. The client's KEXINIT is limited to 8,192 bytes.
    * **A server rekey behind unread data.** From its KEXINIT to its NEWKEYS the server holds its output. An application whose writes are full then stops consuming, and the read bound stops reading, while the client's KEXINIT and init sit behind channel data in the input: a deadlock. So when reading is held back during its own key exchange, the engine decrypts ahead through the input it was given, without consuming it. It handles the client's KEXINIT and init there, replies, sends NEWKEYS, and skips those two sequence numbers when in-order reading reaches them (`kex.ahead` in the state). The OpenSSH server-rekey test goes through this path (4 of its 6 rekeys).
    * **The channel window** is an engine option, `window`, default 98,304 bytes (the plan said 2 MiB). That read-ahead needs the client's data in flight to fit the relay's input window, so the channels' windows together, plus 64 KiB, should fit the service's `window`. 10 × 98,304 + 64 KiB is about 1 MiB, the window SSH services should use; 5.5.10's layer derives the option from the service.
    * **Only the request kind is kept** for each channel's exec, shell or subsystem. The command or subsystem name reaches the application once, in its event, so a long command doesn't stay in the state.
    * Other details:
      * Without strict kex, an unexpected message during a key exchange gets UNIMPLEMENTED, as OpenSSH does.
      * The state counts completed key exchanges (`kexes`).
      * A wrong guess (`first_kex_packet_follows`) skips the client's next packet, in order and in the read-ahead alike.
    * **The test client** (`ssh::client`, feature `test-utils`) is built on the engine's own wire, cipher and kex code, and checks the host-key signature. It holds its own messages during its key exchange, as RFC 4253 §7.1 requires.
    * The engines' strict CBOR reading (`Fields`) moves from `tls/state.rs` to a shared `cbor` module.
  * Verified:
    * `cargo fmt --all --check`
    * clippy `--all-targets -D warnings` on the workspace, default and `--no-default-features`; on zen-svc with `--all-features`, and for wasm32
    * `cargo check -p zen-svc-wasm --target wasm32-unknown-unknown`; `cargo tree -p zen-svc -e normal -i ring` is empty
    * `cargo test -p zen-svc` 3×:
      * 11 unit tests
      * `tests/ssh.rs` (18): sessions in whole reads and one byte at a time for every method and cipher, strict kex both ways, a wrong guess, the auth limit, a question across a restore, the read and write bounds, rekeys from either side, channel refusals, 300 malformed messages, bit flips, the version line, the state's checks
      * `tests/openssh.rs` (10) against OpenSSH 9.6 (apt): both ciphers, RSA keys with SHA-2 and SHA-1 refused, an unauthorized key, 10 MB `cat` with `RekeyLimit=1M` (about 20 client rekeys), server rekeys, a 20 MB download, a shell, a subsystem, an exit status, 7-byte steps, keys read and fingerprinted as `ssh-keygen` does, and Terrapin through an injecting proxy
      * the TLS tests unchanged
    * Against OpenSSH 10.6p1 built with `scripts/build-openssh.sh`: `tests/openssh.rs` and `tests/openssh_pq.rs` (the hybrid with both ciphers, a 1 MB `cat` with rekeys both ways)
  * CI green on PR #46's heads 08641d2 and 145c9b8 (all 10 checks, `openssh-pq` included: the OpenSSH build took 36 s); merged.
  * The PR merged before the switch to on-demand was pushed. So that commit, `ci: the OpenSSH-from-source job runs on demand only`, is the second commit of step 5.5.10's plan PR.
* **Step 5.5.10** (the `ssh()` layer): done, PR #47. Restarted from main after PR #46 merged. Commits: `docs: milestone 5.5 step 10 plan`; `ci: the OpenSSH-from-source job runs on demand only`; `svc: the SSH engine for wasm (@zen/svc-wasm)`; `svc: layers make their handler's step; the ssh() layer`; `apps: echo over SSH, and a git-upload-pack stub`; the docs and this log. No `spec:` commit: svc.md §7.2 already says what the runtime does, the window rule included.
  * As planned, plus details settled while building:
    * **The engine's `Progress` gains `held`:** reading stopped with complete input left, because a question waits for its answer or unread data or held replies reached their bound. The layer's `again` rule uses it. Without it, any partial packet left over would look the same as input held back. `tests/ssh.rs` checks it.
    * **The binding** gives the client's `exec` command as bytes; `SshStep`'s events decode it as UTF-8. `SshConfig.hostKeyLine` and `SshConn.ciphers` were added for tests. The module loader moved to `packages/svc/src/wasm.ts`.
    * **Layers open with the service's settings.** `Layer.open(info)` gets `/v1/svc/info`, which the SSH window default needs. So `serve()` reads the info before opening the layer (and before the stream). A bad host key or certificate is still one `onError`, with nothing claimed.
    * **`serve()` has two overloads.** One takes a layer and infers the step type from it; one has no layer and gives `RawStep`. A single generic signature inferred `unknown` for an untyped handler without a layer.
    * **`Live.dispose()`** frees an attempt's engine when the handler throws, as well as after `finish()`.
    * **`SshStep`:**
      * A question the handler leaves unanswered is refused at the end of the step. A client waiting for an answer would otherwise wait for input that never comes.
      * The handler isn't called once the engine has ended. The client's DISCONNECT (how OpenSSH ends every session) isn't reported as an error; a protocol error is.
    * **Room per step:** `out_window`, minus the engine's own output from the input, minus 1,024 bytes kept for window adjusts, EOF, exit and CLOSE. Each write is offered 32,768/32,832 of what is left, the packet overhead.
    * **Test helpers:** `testSshKeys()` and `runSsh()` are in `@zen/client/testing`. `known_hosts` names the host key by `HostKeyAlias`, so one file serves any port.
    * **The echo app:**
      * schema 3 adds `ssh_keys`, keyed by `(user, fingerprint)`; the fingerprint is computed with WebCrypto in `schema.ts`, so the Playwright spec needs no WASM
      * the host key is the `certs` row `ssh` (`key` the PEM, `chain` the public line)
      * a shell runs `lines`; a subsystem exits 127
      * output the client's window doesn't take is kept in the channel's state, hex
      * the git stub advertises protocol v0 (git sends `GIT_PROTOCOL` through `env`, which the engine refuses, so v0 it is)
    * **A layer-level test** (`packages/svc/test/sshlayer.test.ts`) drives `ssh()` as `serve()` does, without a server, against OpenSSH. It checks each step's output against a 64 KiB `out_window` and that cut steps run again (a 3 MB download), held input is read after acks (2 MB `cat` with rekeys), and an unanswered question is refused.
    * The in-process git test runs `git` with `execFile`, not `execFileSync`: a blocked event loop starves the handler, and the relay closes the connection after `accept_wait_ms`.
    * **Sizes:** `zen_svc_wasm_bg.wasm` is 1.23 MB, 465 KB gzipped (1.05 MB and 403 KB with TLS alone); glue 44 KB.
    * **Measured locally:** 50 MB through `cat` across two kills, with rekeys every 4 MB, in about 50 s.
  * Verified:
    * `cargo fmt --all --check`; clippy on zen-svc and zen-svc-wasm `--all-targets -D warnings` (default, `--no-default-features`, and zen-svc `--all-features`), and zen-svc-wasm for wasm32
    * `cargo test -p zen-svc --test ssh` (18, with the `held` checks)
    * `scripts/build-wasm.sh`, `--dev` and release
    * `npm run check`
    * `npx vitest run` 3×, 479 tests each (7 skipped: the opt-in benches). Runs 1 and 2 passed every test. This includes `packages/svc/test/sshwasm.test.ts` (5), `sshlayer.test.ts` (4) and `apps/echo/test/ssh.test.ts` (5).
    * Run 3 had one failure, not in this step's code: `packages/db/test/patterns.test.ts` "retry 'delay': attempts 1..max, then the class DLQ with the same id".
      * `ZenError: decrypt` in `Topic.open` while the test read back topic `t`'s events (the retry copies).
      * The test then passed 6 times alone and 6 times as the whole file, three copies at once.
      * This step changes nothing in `@zen/db` or `@zen/client` but the test helpers. Not investigated further: watch CI for it.
    * `npx playwright test apps/echo` (3 specs, the SSH case included)
    * `cargo test -p zen-svc`: 11 unit, `openssh.rs` 10, `openssl.rs` 6, `ssh.rs` 18, `tls.rs` 23
  * CI on PR #47's head 6c2b32a: `rust`, `pure-rust` and `fdb` are green in both runs. `openssh-pq` was skipped, as it now should be. The push run's `client` is green; the pull-request run's `client` failed.
  * Vitest passed in that run, the SSH tests included. Playwright passed 14 of 16 specs, the echo SSH case included. The 2 failures are both in `apps/chat/e2e/chat.spec.ts`:
    * "typing reaches the others…": Carol's `#typing` read "alice, bob typing…" for a while, then "" once Bob's indicator expired, instead of "bob typing…".
    * "a reaction shows on every page" timed out: it runs on the same pages after that failure. A consequence, not its own failure.
  * CI fix done (below): commit `apps: chat: a typing indicator older than its display time is ignored`.
    * The unit test passes with the existing typing test: `apps/chat` vitest, 9 tests.
    * `npx playwright test apps/chat` passed 5 times; `npm run check` passes.
    * The test's age bound is a minute below zero, so it doesn't depend on how the two clocks compare.
  * CI green on PR #47's head f30dfa8: all 8 checks across both runs (`client`, `rust`, `pure-rust`, `fdb`), with `openssh-pq` skipped as it now should be.
  * Merged at f30dfa8. The commit that logged the green CI (`docs: milestone 5.5 step 10 CI green in the log`) was pushed after the merge, so it isn't in main. Step 5.5.11's plan commit carries that log entry.
* **Step 5.5.11** (`tcp-auth` and `zen-connect`): done, PR #48. Restarted from main after PR #47 merged. Commits: `docs: milestone 5.5 step 11 plan`; `spec: tcp-auth details (api.md §14.9, svc.md §3)`; `server: tcp-auth services over /v1/svc/connect`; `client: connect to a tcp-auth service (Session.svcConnect)`; `svc: zen-connect`; `apps: echo over tcp-auth`; the docs and this log.
  * As planned, plus details settled while building:
    * **A client frame over 64 KiB resets with close 4000, not 1009.** axum's WebSocket reports the oversized message as an opaque read error, and the server can't tell it from other failures without matching on another crate's error type. So every reset is 4000, and api.md §14.9 says so.
    * **Failed accepts.** The plan said an `unavailable` error; there is no such code. A failed accept sends the store's own error (`err` with its code), then close 4000.
    * **Admission slots are a guard** (`svc::Slot`): `Service::open` returns one, and dropping it gives the `max_conns` slot back. A refused or abandoned upgrade can't leak a slot. The TCP accept loop uses it too.
    * **Ending a WebSocket.** Every end sends its close (1000 or 4000), then reads until the client's close for up to 1 s. Dropping at once could make the kernel reset the TCP connection under the close frame. A client's close, an oversized frame or a text frame other than `"fin"` ends the connection as a reset.
    * `auth::bearer` and `auth::resolve_bearer` factor out the `Caller` extractor's header parsing, so the connect handler can check the header before the upgrade.
    * **`SvcSocket` takes its handlers in `svcConnect`'s options** (`onData`, `onFin`, `onClose`), so nothing arrives before they are set.
    * **`zen-connect` and ssh's hang-up.** OpenSSH sends its ProxyCommand SIGHUP as it exits, after writing its last bytes. Killed at once, zen-connect dropped the WebSocket. The handler then learned of the end only from its 10 s quiet check (`/v1/svc/state`), and the in-process test's "connections forgotten" wait timed out. Now a hang-up in stdio mode lets zen-connect forward the rest of stdin and its EOF as `fin`, then wait up to 5 s for the service to close.
    * **The harness:** `WorldOptions.grantsOf` gives a member other grants (Bob serves but may not connect). `WorldService.mode` and `World.serviceId(name)` were added. The echo grants add `append` on `svc/`.
    * The example configs gain a commented `tcp-auth` entry; their parse test uncomments both and checks the modes.
    * The `fdb` multinode test shares its handler steps (`svc_lease`, `svc_answer`) with the existing two-node echo.
  * Verified:
    * `cargo fmt --all --check`; clippy `--all-targets -D warnings`, default, `--no-default-features` and `--all-features`
    * `cargo test -p zen-server --test svc` 3× on embedded (30 tests, 6 new) and once on FDB; `tests/multinode.rs` on FDB (12, `tcp_auth_across_nodes` new)
    * the full zen-server and zen-proto suites on embedded
    * the dev wasm build (`Frame`'s `ok` gains `conn`)
    * `npm run check`
    * `npx vitest run` 3×: 481 passed each time, 7 skipped (the opt-in benches). This includes `apps/echo/test/connect.test.ts` (5) and `packages/client/test/svcconnect.test.ts` (3).
    * `npx playwright test`: smoke and apps, 16 specs, unchanged
  * CI green on PR #48's head f893d8a: all 8 checks across both runs, `openssh-pq` skipped. Merged at 2061b38.
* **Step 5.5.12** (the sealed inbox): done, PR #49. Restarted from main after PR #48 merged. Commits: `docs: milestone 5.5 step 12 plan`; `spec: sealed inbox details (svc.md §9, api.md §14)`; `server: the sealed inbox (config, sealing, unseal)`; `wasm: inbox bindings`; `svc: sealed input in serve(), and RawStep.unseal()`; `apps: echo with a sealed inbox`; `docs: the sealed inbox (CLIENT.md §11, apps README, TD-SVC-INBOX-ROTATION)`; this log.
  * As planned, plus details settled while building:
    * **The server** encapsulates in the accept transaction's setup. The `InboxKey` lives in a `OnceLock` on the connection task, next to a `sealing` flag. `append` seals the batch once, before its transaction. Inside it, while sealing, it reads `ra` by snapshot; once it sees `unseal`, it clears the flag (sticky) and stores the batch plain. A failed encapsulation fails the accept.
    * **Plaintext lengths:** `api::plain_len` (data less the 16-byte tag when sealed) serves `/v1/svc/read` (the chunk holding `from`, and the `max_bytes` budget) and `svc_sub` pushes. Before the fix, a read from the end of a sealed chunk also returned that chunk, and two chunks' worth of `max_bytes` returned one. The new test fails on the old code at that read.
    * **The config:** `inbox` is hex with an exact length (`INBOX_PUBLIC_LEN`). The example configs describe it in a `## ` prose line, not a commented setting, since their parse test uncomments the service examples.
    * **The binding:** `InboxKey.encaps` returns a key whose `ct` getter holds the ciphertext (`undefined` for `decaps`), instead of a `{ct, key}` object. A wrong seed length is `param`; a wrong ciphertext length is `format`.
    * **The runtime:**
      * a claim with `inbox_ct` and no seed throws `inbox` in `load`, so it is reported and released with `close`
      * sealed chunks are opened in `input()`, which pushes, early chunks and `repair()` all go through; an early chunk's span counts plaintext
      * the claim flag `C_UNSEAL` (32, `ra`'s `RA_UNSEAL` shifted) marks it sent
      * `Finished.unseal` is optional, so the SSH layer and the error paths don't set it
    * **The serve()-level tests** are in `apps/echo/test/inbox.test.ts` rather than `packages/svc`: the echo harness is where `serve()` runs against a live server. They count `InboxKey.openChunk` calls to show that input after the `STARTTLS` reply arrives plain.
    * The echo app's `STARTTLS` line is echoed like any other (`<n>: STARTTLS`), so transcripts stay uniform.
  * Verified:
    * `cargo fmt --all --check`; clippy `--all-targets -D warnings` on zen-server, default, `--no-default-features` and `--all-features`; clippy on zen-wasm for wasm32
    * `cargo test -p zen-server --test svc` 3× on embedded and once on FDB (33 tests, 3 new: `inbox_seals_input`, `unseal_stops_sealing`, `unseal_without_inbox_is_a_no_op`); `tests/multinode.rs` and `tests/dump.rs` on FDB
    * the config unit tests (bad length, not hex, a valid key)
    * the full zen-server suite; `cargo test -p zen-core` (54, vectors unchanged)
    * `scripts/build-wasm.sh --dev`; `npm run check`
    * `npx vitest run` 3×: 486 passed each time, 7 skipped (the opt-in benches). This includes `packages/client/test/svcvectors.test.ts` (7, inbox vectors new) and `apps/echo/test/inbox.test.ts` (4).
    * `npx playwright test`: 16 specs, unchanged
  * The local FDB cluster had stopped (a container restart); it was restarted with the binary rebuilt `--features fdb`. Stale test executables were pruned (4 GB).
  * CI green on PR #49's head a798c2e: all 8 checks across both runs (`client`, `rust`, `pure-rust`, `fdb`), `openssh-pq` skipped. Merged at 0185426.
* **Step 5.5.13** (`apps/mail`) is split in two PRs (the user's decision):
  * **5.5.13a**: STARTTLS, the line reader and inbound SMTP: done, PR #50. Commits: `docs: milestone 5.5 step 13a plan`; `spec: STARTTLS (svc.md §7.1)`; `svc: a line reader, and starttls()`; `apps: mail, inbound SMTP`; the docs and this log.
    * As planned, plus details settled while building:
      * **Four phases, not three.** The `starttls` engine state's `phase` is 0 plain, 1 switched (the handshake not begun), 2 handshake, 3 TLS. A connection switched but whose handshake hasn't consumed or sent anything is still resumable: a successor begins the handshake. So the window between the `220` and the client's ClientHello survives a takeover.
      * **Input after the command is read at once.** The switching step sets `again` when input follows what it acked, so pipelined or injected plaintext reaches the TLS engine without waiting for more input, and the handshake fails on it then.
      * **`TlsConn` carries the phase** for both layers. `tls()` begins at "switched" and seals the bare TLS state, as before; `starttls()` wraps it in `{v, phase, tls?}`. `RawStep.startTls()` comes from the `Step` class (allowed only in `starttls()`'s plain phase).
      * **`LineReader.next()` gives bytes and `nextText()` UTF-8 text**; `max` counts the terminator, and a line over it, complete or not, sets `tooLong`.
      * **The DATA decoder** treats only CRLF as a line end: a dot after a bare LF stays, and `\n.\n` never ends a message (no SMTP smuggling). A dot that starts a line is always dropped, as RFC 5321 §4.5.2 says.
      * **Parts are filled to 64 KiB**: each step appends to the last part (read, concatenate, put) before starting the next, so a 10 MB message is at most 160 rows whatever the step sizes.
      * **Drafts:** a client's EOF mid-DATA deletes its draft at once; only a connection that dies without one (a reset, a handler that never steps again) leaves one for `sweepDrafts`. A draft swept while its message still arrives makes the end reply `451` (try again), not a truncated message.
      * **The Python client** sets smtplib's `_host` after `connect()`: `starttls()` checks the certificate against the host given to the constructor, which `connect()` alone leaves unset.
      * **The layer test** (`packages/svc/test/starttls.test.ts`) drives the layer over real TCP sockets. Over an in-memory Duplex that had been read in plain first, Node's TLS client never finished a 1.3 handshake (1.2 worked); with a `net.Socket`, as real STARTTLS clients use, both do.
    * Verified:
      * `npm run check`
      * `npx vitest run` 3×: 508 passed each time, 7 skipped (the opt-in benches). This includes `packages/svc/test/lines.test.ts` (4), `starttls.test.ts` (6) and `apps/mail/test/smtp.test.ts` (12); the mail suite also passed 3 more times alone.
      * `npx playwright test`: 16 specs, unchanged
      * `cargo test -p zen-server --test svc` (33; the server is unchanged)
  * **5.5.13b**: waking handlers without input, the IMAP subset with IDLE, and the inbox page. Planned after 5.5.13a merges.
  * Messages may be up to 10 MB (the user's decision), advertised as `SIZE 10485760`.

## Step 5.5.13a in detail: STARTTLS, the line reader, inbound SMTP

An inbound mail server (RFC 5321) as a relayed service. A message is accepted, stored, its event emitted and `250` sent in **one commit**, so a handler killed at any point, even between the end of DATA and the reply, never loses or doubles a message. Sessions may switch to TLS with STARTTLS (RFC 3207), which `@zen/svc` gains as a layer. Before the switch, the sealed inbox (5.5.12) keeps the plaintext sealed at rest. Text protocols get a line reader.

**Commits:**
1. `docs: milestone 5.5 step 13a plan` (also logs step 5.5.12's CI and merge)
2. `spec: STARTTLS (svc.md §7.1)`
3. `svc: a line reader, and starttls()`
4. `apps: mail, inbound SMTP`
5. the docs and the log

### Facts from the survey
* **`tls()`** (`packages/svc/src/tls.ts`) assumes TLS from the first byte.
  * Its `TlsConn` holds the handshake in memory (`hs`) and the committed state once established.
  * `load()` refuses a connection taken over mid-handshake.
  * `prepare()` runs `after()` on an established engine; the handler is called on the step that finishes the handshake, on new plaintext, or on an unseen EOF.
  * The engine holds at most 16 KiB of unconsumed plaintext and stops decrypting there, so a handler must consume as it reads.
* **The runtime** (`serve.ts`): a fresh connection gets a first step without input (`due`), so a server-first protocol can greet. The raw layer is resumable from the first step; `Finished.unseal` (5.5.12) sets the inbox's sticky flag.
* **No line splitting is shared:** echo's raw handler (`apps/echo/src/echo.ts`) and its SSH `lines` command (`ssh.ts`) repeat the same `indexOf(10)` loop.
* **zen-db:**
  * A row's large fields are split into parts automatically (`packages/db/src/row.ts` `layoutRow`), but a commit carries at most 8 MB, so a 10 MB message can't be written in one commit.
  * `tx.emit(topic, type, body, {key, id})` emits an event in the transaction (`txn.ts:166`).
  * A conflict re-runs the transaction, so a counter row read-modify-write is safe (`db.ts:547`).
  * Indexes: `createIndex(table, name, {fields, unique})`.
* **Apps are found by globs** (`scripts/build-apps.mjs` `appNames`, `apps/tsconfig.json`, `vitest.config.ts`, `playwright.config.ts`): `apps/mail` needs no wiring.
  * The harness: `startWorld({grants, services: [{name, path, mode, resume, more}]})`, `w.servicePort`, `spawnWorker`, `workerEnv`, `waitFor`, `kill`.
  * `testCerts()` gives a P-256 chain for `localhost` and `127.0.0.1` (needs `openssl`).
* **Python** 3.11 here, 3.12 on the runners (preinstalled on `ubuntu-latest`), with `smtplib` and `ssl`. Only `packages/fuse/test/mount.test.ts` runs Python today (`python3 -c`).

### spec: svc.md §7.1 (`spec:`)
* §7's engine list gains `"starttls"`.
* **A new paragraph, STARTTLS.** A connection that starts plain and switches to TLS in mid-stream.
  * **The switch.** A step of the plain phase may end it: its records go out in plain (the protocol's go-ahead reply), and the input from its `in_to` on is TLS.
  * **Plaintext the client sent after the command** (pipelined, or injected, CVE-2011-0411) is handed to the TLS engine as TLS input. So it is never read as a command after the switch: a handshake that begins with it fails, and the connection closes.
  * **Sealing:** the step that switches sets `unseal` (§9): what follows is TLS ciphertext.
  * **Resumability:** the plain phase is resumable from the first step, as raw; the handshake isn't (as in `tls`); the TLS phase is again. A successor that finds the handshake phase releases the connection with `close`.
  * **The state:** `engine_state` is `{v: 1, phase: 0 plain | 1 handshake | 2 tls, tls?: <§7.1's engine_state>}`, absent in the plain phase until the switch.

### `@zen/svc` (`svc:`)
* **`LineReader`** (`packages/svc/src/lines.ts`):
  ```ts
  const r = new LineReader(c.input, { max: 1000 });   // max line, with its terminator
  for (let l = r.next(); l !== null; l = r.next()) { … }  // a line without its CRLF (or bare LF)
  r.consumed;     // bytes of whole lines read: what to ack
  r.rest();       // the bytes after them (e.g. message data after DATA)
  r.tooLong;      // no terminator within `max` bytes
  ```
  * Echo's two loops use it.
* **`starttls(opts: TlsLayerOptions): Layer<RawStep>`**, engine `starttls`:
  * Plain phase: as `raw()`, plus `c.startTls()`. The step's writes go out in plain; the switch happens after them.
  * TLS phase: as `tls()`, with `c.tls` set; the handler is called when the handshake finishes, so a protocol that has the server speak again can.
  * `RawStep.startTls()` throws under the other layers and after the switch.
* **Refactor:**
  * `tls.ts`'s `TlsConn` gains a starting phase.
  * `tls()` is `TlsConn` started in TLS; `starttls()` starts it plain.
  * The `Step` class gets the `startTls` hook.
  * `Finished.unseal` is set on the switching step.
* **Tests:**
  * `packages/svc/test/lines.test.ts`: CRLF and bare LF, a partial last line, `max`, `rest()` after a line.
  * `packages/svc/test/starttls.test.ts`, driving the layer as `serve()` does with no server (the pattern of `sshlayer.test.ts`), against Node's `tls.connect` over an in-memory socket, restoring from the committed state at every step:
    * a plain exchange, the switch, data both ways, close_notify, on 1.2 and 1.3
    * pipelined plaintext after the command fails the handshake
    * a handshake-phase state refused by `load`
    * `unseal` on the switching step

### `apps/mail`: inbound SMTP (`apps:`)
* **`src/schema.ts`, namespace `mail`:**
  * `mailboxes {address → user}`: the local addresses
  * `drafts {draft, conn, started}` and `parts {draft, seq → data}`: DATA staged as it arrives
  * `messages {user, uid → draft, size, from, to, received, head, flags}`, with index `by_user_uid`
  * `counters {user → next}`: UIDs
  * `certs`: the TLS chain and key, and the inbox seed, as echo keeps them
  * The topic `mail/received`.
* **`src/smtp.ts`, the handler (RFC 5321 subset, with RFC 3207 and RFC 1870):**
  * **The greeting** `220 <host> ESMTP zen` on the first step.
  * **Commands:**
    * `EHLO`/`HELO`: `EHLO` lists `PIPELINING`, `SIZE 10485760`, `8BITMIME`, and `STARTTLS` before the switch
    * `MAIL FROM:<…> [SIZE=n]`: over the limit gives `552`
    * `RCPT TO:<…>`: an address not in `mailboxes` gives `550`; at most 100 recipients
    * `DATA`, `RSET`, `NOOP`, `QUIT`, `VRFY` (`252`), `STARTTLS` (`220`, then `c.startTls()`)
    * anything else: `500`/`502`; out of order: `503`
  * After the switch the session starts over (RFC 3207 §4.2): the state forgets EHLO and the envelope.
  * Pipelined commands are answered in order within one step. A line over 1,000 bytes gets `500` and closes.
  * **DATA:**
    * At `DATA`, a draft row is created, and `354` is sent.
    * Each later step decodes what arrived with a streaming decoder (dot-unstuffing, the end `CRLF.CRLF` across steps, no line limit), appends it as `parts` rows of at most 64 KiB, and acks it, in the same commit. The decoder's position and the draft's counters live in the state.
    * At the end, one commit does the rest, then replies `250 2.0.0 Ok <id>`:
      * for each recipient, a `messages` row with the next UID of its user and a `Received:` header (peer, HELO, TLS version and suite, time) in `head`; the parts are shared
      * the draft row is deleted
      * `tx.emit(received, 'message', {user, uid, size}, {key: user})`
    * Over 10 MB: the rest is read and dropped, the staged parts are deleted, and the reply is `552`.
    * `RSET`, or a new `MAIL`, deletes a staged draft's parts.
  * **Exactly once:** the reply is released only after its commit (svc.md §5). A successor continues from the last committed step: the staged parts, the decoder state and the envelope.
* **`src/mail.ts`:**
  * `SMTP_SERVICE = ['svc', 'smtp']`
  * `putMailbox(db, address, user)`
  * `messageBytes(db, user, uid)`: head plus parts, for the tests and for 5.5.13b
  * `sweepDrafts(db, olderThanMs)`: deletes drafts left by connections that died mid-DATA, with their parts
* **`workers/smtp.ts`:** serves `svc/smtp` with `starttls()` and the inbox seed from `certs`, and sweeps drafts every 10 minutes. `MAIL_STALL=data-end` makes the handler print `stall` and hang inside the transaction at the end of DATA, for the kill tests.
* **`test/client.py`** (run as `python3 -I`): `smtplib` with options for plain, STARTTLS on 1.2 or 1.3, a CA file, the messages to send (a JSON file) and a timeout. It prints each reply as a JSON line.
* **Vitest (`apps/mail/test/smtp.test.ts`).** The service has `inbox` set (5.5.12); the certificates are `testCerts()`.
  1. **In process, with Python:**
     * one message plain, one over STARTTLS 1.3, one over 1.2
     * each lands once for each recipient, with its exact bytes (`messageBytes`) and an event on `mail/received`
     * the `Received:` header names the TLS version
  2. **Refusals:** an unknown recipient (`550`); `SIZE=` over the limit (`552`); a body over the limit (`552`, and no parts left behind); out of order (`503`); `RSET` mid-transaction.
  3. **Pipelining and injection:** a Node `net` client.
     * `MAIL`, `RCPT` and `DATA` in one write get three replies.
     * `STARTTLS\r\nNOOP\r\n` in one write: `NOOP` is never answered, and the connection closes when the handshake fails.
  4. **A kill between DATA and the reply:**
     * a worker with `MAIL_STALL=data-end` prints `stall` and is SIGKILLed
     * a fresh worker (lease TTL 1 s) takes over, and Python gets `250`
     * the message is stored exactly once
     * done plain (sealed inbox) and over STARTTLS 1.3 (the TLS state resumes)
  5. **A kill mid-DATA:** a 3 MB message over STARTTLS 1.2; the worker is killed after some parts are staged. The successor continues, the stored bytes' SHA-256 equals the sent one, and no draft is left.
  6. **`sweepDrafts`:** a connection dropped mid-DATA leaves a draft, which the sweep removes.

### Docs
* `docs/CLIENT.md` §11: `starttls()` and `c.startTls()`, `LineReader`, the injection rule; the "Not yet" line becomes IMAP and waking handlers (5.5.13b).
* `apps/README.md`: the mail app.
* TECH_DEBT **TD-MAIL-SCOPE**: inbound only.
  * No SPF, DKIM or DMARC checks, no outbound delivery, no submission port or SMTP AUTH, no `CHUNKING`/`BDAT` or `SMTPUTF8`.
  * Members of the fs can read every mailbox: mail is as private as the fs, not per user.
* The log in both plan copies.

### Out of scope (5.5.13b and later)
* waking handlers without input; IMAP (LOGIN with app passwords, SELECT, FETCH, STORE, SEARCH, EXPUNGE, IDLE); the inbox page and its Playwright spec
* everything in TD-MAIL-SCOPE

### Verification
* `npm run check`; `npx vitest run` 3×; `npx playwright test` once (unchanged)
* `cargo test -p zen-server --test svc` once (the server is unchanged)
* CI green on the PR

## Step 5.5.12 in detail: the sealed inbox

A service with `inbox` (an X-Wing public key) never stores its input in plaintext. The owning node encapsulates once per connection, stores the ciphertext in the connection record, and seals every input chunk at rest (svc.md §9, formats.md §13.3). Handlers hold the decapsulation seed, which the app keeps sealed in the fs, and open the chunks. A step can set the sticky `unseal` flag (after STARTTLS the input is TLS ciphertext anyway), and the node then stores plain chunks. zen-core already has the crypto and its vectors (`inbox_public`, `inbox_encaps`, `inbox_decaps`, `InboxKey::seal_chunk`/`open_chunk`, `spec/test-vectors/svc.json` `inbox`). The proto has every field: `SvcInfo.inbox`, `SvcClaim.inbox_ct`, `SvcChunk.sealed`, `SvcStep.unseal`, `svc_in.sealed`. This step turns it on end to end.

**Commits:**
1. `docs: milestone 5.5 step 12 plan` (also logs step 5.5.11's CI and merge)
2. `spec: sealed inbox details (svc.md §9, api.md §14)`
3. `server: the sealed inbox (config, sealing, unseal)`
4. `wasm: inbox bindings`
5. `svc: sealed input in serve(), and RawStep.unseal()`
6. `apps: echo with a sealed inbox`
7. the docs and the log

### Facts from the survey
* **Config:** `ServiceConfig` (`config.rs:317`, `deny_unknown_fields`) has no `inbox`. The test at `config.rs:784` expects `inbox = "00"` to be refused as an unknown field. `validate_services` is at :627. The struct literals in `tests/svc.rs:27`, `tests/multinode.rs:493` and `tests/dump.rs:206` need the new field.
* **The server leaves the inbox off everywhere:**
  * `ConnRec.inbox: Option<Inbox{v, ct}>` (`svc/mod.rs:458`) is always `None` (`conn.rs` `accept`).
  * `append` always stores the prefix byte 0.
  * `api.rs` `info` sets `inbox: None`.
  * `claim_of` already passes `rc.inbox` on as `inbox_ct`.
  * `step.rs:171` ORs `RA_UNSEAL` into `ra`, but nothing reads it.
* **A latent bug for sealed chunks.** The read path (`api.rs:299, 301, 311`) and the push path (`push.rs:278`) take `data.len()` as the chunk's plaintext length and as the `max_bytes` budget. A sealed chunk is 16 bytes longer (the tag). `step.rs:202` `chunk_len` already subtracts 17.
* **Exports leave relay keys out** (`dump::export`, test `export_skips_relay_keys`). So "no plaintext at rest" is checked by scanning the live store's `r*` values. `relay_data` (`tests/svc.rs:204`) returns keys only.
* **Client:**
  * zen-wasm binds nothing of the inbox.
  * `serve.ts:546` releases a connection with `close` on a sealed chunk ("not supported yet").
  * Input is joined by offset in `input.ts` (`Input.add`); `early` buffers pushes for connections not claimed yet; `repair()` reads after a gap.
  * `build()` never sets `unseal`.
  * `RawStep` (`layer.ts:26`) has no `unseal`, and `Finished` (`layer.ts:74`) has no field for it.
* **Where secrets live:** apps keep keys in sealed zen-db rows. The echo app's `certs` table holds the TLS key and the SSH host key; `tlsLayer` and `sshLayer` read them before `serve()`.
* **Test config:** `ServiceSpec.more` and `WorldService.more` pass any `[[service]]` setting, so `more: {inbox: hex}` works once the server takes the field.

### spec (`spec:`)
* **svc.md §2:** `inbox` is the X-Wing public key in hex, 1,216 bytes; another length is refused at start.
* **svc.md §9:**
  * The node notices `unseal` when it stores a batch: it reads `ra` with the batch. It stores later batches plain. A batch it was already storing stays sealed.
  * `unseal` on a service without `inbox` is a no-op.
  * The quota counts input bytes as plaintext, as `in_head` does. The 17 bytes a sealed chunk adds aren't counted, as the state's framing isn't.
  * A handler without the seed can't serve the service: it releases a connection that has `inbox_ct` with `close`.
  * A resent batch (after `commit_unknown`) seals the same bytes at the same offset under the same key, so it gives the same chunk. This is already formats.md §13.3; the step only cross-references it.
* **api.md:**
  * §14.1 `inbox` is present when configured.
  * §14.6: `max_bytes` and chunk lengths count plaintext bytes. A sealed chunk's `data` is 16 bytes longer.
  * §14.5 `unseal`: sticky; a no-op without an inbox.

### Server (`crates/zen-server`)
* **`config.rs`:**
  * `inbox: Option<Vec<u8>>`, hex (an `Option` variant of `de_hex`).
  * `validate_services` checks the length against `zen_core::svc::INBOX_PUBLIC_LEN`.
  * The `inbox = "00"` test becomes a length refusal, plus a valid-key test.
  * The example configs gain a commented `# inbox = "…"` line, explained.
* **`svc/conn.rs`:**
  * At accept, with `cfg.inbox`: `inbox_encaps(pk, fs, svc, conn, OsRng)`. The ciphertext goes into `rc.inbox = {v: 1, ct}`, and `Conn` keeps the `InboxKey` and a `sealing` flag.
  * `append`, in its transaction, snapshot-reads `ra` while sealing. If `RA_UNSEAL` is set, sealing stops from that batch on (sticky). Otherwise the batch is stored as `1 ‖ key.seal_chunk(in_head, data)`. The quota still adds `len`.
  * A failed encapsulation fails the accept, as a storage error does today.
* **`svc/api.rs`:** `info` returns `cfg.inbox`. In `chunks`/`chunk`, a chunk's plaintext length is `data.len() − 16` when it is sealed: for the next offset, and for the `max_bytes` budget.
* **`svc/push.rs:278`:** the same plaintext length for `next`.
* **Tests (`tests/svc.rs`, embedded and FDB):**
  * **`inbox_seals_input`:**
    * `info` shows the key, and the claim carries `inbox_ct`.
    * A peer writes lines holding a marker. `read` returns sealed chunks; decapsulating with the seed and opening each at its offset gives exactly the input, contiguous.
    * A scan of every `r*` value in the store finds no marker.
    * Reads with a small `max_bytes` across sealed chunks advance by plaintext length, with no gap or overlap.
    * A `svc_sub` push of sealed chunks has consecutive offsets.
  * **`unseal_stops_sealing`:**
    * A step with `unseal: true`.
    * Input written after it is eventually stored plain (`sealed: false`), and opens or reads as written.
    * A chunk sealed in between still opens.
    * The handler sees one contiguous stream across the switch.
  * Config validation (a bad length).

### zen-wasm (`crates/zen-wasm`)
* `inboxPublic(seed: Uint8Array) → Uint8Array`
* `InboxKey.decaps(seed, ct, fsId, svc, conn) → InboxKey`, with `openChunk(off: bigint, sealed: Uint8Array) → Uint8Array` and `free()`. Errors are `ZenCryptoError` `format` and `decrypt`, as the other bindings.
* Also `InboxKey.encaps(public, fsId, svc, conn) → {ct, key}` and `sealChunk`, for tests that play the server's part.
* The vector test in `packages/client/test/svcvectors.test.ts` covers the `inbox` vectors: the public key, decapsulation, and the two chunks.

### `@zen/svc`
* **`ServeOptions.inbox?: Uint8Array`:** the 32-byte decapsulation seed.
* **Loading a claim with `inbox_ct`:**
  * With a seed: `InboxKey.decaps(...)`, kept with the connection and freed when it is forgotten.
  * Without one: `onError` ("the service seals its input: serve() needs its inbox seed"), and the connection is released with `close`.
* **Sealed chunks** (pushed, read after a gap, or buffered early) are opened at their offset before `Input.add`. A chunk that doesn't open is a bad record: `onError`, and the connection is released with `close`. Contiguity is `Input`'s offset joining (the nonce binds the offset, so a moved chunk doesn't open).
* **`RawStep.unseal()`:** sets the sticky flag. The step that carries it sends `unseal: true`. The connection remembers it was sent, so the flag isn't sent again.
  * `Finished` gains `unseal`. `raw()` and `tls()` pass it from their `Step`.
  * SSH has no use for it; the TLS layer has no STARTTLS (5.5.13).
* **Tests:** `packages/svc/test/input.test.ts`, or a new `inbox.test.ts` against a `TestServer` with an inbox service, driving `serve()`:
  * sealed input echoed
  * a seed-less `serve()` releases with `close` and reports it
  * `unseal()` sent once

### Echo with a sealed inbox
* **`apps/echo/src/schema.ts`:** `putInboxSeed(db, seed)` puts the seed in `certs` under `inbox` (sealed like every row); `inboxSeed(db)` reads it.
* **`echo.ts`:**
  * `INBOX_SERVICE = ['svc', 'echo-inbox']`.
  * The echo handler gains one command: the line `STARTTLS` gets its reply and calls `c.unseal()`. It stands in for the real STARTTLS of 5.5.13: after it, the client's bytes would be TLS ciphertext and need no sealing.
* **`workers/handler.ts`:** `ECHO_LAYER=inbox` serves `echo-inbox` with the seed from `certs`.
* **Harness:** nothing new. The test computes `inboxPublic(seed)` before `startWorld` and passes it as `more: {inbox: hex}`.
* **Vitest (`apps/echo/test/inbox.test.ts`):**
  1. In-process: lines echoed and stored as rows; the peer's FIN gives `bye`.
  2. `STARTTLS` mid-stream: the lines after it are still echoed in order.
  3. A worker SIGKILLed mid-stream (lease TTL 1 s): the successor decapsulates again and continues. Every line comes back once and in order, and the table holds exactly the lines.
  4. A `serve()` without the seed: `onError` reports it, and the peer's connection closes.
* Playwright: none (the browser path is unchanged but for the WASM binding, which vitest covers).

### Docs
* `docs/CLIENT.md` §11:
  * the inbox: how an admin makes the seed (`zw.randomBytes(32)`) and the configuration's key (`bytes.hex(zw.inboxPublic(seed))`)
  * where the seed lives (a sealed row)
  * `serve({inbox})` and `c.unseal()`
  * what the inbox protects: at rest, not from the server process
  * The "Not yet" line drops the inbox.
* `apps/README.md`: echo's inbox service.
* The example configs.
* **TECH_DEBT TD-SVC-INBOX-ROTATION:** changing a service's inbox key needs the old seed until the connections opened under it end. There is no rotation helper.
* The log in both plan copies.

### Out of scope
* STARTTLS in the TLS layer (raw, then TLS on the same connection) and the line helper: 5.5.13, with `apps/mail`.
* Inbox key rotation tooling (TD).
* Delegating the seed to a worker without fs access.

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings`: default, `--no-default-features` and `--all-features`
* `cargo test -p zen-server --test svc` 3× on embedded and once on FDB; the full zen-server suite once; `cargo test -p zen-core` (unchanged vectors)
* `scripts/build-wasm.sh --dev`; clippy on zen-wasm for wasm32
* `npm run check`; `npx vitest run` 3×; `npx playwright test` once
* CI green on the PR

Housekeeping before "go": the self check-in scheduled for 22:52 UTC (PR #48's CI) is obsolete now that the PR is merged; delete it if it hasn't fired.

### CI fix in step 5.5.10: a stale typing indicator in the chat app

**Root cause.**
1. In the spec's first test Carol's context goes offline while Alice and Bob talk. `send()` fills the composer, so each message also publishes a typing indicator.
2. The server never replays ephemeral messages (`eph.rs` tails from the newest entry). Chromium's offline emulation holds the frames of the open WebSocket instead of dropping them. So Alice's indicator from the offline period reaches Carol when she comes back.
3. The second test starts within the 3 s an indicator is shown, so Carol shows Alice as typing next to Bob.
4. The chat UI (`apps/chat/ui/main.ts` `showTyping`) shows any indicator for 3 s from its arrival, however old it is. That is a real bug: a device back online shows stale "typing" lines.

The push run passed because the overlap depends on timing. Nothing here is from this step's changes.

**Fix** (commit `apps: chat: a typing indicator older than its display time is ignored`):
* `onTyping(db, room, fn, maxAgeMs = 3000)` in `apps/chat/src/chat.ts` drops a message whose sender's time is too old:
  * the sender's time is `m.hlc >> 16n`, Unix ms (formats.md §11.1)
  * "too old" is more than `maxAgeMs` behind the corrected clock, `db.fs.session.clock.now()` (`packages/client/src/clock.ts`)
  * `EphemeralMsg` already carries `hlc` (`packages/db/src/broker.ts`)
* The UI's 3 s display time becomes a shared constant, `TYPING_SHOW_MS`, used both there and as the default `maxAgeMs`.
* Unit test in `apps/chat/test/chat.test.ts`:
  * an indicator published, then received through `onTyping` with `maxAgeMs: 0` after a 50 ms wait, is dropped
  * a fresh one is passed on
  * the existing typing test is unchanged

**Verify:**
* the chat unit tests
* `npx playwright test apps/chat` 5×
* `npm run check`
* push, then CI green on both `client` runs

After that: log the CI result in both plan copies, push, stop for compaction.

## Step 5.5.11 in detail: `tcp-auth` and `zen-connect`

In `tcp-auth` mode, members reach a service through zen-serve's own HTTPS listener instead of a public port. The connector opens a WebSocket, `GET /v1/svc/connect` (api.md §14.9), with a zen token. The server checks topic `append` on the service id, then accepts the connection as if it came from a TCP socket. Everything after that is unchanged: the queue, claims, steps, reveals and takeover. Handlers see `peer = {user, device}`. A CLI, `zen-connect`, turns the WebSocket into a local port (`--listen`) or into stdin and stdout, for `ssh -o ProxyCommand=…`.

**Commits:**
1. `docs: milestone 5.5 step 11 plan` (also logs step 5.5.10's CI and merge)
2. `spec: tcp-auth details (api.md §14.9, svc.md §3)`
3. `server: tcp-auth services over /v1/svc/connect`
4. `client: connect to a tcp-auth service (Session.svcConnect)`
5. `svc: zen-connect`
6. `apps: echo over tcp-auth`
7. the docs and the log

### Facts from the survey
* **What exists today:**
  * `SvcMode::TcpAuth` is in the config: `listen` is refused, and `Svc::new` builds its `Service`. The ready queue, `/v1/svc/*`, `svc_sub` and steps all work by `(fs, svc)` and don't depend on the mode. `/v1/svc/info` returns the mode. `ConnRec.mode` comes from the config.
  * Nothing accepts `tcp-auth` connections: there is no route for `/v1/svc/connect`.
  * `listen::start` takes the node lease and starts the work loop whenever any service is configured, including a `tcp-auth`-only node.
* **The connection task is written against TCP.** `conn::spawn`, `Conn::run`, `Conn::serve`, `Conn::writer`, `drain` and `listen::reset` all take a `TcpStream` or one of its halves. The storage side doesn't depend on the transport: `accept`, `append`, `output`, `advance`, the timers and `Cmd`. Nothing reads `Service.addr` except `local_addr`, which only tests use. A connection belongs to the node that accepted it, through its node id.
* **Admission:** `Service::take` (the per-IP bucket) and `Service::open`/`close` (`max_conns`) are private to `svc/mod.rs`. The HTTP router has `ConnectInfo<tls::Peer>`, which gives the client's address.
* **Auth for WebSockets:** `/v1/stream` (stream.rs:28) authenticates with a first frame only. `Caller` reads `Authorization: Bearer` (auth.rs:284), `resolve` is at auth.rs:240, and `require_topic` at :146. `Frame::Ok {id}` has no `conn`.
* **Rust tests:** `tokio-tungstenite` is already a dev-dependency, with `tests/common` `connect`/`send`/`recv`. `tests/svc.rs` drives handlers over HTTP (`lease`, `next`, `read`, `step`, `reveal`). Its `service()` builds a `tcp` service, and `setup` unwraps `local_addr`.
* **Client:**
  * `Stream.connect` uses `globalThis.WebSocket` (undici in Node 22) and authenticates with a first frame (`authToken`, stream.ts:228). `Transport.wsUrl` builds the URL. The `ws` package isn't installed.
  * A service id is keyed by the fs naming key, so only an unlocked fs can compute it (`UnlockedFs.service().id`). A connector needs only a session, so it takes the id in hex, the one in `[[service]] id`.
  * There's no token refresh: a session lasts `session_ttl_secs` (default 1 day), and API tokens don't expire unless set to.
* **CLI precedent:** `zen-mount` (`packages/fuse/src/cli.ts`):
  * `parseArgs`, a `HELP` text, exit 2 on bad usage
  * secrets never taken from argv
  * sign-in by `--user`, `--opaque`, `--api-token-file` / `$ZEN_API_TOKEN`, `--device-key` or `--vault`
  * its test builds it with `tsc` first
  * CI builds only `@zen/client` and `@zen/fs`, so a CLI in `@zen/svc` can't be built with `tsc` there: `@zen/db` isn't built.
* **The harness:**
  * `serviceToml` hard-codes `mode = "tcp"` and a `listen`.
  * `startWorld` computes the service ids but doesn't expose them.
  * The echo grants give members `consume` on `svc/`, not `append`.

### spec (`spec:`)
* **api.md §14.9:**
  * Query `fs` (decimal) and `svc` (hex).
  * **With a Bearer header,** everything is checked before the upgrade, and failures are HTTP errors:
    * 401 `unauthorized`
    * 403 `forbidden` (no topic `append`)
    * 404 `not_found` (no such service, or not `tcp-auth`)
    * 429 `quota` (`max_conns` or the address's token bucket)
  * **Without a header,** the first frame must be `{op: "auth", token}`, within 10 s. A failure is a frame `{op: "err", code, message}` with the same codes, then a close.
  * On success the server sends `{op: "ok", conn}`; `Frame::Ok` gains an optional `conn`.
  * **Frames:**
    * The client's binary frames are at most 65,536 bytes; a larger one is a reset (close 1009).
    * The server's binary frames are at most 65,536 bytes.
  * **Close codes:**
    * 1000 from the server: the connection ended normally (a close record, or FIN both ways with all input acked).
    * 4000 from the server: a reset (an abort record, a timeout, a reap). The connector should reset its local side.
    * Any close from the client is a reset.
  * **Rights are checked once, at connect,** as a TCP peer is admitted once. A later revocation doesn't end an open connection; the handler sees the connector's user and device, and can end it.
* **svc.md §3:**
  * Admission for `tcp-auth`: `max_conns`, and the token bucket of the WebSocket client's address.
  * Steps 3, 8 and 9 in WebSocket terms:
    * a `"fin"` text frame is the peer's FIN, both ways
    * a close record sends close 1000, then waits up to 1 s for the client's close, discarding data
    * an abort sends close 4000
  * The connection is the WebSocket: losing the WebSocket loses the connection, as for TCP.

### Server (`crates/zen-server`)
* **`svc/conn.rs`:** the connection task gets a transport enum, `Sock::Tcp(TcpStream) | Sock::Ws(WebSocket)`, split into `Rd` and `Wr` enums with:
  * `read(&mut buf) → io::Result<usize>`, where 0 is EOF: TCP's `Ok(0)`, or a `"fin"` frame
  * `write_all`, `fin()`, `close()` (TCP: the existing drain; WS: close 1000, then drain) and `reset()` (TCP: `SO_LINGER` 0; WS: close 4000)
  * For WS:
    * The reader keeps the rest of a frame larger than the room it's given. It reads nothing new until that rest is consumed, so the input window holds.
    * The writer splits output into 64 KiB frames.
    * Halves come from `futures::StreamExt::split`, as in stream.rs.
  * `run`, `serve`, `writer` and `drain` take these; TCP behaves exactly as before.
  * For WS, `run` sends `{op: "ok", conn}` once `accept` has committed. If `accept` fails, it sends an `err` frame (`unavailable`) and closes.
* **`svc/connect.rs` (new):** the axum handler `connect(State, ConnectInfo<tls::Peer>, Query, HeaderMap, WebSocketUpgrade)`.
  * It runs the checks in the order above (auth, then service and mode, then `append`, then admission), before the upgrade when there's a header and after the first frame otherwise.
  * It then calls `conn::spawn(st, svc, Sock::Ws(ws), SvcPeer {user, device})`.
  * Its WebSocket limits are 65,536 bytes plus framing.
* **Other changes:**
  * `Service::take`, `open` and `close` become `pub(crate)`.
  * `lib.rs` adds the route.
  * zen-proto `Frame::Ok` gains `conn: Option<bytes>`, with serde default and skipped when absent.
* **Tests (`tests/svc.rs`, both backends):**
  * a `tcp-auth` service helper, and a WebSocket connector helper (header or first-frame auth)
  * echo through the HTTP-driven handler:
    * the claim's peer is the connector's `{user, device}`
    * data both ways
    * the client's `"fin"` sets EOF
    * a FIN record arrives as `"fin"`
    * then close 1000, and the connection is reaped
  * first-frame auth works, and a bad first frame gets `err` `unauthorized`
  * refusals:
    * no token: 401
    * a member without `append`: 403
    * a `tcp` service or an unknown id: 404
    * over `max_conns`: 429
  * close and abort records: 1000 after the data; 4000
  * the client's close: the handler's next step gets `conn_closed`
  * the window: 300 KiB in 64 KiB frames to a 64 KiB window never puts `in_head` more than a window past `in_acked`, and all of it arrives as the handler acks
  * a frame over 64 KiB: close 1009
  * a takeover: the handler's lease lapses mid-stream, a second lease continues, and the transcript is exact
  * `tests/multinode.rs` (FDB): connect on node A and handle on node B

### `@zen/client`: `Session.svcConnect`
* `session.svcConnect({fs, svc: Uint8Array, WebSocket?}) → Promise<SvcSocket>`, in `packages/client/src/svc.ts`. It opens `Transport.wsUrl('/v1/svc/connect?…')`, authenticates with a first frame (`authToken`, shared with stream.ts, since a browser can't set headers), and waits for `ok` or `err` (a `ZenError` with the code).
* `SvcSocket`:
  * `conn`
  * `onData`, `onFin` and `onClose(reset: boolean)` callbacks
  * `write(bytes)` in frames of at most 64 KiB, and `bufferedAmount`
  * `fin()` and `reset()`
* **Backpressure:**
  * Outgoing: the caller pauses its source while `bufferedAmount` is over 1 MiB.
  * Incoming: the standard WebSocket can't pause reading, so what the server sends is buffered when the local reader is slower. Protocols with their own flow control (SSH) bound this. Logged as TD-SVC-CONNECT-BACKPRESSURE.
* **Tests (`packages/client/test/svcconnect.test.ts`),** against a `TestServer` with a `tcp-auth` service:
  * a raw handler driven through `fs.service()`'s HTTP requests echoes bytes
  * `fin` both ways
  * the refusals as `ZenError` codes: `forbidden`, `not_found`, `unauthorized`

### `zen-connect` (`packages/svc/src/connect-cli.ts`)
* **Usage:** `zen-connect --url URL --fs N --svc HEX [--listen HOST:PORT]`, plus a sign-in option.
  * Without `--listen`, it bridges stdin and stdout: ssh's `ProxyCommand`.
  * With `--listen`, it opens one connection per accepted socket.
* **Sign-in:** `--api-token-file` / `$ZEN_API_TOKEN`, `--device-key FILE`, or `--user NAME` with `$ZEN_PASSWORD`.
  * There are no TTY prompts: under `ProxyCommand`, stdin is the byte stream.
  * `--opaque` and `--vault` aren't supported; `zen-mount` keeps them.
  * In `--listen` mode it signs in again when the session is within 60 s of expiry.
* **Mapping between the two sides:**
  * local EOF → `fin()`; the server's `"fin"` → `socket.end()`
  * close 1000 → end, once written; 4000 or an error → `destroy()` (TCP reset)
  * a local error → `reset()`
* **Messages and exit codes:** errors print as `zen-connect: …`; exit 1 on failure, 2 on bad usage.
* **The build:**
  * `scripts/build-zen-connect.mjs` bundles it with esbuild into `packages/svc/dist/zen-connect.mjs`, as `build-apps.mjs` does for workers: `@zen/*` from sources, and `@zen/wasm` external.
  * `@zen/svc`'s `bin` is `{"zen-connect": "./dist/zen-connect.mjs"}`, and its `build` script runs both.
  * Tests run the script first.
  * It imports only `@zen/client` and `node:*`, so it builds without `@zen/db`.

### The echo app over `tcp-auth`
* **Harness:**
  * `ServiceSpec` and `WorldService` gain `mode` (`'tcp' | 'tcp-auth'`); `tcp-auth` has no port and no `listen`.
  * `World.serviceId(name)` gives the hex id.
  * The echo grants give members `append` on `svc/` too.
* **`SSH_AUTH_SERVICE` (`svc/echo-ssh-auth`) and `AUTH_SERVICE` (`svc/echo-auth`):** the same handlers, on `tcp-auth`.
* **`ECHO_LAYER`** gains `auth` (raw) and `ssh-auth`.
* **Vitest (`apps/echo/test/connect.test.ts`):**
  1. **ssh through `ProxyCommand`:**
     * `ssh -o ProxyCommand="node …/zen-connect.mjs … --device-key …" … lines` gives `1: …`, `bye` and exit 0; `exit 3` gives 3.
     * The `lines` rows record the connection.
     * Signed in as Bob without `append`: ssh fails, and zen-connect's stderr says `forbidden`.
  2. **`--listen` with the raw echo:**
     * a Node `net` client gets the replies, then `bye` and EOF after its own `end()`
     * two clients at once
  3. **A takeover through the connector:** 200 lines, 5 ms apart, through `--listen` to a worker that is SIGKILLed at line ~80. The transcript is exact, and the table holds 200 rows.
  4. **`git ls-remote`** through `GIT_SSH_COMMAND` with the `ProxyCommand`.
* Playwright: none. The browser is a handler as before. A browser connector (`svcConnect` from a tab) is possible, but no page uses it yet.

### CI
Nothing new. The `client` job already has `ssh`, and the tests build `zen-connect` themselves.

### Docs
* `docs/CLIENT.md` §11: `tcp-auth`, `svcConnect`, `zen-connect` with an `ssh_config` example (`ProxyCommand zen-connect --url … --fs 1 --svc … --device-key …`), and the rights.
* `apps/README.md`: echo over `tcp-auth`.
* The example configs: a `tcp-auth` example entry.
* TECH_DEBT: TD-SVC-CONNECT-BACKPRESSURE.
* The log in both plan copies.

### Out of scope
* the sealed inbox (5.5.12)
* resuming a connector's WebSocket after a network drop: the connection ends, as a TCP peer's would
* rights re-checked during a connection
* a browser page acting as a connector
* `--vault` and OPAQUE sign-in in `zen-connect`

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings`: default, `--no-default-features` and `--all-features`
* `cargo test -p zen-server --test svc` 3× on embedded and once on FDB; `tests/multinode.rs` on FDB; the full zen-server suite once (`tests/stream.rs` for the `Frame::Ok` change)
* the dev wasm build (the `Frame` type)
* `npm run check`; `npx vitest run` 3×
* `npx playwright test` once (unchanged)
* CI green on the PR

## Step 5.5.10 in detail: `ssh()` in `@zen/svc`

Handlers serve SSH. The 5.5.9 engine is built into `@zen/svc-wasm`, and `@zen/svc` gains an `ssh()` layer. An SSH session isn't one byte stream: it has authentication and channels. So the layer gives its handler an `SshStep` rather than a `RawStep`, and `serve()` becomes generic over the step type. The echo app gains an SSH service with:
* `cat` (the bulk test), `lines` (the line echo, with rows), and `exit <code>`
* a `git-upload-pack` stub, for the git-transport demo

A handler killed at any point (mid-transfer, mid-rekey, mid-key-exchange) is resumed by another.

**Commits:**
1. `docs: milestone 5.5 step 10 plan` (also logs step 5.5.9's CI and merge)
2. `ci: the OpenSSH-from-source job runs on demand only`: `workflow_dispatch:` in the triggers and `if: github.event_name == 'workflow_dispatch'` on `openssh-pq`. Already decided in 5.5.9 and validated there. It goes in now, with the plan, so ordinary builds stop building OpenSSH.
3. `svc: the SSH engine for wasm (@zen/svc-wasm)`
4. `svc: layers make their handler's step; the ssh() layer`
5. `apps: echo over SSH, and a git-upload-pack stub`
6. the docs and the log

### Facts from the survey
* **The runtime (`packages/svc/src/serve.ts`):**
  * The runtime builds the handler's `Step` (a `RawStep`) itself.
  * The layer turns `HandlerOut {acked, writes, endFlag}` into bytes in `finish(p, h)`; `committed(p, f, h)` follows.
  * `p.held && h.acked > 0` makes the step due again.
  * `p.call` gates the handler; `room(p, outWindow)` bounds its writes.
  * The handler type is fixed: `ServeOptions.handler: RawHandler`.
* **`crates/zen-svc-wasm/src/lib.rs`** (332 lines): TLS bindings. They build objects with `js_sys` and type them through `unchecked_return_type` and a `typescript_custom_section`. Errors are `ZenTlsError` with a code as the message.
* **The echo app:**
  * `workers/handler.ts` picks the layer by `ECHO_LAYER` (`raw`, `tls`, `mirror`).
  * The world harness passes `[[service]]` settings through `WorldService.more` (so `window = 1048576` for SSH).
  * `testCerts` makes TLS certificates with the openssl CLI.
* **Tools:** git 2.43 is here and on the runners. OpenSSH 9.6 is installed in the `rust` and `pure-rust` jobs, not yet in `client`.

### `@zen/svc-wasm`: the SSH bindings (`crates/zen-svc-wasm/src/ssh.rs`)
Errors are `ZenSshError`, codes `config`, `key`, `state`, `closed`, `no_channel`, `channel_ended`, `not_asked`.
* `new SshConfig(hostKeyPem, opts?)`:
  * the host key is OpenSSH's unencrypted Ed25519 PEM
  * `opts`: `{window?, rekeyBytes?, rekeySecs?, kex?: string[], ciphers?: string[], software?}`
* `sshHostKeyLine(pem) → string`, for `known_hosts`.
* `sshPublicKey(line) → {algorithm, fingerprint, blob}`, for an application's `authorized_keys`.
* **`SshConn`:**
  * `SshConn.start(config, now)` and `SshConn.restore(config, state)`. `now` is Unix seconds.
  * The greeting (version line and KEXINIT) comes out of the first `input`.
  * `input(buf, eof, now) → {consumed, out, events, peerClosed, error?: {code, reason?, detail}}`:
    * events: `{type: 'authorize' | 'open' | 'exec' | 'shell' | 'subsystem' | 'data' | 'eof' | 'close', ch?, command?, name?, …}`
    * error codes: `protocol` (we sent DISCONNECT, with its reason) and `disconnected` (the client did)
  * `pendingAuthorize() → {user, key: {algorithm, fingerprint, blob}, signed} | undefined` and `authorize(ok) → out`.
  * Channels:
    * `channels() → [{ch, request, eof, closed, unread, queued, peerWindow}]`
    * `channelData(ch)` and `consume(ch, n) → out`
    * `write(ch, data)` and `writeStderr(ch, data) → {taken, out}`
    * `exitStatus(ch, code)`, `eof(ch)` and `close(ch) → out`
  * `rekey()`, `disconnect(reason, message)`, `state()`.
  * Getters: `user`, `ended`, `inKex`, `kexes`.
* **Its own tests (`packages/svc/test/sshwasm.test.ts`, no zen server):**
  * A Node TCP server drives the binding, restoring it from its state at every read, with OpenSSH's `ssh` as the client: exec, `cat` of 1 MB with `RekeyLimit=256K`, a refused key, and the error codes.
  * The tests need `ssh` and say so if it's missing.

### `@zen/svc`: layers make their handler's step, and `ssh()`
* **Generalize the layer (`layer.ts`):**
  * `Layer<S>` and `LayerConn<S>` gain `open(p, ctx) → Live<S>`, where:
    * `ctx = {conn, peer, state, finSent, outWindow}`
    * `Live<S> = {view: S, app(): Uint8Array | null, finish(): Finished}`
  * `finish` and `committed` take the live step instead of `HandlerOut`.
  * `Finished` gains `again`: the step is due again at once. It replaces the runtime's `p.held && h.acked > 0`.
  * The raw `Step` moves out of `serve.ts` into `layer.ts`. `raw()` and `tls()` wrap it as before: `RawStep` and its behaviour don't change.
* **Make `serve` generic:** `serve<S = RawStep>(db, opts: ServeOptions<S>)`, with `layer?: Layer<S>` and `handler: (c: S, tx) => …`. Existing callers compile unchanged.
* **`ssh({hostKey, window?, rekeyBytes?, rekeySecs?, wasm?}): Layer<SshStep>`:**
  * engine name `ssh`; resumable from the first step; never fragile
  * The layer's `window` defaults to `(service window − 64 KiB) / 10`, from `/v1/svc/info`, as svc.md §7.2 says. `open()` gets the service info for this.
* **What the handler sees:**
  ```ts
  interface SshStep {
    readonly conn: Uint8Array; readonly peer: SvcPeer;
    state: Uint8Array | null;               // the application's, as RawStep's
    readonly user: string | null;           // once authenticated
    readonly question: SshQuestion | null;  // {user, key: {algorithm, fingerprint, blob}, signed}
    answer(ok: boolean): void;
    readonly events: SshEvent[];            // this step's: open, exec (command), shell, subsystem (name), data, eof, close
    readonly channels: SshChannel[];
    channel(id: number): SshChannel | undefined;
    disconnect(message?: string): void;
  }
  interface SshChannel {
    readonly id: number; readonly request: 'exec' | 'shell' | 'subsystem' | null;
    readonly eof: boolean; readonly closed: boolean;
    readonly data: Uint8Array;              // unread
    ack(n: number): void;
    write(d: Uint8Array | string): number;  // bytes taken (the window, the queue, out_window)
    stderr(d: Uint8Array | string): number;
    exit(code: number): void; end(): void; close(): void;
  }
  ```
* **How a step runs:**
  * `prepare` (outside the transaction) restores the committed state, feeds the input, and keeps:
    * `consumed`, `out` and the events
    * the state after the input
    * whether reading was held back
  * The engine is restored again for each transaction attempt, so every step is also a takeover test.
  * The handler runs on every step: a window adjust carries no event, but lets a writer go on.
  * `again` is set when:
    * reading was held back and the handler consumed, or answered a question
    * a write was cut short by `out_window` (not by the peer's window)
  * Writes are cut to the step's room, `out_window` minus the packet overhead.
  * An engine error sends its output (the DISCONNECT) and closes, as TLS's alerts do. An ended engine closes the connection.
* **Unit tests** (`packages/svc/test/sshstep.test.ts`): the `again` and room rules, on recorded `Prepared` values, if they separate cleanly; otherwise in the app tests.

### `apps/echo` over SSH
* **`src/schema.ts`:** schema 3 adds `ssh_keys` (user, key line), and the host key goes into `certs` under `ssh`.
  * `src/ssh.ts`: `sshLayer(db)`, and `sshEcho`, the handler:
    * authorizes a key when a `ssh_keys` row of that user has its blob
    * keeps each channel's command in the application state, since the engine gives it once
    * commands:
      * `cat`: mirror until EOF, then exit 0
      * `lines`: each line becomes `<n>: line` and a `lines` row, as echo
      * `exit <n>`: `bye` on stderr, then status n
      * `git-upload-pack '<path>'`: a v0 ref advertisement for `HEAD` and `refs/heads/main` (a fixed id, with capabilities), then exit 0 at the client's flush
      * anything else: exit 127
* **`workers/handler.ts`:** `ECHO_LAYER=ssh` (service `echo-ssh`).
* **Test keys:** the host and user keys come from `ssh-keygen` in a test helper (`testSshKeys()` in `@zen/client/testing`, beside `testCerts`).
* **Vitest (`apps/echo/test/ssh.test.ts`).** The services use `window = 1048576`, and the clients are OpenSSH `ssh` with a `known_hosts` file and `StrictHostKeyChecking=yes`.
  1. **In process:**
     * `lines` over stdin: the replies and the rows
     * `exit 3`: the status and `bye`
     * a key not in `ssh_keys`: `Permission denied (publickey)`
  2. **50 MB across two kills:**
     * `cat` of 50 MB of random bytes to a worker, with `RekeyLimit=4M`, so rekeys cross takeovers
     * the worker is SIGKILLed twice, and the SHA-256 of what came back equals what was sent
     * the test reports the workers' output on failure
  3. **A takeover mid-key-exchange:**
     * A TCP proxy forwards the client's version line and KEXINIT, then holds the rest until the worker has committed that step (its state shows a key exchange in progress) and is killed.
     * The successor computes the reply, and the `exec` completes.
  4. **git:** `git ls-remote ssh://test@127.0.0.1:<port>/repo.git`, with `GIT_SSH_COMMAND` carrying the options, prints the stub's two refs.
* **Playwright (`apps/echo/e2e/echo.spec.ts`):** an SSH case. Alice's tab serves `echo-ssh`, and a Node-spawned `ssh … lines` gets `1: …`. This checks the browser's module loading with the SSH code.

### CI
* The `client` job installs `openssh-client` (vitest and Playwright use `ssh`). git is already there.
* Nothing else: the bindings are in the module `scripts/build-wasm.sh` already builds.

### Docs
* `docs/CLIENT.md` §11:
  * `ssh()` and `SshStep`
  * the host key and authorized keys in the app's tables
  * the window rule
  * what a takeover keeps: everything, from the first step
* `apps/README.md`: echo over SSH, and the git demo.
* `docs/STATS.md` §4.1: the module's new size.
* The log in both plan copies.

### Out of scope
* `tcp-auth` and `zen-connect` (5.5.11)
* real git (milestone 10)
* host-key rotation
* a PTY (TD-SVC-SSH-SCOPE)

### Verification
* `cargo fmt --check`; clippy on zen-svc-wasm (native, `--no-default-features`, wasm32); `scripts/build-wasm.sh` release and `--dev`
* `npm run check`; `npx vitest run` 3×; `npx playwright test`
* `cargo test -p zen-svc` once (unchanged)
* CI green on the PR (without `openssh-pq`, now on demand)

## Step 5.5.9 in detail: the SSH engine (`crates/zen-svc`, native)

A sans-IO SSH server whose whole state serializes at every packet boundary, mid key exchange included. A successor handler continues the live session at any point. So unlike TLS, an SSH connection is resumable from its first step.

This step is native only. The wasm binding and `ssh()` in `@zen/svc` come in 5.5.10. Normative behaviour goes into spec/svc.md §7.2 (the `spec:` commit).

**Commits:**
1. `docs: milestone 5.5 step 9 plan` (also logs step 5.5.8's CI and merge)
2. `spec: the SSH engine (svc.md §7.2)`
3. `svc: the SSH engine (crates/zen-svc), native`
4. `ci: OpenSSH from source for the hybrid key exchange`
5. the docs and the log
6. `ci: the OpenSSH-from-source job runs on demand only`, pushed after commit 4's job is green on the PR (the user's instruction)

### Facts from the survey
* **OpenSSH here and on CI:** no `ssh` is installed locally; apt has `openssh-client` 9.6p1. Ubuntu runners ship 9.6.
  * 9.6 has `curve25519-sha256`, `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`, strict kex and EXT_INFO. It has no `mlkem768x25519-sha256`, which came in 9.9.
* **Building OpenSSH from source:**
  * cdn.openbsd.org and ftp.openbsd.org are blocked here, but `git clone --depth 1 --branch V_10_6_P1 https://github.com/openssh/openssh-portable` works. That tag is commit `a8c72b842423f0774f542d3c2197247c3dd28d74`.
  * `autoreconf -i && ./configure && make ssh ssh-keygen` takes about 10 s on 4 cores, with OpenSSL 3.0 and zlib from the system (`libssl-dev`, `zlib1g-dev`).
  * Its `ssh -Q kex` lists `mlkem768x25519-sha256`, and `ssh -Q cipher` both of our ciphers.
* **Crates already in the workspace (pure Rust, build for wasm32):**
  * chacha20 0.10 (feature `legacy`: `ChaCha20Legacy`, the 64-bit nonce openssh's cipher uses), poly1305 0.9, aes-gcm 0.11
  * x25519-dalek 3, ml-kem 0.3, ed25519-dalek 3, sha2 0.11, base64 0.22
  * rsa `=0.10.0-rc.18`, and `zen_tls_rustcrypto::rsakey::from_components(n, e)`, which applies the key policy (2048–4096 bits, e ≥ 65537) to an `ssh-rsa` blob's e and n as they are
* **The state budget:** the whole sealed state is at most `max_value_bytes` = 90,000 (svc.md §7). The engine's buffers must stay well under that, with room for the app's state.

### spec/svc.md §7.2 (`spec:`)
* **Algorithms:**
  * kex: `mlkem768x25519-sha256`, `curve25519-sha256` (and its `@libssh.org` alias)
  * host key: `ssh-ed25519`
  * ciphers: `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`; compression `none`
  * MACs are listed (`hmac-sha2-256-etm@openssh.com`) but never used: both ciphers are AEAD, and OpenSSH doesn't negotiate a MAC for an AEAD cipher
  * user keys: `ssh-ed25519`, and `rsa-sha2-256`/`rsa-sha2-512` on `ssh-rsa` keys within the key policy. `ssh-rsa` with SHA-1 is refused.
* **Strict kex** (`kex-strict-s-v00@openssh.com`, Terrapin, CVE-2023-48795), when the client offers `kex-strict-c-v00@openssh.com`:
  * the client's first packet must be KEXINIT
  * any other message during the initial kex disconnects
  * sequence numbers reset to 0 at each NEWKEYS, in each direction
* **EXT_INFO:** after the first NEWKEYS when the client offers `ext-info-c`, with `server-sig-algs` = `ssh-ed25519,rsa-sha2-256,rsa-sha2-512`.
* **Authentication:**
  * `publickey` only, with `none` answered by a FAILURE that lists it
  * a query without a signature gets PK_OK if the app authorizes the key
  * a signed request is verified first (over the session id, as RFC 4252 §7), then authorized by the app
  * at most 20 attempts, then disconnect
* **Connection:**
  * only `session` channels, at most 10 open
  * `exec`, `shell` and `subsystem` once per channel; `pty-req`, `env`, `x11-req`, `auth-agent-req` and other requests get FAILURE when a reply is wanted
  * global requests get REQUEST_FAILURE; port forwarding is refused
  * `exit-status`, EOF and CLOSE, and stderr as extended data type 1
* **Rekey:** on the client's KEXINIT at any time, and by the server after 1 GiB in either direction or one hour since the last kex. While a kex is in progress, the app's output waits in the engine.
* **Bounds:**
  * the engine stops reading packets once 32 KiB of channel data is unread by the app (its input acks stop there, as TLS does at 16 KiB)
  * a write takes at most what fits the peer's window plus a 32 KiB queue, and returns how much it took; the app keeps the rest (in its state, or by not acking its own input)
  * the advertised channel window is 2 MiB, with a maximum packet of 32,768; packets up to 256 KiB are accepted
* **When it becomes resumable:** from the first step. Everything persists: the version strings, both KEXINITs during a kex, keys and sequence numbers, the next keys waiting for the client's NEWKEYS, the auth state (an authorization in progress included), the channels with their windows and buffers, and the rekey counters. The server's ephemeral kex secrets never persist: the reply is computed in the step that receives the client's kex message.
* **The `engine_state` layout** (CBOR, versioned); a version it doesn't know is refused.

### `crates/zen-svc` (`ssh` module)
* **Dependencies:** those above (chacha20 with `legacy`, poly1305, aes-gcm, x25519-dalek, ml-kem, ed25519-dalek, sha2, base64, rsa, the rsakey policy). All are already in Cargo.lock.
* **Files:**
  * `ssh/mod.rs`: `Config`, `Conn`, `Event`, `Error`, `Progress`
  * `ssh/wire.rs`: SSH encodings (string, mpint, name-list) with strict decoding
  * `ssh/cipher.rs`: the two packet protections
  * `ssh/kex.rs`: KEXINIT negotiation, both kex methods, the exchange hash, key derivation (RFC 4253 §7.2), the host key signature
  * `ssh/keys.rs`: `HostKey` and `PublicKey`
  * `ssh/auth.rs`, `ssh/channel.rs`
  * `ssh/state.rs`: the CBOR state
* **The packet protections:**
  * **chacha20-poly1305@openssh.com:** a 64-byte key, of which the main key is the first 32 bytes and the header key the last 32. The nonce is the sequence number. The length is encrypted with the header key. The Poly1305 key is the main key's block 0, and the payload is encrypted from block 1. The tag covers the encrypted length and payload.
  * **aes256-gcm@openssh.com:** the length in the clear as AAD, and a 12-byte IV of 4 fixed bytes plus an 8-byte invocation counter incremented per packet (kept in the state); padding to 16.
* **The kex methods:**
  * `curve25519-sha256`: K as an mpint (RFC 8731).
  * `mlkem768x25519-sha256` (draft-ietf-sshm-mlkem-hybrid-kex):
    * the client's C_INIT is the ML-KEM-768 encapsulation key (1184 bytes) followed by its X25519 key
    * the server's S_REPLY is the ciphertext (1088 bytes) followed by its X25519 key
    * K = SHA-256(ML-KEM shared secret ‖ X25519 shared secret), as a string
  * Both refuse an all-zero X25519 result.
* **Keys:**
  * `HostKey::from_openssh(pem)` reads an unencrypted `openssh-key-v1` Ed25519 key, as `ssh-keygen -t ed25519 -N ''` writes it. Also `HostKey::from_seed(&[u8; 32])` and `public_line()` (for `known_hosts`).
  * `PublicKey::from_line("ssh-ed25519 AAAA… comment")` and `from_blob`, `fingerprint()` (`SHA256:…`, as `ssh-keygen -l`), equality on the blob: what an app needs to check an `authorized_keys` list.
* **API:**
  ```rust
  Config::new(host_key, Options { kex, ciphers, rekey_bytes, rekey_secs, software /* "zen" */ })
  Conn::new(&config, now_secs) -> (Conn, Vec<u8>)  // version line + KEXINIT: the server speaks first
  Conn::restore(&config, state) -> Conn
  conn.input(buf, eof, now_secs) -> Progress { consumed, out, events, peer_closed, error }
  // events: Authorize { user, key: PublicKey, signed }, ChannelOpen { ch },
  //         Exec { ch, command } | Shell { ch } | Subsystem { ch, name },
  //         Data { ch } (read with channel_data/consume), Eof { ch }, Close { ch }
  conn.authorize(accept) -> Vec<u8>       // answers a pending Authorize; input stops until then
  conn.channel_data(ch) -> &[u8]; conn.consume(ch, n)   // sends WINDOW_ADJUST as the window drains
  conn.write(ch, data) / write_stderr(ch, data) -> (taken, out)
  conn.exit_status(ch, code), conn.eof(ch), conn.close(ch), conn.rekey(), conn.disconnect(reason, msg)
  conn.state() -> Vec<u8>
  ```
  * A pending `Authorize` is in the state, so a successor asks its app again.
  * Protocol errors send DISCONNECT with the RFC 4253 reason code and end the connection, like TLS's fatal alerts.
  * A test-only `Options` knob lowers the rekey thresholds.
* **The version exchange:**
  * we send `SSH-2.0-zen` (configurable software name)
  * we accept a client line `SSH-2.0-…` or `SSH-1.99-…` of at most 255 bytes, after at most 20 other lines
  * the version strings go into every exchange hash, so they stay in the state

### Tests
* **The harness** (`tests/common`, extended), driving the engine the way a handler does:
  * after every `input`, it serializes the state, drops the connection and restores it, so every packet boundary is a takeover point, mid-kex included
  * variants feed whole reads, one packet at a time, and one byte at a time
  * a small app on top: `echo <text>`, `cat` (stdin to stdout until EOF), `big <n>` (n bytes out), `exit <code>` (with stderr), a `test-sub` subsystem, and a line-echo shell
  * the authorized keys are the test's Ed25519 and RSA keys
* **`tests/openssh.rs`, against OpenSSH (`ssh` on PATH, or `ZEN_TEST_SSH`):**
  * The listener thread drives the engine and restores it at every read.
  * The client runs with `-T`, `UserKnownHostsFile` holding our host key and `StrictHostKeyChecking=yes`, so the host key signature is checked.
  1. exec with an Ed25519 user key, for each cipher on `curve25519-sha256`: the stdout and the exit status
  2. RSA user keys with `rsa-sha2-256` and `rsa-sha2-512`. Refused: an unauthorized key, and `ssh-rsa` (SHA-1) only (`Permission denied (publickey)`).
  3. `cat` of 10 MB with `-o RekeyLimit=1M` (about ten client rekeys), checked by SHA-256
  4. server-initiated rekeys during a 5 MB transfer, with the lowered threshold
  5. `big 20M` (window adjusts from the client)
  6. the shell over stdin; `ssh -s … test-sub`; a non-zero exit status with stderr
  7. **Terrapin:**
     * a proxy injects an SSH_MSG_IGNORE into the client's stream during the initial kex, and `ssh` fails with our DISCONNECT
     * a raw client sending IGNORE before its KEXINIT is disconnected under strict kex
     * the same IGNORE is accepted without strict kex
* **`tests/openssh_pq.rs`:** the same exec and a 1 MB `cat` on `mlkem768x25519-sha256`.
  * Marked `#[ignore = "needs OpenSSH 9.9+ (ZEN_TEST_SSH)"]`, since the system `ssh` can't run it.
  * Runs with `ZEN_TEST_SSH=… cargo test -p zen-svc --test openssh_pq -- --ignored`: here against the 10.6p1 build, and in CI's dedicated job.
* **`tests/ssh.rs`, engine-level:**
  * malformed input from a deterministic generator over real transcripts (bit flips, lengths, truncation, bad padding, unknown messages): a DISCONNECT or progress, never a panic, and the state stays restorable or the connection ends
  * the strict-kex rules
  * the auth attempt limit
  * a pending `Authorize` across a restore
  * the read bound (32 KiB unread stops reading)
  * the write bound and the queue across a closed window
  * `HostKey`/`PublicKey` parsing against `ssh-keygen` output
  * a state round trip; refusal of unknown versions and bad lengths; a state-size bound with full buffers (well under 90,000)

### CI
* `rust` and `pure-rust` jobs: `sudo apt-get install -y openssh-client` (already on the runners; this makes it explicit). `cargo test --all` then runs `tests/openssh.rs`.
* **New job `openssh-pq`:**
  * `scripts/build-openssh.sh <prefix>`: clone tag `V_10_6_P1` with depth 1, check the commit `a8c72b84…`, then `autoreconf`, `configure`, `make ssh ssh-keygen`
  * cached by version with `actions/cache`
  * then `ZEN_TEST_SSH=<prefix>/ssh cargo test -p zen-svc --test openssh_pq -- --ignored`
* **After it passes on the PR** (the user's instruction): commit 6 adds `workflow_dispatch:` to the workflow's triggers and `if: github.event_name == 'workflow_dispatch'` to the job. Ongoing builds then skip it; it runs when started by hand. CI is confirmed green after that push too.
* `rust` job's wasm32 check: zen-svc already builds for wasm32; the new dependencies must too.

### Docs
* README crates table: `crates/zen-svc` gains the SSH engine.
* TECH_DEBT:
  * **TD-SVC-SSH-SCOPE:** no password or keyboard-interactive auth, no PTY, no port or agent forwarding, no ECDSA or security-key (`sk-…`) user keys, no `publickey-hostbound`
  * **TD-SVC-SSH-PQ-CI:** the hybrid kex is tested against OpenSSH only on demand, until the runners' OpenSSH has it
* The log in both plan copies.

### Out of scope
* the wasm build and `ssh()` in `@zen/svc`, the git-transport demo and the takeover tests across handler kills (5.5.10)
* host-key rotation and certificates; `sntrup761x25519-sha512`

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings`: default, `--no-default-features`, `--all-features`; clippy on zen-svc for wasm32
* `cargo test -p zen-svc` 3× with OpenSSH 9.6 (apt), and the `openssh_pq` tests with the 10.6p1 build, here
* `cargo tree -p zen-svc -e normal -i ring` is empty
* CI green on the PR, with `openssh-pq` green before commit 6, and again after it

## Step 5.5.8 in detail: `@zen/svc-wasm` and the `tls()` layer

Handlers serve TLS: the step-5.5.7 engine is built for wasm as a second module, loaded only by services that use it, and `@zen/svc` gains a `tls()` layer between the relay's bytes and the app's handler. The handler sees plaintext through the same `RawStep` interface as a raw handler, so the echo handler runs over TLS unchanged. A handler killed mid-stream is resumed from the engine state; one killed mid-handshake leaves a connection the server aborts (svc.md §7.1, TD-SVC-TLS-HANDSHAKE-RESUME).

**Commits:**
1. `docs: milestone 5.5 step 8 plan` (also logs step 5.5.7's CI)
2. `svc: the TLS engine for wasm (crates/zen-svc-wasm, @zen/svc-wasm)`
3. `svc: the tls() layer in @zen/svc`
4. `apps: echo over TLS`
5. the docs and the log

No `spec:` commit is expected: the engine, its state and when a connection becomes resumable are already in svc.md §7.1. If the runtime finds a gap, it gets one.

### Facts from the survey
* **The first module:**
  * `scripts/build-wasm.sh` builds only `crates/zen-wasm` into `packages/zen-wasm/pkg` (wasm-bindgen `--target web`, with the version pinned from Cargo.lock: `=0.2.129`).
  * `packages/zen-wasm` is just a `package.json`: name `@zen/wasm`, `exports` `"."` and `"./zen_wasm_bg.wasm"`.
* **Loading the first module:** `packages/client/src/wasm.ts` `initWasm(source?)` memoizes one promise.
  * Node: `createRequire(...).resolve('@zen/wasm/zen_wasm_bg.wasm')`, `readFile`, then `initSync`.
  * Browsers: the glue's default, which fetches the file next to `import.meta.url`.
* **App bundles (`scripts/build-apps.mjs`):**
  * The `wasm` list copies a `.wasm` next to `main.js` when the metafile inputs show its glue.
  * Node workers keep `@zen/wasm` external and load it from node_modules.
  * `loro-crdt` is the precedent for a second module.
* **zen-wasm's conventions:**
  * Opaque handles are `#[wasm_bindgen] pub struct` newtypes, freed with `free()` from JS.
  * Errors are a `js_sys::Error` named `ZenCryptoError` with a stable code as the message.
  * `u64` maps to bigint; objects are returned typed through `unchecked_return_type`.
* **`serve.ts`:**
  * One `db.transaction` per step, whose callback may run more than once (conflicts).
  * `'raw'` is hard-coded in two places: the state's `engine` check in `load`, and `encodeState` in `build`.
  * `resumable` goes on the first step.
  * A step is due on new input, on an unseen EOF, or on `due` for a fresh connection.
* **CI:**
  * The `client` job runs `scripts/build-wasm.sh` (release), installs wasm-bindgen through `taiki-e/install-action`, runs vitest and Playwright (smoke and apps).
  * The `rust` job checks zen-svc for wasm32.
  * The runners have `openssl`.
* **Size:** the first module is 2.14 MB (589 KB gz) without wasm-opt (TD-CLIENT-WASM-OPT, STATS §4.1).
* **rustls-pki-types** (1.15) parses PEM with its default `alloc` feature (`PemObject::from_pem_slice`, `pem_slice_iter`).

### `crates/zen-svc-wasm` (new) and `@zen/svc-wasm`
* **The crate:**
  * A workspace member: `crate-type = ["cdylib", "rlib"]`, `wasm-bindgen = "=0.2.129"`, `js-sys`, and zen-svc.
  * zen-svc already turns on getrandom's `wasm_js` and rustls-pki-types' `web` for wasm32.
* **Bindings,** following zen-wasm's conventions. Errors are named `ZenTlsError`, with the codes `config`, `state`, `handshaking`, `closed`, `key_limit`, `unsupported`.
  * `TlsConfig`:
    * `new TlsConfig(certPem: Uint8Array, keyPem: Uint8Array, opts?: {alpn?: Uint8Array[], tls12?: boolean, tls13?: boolean, keyLimit?: number})`
    * The PEM holds the chain, leaf first, and the key: PKCS#8 or SEC1. An RSA key or a mismatch gives `config`.
  * `TlsConn`:
    * `TlsConn.start(config)` and `TlsConn.restore(config, state)`.
    * `input(buf, eof) → {consumed: number, out: Uint8Array, established, peerClosed, truncated, held, error?: {code, alert?}}`. `held`: the plaintext reached `PLAIN_HELD`, so complete records may be waiting.
    * `plaintext()`, `consume(n)`, `write(data)`, `keyUpdate(requestPeer)`, `close()`, `state() → Uint8Array | undefined`.
    * Getters: `established`, `version` (`'TLSv1.3'` or `'TLSv1.2'`), `suite` (the IANA name), `sni`, `alpn`.
  * `PLAIN_HELD`.
  * Handles are freed with `free()`; zen-svc's keys zeroize on drop.
* **`scripts/build-wasm.sh`** builds both modules in turn: `zen-wasm` into `packages/zen-wasm/pkg`, `zen-svc-wasm` into `packages/svc-wasm/pkg`, printing both sizes. `--dev` applies to both.
* **`packages/svc-wasm`** (`@zen/svc-wasm`):
  * A `package.json` like `@zen/wasm`'s, with `exports` `"."` and `"./zen_svc_wasm_bg.wasm"`.
  * Wiring: a root `workspaces` entry, `package-lock.json`, `.gitignore` (`packages/svc-wasm/pkg/`), biome's ignore list, and a `@zen/svc` dependency.
* **App bundles:** `scripts/build-apps.mjs` adds the module to its `wasm` copy list (keyed on `svc-wasm/pkg/zen_svc_wasm.js`) and to the Node workers' externals.

### `@zen/svc`: layers and `tls()`
* **API:**
  ```ts
  serve(db, { service, handler, layer?: Layer /* default: raw() */, ... })
  raw(): Layer
  tls(opts: {
    cert: string | Uint8Array;   // PEM: the chain, leaf first
    key: string | Uint8Array;    // PEM: PKCS#8 or SEC1 (ECDSA P-256/P-384, Ed25519)
    alpn?: string[];
    versions?: ('1.2' | '1.3')[];  // default both
    wasm?: URL | string | Response | BufferSource;  // where the module is, if not next to the glue
  }): Layer
  ```
  * `RawStep` gains `readonly tls?: {version, suite, sni, alpn, truncated}`, set under `tls()`. So `RawHandler` serves both layers, and the echo handler runs over TLS unchanged.
  * The handler's PEM comes from wherever the app keeps it; the echo app keeps it in a zen-db table, which is sealed like every row.
* **Loading (`src/tls/wasm.ts`):**
  * `loadSvcWasm(source?)` is memoized and imports `@zen/svc-wasm` dynamically, so the glue loads only when `tls()` is used.
  * Node reads the `.wasm` through `createRequire` + `initSync`; browsers use the glue's fetch next to `import.meta.url`, or `source`.
  * The layer loads it and builds its `TlsConfig` once, when `serve()` starts. A bad certificate or key is reported through `onError`, and nothing is claimed.
* **The runtime (`serve.ts`), changed so a layer sits between bytes and handler:**
  * **The engine name** in the state is the layer's (`raw` or `tls`). A claim whose state names another engine is released with `close` (as today for raw).
  * **Before each attempt, outside the transaction,** the layer prepares the step from the input:
    * The raw layer passes the input through.
    * TLS feeds the raw bytes (from `in_acked`) to the engine and keeps:
      * `consumed` (the step's input ack)
      * the engine's `out` (handshake flights, KeyUpdate answers, alerts)
      * `established`, `peerClosed`, `truncated`, `held`
      * the engine state after input
    * Established: the engine is restored from the last committed state on each attempt, so every step is also a takeover test.
    * Handshaking: the `TlsConn` lives on the held connection between steps, because it can't be serialized. Its output and ack for this step are computed once and reused if the transaction runs again.
  * **Inside the transaction** (which may run again):
    * The TLS layer restores a `TlsConn` from the prepared state.
    * The handler gets the plaintext as `input`. `eof` is `peerClosed || truncated`, and the handler can tell them apart through `c.tls.truncated`.
    * The handler's `ack(n)` consumes plaintext. Its `write`s are encrypted. `end()` sends close_notify, then FIN; `close()` sends close_notify, then close; `abort()` resets without close_notify.
    * The step's records carry the engine's `out` followed by the encrypted writes. Its state is `{engine: 'tls', engine_state, app}`. The object is freed.
    * During the handshake the handler isn't called (no plaintext yet), unless the handshake finished in this step and data came with it.
  * **`resumable`:**
    * Raw: on the first step, as now.
    * TLS: on the step that finished the handshake.
    * Until then the connection has no state, and a lease loss aborts it on the server (svc.md §6).
  * **Failures:**
    * A fatal engine error sends `out` (the alert) with a close record, without calling the handler, and reports through `onError`.
    * During the handshake, `cursor_moved`, `commit_unknown` or `network` after retries releases the connection with `close` instead of resyncing (svc.md §7.1).
    * An established connection resyncs as today, restoring from the re-read state.
  * **Progress without input:** when the engine held records back (`held`) and the handler consumed plaintext, the step is due again at once.
  * **Cleanup:** wasm objects are freed when a connection is forgotten or given up, and on `stop()`.
* **Unit tests (`packages/svc/test/tlswasm.test.ts`, no server):**
  * The binding against Node's own TLS client, `tls.connect({socket})` over an in-memory duplex, with the engine as the server, restored from its state after every input.
  * 1.3 and 1.2, data both ways, a client KeyUpdate where Node allows it, close_notify.
  * Error codes: a bad PEM, an RSA key, a write while handshaking, a bad state.
  * Also: the layer's prepare and step logic on a fake transaction, if it can be separated cleanly; otherwise it's covered by the app tests.

### `apps/echo` over TLS
* **`src/echo.ts`:**
  * `TLS_SERVICE = ['svc', 'echo-tls']`, and `serveEcho(db, {tls?: {cert, key}})` passes `layer: tls(...)`.
  * A `mirror` handler, which writes back all input and acks it, with no rows, for the bulk test, on `['svc', 'mirror-tls']`.
  * A `certs` table (name → `{chain, key}`) holds the certificate and key.
* **`workers/handler.ts`:** `ECHO_LAYER=tls` (or `mirror`) reads the `certs` row and serves that service.
* **`ui/`:** a second toggle serves `echo-tls` from the tab.
* **Test certificates:** made with the `openssl` CLI (P-256, SAN `localhost` and `127.0.0.1`) by a helper in `apps/shared`. It fails with a clear message when `openssl` is missing.
* **Vitest (`apps/echo/test/tls.test.ts`),** with Node `tls.connect` peers trusting the test CA, `servername: 'localhost'`:
  1. **The line echo over TLS 1.3 and over 1.2** (`maxVersion: 'TLSv1.2'`), in-process:
     * the replies, the rows, and `bye` on the client's `end()`
     * the peer sees close_notify, then EOF
     * the negotiated version and suite on both sides
  2. **SIGKILL mid-stream:**
     * 200 lines, 5 ms apart, to a worker; it is killed at line ~80 and a second worker takes over (lease TTL 1 s).
     * The TLS client sees no error, the transcript is `1:`…`200:` exactly once and in order, and the table holds 200 rows.
  3. **SIGKILL mid-handshake:**
     * A TCP proxy in the test forwards the ClientHello and the server's flight, then holds the client's second flight until the worker is killed.
     * The server aborts the connection once the lease lapses, so the client gets an error or close, not a hang.
     * A new connection then works with the successor.
  4. **20 MB across two kills:**
     * The client sends 20 MB of random bytes to `mirror-tls` and reads 20 MB back.
     * The worker is killed twice during the transfer; the SHA-256 of what came back equals what was sent.
* **Playwright (`apps/echo/e2e/echo.spec.ts`):** a TLS case. Alice's tab serves `echo-tls`; a Node TLS peer gets `1: …`. This checks the browser loading path: build-apps copies the second module and the glue fetches it.

### CI
* `client` job: `scripts/build-wasm.sh` now builds both modules; nothing else changes.
* `rust` job's wasm32 check: `-p zen-svc-wasm`.

### Docs
* `docs/CLIENT.md` §11: `tls()`, `RawStep.tls`, where certificates live, and what a takeover keeps (established connections) and loses (handshakes).
* `apps/README.md`: echo over TLS.
* `docs/STATS.md` §4.1: the second module's size (release, gzipped).
* TECH_DEBT TD-CLIENT-WASM-OPT: the second module.
* README crates and packages tables: `crates/zen-svc-wasm`, `packages/svc-wasm`.
* The log in both plan copies.

### Out of scope
* SSH (5.5.9–5.5.10); `tcp-auth` (5.5.11); the sealed inbox (5.5.12)
* An admin helper to put certificates into the fs, and ACME (TD-SVC-ACME); SNI-based certificate selection; client certificates; RSA server keys
* wasm-opt (TD-CLIENT-WASM-OPT)

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings` default, `--no-default-features`, `--all-features`; clippy on `zen-svc-wasm` for wasm32
* `scripts/build-wasm.sh` (release) and `--dev`
* `npm run check`; `npx vitest run` 3× (all packages and apps, including the new tests)
* `npx playwright test` (smoke and apps)
* `cargo test -p zen-svc` once (unchanged)
* CI green on the PR

## Step 5.5.7 in detail: the TLS engine (`crates/zen-svc`, native)

A sans-IO TLS server for relayed services:
* The handshake runs through rustls's unbuffered server.
* After the handshake, the engine's own record layer takes over, and its state serializes to bytes. A successor handler restores it at any record boundary and continues the live connection.

This step is native only. The wasm build, the JS glue and `tls()` in `@zen/svc` come in 5.5.8. Normative behaviour goes into spec/svc.md §7.1 (the `spec:` commit).

**Commits:**
1. `docs: milestone 5.5 step 7 plan`
2. `spec: the TLS engine (svc.md §7.1)`
3. `tls: TLS 1.2 suites and a record API in zen-tls-rustcrypto`
4. `svc: the TLS engine (crates/zen-svc), native`
5. the docs and the log

### Facts from the survey (rustls 0.23.45)
* **`UnbufferedServerConnection::process_tls_records(&mut buf) → {discard, state}`.**
  * Handshake fragments are reassembled in place in the caller's buffer.
  * rustls's own buffers are `received_plaintext` (app data decrypted after the handshake), `sendable_tls`, and the read and write sequence numbers.
  * `is_handshaking()` turns false once the client Finished is verified (1.3). App data in the same flight then arrives as `ReadTraffic` before the first `WriteTraffic`.
* **`dangerous_into_kernel_connection`:**
  * It needs `enable_secret_extraction`, a finished handshake and an empty `sendable_tls`.
  * It does **not** check `received_plaintext`, so the engine drains `ReadTraffic` first.
  * It returns `ExtractedSecrets {tx: (seq, ConnectionTrafficSecrets), rx}`: key and IV only, not the traffic secrets.
  * For 1.2 GCM the IV is the 4-byte salt followed by 8 bytes of rustls's "extra" key block.
* **TLS 1.3 traffic secrets** come only from `KeyLog` (`CLIENT_TRAFFIC_SECRET_0`, `SERVER_TRAFFIC_SECRET_0`), which is called with the `client_random`. KeyUpdate needs them: `HKDF-Expand-Label(secret, "traffic upd", "", Hash.len)`, then `"key"` and `"iv"`.
* **Resumption off:** `send_tls13_tickets = 0`, `NoServerSessionStorage`, the default ticketer (never issues tickets). `max_early_data_size` and `send_half_rtt_data` already default to off.
* **TLS 1.2:**
  * It needs rustls's `tls12` feature, a `Tls12CipherSuite` (`PrfUsingHmac`, `Tls12AeadAlgorithm` with `KeyBlockShape`), and `require_ems` (default false outside FIPS).
  * ring's GCM explicit nonce is `extra XOR seq`. The record AAD is `seq ‖ type ‖ version ‖ len`.
* **`AeadKey`** can only be built from outside rustls as a 32-byte key (`with_length` is crate-private). So the engine can't build the provider's AES-128 encrypters through rustls's traits. Hence a byte-level record API in zen-tls-rustcrypto (below).
* **Limits:** GCM's confidentiality limit is `1 << 24` records per key, ChaCha has none. At the limit, 1.3 rekeys and 1.2 closes.
* zen-server's listener pins TLS 1.3 (`tls/mod.rs:128`), so turning on `tls12` changes nothing there.
* OpenSSL 3.0.13 is installed locally, and on CI's ubuntu runners. Its `s_client` has `-tls1_2`/`-tls1_3`, `-ciphersuites`/`-cipher`, `-groups`, and the interactive `k`/`K` key-update commands. It has no X25519MLKEM768 (3.5+).

### zen-tls-rustcrypto
* **Feature `tls12`** (turns on `rustls/tls12`):
  * Suites `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`, `…_AES_256_GCM_SHA384` and `…_CHACHA20_POLY1305_SHA256`. Only ECDSA suites, since this provider signs ECDSA and Ed25519 but not RSA (TD-TLS-RSA-SERVER-KEY).
  * `provider_tls12()` gives the 1.3 suites plus these. `provider()` is unchanged, so zen-server stays on 1.3.
  * The groups are as today. The hybrid already refuses 1.2 (`usable_for_version`).
* **The 1.2 GCM explicit nonce** is a random 32-bit prefix followed by the low 32 bits of the sequence number.
  * Within a connection it is unique by construction: GCM closes before `2^24` records.
  * Under a state rollback (TD-SVC-STATE-ROLLBACK) a replayed sequence number repeats a nonce only with probability `2^-32` per record. svc.md §12 now says this instead of "random 64-bit".
  * ChaCha20-Poly1305 1.2 is RFC 7905 (`iv XOR seq`).
* **A byte-level record API** (`pub mod record`), shared by the provider's encrypters and the engine:
  * `Aead` (`Aes128Gcm`, `Aes256Gcm`, `Chacha20Poly1305`) and `RecordKey::new(aead, key: &[u8], iv: &[u8])` (zeroized on drop).
  * `seal13(seq, typ, plain) → record` and `open13(seq, record) → (typ, plain)`, with the inner content type and zero padding.
  * `seal12(seq, typ, plain)` and `open12(seq, typ, record)`.
  * `expand_label(hash, secret, label, ctx, len)` (HKDF-Expand-Label).
  * The provider's `Sealer`/`Opener` (1.3) and the new 1.2 ones call it, so rustls's handshake records and the engine's records come from the same code.
* **Tests:**
  * the existing ones
  * RFC 8448 known answers for `expand_label` (traffic key and IV) and for a 1.3 record
  * 1.2 round trips and tamper checks
  * the 1.2 nonce layout
  * 1.2 handshakes between this provider and itself (rustls client and server, every suite and group)

### `crates/zen-svc` (new; `tls` module)
* **Dependencies:** rustls (`std`, `tls12`), zen-tls-rustcrypto (`tls12`), ciborium, serde, zeroize. Dev-dependencies: rcgen (certificates in tests).
* **`tls::Config::new(chain: Vec<CertificateDer>, key: PrivateKeyDer, opts)`** (`alpn`, versions; both on by default). The base `ServerConfig`:
  * `provider_tls12()`; versions 1.3 and 1.2; no client auth
  * `require_ems = true`; `enable_secret_extraction = true`
  * `send_tls13_tickets = 0`; `NoServerSessionStorage`
  * The default time provider, which works on wasm32 thanks to step 6's web-time.
* **`tls::Conn`**, one per connection:
  * **`Conn::new(&Config)`**
    * Clones the base config with a per-connection `KeyLog` that captures the two 1.3 traffic secrets.
    * Starts an `UnbufferedServerConnection`.
  * **`Conn::restore(state: &[u8])`** rebuilds an established connection. A handshaking connection has no state: `state()` is `None` until the handshake is done.
  * **`input(&mut self, buf: &[u8], eof: bool) → Progress`**, where `Progress` has:
    * `consumed`: input bytes processed, always at a record boundary, which is the step's input ack
    * `out`: bytes to send (handshake flights, alerts, a KeyUpdate answer)
    * `established`: true on the call that finished the handshake (the step that sets `resumable`)
    * `peer_closed` (close_notify), `truncated` (EOF without close_notify), and `error` (a fatal alert was queued in `out`; the connection is closed)
  * **Plaintext:**
    * `plaintext()` gives what was decrypted and not yet consumed; `consume(n)` consumes it.
    * The unconsumed plaintext is kept in the state. The engine stops decrypting (and acking) once it holds 16 KiB, so the state stays under about 33 KiB.
  * **`write(&mut self, data) → Vec<u8>`** splits `data` into records of up to 16 KiB.
    * 1.3 rekeys itself before the confidentiality limit.
    * 1.2 sends close_notify there and refuses further writes.
    * A test-only `Config` knob lowers the limit.
  * **`key_update(&mut self, request_peer: bool) → Vec<u8>`** (1.3 only).
  * **`close(&mut self) → Vec<u8>`** sends close_notify.
  * **`state(&self) → Option<Vec<u8>>`.**
  * The established `Session` is `Clone`, so a runtime can run a step on a copy and drop it if the commit fails (5.5.8).
* **The handshake:**
  * The engine drives rustls's states: `EncodeTlsData` into `out`, `TransmitTlsData` then `done()`, and `ReadTraffic` drained into the plaintext buffer.
  * Once `is_handshaking()` is false, nothing is queued, and the plaintext is drained, it calls `dangerous_into_kernel_connection`:
    * 1.3: key and IV are re-derived from the captured traffic secrets and must equal the extracted ones. A mismatch is an internal error (fail closed, abort). The sequence numbers come from the extraction.
    * 1.2: key and salt come from the extraction.
  * The rest of the buffer then goes through the engine's own record layer in the same call.
  * Mid-handshake, `input` isn't idempotent: rustls has moved on. A runtime keeps a handshake step's `Progress` until it commits, and re-sends it on a retry (5.5.8). A `cursor_moved` or unknown commit during the handshake aborts the connection.
* **The record layer:**
  * **Header and length checks:** the 5-byte header; ciphertext at most `2^14 + 256` (1.3) or `2^14 + 2048` (1.2), else `record_overflow`.
  * **Decrypt failures** get `bad_record_mac`.
  * **1.3 inner types:**
    * application data → plaintext
    * handshake → KeyUpdate only (it updates rx, and answers with our own KeyUpdate when the peer requests one); any other handshake message → `unexpected_message`
    * close_notify → `peer_closed`; user_canceled is ignored; other warning alerts are `decode_error`, as in rustls; fatal alerts end the connection
    * ChangeCipherSpec → `unexpected_message`
  * **1.2:** a handshake record (renegotiation) gets a `no_renegotiation` warning, as rustls does, and is otherwise ignored.
  * **Errors** queue an encrypted fatal alert and close.
  * **An incomplete record** waits for more input. At EOF it is `truncated`.
* **State** (`engine_state`, CBOR, svc.md §7.1):
  ```
  {v: 1, ver: 0x0304|0x0303, suite: u16,
   tx: {seq, secret (1.3) | key + iv (1.2)}, rx: {…},
   plain: bytes, sni?: text, alpn?: bytes,
   flags: peer_closed | sent_close | closed}
  ```
  * Keys are zeroized on drop.
  * Restoring a state checks the version, the suite and the field lengths. A bad state is refused, and the runtime releases with close.

### spec/svc.md §7.1 (`spec:`)
* **Versions and suites:** versions 1.3 and 1.2; the 1.2 suites are ECDHE AEAD only and require EMS.
* **Refused:** no tickets, resumption, early data or renegotiation, no post-handshake authentication, no client certificates. A 1.3 post-handshake message other than KeyUpdate gets `unexpected_message`.
* **When it becomes resumable:** once the handshake is done. A connection mid-handshake is not resumable.
* **Acks:** input is acked at record boundaries, and the unconsumed plaintext (≤ 16 KiB plus one record) is carried in the state.
* **Limits:** at the GCM confidentiality limit, 1.3 sends KeyUpdate and 1.2 sends close_notify.
* **The 1.2 GCM explicit nonce:** the prefix and sequence-number rule above.
* **The `engine_state` layout** above; a version it doesn't know is refused.
* **§12** gets the new nonce wording.

### Tests (`crates/zen-svc/tests/tls.rs`, native)
* **A pump drives the engine the way a handler would:**
  * After every `input` call it serializes the state, drops the `Conn` and restores it. So **every record boundary is a takeover point**.
  * Variants feed whole flights, one record at a time, and one byte at a time.
* **rustls clients** on `provider_tls12()`, with certificates from rcgen on RustCrypto keys (P-256, P-384, Ed25519):
  * 1.3 with every suite and group, and 1.2 with every suite, on X25519 and P-256
  * an echo of 1 MB both ways, with the transcript checked
  * a second connection gets a full handshake (no tickets)
* **openssl `s_client`** against a TCP listener whose thread drives the engine (restoring at every read):
  * `-tls1_3` with each `-ciphersuites`, and `-tls1_2` with each `-cipher`
  * `-groups X25519` and `P-256`
  * data both ways
  * `k` and `K` (client KeyUpdate, with and without a requested update)
  * a 1.2 `R` (renegotiation) refused without the engine breaking
  * The tests need `openssl` and say so if it's missing.
* **KeyUpdate:**
  * from the client: rustls `refresh_traffic_keys` if 0.23.45 has it on `ClientConnection`, and openssl `k`/`K`
  * from the server: `key_update(true)` and `key_update(false)`
  * automatic, at a lowered limit, in 1.3; 1.2 closes there
* **Cross-check:** for every 1.3 suite, the keys derived from the captured secrets equal `into_kernel_connection`'s, at the switch and after each KeyUpdate. The latter is checked against a client-side session built from the same secrets.
* **Malformed input**, with a deterministic generator over established transcripts: bit flips, truncation, length fields, bad types, oversized records, a handshake message that isn't KeyUpdate, a ChangeCipherSpec. Each case ends in progress or a specific alert, never a panic, and the state stays restorable or the connection is closed.
* **Refusals:**
  * no early data accepted
  * a 1.2 client without EMS is refused: openssl with `Options = -ExtendedMasterSecret` through `OPENSSL_CONF`, if 3.0 takes it, otherwise a rustls-level check that `require_ems` is set
  * an RSA server key is refused by `Config::new`
* **State:** a state round trip, refusal of unknown versions and bad lengths, and a state size bound with 16 KiB of plaintext held.

### CI
* `rust` job: `cargo check -p zen-svc --target wasm32-unknown-unknown`.
* The tests run in `cargo test --all` (rust) and in `pure-rust` (`--no-default-features`); both runners have openssl.

### Docs
* README crates table: `crates/zen-svc`.
* TECH_DEBT: **TD-SVC-TLS-HANDSHAKE-RESUME** (a handshake in progress dies with its handler, and the connection is aborted), created here rather than in 5.5.14.
* The provider-review entry notes that the hybrid isn't tested against OpenSSL (3.0 has no ML-KEM).
* The log in both plan copies.

### Out of scope
* the wasm build, the JS time provider (not needed after step 6), and `tls()` in `@zen/svc` (5.5.8)
* client certificates for relayed services; SNI-based certificate selection (one chain per `Config`); RSA server keys (TD-TLS-RSA-SERVER-KEY)
* X25519MLKEM768 against an independent peer (OpenSSL 3.5 isn't on the runners)

### Verification
* `cargo fmt --check`; clippy `--all-targets -D warnings`: default, `--no-default-features`, `--all-features`
* `cargo test -p zen-tls-rustcrypto --all-features`; `cargo test -p zen-svc` 3×
* zen-server `tests/tls.rs` and `tests/mtls.rs` (default and `--no-default-features`), since the provider's 1.3 encrypters move onto the record API
* `cargo check -p zen-svc -p zen-tls-rustcrypto --target wasm32-unknown-unknown`
* `cargo tree -p zen-svc -i ring` is empty
* CI green on the PR

## Step 5.5.6 in detail: the shared TLS provider crate

The TLS engine (5.5.7, native; 5.5.8, wasm) needs a rustls crypto provider that builds for wasm32 with no C compiler. zen-server already has one: `crates/zen-server/src/tls/rustcrypto.rs` (RustCrypto primitives, TLS 1.3, X25519MLKEM768/X25519/P-256, ECDSA/Ed25519/RSA verification, ECDSA/Ed25519 signing), which uses the RSA key policy in `src/rsakey.rs`. This step moves both into a crate of their own, `crates/zen-tls-rustcrypto`, so the engine can depend on them without zen-server. It is a move: **no behaviour changes**, and the server's TLS and mTLS tests stay as they are.

**Commits:**
1. `docs: milestone 5.5 step 6 plan`
2. `tls: the RustCrypto rustls provider in its own crate (zen-tls-rustcrypto)`
3. the docs and the log

There's no `spec:` commit: no behaviour or format changes.

### The crate
* `crates/zen-tls-rustcrypto/`, a workspace member:
  * `src/lib.rs` is the provider (`git mv` of `tls/rustcrypto.rs`, so its history follows): `provider()`, the suites, the groups (`X25519`, `SECP256R1`, `X25519MLKEM768`), verification, `Keys`.
  * `src/rsakey.rs` is the key policy (`git mv` of `src/rsakey.rs`), as `pub mod rsakey`.
* **Dependencies:** what those files use, moved from zen-server's manifest with the same versions and features:
  * rustls (`std`), aes-gcm, chacha20poly1305, hmac, sha2, x25519-dalek, p256, p384, ml-kem, ed25519-dalek, ecdsa
  * `rsa` (still pinned `=0.10.0-rc.18`, with its comment)
  * getrandom 0.4, with `wasm_js` on wasm32, as in zen-core
  * zen-server keeps the ones it uses itself (p256 and rsa for WebAuthn, and so on).
* **Feature `test-utils`:** `rsakey::testing` (`OsRng`, `public_der`, `private_der`, `pkcs8_der`). It also gains `rsa_key(seed, bits)`, the deterministic test key, moved from `webauthn::soft::rsa_key` together with its `XofRng` (blake3), with the same derivation label so the keys don't change. `webauthn::soft::rsa_key` calls it.
* **Its own tests:** the provider's unit tests move with it. They used `crate::webauthn::soft::rsa_key`, and now use `rsakey::testing::rsa_key`.

### zen-server
* `Cargo.toml`: depends on `zen-tls-rustcrypto`; feature `test-utils` turns on `zen-tls-rustcrypto/test-utils`. Dependencies only the moved code used are dropped.
* **Paths stay as they were:**
  * `tls/mod.rs`: `pub use zen_tls_rustcrypto as rustcrypto;`, so `tls::rustcrypto::provider()` (`tls::provider`, `tests/tls.rs`) is unchanged.
  * `lib.rs`: `pub use zen_tls_rustcrypto::rsakey;`, so `crate::rsakey` (`webauthn.rs`, `tls/ring.rs`) and `zen_server::rsakey::testing` (`tests/common/pki.rs`, `tests/tls.rs`) are unchanged.
  * So `tests/tls.rs`, `tests/mtls.rs` and `tests/common/pki.rs` aren't edited.
* `tls/ring.rs` stays in zen-server: it's ring's provider with the shared `X25519MLKEM768` and the key policy in front.

### CI
* The `rust` job's wasm32 check adds `cargo check -p zen-tls-rustcrypto --target wasm32-unknown-unknown`. This checks now that the provider builds where 5.5.8 needs it.
* `cargo test --all` and the `pure-rust` job (`--no-default-features`) cover the crate as a workspace member.

### Docs
Paths that name `tls/rustcrypto.rs` or `rsakey.rs` change to the crate:
* spec/TECH_DEBT.md (TD-TLS-RSA-SERVER-KEY, the hybrid's X25519, the provider review)
* spec/operations.md §8's provider table, if it names a path; `tls::rustcrypto` stays valid
* this plan's survey line
* the README crates table gets a row
* the log in both plan copies

### Out of scope (5.5.7)
* TLS 1.2 suites (ECDHE AEAD with EMS) and a 1.2-capable provider for the engine. They come with the engine that uses them, tested against real clients there. zen-server's own listener stays TLS 1.3 only.
* RSA signing in the pure-Rust provider: still refused (TD-TLS-RSA-SERVER-KEY). Until that's resolved, the engine's certificates are ECDSA or Ed25519.

### Verification
* `cargo fmt --check`
* clippy, `--all-targets -D warnings`: default, `--no-default-features`, `--all-features`
* `cargo test -p zen-tls-rustcrypto` (the moved unit tests)
* `cargo test -p zen-server --test tls --test mtls`, default (ring and the two providers against each other) and `--no-default-features`
* the full zen-server suite once
* `cargo check -p zen-tls-rustcrypto --target wasm32-unknown-unknown`
* `cargo tree -p zen-server --no-default-features -i ring` is still empty: the pure build stays pure
* `npx vitest run apps/echo` and the client TLS tests (`packages/client/test/mtls.test.ts`) once against the rebuilt binary
* CI green on the PR

## Step 5.5.5 in detail: `@zen/svc`, raw

This step gives handlers a TypeScript runtime: `serve(db, {service, handler})` holds a handler lease, claims connections, gets their input pushed over the stream, runs the app's handler in **one `db.transaction`** with the step, reveals after the commit, and re-reveals on a takeover. Only the raw layer: the handler sees bytes. It is demonstrated by a new example app, `apps/echo`, whose handler runs as a Node worker or in a browser tab. Normative behaviour: svc.md §4–§8 and §10, api.md §14, formats.md §13.1.

**Commits:**
1. `docs: milestone 5.5 step 5 plan`
2. `db: a delivery whose commit failed on the network is retried, not nacked` (the CI fix below, done before "go")
3. `wasm: svc seals and the svc request codecs`
4. `client: svc_sub on the stream; a svc step makes a transaction non-empty`
5. `svc: @zen/svc, the raw handler runtime`
6. `apps: echo, a relayed line service`
7. the docs and the log

No `spec:` commit is expected; if the code finds a gap in svc.md, it gets one.

### CI fix first: the kanban offline test (PR #41's `client` job)
On the plan commit, one of the two `client` runs failed:
* **The test:** `apps/kanban/test/kanban.test.ts:380`, "offline edits merge field by field", timed out after 15 s waiting for "the index to follow the move".
* **Its log** shows `consume $index: network: offline (test)`.
* The other run on the same commit passed, and the PR changes only docs. The failure isn't new code's, but its cause is real, in `@zen/db`.

**Root cause** (`packages/db/src/broker.ts`, `ConsumerHandle.one`):
1. The CRDT indexer is a `per_key` consumer (`$index`, `ixer.ts:96`). When the test switches the workers' sessions offline, a delivery the indexer is handling at that moment fails its commit with `network`.
2. `one()` then nacks it (`:742`), and the nack fails with `network` too (`nack()` only reports it).
3. So the claim stays on the key until `claim_ttl_ms` (default 30 s; the apps harness keeps the default).
4. `per_key` delivers a key's events in order, so the card's later change events wait behind the stale claim for up to 30 s. The test waits 15 s.
5. It passes when no delivery was in flight at the moment the sessions went offline.

The same happens in production: a short network failure while a handler commits stalls that key until the claim TTL.

**Fix:**
* In `one()`, when the handler's transaction fails with `network` (the transport failed, not the handler), run the delivery again with the loop's backoff (100 ms doubling to 5 s) instead of nacking. It stops when the stop signal fires or on any other error.
* This is safe because the step carries the claim token:
  * If an earlier attempt's commit did apply, the retry's consume step gets `claim_lost` or `cursor_moved`, which `dropped()` already treats as done. Nothing is applied twice.
  * Once the server has redelivered the event, the stale token gets `claim_lost` the same way.
* `attempts` isn't counted for a network failure, so an outage can't dead-letter a message.
* `nack()` also retries on `network`, with the same backoff, for the case where the handler really failed but the nack can't reach the server.
* `one()` gets the loop's `signal`.

**Test:**
* A new `packages/db/test/network.test.ts`, with its own `world()` at the default 30 s claim TTL, and a local `net()` offline switch like the one in `crdtoffline.test.ts`.
* Setup: a `per_key` consumer handles two events of one key. During the first one, the handler switches the session offline, so that commit fails with `network`, and switches it back on 500 ms later.
* Expected: both events are handled, each exactly once in the table, within 5 s. Without the fix, the second event waits for the 30 s claim TTL.
* The test is written and run first, and must fail on the current code before the fix goes in.

**Verify:**
* the new test, plus `packages/db` and `apps/kanban` (the kanban offline test 5×)
* `npm run check`
* push to PR #41, and CI green on both runs

**Out of scope:**
* the TLS and SSH layers, and the second wasm module (5.5.7–5.5.10)
* `tcp-auth` (5.5.11) and the inbox (5.5.12): a sealed `svc_in` chunk makes the runtime release the connection with `close`
* delegated keys (`K_svc` given to a worker without fs keys): the binding takes them (`SvcKeys.fromParts`), but `serve()` derives keys from the fs only
* `svc_reveal` from the client: reveals go over HTTP, which is idempotent and already retried. The stream variant can come later if measurements show a gain.
* a line helper for text protocols (5.5.13 needs one; the echo app splits lines itself)

### Gaps found in the survey (fixed here)
* zen-wasm exposes none of zen-core's `svc` functions, and `wire.rs` lists none of the `Svc*` request and response types. (The stream frames and `Commit.svc` already encode, through `Frame` and `Commit`.)
* `Transaction.empty` (`packages/client/src/kv.ts:546`) ignores `extra.svc`, so a transaction holding only a step would not commit.
* `DbTransaction.crdtOnly()` (`packages/db/src/txn.ts:523`) ignores `svc`, so a CRDT write plus a step would be queued and the step dropped.
* `Stream` (`packages/client/src/stream.ts`) drops unknown frames in its `default` branch and has no way to subscribe to a lease.
* A service's id is the keyed topic id of `svc/<name>`, which only an unlocked fs can compute, but the server needs it in its config at start. The test harness therefore restarts the server once the fs exists (below).

### zen-wasm
* **`SvcKeys`**, an opaque handle (zeroized on `free()`), like `FsKeys` and `TopicKeys`:
  * `FsKeys.svcKeys(topic: TopicKeys): SvcKeys` (zen-core `SvcKeys::new`)
  * `SvcKeys.fromParts(fsId, epoch, id, key)` (delegated keys, for later)
  * getters `fsId`, `epoch`
  * `sealState(conn, inAcked: bigint, outHead: bigint, plaintext)` and `openState(…)`
  * `sealRecord(conn, off: bigint, flags, raw) → {record, key}`
  * `openKeyEscrow(conn, off: bigint, escrow) → key` (for a claim's `pending`)
* `plainRecord(flags, raw)` and the flag constants (`SVC_FIN`, `SVC_CLOSE`, `SVC_ABORT`).
* Which keys open a state or an escrow: the epoch from `peekSealed`, as `UnlockedFs.keysFor` does today. A test checks that `peekSealed` reads an escrow's epoch; if it can't, the binding gains `escrowEpoch(escrow)`.
* `wire.rs` adds codecs for `SvcInfoRequest`/`SvcInfo`, `SvcLeaseRequest`/`SvcLeaseResult`, `SvcNextRequest`/`SvcClaims`, `SvcStateRequest`/`SvcClaim`, `SvcReadRequest`/`SvcReadResult`, `SvcReleaseRequest` and `SvcRevealRequest`/`SvcRevealResult`.
* Tests (vitest, `packages/client/test/wasm.test.ts` or a new `svc.test.ts`): a state round trip, and refusal at other cursors; a record sealed in TS opens through `openKeyEscrow`, and its `key` equals the escrowed one; the vectors in `spec/test-vectors/svc.json` match.

### @zen/client
* **`Stream.svcSub(fs, svc, lease, {input, ready, end}) → {close()}`**, a third subscription kind next to `sub` and `esub`:
  * `svc_in` calls `input({conn, off, data, sealed, flags})`; `svc_ready` calls `ready()`; an `err` with its id calls `end(error)` and drops the subscription.
  * After a reconnect, it is sent again. The server then pushes from `in_acked` again, and says `svc_ready` if the queue holds connections, so the runtime only dedupes by `off`.
  * `close()` sends `unsub`.
* **`kv.ts`:** `empty` is false when `extra.svc` has a step. **`packages/db` `txn.ts`:** `crdtOnly()` is false when the commit has a step.
* Tests in `packages/client/test/stream.test.ts`: a `svc_sub` gets `svc_ready` and `svc_in`, survives a forced reconnect (the pushes restart at `in_acked`), and ends with `not_leader` when its lease is released.

### `packages/svc` (`@zen/svc`), wired like `packages/loro`
Root `workspaces`, the `check` script, `vitest.config.ts` and `scripts/build-apps.mjs` aliases, `apps/tsconfig.json` paths, the CI typecheck line, and `package-lock.json`.

**API:**
```ts
serve(db: Db, opts: {
  service: string[];        // the topic path, e.g. ['svc', 'echo']; its topic id is the service id
  handler: RawHandler;
  maxConns?: number;        // held at once (default 64)
  leaseTtlMs?: number;      // default 5000; renewed every renewMs (default 1000)
  renewMs?: number;
  onError?: (e: unknown) => void;
}): { stop(): Promise<void>; readonly conns: number }

type RawHandler = (c: RawStep, tx: DbTransaction) => void | Promise<void>;
interface RawStep {
  readonly conn: Uint8Array;
  readonly peer: SvcPeer;
  readonly input: Uint8Array;      // the input not yet acked, from in_acked
  readonly eof: boolean;           // the peer sent FIN: `input` is all there is
  state: Uint8Array | null;        // the app's state; carried across steps and takeovers
  ack(n: number): void;            // consume the first n bytes (end at a protocol unit)
  write(data: Uint8Array | string): void;
  end(): void;                     // FIN after the writes
  close(): void;                   // close after the writes
  abort(): void;                   // reset
}
```

**How it runs:**
* **Owner gating:** it starts on `db.onOwner`, like `ConsumerHandle`, so one tab of a browser serves and the others don't.
* **Start-up:**
  * `/v1/svc/info` gives `resume`, `window` and `out_window`, and `/v1/info` gives `max_value_bytes`.
  * It takes a lease with a random 16-byte id, opens a `svcSub`, and renews the lease every `renewMs`.
  * If renewal fails until the TTL has passed, or gives `not_leader`, every held connection is dropped locally and it starts over with a new lease id. Since the server requeues them, nothing is lost.
* **Claims:** `/v1/svc/next` with `limit = maxConns − held` on each `svc_ready`, and again while the last call came back full.
* **On a claim:**
  * **The rollback check (svc.md §6):** a claim whose `(in_acked, out_head)` is below what this process saw for the connection is released with `close`, and `onError` is told.
  * **The state:** its CBOR `{v: 1, engine: "raw", engine_state: h'', app}` (svc.md §7) is opened with the `SvcKeys` of its epoch. If it doesn't open, the connection is released with `close`.
  * **Pending records** are re-revealed: each escrow is opened with `openKeyEscrow`, then sent to `/v1/svc/reveal`.
  * **Input** pushed for a connection before its claim response arrives is kept briefly, up to `window` bytes per connection, then dropped after 10 s.
* **Input:**
  * `svc_in` chunks are joined from `in_acked` by `off`; overlaps are dropped.
  * A connection has at most one step in flight. A new step runs when there is input the handler hasn't seen, or an EOF it hasn't seen.
* **A step,** inside `db.transaction(async tx => …)`:
  * The handler runs. Its writes become records of at most `max_value_bytes − 130` raw bytes: sealed with a fresh key each on a `resume` service, plain otherwise.
  * Its ack becomes `in_to`, and its flags become a last record (FIN, close or abort).
  * The state is re-sealed at the new cursors whenever one exists, so a step that moves a cursor always carries it (svc.md §4).
  * `resumable: true` is sent on a connection's first step on a `resume` service: raw has no handshake, so it is resumable at once.
  * The `SvcStep` goes into `tx.raw.extra.svc`. A step that changes nothing is left out, and the handler's db writes still commit.
  * A conflict re-runs the whole transaction, handler included, and re-seals with new keys (a new `commit_id`).
* **After the commit** (`onCommit`):
  * The cursors and state advance and the acked input is dropped.
  * The record keys are revealed (HTTP, retried).
  * After close or abort, the connection is forgotten.
* **Errors:**
  * `claim_lost`, `conn_closed`: forget the connection.
  * `cursor_moved`, or a `commit_unknown` still unresolved after `sendCommit`'s retries: re-read the connection with `/v1/svc/state` and continue from it. That attempt's keys are never revealed; if the commit applied, its records come back as `pending` escrows.
  * `window` (429): retry the step with backoff (20 ms up to 500 ms). A handler writes at most `out_window` bytes per step; more throws.
  * A handler that throws: the connection is released with `close`, and `onError` is told.
* **`resume: false`:** plain records, no reveal, no `resumable`. A lost claim aborts the connection on the server.
* **`stop()`:** stops claiming, waits for the steps in flight, then releases the lease (the server requeues or aborts its connections).

**Unit tests** (`packages/svc/test/input.test.ts`): joining pushed chunks (overlaps, a first chunk before `in_acked`, gaps held back), and the state encoding.

### `apps/echo`: a relayed line service
* `src/echo.ts`, the handler:
  * each complete line `l` becomes the reply `<n>: l\n`, with the count `n` kept in the state
  * a row `{conn, n, line}` goes into the table `lines`, in the same transaction
  * input is acked only up to the last newline
  * on EOF, `bye\n` and FIN
* `workers/handler.ts` runs it as a Node worker (`runWorker`, `signInWorker`); `ui/` signs in, serves from the tab on a button, and lists `lines` live.
* **The harness** (`apps/shared/harness.ts`): `WorldOptions.services` (name, path, `resume`, port).
  * Once the fs exists, `startWorld` computes each service's topic id, stops the server (`kill`, keeping its data), and starts it again in the same data directory and on the same port, with `[[service]]` entries on free ports. `World.servicePort(name)` gives the port.
  * Members get topic `consume` on `svc/` by default.
* **Vitest** (`apps/echo/test/echo.test.ts`), with Node `net` peers:
  1. **Echo in-process:**
     * `a\nb` then `c\n` gives `1: a\n2: bc\n`: a partial line stays unacked across steps.
     * The rows match the replies, and the peer's FIN gives `bye\n`, then EOF.
  2. **SIGKILL mid-stream:**
     * A peer sends 200 lines, 5 ms apart, to a worker; the worker is killed at line ~80 and a second worker takes over (lease TTL 1 s).
     * The peer's transcript is `1: …` to `200: …`, each exactly once and in order.
     * The table holds exactly 200 rows, `n` = 1..200.
  3. **Atomicity across kills:** as in 2, with three kills at random points. Every reply has its row, every row's reply arrives, and nothing is doubled.
  4. **Two handlers share connections:** Alice's and Bob's `serve()` with `maxConns: 10`, 20 peers. All are echoed, and each handler served some.
  5. **`resume: false`:** a second service; the echo works, and killing its handler closes the peer's connection once the lease lapses.
* **Playwright** (`apps/echo/e2e/echo.spec.ts`): Alice's tab serves; a Node `net` peer sends a line, gets `1: …`, and the line appears in the tab's list.

### Docs
* `apps/README.md`: the echo app.
* `docs/CLIENT.md`: a short `@zen/svc` section (`serve`, the handler, keys, what a takeover keeps). The full guide comes with 5.5.14.
* The log in both plan copies.

### Verification
* `cargo fmt --check`; clippy on zen-wasm; the dev wasm build
* `npm run check`
* `npx vitest run` 3× (all packages and apps)
* `npx playwright test` (smoke and apps)
* the Rust suites are untouched except zen-wasm, so `cargo test -p zen-wasm` (if it has tests) and one `cargo test -p zen-server --test svc`
* CI green on the PR

## Facts the design rests on (code survey)

* **Config:** `config.rs:11` `Config` (`deny_unknown_fields`); `LimitsConfig` :298; `validate` :443; the example configs are parse-tested (:524).
* **Startup:** `lib.rs:437` `start()` binds one `TcpListener` (:476), then spawns `acl::follow`, the sweeper and axum. `Server` has one `JoinHandle` and `abort` (:431); there is no graceful shutdown.
* **Consumer claims** (`consume.rs`):
  * `deliver` :585–648 (`kc`/`kr`/`km`, `claim_ttl_ms` default 30 s)
  * `next` long-poll :691, which watches `lh`
  * `check_token` :304, returning `not_leader` / `claim_lost`
  * `consume_step` :374, called from `commit.rs:339`
* **Commit** (`commit.rs`):
  * `validate` :60: op counting, 512 B per op
  * `execute` :250 in one `txn_loop!`: the `cid` replay, reads, writes, consume, append, chunks, pages, crdt_ops, quota (:415–437), then the `cid` record
  * A new field means: zen-proto `Commit` (serde default), `validate`, `execute`, `keys.rs`, a `/v1/info` feature (`lib.rs:68`), and wasm `wire.rs`.
* **The stream** (`stream.rs`): `Frame` is tagged by `op`, the first frame must be `auth`, at most 256 subscriptions per socket. A subscription arms `store.watch(head)` before range-reading. The ephemeral ring is in `eph.rs`.
* **Watches:** the store hub keeps one watch per key, and FDB allows about 10k watches per client (`crates/zen-store/src/fdb.rs:11`). So **no watch per connection**: per-node and per-lease doorbells instead.
* **Auth:** `auth.rs` `resolve` :240 and `Caller` :89. Rights bits `R_READ` 1, `R_APPEND` 4, `R_CONSUME` 8. Topic grants are prefix-ORed (`acl.rs:86`); `require_topic` is at `auth.rs:146`.
* **Keyspace:** no `r*` prefix is in use.
* **rustls 0.23.45** (`~/.cargo/registry/src/*/rustls-0.23.45/`):
  * `UnbufferedServerConnection`.
  * `dangerous_into_kernel_connection` (`server_conn.rs:987`; the older `dangerous_extract_secrets` is deprecated) needs `enable_secret_extraction`, a finished handshake and nothing left to send (`conn.rs:1198–1235`). It returns `(seq, key+iv)` per direction.
  * After that, KeyUpdate state is not serializable (`conn/kernel.rs:118–148`). **So we capture the traffic secrets with a per-connection `KeyLog`** (`key_log.rs:30–34`) and do HKDF-Expand-Label for the key, IV and `traffic upd` ourselves.
  * `send_tls13_tickets` must be 0 (default 2, `builder.rs:123`). Early data stays 0.
  * On wasm32, `ServerConfig::builder_with_details(provider, time_provider)` (`server_conn.rs:535`) takes JS time; the default time provider panics there.
* **Crypto crates** already in the workspace: aes-gcm, chacha20poly1305, x25519-dalek, ed25519-dalek, sha2, hmac, blake3, ml-kem, x-wing. The RustCrypto rustls provider is `crates/zen-tls-rustcrypto` (since step 5.5.6; it was zen-server's `tls/rustcrypto.rs`, with `rsakey.rs`). There is no SSH crate.
* **Client:**
  * `kv.ts` `Transaction`: `extra: Partial<Commit>`, the `onCommit`/`onError` hooks, `build()`. `empty()` must learn every new field.
  * `log.ts` `Consumer` (claims, tokens).
  * `broker.ts` `ConsumerHandle`: owner gating via `db.onOwner`; `one()` is `db.transaction` plus the ack in `tx.raw`.
  * Wire types come from the zen-proto `ts` feature, listed in `crates/zen-wasm/src/wire.rs`.
  * Node workers: `apps/shared/node.ts` (`signInWorker`), `apps/checkout/src/worker.ts`.

## Design

### Services and modes
`[[service]]` in `zen-serve.toml`:
* **Identity:** `name`, `fs`, `id` (a topic id, 16·n bytes, hex; conventionally `svc/<name>`). Rights are checked as **topic rights on `id`**, so the existing prefix grants apply: a handler needs `consume`; a `tcp-auth` connector needs `append`.
* **Mode:**
  * `tcp`: a public `listen` address.
  * `tcp-auth`: no port of its own. Clients open a WebSocket `/v1/svc/connect?svc=<id>` on zen-serve's own listener with a Bearer token. The `zen-connect` CLI turns it into a local port, or works as an ssh `ProxyCommand`.
* **Limits:** `max_conns`, `per_ip` (accepts per second and burst), `window` (unacked input bytes; at least 64 KiB, default 256 KiB, 1 MiB for SSH), `idle_secs`, `handshake_secs`, `accept_wait_ms` (how long a connection waits for a handler before it is closed).
* `resume` (default true).
* `inbox` (an X-Wing public key, optional, below).

### Connection lifecycle (server)
1. **Accept:** the per-IP token bucket is checked before any write. A connection record `rc` is written: node id and boot epoch, peer address, the time, `in_head` 0, `in_acked` 0, `out_head` 0, `written_to` 0, flags. It is pushed onto the service's ready queue `rq`.
2. **Input:** the owning node is the only writer. It reads the socket in batches (up to 32 KiB or 5 ms) into `ri(fs, svc, conn, u64 start offset)` and advances `in_head`, in one transaction. It stops reading once `in_head − in_acked ≥ window` (TCP backpressure). A peer FIN becomes an EOF marker; a reset becomes an RST marker.
3. **Claim:** a handler holds a **handler lease** `rl` (renewed every 1 s, TTL 5 s; O(handlers), not O(connections)). It claims ready connections with `/v1/svc/next` (a long poll, or pushed over the stream). The claim token is bumped on every new claim. A connection becomes claimable again when its handler's lease lapses or the handler releases it.
4. **The step,** in `/v1/commit` as a new `svc` list of `SvcStep {fs, svc, conn, token, in_from, in_to, out_from, out: [record], state?, resumable?, unseal?}` (close, FIN and abort are record flags; normative: spec/svc.md §4). It is atomic with everything else in the commit (zen-db rows, events, consume steps). It checks and does:
   * the claim token is current (else `claim_lost`, 412)
   * `in_from == in_acked` and `out_from == out_head` (else `cursor_moved`)
   * `in_to ≤ in_head` (a snapshot read: `in_head` only grows)
   * clears input below `in_to`
   * appends the output records at `out_head`
   * writes the sealed protocol state (`rs`, seal kind 9)
   * applies close flags
   * rings the owning node's doorbell
5. **Reveal (the two-phase release):** each output record is `key_hash(k_r) ‖ escrow(k_r) (seal kind 10) ‖ AEAD(k_r, raw)` (formats.md §13.1), with `k_r` fresh and random **per record per commit attempt** (never derived from the offset). The server stores a revealed key in `rv` and decrypts at write time. Only after the commit is known to have succeeded (`commit_unknown` is replayed with the same `commit_id` first) does the handler send `reveal {conn, seq, k_r}`. The server checks `BLAKE3(k_r)` and the AEAD tag, so **only committed bytes can ever reach the wire**. Reveals need no token, only `consume`, because revealing a committed record is always safe. A successor re-reveals by opening `seal_fs(k_r)`.
6. **Writing:** the owning node watches one doorbell key per node. It writes revealed records in order from `written_to` (an unrevealed record blocks the ones after it). After `write_all`, one transaction clears the records and advances `written_to`. Close markers: `shutdown_write` (FIN once everything before it is written) or `close`. Before a close, unread input is drained, so the kernel doesn't reset the last bytes. A failed socket write marks the connection `peer_gone`, and the handler's next step gets `conn_closed`. Fast path: a reveal that arrives on the owning node is written at once.
7. **Node failure:** a socket is lost with its node (inherent to TCP). Nodes hold a node lease. The sweeper reaps connections of dead or old-epoch nodes, and a node reaps its own old epoch at boot.
8. **`resume: false`:** the output is sent raw in the step (fenced by the token), there is no reveal, and a claim change closes the connection.

### Takeover rules
* A successor loads `rs` (the sealed state), re-reveals committed records that weren't written, reads input from `in_acked` and continues.
* **Input is acked only at record or packet boundaries** (the end of a complete TLS record or SSH packet), so the persisted state holds no partial records, only the sequence numbers and bounded partial application data (half an SMTP line).
* A connection becomes **resumable** only after the first committed state. For TLS that comes after the handshake (rustls handshake state can't be serialized), so a TLS connection whose handler dies mid-handshake is closed by the server. The SSH engine is ours and persists everything (ephemeral kex secrets, the exchange hash inputs, the session id), so SSH resumes anywhere, even mid-kex or mid-rekey.

### Protocol engines (Rust, sans-IO, a separate wasm module)
`crates/zen-svc`, built to `@zen/svc-wasm` and loaded lazily, so ordinary clients don't download it. The engine interface: `step(state, input) → {consumed_to, app_events, out_bytes, state}` and `send(state, app_bytes) → {out_bytes, state}`, with state as CBOR. Sealing stays in TypeScript through the existing zen-wasm.
* **TLS** (1.3 and 1.2):
  * The rustls unbuffered server on the RustCrypto provider (moved to a shared crate) with JS time.
  * A per-connection `KeyLog` captures the traffic secrets (1.3) or the key block (1.2).
  * `dangerous_into_kernel_connection` gives the sequence numbers and a cross-check, after which our own record layer takes over:
    * TLS 1.3: AES-GCM and ChaCha20-Poly1305, KeyUpdate both ways (self-initiated before the AES-GCM confidentiality limit), `close_notify`, alerts. Any post-handshake message but KeyUpdate gets an `unexpected_message` alert.
    * TLS 1.2: the ECDHE AEAD suites only (explicit nonce for GCM, RFC 7905 for ChaCha), extended master secret required, no renegotiation.
  * `send_tls13_tickets = 0`, no early data.
  * Application data the client sends right after Finished is handed to the app before the switch.
* **SSH** (our own server):
  * Kex: `curve25519-sha256` and `mlkem768x25519-sha256`. Host key: `ssh-ed25519`.
  * Ciphers: `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`.
  * Strict kex (`kex-strict-s-v00@openssh.com`, Terrapin): sequence numbers reset at each NEWKEYS, and an unexpected message during the first kex is fatal.
  * EXT_INFO with `server-sig-algs` when the client offers `ext-info-c`.
  * Publickey userauth through a callback, with ed25519 and RSA-SHA2 user keys.
  * Session channels: exec, shell and subsystem; window and max packet; EOF and close; exit-status.
  * Rekey on the client's request, and by the server after 1 GiB or 1 h.
  * Persisted: keys, sequence numbers, channel windows, and output queued while the window is 0.

### Client runtime `@zen/svc` (TypeScript)
* **`serve(db, {service, handler, maxConns, resume})`:**
  * owner-gated like consumers (`db.onOwner`)
  * holds the handler lease; claims connections and gets input pushed over the stream
  * per connection, runs the engine, then the app's handler in **one `db.transaction`** whose `tx.raw.extra.svc` carries the step
  * reveals after the commit, re-reveals on claim
* **Layers:** `raw()`, `tls({cert, key})`, `ssh({hostKey, authorize})`, and a line helper for text protocols.
* **Keys:** certificates and host keys are read from a sealed zen-db row or file, put there by the admin with a helper.
* **Sealed inbox:** with `inbox` set on a service, the owning node encapsulates once per connection to the X-Wing key, then seals input chunks with a streaming AEAD (counter nonces). Plaintext input (SMTP without STARTTLS) is then never stored in plaintext; the server still sees it in transit, as documented. The handler opens it; a `seal_until` switch stops sealing after STARTTLS.

## Steps (one PR each)

**5.5.0, plan:** above. `docs/MILESTONE-5.5.md`, DESIGN-3 §6, README.

**5.5.1, spec (⚠️ `spec:` commits):**
* New `spec/svc.md`: services and modes, the connection lifecycle, the step, reveal, takeover, engine state, limits, failure table, and what the server sees.
* `spec/api.md` §14: `/v1/svc/next`, `/v1/svc/lease`, `/v1/svc/read`, `/v1/svc/reveal`, `/v1/svc/connect`, the `svc` commit step, the stream frames, the error codes `conn_closed` and `window`.
* `spec/keyspace.md` §3.9: `rc`, `ri`, `ro`, `rs`, `rq`, `rl`, `rn` (node doorbell and queue), `rk` (node lease).
* `spec/formats.md` §4: seal kinds 9 (state) and 10 (the output key envelope); §13: the inbox stream.
* `labels.md`.
* Vectors in `spec/test-vectors/svc.json` (kinds 9 and 10, the inbox stream), with zen-core `svc` seal functions and their vector tests.

**5.5.2, server relay core, input side:**
* `config.rs` `[[service]]` with validation and an example; `src/svc/{mod,listen,conn,lease,node}.rs`; `keys.rs`.
* `lib.rs`: listeners, the node lease and the reaper, kept in `Server` so `abort` stops them.
* Endpoints `/v1/svc/next`, `/v1/svc/lease`, `/v1/svc/read`.
* Tests in `crates/zen-server/tests/svc.rs` (both backends):
  * claim and read exact bytes
  * backpressure: the peer's write blocks once the window is full
  * EOF and RST markers
  * no handler: the connection closes after `accept_wait_ms`
  * per-IP limits and `max_conns`
  * reaping after an epoch change
  * rights: no `consume`, no claim

**5.5.3, the step, reveal, the writer:**
* zen-proto `Commit.svc` and `SvcStep`; `commit.rs` validate (op and byte costs) and execute.
* `svc/step.rs`; `/v1/svc/reveal`; the per-node doorbell writer; half-close and close; `peer_gone`; the `/v1/info` feature `svc`; quota accounting.
* Tests:
  * an echo handler
  * a stale token gives `claim_lost`; a stale `in_from` gives `cursor_moved`
  * a reveal of an uncommitted record, and a reveal with a wrong key, are refused
  * takeover mid-stream with an exact transcript (no loss, no duplicate)
  * `resume: false`
  * `tests/multinode.rs` (FDB): the socket on node A, the handler on node B
  * 5,000 idle connections without a watch each

**5.5.4, stream push:**
* `Frame` `svc_sub`, `svc_in`, `svc_reveal`; `stream.rs`; per-lease doorbells.
* Tests: push latency; a reconnect resumes from `in_acked`.

**5.5.5, `@zen/svc`, raw:**
* `packages/svc` (wired like `packages/loro`); the wire types in `crates/zen-wasm/src/wire.rs`.
* `kv.ts` `empty()` learns `svc`.
* `serve()`: the handler lease, owner gating, the transactional step, reveal after commit, re-reveal on claim, `resume: false`.
* Vitest:
  * a stateful line protocol with a Node `net` client
  * the handler worker SIGKILLed mid-stream, with an exact transcript
  * two handlers share connections
  * a handler's zen-db write and its reply commit together (a kill between them never shows one without the other)
* Playwright: a browser tab as the handler.

**5.5.6, shared TLS provider crate:** move `tls/rustcrypto.rs` and `rsakey.rs` into `crates/zen-tls-rustcrypto`; zen-server depends on it. `tests/tls.rs` and `tests/mtls.rs` stay green unchanged.

**5.5.7, the TLS engine (`crates/zen-svc`, native):**
* The handshake through rustls unbuffered, with KeyLog capture; the 1.3 and 1.2 record layers; KeyUpdate; alerts; serialization.
* Native tests:
  * tokio-rustls clients on 1.3 and 1.2, and `openssl s_client`
  * a serialize-and-restore at **every** record boundary, the transcript unchanged
  * a KeyUpdate forced by the client, and one by the server
  * key and IV cross-checked against `into_kernel_connection`
  * fuzz-style malformed records

**5.5.8, `@zen/svc-wasm` and the `tls()` layer:**
* `scripts/build-wasm.sh` builds the second module; the JS time provider.
* Vitest: a Node `tls.connect` echo over 1.3 and 1.2 with handler kills mid-stream (resumed) and mid-handshake (closed cleanly); a 20 MB transfer across two kills, checksum verified.

**5.5.9, the SSH engine (native):**
* kex (curve25519, mlkem768x25519), ed25519 host key, both ciphers, strict kex, EXT_INFO, publickey, channels, rekey; persistence.
* Native tests against OpenSSH `ssh` (CI installs `openssh-client`): exec, shell, a subsystem, `RekeyLimit=1M`, a restore at every packet boundary, and Terrapin-style injection refused.

**5.5.10, the `ssh()` layer:**
* Vitest: `ssh -p … exec` streaming 50 MB across two handler kills, checksum verified; a rekey across a takeover; a takeover mid-kex.
* A git-transport demo: `ssh … git-upload-pack` answered by a stub (real git comes with M10).

**5.5.11, `tcp-auth` and `zen-connect`:**
* The WebSocket `/v1/svc/connect`; `packages/svc/bin/zen-connect` (`--listen` port forward, or stdio for `ProxyCommand`).
* Tests: `ssh -o ProxyCommand="zen-connect …"`; a token without `append` refused; an unauthenticated connect refused.

**5.5.12, the sealed inbox:**
* Server encapsulation and the streaming AEAD; the handler's open and `seal_until`.
* Tests: a store dump during a connection shows no plaintext under `r*`; STARTTLS switches sealing off.

**5.5.13, `apps/mail`:**
* Inbound SMTP (STARTTLS over TLS 1.2 and 1.3, the sealed inbox for plaintext) and an IMAP subset (LOGIN through app passwords kept in zen-db, SELECT, FETCH, STORE, IDLE over TLS).
* A message is stored, its event emitted and `250` sent in **one commit**.
* Vitest with python `smtplib`/`imaplib` (`python3 -I`), killing handlers between DATA and the reply: every message stored exactly once. One Playwright view of the inbox.

**5.5.14, docs, bench, outcome:**
* `docs/SVC.md` (usage, modes, keys, takeover, what the server sees); `docs/API.md`, `docs/CLIENT.md`, `GAPS.md`, `spec/OPS.md`.
* STATS §5.5: step latency, throughput, the takeover pause, the cost of an idle connection.
* TECH_DEBT: TD-SVC-ACME, TD-SVC-UDP, TD-SVC-TLS-HANDSHAKE-RESUME.
* The Outcome.

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy (default and `--no-default-features`); `cargo test -p zen-server --test svc` and the touched crates; FDB once before the final commit (CI's `fdb` job is the gate).
* **JS:** `npm run check`; `npx vitest run` three times; `npx playwright test` when the browser is touched.
* **Real clients:** Node `net`/`tls`, `openssl s_client`, OpenSSH `ssh`, python `smtplib`/`imaplib`; CI gains `openssh-client`.
* CI green on the PR before stopping.

## Risks
* **TLS engine correctness** (our own record layer after the handshake): every record boundary is checked against rustls in tests; KeyUpdate is cross-checked; fuzz tests.
* **SSH engine scope:** it's the largest piece, so it is kept to the subset OpenSSH needs.
* **Latency:** about 6–10 ms per output step with `resume: true`. Interactive SSH may want `resume: false` per service.
* **Storage churn:** every byte is a durable write and delete. Bounded by `max_conns × window` and the quota.
