//! The server-merged CRDT filesystem (spec/fs.md):
//! * [`Engine`] applies chunk uploads and filesystem operations inside the
//!   `/v1/commit` transaction: the Kleppmann move-tree with undo/redo over
//!   the move log, LWW meta, multi-value content, chunk reference counts.
//! * Read endpoints (api.md §12).
//! * [`sweep`]: move-log trimming, trash purge, tombstones, chunk GC.

use crate::acl::R_READ;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::offset;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};
use zen_core::fs::{self as zfs, Id, ROOT, TRASH, hlc_ms};
use zen_proto::{
    ByteBuf, Change, Changes, ChunkData, ChunkPut, Chunks, ChunksGet, CrdtOp, FileGet, NodeState,
    Nodes, TreeChain, TreeChanges, TreeChildren, TreeEntry, TreeGet, TreeList, TreeRef, Trees,
    Version as FileVersion, Versions,
};
use zen_store::tuple::{Elem, strinc, unpack_prefix};
use zen_store::{Storage, Txn, VERSIONS_PER_SEC, Version, key_after, stamp_of};

const MAX_WAIT_MS: u32 = 30_000;
/// Fixed part of a node record after `changed`.
const REC_FIXED: usize = 1 + 16 + 8 + 32 + 8 + 32 + 4;
/// Guard against corrupt (cyclic) parent chains.
const MAX_DEPTH: usize = 100_000;

/// An operation timestamp `(hlc, device)`, ordered by hlc then device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ts {
    /// Hybrid logical clock.
    pub hlc: u64,
    /// Device fingerprint.
    pub dev: [u8; 32],
}

/// A node record (keyspace.md §3.6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeRec {
    /// Change offset as stored.
    pub changed: [u8; 12],
    /// Current parent.
    pub parent: Option<Id>,
    /// Timestamp of the move that set `parent`.
    pub move_ts: Ts,
    /// LWW meta.
    pub meta: Option<(Ts, Vec<u8>)>,
    /// Number of content versions.
    pub versions: u32,
}

impl NodeRec {
    /// Everything after `changed(12)`.
    fn body(&self) -> Vec<u8> {
        let meta_len = self.meta.as_ref().map_or(0, |m| m.1.len());
        let mut b = Vec::with_capacity(REC_FIXED + meta_len);
        b.push(u8::from(self.parent.is_some()) | u8::from(self.meta.is_some()) << 1);
        b.extend_from_slice(&self.parent.unwrap_or_default());
        b.extend_from_slice(&self.move_ts.hlc.to_be_bytes());
        b.extend_from_slice(&self.move_ts.dev);
        let (mts, meta) = match &self.meta {
            Some((ts, m)) => (*ts, &m[..]),
            None => (Ts::default(), &[][..]),
        };
        b.extend_from_slice(&mts.hlc.to_be_bytes());
        b.extend_from_slice(&mts.dev);
        b.extend_from_slice(&self.versions.to_be_bytes());
        b.extend_from_slice(meta);
        b
    }

    fn decode(v: &[u8]) -> ApiResult<Self> {
        if v.len() < 12 + REC_FIXED {
            return Err(internal("bad node record"));
        }
        let changed = v[..12].try_into().expect("12");
        let b = &v[12..];
        let flags = b[0];
        let id = |r: std::ops::Range<usize>| -> [u8; 16] { b[r].try_into().expect("16") };
        let dev = |r: std::ops::Range<usize>| -> [u8; 32] { b[r].try_into().expect("32") };
        let u64_at = |at: usize| u64::from_be_bytes(b[at..at + 8].try_into().expect("8"));
        let move_ts = Ts {
            hlc: u64_at(17),
            dev: dev(25..57),
        };
        let meta_ts = Ts {
            hlc: u64_at(57),
            dev: dev(65..97),
        };
        let versions = u32::from_be_bytes(b[97..101].try_into().expect("4"));
        Ok(NodeRec {
            changed,
            parent: (flags & 1 != 0).then(|| id(1..17)),
            move_ts,
            meta: (flags & 2 != 0).then(|| (meta_ts, b[REC_FIXED..].to_vec())),
            versions,
        })
    }

    /// The API view of the node.
    pub fn state(&self, node: &Id) -> NodeState {
        NodeState {
            node: node.to_vec(),
            parent: self.parent.map(|p| p.to_vec()),
            move_hlc: self.move_ts.hlc,
            move_device: self.move_ts.dev.to_vec(),
            meta: self.meta.as_ref().map(|m| m.1.clone()),
            meta_hlc: self.meta.as_ref().map(|m| m.0.hlc),
            meta_device: self.meta.as_ref().map(|m| m.0.dev.to_vec()),
            versions: self.versions,
            changed: self.changed.to_vec(),
        }
    }
}

/// A move-log entry: the move and the parent/timestamp it replaced.
#[derive(Clone, Debug)]
struct LogEntry {
    node: Id,
    parent: Id,
    old: Option<(Id, Ts)>,
}

