//! The signed ACL document (spec/formats.md §9).
//!
//! Struct fields are declared in deterministic-CBOR key order (shorter keys
//! first, then bytewise), so [`crate::to_cbor`] produces deterministic CBOR.

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// fs rights.
pub const FS_RIGHTS: &[&str] = &["read", "write"];
/// Topic rights.
pub const TOPIC_RIGHTS: &[&str] = &["read", "append", "consume"];

/// The ACL document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AclDoc {
    /// Admin user fingerprints.
    pub admins: Vec<ByteBuf>,
    /// Grants.
    pub grants: Vec<Grant>,
    /// Per-fs quotas.
    pub limits: Vec<FsLimit>,
    /// Members.
    pub members: Vec<Member>,
    /// Version: 1, 2, 3, …
    pub version: u64,
    /// Hash of the previous doc (zeros for version 1).
    #[serde(with = "serde_bytes")]
    pub prev_hash: Vec<u8>,
}

/// A member: a user identity and its device certificates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    /// Device certificates (formats.md §7.4).
    pub devices: Vec<ByteBuf>,
    /// Encoded public identity (formats.md §7.2).
    #[serde(with = "serde_bytes")]
    pub identity: Vec<u8>,
}

/// A grant. Without `topic` it is an fs grant; with `topic` it covers every
/// topic id with that prefix.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    /// fs_id.
    pub fs: u32,
    /// Topic-id prefix.
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    pub topic: Option<Vec<u8>>,
    /// Rights.
    pub rights: Vec<String>,
    /// User fingerprint.
    #[serde(with = "serde_bytes")]
    pub subject: Vec<u8>,
}

/// Per-fs quota.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FsLimit {
    /// fs_id.
    pub fs: u32,
    /// Max KV keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_keys: Option<u64>,
    /// Max bytes (KV keys + values + event envelopes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// A signed ACL: `{doc, sig, signer}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedAcl {
    /// The encoded [`AclDoc`], verbatim.
    #[serde(with = "serde_bytes")]
    pub doc: Vec<u8>,
    /// Hybrid signature, purpose `zen/v1/sig/acl`.
    #[serde(with = "serde_bytes")]
    pub sig: Vec<u8>,
    /// Signer's user fingerprint.
    #[serde(with = "serde_bytes")]
    pub signer: Vec<u8>,
}
