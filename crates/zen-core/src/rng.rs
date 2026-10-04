//! Injectable randomness. All functions that need randomness take `&mut dyn Rng`,
//! so test vectors can be generated deterministically.

use crate::{Error, Result};

/// Source of cryptographically secure random bytes.
pub trait Rng {
    /// Fill `buf` with random bytes.
    fn fill(&mut self, buf: &mut [u8]) -> Result<()>;
}

/// The operating system RNG (`Crypto.getRandomValues` on wasm32).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRng;

impl Rng for OsRng {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        getrandom::fill(buf).map_err(|_| Error::Rng)
    }
}

pub(crate) fn array<const N: usize>(rng: &mut dyn Rng) -> Result<[u8; N]> {
    let mut out = [0u8; N];
    rng.fill(&mut out)?;
    Ok(out)
}

/// Deterministic RNG for test vectors: the BLAKE3 XOF of a seed.
/// **Never** use outside tests.
#[cfg(feature = "test-utils")]
pub struct DetRng(blake3::OutputReader);

#[cfg(feature = "test-utils")]
impl DetRng {
    /// Create a deterministic RNG from a seed string.
    pub fn new(seed: &str) -> Self {
        let mut h = blake3::Hasher::new_derive_key("zen/test/det-rng");
        h.update(seed.as_bytes());
        DetRng(h.finalize_xof())
    }
}

#[cfg(feature = "test-utils")]
impl Rng for DetRng {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        self.0.fill(buf);
        Ok(())
    }
}
