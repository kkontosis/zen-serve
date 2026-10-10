//! Domain-separation labels. Every label here is listed in spec/labels.md and
//! vice versa; `tests/labels.rs` enforces the correspondence.

/// KDF: per-(fs, epoch) AEAD key for KV values.
pub const KV_DATA: &str = "zen/v1/kv-data";
/// KDF: per-(fs, epoch) key that seals the previous epoch key (backward chain).
pub const EPOCH_CHAIN: &str = "zen/v1/epoch-chain";
/// KDF: root naming key for KV key tokens of an fs.
pub const KV_NAME: &str = "zen/v1/kv-name";
/// KDF: root naming key for topic ids of an fs.
pub const TOPIC_NAME: &str = "zen/v1/topic-name";
/// KDF: per-(fs, epoch) root of the topic data-key tree.
pub const TOPIC_DATA: &str = "zen/v1/topic-data";
/// KDF: per-(fs, epoch) AEAD key for filesystem meta, manifests and chunks.
pub const FS_DATA: &str = "zen/v1/fs-data";
/// KDF: step of a naming chain (KV elements and topic segments).
pub const NAME_CHAIN: &str = "zen/v1/name-chain";
/// KDF: step of the topic data-key chain.
pub const TOPIC_DATA_CHAIN: &str = "zen/v1/topic-data-chain";
/// KDF: per-topic key for event key tokens.
pub const EVENT_KEY: &str = "zen/v1/event-key";
/// KDF: per-topic AEAD key for events.
pub const EVENT_AEAD: &str = "zen/v1/event-aead";
/// KDF: per-topic PRF key for consumer-group ids of the zen-db broker.
pub const BROKER_GROUP: &str = "zen/v1/broker-group";
/// KDF: key-encryption key of a keyslot.
pub const KEYSLOT_KEK: &str = "zen/v1/keyslot-kek";
/// KDF: Ed25519 seed from a hybrid signing seed.
pub const SIG_ED25519: &str = "zen/v1/sig-ed25519";
/// KDF: ML-DSA-65 seed from a hybrid signing seed.
pub const SIG_ML_DSA_65: &str = "zen/v1/sig-ml-dsa-65";
/// KDF: device signing seed from a device secret.
pub const DEVICE_SIG: &str = "zen/v1/device-sig";
/// KDF: device X-Wing seed from a device secret.
pub const DEVICE_KEM: &str = "zen/v1/device-kem";
/// KDF: hybrid identity seed of a password-derived key from its Argon2id output.
pub const PASSWORD_SIG: &str = "zen/v1/password-sig";
/// KDF: root secret of a zen-db database (spec/zendb.md §2.2).
pub const DB: &str = "zen/v1/db";
/// KDF: PRF key for private-index node boundaries and shards.
pub const DB_BOUNDARY: &str = "zen/v1/db-boundary";
/// KDF: PRF key for private-index node ids.
pub const DB_NODE_ID: &str = "zen/v1/db-node-id";
/// KDF: PRF key for a zen-db CRDT table's field and element tokens.
pub const DB_CRDT: &str = "zen/v1/db-crdt";
/// Hash: fingerprint of public key material.
pub const FINGERPRINT: &str = "zen/v1/fingerprint";
/// Hash: chain hash of a signed ACL document.
pub const ACL_CHAIN: &str = "zen/v1/acl-chain";
/// Hash: `expect_ranges` hash of a KV range.
pub const RANGE_HASH: &str = "zen/v1/range-hash";
/// Hash: per-tree chain over filesystem operations.
pub const TREE_OP_CHAIN: &str = "zen/v1/tree-op-chain";
/// A passkey's credential-store id from its WebAuthn credential id.
pub const PASSKEY_ID: &str = "zen/v1/passkey-id";
/// Hash: the secret of an OPAQUE export-key keyslot from the export key.
pub const OPAQUE_KEYSLOT: &str = "zen/v1/opaque-keyslot";
/// Hash: digest of a zen-db value split into parts.
pub const DB_PARTS_DIGEST: &str = "zen/v1/db-parts-digest";

/// OPAQUE: prefix of the AKE context, `label ‖ 0x00 ‖ origin` (sign-in
/// method 3).
pub const OPAQUE_CONTEXT: &str = "zen/v1/opaque";

