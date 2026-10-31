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
/// KDF: AEAD key for the chunks of a version-2 file, from its file key.
pub const FS_FILE: &str = "zen/v1/fs-file";
/// KDF: key of the chunker's gear table for files under the fs keys (formats.md §11.7).
pub const FS_CDC: &str = "zen/v1/fs-cdc";
/// KDF: key of the chunker's gear table for a file with a key of its own (formats.md §11.7).
pub const FS_CDC_FILE: &str = "zen/v1/fs-cdc-file";
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
/// KDF: a relayed service's key `K_svc`, from its topic data key (spec/formats.md §13).
pub const SVC_AEAD: &str = "zen/v1/svc-aead";
/// KDF: the key sealing a connection's input chunks (spec/formats.md §13.3).
pub const SVC_INBOX: &str = "zen/v1/svc-inbox";
/// Hash: fingerprint of public key material.
pub const FINGERPRINT: &str = "zen/v1/fingerprint";
/// Hash: chain hash of a signed ACL document.
pub const ACL_CHAIN: &str = "zen/v1/acl-chain";
/// Hash: `expect_ranges` hash of a KV range.
pub const RANGE_HASH: &str = "zen/v1/range-hash";
/// Hash: per-tree chain over filesystem operations.
pub const TREE_OP_CHAIN: &str = "zen/v1/tree-op-chain";
/// Hash: digest of a chunk-list page of a paged file version.
pub const FS_PAGE: &str = "zen/v1/fs-page";
/// A passkey's credential-store id from its WebAuthn credential id.
pub const PASSKEY_ID: &str = "zen/v1/passkey-id";
/// Hash: the secret of an OPAQUE export-key keyslot from the export key.
pub const OPAQUE_KEYSLOT: &str = "zen/v1/opaque-keyslot";
/// Hash: digest of a zen-db value split into parts.
pub const DB_PARTS_DIGEST: &str = "zen/v1/db-parts-digest";
/// Hash: `key_hash` of an output record's key (spec/formats.md §13.1).
pub const SVC_REVEAL: &str = "zen/v1/svc-reveal";
/// Hash: `HC(body)` of a device checkpoint (spec/formats.md §14.1).
pub const CHECKPOINT_HASH: &str = "zen/v1/checkpoint-hash";
/// Hash: `HH` of a signed fs header (spec/formats.md §12.1).
pub const FS_HEADER_CHAIN: &str = "zen/v1/fs-header-chain";
/// Hash: `HE(body)` of a version-2 event body (spec/formats.md §5.1).
pub const EVENT_CHAIN: &str = "zen/v1/event-chain";
/// Hash: leaves and inner nodes of a filesystem tree state (spec/fs.md §9).
pub const TREE_STATE: &str = "zen/v1/tree-state";
/// Hash: `VH` of a value in a zen-db state tree (spec/zendb.md §14.2).
pub const DB_STATE_VALUE: &str = "zen/v1/db-state-value";
/// Hash: `NH` of a zen-db NsRoot (spec/zendb.md §14.4).
pub const DB_NS_ROOT: &str = "zen/v1/db-ns-root";
/// Hash: `leaf(i)` of a stored block of a volume (spec/blk.md §2).
pub const BLK_LEAF: &str = "zen/v1/blk-leaf";
/// Hash: `hash(l, j)` of a volume's tree node (spec/blk.md §2).
pub const BLK_NODE: &str = "zen/v1/blk-node";
/// Hash: `RH` of a volume's root record (spec/blk.md §3).
pub const BLK_ROOT: &str = "zen/v1/blk-root";

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
/// AAD domain: a local record (kind 8).
pub const AAD_LOCAL: &str = "zen/v1/aad/local";
/// AAD domain: svc state (kind 9).
pub const AAD_SVC_STATE: &str = "zen/v1/aad/svc-state";
/// AAD domain: svc output-key escrow (kind 10).
pub const AAD_SVC_KEY: &str = "zen/v1/aad/svc-key";
/// AAD domain: body of an output record (no header).
pub const AAD_SVC_OUT: &str = "zen/v1/aad/svc-out";
/// AAD domain: sealed input chunk (no header).
pub const AAD_SVC_IN: &str = "zen/v1/aad/svc-in";
/// AAD domain: device checkpoint (kind 11).
pub const AAD_CHECKPOINT: &str = "zen/v1/aad/checkpoint";

