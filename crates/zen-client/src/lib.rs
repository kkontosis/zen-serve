//! zen-client: a native client of zen-serve.
//!
//! The parts of `@zen/client` that native programs need (zen-blk, zen-ublk,
//! the `zen-client` CLI), with the same rules, so a TS device and a Rust
//! device verify each other (docs/CLIENT.md §13):
//! * device sign-in (`DeviceKey`, zen-mount's key file) and API tokens
//! * the ACL walk with a pin ([`Acl`])
//! * the fs header, signed or not, and device keyslots ([`Fs`], [`UnlockedFs`])
//! * KV reads and transactions with fences ([`Transaction`]), named leases ([`Lease`])
//! * device checkpoints, floors and pins ([`Integrity`]), kept in a file
//!
//! Topics, streams, trees, relayed services and zen-db stay TS-only.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod acl;
pub mod client;
pub mod device;
pub mod error;
pub mod fs;
pub mod integrity;
pub mod kv;
pub mod lease;
mod pins;
pub mod session;
pub mod transport;

pub use acl::{Acl, AclVersion};
pub use client::{Client, ClientOptions};
pub use device::DeviceKey;
pub use error::{Alarm, AlarmKind, AlarmObject, Error, Result};
pub use fs::{Fs, StoredHeader, UnlockedFs};
pub use integrity::{Integrity, IntegrityOptions};
pub use kv::{KvEntry, Mode, RangeOptions, Transaction, TxnOptions};
pub use lease::Lease;
pub use session::Session;
pub use transport::TlsOptions;
pub use zen_core::integrity::{Entry, ExportedPins, Head, Seen};

/// Lowercase hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parse hex (either case); `None` if it isn't.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The first 4 bytes of an id in hex, for messages.
pub(crate) fn short(b: &[u8]) -> String {
    hex(&b[..b.len().min(4)])
}
