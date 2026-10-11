//! Client formats of the CRDT filesystem (spec/fs.md, spec/formats.md §11):
//! hybrid logical clocks, sealed node meta, manifests and chunks, and the
//! canonical operation encoding behind the per-tree op chain.

use crate::encoding::{Reader, lp};
use crate::kdf::{Key32, kdf};
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

/// The body of a version-2 manifest: the chunk list inline, or its pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Chunk ids in file order (form 1).
    Inline(Vec<Id>),
    /// Pages of `page_size` chunk ids each, the last possibly shorter, with
    /// their digests (form 2).
    Paged {
        /// Chunk ids per page.
        page_size: u32,
        /// `(page_id, page_digest)` in file order.
        pages: Vec<(Id, [u8; 32])>,
    },
}

/// Plaintext of a version-2 manifest (formats.md §11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestV2 {
    /// File size in bytes.
    pub size: u64,
    /// Plaintext bytes per chunk (all but the last).
    pub chunk_size: u32,
    /// Number of chunks.
    pub n: u64,
    /// The epoch the file key was made in; the chunks' header epoch.
    pub key_epoch: u32,
    /// The file key (formats.md §11.4).
    pub file_key: [u8; 32],
    /// The chunk list or its pages.
    pub body: Body,
}

impl ManifestV2 {
    /// `0x02 ‖ u64 size ‖ u32 chunk_size ‖ u64 n ‖ u32 key_epoch ‖ file_key ‖ u8 form ‖ body`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(58 + 48 * self.n.min(1 << 16) as usize);
        out.push(2);
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.chunk_size.to_be_bytes());
        out.extend_from_slice(&self.n.to_be_bytes());
        out.extend_from_slice(&self.key_epoch.to_be_bytes());
        out.extend_from_slice(&self.file_key);
        match &self.body {
            Body::Inline(chunks) => {
                out.push(1);
                for c in chunks {
                    out.extend_from_slice(c);
                }
            }
            Body::Paged { page_size, pages } => {
                out.push(2);
                out.extend_from_slice(&page_size.to_be_bytes());
                out.extend_from_slice(&(pages.len() as u32).to_be_bytes());
                for (id, digest) in pages {
                    out.extend_from_slice(id);
                    out.extend_from_slice(digest);
                }
            }
        }
        out
    }

    /// Decode, checking the form and the page count against `n`.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != 2 {
            return Err(Error::Format);
        }
        let size = r.u64()?;
        let chunk_size = r.u32()?;
        let n = r.u64()?;
        let key_epoch = r.u32()?;
        let file_key = r.array()?;
        let body = match r.u8()? {
            1 => {
                // Each id is 16 bytes, so `n` can't exceed what is left.
                if n > (bytes.len() / ID_LEN) as u64 {
                    return Err(Error::Format);
                }
                let mut chunks = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    chunks.push(r.array()?);
                }
                Body::Inline(chunks)
            }
            2 => {
                let page_size = r.u32()?;
                let m = r.u32()? as u64;
                let want = if n == 0 {
                    0
                } else if page_size == 0 {
                    return Err(Error::Format);
                } else {
                    n.div_ceil(page_size as u64)
                };
                if m != want || m > (bytes.len() / 48) as u64 {
                    return Err(Error::Format);
                }
                let mut pages = Vec::with_capacity(m as usize);
                for _ in 0..m {
                    pages.push((r.array()?, r.array()?));
                }
                Body::Paged { page_size, pages }
            }
            _ => return Err(Error::Format),
        };
        r.finish()?;
        Ok(ManifestV2 {
            size,
            chunk_size,
            n,
            key_epoch,
            file_key,
            body,
        })
    }

    /// The number of chunk ids page `i` holds: `page_size`, the last fewer.
    pub fn page_len(&self, i: usize) -> Option<u32> {
        let Body::Paged { page_size, pages } = &self.body else {
            return None;
        };
        if i >= pages.len() {
            return None;
        }
        let before = i as u64 * *page_size as u64;
        Some((self.n - before).min(*page_size as u64) as u32)
    }
}

/// A manifest of either version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnyManifest {
    /// Version 1: chunks under the fs keys.
    V1(Manifest),
    /// Version 2: chunks under a file key, inline or paged.
    V2(ManifestV2),
}

