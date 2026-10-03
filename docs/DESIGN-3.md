# zen-serve: design part 3 (final primitives)

Part 3 of the design, following `DESIGN.md` and `DESIGN-2.md`. Where they disagree, this document wins.

## 0. Decisions in this round

* **CRDT:** Loro is the default.
* **Web trust:** browser clients are only as trustworthy as the server at page-load time. Accepted and documented.
* **Client:** library mode (`@zen/client`). No WebContainers. The service worker is an optional cache only.
* **Event signatures:** a hybrid-signed checkpoint every `sig_every` events, configurable per topic. `sig_every = 1` signs every event.
* **Filesystems:** a configured set of filesystems (defaults: `/` and `/home/user`), mounted together on the client. Permissions apply per filesystem only. All of them live in the same FoundationDB keyspace, distinguished by an integer `fs_id` key prefix (§4.1).
* **Static files:** served plaintext from `/unencrypted` (§4).
* **zen-db runs on the client.** The SQLite-on-pages idea is **dropped**. zen-db becomes a client-side encryption layer over an encrypted key-value API (§2).
* **Single-leader consumption:** leases plus fencing plus atomic consume-commit, all inside FoundationDB transactions (§3). No separate Raft.

---

## 1. Final primitive set

Everything is one FoundationDB keyspace. zen-serve stays keyless and stateless.

| Primitive | What it is |
|---|---|
| **KV** | Ordered key-value store. Encrypted keys and values, namespaced by `fs_id`, with transactions |
| **Log** | Topics: exactly-once append, cursors, live push (DESIGN-2 §2) |
| **Consume** | Consumer cursors and leader leases with fencing tokens (§3) |
| **Ephemeral** | Fan-out pub/sub that is never persisted (presence, typing); optional |
| **Static** | `/unencrypted`, plaintext |
| **Admin** | Volume headers and keyslots, signed ACL, backup |

**One write endpoint does it all.** A commit can atomically:
* validate reads
* write KV
* append events
* advance a consumer cursor
* check a lease

```
POST /v1/commit
{ read_version?,                       // short txn: native FDB conflict detection
  expect:  [{fs, key, version}],       // long txn: per-key version CAS
  expect_ranges: [{fs, begin, end, hash}],
  writes:  [{fs, key, value | null}],
  append:  [{topic, device_id, device_seq, envelope}],
  consume: {group, topic, partition, from: v_prev, to: v_event},
  lease:   {group, topic, partition, token} }
→ 200 {commit_version} | 409 {conflict: [...]} | 412 {not_leader | cursor_moved}
```

Everything is now built **from KV**:

| Feature | How it's built |
|---|---|
| Native block device | key `(fs, "blk", block_idx)`, value = encrypted block (DESIGN.md §2.4) |
| Filesystem | inodes, directory entries and file chunks as KV rows, with POSIX-ish semantics through transactions. Simpler than the tree-CRDT filesystem in DESIGN-2 §3.2, which becomes optional. |
| zen-db | tables, rows and indexes as KV (§2) |
| CRDT documents | snapshots in KV, updates in the Log |

---

## 2. Encrypted KV, and zen-db on top of it

### 2.1 Does FoundationDB do transactions?

Yes. That's its defining feature: **ACID, strictly serializable, multi-key, across all shards**, with optimistic concurrency. It has been tested with deterministic simulation for over a decade. Limits that shape our API:

| Limit | Value | What we do |
|---|---|---|
| Transaction lifetime | about **5 s** (the MVCC window) | short client transactions use FoundationDB's native conflict tracking; longer ones use per-key version CAS (§2.4) |
| Transaction size | 10 MB | the library splits large imports |
| Value size | 100 KB | the library chunks large values |
| Key size | 10 KB | not an issue: encrypted keys are short |

Underneath, FoundationDB's coordinators run Paxos. A zen-serve node cut off from the quorum **can't commit anything**, so split brain is impossible at the data layer. That's why we never need Raft among clients: **every decision is a FoundationDB transaction.**

### 2.2 Key encryption: keyed hashing, element by element

A logical key is a tuple, for example `(namespace, table, primary_key, …)`. On the wire:

```
stored_key = fs_id ‖ PRF_k0(ns) ‖ PRF_k1(table) ‖ PRF_k2(pk) ‖ …     (16 bytes each)
k0 = HKDF(K_fs, "kv"),  k1 = HKDF(k0, ns),  k2 = HKDF(k1, table), …
```

Here a PRF is a keyed hash: BLAKE3 (keyed) in the modern suite, HMAC-SHA-384 in FIPS. This is the same hierarchical scheme as the topic ids.

