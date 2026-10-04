//! Key derivation, PRF and fingerprints (suite `modern`: BLAKE3).

use zeroize::Zeroizing;

/// A 32-byte secret key that is wiped on drop.
pub type Key32 = Zeroizing<[u8; 32]>;

/// `KDF(label, key, info) = BLAKE3.derive_key(label, key || info)`.
///
/// `key` is always exactly 32 bytes, so `key || info` is unambiguous.
pub fn kdf(label: &str, key: &[u8; 32], info: &[u8]) -> Key32 {
    let mut h = blake3::Hasher::new_derive_key(label);
    h.update(key);
    h.update(info);
    Zeroizing::new(*h.finalize().as_bytes())
}

/// `PRF16(key, data) = BLAKE3.keyed_hash(key, data)[0..16]`.
pub fn prf16(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let h = blake3::keyed_hash(key, data);
    h.as_bytes()[..16].try_into().expect("16 <= 32")
}

/// `FP(public_bytes) = BLAKE3.derive_key("zen/v1/fingerprint", public_bytes)`.
pub fn fingerprint(public_bytes: &[u8]) -> [u8; 32] {
    blake3::derive_key(crate::labels::FINGERPRINT, public_bytes)
}

/// `H(doc) = BLAKE3.derive_key("zen/v1/acl-chain", doc)`: chain hash of a
/// signed ACL document (spec/formats.md §9.2).
pub fn acl_hash(doc: &[u8]) -> [u8; 32] {
    blake3::derive_key(crate::labels::ACL_CHAIN, doc)
}

/// Incremental `expect_ranges` hash (spec/api.md §6):
/// `H("zen/v1/range-hash", lp(key_1) || version_1 || lp(key_2) || version_2 || ...)`.
pub struct RangeHasher(blake3::Hasher);

impl RangeHasher {
    /// Start an empty range.
    pub fn new() -> Self {
        RangeHasher(blake3::Hasher::new_derive_key(crate::labels::RANGE_HASH))
    }

    /// Add the next item, in key order.
    pub fn update(&mut self, stored_key: &[u8], version: &[u8; 10]) {
        let len = u32::try_from(stored_key.len()).expect("key length fits u32");
        self.0.update(&len.to_be_bytes());
        self.0.update(stored_key);
        self.0.update(version);
    }

    /// The final hash.
    pub fn finalize(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

impl Default for RangeHasher {
    fn default() -> Self {
        Self::new()
    }
}
