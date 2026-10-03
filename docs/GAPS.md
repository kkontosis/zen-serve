# zen-serve: spec gap analysis (before build)

Each gap gets a proposed resolution.

**Tier 1** gaps change on-disk or on-wire formats. They should be settled before writing `zen-core`.
**Tier 2** gaps change server state layout or semantics. They should be settled before the matching milestone.
**Tier 3** can be settled during the build.

---

## Tier 1: blocks the format work

### G1. Identity, device enrollment and key-substitution attacks
**Gap:** the server stores members' public keys. A malicious server can **swap in its own public key** when an admin adds a member or device. The master key is then wrapped to the server, and E2EE is silently broken. Also unspecified so far: users versus devices, the first-admin ceremony, invites, and losing a device.

**Proposal:**
* **Identities:**
  * **User identity** = a long-lived hybrid signing key (Ed25519 + ML-DSA).
  * Each **device key** (X-Wing + signing) is **certified by the user key**.
  * The user key is held by the user's first device and backed up by the recovery key.
* **Invites:** out-of-band code or QR code with a short authentication string (SAS) or fingerprint comparison, as in Signal safety numbers. Keyslots are wrapped only to keys verified this way.
* **A signed membership log**, hash-chained and appended by admins, holds all public keys and certificates. Clients verify the chain and pin its head. Optionally, a witness provides key transparency.
* **Lost device:** revoke its certificate, rotate the epoch (G2).
* **Lost every device:** the recovery key restores the user key.
* **Admin role:** a set of admin user keys. Changing the ACL needs a signature from an admin, optionally k-of-n.

### G2. Key rotation would change every key token and topic id
**Gap:** key tokens (DESIGN-3 §2.2) and topic ids (DESIGN-2 §2.5) are derived from keys that rotate on revocation. A rotation would change **every stored key and every topic id**: rewriting the whole keyspace and breaking ACL prefixes, cursors and subscriptions.

**Proposal:** split the keys in two.
* `K_name_fs`: a **naming key**. Long-lived, *not* rotated on revocation, used only for PRF tokens.
* `K_data_fs,e`: the **data key**. Rotated per epoch, used for the AEAD.

A revoked member can still compute tokens for names they guess, but they've lost server access (ACL) and can't decrypt new values. Re-keying the names is a separate, explicit offline migration. **Formats must carry `key_epoch` on values only, never in keys.**

### G3. Canonical encodings and domain separation
**Gap:** no byte-exact spec exists yet for envelopes, AAD, signed structures or derivation labels. Without one, the Rust core and any future implementation will diverge, and signatures over non-canonical data are a known bug class.

**Proposal:**
* **Deterministic CBOR** (RFC 8949 §4.2) for all structured data.
* Fixed binary layouts for the hot paths: block, value header, event envelope.
* A **registry of domain-separation labels**, e.g. `"zen/v1/kv-name"`, `"zen/v1/topic-key"`, `"zen/v1/commit-sig"`, with every HKDF, PRF and signature using a unique label.
* `spec/` holds the formats as a document, plus **test vectors** that CI checks.

### G4. Versioning and negotiation
**Gap:** nothing says how clients and servers of different versions talk, or how formats evolve.

**Proposal:**
* Every stored object has `format_version` + `suite`.
* The API is under `/v1`.
* `GET /v1/info` returns the server version, supported suites, limits and features.
* The client library refuses newer formats it can't read. The server never rewrites ciphertext formats.

### G5. Chunk and blob garbage collection
**Gap:** a file's chunk list lives *inside* an encrypted payload, so the server can't tell which chunks are still referenced. Overwritten file versions, resolved conflict siblings, deleted files and large-event bodies would leak storage forever.

**Proposal:** each content reference carries its **chunk ids in plaintext metadata**. That reveals nothing new: chunk counts already leak. The server keeps **reference counts** inside the same transaction, and deletes a chunk when its count reaches 0 and it's outside the retention or snapshot window.

The same pattern applies to large event bodies (event → chunk refs), freed by event retention.

### G6. The encrypted catalog (zen-db schema)
**Gap:** where do table definitions and index definitions live? How are schema migrations done?

**Proposal:**
* A reserved, encrypted **catalog** namespace per database: tables, indexes, the schema version and a migration log.
* Migrations are client-run, resumable jobs: build the index, then flip it to active in one transaction.
* Clients check the schema version on open.

---

## Tier 2: before the matching milestone

### G7. Poison events and stuck sequential consumers (Log/Consume milestone)
**Gap:** in `sequential` or `per_key` mode, an event whose handler always fails **blocks its key, or the whole topic, forever**.

**Proposal:** a per-group retry policy: `max_attempts`, backoff, then **dead-letter** the event.
1. In one transaction, append it to `<topic>.dlq` (same key) and advance the cursor.
2. Record the attempt count in the claim.
3. Admin APIs: `skip`, `retry`, `replay from offset`.

Optionally `on_poison: block` for groups where order matters more than progress.

### G8. Repartitioning and changes to consumer groups
**Gap:** changing a group's N, or its mode, breaks per-key ordering.

**Proposal:** partition count and mode are **immutable** per group. To change them, create a new group starting at the old group's low watermark and drain the old one: a "group handover" API.

