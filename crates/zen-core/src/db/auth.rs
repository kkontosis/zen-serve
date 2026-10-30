//! The authenticated tier (spec/zendb.md §14): state-tree entries, the
//! NsRoot and its hash, and the reference state tree.
//!
//! `@zen/db` keeps the state tree incrementally; this is the reference the
//! vectors and its tests are checked against.

use super::IndexKeys;
use super::cbor::{self, Value};
use super::prolly::{self, Tree};
use crate::labels;
use crate::{Error, Result};

/// The fanout of a state tree (§14.2).
pub const STATE_FANOUT: u32 = 64;

/// `esc(x)`: each 0x00 replaced by 0x00 0xFF (§14.2).
pub fn esc(x: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(x.len() + 4);
    for &b in x {
        out.push(b);
        if b == 0 {
            out.push(0xff);
        }
    }
    out
}

/// `VH = H("zen/v1/db-state-value", plaintext)`.
pub fn value_hash(plaintext: &[u8]) -> [u8; 32] {
    blake3::derive_key(labels::DB_STATE_VALUE, plaintext)
}

/// The prefix every entry of a key starts with: `esc(suffix) ‖ 0x00 0x00`.
pub fn entry_prefix(suffix: &[u8]) -> Vec<u8> {
    let mut e = esc(suffix);
    e.extend_from_slice(&[0, 0]);
    e
}

/// The state-tree entry of a value: `esc(suffix) ‖ 0x00 0x00 ‖ VH`, where
/// `suffix` is its stored key without the stored key of `D`.
pub fn state_entry(suffix: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut e = entry_prefix(suffix);
    e.extend_from_slice(&value_hash(plaintext));
    e
}

/// The state tree's root, as the NsRoot names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRoot {
    /// The root node id (32 bytes).
    pub root: [u8; 32],
    /// The root's level + 1.
    pub height: u8,
    /// Values in the tree.
    pub count: u64,
}

/// `{1: v = 1, 2: seq, 3: prev, 4?: [root, height, count]}` (§14.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NsRoot {
    /// 1, 2, …
    pub seq: u64,
    /// `NH` of the previous NsRoot, zeros for seq 1.
    pub prev: [u8; 32],
    /// Absent when the database holds no values.
    pub tree: Option<StateRoot>,
}

impl NsRoot {
    /// Deterministic CBOR.
    pub fn encode(&self) -> Vec<u8> {
        let mut m = vec![
            (Value::Int(1), Value::Int(1)),
            (Value::Int(2), Value::Int(self.seq as i128)),
            (Value::Int(3), Value::Bytes(self.prev.to_vec())),
        ];
        if let Some(t) = &self.tree {
            m.push((
                Value::Int(4),
                Value::Array(vec![
                    Value::Bytes(t.root.to_vec()),
                    Value::Int(t.height as i128),
                    Value::Int(t.count as i128),
                ]),
            ));
        }
        cbor::encode(&Value::Map(m)).expect("valid NsRoot")
    }

    /// Strict decode: deterministic CBOR, `v = 1`, `seq ≥ 1`, no unknown keys.
    pub fn decode(b: &[u8]) -> Result<NsRoot> {
        let Value::Map(m) = cbor::decode(b)? else {
            return Err(Error::Format);
        };
        let int = |v: &Value| match v {
            Value::Int(i) if *i >= 0 && *i <= u64::MAX as i128 => Ok(*i as u64),
            _ => Err(Error::Format),
        };
        let b32 = |v: &Value| match v {
            Value::Bytes(x) => <[u8; 32]>::try_from(x.as_slice()).map_err(|_| Error::Format),
            _ => Err(Error::Format),
        };
        let (mut v, mut seq, mut prev, mut tree) = (None, None, None, None);
        for (k, x) in &m {
            match k {
                Value::Int(1) => v = Some(int(x)?),
                Value::Int(2) => seq = Some(int(x)?),
                Value::Int(3) => prev = Some(b32(x)?),
                Value::Int(4) => {
                    let Value::Array(a) = x else {
                        return Err(Error::Format);
                    };
                    let [r, h, c] = a.as_slice() else {
                        return Err(Error::Format);
                    };
                    let height = u8::try_from(int(h)?).map_err(|_| Error::Format)?;
                    tree = Some(StateRoot {
                        root: b32(r)?,
                        height,
                        count: int(c)?,
                    });
                }
                _ => return Err(Error::Format),
            }
        }
        if v != Some(1) {
            return Err(Error::Format);
        }
        let seq = seq.filter(|s| *s >= 1).ok_or(Error::Format)?;
        let r = NsRoot {
            seq,
            prev: prev.ok_or(Error::Format)?,
            tree,
        };
        if r.encode() != b {
            return Err(Error::Format);
        }
        Ok(r)
    }

