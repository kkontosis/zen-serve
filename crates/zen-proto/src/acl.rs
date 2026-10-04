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
/// Max entries of [`AclDoc::origins`].
pub const MAX_ACL_ORIGINS: usize = 16;

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
    /// Origins the admins vouch for as the server's own (spec/auth.md §5.3).
    /// Omitted when empty, so documents without it encode as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CborValue, from_cbor, to_cbor};

    /// The document as it was before `origins` existed.
    #[derive(Serialize)]
    struct OldDoc {
        admins: Vec<ByteBuf>,
        grants: Vec<Grant>,
        limits: Vec<FsLimit>,
        members: Vec<Member>,
        version: u64,
        #[serde(with = "serde_bytes")]
        prev_hash: Vec<u8>,
    }

    fn doc(origins: Vec<String>) -> AclDoc {
        AclDoc {
            admins: vec![ByteBuf::from(vec![1; 32])],
            grants: vec![Grant {
                fs: 1,
                topic: None,
                rights: vec!["read".into()],
                subject: vec![1; 32],
            }],
            limits: vec![],
            members: vec![Member {
                devices: vec![],
                identity: vec![9; 8],
            }],
            origins,
            version: 3,
            prev_hash: vec![7; 32],
        }
    }

    #[test]
    fn empty_origins_encode_as_before() {
        let d = doc(vec![]);
        let old = OldDoc {
            admins: d.admins.clone(),
            grants: d.grants.clone(),
            limits: d.limits.clone(),
            members: d.members.clone(),
            version: d.version,
            prev_hash: d.prev_hash.clone(),
        };
        assert_eq!(to_cbor(&d), to_cbor(&old));
        // And an old document decodes with no origins.
        let back: AclDoc = from_cbor(&to_cbor(&old)).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn origins_keep_deterministic_key_order() {
        let bytes = to_cbor(&doc(vec!["https://zen.example.org".into()]));
        let CborValue::Map(entries) = from_cbor::<CborValue>(&bytes).unwrap() else {
            panic!("a map");
        };
        // RFC 8949 §4.2.1: keys sorted bytewise by their encoding, which for
        // text keys is shorter first, then bytewise.
        let keys: Vec<String> = entries
            .iter()
            .map(|(k, _)| k.as_text().unwrap().to_string())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_by(|a, b| (a.len(), a.as_bytes()).cmp(&(b.len(), b.as_bytes())));
        assert_eq!(keys, sorted);
        assert_eq!(keys[4], "origins");
    }
}
