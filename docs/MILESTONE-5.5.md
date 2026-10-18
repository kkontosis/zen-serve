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
* **Step 5.5.9** (the SSH engine, native): planned. Restarted from main after PR #45 merged; the plan is below.
  * Decided with the user: implement `mlkem768x25519-sha256` too. OpenSSH has it only from 9.9, and Ubuntu 24.04 (here and on the runners) ships 9.6. So a CI job builds OpenSSH 10.6p1 from source to test the hybrid against it. Once that job has passed on the PR, it is switched to run on demand only.
  * Next: on "go", the `spec:` commit, then the code.

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
