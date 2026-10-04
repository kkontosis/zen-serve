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
| `zen/v1/name-chain` | chain key N_{i-1} | `lp(element)` | next naming-chain key N_i |
| `zen/v1/topic-data-chain` | topic data key D_{i-1} | `lp(segment)` | next topic data key D_i |
| `zen/v1/event-key` | topic naming key N_topic | empty | key for event key tokens |
| `zen/v1/event-aead` | topic data key D_topic | empty | AEAD key for events |
| `zen/v1/keyslot-kek` | slot secret | `slot_id` (16 B) | keyslot key-encryption key |
| `zen/v1/sig-ed25519` | identity seed | empty | Ed25519 secret seed |
| `zen/v1/sig-ml-dsa-65` | identity seed | empty | ML-DSA-65 seed ξ |
| `zen/v1/device-sig` | device secret | empty | device identity seed |
| `zen/v1/device-kem` | device secret | empty | X-Wing decapsulation seed |
| `zen/v1/password-sig` | Argon2id output of a password (formats.md §7.5) | empty | identity seed of a password-derived key |

## Hash labels (`BLAKE3.derive_key(label, data)`)

| Label | Use |
|---|---|
| `zen/v1/fingerprint` | `FP(public_bytes)` |
| `zen/v1/acl-chain` | `H(doc)` of a signed ACL, chaining ACL versions (formats.md §9.2) |
| `zen/v1/range-hash` | hash of a KV range for `expect_ranges` (api.md §6) |
| `zen/v1/tree-op-chain` | per-tree chain over filesystem operations (formats.md §11.5) |
| `zen/v1/passkey-id` | a passkey's credential-store id from its WebAuthn credential id (auth.md §4.1, formats.md §6) |
| `zen/v1/opaque-keyslot` | the secret of an OPAQUE export-key keyslot from the 64-byte export key (formats.md §6, type 5) |

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

## OPAQUE (auth.md §8)

| Label | Use |
|---|---|
| `zen/v1/opaque` | prefix of the AKE context of an OPAQUE sign-in: `"zen/v1/opaque" ‖ 0x00 ‖ origin` |

## Signatures

`zen/v1/sig` is the domain prefix of every signed message: `SM = "zen/v1/sig" ‖ 0x00 ‖ lp(purpose) ‖ msg`. Purposes:

| Purpose | Signed by | Message |
|---|---|---|
| `zen/v1/sig/device-cert` | user identity | device certificate body (formats.md §7.4) |
| `zen/v1/sig/commit` | device | commit record / signed root (later milestone) |
| `zen/v1/sig/checkpoint` | device | per-device event checkpoint, every `sig_every` events (later milestone) |
| `zen/v1/sig/acl` | admin identity | signed ACL document (formats.md §9) |
| `zen/v1/sig/membership` | admin identity | reserved; the membership log is the ACL chain (formats.md §9) |
| `zen/v1/sig/session` | device | sign-in challenge (formats.md §10) |
| `zen/v1/sig/event` | device | a single event (later milestone) |
| `zen/v1/sig/tree-checkpoint` | device | filesystem tree checkpoint over (state hash, count, chain) (fs.md §9, later milestone) |
| `zen/v1/sig/password-session` | password-derived key | sign-in challenge (formats.md §7.5, §10) |

Signing with a purpose not in this table is an error.

## Test-only

`zen/test/det-rng` is the deterministic RNG used for test vectors. It is never used by production code.
