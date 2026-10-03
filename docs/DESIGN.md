# zen-serve — design proposal (draft, pre-code)

A remotely served, end-to-end encrypted block store ("remote LUKS"), with a
companion E2EE database mode, on top of a replicated, sharded backend.

The server **never** sees keys or plaintext. It stores and replicates opaque,
fixed-size ciphertext blocks and offers one atomic primitive: a conditional
multi-block commit. Everything else (encryption, filesystem, SQL) runs in the
client: a native binary, or the browser through WASM.

---

## 1. Threat model

| Party | Trusted for | Not trusted for |
|---|---|---|
| Client device, while unlocked | everything | — |
| Server operator / storage nodes / backups | availability, eventually | confidentiality, integrity, freshness |
| Network | nothing (TLS is defence in depth only) | — |

Goals:

* **Confidentiality** of contents, file names, schema, column names and keys.
* **Integrity**: tampered blocks are detected.
* **Freshness**: the server can't quietly serve an *older valid* block (rollback or replay). LUKS/dm-crypt does not protect against this.
* **Post-quantum** resistance for everything asymmetric.

Non-goals (stated plainly):

* Hiding **volume size, access patterns and timing** costs extra (ORAM, §2.7). It's optional, not in the MVP.
* With several writers and a malicious server, freshness is limited to **fork consistency** (§2.6). That's a hard theoretical limit, not something the implementation can fix.
* A member you revoke keeps whatever they already decrypted.

---

## 2. Cryptography

### 2.1 Why not copy LUKS literally

LUKS2/dm-crypt uses **AES-XTS**: same length in and out, deterministic per sector, and **no authentication**. On a local disk that's acceptable. Against an untrusted remote server it fails in three ways:

1. The server can flip bits. With XTS, a change garbles 16 bytes and nobody notices.
2. Writing the same plaintext to the same sector gives the same ciphertext, which leaks equality over time.
3. The server can replay an old sector, and nothing detects it.

So we keep the **LUKS key-management model** (header, keyslots, a master key wrapped many times) and replace the **data cipher** with an AEAD plus a freshness tree.

### 2.2 Primitives

| Purpose | Choice | Notes |
|---|---|---|
| Block AEAD | **XChaCha20-Poly1305** | 192-bit random nonce, so nonce reuse can't happen in practice even after many writes. AES-GCM's 96-bit random nonce reaches its 2³² bound after about 16 TiB of 4 KiB writes per key, which a long-lived block device can reach. Optional native alternative: AES-256-GCM-SIV. |
| Hash / Merkle | BLAKE3 (or SHA-256 / SHA3-256 for FIPS) | |
| KDF (subkeys) | HKDF-SHA-256 (or BLAKE3 derive_key) | |
| Password KDF | **Argon2id** | Native: m=1 GiB, t=4. Browser: m=256 MiB, t=3. Parameters are stored per keyslot. |
| Key encapsulation (sharing, devices) | **X-Wing** (ML-KEM-768 + X25519 hybrid, IETF draft) | PQ-safe and still safe if ML-KEM turns out to be broken |
| Signatures (commits, header) | **Ed25519 + ML-DSA-65** hybrid (FIPS 204) | Both must verify. ML-DSA signatures are about 3.3 KB, so we sign commits, not blocks. Optional SLH-DSA (hash-based, very conservative) for the long-lived volume admin key. |
| Hardware unlock | **FIDO2 / WebAuthn PRF** (`hmac-secret`) | Works in browsers today. It's a keyslot type, like `systemd-cryptenroll --fido2`. |

