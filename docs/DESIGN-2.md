# zen-serve: design part 2 (events, clients, permissions, packaging, FIPS)

This follows `DESIGN.md`. Decisions so far:

* **Crypto:** the suite in §2 of DESIGN.md is accepted.
* **Storage:** FoundationDB.
* **Scale:** family-sized deployments first, with an event store that can grow to multi-tenant.
* **Both client stories** are wanted: a lightweight library, and a full in-browser environment.
* **FIPS:** a configurable suite, deferred to later.

---

## 1. Revised architecture: four server primitives, everything else on the client

The server stays dumb and keyless. It offers four primitives, all stored in and replicated by FoundationDB:

| # | Primitive | Server sees | Used for |
|---|---|---|---|
| P1 | **Blocks**: versioned, multi-block conditional commit | block ids, sizes, timing | zen-db pages, file contents, native block device |
| P2 | **Logs**: append-only topics, idempotent append, cursor reads, live push | opaque topic ids, event sizes, timing | event sourcing, CRDT sync, messages, broadcast |
| P3 | **Ephemeral pub/sub**: fan-out only, never persisted | opaque channel ids | presence, cursors, "typing…", WebRTC signalling |
| P4 | **Public bootstrap**: signed, *unencrypted* static files | everything (it's public code) | loader HTML/JS/WASM, SPA fallback |

Two cross-cutting pieces sit alongside them: **access control lists (ACLs)** (§5) and **volume headers and keyslots** (DESIGN.md §2.3).

The client libraries build these on top:

```
zen-db    SQLite pages on P1, optimistic concurrency   (online, serializable SQL)
zen-sync  CRDT / event sourcing on P2, local replica   (offline-first, many writers)
zen-fs    file tree as a tree CRDT on P2 + file chunks on P1
zen-blk   ext4 over P1 via ublk (native, single writer)
```

One FoundationDB transaction can touch P1 and P2 together. "Commit these pages **and** append this event" is atomic, which gives you the transactional outbox pattern for free.

---

## 2. The event log ("Kafka-esque", built on FoundationDB)

### 2.1 Yes, CRDTs and event sourcing live on the client

The server can't read events, so it can't merge, fold or compact them. It only has to:

1. append durably, exactly once
2. assign a total order per topic
3. let consumers read from a cursor
4. wake subscribers

All of that is a small layer over FoundationDB. **We don't need to run Kafka.** Kafka earns its keep at very high throughput, and its exactly-once machinery solves problems FoundationDB transactions solve directly. Apple's QuiCK and the FoundationDB Record Layer are well-known queue designs built on the same pattern.

### 2.2 Data layout

```
(t, topic_id, versionstamp)            -> event envelope           // the log
(t, topic_id, "head")                  -> last versionstamp        // watched for push
(t, topic_id, "dev", device_id)        -> last device_seq          // idempotency
(t, topic_id, "snap", versionstamp)    -> encrypted snapshot ref   // compaction points
```

A **versionstamp** is a 10-byte, cluster-wide, strictly increasing commit version that FoundationDB assigns at commit time. Because FoundationDB is strictly serializable, a reader never sees offset N+1 before offset N. Postgres-based queues get this wrong when sequences commit out of order.

### 2.3 Exactly-once, broken down

Kafka's "exactly-once" is really two things: an **idempotent producer** and **atomic consume-and-update**. We get both cheaply.

**Producer (exactly-once append).** The envelope is `{topic_id, device_id, device_seq, key_epoch, nonce, ciphertext, sig?}`. In one FoundationDB transaction:

| Incoming `device_seq` | Server action |
|---|---|
| `<= last` | duplicate retry: return the original position |
| `== last+1` | append at a fresh versionstamp and update `last` |
| `> last+1` | reject: a gap, so the client must resend in order |

So retries after timeouts or crashes are safe, and per-device order is guaranteed. **Atomic multi-topic appends**, Kafka's "transactions", are just one FoundationDB transaction.

**Consumer (exactly-once effect).** Delivery is at-least-once: pull from a cursor, plus pushes. The client persists `last_applied_versionstamp` **in the same local transaction** as the state change it applied, whether in local SQLite or IndexedDB. That gives effectively-once processing, which is Kafka's exactly-once consume pattern done locally.

With CRDTs it's simpler still. Operations carry unique ids `(device_id, device_seq)` and are idempotent and commutative, so duplicates and reordering are harmless by construction.

**Ordering:**
* Total order per topic comes from the versionstamp.
* Causal order across devices comes from hybrid logical clocks or vector clocks *inside* the encrypted payload.
* Cross-topic order is also available, because versionstamps are global.

### 2.4 Delivery modes

* **Pull:** `GET /v1/log/{topic}?after=<cursor>&limit=`
* **Push:** a WebSocket or SSE stream. Each API server holds **one** FoundationDB watch on `head` per active topic and fans out to all its connections. FoundationDB caps watches per process, so we never create one per client.
* **Direct message:** a per-recipient inbox topic. **Broadcast:** a shared topic. **Topic subscribe:** a subscription to a topic-id prefix (§2.5).
* **Competing consumers / work queues** (lease-based claiming, as in QuiCK): deferred. A family doesn't need them, but the layout allows them later.

### 2.5 Topic names without leaking them

Topic names are private ("chat/alice-bob" says too much). Ids are derived hierarchically, one HMAC per path segment:

```
id("chat")            = HMAC(K_topics, "chat")
id("chat/alice-bob")  = id("chat") ‖ HMAC(K_chat, "alice-bob")
K_chat                = HKDF(K_topics, "chat")      // same tree for encryption keys
```

* The server sees **prefix structure** but never names. So it can enforce "may read `chat/*`" and serve prefix subscriptions.
* Encryption keys use the same tree. Holding `K_chat` lets a member derive every `chat/*` key and no sibling's. **Topic read filters are therefore cryptographic, not only server ACLs.** Revoking access rotates that subtree's epoch.
* What leaks: the shape and depth of the topic tree, and how busy each branch is.

### 2.6 Authenticity of events

* **Group level:** the AEAD tag under the topic key proves the event came from a member.
* **Per-device:** members could otherwise forge each other, and ML-DSA signatures (3.3 KB) are too large for every small event. Instead:
  * each event carries a 64-byte **Ed25519** signature (optional, per topic)
  * every N events or T seconds, each device appends a **hybrid-signed checkpoint** (Ed25519 + ML-DSA-65) covering a hash chain of its own events
* Withheld events show up as gaps in a device's `device_seq`. Fork detection works as in DESIGN.md §2.6.

### 2.7 Compaction, retention and size

* The server can't compact by key, because keys are encrypted. Instead a client periodically writes an **encrypted snapshot** of the folded or CRDT state (blob in P1, reference in `snap`). Events before the oldest snapshot still needed are deleted once retention allows.
* FoundationDB values are capped at 100 KB. Larger events put their body in P1 blocks and append a reference.
* **Scaling:** FoundationDB splits key ranges automatically. Many topics spread across the cluster with no extra work. A single very hot topic hits one storage team at its tail; for multi-tenant scale, a topic can be split into N partitions (`topic_id ‖ p`), as in Kafka. A family will never notice.

### 2.8 Which CRDT

The log is CRDT-agnostic: it carries opaque encrypted updates. The client library ships adapters for:

* **Loro** (Rust/WASM): fast, with a **movable-tree CRDT**, which is ideal for zen-fs. Recommended default.
* **Automerge** (Rust/WASM): mature, with Ink & Switch's Keyhive/Beelay work on E2EE sync.
* **Yjs** (JS): the de-facto standard for collaborative text editors.
* **Plain event sourcing:** app-defined reducers over the ordered log.

**zen-db vs zen-sync:**
* Use **zen-db** (serializable SQL) for invariants that need coordination: uniqueness, balances, "only one person books the slot".
* Use **zen-sync** for everything collaborative and offline.

Both run in one app, and the atomic P1+P2 transaction ties them together.

---

## 3. Filesystems, volumes and the "static/" question

### 3.1 The server can't see paths

Inside an encrypted filesystem the server has no idea which block belongs to `/etc` or `/home/user`. Two consequences:

1. **Permission boundaries must be volume boundaries.** `/` and `/home/user` are **separate volumes**, each with its own master key, keyslots and server ACL. An encrypted mount table (`/etc/fstab`-like) stitches them into one tree on the client.
2. **The server can't serve files out of the encrypted filesystem as web pages.** Only the client can decrypt them. Hence the split below.

### 3.2 Volume types

| Type | Built on | Writers | Typical use |
|---|---|---|---|
| `fs` | tree CRDT (P2) + chunks (P1) | many, offline | home dirs, web app sources, documents |
| `db` | SQLite pages (P1) | many, online, optimistic concurrency | app databases |
| `blk` | raw blocks (P1) | one (lease) | native ext4/btrfs mount |
| `topics` | P2 | many | events, messages |

How `fs` handles conflicts:
* The **tree** (create, rename, move, delete) is a movable-tree CRDT, so concurrent moves never corrupt it (Kleppmann et al., 2021).
* **File contents** are immutable encrypted chunks with random ids. Concurrent edits to the same file resolve to last-writer-wins plus a kept "conflict copy", Dropbox-style. Text files can opt into a text CRDT.

### 3.3 Serving the web app: two layers

* **P4, public bootstrap** (served by zen-serve, plaintext):
  * a tiny `index.html`, the loader JS, the WASM core and the service worker
  * **SPA fallback**: an unknown path returns `index.html`
  * signed with the deployment's hybrid release key
* **App assets** live in the encrypted `fs`, e.g. `/srv/www/` or `static/`. After unlock, the **service worker** decrypts and serves them for the app's origin, with the same SPA fallback. The server never sees the app's code, routes or assets.

### 3.4 The honest weak point: bootstrapping web E2EE

Whoever controls the server can serve a malicious loader that steals keys. Every browser-based E2EE product (Proton, Bitwarden web, Cryptpad) shares this problem. Mitigations, from weakest to strongest:

1. **Subresource Integrity** (SRI) and a pinned, hybrid-signed manifest that the loader verifies before running anything.
2. A **service worker** that pins code after first use. This is trust-on-first-use, and it's not watertight: the browser re-checks `sw.js` with the server and will eventually install a changed one.
3. A **browser extension** that checks bootstrap hashes, as Meta's Code Verify does, or Chrome **Isolated Web Apps** (signed bundles).
4. A **native or desktop client** (Tauri) or the Node CLI.

The docs should say plainly: a browser client is only as trustworthy as the server operator *at load time*. The project ships options 1 and 2, and options 3 and 4 for people who need more.

---

## 4. Client library: two tiers, one core

Both of your options are tiers of the **same** library:

```
@zen/core   (Rust → WASM, single source of truth for formats + crypto)
  └─ @zen/client   TS API: volumes, keyslots, P1/P2/P3, ACLs. Node + browser. Option 2.
       ├─ zen-db     SQLite-WASM (OPFS) + custom VFS
       ├─ zen-sync   local replica (SQLite/OPFS or IndexedDB) + CRDT adapters
       ├─ zen-fs     tree CRDT + chunk store, POSIX-ish API
       └─ @zen/sw    service worker: decrypt+serve app assets, offline cache.   ┐
            └─ @zen/webcontainer  two-way sync adapter for WebContainers      ┘ Option 1
```

**Option 2, the library:**
* Works in Node and the browser with no service worker.
* **Data can still be offline**, because the zen-sync replica persists in OPFS or IndexedDB. Only the *app shell* needs a service worker to load offline.
* Node support also covers bots and server-side agents. Each one is a device with its own keyslot, so it holds keys and the zen-serve server still doesn't.

**Option 1, StackBlitz-like:**
* **WebContainers is proprietary.** StackBlitz requires a commercial licence for most production use, and its filesystem can't be swapped for a custom backend. Integration would be a **two-way sync**: `mount()` the decrypted tree, then `fs.watch` → zen-fs, and zen-fs events → `fs.writeFile`. That works, but it's a mirror, not an integral filesystem.
* Open alternatives:
  * Your own environment: zen-fs exposed through a service worker, plus SQLite-WASM.
  * **PGlite** (Postgres in WASM) or **SQLite-WASM** with a custom VFS.
  * Later, a WASI runtime with zen-fs as its filesystem.
* Recommendation: build Option 2 first. Option 1 then consists of the `@zen/sw` and `@zen/webcontainer` packages added on top.

**Local data at rest in the browser:** the local replica is encrypted with a non-extractable WebCrypto key held in IndexedDB. That protects against casual disk theft, not against a compromised browser profile.

---

## 5. Permissions

The server can't read an `/etc/permissions` file stored inside an encrypted volume. So permission is enforced at **two layers**:

| Layer | Who enforces | Protects against |
|---|---|---|
| **Cryptographic**: who holds which volume or topic-subtree key | math | reading, even if the server is malicious |
| **Server ACL**: a *signed* policy document the server can parse | zen-serve | vandalism, deletion, downloading ciphertext, quota abuse |

The ACL document is plaintext to the server but contains only **opaque ids**: user/device key fingerprints, volume ids and topic-id prefixes. It's signed with the admin hybrid key, the server verifies it, and changes to it are versioned and committed with CAS.

```
subject: <user fp> | group:<id>
grants:
  vol:<id(/)>            read
  vol:<id(/home/user)>   read, write
  topics:<id(chat)>/*    read, append
  topics:<id(inbox/u)>   read
  admin                  (manage ACL, keyslots, retention)
```

The **readable** version (names, groups, comments) lives encrypted at `/etc/zen/permissions` for humans and clients. The client compiles it into the signed opaque ACL and distributes keys to match. The two always agree because one is generated from the other.

**Limitation:** a permission can't be narrower than a volume or a topic subtree. "Write only `/home/user/docs`" means making `docs` its own volume.

---

## 6. FoundationDB as part of the product, without docker-compose

What's possible:

| Approach | Verdict |
|---|---|
| Run `fdbserver` **in-process** as a library | Not possible. FoundationDB's servers run on their own Flow runtime and process model; there is no embeddable server. |
| **Link the client** `libfdb_c` | Yes. The Rust `foundationdb` crate does this. The library is loaded from our install dir, with a pinned version. |
| **Bundle and supervise `fdbserver`** | **Yes, recommended.** zen-serve takes over `fdbmonitor`'s job (see below). |
| Git **submodule** + build from source | Yes, in CI: pin a 7.3.x tag and build `fdbserver` + `libfdb_c` once per target (Linux x86_64/aarch64). The FoundationDB build is heavy, so it belongs in CI, not on users' machines. Apache-2.0 is compatible with our MIT licence; we ship its NOTICE. |

Bundling and supervising `fdbserver` means **one binary/package, one config, one systemd unit**:

```
zen-serve init                 # node 1: generates cluster file, starts N fdbserver children,
                               #         `configure new single ssd`, starts API on :443
zen-serve join <token>         # node 2..n: fetch cluster file, start children,
                               #            auto-raise redundancy + coordinators
                               #            (1 node → single, ≥3 → double, ≥5 → triple)
zen-serve status | backup | upgrade
```

* Process layout: zen-serve spawns and restarts `fdbserver` children (one per core or disk), keeps the cluster file and coordinators in sync, and exposes their health through its own API.
* The API side links `libfdb_c`.
* Docker becomes an *optional* packaging of the same binary.

Also kept: the **embedded backend** (redb, no FoundationDB at all) for development, tests, and the smallest single-node installs. It sits behind the same `Storage` trait, and a cluster can migrate from it to FoundationDB with `zen-serve migrate`.

---

## 7. FIPS, explained

**FIPS 140-3** is the US government standard (NIST) for cryptographic modules. It matters if you sell to US federal agencies or contractors, and to some regulated finance or health customers. It has two parts:

1. **Approved algorithms only:**
   * allowed: AES-GCM, SHA-2/3, HMAC/HKDF, PBKDF2, ECDH/ECDSA P-256/384, Ed25519 (FIPS 186-5), and post-quantum ML-KEM (FIPS 203), ML-DSA (204), SLH-DSA (205)
   * **not** allowed: ChaCha20-Poly1305, BLAKE3, Argon2
2. **A validated module.** The certificate covers a specific *implementation*, e.g. AWS-LC FIPS (`aws-lc-rs` with the `fips` feature, which includes ML-KEM), BoringCrypto, or the OpenSSL 3 FIPS provider. Choosing the right algorithms isn't enough on its own.

FIPS does **not** make things more secure than the modern suite. It's a compliance label.

**Is it a simple switch?** For the algorithm set, yes: a per-volume `suite` field, just like LUKS's cipher spec.

| | `modern` (default) | `fips` |
|---|---|---|
| Block AEAD | XChaCha20-Poly1305 | **XAES-256-GCM** (C2SP spec: AES-GCM with a 192-bit nonce via a NIST SP 800-108 KDF, so the same 24-byte nonce and the same block format) |
| Hash / Merkle | BLAKE3 | SHA-384 |
| Password KDF | Argon2id | PBKDF2-HMAC-SHA-512 (weaker against GPUs, so prefer FIDO2/hardware keyslots in this mode) |
| KEM | X-Wing (ML-KEM-768 + X25519) | ML-KEM-1024 + ECDH P-384 hybrid |
| Signatures | Ed25519 + ML-DSA-65 | ML-DSA-87 + ECDSA P-384 (or Ed25519) |

* **XChaCha stays available.** The suite is chosen per volume, and one deployment can hold volumes of both suites.
* **The catch is the browser.** WebCrypto isn't a validated module you can rely on, and our WASM core wouldn't be validated either. **FIPS mode is realistic only for native clients** (CLI, ublk, desktop) using `aws-lc-rs` in FIPS mode. The deferred plan:
  1. Define the `suite` field in the header now.
  2. Implement `modern` first.
  3. Add `fips` later.

---

## 8. Updated milestones

1. `zen-core`: formats with a `suite` field, AEAD blocks, keyslots, Merkle tree, hierarchical ids and keys, test vectors.
2. `zen-server` + **embedded backend**: P1 blocks, P2 logs (exactly-once append, cursors, WebSocket push), P4 bootstrap, signed ACLs.
3. **FoundationDB backend + supervisor**: `init`/`join`, MVCC retention, PITR, snapshots, ciphertext backup.
4. `@zen/client` (TS + WASM, Node + browser): volumes, keyslots, logs, ACL tooling.
5. **zen-sync** (Loro adapter, local replica) and **zen-db** (SQLite-WASM VFS).
6. **zen-fs** (tree CRDT + chunks), `@zen/sw` (decrypt-and-serve, SPA fallback, offline).
7. `@zen/webcontainer` adapter, `zen-ublk`, P3 ephemeral pub/sub.
8. Later: `fips` suite, partitioned topics and work queues, ORAM volumes, verification extension.

## 9. Open questions

1. Is Loro acceptable as the default CRDT? Alternatives are Automerge, or both with Yjs for text.
2. Is it acceptable that web clients are only as trustworthy as the server at load time? If not, the verification extension or desktop client moves earlier.
3. Is the WebContainers commercial licence acceptable for Option 1, or do we build our own environment?
4. Do we need per-event Ed25519 signatures by default, or only hybrid-signed checkpoints?
