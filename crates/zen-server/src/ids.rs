//! Small helpers for ids, offsets and validation.

use crate::error::{ApiResult, bad_request};
use zen_proto::{KEY_TOKEN_LEN, OFFSET_LEN, ZERO_OFFSET};
use zen_store::{STAMP_LEN, Stamp};

/// Max topic id length (16 elements).
pub const MAX_TOPIC_LEN: usize = 16 * 16;

/// A 12-byte offset.
pub type Offset = [u8; OFFSET_LEN];

/// `stamp ‖ u16(index)`.
pub fn offset(stamp: &Stamp, index: u16) -> Offset {
    let mut o = [0u8; OFFSET_LEN];
    o[..STAMP_LEN].copy_from_slice(stamp);
    o[STAMP_LEN..].copy_from_slice(&index.to_be_bytes());
    o
}

/// Parse an optional offset (absent = zero).
pub fn parse_offset(b: Option<&[u8]>) -> ApiResult<Offset> {
    match b {
        None => Ok(ZERO_OFFSET),
        Some(b) => b
            .try_into()
            .map_err(|_| bad_request("offset must be 12 bytes")),
    }
}

/// Validate a topic id.
pub fn check_topic(t: &[u8]) -> ApiResult<()> {
    if t.is_empty() || !t.len().is_multiple_of(16) || t.len() > MAX_TOPIC_LEN {
        return Err(bad_request("topic id must be 16·n bytes, 1 ≤ n ≤ 16"));
    }
    Ok(())
}

/// Validate a topic prefix (may be empty).
pub fn check_prefix(t: &[u8]) -> ApiResult<()> {
    if !t.len().is_multiple_of(16) || t.len() > MAX_TOPIC_LEN {
        return Err(bad_request("topic prefix must be 16·n bytes, n ≤ 16"));
    }
    Ok(())
}

/// Validate an optional key token.
pub fn check_key_token(k: Option<&[u8]>) -> ApiResult<()> {
    match k {
        Some(k) if k.len() != KEY_TOKEN_LEN => Err(bad_request("key_token must be 16 bytes")),
        _ => Ok(()),
    }
}

/// Validate a group name.
pub fn check_group(g: &[u8]) -> ApiResult<()> {
    if g.is_empty() || g.len() > 64 {
        return Err(bad_request("group name must be 1..=64 bytes"));
    }
    Ok(())
}

/// 32 bytes from the OS RNG.
pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS RNG");
    b
}

/// Seconds since the unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Event entry: `u8 flags ‖ [key_token] ‖ envelope` (keyspace.md §3.2).
pub fn encode_entry(key_token: Option<&[u8]>, envelope: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(1 + KEY_TOKEN_LEN + envelope.len());
    match key_token {
        Some(k) => {
            e.push(1);
            e.extend_from_slice(k);
        }
        None => e.push(0),
    }
    e.extend_from_slice(envelope);
    e
}

/// Decode an event entry into `(key_token, envelope)`.
pub fn decode_entry(e: &[u8]) -> (Option<Vec<u8>>, Vec<u8>) {
    match e.first() {
        Some(1) if e.len() > KEY_TOKEN_LEN => (
            Some(e[1..1 + KEY_TOKEN_LEN].to_vec()),
            e[1 + KEY_TOKEN_LEN..].to_vec(),
        ),
        Some(_) => (None, e[1..].to_vec()),
        None => (None, Vec::new()),
    }
}