impl LogEntry {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + 1 + 56);
        b.extend_from_slice(&self.node);
        b.extend_from_slice(&self.parent);
        match &self.old {
            Some((p, ts)) => {
                b.push(1);
                b.extend_from_slice(p);
                b.extend_from_slice(&ts.hlc.to_be_bytes());
                b.extend_from_slice(&ts.dev);
            }
            None => b.push(0),
        }
        b
    }

    fn decode(v: &[u8]) -> ApiResult<Self> {
        let bad = || internal("bad move-log entry");
        if v.len() < 33 {
            return Err(bad());
        }
        let old = match v[32] {
            0 => None,
            1 if v.len() == 33 + 56 => Some((
                v[33..49].try_into().expect("16"),
                Ts {
                    hlc: u64::from_be_bytes(v[49..57].try_into().expect("8")),
                    dev: v[57..89].try_into().expect("32"),
                },
            )),
            _ => return Err(bad()),
        };
        Ok(LogEntry {
            node: v[..16].try_into().expect("16"),
            parent: v[16..32].try_into().expect("16"),
            old,
        })
    }
}

/// Tree header: `u64 ops ‖ chain(32) ‖ resync_before(12)`.
#[derive(Clone, Debug, Default)]
pub struct Header {
    /// Operations applied.
    pub ops: u64,
    /// Op chain head.
    pub chain: [u8; 32],
    /// Change cursors before this need a full resync.
    pub resync_before: [u8; 12],
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(52);
        b.extend_from_slice(&self.ops.to_be_bytes());
        b.extend_from_slice(&self.chain);
        b.extend_from_slice(&self.resync_before);
        b
    }

    fn decode(v: &[u8]) -> ApiResult<Self> {
        if v.len() != 52 {
            return Err(internal("bad tree header"));
        }
        Ok(Header {
            ops: u64::from_be_bytes(v[..8].try_into().expect("8")),
            chain: v[8..40].try_into().expect("32"),
            resync_before: v[40..52].try_into().expect("12"),
        })
    }
}

/// A content version: `dev(32) ‖ u32 n ‖ n × chunk ‖ manifest`.
fn encode_version(dev: &[u8; 32], chunks: &[Id], manifest: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(36 + 16 * chunks.len() + manifest.len());
    b.extend_from_slice(dev);
    b.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for c in chunks {
        b.extend_from_slice(c);
    }
    b.extend_from_slice(manifest);
    b
}

fn decode_version(v: &[u8]) -> ApiResult<([u8; 32], Vec<Id>, Vec<u8>)> {
    let bad = || internal("bad content version");
    if v.len() < 36 {
        return Err(bad());
    }
    let n = u32::from_be_bytes(v[32..36].try_into().expect("4")) as usize;
    let end = 36 + 16 * n;
    if v.len() < end {
        return Err(bad());
    }
    let chunks = v[36..end]
        .chunks(16)
        .map(|c| c.try_into().expect("16"))
        .collect();
    Ok((v[..32].try_into().expect("32"), chunks, v[end..].to_vec()))
}

/// The single 16-byte id element after `pfx_len` bytes of a key.
fn tail_id(k: &[u8], pfx_len: usize) -> ApiResult<Id> {
    match unpack_prefix(&k[pfx_len..], 1)
        .map_err(|_| internal("bad key"))?
        .0
        .as_slice()
    {
        [Elem::Bytes(b)] => b[..].try_into().map_err(|_| internal("bad id in key")),
        _ => Err(internal("bad key")),
    }
}

/// A 16-byte id from the wire.
pub fn id16(b: &[u8], what: &str) -> ApiResult<Id> {
    b.try_into()
        .map_err(|_| bad_request(format!("{what} must be 16 bytes")))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Release one reference to a chunk and make it a GC candidate.
fn release_chunk(t: &mut Box<dyn Txn>, fs: u32, chunk: &Id) {
    t.atomic_add(&keys::chunk_refs(fs, chunk), -1);
    let (p, s) = keys::chunk_gc(fs)
        .vs_incomplete(0)
        .bytes(chunk)
        .finish_incomplete();
    t.set_versionstamped_key(&p, &s, &[]);
}

type NodeKey = (u32, Id, Id);

/// Applies one commit's chunks and filesystem operations.
pub struct Engine<'a> {
    st: &'a Shared,
    device: [u8; 32],
    now_ms: u64,
    nodes: HashMap<NodeKey, Option<NodeRec>>,
    orig: HashMap<NodeKey, Option<NodeRec>>,
    dirty: BTreeSet<NodeKey>,
    /// Nodes whose content changed: re-stamped even if the record is not.
    touched: HashSet<NodeKey>,
    trees: BTreeMap<(u32, Id), Header>,
    new_chunks: HashSet<(u32, Id)>,
    /// Writes so far (the next write's dot index).
    pub writes: u16,
    /// Quota deltas per fs: (bytes, keys).
    pub delta: HashMap<u32, (i64, i64)>,
}

impl<'a> Engine<'a> {
    /// A fresh engine for one transaction attempt.
    pub fn new(st: &'a Shared, device: [u8; 32]) -> Self {
        Engine {
            st,
            device,
            now_ms: unix_ms(),
            nodes: HashMap::new(),
            orig: HashMap::new(),
            dirty: BTreeSet::new(),
            touched: HashSet::new(),
            trees: BTreeMap::new(),
            new_chunks: HashSet::new(),
            writes: 0,
            delta: HashMap::new(),
        }
    }

    async fn get_node(&mut self, t: &mut Box<dyn Txn>, k: NodeKey) -> ApiResult<Option<NodeRec>> {
        if let Some(r) = self.nodes.get(&k) {
            return Ok(r.clone());
        }
        let r = match t.get(&keys::node(k.0, &k.1, &k.2)).await? {
            Some(v) => Some(NodeRec::decode(&v)?),
            None => None,
        };
        self.orig.insert(k, r.clone());
        self.nodes.insert(k, r.clone());
        Ok(r)
    }

