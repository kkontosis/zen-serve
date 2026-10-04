//! zen-core: the client-side cryptographic core of zen-serve.
//!
//! Everything here runs on the client (native or WASM). The server never sees
//! keys or plaintext. Byte formats are specified normatively in `spec/`; this
//! crate must match `spec/formats.md` and the vectors in `spec/test-vectors/`.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod encoding;
pub mod error;
pub mod fs;
pub mod kdf;
pub mod keys;
pub mod keyslot;
pub mod labels;
pub mod rng;
pub mod seal;
pub mod sig;
pub mod suite;
pub mod token;

#[cfg(feature = "test-utils")]
pub mod vectors;

pub use error::{Error, Result};
pub use suite::{FORMAT_VERSION, Suite};
