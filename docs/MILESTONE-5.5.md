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

**Constraints:** no model names in the repo; commit trailers as before; `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push; about 4.4 GB of disk free.

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
* **Step 5.5.3** (the step, reveal, the writer): planned below ("Step 5.5.3 in detail"). Restart from main after PR #38 merged; commit `docs: milestone 5.5 step 3 plan`, open a PR, **stop for the user to compact**; then on "go" the code.

## Step 5.5.3 in detail: the step, reveal, the writer

This step adds the output side, on the server only. Normative behaviour is spec/svc.md §3 (steps 6–10), §4 and §5, and api.md §6 and §14.5–14.7. Formats are formats.md §13.1–13.2, using the zen-core `svc` functions from 5.5.1. With this step a Rust test can act as a complete handler: claim, read, step, reveal, and see the bytes arrive at the peer.

**Commits:**
1. `docs: milestone 5.5 step 3 plan`
2. `spec: svc step and writer details`
3. `server: svc commit step, reveal, writer, timers`
4. the docs and the log

**Out of scope:**
* stream frames `svc_sub`/`svc_reveal`/`svc_in` (5.5.4)
* the TS client, `kv.ts` `empty()` and the wasm wire list (5.5.5)
* `tcp-auth` (5.5.11)
* the inbox and `unseal`'s effect (5.5.12; the flag is stored now)

### Spec clarifications (`spec:` commit; svc.md, api.md §14.5/§14.7)
* **Half-close then end.** When a FIN record has been written, the peer's EOF is stored and all input is acked, the node closes the socket and reaps.
* **Timers.**
  * "No committed step" (`handshake_secs`) means `ra` is still all zeros and there is no state.
  * "Bytes wait" (`idle_secs`) means unacked input (`in_acked < in_head`) or unwritten output (`written_to < out_head`).
  * Both timers abort with a reset.
* **`resumable` on a `resume: false` service is 400** (it could never be honoured; svc.md §5 says such a connection is aborted when its claim lapses after a step).
* **Reveal results.**
  * A plain record returns 1: there is nothing to reveal.
  * At most 1,000 entries per request (413 over).

### zen-proto
* **`SvcStep`:** `{fs, svc, conn, token, in_from, in_to, out_from, out: [bytes], state?, resumable?, unseal?}`.
* **`Commit.svc: Vec<SvcStep>`:** `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. Every Rust `Commit` literal already ends in `..Default::default()`, so nothing else breaks.
* **Reveal types:** `SvcRevealRequest {fs, svc, reveals: [SvcReveal {conn, off, key}]}` and `SvcRevealResult {results: [u8]}`.

### zen-server
* **`commit.rs` `validate`:**
  * For each step:
    * the fs is configured and the service exists (404)
    * topic `consume` on `svc`
    * `conn` is 16 bytes
    * `in_from ≤ in_to`
    * at most one step per `(fs, svc, conn)`
    * `state` ≤ `max_value_bytes`
  * For each record:
    * it parses (`zen_core::svc::parse_record`)
    * flags are only 1|2|4|8, with at most one of FIN, close and abort
    * no record follows a FIN, close or abort record
    * an empty record carries FIN, close or abort
    * its raw length is ≤ `max_value_bytes − 130`
  * Each step and each record counts as one operation. Records and state count toward `max_commit_bytes`.
* **`commit.rs` `execute`:** after the consume steps and before the appends, `svc::step::apply(&mut t, st, caller, step, i as u16, &mut delta)` runs for each step. The quota delta merges into the existing per-fs `delta`, so the existing quota check and `atomic_add` cover it.
* **New `svc/step.rs` `apply`** (svc.md §4, checks in order):
  1. `check_claim` (`rm` read normally, `rz` → `conn_closed`).
  2. Read `rc` and `ra` normally:
     * sealed records with `rc.resume` false, or plain ones with it true → 400
     * `resumable` with `resume` false → 400
  3. `in_from == in_acked` and `out_from == out_head`, else `cursor_moved`.
  4. `in_to ≤ in_head` by a snapshot read of `rh`, else `cursor_moved`.
  5. `ra` FIN with records → `conn_closed`; `ra` close or abort → `conn_closed`.
  6. `out_head + raw − written_to` (snapshot of `rw`) ≤ `rc.out_window`, else 429 `window`.
  7. The state rule: if `state_len > 0`, the step moves a cursor, and it carries no `state` → 400.

  Then the effects:
  * **Input.** A snapshot reverse read of one `ri` key below `in_to`. A chunk that spans `in_to` stays, and `in_cleared` becomes its start; otherwise `in_cleared = in_to`. Then `clear_range(ri(0), ri(in_cleared))`.
  * **Records.** `ro(off)` for each, with `off` advancing by the raw length.
  * **Cursors.** The new `ra`; flags `|=` FIN/close/abort from the records, 8 for `resumable`, 16 for `unseal`.
  * **State.** `rs` is set when `state` is present.
  * **Quota.** `+raw − (new in_cleared − old) + (new state_len − old)`.
  * **Wake-up.** The node's `rnq(node, vs_incomplete(i))` and its `rn` doorbell (with user version `i`, so steps in one commit don't collide).