    fn put_node(&mut self, k: NodeKey, r: NodeRec) {
        self.nodes.insert(k, Some(r));
        self.dirty.insert(k);
    }

    /// ROOT, TRASH, or a node with a record.
    async fn exists(&mut self, t: &mut Box<dyn Txn>, fs: u32, tree: Id, n: Id) -> ApiResult<bool> {
        Ok(n == ROOT || n == TRASH || self.get_node(t, (fs, tree, n)).await?.is_some())
    }

    /// Whether `anc` is `n` or one of its ancestors.
    async fn is_ancestor(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        anc: Id,
        mut n: Id,
    ) -> ApiResult<bool> {
        for _ in 0..MAX_DEPTH {
            if n == anc {
                return Ok(true);
            }
            if n == ROOT || n == TRASH {
                return Ok(false);
            }
            match self
                .get_node(t, (fs, tree, n))
                .await?
                .and_then(|r| r.parent)
            {
                Some(p) => n = p,
                None => return Ok(false),
            }
        }
        Err(internal("tree too deep or corrupt"))
    }

    async fn header(&mut self, t: &mut Box<dyn Txn>, fs: u32, tree: Id) -> ApiResult<&mut Header> {
        use std::collections::btree_map::Entry;
        Ok(match self.trees.entry((fs, tree)) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let h = match t.get(&keys::tree_header(fs, &tree)).await? {
                    Some(v) => Header::decode(&v)?,
                    None => Header::default(),
                };
                e.insert(h)
            }
        })
    }

    async fn chain(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        op_bytes: &[u8],
    ) -> ApiResult<()> {
        let device = self.device;
        let h = self.header(t, fs, tree).await?;
        h.ops += 1;
        h.chain = zfs::chain_next(&h.chain, op_bytes, &device);
        Ok(())
    }

    /// Kleppmann's do_op: returns the parent and timestamp the move replaced.
    async fn do_move(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        ts: Ts,
        node: Id,
        parent: Id,
    ) -> ApiResult<Option<(Id, Ts)>> {
        let k = (fs, tree, node);
        let existing = self.get_node(t, k).await?;
        let old = existing
            .as_ref()
            .and_then(|r| r.parent.map(|p| (p, r.move_ts)));
        if node == parent || self.is_ancestor(t, fs, tree, node, parent).await? {
            return Ok(old);
        }
        let mut rec = match existing {
            Some(r) => r,
            None => {
                self.delta.entry(fs).or_default().1 += 1;
                NodeRec::default()
            }
        };
        rec.parent = Some(parent);
        rec.move_ts = ts;
        self.put_node(k, rec);
        Ok(old)
    }

    async fn undo(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        e: &LogEntry,
    ) -> ApiResult<()> {
        let k = (fs, tree, e.node);
        // Undo never removes a record, so a missing one was purged (a
        // skipped move can name a node purged since): nothing to undo.
        let Some(mut rec) = self.get_node(t, k).await? else {
            return Ok(());
        };
        match e.old {
            Some((p, ts)) => {
                rec.parent = Some(p);
                rec.move_ts = ts;
            }
            None => {
                rec.parent = None;
                rec.move_ts = Ts::default();
            }
        }
        self.put_node(k, rec);
        Ok(())
    }

    fn check_clock(&self, hlc: u64) -> ApiResult<()> {
        let l = &self.st.cfg.limits;
        if hlc > i64::MAX as u64 {
            return Err(bad_request("hlc out of range"));
        }
        if hlc_ms(hlc) > self.now_ms + l.crdt_max_skew_ms {
            return Err(clock_skew(format!(
                "hlc is ahead of the server clock ({} ms)",
                self.now_ms
            )));
        }
        if hlc_ms(hlc).saturating_add(l.crdt_horizon_secs * 1000) < self.now_ms {
            return Err(stale_op("hlc is older than the horizon; rebase"));
        }
        Ok(())
    }

    async fn apply_move(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        node: Id,
        parent: Id,
        hlc: u64,
    ) -> ApiResult<()> {
        if node == ROOT || node == TRASH {
            return Err(bad_request("ROOT and TRASH cannot be moved"));
        }
        if node == parent {
            return Err(bad_request("a node cannot be its own parent"));
        }
        self.check_clock(hlc)?;
        if !self.exists(t, fs, tree, parent).await? {
            return Err(bad_request("unknown parent"));
        }
        let ts = Ts {
            hlc,
            dev: self.device,
        };
        let prefix = keys::move_log(fs, &tree);
        let pfx = prefix.clone().finish();
        let key = prefix.int(hlc as i64).bytes(&ts.dev).finish();
        if t.get(&key).await?.is_some() {
            return Err(bad_request("operation timestamp reused"));
        }
        let max = self.st.cfg.limits.crdt_max_redo as usize;
        let later = t
            .get_range(&key_after(&key), &keys::end_of(&pfx), max + 1, false)
            .await?;
        if later.len() > max {
            return Err(stale_op("too many later moves to undo; rebase"));
        }
        let mut redo = Vec::with_capacity(later.len());
        for (k, v) in later {
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad move-log key"))?;
            let ts = match elems.as_slice() {
                [Elem::Int(h), Elem::Bytes(d)] if d.len() == 32 => Ts {
                    hlc: *h as u64,
                    dev: d[..].try_into().expect("32"),
                },
                _ => return Err(internal("bad move-log key")),
            };
            redo.push((k, ts, LogEntry::decode(&v)?));
        }
        for (_, _, e) in redo.iter().rev() {
            self.undo(t, fs, tree, e).await?;
        }
        let old = self.do_move(t, fs, tree, ts, node, parent).await?;
        t.set(&key, &LogEntry { node, parent, old }.encode());
        for (k, ts, e) in &redo {
            // A move of a purged node (not a creation) stays as logged.
            if e.old.is_some() && self.get_node(t, (fs, tree, e.node)).await?.is_none() {
                continue;
            }
            let old = self.do_move(t, fs, tree, *ts, e.node, e.parent).await?;
            let entry = LogEntry {
                node: e.node,
                parent: e.parent,
                old,
            };
            t.set(k, &entry.encode());
        }
        Ok(())
    }

    async fn apply_meta(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        tree: Id,
        node: Id,
        hlc: u64,
        meta: &[u8],
    ) -> ApiResult<()> {
        let k = (fs, tree, node);
        let mut rec = self
            .get_node(t, k)
            .await?
            .ok_or_else(|| bad_request("unknown node"))?;
        let ts = Ts {
            hlc,
            dev: self.device,
        };
        if rec.meta.as_ref().is_none_or(|(mts, _)| ts > *mts) {
            let old = rec.meta.as_ref().map_or(0, |m| m.1.len()) as i64;
            self.delta.entry(fs).or_default().0 += meta.len() as i64 - old;
            rec.meta = Some((ts, meta.to_vec()));
            self.put_node(k, rec);
        }
        Ok(())
    }

    async fn apply_write(
        &mut self,
        t: &mut Box<dyn Txn>,
        k: NodeKey,
        replaces: &[ByteBuf],
        chunks: &[Id],
        manifest: &[u8],
    ) -> ApiResult<()> {
        let (fs, tree, node) = k;
        let mut rec = self
            .get_node(t, k)
            .await?
            .ok_or_else(|| bad_request("unknown node"))?;
        let mut bytes = 0i64;
        for d in replaces {
            if d.len() != 12 {
                return Err(bad_request("dots are 12 bytes"));
            }
            let key = keys::versions(fs, &tree, &node)
                .vs(d[..].try_into().expect("12"))
                .finish();
            if let Some(v) = t.get(&key).await? {
                t.clear(&key);
                let (_, old_chunks, old_manifest) = decode_version(&v)?;
                for c in &old_chunks {
                    release_chunk(t, fs, c);
                }
                bytes -= (old_manifest.len() + 16 * old_chunks.len()) as i64;
                rec.versions = rec.versions.saturating_sub(1);
            }
        }
        for c in chunks {
            if !self.new_chunks.contains(&(fs, *c)) && t.get(&keys::chunk(fs, c)).await?.is_none() {
                return Err(bad_request("unknown chunk"));
            }
        }
        let i = self.writes;
        self.writes = self
            .writes
            .checked_add(1)
            .ok_or_else(|| too_large("too many writes"))?;
        let (p, s) = keys::versions(fs, &tree, &node)
            .vs_incomplete(i)
            .finish_incomplete();
        t.set_versionstamped_key(&p, &s, &encode_version(&self.device, chunks, manifest));
        for c in chunks {
            t.atomic_add(&keys::chunk_refs(fs, c), 1);
        }
        bytes += (manifest.len() + 16 * chunks.len()) as i64;
        self.delta.entry(fs).or_default().0 += bytes;
        rec.versions += 1;
        self.put_node(k, rec);
        self.touched.insert(k);
        Ok(())
    }

    /// Store an uploaded chunk (idempotent for identical bytes).
    pub async fn put_chunk(&mut self, t: &mut Box<dyn Txn>, c: &ChunkPut) -> ApiResult<()> {
        let id = id16(&c.id, "chunk id")?;
        let key = keys::chunk(c.fs, &id);
        match t.get(&key).await? {
            Some(v) if v == c.data => {}
            Some(_) => return Err(bad_request("chunk id already holds other data")),
            None => {
                t.set(&key, &c.data);
                self.delta.entry(c.fs).or_default().0 += c.data.len() as i64;
            }
        }
        // A (re-)upload starts the grace period (again).
        let (p, s) = keys::chunk_gc(c.fs)
            .vs_incomplete(0)
            .bytes(&id)
            .finish_incomplete();
        t.set_versionstamped_key(&p, &s, &[]);
        self.new_chunks.insert((c.fs, id));
        Ok(())
    }

    /// Apply one operation and extend its tree's chain.
    pub async fn apply(&mut self, t: &mut Box<dyn Txn>, op: &CrdtOp) -> ApiResult<()> {
        match op {
            CrdtOp::Move {
                fs,
                tree,
                node,
                parent,
                hlc,
                meta,
            } => {
                let (tree, node, parent) = (
                    id16(tree, "tree")?,
                    id16(node, "node")?,
                    id16(parent, "parent")?,
                );
                self.apply_move(t, *fs, tree, node, parent, *hlc).await?;
                if let Some(m) = meta {
                    self.apply_meta(t, *fs, tree, node, *hlc, m).await?;
                }
                let b = zfs::move_bytes(
                    *fs,
                    &tree,
                    &node,
                    &parent,
                    *hlc,
                    meta.as_deref().unwrap_or(&[]),
                );
                self.chain(t, *fs, tree, &b).await
            }
            CrdtOp::Meta {
                fs,
                tree,
                node,
                hlc,
                meta,
            } => {
                let (tree, node) = (id16(tree, "tree")?, id16(node, "node")?);
                if node == ROOT || node == TRASH {
                    return Err(bad_request("ROOT and TRASH have no meta"));
                }
                self.check_clock(*hlc)?;
                self.apply_meta(t, *fs, tree, node, *hlc, meta).await?;
                let b = zfs::meta_bytes(*fs, &tree, &node, *hlc, meta);
                self.chain(t, *fs, tree, &b).await
            }
            CrdtOp::Write {
                fs,
                tree,
                node,
                replaces,
                chunks,
                manifest,
            } => {
                let (tree, node) = (id16(tree, "tree")?, id16(node, "node")?);
                if node == ROOT || node == TRASH {
                    return Err(bad_request("ROOT and TRASH have no content"));
                }
                let chunks: Vec<Id> = chunks
                    .iter()
                    .map(|c| id16(c, "chunk id"))
                    .collect::<ApiResult<_>>()?;
                self.apply_write(t, (*fs, tree, node), replaces, &chunks, manifest)
                    .await?;
                let dots: Vec<[u8; 12]> = replaces
                    .iter()
                    .map(|d| d[..].try_into().expect("checked"))
                    .collect();
                let b = zfs::write_bytes(*fs, &tree, &node, &dots, &chunks, manifest);
                self.chain(t, *fs, tree, &b).await
            }
        }
    }

    /// Write every changed node, its indexes and the tree headers.
    pub fn flush(&mut self, t: &mut Box<dyn Txn>) -> ApiResult<()> {
        for ((fs, tree), h) in &self.trees {
            t.set(&keys::tree_header(*fs, tree), &h.encode());
            t.set_versionstamped_value(&keys::tree_head(*fs, tree), &[], &[]);
        }
        let mut idx: u16 = 0;
        for k @ (fs, tree, node) in &self.dirty {
            let rec = self.nodes[k].clone().expect("dirty nodes exist");
            let orig = self.orig.get(k).cloned().flatten();
            if !self.touched.contains(k) && orig.as_ref().is_some_and(|o| o.body() == rec.body()) {
                continue; // undone and redone back to the same state
            }
            if let Some(o) = &orig {
                t.clear(&keys::changes(*fs, tree).vs(&o.changed).bytes(node).finish());
            }
            let old_parent = orig.as_ref().and_then(|o| o.parent);
            if old_parent != rec.parent {
                if let Some(p) = old_parent {
                    t.clear(&keys::children(*fs, tree, &p).bytes(node).finish());
                }
                if let Some(p) = rec.parent {
                    t.set(&keys::children(*fs, tree, &p).bytes(node).finish(), &[]);
                }
            }
            let mut suffix = idx.to_be_bytes().to_vec();
            suffix.extend_from_slice(&rec.body());
            t.set_versionstamped_value(&keys::node(*fs, tree, node), &[], &suffix);
            let (p, s) = keys::changes(*fs, tree)
                .vs_incomplete(idx)
                .bytes(node)
                .finish_incomplete();
            t.set_versionstamped_key(&p, &s, &[]);
            idx = idx
                .checked_add(1)
                .ok_or_else(|| too_large("a commit may change at most 65535 nodes"))?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ reads

async fn read_txn(st: &Shared, rv: Option<u64>) -> ApiResult<Box<dyn Txn>> {
    Ok(st.store.begin(rv).await?)
}

async fn load_node(
    t: &mut Box<dyn Txn>,
    fs: u32,
    tree: &Id,
    node: &Id,
) -> ApiResult<Option<NodeRec>> {
    match t.snapshot_get(&keys::node(fs, tree, node)).await? {
        Some(v) => Ok(Some(NodeRec::decode(&v)?)),
        None => Ok(None),
    }
}

async fn load_header(t: &mut Box<dyn Txn>, fs: u32, tree: &Id) -> ApiResult<Header> {
    match t.snapshot_get(&keys::tree_header(fs, tree)).await? {
        Some(v) => Header::decode(&v),
        None => Ok(Header::default()),
    }
}

fn check_read(st: &Shared, caller: &Caller, fs: u32) -> ApiResult<()> {
    st.check_fs(fs)?;
    caller.require_fs(fs, R_READ)
}

fn limit_of(st: &Shared, limit: Option<u32>) -> usize {
    limit
        .unwrap_or(1000)
        .clamp(1, st.cfg.limits.max_range_items) as usize
}

/// `POST /v1/fs/tree/list`.
pub async fn tree_list(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<TreeList>,
) -> ApiResult<Cbor<Trees>> {
    check_read(&st, &caller, req.fs)?;
    let mut t = read_txn(&st, None).await?;
    let pfx = keys::trees(req.fs).finish();
    let got = t
        .snapshot_get_range(
            &pfx,
            &keys::end_of(&pfx),
            st.cfg.limits.max_range_items as usize,
            false,
        )
        .await?;
    let mut trees = Vec::new();
    for (k, v) in got {
        let (elems, _) = unpack_prefix(&k[pfx.len()..], 1).map_err(|_| internal("bad tree key"))?;
        if let [Elem::Bytes(tree)] = elems.as_slice() {
            trees.push(TreeEntry {
                tree: tree.clone(),
                ops: Header::decode(&v)?.ops,
            });
        }
    }
    Ok(Cbor(Trees { trees }))
}

/// `POST /v1/fs/tree/get`.
pub async fn tree_get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<TreeGet>,
) -> ApiResult<Cbor<Nodes>> {
    check_read(&st, &caller, req.fs)?;
    if req.nodes.len() > 1000 {
        return Err(too_large("at most 1000 nodes"));
    }
    let tree = id16(&req.tree, "tree")?;
    let mut t = read_txn(&st, req.read_version).await?;
    let mut nodes = Vec::new();
    for n in &req.nodes {
        let n = id16(n, "node")?;
        if let Some(r) = load_node(&mut t, req.fs, &tree, &n).await? {
            nodes.push(r.state(&n));
        }
    }
    Ok(Cbor(Nodes {
        read_version: t.read_version(),
        nodes,
        more: false,
    }))
}

/// `POST /v1/fs/tree/children`.
pub async fn tree_children(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<TreeChildren>,
) -> ApiResult<Cbor<Nodes>> {
    check_read(&st, &caller, req.fs)?;
    let (tree, parent) = (id16(&req.tree, "tree")?, id16(&req.parent, "parent")?);
    let limit = limit_of(&st, req.limit);
    let mut t = read_txn(&st, req.read_version).await?;
    let prefix = keys::children(req.fs, &tree, &parent);
    let pfx = prefix.clone().finish();
    let begin = match &req.after {
        Some(a) => key_after(&prefix.bytes(&id16(a, "after")?).finish()),
        None => pfx.clone(),
    };
    let got = t
        .snapshot_get_range(&begin, &keys::end_of(&pfx), limit + 1, false)
        .await?;
    let more = got.len() > limit;
    let mut nodes = Vec::new();
    for (k, _) in got.into_iter().take(limit) {
        let n = tail_id(&k, pfx.len())?;
        if let Some(r) = load_node(&mut t, req.fs, &tree, &n).await? {
            nodes.push(r.state(&n));
        }
    }
    Ok(Cbor(Nodes {
        read_version: t.read_version(),
        nodes,
        more,
    }))
}

async fn read_changes(st: &Shared, req: &TreeChanges, tree: &Id) -> ApiResult<Changes> {
    let limit = limit_of(st, req.limit);
    let mut t = read_txn(st, None).await?;
    let prefix = keys::changes(req.fs, tree);
    let pfx = prefix.clone().finish();
    let begin = match &req.after {
        Some(a) => {
            let a: [u8; 12] = a[..]
                .try_into()
                .map_err(|_| bad_request("after must be 12 bytes"))?;
            if a < load_header(&mut t, req.fs, tree).await?.resync_before {
                return Err(resync("the cursor is older than the kept tombstones"));
            }
            strinc(&prefix.vs(&a).finish()).ok_or_else(|| internal("bad cursor key"))?
        }
        None => pfx.clone(),
    };
    let got = t
        .snapshot_get_range(&begin, &keys::end_of(&pfx), limit + 1, false)
        .await?;
    let more = got.len() > limit;
    let mut changes = Vec::new();
    for (k, v) in got.into_iter().take(limit) {
        let (elems, _) =
            unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad change key"))?;
        let [Elem::Vs(o), Elem::Bytes(n)] = elems.as_slice() else {
            return Err(internal("bad change key"));
        };
        let node = id16(n, "node")?;
        let state = if v.first() == Some(&1) {
            None
        } else {
            match load_node(&mut t, req.fs, tree, &node).await? {
                Some(r) => Some(r.state(&node)),
                None => continue,
            }
        };
        changes.push(Change {
            offset: o.to_vec(),
            node: node.to_vec(),
            state,
        });
    }
    let cursor = changes
        .last()
        .map(|c| c.offset.clone())
        .or_else(|| req.after.clone());
    Ok(Changes {
        changes,
        cursor,
        more,
    })
}

/// `POST /v1/fs/tree/changes` (long-polls with `wait_ms`).
pub async fn tree_changes(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<TreeChanges>,
) -> ApiResult<Cbor<Changes>> {
    check_read(&st, &caller, req.fs)?;
    let tree = id16(&req.tree, "tree")?;
    let wait = Duration::from_millis(req.wait_ms.unwrap_or(0).min(MAX_WAIT_MS).into());
    let deadline = Instant::now() + wait;
    let head = keys::tree_head(req.fs, &tree);
    loop {
        // Watch before reading, so no change is missed.
        let w = if wait.is_zero() {
            None
        } else {
            Some(st.store.watch(&head).await?)
        };
        let c = read_changes(&st, &req, &tree).await?;
        let now = Instant::now();
        if !c.changes.is_empty() || now >= deadline {
            return Ok(Cbor(c));
        }
        if let Some(w) = w {
            let _ = tokio::time::timeout(deadline - now, w).await;
        }
    }
}

/// `POST /v1/fs/tree/chain`.
pub async fn tree_chain(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<TreeRef>,
) -> ApiResult<Cbor<TreeChain>> {
    check_read(&st, &caller, req.fs)?;
    let tree = id16(&req.tree, "tree")?;
    let mut t = read_txn(&st, None).await?;
    let h = load_header(&mut t, req.fs, &tree).await?;
    Ok(Cbor(TreeChain {
        ops: h.ops,
        chain: h.chain.to_vec(),
    }))
}

/// `POST /v1/fs/file/get`.
pub async fn file_get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<FileGet>,
) -> ApiResult<Cbor<Versions>> {
    check_read(&st, &caller, req.fs)?;
    let (tree, node) = (id16(&req.tree, "tree")?, id16(&req.node, "node")?);
    let mut t = read_txn(&st, req.read_version).await?;
    let pfx = keys::versions(req.fs, &tree, &node).finish();
    let got = t
        .snapshot_get_range(&pfx, &keys::end_of(&pfx), 10_000, false)
        .await?;
    let mut versions = Vec::new();
    for (k, v) in got {
        let (elems, _) =
            unpack_prefix(&k[pfx.len()..], 1).map_err(|_| internal("bad version key"))?;
        let [Elem::Vs(dot)] = elems.as_slice() else {
            return Err(internal("bad version key"));
        };
        let (device, chunks, manifest) = decode_version(&v)?;
        versions.push(FileVersion {
            dot: dot.to_vec(),
            device: device.to_vec(),
            chunks: chunks.iter().map(|c| ByteBuf::from(c.to_vec())).collect(),
            manifest,
        });
    }
    Ok(Cbor(Versions {
        read_version: t.read_version(),
        versions,
    }))
}

