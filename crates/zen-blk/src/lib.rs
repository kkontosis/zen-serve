//! zen-blk: the block engine (spec/blk.md).
//!
//! A [`Volume`] is a fixed-size array of 4,096-byte blocks inside an fs,
//! stored as sealed KV values through `zen-client`:
//! * every read is checked against a Merkle tree over the blocks and a
//!   hash-chained root record, whose `seq` rides in device checkpoints
//!   (formats.md §14, entry type 4), so a rolled-back or forked volume is
//!   caught;
//! * one writer at a time holds a fenced lease (api.md §16), so a writer
//!   that lost it writes nothing;
//! * writes are buffered and committed in batches, each commit moving the
//!   volume from one verified root to the next.
//!
//! The frontends (`zen-ublk`) only call this engine.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod pin;
mod tree;
mod volume;

pub use volume::{Volume, VolumeOptions};
pub use zen_core::blk::BLOCK_SIZE;
