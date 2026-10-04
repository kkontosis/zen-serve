//! Client formats of the CRDT filesystem (spec/fs.md, spec/formats.md §11):
//! hybrid logical clocks, sealed node meta, manifests and chunks, and the
//! canonical operation encoding behind the per-tree op chain.

use crate::encoding::{Reader, lp};
use crate::keys::FsKeys;
use crate::labels;
use crate::rng::Rng;
use crate::seal::{self, Aad, Kind};
use crate::{Error, Result};

/// Length of tree, node and chunk ids.
pub const ID_LEN: usize = 16;
/// Length of a version dot (`versionstamp ‖ u16`).
pub const DOT_LEN: usize = 12;
/// A tree, node or chunk id.
pub type Id = [u8; ID_LEN];
/// The root node of every tree.
pub const ROOT: Id = [0x00; ID_LEN];
/// The trash node of every tree.
pub const TRASH: Id = [0xFF; ID_LEN];

// ------------------------------------------------------------------ HLC

/// `unix_ms << 16 | counter` (formats.md §11.1).
pub fn hlc(unix_ms: u64, counter: u16) -> u64 {
    unix_ms << 16 | counter as u64
}

/// The wall-clock milliseconds of an HLC value.
pub fn hlc_ms(hlc: u64) -> u64 {
    hlc >> 16
}

/// A client's hybrid logical clock (fs.md §2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock {
    /// The last value issued or observed.
    pub last: u64,
}

impl Clock {
    /// The timestamp for a new operation at wall-clock `wall_ms`.
    pub fn tick(&mut self, wall_ms: u64) -> u64 {
        self.last = (wall_ms << 16).max(self.last + 1);
        self.last
    }

    /// Observe a timestamp read from the server.
    pub fn observe(&mut self, seen: u64) {
        self.last = self.last.max(seen);
    }
}

// ------------------------------------------------------------------ meta

/// Node type, inside the sealed meta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeType {
    /// Directory.
    Dir = 1,
    /// Regular file.
    File = 2,
    /// Symbolic link (its target is the content).
    Symlink = 3,
}

/// Plaintext of a node's meta (formats.md §11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeMeta {
    /// Type.
    pub node_type: NodeType,
    /// Name: UTF-8, 1–255 bytes, no `/` or NUL.
    pub name: String,
    /// POSIX permission bits.
    pub mode: u32,
    /// Modification time, unix milliseconds.
    pub mtime_ms: u64,
    /// CBOR map of extended attributes (empty = none).
    pub xattrs: Vec<u8>,
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name.contains(['/', '\0']) {
        Err(Error::Param)
    } else {
        Ok(())
    }
}

impl NodeMeta {
    /// `0x01 ‖ u8 type ‖ lp(name) ‖ u32 mode ‖ u64 mtime_ms ‖ lp(xattrs)`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        check_name(&self.name)?;
        let mut out = Vec::with_capacity(2 + 4 + self.name.len() + 12 + 4 + self.xattrs.len());
        out.push(1);
        out.push(self.node_type as u8);
        lp(&mut out, self.name.as_bytes());
        out.extend_from_slice(&self.mode.to_be_bytes());
        out.extend_from_slice(&self.mtime_ms.to_be_bytes());
        lp(&mut out, &self.xattrs);
        Ok(out)
    }

    /// Decode.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != 1 {
            return Err(Error::Format);
        }
        let node_type = match r.u8()? {
            1 => NodeType::Dir,
            2 => NodeType::File,
            3 => NodeType::Symlink,
            _ => return Err(Error::Format),
        };
        let name = String::from_utf8(r.lp()?.to_vec()).map_err(|_| Error::Format)?;
        check_name(&name).map_err(|_| Error::Format)?;
        let mode = r.u32()?;
        let mtime_ms = r.u64()?;
        let xattrs = r.lp()?.to_vec();
        r.finish()?;
        Ok(NodeMeta {
            node_type,
            name,
            mode,
            mtime_ms,
            xattrs,
        })
    }
}

// ------------------------------------------------------------------ manifest

/// Plaintext of a file version's manifest (formats.md §11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// File size in bytes.
    pub size: u64,
    /// Plaintext bytes per chunk (all but the last).
    pub chunk_size: u32,
    /// Chunk ids in file order.
    pub chunks: Vec<Id>,
}

impl Manifest {
    /// `0x01 ‖ u64 size ‖ u32 chunk_size ‖ u32 n ‖ n × chunk_id`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(17 + ID_LEN * self.chunks.len());
        out.push(1);
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.chunk_size.to_be_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        for c in &self.chunks {
            out.extend_from_slice(c);
        }
        out
    }

    /// Decode.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != 1 {
            return Err(Error::Format);
        }
        let size = r.u64()?;
        let chunk_size = r.u32()?;
        let n = r.u32()? as usize;
        let mut chunks = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            chunks.push(r.array()?);
        }
        r.finish()?;
        Ok(Manifest {
            size,
            chunk_size,
            chunks,
        })
    }
}

// ------------------------------------------------------------------ sealing

fn node_ctx(label: &'static str, fs_id: u32, tree: &Id, node: &Id) -> Aad {
    let mut ctx = Vec::with_capacity(4 + 2 * ID_LEN);
    ctx.extend_from_slice(&fs_id.to_be_bytes());
    ctx.extend_from_slice(tree);
    ctx.extend_from_slice(node);
    Aad { label, ctx }
}