/// `POST /v1/fs/chunks/get`.
pub async fn chunks_get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<ChunksGet>,
) -> ApiResult<Cbor<Chunks>> {
    check_read(&st, &caller, req.fs)?;
    if req.ids.len() > 64 {
        return Err(too_large("at most 64 chunks per request"));
    }
    let mut t = read_txn(&st, None).await?;
    let mut chunks = Vec::new();
    for id in &req.ids {
        let id = id16(id, "chunk id")?;
        chunks.push(ChunkData {
            id: id.to_vec(),
            data: t.snapshot_get(&keys::chunk(req.fs, &id)).await?,
        });
    }
    Ok(Cbor(Chunks { chunks }))
}

// ------------------------------------------------------------------ sweeper

/// One sweeper pass over every configured fs (spec/fs.md §6).
pub async fn sweep(st: &Shared, now: Version) -> ApiResult<()> {
    let l = &st.cfg.limits;
    let now_ms = unix_ms();
    // Another node with a slower clock still accepts ops down to its own
    // `now − horizon`: keep `crdt_max_skew_ms` of margin.
    let cutoff_ms = now_ms.saturating_sub(l.crdt_horizon_secs * 1000 + l.crdt_max_skew_ms);
    let horizon_cvs = offset(
        &stamp_of(now.saturating_sub(l.crdt_horizon_secs * VERSIONS_PER_SEC)),
        0,
    );
    let grace_cvs = offset(
        &stamp_of(now.saturating_sub(l.chunk_grace_secs * VERSIONS_PER_SEC)),
        0,
    );
    for f in &st.cfg.fs {
        let fs = f.id;
        let pfx = keys::trees(fs).finish();
        let trees: Vec<Id> = {
            let mut t = st.store.begin(None).await?;
            t.snapshot_get_range(&pfx, &keys::end_of(&pfx), 100_000, false)
                .await?
                .into_iter()
                .filter_map(
                    |(k, _)| match unpack_prefix(&k[pfx.len()..], 1).ok()?.0.as_slice() {
                        [Elem::Bytes(t)] => t[..].try_into().ok(),
                        _ => None,
                    },
                )
                .collect()
        };
        for tree in trees {
            trim_log(st, fs, &tree, cutoff_ms).await?;
            for _ in 0..100 {
                if purge_trash(st, fs, &tree, cutoff_ms).await? == 0 {
                    break;
                }
            }
            drop_tombstones(st, fs, &tree, &horizon_cvs).await?;
        }
        for _ in 0..100 {
            if collect_chunks(st, fs, &grace_cvs).await? == 0 {
                break;
            }
        }
    }
    Ok(())
}

