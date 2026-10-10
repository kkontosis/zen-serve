//! The canonical prolly tree of a private index (spec/zendb.md §5.4.1–5.4.2).
//!
//! The client library builds and edits trees incrementally; this is the
//! reference: the whole tree for a set of entries, used for the vectors and to
//! check the incremental code.

use super::IndexKeys;
use super::cbor::{self, Value, head_len};
use crate::{Error, Result};

/// A node closes at this many entries at the latest.
pub const MAX_NODE_ENTRIES: usize = 1024;
/// A node closes once its encoding (unsalted, unpadded) reaches this size.
pub const MAX_NODE_BYTES: usize = 60_000;

/// An entry of a node: a sort key (level 0) or a child reference (above).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A level-0 entry: the sort key.
    Key(Vec<u8>),
    /// An entry above level 0.
    Child {
        /// Key of the child's first entry.
        first_key: Vec<u8>,
        /// The child's node id.
        child: [u8; 16],
        /// Level-0 entries under the child.
        count: u64,
    },
}

impl Entry {
    /// `key(e)`: the entry at level 0, `first_key` above.
    pub fn key(&self) -> &[u8] {
        match self {
            Entry::Key(k) => k,
            Entry::Child { first_key, .. } => first_key,
        }
    }

    fn count(&self) -> u64 {
        match self {
            Entry::Key(_) => 1,
            Entry::Child { count, .. } => *count,
        }
    }

    fn value(&self) -> Value {
        match self {
            Entry::Key(k) => Value::Bytes(k.clone()),
            Entry::Child {
                first_key,
                child,
                count,
            } => Value::Array(vec![
                Value::Bytes(first_key.clone()),
                Value::Bytes(child.to_vec()),
                Value::Int(*count as i128),
            ]),
        }
    }

    /// Encoded length of the entry.
    pub fn encoded_len(&self) -> usize {
        match self {
            Entry::Key(k) => head_len(k.len() as u64) + k.len(),
            Entry::Child {
                first_key, count, ..
            } => 1 + head_len(first_key.len() as u64) + first_key.len() + 17 + head_len(*count),
        }
    }
}

/// A node: `{1: level, 2: entries, 3?: salt, 4?: pad}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Level, 0 for leaves.
    pub level: u8,
    /// Entries, in key order.
    pub entries: Vec<Entry>,
    /// Decoy salt (§5.4.6).
    pub salt: Option<[u8; 16]>,
    /// Decoy padding (§5.4.6).
    pub pad: Option<Vec<u8>>,
}

/// Encoded length of an unsalted, unpadded node with these entries.
pub fn node_len(level: u8, entries_len: usize, n: usize) -> usize {
    1 + 1 + head_len(level as u64) + 1 + head_len(n as u64) + entries_len
}

impl Node {
    /// Deterministic CBOR.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = vec![
            (Value::Int(1), Value::Int(self.level as i128)),
            (
                Value::Int(2),
                Value::Array(self.entries.iter().map(Entry::value).collect()),
            ),
        ];
        if let Some(s) = self.salt {
            m.push((Value::Int(3), Value::Bytes(s.to_vec())));
        }
        if let Some(p) = &self.pad {
            m.push((Value::Int(4), Value::Bytes(p.clone())));
        }
        cbor::encode(&Value::Map(m)).expect("valid node")
    }
}

/// `{1: root, 2: height, 3: count}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    /// The root node id.
    pub root: [u8; 16],
    /// Number of levels (the root's level + 1).
    pub height: u8,
    /// Level-0 entries in the tree.
    pub count: u64,
}

impl RootRecord {
    /// Deterministic CBOR.
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&Value::Map(vec![
            (Value::Int(1), Value::Bytes(self.root.to_vec())),
            (Value::Int(2), Value::Int(self.height as i128)),
            (Value::Int(3), Value::Int(self.count as i128)),
        ]))
        .expect("valid root")
    }
}

/// A built tree: every node with its id, bottom level first, and the root.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// `(node_id, node)`, level by level, left to right.
    pub nodes: Vec<([u8; 16], Node)>,
    /// `None` for an empty index.
    pub root: Option<RootRecord>,
}

/// Cut one level into nodes (§5.4.1, step 2).
pub fn chunk(keys: &IndexKeys, fanout: u32, level: u8, items: Vec<Entry>) -> Vec<Vec<Entry>> {
    let mut nodes = Vec::new();
    let mut cur = Vec::new();
    let mut bytes = 0;
    let last = items.len().saturating_sub(1);
    for (i, e) in items.into_iter().enumerate() {
        bytes += e.encoded_len();
        let boundary = keys.is_boundary(level, e.key(), fanout);
        cur.push(e);
        if boundary
            || cur.len() >= MAX_NODE_ENTRIES
            || node_len(level, bytes, cur.len()) >= MAX_NODE_BYTES
            || i == last
        {
            nodes.push(std::mem::take(&mut cur));
            bytes = 0;
        }
    }
    nodes
}

/// Build the canonical tree of a set of sort keys (any order, no duplicates).
pub fn build(keys: &IndexKeys, fanout: u32, mut entries: Vec<Vec<u8>>) -> Result<Tree> {
    if fanout < 2 {
        return Err(Error::Param);
    }
    entries.sort();
    if entries.windows(2).any(|w| w[0] == w[1]) {
        return Err(Error::Param);
    }
    let mut tree = Tree::default();
    if entries.is_empty() {
        return Ok(tree);
    }
    let mut items: Vec<Entry> = entries.into_iter().map(Entry::Key).collect();
    let mut level: u8 = 0;
    loop {
        let nodes = chunk(keys, fanout, level, items);
        let single = nodes.len() == 1;
        let mut up = Vec::with_capacity(nodes.len());
        for entries in nodes {
            let count = entries.iter().map(Entry::count).sum();
            let first_key = entries[0].key().to_vec();
            let node = Node {
                level,
                entries,
                salt: None,
                pad: None,
            };
            let id = keys.node_id(&node.encode());
            tree.nodes.push((id, node));
            up.push(Entry::Child {
                first_key,
                child: id,
                count,
            });
        }
        if single {
            let Entry::Child { child, count, .. } = up[0] else {
                unreachable!()
            };
            tree.root = Some(RootRecord {
                root: child,
                height: level + 1,
                count,
            });
            return Ok(tree);
        }
        // Level 255 would collide with the shard domain (0xFF, §5.4.5).
        level = level
            .checked_add(1)
            .filter(|l| *l < 0xff)
            .ok_or(Error::Param)?;
        items = up;
    }
}
