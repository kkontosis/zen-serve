//! Keyspace builders (spec/keyspace.md §3).

use zen_store::key_after;
use zen_store::tuple::{Key, strinc};

/// Start of the keys strictly after `prefix ‖ vs(offset)` within `prefix`'s
/// range, and the end of that range.
pub fn after_offset(prefix: Key, offset: &[u8; 12]) -> (Vec<u8>, Vec<u8>) {
    let p = prefix.clone().finish();
    let at = prefix.vs(offset).finish();
    (key_after(&at), end_of(&p))
}

/// End of the range of keys starting with `prefix`.
pub fn end_of(prefix: &[u8]) -> Vec<u8> {
    strinc(prefix).unwrap_or_else(|| vec![0xFF])
}

// ---- KV

/// Prefix of an fs's KV keys.
pub fn kv_prefix(fs: u32) -> Vec<u8> {
    Key::new().str("kv").int(fs.into()).finish()
}

/// Storage key of a stored key.
pub fn kv(fs: u32, stored_key: &[u8]) -> Vec<u8> {
    let mut k = kv_prefix(fs);
    k.extend_from_slice(stored_key);
    k
}

/// Storage range of `[begin, end)` stored keys (`end` absent = end of fs).
pub fn kv_range(fs: u32, begin: &[u8], end: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    let b = kv(fs, begin);
    let e = match end {
        Some(e) => kv(fs, e),
        None => end_of(&kv_prefix(fs)),
    };
    (b, e)
}

/// fs header.
pub fn header(fs: u32) -> Vec<u8> {
    Key::new().str("hdr").int(fs.into()).finish()
}

/// Quota counter (`"bytes"` or `"keys"`).
pub fn quota(fs: u32, what: &str) -> Vec<u8> {
    Key::new().str("q").int(fs.into()).str(what).finish()
}

// ---- log

/// `("log", fs, topic)`.
pub fn log_prefix(fs: u32, topic: &[u8]) -> Key {
    Key::new().str("log").int(fs.into()).bytes(topic)
}

/// `("lk", fs, topic, key)`.
pub fn lk_prefix(fs: u32, topic: &[u8], key: &[u8]) -> Key {
    Key::new().str("lk").int(fs.into()).bytes(topic).bytes(key)
}

/// Topic head.
pub fn topic_head(fs: u32, topic: &[u8]) -> Vec<u8> {
    Key::new().str("lh").int(fs.into()).bytes(topic).finish()
}

/// `("gl", fs)`.
pub fn gl_prefix(fs: u32) -> Key {
    Key::new().str("gl").int(fs.into())
}

/// fs-wide log head.
pub fn fs_head(fs: u32) -> Vec<u8> {
    Key::new().str("gh").int(fs.into()).finish()
}

// ---- consumer groups

/// Group definition.
pub fn group(fs: u32, group: &[u8]) -> Vec<u8> {
    Key::new().str("cg").int(fs.into()).bytes(group).finish()
}

/// `("ct", fs, topic)`: groups on a topic.
pub fn topic_groups(fs: u32, topic: &[u8]) -> Key {
    Key::new().str("ct").int(fs.into()).bytes(topic)
}

/// Committed cursor of a partition.
pub fn cursor(fs: u32, group: &[u8], part: u32) -> Vec<u8> {
    Key::new()
        .str("cc")
        .int(fs.into())
        .bytes(group)
        .int(part.into())
        .finish()
}

/// Lease of a partition.
pub fn lease(fs: u32, group: &[u8], part: u32) -> Vec<u8> {
    Key::new()
        .str("cl")
        .int(fs.into())
        .bytes(group)
        .int(part.into())
        .finish()
}

/// Per-key committed cursor.
pub fn key_cursor(fs: u32, group: &[u8], key: &[u8]) -> Vec<u8> {
    Key::new()
        .str("kc")
        .int(fs.into())
        .bytes(group)
        .bytes(key)
        .finish()
}

/// `("kr", fs, group)`: the ready list.
pub fn ready_prefix(fs: u32, group: &[u8]) -> Key {
    Key::new().str("kr").int(fs.into()).bytes(group)
}

/// Pointer to a key's ready entry.
pub fn ready_ptr(fs: u32, group: &[u8], key: &[u8]) -> Vec<u8> {
    Key::new()
        .str("kp")
        .int(fs.into())
        .bytes(group)
        .bytes(key)
        .finish()
}

/// A key's claim.
pub fn claim(fs: u32, group: &[u8], key: &[u8]) -> Vec<u8> {
    Key::new()
        .str("km")
        .int(fs.into())
        .bytes(group)
        .bytes(key)
        .finish()
}