fn chunk_ctx(fs_id: u32, chunk: &Id) -> Aad {
    let mut ctx = Vec::with_capacity(4 + ID_LEN);
    ctx.extend_from_slice(&fs_id.to_be_bytes());
    ctx.extend_from_slice(chunk);
    Aad {
        label: labels::AAD_FS_CHUNK,
        ctx,
    }
}

fn open_at(fs: &FsKeys, kind: Kind, aad: &Aad, sealed: &[u8]) -> Result<Vec<u8>> {
    let (epoch, pt) = seal::open(kind, &fs.fs_data_key(), aad, sealed)?;
    if epoch != fs.epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
}

/// Seal a node's meta (kind 4), bound to its fs, tree and node.
pub fn seal_meta(
    fs: &FsKeys,
    tree: &Id,
    node: &Id,
    meta: &NodeMeta,
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    let aad = node_ctx(labels::AAD_FS_META, fs.fs_id, tree, node);
    seal::seal(
        Kind::FsMeta,
        fs.epoch,
        &fs.fs_data_key(),
        &aad,
        &meta.encode()?,
        rng,
    )
}

/// Open a node's meta. `fs` must be at the object's epoch ([`seal::peek`]).
pub fn open_meta(fs: &FsKeys, tree: &Id, node: &Id, sealed: &[u8]) -> Result<NodeMeta> {
    let aad = node_ctx(labels::AAD_FS_META, fs.fs_id, tree, node);
    NodeMeta::decode(&open_at(fs, Kind::FsMeta, &aad, sealed)?)
}

/// Seal a manifest (kind 5), bound to its fs, tree and node.
pub fn seal_manifest(
    fs: &FsKeys,
    tree: &Id,
    node: &Id,
    manifest: &Manifest,
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    let aad = node_ctx(labels::AAD_FS_MANIFEST, fs.fs_id, tree, node);
    seal::seal(
        Kind::FsManifest,
        fs.epoch,
        &fs.fs_data_key(),
        &aad,
        &manifest.encode(),
        rng,
    )
}

/// Open a manifest.
pub fn open_manifest(fs: &FsKeys, tree: &Id, node: &Id, sealed: &[u8]) -> Result<Manifest> {
    let aad = node_ctx(labels::AAD_FS_MANIFEST, fs.fs_id, tree, node);
    Manifest::decode(&open_at(fs, Kind::FsManifest, &aad, sealed)?)
}

/// Seal a chunk (kind 6), bound to its fs and id.
pub fn seal_chunk(fs: &FsKeys, chunk: &Id, data: &[u8], rng: &mut dyn Rng) -> Result<Vec<u8>> {
    seal::seal(
        Kind::FsChunk,
        fs.epoch,
        &fs.fs_data_key(),
        &chunk_ctx(fs.fs_id, chunk),
        data,
        rng,
    )
}

/// Open a chunk.
pub fn open_chunk(fs: &FsKeys, chunk: &Id, sealed: &[u8]) -> Result<Vec<u8>> {
    open_at(fs, Kind::FsChunk, &chunk_ctx(fs.fs_id, chunk), sealed)
}

// ------------------------------------------------------------------ op chain

fn op_head(tag: u8, fs_id: u32, tree: &Id, node: &Id) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    b.push(tag);
    b.extend_from_slice(&fs_id.to_be_bytes());
    b.extend_from_slice(tree);
    b.extend_from_slice(node);
    b
}

/// Canonical bytes of a `move` (formats.md §11.5). `meta` is the sealed meta,
/// or empty.
pub fn move_bytes(fs_id: u32, tree: &Id, node: &Id, parent: &Id, hlc: u64, meta: &[u8]) -> Vec<u8> {
    let mut b = op_head(1, fs_id, tree, node);
    b.extend_from_slice(parent);
    b.extend_from_slice(&hlc.to_be_bytes());
    lp(&mut b, meta);
    b
}

/// Canonical bytes of a `meta` operation.
pub fn meta_bytes(fs_id: u32, tree: &Id, node: &Id, hlc: u64, meta: &[u8]) -> Vec<u8> {
    let mut b = op_head(2, fs_id, tree, node);
    b.extend_from_slice(&hlc.to_be_bytes());
    lp(&mut b, meta);
    b
}

/// Canonical bytes of a `write`.
pub fn write_bytes(
    fs_id: u32,
    tree: &Id,
    node: &Id,
    replaces: &[[u8; DOT_LEN]],
    chunks: &[Id],
    manifest: &[u8],
) -> Vec<u8> {
    let mut b = op_head(3, fs_id, tree, node);
    b.extend_from_slice(&(replaces.len() as u32).to_be_bytes());
    for d in replaces {
        b.extend_from_slice(d);
    }
    b.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for c in chunks {
        b.extend_from_slice(c);
    }
    lp(&mut b, manifest);
    b
}

/// `chain_n = BLAKE3.derive_key("zen/v1/tree-op-chain", chain_{n−1} ‖ lp(op) ‖ device)`.
pub fn chain_next(prev: &[u8; 32], op_bytes: &[u8], device: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(labels::TREE_OP_CHAIN);
    h.update(prev);
    h.update(&(op_bytes.len() as u32).to_be_bytes());
    h.update(op_bytes);
    h.update(device);
    *h.finalize().as_bytes()
}
