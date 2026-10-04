//! Algorithm suites (spec/suites.md).

use crate::{Error, Result};

/// Version of every binary format defined in spec/formats.md.
pub const FORMAT_VERSION: u8 = 1;

/// Algorithm suite, recorded in every stored object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Suite {
    /// XChaCha20-Poly1305, BLAKE3, Argon2id, X-Wing, Ed25519 + ML-DSA-65.
    Modern = 1,
    /// Reserved: XAES-256-GCM, SHA-384, PBKDF2, ML-KEM-1024 + P-384,
    /// ML-DSA-87 + P-384. Not implemented; rejected on input.
    Fips = 2,
}

impl Suite {
    /// Parse a suite id, accepting only implemented suites.
    pub fn from_id(id: u8) -> Result<Self> {
        match id {
            1 => Ok(Suite::Modern),
            _ => Err(Error::Format),
        }
    }
}

/// Check the leading `format_version || suite` bytes of an object.
pub(crate) fn check_prefix(bytes: &[u8]) -> Result<Suite> {
    match bytes {
        [FORMAT_VERSION, suite, ..] => Suite::from_id(*suite),
        _ => Err(Error::Format),
    }
}
