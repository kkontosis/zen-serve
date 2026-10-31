//! Block volumes (spec/blk.md): stored keys, leaves, tree nodes, the root
//! record, and the reference tree.
//!
//! zen-blk keeps the tree incrementally; [`tree`] is the reference the
//! vectors and its tests are checked against.

use crate::db::cbor::{self, Value};
use crate::keys::FsKeys;
use crate::labels;
use crate::token::NameChain;
use crate::{Error, Result};
use std::collections::BTreeMap;

/// Bytes per block.
pub const BLOCK_SIZE: usize = 4096;
/// Children per tree node.
pub const FANOUT: usize = 128;
/// log2 of [`FANOUT`].
const FANOUT_BITS: u32 = 7;

/// A leaf or node hash.
pub type Hash = [u8; 32];
/// A tree node: 128 child hashes.
pub type Node = [Hash; FANOUT];

/// The empty hash (an absent block, an empty node).
pub const ZERO: Hash = [0; 32];

/// The tree depth `D` of a volume: the least `d ≥ 1` with `128^d ≥ blocks`.
pub fn depth(blocks: u64) -> u8 {
    let mut d = 1u8;
    let mut cap = FANOUT as u128;
    while cap < u128::from(blocks) {
        d += 1;
        cap *= FANOUT as u128;
    }
    d
}

/// The index of the node at `level` (≥ 1) above leaf `i`.
pub fn node_of(level: u8, i: u64) -> u64 {
    i >> (FANOUT_BITS * u32::from(level))
}

/// The slot of leaf `i`'s path in its node at `level` (≥ 1).
pub fn slot_of(level: u8, i: u64) -> usize {
    ((i >> (FANOUT_BITS * u32::from(level - 1))) as usize) & (FANOUT - 1)
}

/// `leaf(i) = H("zen/v1/blk-leaf", u64(i) ‖ sealed)` over the stored bytes
/// of block `i`.
pub fn leaf(i: u64, sealed: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new_derive_key(labels::BLK_LEAF);
    h.update(&i.to_be_bytes());
    h.update(sealed);
    *h.finalize().as_bytes()
}

/// `hash(l, j) = H("zen/v1/blk-node", u8(l) ‖ u64(j) ‖ node)`, or zeros when
/// every child is zero.
pub fn node_hash(level: u8, j: u64, node: &Node) -> Hash {
    if is_empty(node) {
        return ZERO;
    }
    let mut h = blake3::Hasher::new_derive_key(labels::BLK_NODE);
    h.update(&[level]);
    h.update(&j.to_be_bytes());
    for c in node {
        h.update(c);
    }
    *h.finalize().as_bytes()
}

/// Whether every child of a node is zero.
pub fn is_empty(node: &Node) -> bool {
    node.iter().all(|c| *c == ZERO)
}

/// A node's stored plaintext, 4,096 bytes.
pub fn node_bytes(node: &Node) -> Vec<u8> {
    node.concat()
}

/// A node from its stored plaintext (exactly 4,096 bytes).
pub fn node_from_bytes(b: &[u8]) -> Result<Node> {
    let (chunks, rest) = b.as_chunks::<32>();
    if chunks.len() != FANOUT || !rest.is_empty() {
        return Err(Error::Format);
    }
    let mut n = [ZERO; FANOUT];
    n.copy_from_slice(chunks);
    Ok(n)
}

/// The stored keys of one volume (blk.md §1).
#[derive(Clone)]
pub struct VolumeKeys {
    vol: NameChain,
    blocks: NameChain,
    nodes: NameChain,
}

impl VolumeKeys {
    /// The keys of volume `name` in `fs`.
    pub fn new(fs: &FsKeys, name: &[u8]) -> Self {
        let vol = NameChain::new(fs.kv_name_root()).child(b"blk").child(name);
        VolumeKeys {
            blocks: vol.child(b"b"),
            nodes: vol.child(b"t"),
            vol,
        }
    }

    /// The vol id: the stored key of `("blk", name)`, 32 bytes.
    pub fn vol_id(&self) -> Vec<u8> {
        self.vol.token().to_vec()
    }

    /// The root record's key, `("blk", name, "r")`.
    pub fn root(&self) -> Vec<u8> {
        self.vol.child(b"r").token().to_vec()
    }

    /// The lease name: the stored key of `("blk", name, "lease")`, 48 bytes.
    pub fn lease(&self) -> Vec<u8> {
        self.vol.child(b"lease").token().to_vec()
    }

    /// Block `i`'s key, `("blk", name, "b", u64(i))`.
    pub fn block(&self, i: u64) -> Vec<u8> {
        self.blocks.child(&i.to_be_bytes()).token().to_vec()
    }

