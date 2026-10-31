# zen-serve: numbers

Storage cost, retention, binary sizes and timings. Byte layouts come from `spec/formats.md` and `spec/keyspace.md`; default limits from `crates/zen-server/src/config.rs`. Figures marked **measured** were taken on the development machine (4 cores, 15 GB RAM, ext4); everything else is computed from the formats or is an estimate, and says so.

See also [`spec/OPS.md`](../spec/OPS.md) for pitfalls and how to avoid the slow paths.

## 1. Bytes in, bytes stored

### 1.1 Logical overhead (what zen-serve writes)

Every sealed object costs **48 bytes** over its plaintext (formats.md §4: 8-byte header, 24-byte nonce, 16-byte tag). Keys are packed tuples; ids and tokens are 16 bytes per element.

| What | Stored per item, beyond the user's bytes |
|---|---|
| **File content, per 64 KiB chunk** (clients chunk at 64 KiB) | ~180 B: the seal (48), the chunk key, its refcount and GC pointer (~100), and 32 B in the version's chunk list and sealed manifest. **≈ 0.27 %** |
| **Each file or directory** (node, meta, indexes, one version record) | ~650 B fixed: node record (~125 B + sealed meta ~70 B + name), children and change-feed index entries, a version record and a sealed manifest. A move also logs ~90 B for 7 days. |
| **KV value** | 48 (seal) + 10 (versionstamp) + key (16 B per path element + ~8 B prefix) ≈ **+90 B** per entry for a 2-element key |
| **Event** (log append) | 48 (seal) + log key (~40) + fs-wide index (~40) + per-key index (~60, keyed events only) ≈ **+150–190 B** per event |
| **Chained event** (integrity attached, body version 2: formats.md §5.1) | **+52 B** per event over a version-1 body: the chain id (12), `dseq` (8) and `dprev` (32) |
| **Device checkpoint** (formats.md §14) | ~4.4 KB sealed with a few entries (the hybrid signature is 3,375 B of it, §5.6), plus ~70–100 B per object entry and ~70 B per `seen` device, + 10 B versionstamp and ~60 B key. The server keeps the newest `checkpoint_keep` (1,024) per device: **at most ~4.5 MB per device**, counted in the fs quota (api.md §15.1) |
| **Block volume** (spec/blk.md), per stored 4 KiB block | 48 (seal) + 10 (versionstamp) + key (~72 B: four path elements) ≈ 130 B, **~3.2 %**; plus one 4 KiB level-1 node per 128 written blocks, **~0.8 %**; the upper levels and the root record are negligible. **A fully written volume costs ~4 % over its size**; an empty one, only its root record. Discarded blocks and empty nodes aren't stored |
| **Per sign-in credential** | a few hundred bytes (password key: public identity ~2 KB; passkey: COSE key ~100 B) |

**Byte-for-byte conversion for files** (logical bytes zen-serve writes ÷ bytes in the file):

| File size | Stored | Ratio |
|---|---|---|
| empty | ~650 B | — |
| 1 KiB | ~1.85 KiB | **~1.8×** |
| 10 KiB | ~10.8 KiB | ~1.08× |
| 100 KiB | ~101 KiB | ~1.01× |
| 1 MiB and up | +0.27 % + 650 B | **~1.003×** |

Small files are dominated by the ~650 B per-file metadata; large files cost almost nothing extra. The last chunk of a file is stored at its real size: zen-serve doesn't pad (a client may, to hide sizes: spec/fs.md §10).

### 1.2 On disk (what the storage engine adds): estimates

| Backend | Multiplier on the logical bytes |
|---|---|
| Embedded (`redb`, B-tree, 4 KiB pages) | **~1.1–1.5×**. Freed pages are reused, but **the file never shrinks**: zen-serve doesn't compact it. |
| FoundationDB (`ssd` engine) | **~1.2–2× per replica**, times the replication factor: `single` ×1, `double` ×2, `triple` ×3 across nodes (operations.md §3.2). Plus the transaction logs (bounded, transient). Deleted space is reused; files shrink only slowly. |

**Rule of thumb for large files:** ~1.0× logical, so about 1.1–1.5× on an embedded node and 1.2–2× per replica on FoundationDB. A triple-replicated 1 TB of large files needs roughly 4–6 TB of raw cluster disk.

