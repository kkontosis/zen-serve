//! Error type. Deliberately coarse: decryption failures never say why.

use core::fmt;

/// All zen-core errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Authentication failed: wrong key, wrong context, or tampered data.
    Decrypt,
    /// Input bytes do not match the expected format, version or suite.
    Format,
    /// A parameter is out of the allowed range (e.g. Argon2id below the floor).
    Param,
    /// A signature (either half of a hybrid signature) failed to verify.
    Signature,
    /// The operating system RNG failed.
    Rng,
}

/// Result alias.
pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Decrypt => "decryption failed",
            Error::Format => "invalid format",
            Error::Param => "invalid parameter",
            Error::Signature => "signature verification failed",
            Error::Rng => "random number generator failure",
        })
    }
}

impl std::error::Error for Error {}