On post-quantum: symmetric crypto with 256-bit keys is already quantum-safe (Grover's algorithm leaves about 128 bits). The "harvest now, decrypt later" risk sits in the **asymmetric keyslots**, which is why the KEM is hybrid PQ from day one. Hybrid signatures guard against future forgery. That's less urgent, but costs little to do now.

Implementation: **libcrux** (formally verified ML-KEM/ML-DSA, Cryspen) or RustCrypto `ml-kem`/`ml-dsa`, `chacha20poly1305`, `x25519-dalek`, `ed25519-dalek`, `argon2`, `blake3`. One Rust crypto core is compiled to native and to `wasm32`, so the browser and the CLI run byte-identical code. WebCrypto doesn't offer ML-KEM or XChaCha everywhere yet.

### 2.3 Key hierarchy (the "LUKS header")

```
passphrase ──Argon2id──┐
FIDO2 PRF ─────────────┤
recovery key (256b) ───┼──> KEK_slot ──AEAD-unwrap──> MK_e   (master key, epoch e)
X-Wing decaps(device) ─┘                                │
                                                        ├─HKDF "data"   -> K_data_e
                                                        ├─HKDF "tree"   -> K_tree_e
                                                        └─HKDF "names"  -> K_names_e
```

* **Volume header** (stored in the server's metadata space, replicated like any other value):
  * format version, volume UUID, block size
  * key-epoch table
  * **keyslots[]**
  * admin signature
  * The header is versioned and updated with CAS, like every other record.
* **Keyslot** = `{type, kdf params | KEM ciphertext, AEAD(KEK, MK_e, aad = vol_uuid‖slot_id‖epoch)}`. Changing a passphrase rewrites one slot. Adding a device or user is one X-Wing encapsulation to their public key, and they don't need to be online. Slot count can be padded to a fixed number so it doesn't reveal how many members there are.
* **Epochs, for revocation:**
  1. Generate `MK_{e+1}`.
  2. Wrap it only to the remaining members.
  3. Store `AEAD(MK_{e+1}, MK_e)` (a backward key chain), so current members can still read old blocks.
  4. New writes use epoch `e+1`, and a background job re-encrypts old blocks lazily.

  Every block records its epoch, so a mixed-epoch volume is normal.

### 2.4 Block format

The plaintext block size is fixed (default **4096 B**; configurable to 16/64 KiB for throughput).

```
stored block (4096 + 45 B, ≈1.1% overhead):
  u8   format
  u32  key_epoch
  24B  nonce (random per write)
  4096 ciphertext
  16B  Poly1305 tag
AAD = vol_uuid ‖ block_index (u64) ‖ write_counter (u64) ‖ key_epoch
```

* Every write uses a fresh nonce, so rewriting identical data gives an unrelated ciphertext. There's deliberately **no convergent encryption or dedup**, because both leak equality.
* All blocks are the same size, so length leaks nothing. An unwritten block is simply absent, so the server learns the *allocated* size. Preallocate if that matters.
* Swapping blocks between positions fails authentication, because the AAD binds the block index and volume.

### 2.5 Freshness: authenticated version tree

A writable dm-verity, in effect:

* Leaf `i` = `H(i ‖ write_counter_i ‖ tag_i)`.
* Internal nodes are 4 KiB and hold 128 hashes (fan-out 128). A 1 TiB volume (2²⁸ blocks) has depth 4.
* Tree nodes are stored as ordinary encrypted blocks in a reserved index range. The upper levels stay cached in the client.
* Each **commit** carries a **signed root**: `{vol, root, commit_seq, prev_root_hash, writer_id}`, signed with a hybrid signature.
* Cost per commit: one leaf-to-root path per touched leaf group. That's amortised when a commit is batched, and tree space is about 0.8% of the volume.

### 2.6 Multiple writers vs. a malicious server

* **Single writer:** the client remembers the last root it signed (locally, or in the key header). Any rollback is detected.
* **Multiple writers:** every writer verifies the hash-chained commit log (`prev_root_hash`). A malicious server can still **fork** clients, showing A and B different histories. It can't merge those histories back without being caught. To detect forks early, clients exchange their latest `(commit_seq, root)` out of band, or publish them to an independent **witness**, similar to Certificate Transparency gossip. The witness is optional, and the design doesn't depend on it.

### 2.7 What still leaks, and how to reduce it

* Leaks to the server:
  * number of blocks
  * which indices are read or written, and when
  * commit sizes
  * who connects (account, IP)
* Optional mitigations:
  * read and write in fixed-size batches, with dummy blocks for padding
  * shuffle the index mapping (`block_id = PRF(K_names, index)`), which hides locality but not repetition
  * full **Path ORAM**, which hides access patterns at roughly `O(log N)` bandwidth cost (about 20–30× at 1 TiB). It's a later, opt-in volume type.

---

## 3. Server protocol (HTTPS, HTTP/2 or HTTP/3)

The server has no keys and is stateless. Authentication covers *who may touch which volume* (OIDC/token, or a device signature), never the contents.

```
GET  /v1/vol/{vid}/header                     -> header blob + version
PUT  /v1/vol/{vid}/header   If-Match: <ver>    (CAS)

POST /v1/vol/{vid}/read
     { at?: <snapshot|commit_version>, idx: [u64...] }
  -> [{idx, version, blob}]                    (batched; absent = never written)

POST /v1/vol/{vid}/commit
     { expect: [{idx, version}],               // read-set / CAS preconditions
       writes: [{idx, blob}],
       signed_root }                           // opaque to server, stored in commit log
  -> 200 {commit_version} | 409 {conflicts:[idx]}

POST /v1/vol/{vid}/snapshot {label}            -> snapshot id (= commit_version)
GET  /v1/vol/{vid}/log?since=<commit_version>  (commit log, for verification/sync)
POST /v1/vol/{vid}/lease                       (exclusive-writer lease + fencing token)
```

**`commit` is the one important primitive**: an *atomic, conditional, multi-block write*. A block device, a file layer and a database can all be built on it. NBD `FLUSH`/FUA maps to "wait until the commit is acknowledged", and an acknowledgement means "durable on a quorum".

---

## 4. Storage, sharding and replication

### Option 1: blocks as values in a real distributed KV (**recommended first**)

Key layout (MVCC at the application level, which also gives backups for free):

```
(vid, "b", idx, ~commit_version) -> block blob      // newest version sorts first
(vid, "c", commit_version)       -> commit record (signed_root, writer, idx list)
(vid, "h")                       -> header
```

Candidate backends:

* **FoundationDB** (preferred)
  * strict-serializable multi-key transactions, which is exactly what `commit` needs
  * values up to 100 KB, so a 4 KiB block fits easily
  * automatic sharding, rebalancing and replication, including across regions
  * the most thoroughly tested distributed database around: deterministic simulation, used in production by Apple and Snowflake
  * C API, plus the Rust `foundationdb` crate
* **TiKV**
  * written in Rust, multi-Raft, transactional (Percolator)
  * a reasonable alternative, with more moving parts (PD)
* **Embedded single node** (`redb`/`fjall`/RocksDB) for development and small self-hosted setups, behind the same `Storage` trait.

Why not S3 / an object store for blocks? Millions of 4 KiB objects cost too much and are too slow, and you'd still need consensus for the CAS. An object store fits **backups**, not the hot path.

### Option 2: our own replication (raft/etcd)

It's possible: multi-Raft with `openraft` per range shard, each shard on RocksDB/fjall, plus shard split and merge, a placement driver, ReadIndex/lease reads, snapshots and log compaction, and fencing. It's also, realistically, **years of work to trust**. The minimum bar is deterministic simulation testing (`madsim`/`turmoil`) and Jepsen-style fault injection.

etcd itself is only good for *metadata* (it tops out at a few GB). It isn't a block store.

**Recommendation:**

1. Write the server against a narrow `Storage` trait: `read_at`, `commit(cas, writes)`, `scan_log`, `snapshot`.
2. Ship FoundationDB as the HA backend and an embedded backend for single-node use.
3. Leave room for a native Raft backend later, if a single-binary HA deployment ever matters enough to justify the cost.

The API servers are stateless and scale horizontally behind any load balancer.

---

## 5. Backups and point-in-time recovery

The MVCC key layout from §4 makes these follow directly:

* **Snapshot**: record a `commit_version`, and the GC never deletes versions a live snapshot still needs. It's instant and copy-on-write.
* **Point-in-time recovery**: read at any `commit_version` inside the retention window. The commit log maps wall-clock time to versions.
* **Clone or fork a volume**: a new volume whose base is `(vid, snapshot)`. Reads fall through to the base.
* **GC**: delete versions older than the retention window that no snapshot needs.
* **Off-site backup**: stream the KV ranges (ciphertext only) to any S3-compatible bucket, tape or provider. **The backup operator needs no keys and no trust.** The client verifies a restore against the signed Merkle root. You could also use FoundationDB's built-in continuous backup, which is cluster-wide and coarser.
* **Crash-consistent snapshots**: the client requests the snapshot after a flush or `fsfreeze`, so the filesystem inside is consistent.
* **Ransomware protection**: retention plus server-enforced append-only commits mean a compromised client can't destroy history, as long as the retention policy is admin-only.

---

## 6. Clients

1. **Native block device** (Linux): a `ublk` (or NBD) userspace daemon.
   * Running `mkfs.ext4 /dev/ublkb0` gives exactly "remote LUKS".
   * **One writer at a time**: a server-side lease plus a fencing token in every commit. Ordinary filesystems corrupt under two writers.
   * Read-only mounts of snapshots are unlimited.
2. **Browser / WASM**: block-level is awkward in a browser, which has no kernel filesystem. Browsers use the **database mode** (§7) or a simple encrypted object/file tree built on the same blocks.
3. **CLI**: init, keyslot add/remove/rotate, snapshot, export/verify, backup.

---

## 7. The multi-user problem and an E2EE database

### 7.1 Why SQLite or Postgres on the shared filesystem fails

A filesystem built on a block device isn't multi-writer. Even on a real network filesystem, SQLite's file locking holds a lock for a whole transaction, so one slow or crashed client can block everyone else. Postgres expects to own its directory exclusively. So the database has to be **built on the commit primitive, not on files**.

### 7.2 Does a "fully E2EE database" exist?

Not in the strong sense you want, at least not as a server-side query engine. Here is what exists:

| Approach | Examples | Problem |
|---|---|---|
| Property-revealing encryption (deterministic, order-preserving, searchable) | CryptDB, SQL Server Always Encrypted, MongoDB Queryable Encryption, CipherStash | The server runs queries, so it learns equality, order and frequency. Known inference attacks recover data (Naveed et al. 2015, and many later papers). Schema and column names are usually plaintext. |
| TEE / enclave | SQL Server with enclaves, EdgelessDB (MariaDB in SGX), confidential VMs | You trust Intel/AMD, and side channels exist. Not E2EE. |
| Oblivious databases (ORAM) | ObliDB, Oblix, Snoopy, Waffle (research) | The strongest model, but research-grade and expensive. |
| **Client-side DB over encrypted storage or sync** | SQLCipher (local only), **mvSQLite** (SQLite VFS on FoundationDB, page-level MVCC, *not* encrypted), Evolu, cr-sqlite, Automerge + Keyhive/Beelay (Ink & Switch), Anytype any-sync, Actual Budget, EteSync/Etebase | **The realistic answer.** The server only sees ciphertext pages or changesets, so schema, column names and keys stay encrypted. |

### 7.3 Proposal: "zen-db", SQLite pages as encrypted blocks with optimistic concurrency

* A real, unmodified **SQLite** runs in the client (native, or WASM in the browser) with a **custom VFS**. Each database page is one encrypted block in a dedicated volume, so it reuses §2–§5 entirely.
* **Reads** run against a snapshot (`at=commit_version`), so readers never block anyone.
* **Commit**:
  1. The VFS has recorded the **read-set** (page, version) and the **write-set**.
  2. `POST /commit` with `expect = read-set` runs atomically on the server.
  3. On `409`, the client refreshes the changed pages and replays the transaction.

  This gives page-level serializable isolation. It's the mvSQLite model with client-side encryption added.
* **No long-held locks.** A crashed or slow client can only abort its *own* transaction. That directly fixes "one client locks up the place".
* **Encrypted from the server**: table names, column names, indexes, keys, values, and `sqlite_master`. **Visible to the server**: page count, which pages a transaction touches, and when.
* **Known limit: write contention on hot pages.**
  * The hot pages are page 1 (the header holds the database size and change counter), freelist trunks and the rightmost leaf when keys are sequential.
  * Mitigations:
    * VFS-level handling of the page-1 header fields (as mvSQLite does)
    * preallocation
    * random primary keys (UUIDv4/ULID-random) to spread inserts
    * **one database per tenant or user** where it makes sense
  * Positioning: many readers, a modest number of concurrent writers, full SQL and ACID. That fits a small team or a family. It doesn't fit thousands of writes per second on one table.

### 7.4 Alternative mode: encrypted changeset log (local-first / CRDT)

Each client keeps a full local SQLite. The server stores only an **append-only log of encrypted changesets** keyed `(db, seq)`, written with the same commit/CAS API. Merging is conflict-free (cr-sqlite or Automerge-style CRDTs).

* Pros: offline-first, any number of writers, minimal leakage (only blob sizes and timing).
* Cons: every client holds a full replica (or a partial one), and the semantics are eventually consistent CRDT merges rather than serializable SQL.

Recommendation: build **7.3 first**, because it reuses the block layer with zero new server concepts. Add **7.4** for collaborative, offline-heavy apps. Both share keys, the server and backups.

### 7.5 Group key management

Per-volume and per-database master keys are wrapped per member with X-Wing (§2.3), and epochs handle revocation. If membership changes often and the groups are large, swap in **MLS (RFC 9420)** with a PQ ciphersuite to derive epoch keys. That's not needed for the MVP.

---

## 8. Language and packaging

**Rust**, because:

* one crypto, format and protocol crate (`zen-core`) builds for native and `wasm32`
* memory safety when parsing data from an untrusted server
* `axum`/`hyper` for the server, `foundationdb` crate, `openraft` if ever needed, `libublk` for the block device, `sqlite-wasm-rs`/`rusqlite` for zen-db

C++ is viable (liboqs, libsodium, the FoundationDB C API), but WASM builds and safe parsing are harder.

```
crates/
  zen-core      # crypto, block/header/keyslot formats, Merkle tree, test vectors
  zen-proto     # HTTP API types
  zen-server    # stateless API server + Storage trait
  zen-store-fdb # FoundationDB backend
  zen-store-local # embedded single-node backend
  zen-cli       # volume admin, backup, verify
  zen-ublk      # Linux block device client
  zen-wasm      # browser bindings
  zen-db        # SQLite VFS (OCC over commit API)
docker/         # multi-stage build; compose: 3x FDB + 2x zen-server + LB
```

## 9. Milestones

1. `zen-core`: formats spec, AEAD blocks, keyslots (Argon2id, recovery key, X-Wing), Merkle tree, test vectors.
2. `zen-server` + local backend + CLI: read, commit with CAS, header, snapshot.
3. FoundationDB backend, MVCC retention/GC, PITR, ciphertext backup export.
4. `zen-ublk`: mount ext4 over it, with a single-writer lease.
5. `zen-wasm` + WebAuthn PRF keyslot.
6. `zen-db` (SQLite VFS, OCC).
7. Multi-user: epochs, revocation and re-encryption, hybrid-signed commit log, optional witness.
8. Optional: changeset/CRDT mode, ORAM volume type.

## 10. Open questions

1. Is the first deliverable the **block device** (native) or **zen-db** (browser and multi-user)?
2. Is FoundationDB acceptable as a dependency, or is a single self-contained binary a hard requirement?
3. Target scale: one family or team, or many tenants?
4. Do we need the FIPS-only algorithm set (AES-GCM-SIV/SHA-2), or is XChaCha/BLAKE3 fine?