/// AAD domain: sealed KV value.
pub const AAD_KV: &str = "zen/v1/aad/kv";
/// AAD domain: sealed event.
pub const AAD_EVENT: &str = "zen/v1/aad/event";
/// AAD domain: epoch-chain record.
pub const AAD_EPOCH_CHAIN: &str = "zen/v1/aad/epoch-chain";
/// AAD domain: keyslot.
pub const AAD_KEYSLOT: &str = "zen/v1/aad/keyslot";
/// AAD domain: filesystem node meta.
pub const AAD_FS_META: &str = "zen/v1/aad/fs-meta";
/// AAD domain: filesystem manifest.
pub const AAD_FS_MANIFEST: &str = "zen/v1/aad/fs-manifest";
/// AAD domain: filesystem chunk.
pub const AAD_FS_CHUNK: &str = "zen/v1/aad/fs-chunk";
/// AAD domain: zen-db CRDT value (kind 7).
pub const AAD_CRDT_VALUE: &str = "zen/v1/aad/crdt-value";

/// Signature domain prefix, prepended to every signed message.
pub const SIG_DOMAIN: &str = "zen/v1/sig";
/// Signature purpose: device certificate issued by a user identity.
pub const SIG_DEVICE_CERT: &str = "zen/v1/sig/device-cert";
/// Signature purpose: commit record / signed root.
pub const SIG_COMMIT: &str = "zen/v1/sig/commit";
/// Signature purpose: per-device event checkpoint.
pub const SIG_CHECKPOINT: &str = "zen/v1/sig/checkpoint";
/// Signature purpose: signed ACL document.
pub const SIG_ACL: &str = "zen/v1/sig/acl";
/// Signature purpose: membership-log entry.
pub const SIG_MEMBERSHIP: &str = "zen/v1/sig/membership";
/// Signature purpose: individual event (when `sig_every = 1` uses Ed25519 only).
pub const SIG_EVENT: &str = "zen/v1/sig/event";
/// Signature purpose: device sign-in challenge.
pub const SIG_SESSION: &str = "zen/v1/sig/session";
/// Signature purpose: filesystem tree checkpoint.
pub const SIG_TREE_CHECKPOINT: &str = "zen/v1/sig/tree-checkpoint";
/// Signature purpose: sign-in challenge signed by a password-derived key.
pub const SIG_PASSWORD_SESSION: &str = "zen/v1/sig/password-session";
/// Signature purpose: zen-db signed root (authenticated tier, milestone 6).
pub const SIG_DB_ROOT: &str = "zen/v1/sig/db-root";

/// Every label, for registry checks.
pub const ALL: &[&str] = &[
    KV_DATA,
    EPOCH_CHAIN,
    KV_NAME,
    TOPIC_NAME,
    TOPIC_DATA,
    FS_DATA,
    NAME_CHAIN,
    TOPIC_DATA_CHAIN,
    EVENT_KEY,
    EVENT_AEAD,
    BROKER_GROUP,
    KEYSLOT_KEK,
    SIG_ED25519,
    SIG_ML_DSA_65,
    DEVICE_SIG,
    DEVICE_KEM,
    PASSWORD_SIG,
    DB,
    DB_BOUNDARY,
    DB_NODE_ID,
    DB_CRDT,
    FINGERPRINT,
    ACL_CHAIN,
    RANGE_HASH,
    TREE_OP_CHAIN,
    PASSKEY_ID,
    OPAQUE_KEYSLOT,
    DB_PARTS_DIGEST,
    OPAQUE_CONTEXT,
    AAD_KV,
    AAD_EVENT,
    AAD_EPOCH_CHAIN,
    AAD_KEYSLOT,
    AAD_FS_META,
    AAD_FS_MANIFEST,
    AAD_FS_CHUNK,
    AAD_CRDT_VALUE,
    SIG_DOMAIN,
    SIG_DEVICE_CERT,
    SIG_COMMIT,
    SIG_CHECKPOINT,
    SIG_ACL,
    SIG_MEMBERSHIP,
    SIG_EVENT,
    SIG_SESSION,
    SIG_TREE_CHECKPOINT,
    SIG_PASSWORD_SESSION,
    SIG_DB_ROOT,
];

/// Signature purposes accepted by [`crate::sig`].
pub const SIG_PURPOSES: &[&str] = &[
    SIG_DEVICE_CERT,
    SIG_COMMIT,
    SIG_CHECKPOINT,
    SIG_ACL,
    SIG_MEMBERSHIP,
    SIG_EVENT,
    SIG_SESSION,
    SIG_TREE_CHECKPOINT,
    SIG_PASSWORD_SESSION,
    SIG_DB_ROOT,
];