async fn trim_log(st: &Shared, fs: u32, tree: &Id, cutoff_ms: u64) -> ApiResult<()> {
    let b = keys::move_log(fs, tree).finish();
    let e = keys::move_log(fs, tree)
        .int(zfs::hlc(cutoff_ms, 0) as i64)
        .finish();
    txn_loop!(st.store, None, |t| {
        t.clear_range(&b, &e);
        Ok(())
    })?;
    Ok(())
}

fn add_quota(t: &mut Box<dyn Txn>, fs: u32, bytes: i64, nkeys: i64) {
    if bytes != 0 {
        t.atomic_add(&keys::quota(fs, "bytes"), bytes);
    }
    if nkeys != 0 {
        t.atomic_add(&keys::quota(fs, "keys"), nkeys);
    }
}

/// Purge one batch of old trash subtrees; returns the nodes purged.
async fn purge_trash(st: &Shared, fs: u32, tree: &Id, cutoff_ms: u64) -> ApiResult<usize> {
    const BATCH: usize = 500;
    let (n, _) = txn_loop!(st.store, None, |t| {
        let tpfx = keys::children(fs, tree, &TRASH).finish();
        let tops = t.get_range(&tpfx, &keys::end_of(&tpfx), 100, false).await?;
        let mut doomed: Vec<(Id, NodeRec)> = Vec::new();
        'tops: for (k, _) in tops {
            let top = tail_id(&k, tpfx.len())?;
            // Post-order walk: children before parents, so a partial purge
            // only ever removes leaves.
            let mut order = Vec::new();
            let mut stack = vec![(top, false)];
            while let Some((n, expanded)) = stack.pop() {
                let Some(rec) = t
                    .get(&keys::node(fs, tree, &n))
                    .await?
                    .map(|v| NodeRec::decode(&v))
                    .transpose()?
                else {
                    continue;
                };
                if hlc_ms(rec.move_ts.hlc) >= cutoff_ms {
                    continue 'tops; // moved recently: a late op may still need it
                }
                if expanded {
                    order.push((n, rec));
                    if doomed.len() + order.len() >= BATCH {
                        break;
                    }
                    continue;
                }
                stack.push((n, true));
                let cpfx = keys::children(fs, tree, &n).finish();
                for (ck, _) in t
                    .get_range(&cpfx, &keys::end_of(&cpfx), BATCH, false)
                    .await?
                {
                    stack.push((tail_id(&ck, cpfx.len())?, false));
                }
            }
            doomed.extend(order);
            if doomed.len() >= BATCH {
                break;
            }
        }
        let (mut bytes, mut nkeys) = (0i64, 0i64);
        for (i, (n, rec)) in doomed.iter().enumerate() {
            let i = i as u16;
            t.clear(&keys::node(fs, tree, n));
            if let Some(p) = rec.parent {
                t.clear(&keys::children(fs, tree, &p).bytes(n).finish());
            }
            t.clear(&keys::changes(fs, tree).vs(&rec.changed).bytes(n).finish());
            let (p, s) = keys::changes(fs, tree)
                .vs_incomplete(i)
                .bytes(n)
                .finish_incomplete();
            t.set_versionstamped_key(&p, &s, &[1]);
            let (p, s) = keys::tombstones(fs, tree)
                .vs_incomplete(i)
                .bytes(n)
                .finish_incomplete();
            t.set_versionstamped_key(&p, &s, &[]);
            let vpfx = keys::versions(fs, tree, n).finish();
            for (_, v) in t
                .get_range(&vpfx, &keys::end_of(&vpfx), 10_000, false)
                .await?
            {
                let (_, chunks, manifest) = decode_version(&v)?;
                for c in &chunks {
                    release_chunk(&mut t, fs, c);
                }
                bytes -= (manifest.len() + 16 * chunks.len()) as i64;
            }
            t.clear_range(&vpfx, &keys::end_of(&vpfx));
            bytes -= rec.meta.as_ref().map_or(0, |m| m.1.len()) as i64;
            nkeys -= 1;
        }
        if !doomed.is_empty() {
            add_quota(&mut t, fs, bytes, nkeys);
            t.set_versionstamped_value(&keys::tree_head(fs, tree), &[], &[]);
        }
        Ok(doomed.len())
    })?;
    Ok(n)
}