impl AnyManifest {
    /// Encode.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            AnyManifest::V1(m) => m.encode(),
            AnyManifest::V2(m) => m.encode(),
        }
    }

    /// Decode by the version byte.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        match bytes.first() {
            Some(1) => Ok(AnyManifest::V1(Manifest::decode(bytes)?)),
            Some(2) => Ok(AnyManifest::V2(ManifestV2::decode(bytes)?)),
            _ => Err(Error::Format),
        }
    }
}

// ------------------------------------------------------------------ pages

/// Most chunk ids a page can hold within `max_value_bytes`.
pub fn page_capacity(max_value_bytes: u32) -> u32 {
    (max_value_bytes.saturating_sub(4) / ID_LEN as u32).max(1)
}

/// A page of a paged version's chunk list (formats.md §11.6): `u32 n ‖ n × chunk_id`.
pub fn encode_page(chunks: &[Id]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + ID_LEN * chunks.len());
    out.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for c in chunks {
        out.extend_from_slice(c);
    }
    out
}

/// Decode a page; at least one id, and nothing after the last.
pub fn decode_page(bytes: &[u8]) -> Result<Vec<Id>> {
    if bytes.len() < 4 {
        return Err(Error::Format);
    }
    let n = u32::from_be_bytes(bytes[..4].try_into().expect("4")) as usize;
    if n == 0 || bytes.len() != 4 + ID_LEN * n {
        return Err(Error::Format);
    }
    Ok(bytes[4..]
        .chunks(ID_LEN)
        .map(|c| c.try_into().expect("16"))
        .collect())
}

/// `page_digest = BLAKE3.derive_key("zen/v1/fs-page", page)`.
pub fn page_digest(page: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key(labels::FS_PAGE)
        .update(page)
        .finalize()
        .as_bytes()
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

/// Seal a manifest of either version (kind 5), bound to its fs, tree and node.
pub fn seal_any_manifest(
    fs: &FsKeys,
    tree: &Id,
    node: &Id,
    manifest: &AnyManifest,
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

/// Open a manifest of either version.
pub fn open_any_manifest(fs: &FsKeys, tree: &Id, node: &Id, sealed: &[u8]) -> Result<AnyManifest> {
    let aad = node_ctx(labels::AAD_FS_MANIFEST, fs.fs_id, tree, node);
    AnyManifest::decode(&open_at(fs, Kind::FsManifest, &aad, sealed)?)
}

/// A fresh file key.
pub fn new_file_key(rng: &mut dyn Rng) -> Result<[u8; 32]> {
    let mut k = [0u8; 32];
    rng.fill(&mut k)?;
    Ok(k)
}

/// The chunk key of a version-2 file: `KDF("zen/v1/fs-file", file_key, u32(fs))`.
pub fn file_chunk_key(fs_id: u32, file_key: &[u8; 32]) -> Key32 {
    kdf(labels::FS_FILE, file_key, &fs_id.to_be_bytes())
}

/// Seal a chunk of a version-2 file (kind 6) under its file key; the
/// header's epoch is the manifest's `key_epoch`.
pub fn seal_file_chunk(
    fs_id: u32,
    file_key: &[u8; 32],
    key_epoch: u32,
    chunk: &Id,
    data: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    seal::seal(
        Kind::FsChunk,
        key_epoch,
        &file_chunk_key(fs_id, file_key),
        &chunk_ctx(fs_id, chunk),
        data,
        rng,
    )
}

/// Open a chunk of a version-2 file; its header epoch must be `key_epoch`.
pub fn open_file_chunk(
    fs_id: u32,
    file_key: &[u8; 32],
    key_epoch: u32,
    chunk: &Id,
    sealed: &[u8],
) -> Result<Vec<u8>> {
    let (epoch, pt) = seal::open(
        Kind::FsChunk,
        &file_chunk_key(fs_id, file_key),
        &chunk_ctx(fs_id, chunk),
        sealed,
    )?;
    if epoch != key_epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
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

/// Canonical bytes of a `write` with `pages` (`write_paged`).
pub fn write_paged_bytes(
    fs_id: u32,
    tree: &Id,
    node: &Id,
    replaces: &[[u8; DOT_LEN]],
    pages: &[Id],
    manifest: &[u8],
) -> Vec<u8> {
    let mut b = op_head(4, fs_id, tree, node);
    b.extend_from_slice(&(replaces.len() as u32).to_be_bytes());
    for d in replaces {
        b.extend_from_slice(d);
    }
    b.extend_from_slice(&(pages.len() as u32).to_be_bytes());
    for p in pages {
        b.extend_from_slice(p);
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
