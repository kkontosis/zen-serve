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

* **Largest file version at the defaults is about 175 MiB.** A version's manifest plus its chunk list must fit one value (`max_value_bytes` = 90,000; 32 B per chunk), so at most ~2,800 chunks of 64 KiB. Raising `max_value_bytes` raises it, up to FoundationDB's 100 KB value limit (`TD-FS-LARGE-FILES`).
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
| `zen_wasm_bg.wasm` (release, LTO, no wasm-opt) | **1.86 MB**, 525 KB gzipped |
| WASM JS glue and declarations | 265 KB and 92 KB |
| `@zen/client` npm package (compiled TS) | 78 KB packed, 330 KB unpacked |

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
| Client tests: 60 Node tests in 8 files (`npx vitest run`, a server per file, FUSE mounts included) | ~6 s |
| Browser smoke test (`npx playwright test`) | ~3 s |
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
