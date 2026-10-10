# Glossary

| Term | Meaning |
|---|---|
| **fs** | An encrypted namespace: the unit of keys, keyslots, ACL grants and permissions. Mounted by clients at a path (e.g. `/`, `/home/user`). Holds KV data, files, zen-db databases, topics and CRDT objects. Replaces the retired term *volume*. |
| **fs_id** | Unsigned integer identifying an fs, `u32` on the wire. `0` is reserved for the replicated plaintext `/unencrypted` store and never has keys. |
| **naming key** (NK) | Long-lived 32-byte secret per fs. Used *only* to derive PRF tokens. **Not rotated** when a member is revoked, so tokens stay stable (G2). |
| **epoch** | `u32` generation counter of an fs's data keys, starting at 0. Incremented on revocation. |
| **epoch key** (MK_e) | 32-byte secret per (fs, epoch). Every AEAD key derives from it. |
| **epoch-chain record** | `MK_{e-1}` sealed under a key derived from `MK_e`. Lets current members read older data. |
| **key bundle** | `u32(fs_id) ‖ u32(epoch) ‖ NK ‖ MK_e` (72 bytes): what a keyslot wraps. |
| **keyslot** | One wrapping of the key bundle, unlocked by a passphrase, a recovery key, or a device's X-Wing key. |
| **key token** | Server-visible stand-in for a logical KV key tuple: 16 bytes per element, from a PRF chain. A parent tuple's token is a prefix of its children's tokens. |
| **stored key** | The key token as stored in FoundationDB, after the `fs_id` tuple element. |
| **topic id** | Server-visible token of a topic path, built like a key token from the fs's topic naming root. |
| **event key token** | 16-byte per-topic PRF of an event's optional `key`, sent next to the topic so the server can keep per-key order without learning the key. |
| **sealed object** | Header + XChaCha20-Poly1305 ciphertext: a KV value, an event or an epoch-chain record. |
| **envelope** | The sealed event as stored in the log. |
| **identity** | A hybrid (Ed25519 + ML-DSA-65) signing key derived from a 32-byte seed. A **user identity** is long-lived and certifies devices. |
| **device** | A client installation, with one 32-byte device secret that derives a signing identity and an X-Wing key. Its **device id** is the fingerprint of its public keys. |
| **device certificate** | A user identity's hybrid signature over a device's public keys (G1). |
| **fingerprint** (FP) | `BLAKE3.derive_key("zen/v1/fingerprint", public_bytes)`, 32 bytes. Compared out of band when inviting someone. |
| **commit** | One atomic, conditional request to `/v1/commit`: validate reads, write KV, append events, advance consumer cursors, apply CRDT ops. Identified by a random 128-bit **commit_id** for idempotency. |
| **versionstamp** | FoundationDB's 10-byte, cluster-wide, strictly increasing commit version. Used as the event offset. |
| **suite** | The algorithm set, recorded per object (spec/suites.md). |
| **tree** | A filesystem tree in an fs, identified by a random 16-byte id (spec/fs.md). |
| **node** | A file, directory or symlink in a tree, identified by a random 16-byte id. `ROOT` (all zeros) and `TRASH` (all `FF`) are reserved. |
| **HLC** | Hybrid logical clock, `unix_ms << 16 \| counter` (formats.md §11.1). |
| **ts** | An operation's timestamp `(hlc, device fingerprint)`, the total order of tree and meta operations. |
| **move** | The tree operation: create, move, rename, delete (move to trash) and restore are all moves. |
| **version** | One content value of a file node: chunk ids plus a sealed manifest. Concurrent writes leave several versions, called **siblings**. |
| **dot** | A version's 12-byte id, `versionstamp ‖ u16(i)`, assigned by the server. |
| **chunk** | An immutable sealed piece of file content (64 KiB of plaintext), with a random 16-byte id. |
| **database** | A zen-db database: `(fs, ns)`, its rows, indexes and catalog under the KV path `("zen", "db", ns)` (zendb.md §2). |
| **catalog** | A database's encrypted schema records: tables, indexes, schema version, migration log (zendb.md §3, G6). |
| **private index** | A zen-db index stored as a prolly tree of sealed, content-addressed nodes; supports order and ranges (zendb.md §5.4). |
| **message** | A broker event whose payload is a typed `Msg` with an id, optional correlation and saga fields (zendb.md §10). |
| **saga** | A series of local transactions in different services, coordinated by an orchestrator, with compensations on failure (zendb.md §12.5). |
| **CRDT table** | A zen-db table whose rows the server merges field by field (last writer wins, counters, sets) instead of serializing transactions; works offline (zendb.md §19). |
| **horizon** | How far back (default 7 days) a late operation may reach. Older ones are refused with `stale_op` and **rebased**: reissued with a fresh HLC. |

