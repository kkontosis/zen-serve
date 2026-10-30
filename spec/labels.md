# Domain-separation label registry

Every KDF context, AAD domain and signature purpose uses a unique label from this table. A label is never reused for a different purpose. Changing a label's meaning means minting a new `zen/v2/...` label.

`tests/labels.rs` checks that this file and `zen_core::labels::ALL` list exactly the same set.

## KDF labels (`KDF(label, key, info)`)

| Label | Key | Info | Output |
|---|---|---|---|
| `zen/v1/kv-data` | MK_e | `u32(fs_id) ‖ u32(epoch)` | AEAD key for KV values |
| `zen/v1/epoch-chain` | MK_e | `u32(fs_id) ‖ u32(epoch)` | key sealing MK_{e-1} |
| `zen/v1/kv-name` | NK | `u32(fs_id)` | root of the KV naming chain |
| `zen/v1/topic-name` | NK | `u32(fs_id)` | root of the topic naming chain |
| `zen/v1/topic-data` | MK_e | `u32(fs_id) ‖ u32(epoch)` | root of the topic data chain |
| `zen/v1/fs-data` | MK_e | `u32(fs_id) ‖ u32(epoch)` | AEAD key for filesystem meta, manifests and chunks (formats.md §11) |
| `zen/v1/fs-file` | a file key (32 B, formats.md §11.3) | `u32(fs_id)` | AEAD key for the chunks of a version-2 file (formats.md §11.4) |
| `zen/v1/name-chain` | chain key N_{i-1} | `lp(element)` | next naming-chain key N_i |
| `zen/v1/topic-data-chain` | topic data key D_{i-1} | `lp(segment)` | next topic data key D_i |
| `zen/v1/event-key` | topic naming key N_topic | empty | key for event key tokens |
| `zen/v1/event-aead` | topic data key D_topic | empty | AEAD key for events |
| `zen/v1/broker-group` | topic naming key N_topic | empty | PRF key turning an app's consumer-group name into the server's group id (zendb.md §11.3) |
| `zen/v1/keyslot-kek` | slot secret | `slot_id` (16 B) | keyslot key-encryption key |
| `zen/v1/sig-ed25519` | identity seed | empty | Ed25519 secret seed |
| `zen/v1/sig-ml-dsa-65` | identity seed | empty | ML-DSA-65 seed ξ |
| `zen/v1/device-sig` | device secret | empty | device identity seed |
| `zen/v1/device-kem` | device secret | empty | X-Wing decapsulation seed |
| `zen/v1/password-sig` | Argon2id output of a password (formats.md §7.5) | empty | identity seed of a password-derived key |
| `zen/v1/db` | NK | `u32(fs_id) ‖ lp(ns)` | root secret of a zen-db database (zendb.md §2.2) |
| `zen/v1/db-boundary` | K_db | `index_id` (16 B) | PRF key choosing private-index node boundaries and shards (zendb.md §5.4) |
| `zen/v1/db-node-id` | K_db | `index_id` (16 B) | PRF key of private-index node ids (zendb.md §5.4.2); with `index_id = 0¹⁶`, of the state tree (§14.2) |
| `zen/v1/db-crdt` | K_db | `table_id` (16 B) | PRF key of a CRDT table's field and element tokens (zendb.md §19.2) |
| `zen/v1/svc-aead` | the topic data key D_svc of a service id | empty | service key `K_svc`: seals svc states and output-key escrows (formats.md §13) |
| `zen/v1/svc-inbox` | X-Wing shared secret of a connection | `u32(fs_id) ‖ lp(svc) ‖ conn` (16 B) | key sealing a connection's input chunks (formats.md §13.3) |

## Hash labels (`BLAKE3.derive_key(label, data)`)

