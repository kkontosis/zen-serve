//! zen-db primitives (spec/zendb.md): database keys, deterministic CBOR,
//! sort keys, the prolly tree, row padding and parts, and CRDT-row tokens
//! and values (kind 7).
//!
//! The database itself lives in `@zen/db`. What is here either needs the fs
//! naming key, which never leaves this crate, or is the reference the
//! TypeScript code is checked against (spec/test-vectors/zendb.json).

pub mod cbor;
pub mod prolly;
pub mod sortkey;

use crate::encoding::lp;
use crate::kdf::{Key32, kdf, prf16};
use crate::keys::FsKeys;
use crate::labels;
use crate::rng::Rng;
use crate::seal::{self, Aad, Kind};
use crate::token::NameChain;
use crate::{Error, Result};
use cbor::Value;

/// The keys of one database `(fs, ns)` (zendb.md §2).
pub struct DbKeys {
    k_db: Key32,
    root: NameChain,
}

impl DbKeys {
    /// Derive `K_db = KDF("zen/v1/db", NK, u32(fs) ‖ lp(ns))` and the naming
    /// chain at `("zen", "db", ns)`.
    pub fn new(fs: &FsKeys, ns: &str) -> Self {
        DbKeys {
            k_db: fs.db_key(ns.as_bytes()),
            root: NameChain::new(fs.kv_name_root()).path(&[b"zen".as_ref(), b"db", ns.as_bytes()]),
        }
    }

    /// The stored key of `D`, the prefix of every key of the database.
    pub fn prefix(&self) -> &[u8] {
        self.root.token()
    }

    /// The stored key of `D ‖ elements`.
    pub fn key<E: AsRef<[u8]>>(&self, elements: &[E]) -> Vec<u8> {
        self.root.path(elements).token().to_vec()
    }

    /// The keys of a private index.
    pub fn index(&self, index_id: &[u8; 16]) -> IndexKeys {
        IndexKeys {
            boundary: kdf(labels::DB_BOUNDARY, &self.k_db, index_id),
            node: kdf(labels::DB_NODE_ID, &self.k_db, index_id),
        }
    }

    /// The token key of a CRDT table (§19.2).
    pub fn crdt(&self, table_id: &[u8; 16]) -> CrdtKeys {
        CrdtKeys(kdf(labels::DB_CRDT, &self.k_db, table_id))
    }

    /// `K_db`, for the vectors only.
    #[cfg(feature = "test-utils")]
    pub fn raw_k_db(&self) -> [u8; 32] {
        *self.k_db
    }
}

/// `K_boundary` and `K_node` of a private index (§2.2, §5.4).
pub struct IndexKeys {
    boundary: Key32,
    node: Key32,
}

impl IndexKeys {
    /// From raw keys (tests and vectors).
    pub fn from_parts(boundary: [u8; 32], node: [u8; 32]) -> Self {
        IndexKeys {
            boundary: zeroize::Zeroizing::new(boundary),
            node: zeroize::Zeroizing::new(node),
        }
    }

    /// Whether an entry with `key` at `level` ends a node by hash (§5.4.1):
    /// `u32_be(PRF16(K_boundary, u8(L) ‖ lp(key))[0..4]) < 2^32 / fanout`.
    pub fn is_boundary(&self, level: u8, key: &[u8], fanout: u32) -> bool {
        let mut d = Vec::with_capacity(5 + key.len());
        d.push(level);
        lp(&mut d, key);
        let h = prf16(&self.boundary, &d);
        let x = u32::from_be_bytes(h[..4].try_into().expect("4")) as u64;
        x < (1u64 << 32) / fanout.max(1) as u64
    }

    /// The shard of a pk element among `k` (§5.4.5).
    pub fn shard(&self, pk_element: &[u8], k: u32) -> u32 {
        let mut d = vec![0xff];
        lp(&mut d, pk_element);
        let h = prf16(&self.boundary, &d);
        u32::from_be_bytes(h[..4].try_into().expect("4")) % k.max(1)
    }

    /// `(K_boundary, K_node)`, for the vectors only.
    #[cfg(feature = "test-utils")]
    pub fn raw(&self) -> ([u8; 32], [u8; 32]) {
        (*self.boundary, *self.node)
    }

    /// `node_id = PRF16(K_node, CBOR(Node))`.
    pub fn node_id(&self, node: &[u8]) -> [u8; 16] {
        prf16(&self.node, node)
    }
}