/// Drop tombstones older than the horizon; later cursors before them resync.
async fn drop_tombstones(st: &Shared, fs: u32, tree: &Id, before: &[u8; 12]) -> ApiResult<()> {
    let prefix = keys::tombstones(fs, tree);
    let pfx = prefix.clone().finish();
    let end = prefix.vs(before).finish();
    txn_loop!(st.store, None, |t| {
        let got = t.get_range(&pfx, &end, 1000, false).await?;
        let Some((last, _)) = got.last() else {
            return Ok(());
        };
        let mut newest = [0u8; 12];
        for (k, _) in &got {
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad tombstone"))?;
            let [Elem::Vs(o), Elem::Bytes(n)] = elems.as_slice() else {
                return Err(internal("bad tombstone"));
            };
            t.clear(&keys::changes(fs, tree).vs(o).bytes(n).finish());
            newest = newest.max(*o);
        }
        t.clear_range(&pfx, &key_after(last));
        let hk = keys::tree_header(fs, tree);
        let mut h = match t.get(&hk).await? {
            Some(v) => Header::decode(&v)?,
            None => Header::default(),
        };
        h.resync_before = h.resync_before.max(newest);
        t.set(&hk, &h.encode());
        Ok(())
    })?;
    Ok(())
}

/// Delete unreferenced chunks whose candidacy is older than the grace
/// period; returns the candidates processed.
async fn collect_chunks(st: &Shared, fs: u32, before: &[u8; 12]) -> ApiResult<usize> {
    let prefix = keys::chunk_gc(fs);
    let pfx = prefix.clone().finish();
    let end = prefix.vs(before).finish();
    let (n, _) = txn_loop!(st.store, None, |t| {
        let got = t.get_range(&pfx, &end, 500, false).await?;
        let mut bytes = 0i64;
        let mut seen = HashSet::new();
        for (k, _) in &got {
            t.clear(k);
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad gc key"))?;
            let [Elem::Vs(_), Elem::Bytes(c)] = elems.as_slice() else {
                return Err(internal("bad gc key"));
            };
            if !seen.insert(c.clone()) {
                continue;
            }
            let refs = t
                .get(&keys::chunk_refs(fs, c))
                .await?
                .map(|v| i64::from_le_bytes(v.try_into().unwrap_or_default()))
                .unwrap_or(0);
            if refs <= 0 {
                if let Some(v) = t.get(&keys::chunk(fs, c)).await? {
                    bytes -= v.len() as i64;
                    t.clear(&keys::chunk(fs, c));
                }
                t.clear(&keys::chunk_refs(fs, c));
            }
        }
        add_quota(&mut t, fs, bytes, 0);
        Ok(got.len())
    })?;
    Ok(n)
}

/// Read every node of a tree (tests, tools): `(node, record)` in id order.
pub async fn all_nodes(store: &dyn Storage, fs: u32, tree: &Id) -> ApiResult<Vec<(Id, NodeRec)>> {
    let mut t = store.begin(None).await?;
    let pfx = zen_store::tuple::Key::new()
        .str("tn")
        .int(fs.into())
        .bytes(tree)
        .finish();
    let mut out = Vec::new();
    for (k, v) in t
        .snapshot_get_range(&pfx, &keys::end_of(&pfx), 1_000_000, false)
        .await?
    {
        let n = tail_id(&k, pfx.len())?;
        out.push((n, NodeRec::decode(&v)?));
    }
    Ok(out)
}