| Label | Use |
|---|---|
| `zen/v1/fingerprint` | `FP(public_bytes)` |
| `zen/v1/acl-chain` | `H(doc)` of a signed ACL, chaining ACL versions (formats.md §9.2) |
| `zen/v1/range-hash` | hash of a KV range for `expect_ranges` (api.md §6) |
| `zen/v1/tree-op-chain` | per-tree chain over filesystem operations (formats.md §11.5) |
| `zen/v1/fs-page` | digest of a chunk-list page (formats.md §11.6) |
| `zen/v1/passkey-id` | a passkey's credential-store id from its WebAuthn credential id (auth.md §4.1, formats.md §6) |
| `zen/v1/opaque-keyslot` | the secret of an OPAQUE export-key keyslot from the 64-byte export key (formats.md §6, type 5) |
| `zen/v1/db-parts-digest` | digest of a zen-db value split into parts: a large row or message body (zendb.md §4.2, §10.3) |
| `zen/v1/svc-reveal` | `key_hash` of an output record's key, which a reveal must match (formats.md §13.1) |
| `zen/v1/checkpoint-hash` | `HC(body)` of a device checkpoint, chaining a device's checkpoints and naming them in `seen` (formats.md §14.1) |
| `zen/v1/fs-header-chain` | `HH` of a signed fs header, chaining header versions (formats.md §12.1) |
| `zen/v1/event-chain` | `HE(body)` of a version-2 event body, chaining a writer's events on a topic (formats.md §5.1) |
| `zen/v1/tree-state` | leaves and inner nodes of a filesystem tree's state hash (fs.md §9) |
| `zen/v1/db-state-value` | `VH`, the hash of a value's plaintext in an authenticated zen-db database's state tree (zendb.md §14.2) |
| `zen/v1/db-ns-root` | `NH`, the hash of an NsRoot, chaining a namespace's roots and naming them in checkpoints (zendb.md §14.4) |
| `zen/v1/blk-leaf` | `leaf(i)`, the hash of a stored block of a volume (blk.md §2) |
| `zen/v1/blk-node` | `hash(l, j)`, the hash of a volume's tree node (blk.md §2) |
| `zen/v1/blk-root` | `RH`, the hash of a volume's root record, chaining its roots and naming them in checkpoints (blk.md §3) |

## AAD domains (`label ‖ 0x00 ‖ header ‖ context`)

| Label | Object |
|---|---|
| `zen/v1/aad/kv` | sealed KV value |
| `zen/v1/aad/event` | sealed event |
| `zen/v1/aad/epoch-chain` | epoch-chain record |
| `zen/v1/aad/keyslot` | keyslot |
| `zen/v1/aad/fs-meta` | filesystem node meta (kind 4) |
| `zen/v1/aad/fs-manifest` | filesystem manifest (kind 5) |
| `zen/v1/aad/fs-chunk` | filesystem chunk (kind 6) |
| `zen/v1/aad/crdt-value` | CRDT value (kind 7; zendb.md §19.2) |
| `zen/v1/aad/local` | Local record (kind 8; zendb.md §9.3) |
| `zen/v1/aad/svc-state` | svc state (kind 9; formats.md §13) |
| `zen/v1/aad/svc-key` | svc output key escrow (kind 10; formats.md §13.1) |
| `zen/v1/aad/svc-out` | body of an output record: `label ‖ 0x00 ‖ context`, no header (formats.md §13.1) |
| `zen/v1/aad/svc-in` | sealed input chunk: `label ‖ 0x00`, no header or context (formats.md §13.3) |
| `zen/v1/aad/checkpoint` | device checkpoint (kind 11; formats.md §14.2) |

## OPAQUE (auth.md §8)

| Label | Use |
|---|---|
| `zen/v1/opaque` | prefix of the AKE context of an OPAQUE sign-in: `"zen/v1/opaque" ‖ 0x00 ‖ origin` |

## Signatures

`zen/v1/sig` is the domain prefix of every signed message: `SM = "zen/v1/sig" ‖ 0x00 ‖ lp(purpose) ‖ msg`. Purposes:

| Purpose | Signed by | Message |
|---|---|---|
| `zen/v1/sig/device-cert` | user identity | device certificate body (formats.md §7.4) |
| `zen/v1/sig/commit` | device | reserved: roots are chained inside sealed values and named by device checkpoints (formats.md §14) instead |
| `zen/v1/sig/checkpoint` | device | device checkpoint body (formats.md §14.2) |
| `zen/v1/sig/acl` | admin identity | signed ACL document (formats.md §9) |
| `zen/v1/sig/membership` | admin identity | reserved; the membership log is the ACL chain (formats.md §9) |
| `zen/v1/sig/session` | device | sign-in challenge (formats.md §10) |
| `zen/v1/sig/event` | device | reserved: events are chained (formats.md §5.1) and their chains named by device checkpoints |
| `zen/v1/sig/tree-checkpoint` | device | reserved: tree states ride in device checkpoints (fs.md §9, formats.md §14) |
| `zen/v1/sig/password-session` | password-derived key | sign-in challenge (formats.md §7.5, §10) |
| `zen/v1/sig/db-root` | device | reserved: authenticated zen-db roots ride in device checkpoints (zendb.md §14, formats.md §14) |
| `zen/v1/sig/fs-header` | admin identity | signed fs header (formats.md §12.1) |
| `zen/v1/sig/app-bundle` | release key | reserved: a signed app bundle checked by a bootstrap service worker (milestone 11) |

Signing with a purpose not in this table is an error.

## Test-only

`zen/test/det-rng` is the deterministic RNG used for test vectors. It is never used by production code.