/// `K_crdt` of a CRDT table (§19.2).
pub struct CrdtKeys(Key32);

impl CrdtKeys {
    /// `K_crdt`, for the vectors only.
    #[cfg(feature = "test-utils")]
    pub fn raw(&self) -> [u8; 32] {
        *self.0
    }

    /// `PRF16(K_crdt, 0x00 ‖ lp(field_name))`.
    pub fn field(&self, name: &str) -> [u8; 16] {
        let mut d = vec![0x00];
        lp(&mut d, name.as_bytes());
        prf16(&self.0, &d)
    }

    /// `PRF16(K_crdt, 0x01 ‖ lp(field_name) ‖ lp(pk element) ‖ lp(CBOR(element)))`.
    pub fn elem(&self, name: &str, pk_element: &[u8], element: &[u8]) -> [u8; 16] {
        let mut d = vec![0x01];
        lp(&mut d, name.as_bytes());
        lp(&mut d, pk_element);
        lp(&mut d, element);
        prf16(&self.0, &d)
    }
}

/// `H("zen/v1/db-parts-digest", bytes)` (§4.2, §5.7, §10.3).
pub fn parts_digest(bytes: &[u8]) -> [u8; 32] {
    blake3::derive_key(labels::DB_PARTS_DIGEST, bytes)
}

/// Row size buckets (§4.3): 256 B, 1 KiB, 4 KiB, 16 KiB, then multiples of 16 KiB.
pub fn row_bucket(len: usize) -> usize {
    match len {
        0..=256 => 256,
        257..=1024 => 1024,
        1025..=4096 => 4096,
        _ => len.div_ceil(16384) * 16384,
    }
}

/// The encoded Row (§4.1–4.3): `{1: pk, 2: fields, 3?: parts, 4?: digest, 5?: pad}`.
/// With `pad`, the pad is the longest that keeps the Row within its bucket.
pub fn encode_row(
    pk: &Value,
    fields: &Value,
    parts: Option<(u32, [u8; 32])>,
    pad: bool,
) -> Result<Vec<u8>> {
    let mut m = vec![(Value::Int(1), pk.clone()), (Value::Int(2), fields.clone())];
    if let Some((n, d)) = parts {
        m.push((Value::Int(3), Value::Int(n as i128)));
        m.push((Value::Int(4), Value::Bytes(d.to_vec())));
    }
    if !pad {
        return cbor::encode(&Value::Map(m));
    }
    m.push((Value::Int(5), Value::Bytes(Vec::new())));
    let base = cbor::encode(&Value::Map(m.clone()))?.len() - 1; // without the empty pad's head
    let bucket = row_bucket(base + 1);
    // base + head_len(p) + p <= bucket, p as large as possible.
    let room = bucket - base;
    let mut p = room.saturating_sub(1);
    while p > 0 && cbor::head_len(p as u64) + p > room {
        p -= 1;
    }
    m.pop();
    m.push((Value::Int(5), Value::Bytes(vec![0; p])));
    cbor::encode(&Value::Map(m))
}

fn crdt_aad(fs_id: u32, object: &[u8], field: &[u8; 16], elem: &[u8; 16]) -> Aad {
    let mut ctx = fs_id.to_be_bytes().to_vec();
    lp(&mut ctx, object);
    ctx.extend_from_slice(field);
    ctx.extend_from_slice(elem);
    Aad {
        label: labels::AAD_CRDT_VALUE,
        ctx,
    }
}

/// Seal a CRDT value (kind 7, §19.2). `field` is zeros for the row register,
/// `elem` zeros when there is none.
pub fn seal_crdt_value(
    fs: &FsKeys,
    object: &[u8],
    field: &[u8; 16],
    elem: &[u8; 16],
    plaintext: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    seal::seal(
        Kind::CrdtValue,
        fs.epoch,
        &fs.kv_data_key(),
        &crdt_aad(fs.fs_id, object, field, elem),
        plaintext,
        rng,
    )
}

/// Open a CRDT value. `fs` must be at the value's epoch ([`seal::peek`]).
pub fn open_crdt_value(
    fs: &FsKeys,
    object: &[u8],
    field: &[u8; 16],
    elem: &[u8; 16],
    sealed: &[u8],
) -> Result<Vec<u8>> {
    let (epoch, pt) = seal::open(
        Kind::CrdtValue,
        &fs.kv_data_key(),
        &crdt_aad(fs.fs_id, object, field, elem),
        sealed,
    )?;
    if epoch != fs.epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
}