* **What works:**
  * point lookups
  * prefix scans: "all rows of a table", "all keys of a namespace"
  * transactions
* **What doesn't:** range scans by *value order*. Keyed hashes destroy order. Order-preserving encryption is rejected: it leaks too much.
* **The PRF is one-way**, so the plaintext key is also stored *inside* the encrypted value. That lets scans return real keys.
* **The server sees:**
  * the `fs_id`
  * the shape of the tree (how many namespaces, tables and rows)
  * which keys are hot
  
  It never sees namespace, table or column names, keys, or values.

### 2.3 Values

```
value = header{suite, key_epoch, nonce, last_writer?} ‖ AEAD(K_fs_e, row_bytes, aad = stored_key)
```

* A row is stored as **one value** holding all its columns (MessagePack or CBOR), so the column count and column names are invisible.
* Optional padding to size buckets (256 B / 1 KiB / 4 KiB / …) hides row sizes.
* The AAD binds each value to its key, so the server can't swap values between keys.
* zen-serve stamps each value with its **commit version** using FoundationDB's versionstamped-value write. That version is what clients `expect` in §2.4.

### 2.4 Two transaction modes

1. **Short (< ~4 s), fully serializable.**
   * The client gets a read version, then reads keys and ranges at it.
   * zen-serve commits with the same read version and with the client's reads registered as **read conflict ranges**.
   * FoundationDB detects every conflict, including phantoms (rows inserted into a range you scanned).
   * This is the default. Retry on conflict.
2. **Long, for slow processing or slow networks.** The commit carries `expect: [{key, version}]`, and zen-serve re-reads those keys inside the commit transaction and compares versions.
   * Scanned ranges are validated by `expect_ranges` with a hash of the (key, version) list. That catches phantoms at the cost of re-reading the range.

### 2.5 zen-db: a client-side typed layer

* **Tables:** `(ns, table, pk) → row`.
* **Secondary indexes**, two kinds, chosen per index:
  * **Private (default):** an encrypted B+tree whose nodes are KV values with random ids. It supports equality *and range or order* queries and hides how often values repeat. Upper levels are cached on the client, so a lookup costs about 1–2 round trips.
  * **Fast:** `(ns, idx, PRF(value), PRF(pk)) → ∅`. One round trip for equality, but **the server learns how many rows share each indexed value**. Opt-in, and documented as such.
* **Integrity levels:**
  * **Basic (default):** the AEAD detects tampering and swapped values. It does **not** stop rollback of a single value to an old valid version.
  * **Authenticated:** the namespace keeps a Merkle B+tree, a prolly or Merkle-search tree whose root is hybrid-signed at each commit. Rollback is detected at the cost of extra node reads. Use it for namespaces where freshness matters, such as balances and permissions.
* **SQL** is out of scope. A small query builder (filter, index, order, limit) runs on the client.

---

## 3. Single-leader event processing

### 3.1 The pattern

Kafka's consumer groups combine three things: partition ownership, committed offsets, and transactional read-process-write. We map them one-to-one.

```
(c, group, topic, partition, "cursor") -> last processed versionstamp
(c, group, topic, partition, "lease")  -> {holder_device, token, expires_at_version}
```

* **Lease, for liveness.** A device that holds the `consume` permission acquires the lease in a transaction if it's empty or expired, and `token += 1`. It renews with a heartbeat.
  * Expiry is measured in **FoundationDB commit versions**, which advance at about 1,000,000 per second. That's a monotonic, cluster-wide clock that doesn't depend on NTP or client clocks. Example: TTL 10 s, heartbeat every 3 s.
* **Fencing, for safety.** Every processing commit carries its `token`. In the **same FoundationDB transaction**, zen-serve:
  1. checks that the lease token is still current
  2. checks that the cursor still equals `from`
  3. applies the KV writes and output events
  4. sets the cursor to `to`
  
  All of it commits or none of it does.

### 3.2 What happens in the cases you described

| Situation | Outcome |
|---|---|
| The leader stalls or disconnects *before* committing | Nothing was written. Its lease expires, another device takes it (`token+1`) and processes the same event. |
| The old leader wakes up and tries to commit late | **Rejected:** the token is stale, so it gets 412 `not_leader`. Its effects never land. |
| Two devices race with no lease at all | The cursor check in the transaction lets exactly one commit; the other gets 412 `cursor_moved`. **Still correct.** Leases only prevent wasted duplicate *work*. |
| Network partition | Only the side that can reach FoundationDB's quorum can commit. No split brain. |

Your idea, "the first to write with an event id takes it", is the **cursor-in-transaction** check. It's correct on its own. The lease adds liveness and saves duplicate computation, and the fencing token makes a stale leader harmless.