### 1.3 Limits worth knowing

* **Largest file version at the defaults is about 640 GiB** with paged versions (spec/fs.md §4; the server since milestone 5 step 7f, and clients page any file past the inline limit since step 7g). A page holds up to 5,624 chunk ids (about 351 MiB at 64 KiB), and the sealed manifest lists up to about 1,870 pages at 48 B each. An inline version (the chunk list in the version itself) still tops out at about 175 MiB: 32 B per chunk within `max_value_bytes` = 90,000.
* **Page costs.** A page is one value of `4 + 16n` bytes, plus one reference-count add per chunk when first uploaded; a version that appends to a file reuses every full page of the one it replaces.
* A commit carries at most `max_commit_bytes` = 8 MB, so a large file is uploaded over several commits (spec/fs.md §4.1).

## 2. Do updates explode the disk?

No, as long as clients use the CRDT filesystem as designed. Each case:

| Operation | Space while it happens | When the old space is freed |
|---|---|---|
| **Rewrite a file** (a `write` that replaces the current version) | The new chunks are added. If the client **reuses the ids of unchanged chunks**, only changed chunks are uploaded; otherwise the whole file is new. | Old chunks lose their last reference at once, and are **deleted after the chunk grace period (24 h)**. So up to ~2× for the rewritten part, for one day. |
| **Concurrent writes** (siblings) | Every sibling version is kept. | When a later write replaces them (a client resolving the conflict). Unresolved siblings stay forever. |
| **Delete a file** (move to TRASH) | Nothing new. | Purged after the **horizon (7 days)** plus `crdt_max_skew_ms`; its chunks then follow after another **24 h**: about **8 days**. |
| **Rename or move** | ~90 B in the move log | Move log entries after **7 days** |
| **KV overwrite** | none: the value is replaced in place | at once (the engine reuses the space) |
| **KV delete / clear_range** | none | at once |
| **Event append** | +150–190 B + the envelope | **Never: the log has no retention yet** (`TD-LOG-RETENTION`) |
| **Uploaded but unreferenced chunk** | its size | after 24 h, if no version references it |

So rewrites cost up to 2× of the changed bytes for 24 h, and deletes are reclaimed after about 8 days. The things that **only grow** are the **event log**, **unresolved siblings**, and **file versions kept by clients that never replace them**.

## 3. Retention: what zen-serve frees, and when

All of it is done by the **sweeper**, every `sweep_interval_secs` (60 s), on every node.

