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
| `zen_wasm_bg.wasm` (release, LTO, no wasm-opt) | **2.14 MB**, 589 KB gzipped (1.86 MB at milestone 4; the growth is zen-db's keyed primitives, CRDT rows and paged files) |
| WASM JS glue and declarations | 311 KB and 109 KB |
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

Reading it:
* **Indexes.** A unique or fast index adds one write. A private index rewrites its path of nodes (here two levels) and the root, about 4 KB per insert; decoys double that. A sealed index rewrites its whole blob, padded to a power-of-two number of parts: 90 KB per insert even at 1,000 rows, so it suits small or rarely written tables.
* **Queries.** Index reads cost one to three round trips; a scan reads every row, so an unindexed filter on 1,000 rows is about 15 times a range page.
* **CRDT writes** are queued: they resolve in microseconds, and the queue sends them after `crdtBatchMs` (10 ms by default), which is most of the 14 ms "waiting" figure. Their commits carry the change event too.
* **The broker.** The emit-to-handler latency is a commit plus the stream's watch wake-up. `per_key` with 8 handlers in flight doubles sequential throughput, from one client.
* These are single-client numbers against a local server: they show relative costs and the round trips, not the throughput of a server under load.