    /// Node `(level, j)`'s key, `("blk", name, "t", u8(level) ‖ u64(j))`.
    pub fn node(&self, level: u8, j: u64) -> Vec<u8> {
        let mut e = [0u8; 9];
        e[0] = level;
        e[1..].copy_from_slice(&j.to_be_bytes());
        self.nodes.child(&e).token().to_vec()
    }
}

/// The root record (blk.md §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    /// 1, 2, …
    pub seq: u64,
    /// `RH` of the previous record, zeros for seq 1.
    pub prev: Hash,
    /// The volume's size in blocks.
    pub blocks: u64,
    /// `hash(D, 0)`.
    pub root: Hash,
    /// The fingerprint of the device that wrote it.
    pub writer: Hash,
    /// When it was written.
    pub hlc: u64,
}

impl RootRecord {
    /// Deterministic CBOR.
    pub fn encode(&self) -> Vec<u8> {
        let int = |x: u64| Value::Int(i128::from(x));
        let m = vec![
            (int(1), int(1)),
            (int(2), int(self.seq)),
            (int(3), Value::Bytes(self.prev.to_vec())),
            (int(4), int(self.blocks)),
            (int(5), int(BLOCK_SIZE as u64)),
            (int(6), Value::Bytes(self.root.to_vec())),
            (int(7), Value::Bytes(self.writer.to_vec())),
            (int(8), int(self.hlc)),
        ];
        cbor::encode(&Value::Map(m)).expect("valid root record")
    }

    /// Strict decode: deterministic CBOR, `v = 1`, `block_size = 4096`,
    /// `seq ≥ 1`, `blocks ≥ 1`, every field, no others.
    pub fn decode(b: &[u8]) -> Result<RootRecord> {
        let Value::Map(m) = cbor::decode(b)? else {
            return Err(Error::Format);
        };
        let int = |v: &Value| match v {
            Value::Int(i) if *i >= 0 && *i <= i128::from(u64::MAX) => Ok(*i as u64),
            _ => Err(Error::Format),
        };
        let b32 = |v: &Value| match v {
            Value::Bytes(x) => Hash::try_from(x.as_slice()).map_err(|_| Error::Format),
            _ => Err(Error::Format),
        };
        let mut f: [Option<&Value>; 8] = [None; 8];
        for (k, x) in &m {
            match k {
                Value::Int(i @ 1..=8) => f[*i as usize - 1] = Some(x),
                _ => return Err(Error::Format),
            }
        }
        let [
            Some(v),
            Some(seq),
            Some(prev),
            Some(blocks),
            Some(bs),
            Some(root),
            Some(w),
            Some(hlc),
        ] = f
        else {
            return Err(Error::Format);
        };
        if int(v)? != 1 || int(bs)? != BLOCK_SIZE as u64 {
            return Err(Error::Format);
        }
        let r = RootRecord {
            seq: int(seq)?,
            prev: b32(prev)?,
            blocks: int(blocks)?,
            root: b32(root)?,
            writer: b32(w)?,
            hlc: int(hlc)?,
        };
        if r.seq == 0 || r.blocks == 0 || r.encode() != b {
            return Err(Error::Format);
        }
        Ok(r)
    }

    /// `RH = H("zen/v1/blk-root", CBOR(Root))`.
    pub fn hash(&self) -> Hash {
        root_hash(&self.encode())
    }
}

/// `RH` of an encoded root record.
pub fn root_hash(encoded: &[u8]) -> Hash {
    blake3::derive_key(labels::BLK_ROOT, encoded)
}

/// A volume's tree: its root and every stored (non-empty) node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// `hash(D, 0)`.
    pub root: Hash,
    /// By `(level, j)`.
    pub nodes: BTreeMap<(u8, u64), Node>,
}

