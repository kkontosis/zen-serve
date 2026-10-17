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

**After compaction, re-read:** this file (log first); `spec/api.md` §6, §8, §9; `spec/keyspace.md` §3.2–3.3; `crates/zen-server/src/{consume,commit,stream,eph,config,lib}.rs`; `crates/zen-proto/src/lib.rs` (`Commit` ~1389, `Consume` ~1355, `Frame` ~2915); `packages/client/src/{kv,log,stream}.ts`; `packages/db/src/broker.ts` (`ConsumerHandle`).

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
* **Identity:** `name`, `fs`, `id` (16 bytes, hex). Rights are checked as **topic rights on `id`**, so the existing prefix grants apply: a handler needs `consume`; a `tcp-auth` connector needs `append`.
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
4. **The step,** in `/v1/commit` as a new `svc` list of `SvcStep {fs, svc, conn, token, in_from, in_to, out_from, out: [record], state?, close?}`. It is atomic with everything else in the commit (zen-db rows, events, consume steps). It checks and does:
   * the claim token is current (else `claim_lost`, 412)
   * `in_from == in_acked` and `out_from == out_head` (else `cursor_moved`)
   * `in_to ≤ in_head` (a snapshot read: `in_head` only grows)
   * clears input below `in_to`
   * appends the output records at `out_head`
   * writes the sealed protocol state (`rs`, seal kind 9)
   * applies close flags
   * rings the owning node's doorbell
5. **Reveal (the two-phase release):** each output record is `AEAD(k_r, raw) ‖ BLAKE3(k_r) ‖ seal_fs(k_r)` (seal kind 10), with `k_r` fresh and random **per commit attempt** (never derived from the seq). Only after the commit is known to have succeeded (`commit_unknown` is replayed with the same `commit_id` first) does the handler send `reveal {conn, seq, k_r}`. The server checks `BLAKE3(k_r)` and the AEAD tag, so **only committed bytes can ever reach the wire**. Reveals need no token, only `consume`, because revealing a committed record is always safe. A successor re-reveals by opening `seal_fs(k_r)`.
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
