//! zen-store: the storage layer of zen-serve.
//!
//! [`Storage`] and [`Txn`] mirror FoundationDB's transaction model, so the
//! FoundationDB backend ([`fdb`], feature `fdb`) is a thin adapter:
//! * strictly serializable optimistic transactions with read versions
//! * read conflict ranges, snapshot reads, read-your-writes
//! * versionstamped keys and values
//! * atomic add
//! * watches
//!
//! [`embedded::Embedded`] is the single-node backend on `redb`;
//! [`prefixed::Prefixed`] places any backend under a key prefix.
#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "testing")]
pub mod conformance;
pub mod embedded;
#[cfg(feature = "fdb")]
pub mod fdb;
pub mod prefixed;
pub mod tuple;

/// A commit version: about 1,000,000 per second, strictly increasing.
pub type Version = u64;

/// Length of a commit versionstamp.
pub const STAMP_LEN: usize = 10;

/// A 10-byte commit versionstamp: `u64(version) ‖ u16(batch_order)`.
pub type Stamp = [u8; STAMP_LEN];

/// Versions per second of the version clock.
pub const VERSIONS_PER_SEC: u64 = 1_000_000;

/// The versionstamp of a commit version (batch order 0).
pub fn stamp_of(version: Version) -> Stamp {
    let mut s = [0u8; STAMP_LEN];
    s[..8].copy_from_slice(&version.to_be_bytes());
    s
}

/// The commit version inside a versionstamp.
pub fn version_of(stamp: &[u8]) -> Version {
    u64::from_be_bytes(stamp[..8].try_into().expect("stamp has 8+ bytes"))
}

/// Storage errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A read conflict range was written after the read version. Retryable.
    Conflict,
    /// The read version is outside the MVCC window (or unknown). Retry with
    /// a fresh read version.
    TooOld,
    /// The key has a pending versionstamped or atomic mutation in this
    /// transaction and cannot be read back.
    Unreadable,
    /// The commit may or may not have been applied (FoundationDB
    /// `commit_unknown_result`). Retry only if the transaction is idempotent.
    CommitUnknown,
    /// Backend I/O or corruption.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Conflict => write!(f, "transaction conflict"),
            Error::TooOld => write!(f, "read version too old"),
            Error::Unreadable => write!(f, "key has an unreadable pending mutation"),
            Error::CommitUnknown => write!(f, "commit result unknown"),
            Error::Io(e) => write!(f, "storage I/O: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Storage result.
pub type Result<T> = std::result::Result<T, Error>;

/// A key-value pair.
pub type KeyValue = (Vec<u8>, Vec<u8>);

/// A watch: resolves some time after the watched key is next written. It may
/// fire spuriously, so the watcher always re-reads.
pub struct Watch(Pin<Box<dyn Future<Output = ()> + Send>>);

impl Watch {
    /// Wrap a future.
    pub fn new(f: impl Future<Output = ()> + Send + 'static) -> Self {
        Watch(Box::pin(f))
    }
}

impl Future for Watch {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

/// A transactional ordered key-value store with FoundationDB semantics.
#[async_trait::async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Begin a transaction. With `read_version`, reads see the snapshot at
    /// that version, and conflicts are checked against it. It must be a
    /// version handed out earlier by `begin`, and still inside the MVCC
    /// window, or the result is [`Error::TooOld`].
    async fn begin(&self, read_version: Option<Version>) -> Result<Box<dyn Txn>>;

    /// Watch `key`. The watch is armed before this returns: any write to
    /// `key` committed after a read version obtained later fires it.
    async fn watch(&self, key: &[u8]) -> Result<Watch>;

    /// The current reading of the version clock (for expiry decisions). It
    /// may lag the newest read version slightly.
    async fn now_version(&self) -> Result<Version>;
}

/// One optimistic transaction.
#[async_trait::async_trait]
pub trait Txn: Send {
    /// The snapshot version this transaction reads at.
    fn read_version(&self) -> Version;

    /// Read a key, adding it to the read conflict set.
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Read a key without a conflict range.
    async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Read `[begin, end)`, at most `limit` pairs, adding the part actually
    /// covered to the read conflict set.
    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>>;

    /// [`Txn::get_range`] without a conflict range.
    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>>;

    /// Add `[begin, end)` to the read conflict set.
    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]);

    /// Write a key.
    fn set(&mut self, key: &[u8], value: &[u8]);

    /// Delete a key.
    fn clear(&mut self, key: &[u8]);

    /// Delete every key in `[begin, end)`.
    fn clear_range(&mut self, begin: &[u8], end: &[u8]);

    /// Write `prefix ‖ stamp ‖ suffix → value`, where `stamp` is the
    /// commit's versionstamp.
    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]);

    /// Write `key → prefix ‖ stamp ‖ suffix`.
    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]);

    /// Add `delta` to the little-endian `i64` at `key` (missing = 0), without
    /// a read conflict.
    fn atomic_add(&mut self, key: &[u8], delta: i64);

    /// Commit. Returns the commit's versionstamp (the one versionstamped
    /// mutations received), or `stamp_of(read_version)` for a transaction
    /// without mutations.
    async fn commit(self: Box<Self>) -> Result<Stamp>;
}

/// `[key, key ‖ 0x00)`: the range holding exactly `key`.
pub fn key_after(key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(key.len() + 1);
    k.extend_from_slice(key);
    k.push(0);
    k
}