### G9. Gap-free subscriptions and client cache coherence
**Gap:**
1. Push plus reconnect must never skip or reorder events.
2. Client-side caches (B+tree upper levels, CRDT state) can go stale.

**Proposal:**
1. A subscription is `(cursor)` → the server streams `after cursor` from storage, then switches to live push with no gap. The FoundationDB watch only wakes it; the data always comes from a range read.
2. Every cached item keeps its version. Cached nodes used by a transaction go into `expect` or the read set, so stale cache causes a retry, never a wrong commit. An optional invalidation feed per filesystem handles freshness.

### G10. Server-side CRDT inside FoundationDB's limits
**Gap:**
* Undo/redo of a late tree operation re-applies every later move. A large offline batch can exceed the 5 s / 10 MB transaction limits.
* All operations on one tree serialize at its move log, so the tree is a single point of contention.

**Proposal:**
* A batch of offline operations is applied in **ordered chunks**, each one transaction. The client sees a batch id and a progress cursor; each operation stays atomic on its own.
* A **maximum late-operation horizon**: operations older than the stability point are rejected and the client rebases.
* Contention: a single tree comfortably handles family load, and one tree per filesystem keeps them independent.

### G11. Mixing CRDT ops with transactions
**Gap:** what isolation do `crdt_ops` get inside a `commit` with read conflicts?

**Proposal:**
* CRDT ops never abort *other* transactions because of their own reads. The merge is internal and conflicts are retried by the server.
* They **do** commit atomically with the KV writes, appends and consumes in the same request.
* Document: "CRDT ops are always accepted, never conflict."

### G12. Quotas, limits and abuse by members
**Gap:** an authorized member, or a compromised device, can fill storage, create millions of nodes or topics, or flood the event log.

**Proposal:**
* Per-filesystem and per-user quotas (bytes, keys, nodes, events per second), maintained with FoundationDB atomic counters.
* Server-wide limits in `/v1/info`.
* Retention policies per topic and filesystem in the signed ACL, so only admins can shorten history.

### G13. Garbage collection of server bookkeeping
**Gap:** idempotency records (`commit_id`), expired claims and leases, the move log and dead subscriptions grow without bound.

**Proposal:** each has a TTL in commit versions, and a background sweeper in zen-serve removes expired entries. `commit_id` records are kept 24 h by default. A client must not retry with the same `commit_id` after that; the library enforces this.

### G14. Backup and restore granularity
**Gap:** restoring one filesystem to a point in time but not its topics, cursors and CRDT logs leaves them inconsistent.

**Proposal:**
* The unit of restore is a **tenant**: all filesystems, topics, cursors and the ACL together.
* **Clone to a new tenant at time T** is supported, for "look at yesterday" without disturbing live data.
* Restoring a single filesystem is an explicit, documented "you'll see cursor anomalies" operation.

---

## Tier 3: during the build

| # | Gap | Proposal |
|---|---|---|
| G15 | **Browser key storage.** ML-DSA / X-Wing aren't in WebCrypto, so private keys sit in WASM memory and can be stolen by XSS. | Wrap them at rest with a non-extractable WebCrypto AES key. Strict CSP + Trusted Types headers on `/unencrypted` by default. Document the XSS risk. |
| G16 | **Multiple tabs:** duplicate connections, caches and leases. | A SharedWorker (with a Web Locks fallback) owns the connection and cache per origin. |
| G17 | **Cross-origin apps** using a zen-serve instance. | Same-origin by default. An explicit CORS allowlist in config. |
| G18 | **TLS for family self-hosting.** | Built-in ACME (Let's Encrypt) in zen-serve. TLS between FoundationDB nodes on by default when there's more than one node. |
| G19 | **Logs and metrics leak metadata** (tokens, ids, timing). | Logging policy: no key or topic tokens in logs, only aggregate metrics. Debug logging is opt-in. |
| G20 | **Clocks:** HLC clamp and ACME rely on server time. | Warn on startup on large skew. Leases use commit versions, so they're unaffected. |
| G21 | **Ephemeral pub/sub authenticity.** | Same AEAD under the topic key, plus a sequence number to block replays within a session. |
| G22 | **Upgrades of the bundled FoundationDB.** | Pin a 7.3.x version. Use the multi-version client for rolling upgrades. `zen-serve upgrade` orchestrates it. |
| G23 | **Testing strategy.** | Cross-language test vectors. Property tests for CRDT convergence (random op orders → same state). Run the server in FoundationDB's simulation where possible, plus fault injection in Jepsen style for leases, cursors and idempotency. |
| G24 | **Terminology drift:** "volume", "filesystem" and "fs" are used for the same thing. | A glossary in `spec/`. **fs** = the encrypted namespace, with its own keys and ACL. "Volume" is retired. |
| G25 | **Mobile Argon2 memory.** | Clients benchmark and choose parameters per keyslot. A floor of 64 MiB, and a warning below 256 MiB. |
| G26 | **Withheld events / log freshness.** Checkpoints detect missing events from *one device*, but the server can hide a whole topic tail. | Topic heads are included in the signed per-device checkpoints, so clients gossip heads. Same fork-consistency limit as for blocks. |

---

## Summary

Tier 1 has **six decisions** to settle before the build: G1–G6. Proposals for all of them are above. Everything else has a home in a later milestone.
