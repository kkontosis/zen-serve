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
/// KDF: step of a naming chain (KV elements and topic segments).
pub const NAME_CHAIN: &str = "zen/v1/name-chain";
/// KDF: step of the topic data-key chain.
pub const TOPIC_DATA_CHAIN: &str = "zen/v1/topic-data-chain";
/// KDF: per-topic key for event key tokens.
pub const EVENT_KEY: &str = "zen/v1/event-key";
/// KDF: per-topic AEAD key for events.
pub const EVENT_AEAD: &str = "zen/v1/event-aead";
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
/// Hash: fingerprint of public key material.
pub const FINGERPRINT: &str = "zen/v1/fingerprint";
/// Hash: chain hash of a signed ACL document.
pub const ACL_CHAIN: &str = "zen/v1/acl-chain";
/// Hash: `expect_ranges` hash of a KV range.
pub const RANGE_HASH: &str = "zen/v1/range-hash";

/// AAD domain: sealed KV value.
pub const AAD_KV: &str = "zen/v1/aad/kv";
/// AAD domain: sealed event.
pub const AAD_EVENT: &str = "zen/v1/aad/event";
/// AAD domain: epoch-chain record.
pub const AAD_EPOCH_CHAIN: &str = "zen/v1/aad/epoch-chain";
/// AAD domain: keyslot.
pub const AAD_KEYSLOT: &str = "zen/v1/aad/keyslot";

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

/// Every label, for registry checks.
pub const ALL: &[&str] = &[
    KV_DATA,
    EPOCH_CHAIN,
    KV_NAME,
    TOPIC_NAME,
    TOPIC_DATA,
    NAME_CHAIN,
    TOPIC_DATA_CHAIN,
    EVENT_KEY,
    EVENT_AEAD,
    KEYSLOT_KEK,
    SIG_ED25519,
    SIG_ML_DSA_65,
    DEVICE_SIG,
    DEVICE_KEM,
    FINGERPRINT,
    ACL_CHAIN,
    RANGE_HASH,
    AAD_KV,
    AAD_EVENT,
    AAD_EPOCH_CHAIN,
    AAD_KEYSLOT,
    SIG_DOMAIN,
    SIG_DEVICE_CERT,
    SIG_COMMIT,
    SIG_CHECKPOINT,
    SIG_ACL,
    SIG_MEMBERSHIP,
    SIG_EVENT,
    SIG_SESSION,
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
];