    /// `NH = H("zen/v1/db-ns-root", CBOR(NsRoot))`.
    pub fn hash(&self) -> [u8; 32] {
        ns_root_hash(&self.encode())
    }
}

/// `NH` of an encoded NsRoot.
pub fn ns_root_hash(encoded: &[u8]) -> [u8; 32] {
    blake3::derive_key(labels::DB_NS_ROOT, encoded)
}

/// The state tree of a set of values `(suffix, plaintext)` (§14.2): fanout
/// 64 unless given, 32-byte node ids.
pub fn state_tree(keys: &IndexKeys, values: &[(Vec<u8>, Vec<u8>)], fanout: u32) -> Result<Tree> {
    let entries = values.iter().map(|(s, p)| state_entry(s, p)).collect();
    prolly::build(keys, fanout, entries, 32)
}

/// The NsRoot's `tree` field for a built state tree.
pub fn state_root(tree: &Tree) -> Option<StateRoot> {
    tree.root.as_ref().map(|r| StateRoot {
        root: r.root.as_slice().try_into().expect("32-byte ids"),
        height: r.height,
        count: r.count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_sort_like_keys_and_are_prefix_free() {
        // Stored-key order, with one key a prefix of another and zero bytes.
        let keys: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 1],
            vec![0, 0xff],
            vec![1],
            vec![1, 0],
            vec![0xff],
        ];
        for w in keys.windows(2) {
            let a = state_entry(&w[0], b"x");
            let b = state_entry(&w[1], b"y");
            assert!(a < b, "{:?} < {:?}", w[0], w[1]);
            assert!(!b.starts_with(&entry_prefix(&w[0])));
            assert!(!a.starts_with(&entry_prefix(&w[1])));
        }
        // A key range maps to the entry range [esc(b), esc(e)).
        for k in &keys {
            for b in &keys {
                let e = state_entry(k, &[0xff; 3]);
                assert_eq!(k >= b, e >= esc(b), "{k:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn ns_root_round_trip_and_strict() {
        let r = NsRoot {
            seq: 2,
            prev: [7; 32],
            tree: Some(StateRoot {
                root: [9; 32],
                height: 2,
                count: 10,
            }),
        };
        assert_eq!(NsRoot::decode(&r.encode()).unwrap(), r);
        let empty = NsRoot {
            seq: 1,
            prev: [0; 32],
            tree: None,
        };
        assert_eq!(NsRoot::decode(&empty.encode()).unwrap(), empty);
        assert_ne!(r.hash(), empty.hash());
        let bad = |m: Vec<(Value, Value)>| NsRoot::decode(&cbor::encode(&Value::Map(m)).unwrap());
        let p = Value::Bytes(vec![0; 32]);
        assert!(
            bad(vec![
                (Value::Int(1), Value::Int(2)),
                (Value::Int(2), Value::Int(1)),
                (Value::Int(3), p.clone())
            ])
            .is_err()
        );
        assert!(
            bad(vec![
                (Value::Int(1), Value::Int(1)),
                (Value::Int(2), Value::Int(0)),
                (Value::Int(3), p.clone())
            ])
            .is_err()
        );
        assert!(
            bad(vec![
                (Value::Int(1), Value::Int(1)),
                (Value::Int(2), Value::Int(1)),
                (Value::Int(3), p.clone()),
                (Value::Int(5), Value::Int(0))
            ])
            .is_err()
        );
        assert!(
            bad(vec![
                (Value::Int(1), Value::Int(1)),
                (Value::Int(2), Value::Int(1)),
                (Value::Int(3), Value::Bytes(vec![0; 16]))
            ])
            .is_err()
        );
    }

    #[test]
    fn state_tree_has_32_byte_ids() {
        let k = IndexKeys::from_parts([1; 32], [2; 32]);
        let values: Vec<(Vec<u8>, Vec<u8>)> = (0..50u8).map(|i| (vec![i; 32], vec![i])).collect();
        let t = state_tree(&k, &values, 4).unwrap();
        assert!(t.nodes.len() > 1);
        for (id, n) in &t.nodes {
            assert_eq!(id.as_slice(), k.node_id32(&n.encode()));
        }
        let root = state_root(&t).unwrap();
        assert_eq!(root.count, 50);
        assert!(state_root(&state_tree(&k, &[], 4).unwrap()).is_none());
    }
}