### 3.3 Options

* **Per topic:**
  * **Broadcast topics:** no consumer group. Every subscriber reads everything.
  * **Leader topics:** one or more consumer groups, each with `partitions = N`. A partition is chosen by a client-side `PRF(partition_key) mod N`, so order holds per entity and N leaders work in parallel.
* **Leader candidates:** any device holding the `consume` right for that topic prefix:
  * browser tabs, where the Web Locks API (`navigator.locks`) first picks one tab per browser to cut contention
  * Node.js workers or bots, which make the most reliable leaders
* **External side effects** (email, payment APIs) can't be exactly-once anywhere. They are at-least-once, with an idempotency key `(topic, versionstamp)` passed to the outside service.

---

## 4. Filesystems and `/unencrypted`

### 4.1 Filesystem configuration

```toml
# zen-serve.toml (server side: only ids, no names needed)
[[fs]]  id = 1   # "/"
[[fs]]  id = 2   # "/home/user"
```

* On the client, an encrypted mount table maps `fs_id → mount path`. Each filesystem has its own master key, keyslots and ACL entry.
* `fs_id` is an **unsigned integer** (u32 on the wire). It's stored as the first element of the FoundationDB tuple key, where the tuple encoding is variable-length: small ids take 2 bytes, so there's no cost to the wider range. `0` is reserved for the replicated plaintext `/unencrypted` store.
* For multi-tenant later, the prefix becomes `(tenant_id, fs_id)`.
* Permission rights per filesystem: `read`, `write`. Per topic prefix: `read`, `append`, `consume` (lead).

### 4.2 `/unencrypted`

| Request | Served from |
|---|---|
| `GET /unencrypted/<path>` | the configured directory, e.g. `unencrypted_dir = "/srv/zen/unencrypted"`. Defaults are **bundled into the binary**. |
| `GET /`, `/index.html` | `/unencrypted/index.html` |
| `GET /favicon.ico`, `/robots.txt`, `/manifest.webmanifest` | aliases into `/unencrypted` (configurable list) |
| `GET /v1/...` | the API |
| Any other `GET` with `Accept: text/html` | `/unencrypted/index.html` (SPA fallback, can be turned off) |

* **Safety:** paths are canonicalised, symlinks out of the directory are refused, and correct `Content-Type`, `ETag` and caching headers are set.
* **Optional service worker:** the file lives at `/unencrypted/sw.js` and is served with `Service-Worker-Allowed: /`, so it can control the whole origin.
* **Optional cross-origin isolation** (needed by WASM programs that block on file calls, e.g. a legacy Linux program running in an emulator or as WASI over zen-fs):

  ```toml
  cross_origin_isolation = false   # default
  ```

  When `true`, every response from zen-serve (`/unencrypted/*`, root aliases, SPA fallback, `/v1/*`) carries:

  ```
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Resource-Policy: same-origin
  ```

  This enables `SharedArrayBuffer` + `Atomics.wait`, so a Web Worker can make zen-fs calls appear synchronous to the program. With COEP on, any resource the app loads from *another* origin must send CORP or CORS headers, so it's off by default. When enabled, it's reported in `GET /v1/info` (G4) so client libraries can choose the synchronous-adapter path.
* **HA option:** `unencrypted_source = "replicated"` stores the directory in FoundationDB (a plaintext `fs_id = 0`, uploaded with `zen-serve static push <dir>`), so every node serves identical files. The default `"dir"` reads from local disk.

---

## 5. Leakage summary

The server sees:
* which filesystem and topic ids exist
* the shape of the key and topic trees
* row and event counts and sizes (unless padded)
* access frequency and timing
* lease holders, as device fingerprints
* group sizes of "fast" indexes, only where opted in

The server never sees names (namespaces, tables, columns, topics, paths), keys, values, file contents or event contents.

---

## 6. Updated milestones

1. `zen-core` (Rust → native + WASM): suites, AEAD, keyslots, hierarchical PRF keys and ids, test vectors.
2. `zen-server` + embedded backend: KV + commit (both modes), Log, Consume, Static, signed ACL.
3. FoundationDB backend + `fdbserver` supervisor (`init`/`join`), backup and PITR (DESIGN-2 §6).
4. `@zen/client` (TS + WASM): KV, transactions, Log, leader consumer, keyslot admin.
5. zen-db (tables, private and fast indexes, query builder), the filesystem library over KV, Loro adapter.
6. Authenticated-namespace Merkle tree, ephemeral pub/sub, `zen-ublk`.
7. Later: `fips` suite, ORAM, tree-CRDT filesystem.