| Data | Kept for | Setting |
|---|---|---|
| Unreferenced chunks | 24 h after the last release or upload | `chunk_grace_secs` = 86,400 |
| Trash (deleted files and directories) | 7 days after the move to trash (plus skew margin) | `crdt_horizon_secs` = 604,800 |
| Move log | 7 days | `crdt_horizon_secs` |
| Change-feed tombstones of purged nodes | 7 days | `crdt_horizon_secs` |
| Idempotency records (commit replay) | 24 h | `idempotency_ttl_secs` = 86,400 |
| Sessions | 24 h | `session_ttl_secs` = 86,400 |
| Ephemeral messages | 60 s | `ephemeral_ttl_secs` = 60 |
| Replaced and deleted KV values | none: gone at once | — |
| **Event log** | **forever** | none (`TD-LOG-RETENTION`) |
| **Continuous backups** (snapshots and mutation logs) | **forever**, until you expire them | `fdbbackup expire` by hand |
| FoundationDB trace logs | about 100 MB per `fdbserver` process (FoundationDB's default rolling) | — |
| Embedded file size | **never shrinks** | (`TD-STORE-COMPACTION`) |

## 4. Binary and install sizes (measured)

`cargo build --release -p zen-server`, with the release profile (LTO, one codegen unit, stripped):

| Build | Size |
|---|---|
| default (ring TLS provider) | **8.1 MB** |
| pure Rust (`--no-default-features`) | 7.7 MB |
| pure Rust, no C at all (`--no-default-features --features pure`) | 7.7 MB |
| FoundationDB (`--features fdb`, ring) | 8.5 MB |
| FoundationDB, pure Rust (`--no-default-features --features fdb`) | 8.1 MB |

Without the release profile the default build was 14.3 MB. The `fdb` builds also need FoundationDB's `libfdb_c.so` (24 MB) at run time.

FoundationDB 7.3.79 installed by `scripts/install-fdb.sh`:

| Install | Size |
|---|---|
| everything, `--dedupe=none` | 277 MB |
| everything, `--dedupe=auto` (default) | **171 MB** |
| `--no-backup --no-dr --no-fdbmonitor` | 144 MB |

Of that, `fdbserver` is 99 MB, `libfdb_c.so` 24 MB and `fdbcli` 27 MB; the backup/DR tool is 28 MB per copy.

### 4.1 Client (measured)

| Artifact | Size |
|---|---|
| `zen_wasm_bg.wasm` (release, LTO, no wasm-opt) | **2.42 MB**, 645 KB gzipped at milestone 5.5 step 8 (2.14 MB at milestone 5, 1.86 MB at milestone 4; the growth is zen-db's keyed primitives, CRDT rows and paged files, then the svc seals and request codecs) |
| WASM JS glue and declarations | 311 KB and 109 KB |
| `zen_svc_wasm_bg.wasm` (`@zen/svc-wasm`, the TLS and SSH engines: rustls, the RustCrypto provider and zen-svc's SSH; release, no wasm-opt), loaded only by services that use `tls()` or `ssh()` | **1.23 MB**, 465 KB gzipped at milestone 5.5 step 10 (1.05 MB and 403 KB with the TLS engine alone, step 8); glue 44 KB, declarations 15 KB |
| `@zen/client` npm package (compiled TS) | 112 KB packed, 475 KB unpacked |
| `@zen/db` | 171 KB packed, 783 KB unpacked |
| `@zen/fs` | 46 KB packed, 217 KB unpacked |
| `@zen/loro` | 11 KB packed, 45 KB unpacked; `loro-crdt`'s own WASM is 3.27 MB, 1.08 MB gzipped |
| An app bundle (`apps/dist/kanban/main.js`: client, db, fs and Loro glue, esbuild, not minified) | 959 KB, 163 KB gzipped, plus the two WASM files |

## 5. Timings

### 5.1 Development (measured on 4 cores)

| Operation | Time |
|---|---|
| `cargo check -p zen-server` after touching one file | 44 s |
| Release build after touching one file (LTO) | 117 s |
| `cargo test -p zen-server` (embedded) after touching one file | 90 s (rebuild + run) |
| `cargo test -p zen-server` (embedded), nothing changed | 26 s (just the run) |
| Release build of all 5 variants, dependencies cached | ~10 min |
| CI per push (cached): `rust`, `pure-rust`, `fdb` jobs | 2–4.5 min each, in parallel |
| FoundationDB suite, after building | ~1–2 min (each test file 3–12 s; `dump.rs` ~10 s because of a deliberate cluster recovery) |
| WASM module release build (`scripts/build-wasm.sh`), dependencies cached | ~60 s |
| TypeScript tests: 417 tests in 38 files (`npx vitest run`: client, db, fs, loro, fuse and the apps, a server per file; FUSE mounts skip without `/dev/fuse`) | ~70 s |
| Browser tests (`npx playwright test`: the smoke test and 12 app specs, after `npm run build:apps`) | ~25 s |
| CI `client` job (wasm build, `npm ci`, typechecks, vitest with FUSE, the app build, Playwright) | ~5.5 min |
| FoundationDB cluster start (`zen-serve init`) to Healthy | ~10–20 s |
| `scripts/install-fdb.sh` | ~20–40 s (download and unpack) |
| Clean build of the workspace with tests, from nothing | about 10–15 min (estimate) |

### 5.2 The WASM core (measured, Node 22, one thread)

| Operation | Time |
|---|---|
| Instantiating the module | 7 ms |
| Argon2id 64 MiB, t=1 (the floor; tests) | 103 ms; in Chromium, a password sign-in end to end took 108–369 ms |
| Argon2id 256 MiB, t=3 (browser recommendation) | ~1.0 s |
| Argon2id 1 GiB, t=4 (native recommendation) | ~8.2 s: WASM runs it on one thread, so native clients should use native code for it |
| Hybrid signature (Ed25519 + ML-DSA-65): sign / verify / key from seed | 2.6 / 0.7 / 0.9 ms |
| Device keyslot (X-Wing encapsulation) | 0.75 ms |
| Sealing a 64 KiB chunk (XChaCha20-Poly1305) | 0.47 ms, ~140 MB/s |
| Tree state (fs.md §9.1) of 100,000 nodes, built from scratch (a replica's first check after opening) | ~0.5–0.6 s to set the leaves, then 60–130 ms for the first `root()` |
| Tree state, one node changed then `root()` | ~55–95 µs; 1,000 changes then one `root()`: ~10 ms + 7 ms |

### 5.3 Runtime operations that are slow by design

| Operation | Cost |
|---|---|
| Argon2id for a password key or OPAQUE (client side) | 64 MiB floor: ~0.1–0.3 s native; recommended 1 GiB/t=4: several seconds; in a browser (256 MiB/t=3) ~1–3 s |
| ML-DSA-65 signature (hybrid sign-in, ACL) | milliseconds; signatures are 3.4 KB, identities 2 KB |
| RSA test key generation | ~0.1 s per key (tests cache them) |
| FoundationDB version clock on an idle cluster | advances in steps of up to ~2 s: leases and sweeper ages measured in versions can lag that much |
| FoundationDB recovery (after `advance_version`, a dead process, a full disk) | seconds; transactions in flight get `commit_unknown` or `too_old` and are retried |
| Trash purge and chunk GC | minutes to days by design: horizon 7 days, grace 24 h, sweeper every 60 s |

### 5.4 zen-db costs (measured)

`ZEN_BENCH=1 npx vitest run packages/db/test/bench.test.ts packages/loro/test/bench.test.ts` (opt-in, not in CI), against a local zen-serve on the embedded backend, 4 cores, one client process. Times are medians of 20 runs (5 to 10 for the slower ones), from the call to its result, so they include the round trips to the local server. Commit ops and bytes are what the step sent in `/v1/commit` (ops: KV writes, range clears, appends, consume steps, CRDT ops). The index lines insert one row into a table already holding 1,000 rows; the query lines read a table of 1,000 rows.

| What | ms | Commit ops | Commit bytes |
|---|---|---|---|
| Row insert (small) | 4.0 | 1 | 601 |
| Row update | 2.8 | 1 | 477 |
| Row get, cached (one version check in the commit) | 2.3 | – | – |
| Row get, not cached | 2.5 | – | – |
| Padded row insert | 4.4 | 1 | 840 |
| Row insert of 200 KB (parts) | 8.5 | 4 | 201,188 |
| Row get of 200 KB, not cached | 6.5 | – | – |
| Insert, no index | 3.5 | 1 | 601 |
| Insert, unique index | 4.7 | 2 | 982 |
| Insert, fast index | 3.9 | 2 | 786 |
| Insert, private index (fanout 64, 1 shard) | 5.2 | 6 | 4,609 |
| Insert, private index (4 shards) | 5.2 | 6 | 3,907 |
| Insert, private index (2 decoys) | 5.8 | 10 | 11,327 |
| Insert, sealed index | 5.3 | 3 | 90,112 |
| Get by pk | 1.7 | – | – |
| Unique equality | 1.9 | – | – |
| Fast equality | 3.0 | – | – |
| Private range, a page of 20 | 3.9 | – | – |
| Sealed range, a page of 20 | 4.1 | – | – |
| Scan of 1,000 rows with a filter (no index) | 54 | – | – |
| `emit` in a commit of its own | 2.2 | 1 | 217 |
| `emit` → `on` handler, latency | 25 | – | – |
| `consume`, sequential | 4.9 per message (about 200/s) | – | – |
| `consume`, per_key (8 in flight) | 2.5 per message (about 400/s) | – | – |
| `request` → reply (as an event) | 6.0 | 3 | 631 |
| A schedule delivered after its `at` | 10 | – | – |
| CRDT insert, waiting for its commit | 14.6 | 6 | 2,184 |
| CRDT patch, queued (resolves once stored) | 0.1 | – | – |
| CRDT patch / incr / add / remove, waiting for the commit | 14–15 | 3–4 | 1,159–1,422 |
| CRDT get: cache hit / expired (a version check) / not cached | 1.2 / 1.8 / 2.0 | – | – |
| CRDT indexer: a patch until an index query finds it | 25 | – | – |
| Loro: a local edit until another installation imports it | 4.2 | – | – |
| Loro: open from a snapshot plus a 100-event tail | 11.7 | – | – |
| Basic / authenticated: row get, not cached (1,000 rows) | 2.3 / 2.6 | – | – |
| Basic / authenticated: row get, cached | 1.1 / 1.1 | – | – |
| Basic / authenticated: row insert | 2.9 / 6.1 | 1 / 6 | 601 / 8,953 |
| Basic / authenticated: a transaction updating 20 rows | 31 / 35 | 20 / 46 | 8,108 / 81,397 |
| Basic / authenticated: a scan of 100 rows | 10.1 / 7.9 | – | – |

Reading it:
* **Indexes.** A unique or fast index adds one write. A private index rewrites its path of nodes (here two levels) and the root, about 4 KB per insert; decoys double that. A sealed index rewrites its whole blob, padded to a power-of-two number of parts: 90 KB per insert even at 1,000 rows, so it suits small or rarely written tables.
* **Queries.** Index reads cost one to three round trips; a scan reads every row, so an unindexed filter on 1,000 rows is about 15 times a range page.
* **CRDT writes** are queued: they resolve in microseconds, and the queue sends them after `crdtBatchMs` (10 ms by default), which is most of the 14 ms "waiting" figure. Their commits carry the change event too.
* **The broker.** The emit-to-handler latency is a commit plus the stream's watch wake-up. `per_key` with 8 handlers in flight doubles sequential throughput, from one client.
* **Authenticated databases** (zendb.md §14). The basic and authenticated lines were measured side by side, on two fs, with one table each. Reads cost about the same: the NsRoot read takes the place of a cached row's version check, and the state tree's upper nodes stay cached, so a lookup adds no round trip once warm (the scan difference is noise). Writes carry the state-tree leaves they touch and the NsRoot: a one-row insert into 1,000 rows grows from 601 bytes to about 9 KB (fanout 64, height 2), and a transaction of 20 scattered rows touches about 20 leaves. The real cost is contention: every writer conflicts on the NsRoot (docs/DB.md §7.1).
* These are single-client numbers against a local server: they show relative costs and the round trips, not the throughput of a server under load.

### 5.5 Relayed service costs (measured)

`ZEN_BENCH=1 npx vitest run apps/echo/test/bench.test.ts apps/mail/test/bench.test.ts` (opt-in, not in CI), on the echo and mail apps' worlds, at milestone 5.5 step 14:
* a local zen-serve on the embedded backend, **release build** (`ZEN_SERVE_BIN`), with release WASM modules
* 4 cores, with the peers, the handler and the server on one machine
* handlers in the test's process, except for the takeover, which uses Node workers

| Step latency (a line and its reply; each step also inserts a row) | median ms | p90 ms |
|---|---|---|
| raw, `resume: true` | 17.6 | 21.5 |
| raw, `resume: false` | 15.6 | 18.8 |
| TLS 1.3 | 18.1 | 21.9 |
| SSH (`lines` on one exec channel) | 19.6 | 23.0 |
| `tcp-auth` (`svcConnect`) | 19.0 | 23.2 |
| IMAP `NOOP` (TLS 1.3) | 18.0 | 23.1 |

| Throughput | MB/s |
|---|---|
| raw mirror, 20 MB each way | 5.6 |
| TLS mirror, 20 MB each way | 2.5 |
| SSH `cat`, 20 MB each way | 2.7 |
| IMAP `FETCH BODY.PEEK[]` of a 9.6 MB message (output only) | 11.3 |

| Takeover (raw echo, a line every 5 ms, the holder SIGKILLed, a successor already running, lease TTL 1 s) | ms |
|---|---|
| the longest wait for a reply, median of 5 rounds | 1,036 (973–1,168) |

| 1,000 idle raw connections held by one handler | |
|---|---|
| server RSS | +124 MiB, about 127 KiB per connection |
| handler heap (Node) | about 7 KiB per connection |
| handler calls during 10 s idle | 0 |

Reading it:
* **A step is a few round trips.** A line goes from the socket into storage (a commit), is pushed to the handler, runs in a step (the handler's transaction, a commit), and with `resume: true` its output is released by a reveal (an HTTP request) before the server writes it. `resume: false` skips the reveal and saves about 2 ms. The layers add their engines' work: TLS about half a millisecond, SSH two. `tcp-auth` adds the WebSocket hop.
* **Throughput** is one step in flight per connection, each taking what input arrived since the last one (at most `window`). TLS and SSH halve it: their engines run in WASM, and each step restores the engine from its sealed state. A large IMAP FETCH writes 256 KiB a step without waiting for input, which is why it is faster than the mirrors.
* **The takeover pause** is the lease's TTL plus a claim: the successor claims within a few milliseconds of the lease lapsing, and the peer sees a pause, not an error. A shorter TTL takes over faster and renews more often (every second by default).
* **An idle connection** costs the server a task, its socket and buffers (the input reader keeps a 32 KiB buffer), and its records in storage; the handler keeps its keys and cursors. It costs no steps: nothing runs until input or a wake-up comes.
* **Debug builds** are slower: the debug server with development WASM measured 22.6 ms for a raw step, and 2.7, 0.8 and 1.1 MB/s for the three mirrors.
* As with §5.4, these are single-client figures on one machine: they show what a step costs, not a server under load.

### 5.6 Device checkpoints in the native client (measured)

`cargo test --release -p zen-client --test integrity -- --ignored --nocapture bench` (4 cores, one thread). The body has 10 volume entries and 4 `seen` heads: 977 bytes, 4,404 sealed (the hybrid signature is 3,375 of them).

| Operation | Release | Debug build |
|---|---|---|
| Sign (Ed25519 + ML-DSA-65) | 0.19–0.23 ms | 2.8 ms |
| Seal (kind 11, XChaCha20-Poly1305) | 0.017 ms | 0.88 ms |
| Open | 0.016 ms | 0.87 ms |
| Verify | 0.14 ms | 1.6 ms |

Reading it:
* **Native signing is about 13× faster than WASM** (2.6 ms, §5.2), so a native writer can afford `sig_every: 1` far more easily: a checkpoint costs a fifth of a millisecond of CPU, and its put one round trip.
* **A sync** costs a verify per device whose head moved, plus one per `seen` link that isn't pinned yet: with ten devices, under 2 ms of CPU; the round trips (heads, then a get per moved device) dominate.
* **Debug builds** compile zen-core without optimization (it is a workspace member; only dependencies get `opt-level = 3`), and the generic cipher code is instantiated in it, which is why sealing is 50× slower there.

### 5.7 The block engine (measured)

`cargo test --release -p zen-blk --test volume bench -- --ignored --nocapture` (4 cores; the server in the same process, embedded backend, loopback). A 256 MiB volume (65,536 blocks, depth 3), `batch_blocks` 256.

| Operation | Release |
|---|---|
| Sequential write of 64 MiB, a flush every 4 MiB | 1.31 s, 49 MiB/s |
| Random 4 KiB read, a fresh reader | 0.14 ms |
| Random 4 KiB read, the writer (its nodes cached) | 0.14 ms |
| A flush of one block (what ext4's `fsync` waits for) | 1.6 ms |

Reading it:
* **A read is one round trip** either way: the nodes a block's path lacks are fetched in the same `get` as the block, so a cold reader costs about what a warm one does. Only the bytes differ (up to `depth` nodes of 4 KiB more, until the level-1 node is cached).
* **A flush is one commit per 256 blocks:** the blocks, about one level-1 node per touched region, the nodes above, and the root record. Sequential writes touch one level-1 node per 128 blocks, so a 1 MiB commit carries about 1.04 MiB of blocks and nodes.
* **Every commit replaces the top node and the root record,** so commits of one volume are serial; one writer per volume is the model anyway.

## 6. Integrity: what it costs

Integrity (docs/INTEGRITY.md) costs signatures, round trips and bytes. Where each is measured:

| What | Where |
|---|---|
| Signing and verifying a checkpoint: WASM 2.6 ms and 0.7 ms; native 0.2 ms and 0.14 ms | §5.2, §5.6 |
| Keeping a replica's tree state: 100,000 nodes from scratch ~0.6 s, then ~55–95 µs per change | §5.2 |
| An authenticated zen-db namespace vs a basic one: reads the same; an insert 1 → 6 ops and 0.6 → 9 KB | §5.4 |
| The block engine in process: 49 MiB/s written, a 4 KiB read 0.14 ms, a one-block flush 1.6 ms | §5.7 |
| Stored bytes: a checkpoint ~4.4 KB (at most ~4.5 MB per device); +52 B per chained event; ~4 % per volume | §1.1 |
| A TS checkpoint end to end, syncs, chained appends and reads | §6.1, §6.2 below |
| ext4 over FUSE + loop and over ublk | §6.3 below |

### 6.1 Checkpoints in the TS client (measured)

`ZEN_BENCH=1 ZEN_SERVE_BIN=target/release/zen-serve npx vitest run packages/client/test/integritybench.test.ts` (opt-in, not in CI): a release zen-serve (embedded backend) and release WASM, on the 4-core development machine, in one Node process; medians of 20 runs, two runs of the bench. Ten writer devices of one user (one in the ACL, nine enrolled) and a verify-only reader.

| Operation | ms (median) | Sealed bytes |
|---|---|---|
| `checkpoint()` carrying 10 entries (sign, seal, put) | 6.4–7.2 | 4,477 |
| `checkpoint()` carrying 100 entries | 8.9–13.3 | 12,680 |
| `sync()`, 1 of 10 devices moved | 46–51 | |
| `sync()`, 10 of 10 devices moved | 68–83 | |
| `exportPins()`, 10 devices | < 0.01 | 809 |

### 6.2 Event chains (measured)

The same bench: 50 appends of a short payload, and five reads of 1,000 events, on one fs without integrity attached and one with it.

| Operation | Without integrity (v1 bodies) | With integrity (v2 bodies, chained) |
|---|---|---|
| `append`, ms (median) | 2.5–3.0 | 2.5–2.7 |
| `append`, request bytes | 197 | 249 |
| An unfiltered read of 1,000 events, ms | 13 | 25–34 |

### 6.3 ext4 on the devices (measured)

`cargo test --release -p zen-ublk --features device-tests[,ublk] --test devices bench -- --ignored --nocapture --test-threads 1`, as root: the server in the test's process (embedded backend), `zen-ublk` as a child process with its defaults (`--batch-blocks` 256, a 5 s idle flush), an empty 1 GiB volume (depth 3), ext4 with 4 KiB blocks. The untar is the repository's `crates`, `spec` and `docs` at this step (241 files, 4.3 MiB). The FUSE figures are from the development machine (4 cores, two runs). The ublk figures are from CI's `devices-bench` job, on a GitHub-hosted runner (UBLKRUNNER), a different machine: compare the two columns of one machine, not across them.

<!-- ext4-stats -->

### 6.4 Reading it

* **Round trips, not crypto,** are where the time goes. A TS checkpoint is 2.6 ms of signing and one put; 100 entries add about 8 KB and a few milliseconds of encoding. Natively, signing is 0.2 ms (§5.6).
* **A sync costs about two round trips per known device, whether it moved or not.** The client re-reads the ACL head and re-fetches each pinned checkpoint to check it's still the one it pinned (formats.md §14.4 rule 2), then fetches a moved device's head and verifies it (0.7 ms in WASM). So ten devices cost about 50 ms on loopback whatever moved, and a moved device adds a get and a verify. That's more than it needs to be: TD-INTEGRITY-SYNC-COST. Prefer `watch()` (a sync per pushed head) to polling `sync()` in a loop.
* **An event chain** adds 52 bytes to each append and no round trip: the chain's position is kept in memory. Appends measured the same with and without it, within noise. Checking chains on a read (a hash per event, opening the v2 fields) doubles the client's time for a 1,000-event read, which is still under 35 ms.
* **Choosing `sigEvery`:**
  * The default (64 changes, or 5 s after the last one) costs at most one checkpoint per 64 writes: well under 1 % of the time of the writes themselves.
  * `sigEvery: 1` costs a checkpoint per change: about 7 ms more per change in a browser, and 4.5 KB stored. A handler step (§5.5) takes 16–20 ms, so a checkpoint per step adds about a third.
* **ext4 over a volume** is bounded by commits. A write that fills a batch flushes before it returns, so sequential writes wait for one commit per MiB. A synchronous 4 KiB write (`O_DSYNC`, as a database's `fsync` does) costs ext4 a data write and a journal commit with its cache flushes, which is several engine commits: about 12 ms over FUSE and loop. Reads that miss the page cache are one request per block the kernel asks for, with readahead making them large.