/// Signature domain prefix, prepended to every signed message.
pub const SIG_DOMAIN: &str = "zen/v1/sig";
/// Signature purpose: device certificate issued by a user identity.
pub const SIG_DEVICE_CERT: &str = "zen/v1/sig/device-cert";
/// Signature purpose: reserved (roots ride in device checkpoints).
pub const SIG_COMMIT: &str = "zen/v1/sig/commit";
/// Signature purpose: device checkpoint (spec/formats.md §14.2).
pub const SIG_CHECKPOINT: &str = "zen/v1/sig/checkpoint";
/// Signature purpose: signed ACL document.
pub const SIG_ACL: &str = "zen/v1/sig/acl";
/// Signature purpose: membership-log entry.
pub const SIG_MEMBERSHIP: &str = "zen/v1/sig/membership";
/// Signature purpose: reserved (events are chained, spec/formats.md §5.1).
pub const SIG_EVENT: &str = "zen/v1/sig/event";
/// Signature purpose: device sign-in challenge.
pub const SIG_SESSION: &str = "zen/v1/sig/session";
/// Signature purpose: reserved (tree states ride in device checkpoints).
pub const SIG_TREE_CHECKPOINT: &str = "zen/v1/sig/tree-checkpoint";
/// Signature purpose: sign-in challenge signed by a password-derived key.
pub const SIG_PASSWORD_SESSION: &str = "zen/v1/sig/password-session";
/// Signature purpose: reserved (zen-db roots ride in device checkpoints).
pub const SIG_DB_ROOT: &str = "zen/v1/sig/db-root";
/// Signature purpose: signed fs header (spec/formats.md §12.1).
pub const SIG_FS_HEADER: &str = "zen/v1/sig/fs-header";
/// Signature purpose: a signed app bundle (reserved, milestone 11).
pub const SIG_APP_BUNDLE: &str = "zen/v1/sig/app-bundle";

/// Every label, for registry checks.
pub const ALL: &[&str] = &[
    KV_DATA,
    EPOCH_CHAIN,
    KV_NAME,
    TOPIC_NAME,
    TOPIC_DATA,
    FS_DATA,
    FS_FILE,
    FS_CDC,
    FS_CDC_FILE,
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
    SVC_AEAD,
    SVC_INBOX,
    FINGERPRINT,
    ACL_CHAIN,
    RANGE_HASH,
    TREE_OP_CHAIN,
    FS_PAGE,
    PASSKEY_ID,
    OPAQUE_KEYSLOT,
    DB_PARTS_DIGEST,
    SVC_REVEAL,
    CHECKPOINT_HASH,
    FS_HEADER_CHAIN,
    EVENT_CHAIN,
    TREE_STATE,
    DB_STATE_VALUE,
    DB_NS_ROOT,
    BLK_LEAF,
    BLK_NODE,
    BLK_ROOT,
    OPAQUE_CONTEXT,
    AAD_KV,
    AAD_EVENT,
    AAD_EPOCH_CHAIN,
    AAD_KEYSLOT,
    AAD_FS_META,
    AAD_FS_MANIFEST,
    AAD_FS_CHUNK,
    AAD_CRDT_VALUE,
    AAD_LOCAL,
    AAD_SVC_STATE,
    AAD_SVC_KEY,
    AAD_SVC_OUT,
    AAD_SVC_IN,
    AAD_CHECKPOINT,
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
    SIG_FS_HEADER,
    SIG_APP_BUNDLE,
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
    SIG_FS_HEADER,
    SIG_APP_BUNDLE,
];