/// The reference tree of a volume of `blocks` blocks whose stored blocks
/// have `leaves` (absent blocks are left out; a zero leaf counts as absent).
pub fn tree(blocks: u64, leaves: &BTreeMap<u64, Hash>) -> Tree {
    let d = depth(blocks);
    let mut nodes = BTreeMap::new();
    // The children of the level being built, by index.
    let mut below: BTreeMap<u64, Hash> = leaves
        .iter()
        .filter(|(_, h)| **h != ZERO)
        .map(|(i, h)| (*i, *h))
        .collect();
    for level in 1..=d {
        let mut built: BTreeMap<u64, Node> = BTreeMap::new();
        for (c, h) in &below {
            let n = built.entry(c >> FANOUT_BITS).or_insert([ZERO; FANOUT]);
            n[(*c as usize) & (FANOUT - 1)] = *h;
        }
        below = BTreeMap::new();
        for (j, n) in built {
            below.insert(j, node_hash(level, j, &n));
            nodes.insert((level, j), n);
        }
    }
    Tree {
        root: below.get(&0).copied().unwrap_or(ZERO),
        nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> RootRecord {
        RootRecord {
            seq: 3,
            prev: [1; 32],
            blocks: 20_000,
            root: [2; 32],
            writer: [3; 32],
            hlc: 1 << 60,
        }
    }

    #[test]
    fn depth_and_slots() {
        assert_eq!(depth(1), 1);
        assert_eq!(depth(128), 1);
        assert_eq!(depth(129), 2);
        assert_eq!(depth(16_384), 2);
        assert_eq!(depth(20_000), 3);
        assert_eq!(depth(1 << 28), 4); // 1 TiB
        assert_eq!(node_of(1, 16_500), 128);
        assert_eq!(slot_of(1, 16_500), 16_500 % 128);
        assert_eq!(node_of(2, 16_500), 1);
        assert_eq!(slot_of(2, 16_500), 0);
        assert_eq!(slot_of(3, 16_500), 1);
    }

    #[test]
    fn root_record_round_trips_and_refuses() {
        let r = rec();
        let b = r.encode();
        assert_eq!(RootRecord::decode(&b).unwrap(), r);
        let with = |k: i128, v: Value| {
            let Value::Map(mut m) = cbor::decode(&b).unwrap() else {
                unreachable!()
            };
            m.retain(|(x, _)| *x != Value::Int(k));
            m.push((Value::Int(k), v));
            cbor::encode(&Value::Map(m)).unwrap()
        };
        for bad in [
            with(1, Value::Int(2)),
            with(5, Value::Int(512)),
            with(2, Value::Int(0)),
            with(4, Value::Int(0)),
            with(9, Value::Int(0)),
            with(3, Value::Bytes(vec![1; 31])),
            with(8, Value::text("x")),
        ] {
            assert!(RootRecord::decode(&bad).is_err());
        }
        let Value::Map(mut m) = cbor::decode(&b).unwrap() else {
            unreachable!()
        };
        m.retain(|(x, _)| *x != Value::Int(7));
        assert!(RootRecord::decode(&cbor::encode(&Value::Map(m)).unwrap()).is_err());
        let mut trailing = b.clone();
        trailing.push(0);
        assert!(RootRecord::decode(&trailing).is_err());
        assert_ne!(r.hash(), RootRecord { seq: 4, ..r }.hash());
    }

    #[test]
    fn reference_tree_by_hand() {
        // Empty.
        assert_eq!(tree(1_000, &BTreeMap::new()).root, ZERO);
        // One block of a one-level volume.
        let l = leaf(5, b"x");
        let t = tree(100, &BTreeMap::from([(5, l)]));
        let mut n = [ZERO; FANOUT];
        n[5] = l;
        assert_eq!(t.root, node_hash(1, 0, &n));
        assert_eq!(t.nodes.len(), 1);
        // A full node, under a two-level volume.
        let leaves: BTreeMap<u64, Hash> = (128..256).map(|i| (i, leaf(i, b"y"))).collect();
        let t = tree(300, &leaves);
        let mut n1 = [ZERO; FANOUT];
        for (k, i) in (128..256).enumerate() {
            n1[k] = leaves[&i];
        }
        let mut top = [ZERO; FANOUT];
        top[1] = node_hash(1, 1, &n1);
        assert_eq!(t.root, node_hash(2, 0, &top));
        assert_eq!(t.nodes.len(), 2);
        // The last block of a volume that isn't a power of 128.
        let last = 20_000 - 1;
        let t = tree(20_000, &BTreeMap::from([(last, leaf(last, b"z"))]));
        assert!(t.nodes.contains_key(&(1, last / 128)));
        assert!(t.nodes.contains_key(&(2, last / 16_384)));
        assert!(t.nodes.contains_key(&(3, 0)));
        // A zero leaf is absent.
        assert_eq!(tree(10, &BTreeMap::from([(1, ZERO)])).root, ZERO);
        // Node bytes round-trip.
        assert_eq!(node_from_bytes(&node_bytes(&n1)).unwrap(), n1);
        assert!(node_from_bytes(&[0; 4095]).is_err());
    }

    #[test]
    fn keys_are_distinct_and_sized() {
        let fs = FsKeys::generate(7, &mut crate::rng::DetRng::new("fs")).unwrap();
        let k = VolumeKeys::new(&fs, b"disk");
        assert_eq!(k.vol_id().len(), 32);
        assert_eq!(k.lease().len(), 48);
        assert_eq!(k.root().len(), 48);
        assert_eq!(k.block(1).len(), 64);
        assert_eq!(k.node(1, 0).len(), 64);
        assert!(k.block(1).starts_with(&k.vol_id()));
        assert_ne!(k.block(1), k.block(2));
        assert_ne!(k.node(1, 0), k.node(2, 0));
        assert_eq!(
            k.block(3),
            crate::token::kv_key(&fs, &[&b"blk"[..], b"disk", b"b", &3u64.to_be_bytes()])
        );
    }
}