* **New `svc/reveal.rs`, `POST /v1/svc/reveal`.** Needs topic `consume`, no token. Each entry gets its own transaction and result code:
  * `rc` missing → 2
  * `off ≥ ra.out_head` → 2
  * `off < written_to` (snapshot) or `rv(off)` present → 1
  * no `ro(off)` → 2
  * a plain record → 1
  * `zen_core::svc::reveal`:
    * `Error::Param` (the key hash doesn't match) → 3
    * `Error::Decrypt` → 3; if the caller is the claim holder, also an abort request with reason 1 (`reap::abort_request`)
  * otherwise `rv(off) = k_r`, `rnq` and `rn` → 0. After the commit, a local `Cmd::Wake` when the connection lives on this node (the fast path).
* **`svc/conn.rs`, the writer.**
  * The socket is split (`into_split`). The writer is a pinned future in the same task, polled as a `select!` branch, so the connection stays one task and `Svc::abort` still stops everything.
  * The writer waits on a `Notify`, rung by `Cmd::Wake` (and once at start):
    * It reads `rw`, `ra` and the `ro`/`rv` entries in `[written_to, out_head)` by snapshot.
    * It decrypts revealed records in order, up to 256 KiB per batch, and stops at the first record that isn't revealed.
    * It runs `write_all`, then one transaction:
      * `rm` read normally (a reap conflicts; `rm` missing → gone)
      * clear `ro`/`rv` in `[old, new)`
      * `rw = new`
      * quota `−(new − old)`
      * ring the holder's `rld` while its lease exists
  * Record flags at the record:
    * **FIN:** `shutdown()` of the write half; the writer ends with `Fin`.
    * **Close:** read and discard input for up to 1 s, then close and reap.
    * **Abort:** reset (`set_zero_linger` through `OwnedWriteHalf::as_ref()`) and reap.
  * A failed write → reap.
  * After FIN, the main loop closes and reaps once EOF is stored and `in_acked == in_head`; it re-checks on `Wake`.
* **`svc/conn.rs`, the timers.**
  * `handshake_secs` after accept: if `ra` is still empty (snapshot), abort.
  * `idle_secs` (0: off): local timestamps of the last input read and the last output written; when they're older than `idle_secs`, a snapshot of `rh`/`ra`/`rw` decides. Waiting bytes → not idle; otherwise abort.
* **`svc/mod.rs`:** `RA_UNSEAL = 16`, and helpers shared by `step`, `reveal` and the writer.
* **`lib.rs`:**
  * the route `/v1/svc/reveal`
  * the `/v1/info` feature `"svc"`
  * api.md §14's "a server without the feature refuses them" holds: the server has the feature from now on
* **`error.rs`:** nothing new (`window` and `conn_closed` exist).

### Tests: `crates/zen-server/tests/svc.rs`, extended (both backends unless noted)
A test handler helper: `SvcKeys::from_parts` with a random key, `seal_record`/`seal_state`, a step builder, a commit, and a reveal.
1. **Echo.** The peer writes `hello\n`. The handler reads, then commits a step: ack 6, one sealed record, a state. It reveals → 0, and the peer reads `hello\n`.
   * The same `commit_id` replayed returns the same result, and `out_head` doesn't move.
   * Quota: input cleared and output written bring the counter back to its baseline.
2. **Refusals.**
   * a stale token → `claim_lost`
   * a stale `in_from` or `out_from`, or `in_to > in_head` → `cursor_moved`
   * a plain record on a resume service → 400
   * two steps for one connection → 400
   * a missing state after a stateful step → 400
   * a record over `out_window` → 429 `window`
   * records after a FIN → `conn_closed`
3. **Reveal codes.**
   * off beyond `out_head` → 2
   * a wrong key → 3, and the connection lives
   * a repeat → 1
   * a record whose `key_hash` matches but whose body is tampered → 3, and the holder's reveal aborts the connection (the peer sees a reset)
4. **Half-close and close.**
   * A FIN record: the peer reads EOF, can still write, and the handler reads that input. Once the peer shuts down too and the input is acked, the connection is reaped.
   * A close record: the peer gets its bytes, then EOF.
   * An abort record: the peer gets a reset.
5. **Takeover with an exact transcript.**
   * Handler A echoes lines with `resumable: true` and a counter in the state. It commits its last step without revealing, and its lease (`ttl_ms` 300) lapses.
   * Handler B claims with token 2 and opens the state at the given cursors. It finds the pending escrow, opens it with `open_escrow` and re-reveals, then continues.
   * The peer receives every echo exactly once, in order.
6. **`resume: false`.** Plain records are written without a reveal. When the claim lapses after a step, the connection is aborted, not requeued.
7. **Timers.**
   * `handshake_secs` 1: no step → reset.
   * `idle_secs` 1: after a step, silence → reset.
   * With input left unacked, the connection is still open after 2.5 s.
8. **Many connections.** 1,000 open, unclaimed connections on one node, and an echo on a new connection still completes within 2 s. This bounds per-connection cost; no per-connection watches exist by construction.
9. **`/v1/info`** lists `"svc"`.
10. **`tests/multinode.rs` (FDB):** the socket on node A, the handler on node B (`h.peer()`); the echo arrives through A.

### Docs
* api.md §14.5/§14.7 and svc.md: the clarifications (the `spec:` commit).
* The log in both plan copies.

### Verification
* fmt; clippy (default, `--no-default-features`, `--all-features`)
* `cargo test -p zen-server --test svc` 3× on embedded and once on FDB (the local cluster in `/tmp/zen-ci`), plus `--test multinode` on FDB
* the full `cargo test -p zen-server` once, and the FDB suite as CI runs it
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
* **Crypto crates** already in the workspace: aes-gcm, chacha20poly1305, x25519-dalek, ed25519-dalek, sha2, hmac, blake3, ml-kem, x-wing. The RustCrypto rustls provider is `crates/zen-server/src/tls/rustcrypto.rs` (it uses `rsakey.rs`). There is no SSH crate.
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
