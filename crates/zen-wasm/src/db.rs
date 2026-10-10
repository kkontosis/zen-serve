//! zen-db primitives for `@zen/db` (spec/zendb.md): database keys, which
//! derive from the fs naming key and so stay in WASM, index keys, CRDT-row
//! tokens and kind-7 values, and consumer-group ids. Encodings that need no
//! key (CBOR, sort keys, rows) are implemented in TypeScript and checked
//! against spec/test-vectors/zendb.json.

use crate::crypto::{FsKeys, TopicKeys};
use crate::{core_err, js_err};
use wasm_bindgen::prelude::*;
use zen_core::db as zdb;
use zen_core::rng::OsRng;

type R<T> = Result<T, JsValue>;

fn id16(b: &[u8]) -> R<[u8; 16]> {
    b.try_into().map_err(|_| js_err("param"))
}

/// The keys of one database `(fs, ns)`. Secret.
#[wasm_bindgen]
pub struct DbKeys(zdb::DbKeys);

#[wasm_bindgen]
impl FsKeys {
    /// The keys of database `ns` (zendb.md §2).
    pub fn db(&self, ns: &str) -> DbKeys {
        DbKeys(zdb::DbKeys::new(&self.0, ns))
    }

    /// Seal a CRDT value (kind 7, zendb.md §19.2). `field` is zeros for the
    /// row register, `elem` zeros when there is none.
    #[wasm_bindgen(js_name = sealCrdtValue)]
    pub fn seal_crdt_value(
        &self,
        object: &[u8],
        field: &[u8],
        elem: &[u8],
        plaintext: &[u8],
    ) -> R<Vec<u8>> {
        zdb::seal_crdt_value(
            &self.0,
            object,
            &id16(field)?,
            &id16(elem)?,
            plaintext,
            &mut OsRng,
        )
        .map_err(core_err)
    }

    /// Open a CRDT value. Its epoch must be this key's.
    #[wasm_bindgen(js_name = openCrdtValue)]
    pub fn open_crdt_value(
        &self,
        object: &[u8],
        field: &[u8],
        elem: &[u8],
        sealed: &[u8],
    ) -> R<Vec<u8>> {
        zdb::open_crdt_value(&self.0, object, &id16(field)?, &id16(elem)?, sealed).map_err(core_err)
    }
}

#[wasm_bindgen]
impl DbKeys {
    /// The stored key of `D = ("zen", "db", ns)`.
    #[wasm_bindgen(getter)]
    pub fn prefix(&self) -> Vec<u8> {
        self.0.prefix().to_vec()
    }

    /// The stored key of `D ‖ elements`.
    pub fn key(&self, elements: Vec<js_sys::Uint8Array>) -> Vec<u8> {
        let e: Vec<Vec<u8>> = elements.into_iter().map(|a| a.to_vec()).collect();
        self.0.key(&e)
    }

    /// The keys of a private index.
    pub fn index(&self, index_id: &[u8]) -> R<IndexKeys> {
        Ok(IndexKeys(self.0.index(&id16(index_id)?)))
    }

    /// The token keys of a CRDT table.
    pub fn crdt(&self, table_id: &[u8]) -> R<CrdtKeys> {
        Ok(CrdtKeys(self.0.crdt(&id16(table_id)?)))
    }
}

/// `K_boundary` and `K_node` of one private index. Secret.
#[wasm_bindgen]
pub struct IndexKeys(zdb::IndexKeys);

#[wasm_bindgen]
impl IndexKeys {
    /// Whether an entry ends a node by hash (zendb.md §5.4.1).
    #[wasm_bindgen(js_name = isBoundary)]
    pub fn is_boundary(&self, level: u8, key: &[u8], fanout: u32) -> bool {
        self.0.is_boundary(level, key, fanout)
    }

    /// For each key, whether it ends a node at `level`: one call per level.
    pub fn boundaries(&self, level: u8, keys: Vec<js_sys::Uint8Array>, fanout: u32) -> Vec<u8> {
        keys.iter()
            .map(|k| self.0.is_boundary(level, &k.to_vec(), fanout) as u8)
            .collect()
    }

    /// The shard of a pk element (zendb.md §5.4.5).
    pub fn shard(&self, pk_element: &[u8], shards: u32) -> u32 {
        self.0.shard(pk_element, shards)
    }

    /// `PRF16(K_node, node)`.
    #[wasm_bindgen(js_name = nodeId)]
    pub fn node_id(&self, node: &[u8]) -> Vec<u8> {
        self.0.node_id(node).to_vec()
    }

    /// The canonical tree of a set of sort keys (the reference of zendb.md
    /// §5.4.1): `[id, encoded node, …]`, bottom level first, then the root
    /// record as the last element (absent for an empty set).
    pub fn build(
        &self,
        entries: Vec<js_sys::Uint8Array>,
        fanout: u32,
    ) -> R<Vec<js_sys::Uint8Array>> {
        let e = entries.into_iter().map(|a| a.to_vec()).collect();
        let t = zdb::prolly::build(&self.0, fanout, e).map_err(core_err)?;
        let mut out = Vec::with_capacity(2 * t.nodes.len() + 1);
        for (id, n) in &t.nodes {
            out.push(js_sys::Uint8Array::from(&id[..]));
            out.push(js_sys::Uint8Array::from(&n.encode()[..]));
        }
        if let Some(r) = t.root {
            out.push(js_sys::Uint8Array::from(&r.encode()[..]));
        }
        Ok(out)
    }
}

/// `K_crdt` of one CRDT table. Secret.
#[wasm_bindgen]
pub struct CrdtKeys(zdb::CrdtKeys);

#[wasm_bindgen]
impl CrdtKeys {
    /// The field token of a field name.
    pub fn field(&self, name: &str) -> Vec<u8> {
        self.0.field(name).to_vec()
    }

    /// The element token of a set element (its CBOR) in a row (its pk element).
    pub fn elem(&self, name: &str, pk_element: &[u8], element: &[u8]) -> Vec<u8> {
        self.0.elem(name, pk_element, element).to_vec()
    }
}

#[wasm_bindgen]
impl TopicKeys {
    /// The server's name of a zen-db consumer group on this topic (zendb.md §11.3).
    #[wasm_bindgen(js_name = groupId)]
    pub fn group_id(&self, name: &str) -> Vec<u8> {
        self.0.group_id(name.as_bytes()).to_vec()
    }
}

/// `H("zen/v1/db-parts-digest", bytes)`.
#[wasm_bindgen(js_name = partsDigest)]
pub fn parts_digest(bytes: &[u8]) -> Vec<u8> {
    zdb::parts_digest(bytes).to_vec()
}