/// Attempt counter of a partition or key.
pub fn attempts(fs: u32, group: &[u8], sub: &Sub) -> Vec<u8> {
    let k = Key::new().str("ca").int(fs.into()).bytes(group);
    match sub {
        Sub::Part(p) => k.int((*p).into()),
        Sub::Key(key) => k.bytes(key),
    }
    .finish()
}

/// `("dlq", fs, group)`.
pub fn dlq_prefix(fs: u32, group: &[u8]) -> Key {
    Key::new().str("dlq").int(fs.into()).bytes(group)
}

/// A partition or a key of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sub {
    /// Partition (lease modes).
    Part(u32),
    /// Key token (`per_key`).
    Key(Vec<u8>),
}

// ---- commits, ACL

/// Idempotency record.
pub fn commit_record(commit_id: &[u8]) -> Vec<u8> {
    Key::new().str("cid").bytes(commit_id).finish()
}

/// `("cix")`: idempotency expiry index.
pub fn commit_index() -> Key {
    Key::new().str("cix")
}

/// ACL version.
pub fn acl(version: u64) -> Vec<u8> {
    Key::new().str("acl").int(version as i64).finish()
}

/// ACL head pointer.
pub fn acl_head() -> Vec<u8> {
    Key::new().str("acl_head").finish()
}

// ---- sessions, challenges, ephemeral ring, metadata

/// A session, by the hash of its bearer token.
pub fn session(token_hash: &[u8; 32]) -> Vec<u8> {
    Key::new().str("sess").bytes(token_hash).finish()
}

/// All sessions.
pub fn session_prefix() -> Vec<u8> {
    Key::new().str("sess").finish()
}

/// A consumed challenge.
pub fn challenge(c: &[u8]) -> Vec<u8> {
    Key::new().str("chal").bytes(c).finish()
}

/// All consumed challenges.
pub fn challenge_prefix() -> Vec<u8> {
    Key::new().str("chal").finish()
}

/// `("eph", fs)`: the ephemeral ring.
pub fn eph_prefix(fs: u32) -> Key {
    Key::new().str("eph").int(fs.into())
}

/// Ephemeral ring head, watched by tailers.
pub fn eph_head(fs: u32) -> Vec<u8> {
    Key::new().str("eh").int(fs.into()).finish()
}

/// Server metadata.
pub fn meta(name: &str) -> Vec<u8> {
    Key::new().str("meta").str(name).finish()
}

// ---- filesystem trees (keyspace.md §3.6)

/// `("tr", fs)`: tree headers of an fs.
pub fn trees(fs: u32) -> Key {
    Key::new().str("tr").int(fs.into())
}

/// Tree header.
pub fn tree_header(fs: u32, tree: &[u8]) -> Vec<u8> {
    trees(fs).bytes(tree).finish()
}

/// Tree head versionstamp, watched by change long-polls.
pub fn tree_head(fs: u32, tree: &[u8]) -> Vec<u8> {
    Key::new().str("th").int(fs.into()).bytes(tree).finish()
}

/// Node record.
pub fn node(fs: u32, tree: &[u8], node: &[u8]) -> Vec<u8> {
    Key::new()
        .str("tn")
        .int(fs.into())
        .bytes(tree)
        .bytes(node)
        .finish()
}

/// `("tc", fs, tree, parent)`: children of a node.
pub fn children(fs: u32, tree: &[u8], parent: &[u8]) -> Key {
    Key::new()
        .str("tc")
        .int(fs.into())
        .bytes(tree)
        .bytes(parent)
}

/// `("tm", fs, tree)`: the move log.
pub fn move_log(fs: u32, tree: &[u8]) -> Key {
    Key::new().str("tm").int(fs.into()).bytes(tree)
}

/// `("tv", fs, tree)`: the change index.
pub fn changes(fs: u32, tree: &[u8]) -> Key {
    Key::new().str("tv").int(fs.into()).bytes(tree)
}

/// `("tx", fs, tree)`: tombstones.
pub fn tombstones(fs: u32, tree: &[u8]) -> Key {
    Key::new().str("tx").int(fs.into()).bytes(tree)
}

/// `("tf", fs, tree, node)`: content versions of a node.
pub fn versions(fs: u32, tree: &[u8], node: &[u8]) -> Key {
    Key::new().str("tf").int(fs.into()).bytes(tree).bytes(node)
}

/// Sealed chunk.
pub fn chunk(fs: u32, id: &[u8]) -> Vec<u8> {
    Key::new().str("ck").int(fs.into()).bytes(id).finish()
}

/// Chunk reference count.
pub fn chunk_refs(fs: u32, id: &[u8]) -> Vec<u8> {
    Key::new().str("cr").int(fs.into()).bytes(id).finish()
}

/// `("cz", fs)`: chunk GC candidates.
pub fn chunk_gc(fs: u32) -> Key {
    Key::new().str("cz").int(fs.into())
}
