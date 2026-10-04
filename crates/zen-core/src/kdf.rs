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
