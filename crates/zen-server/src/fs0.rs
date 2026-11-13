//! The replicated static files, "fs 0" (keyspace.md §3.11, api.md §10,
//! operations.md §9): plaintext files in generations, written by
//! `zen-serve static push` and served at `/unencrypted` when
//! `unencrypted_source = "replicated"`.

use crate::error::{ApiResult, internal};
use crate::keys;
use crate::txn::txn_loop;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use zen_store::Storage;
use zen_store::key_after;
use zen_store::tuple::{Elem, unpack};

/// Bytes per chunk; the last chunk of a file is shorter.
pub const CHUNK: usize = 65_536;

/// Chunks a push writes per transaction (2 MiB).
const PUSH_CHUNKS: usize = 32;

/// Chunks a read fetches per transaction (1 MiB).
pub const READ_BATCH: u64 = 16;

/// Files per transaction when listing.
const LIST_BATCH: usize = 1000;

/// A file of the current generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The generation it belongs to.
    pub generation: u64,
    /// Its size in bytes.
    pub size: u64,
    /// SHA-256 of its bytes.
    pub sha256: [u8; 32],
}

impl Meta {
    /// The number of chunks the file is stored in.
    pub fn chunks(&self) -> u64 {
        self.size.div_ceil(CHUNK as u64)
    }
}

fn encode_meta(size: u64, sha256: &[u8; 32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(41);
    v.push(1);
    v.extend_from_slice(&size.to_be_bytes());
    v.extend_from_slice(sha256);
    v
}

fn decode_meta(v: &[u8]) -> Option<(u64, [u8; 32])> {
    if v.len() != 41 || v[0] != 1 {
        return None;
    }
    let size = u64::from_be_bytes(v[1..9].try_into().ok()?);
    Some((size, v[9..41].try_into().ok()?))
}

fn decode_gen(v: &[u8]) -> ApiResult<u64> {
    Ok(u64::from_be_bytes(
        v.try_into()
            .map_err(|_| internal("bad static generation"))?,
    ))
}

/// The generation in a `("sf" | "sc", gen, …)` key.
fn gen_of(k: &[u8]) -> ApiResult<u64> {
    match unpack(k).map_err(|_| internal("bad static key"))?.get(1) {
        Some(Elem::Int(g)) if *g >= 0 => Ok(*g as u64),
        _ => Err(internal("bad static key")),
    }
}

/// The path in a `("sf", gen, path)` key.
fn path_of(k: &[u8]) -> ApiResult<String> {
    match unpack(k).map_err(|_| internal("bad static key"))?.get(2) {
        Some(Elem::Str(p)) => Ok(p.clone()),
        _ => Err(internal("bad static key")),
    }
}

/// The current generation, if a push has switched to one.
pub async fn current(store: &dyn Storage) -> ApiResult<Option<u64>> {
    let (g, _) = txn_loop!(store, None, |t| {
        match t.snapshot_get(&keys::static_gen()).await? {
            Some(v) => Ok(Some(decode_gen(&v)?)),
            None => Ok(None),
        }
    })?;
    Ok(g)
}

/// The file at `path` in the current generation.
pub async fn open(store: &dyn Storage, path: &str) -> ApiResult<Option<Meta>> {
    let (m, _) = txn_loop!(store, None, |t| {
        let Some(g) = t.snapshot_get(&keys::static_gen()).await? else {
            return Ok(None);
        };
        let generation = decode_gen(&g)?;
        let v = t.snapshot_get(&keys::static_file(generation, path)).await?;
        Ok(v.as_deref()
            .and_then(decode_meta)
            .map(|(size, sha256)| Meta {
                generation,
                size,
                sha256,
            }))
    })?;
    Ok(m)
}

/// Chunks `first..first + n` of a file, in one transaction. Fewer come back
/// when its generation was cleared meanwhile.
pub async fn chunks(
    store: &dyn Storage,
    generation: u64,
    path: &str,
    first: u64,
    n: u64,
) -> ApiResult<Vec<Vec<u8>>> {
    let p = keys::static_chunks(generation, path);
    let b = p.clone().int(first as i64).finish();
    let e = p.int((first + n) as i64).finish();
    let (got, _) = txn_loop!(store, None, |t| {
        Ok(t.snapshot_get_range(&b, &e, n as usize, false).await?)
    })?;
    Ok(got.into_iter().map(|(_, v)| v).collect())
}

/// The current generation's files, in path order.
#[derive(Debug, Default)]
pub struct Listing {
    /// The current generation (none before the first push).
    pub generation: Option<u64>,
    /// `(path, size, sha256)` of each file.
    pub files: Vec<(String, u64, [u8; 32])>,
}

/// List the current generation.
pub async fn list(store: &dyn Storage) -> ApiResult<Listing> {
    let Some(generation) = current(store).await? else {
        return Ok(Listing::default());
    };
    let prefix = keys::static_tag("sf").int(generation as i64).finish();
    let end = keys::end_of(&prefix);
    let mut from = prefix.clone();
    let mut files = Vec::new();
    loop {
        let (got, _) = txn_loop!(store, None, |t| {
            Ok(t.snapshot_get_range(&from, &end, LIST_BATCH, false).await?)
        })?;
        for (k, v) in &got {
            let (size, sha) = decode_meta(v).ok_or_else(|| internal("bad static file record"))?;
            files.push((path_of(k)?, size, sha));
        }
        match got.last() {
            Some((k, _)) if got.len() == LIST_BATCH => from = key_after(k),
            _ => break,
        }
    }
    Ok(Listing {
        generation: Some(generation),
        files,
    })
}

/// Options of [`push`].
#[derive(Debug, Default, Clone)]
pub struct PushOptions {
    /// Write the files, then stop before the switch: an interrupted push
    /// (for tests).
    pub stop_before_switch: bool,
}

/// What [`push`] did.
#[derive(Debug)]
pub struct Pushed {
    /// The new generation.
    pub generation: u64,
    /// Files written.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Paths skipped: symlinks and special files.
    pub skipped: Vec<String>,
}

/// Whether a path segment can be served at `/unencrypted` (statics.rs
/// `clean`): no `\`, NUL, empty, `.` or `..` segment.
fn servable(seg: &str) -> bool {
    !seg.is_empty() && seg != "." && seg != ".." && !seg.contains(['\\', '\0'])
}

/// The regular files below `root`, by `/`-separated relative path, sorted.
fn walk(
    root: &Path,
    rel: &str,
    out: &mut Vec<(String, PathBuf)>,
    skipped: &mut Vec<String>,
) -> Result<(), String> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let path = e.path();
        let Some(name) = name.to_str() else {
            return Err(format!(
                "{}: not UTF-8, so it can't be served",
                path.display()
            ));
        };
        if !servable(name) {
            return Err(format!(
                "{}: can't be served at /unencrypted",
                path.display()
            ));
        }
        let r = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let ft = e
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if ft.is_dir() {
            walk(root, &r, out, skipped)?;
        } else if ft.is_file() {
            out.push((r, path));
        } else {
            skipped.push(r);
        }
    }
    Ok(())
}

/// Writes collected into transactions of at most [`PUSH_CHUNKS`] chunks.
struct Batch<'a> {
    store: &'a dyn Storage,
    sets: Vec<(Vec<u8>, Vec<u8>)>,
    chunks: usize,
}

impl Batch<'_> {
    async fn set(&mut self, k: Vec<u8>, v: Vec<u8>, chunk: bool) -> Result<(), String> {
        self.sets.push((k, v));
        if chunk {
            self.chunks += 1;
            if self.chunks >= PUSH_CHUNKS {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), String> {
        if self.sets.is_empty() {
            return Ok(());
        }
        let sets = &self.sets;
        txn_loop!(self.store, None, |t| {
            for (k, v) in sets {
                t.set(k, v);
            }
            Ok(())
        })
        .map_err(|e| e.message)?;
        self.sets.clear();
        self.chunks = 0;
        Ok(())
    }
}

/// The highest generation present: current, or left by any push.
async fn highest(store: &dyn Storage) -> ApiResult<Option<u64>> {
    let (h, _) = txn_loop!(store, None, |t| {
        let mut h = match t.snapshot_get(&keys::static_gen()).await? {
            Some(v) => Some(decode_gen(&v)?),
            None => None,
        };
        for tag in ["sf", "sc"] {
            let p = keys::static_tag(tag).finish();
            let last = t.snapshot_get_range(&p, &keys::end_of(&p), 1, true).await?;
            if let Some((k, _)) = last.first() {
                h = h.max(Some(gen_of(k)?));
            }
        }
        Ok(h)
    })?;
    Ok(h)
}

/// Read up to `buf.len()` bytes, short only at the end of the file.
async fn read_full(f: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]).await? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Upload the regular files below `dir` as a new generation, switch to it,
/// and clear every generation but the new one and the one before it
/// (keyspace.md §3.11). Run one push at a time.
pub async fn push(store: &dyn Storage, dir: &Path, opts: PushOptions) -> Result<Pushed, String> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    walk(dir, "", &mut files, &mut skipped)?;
    if files.is_empty() {
        return Err(format!("{}: no regular file to push", dir.display()));
    }
    let generation = highest(store)
        .await
        .map_err(|e| e.message)?
        .map_or(1, |h| h + 1);
    let mut batch = Batch {
        store,
        sets: Vec::new(),
        chunks: 0,
    };
    let mut bytes = 0u64;
    let mut buf = vec![0u8; CHUNK];
    for (rel, path) in &files {
        let mut f = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let chunks = keys::static_chunks(generation, rel);
        for i in 0i64.. {
            let n = read_full(&mut f, &mut buf)
                .await
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            size += n as u64;
            batch
                .set(chunks.clone().int(i).finish(), buf[..n].to_vec(), true)
                .await?;
            if n < CHUNK {
                break;
            }
        }
        let sha: [u8; 32] = hash.finalize().into();
        batch
            .set(
                keys::static_file(generation, rel),
                encode_meta(size, &sha),
                false,
            )
            .await?;
        bytes += size;
    }
    batch.flush().await?;
    let pushed = Pushed {
        generation,
        files: files.len() as u64,
        bytes,
        skipped,
    };
    if opts.stop_before_switch {
        return Ok(pushed);
    }
    // The switch.
    let (prev, _) = txn_loop!(store, None, |t| {
        let prev = match t.get(&keys::static_gen()).await? {
            Some(v) => Some(decode_gen(&v)?),
            None => None,
        };
        t.set(&keys::static_gen(), &generation.to_be_bytes());
        Ok(prev)
    })
    .map_err(|e| e.message)?;
    // Clean-up: keep the new generation and the one before it.
    let mut keep: Vec<u64> = prev.into_iter().chain([generation]).collect();
    keep.sort_unstable();
    keep.dedup();
    txn_loop!(store, None, |t| {
        for tag in ["sf", "sc"] {
            let p = keys::static_tag(tag).finish();
            let mut from = p.clone();
            for g in &keep {
                let gp = keys::static_tag(tag).int(*g as i64).finish();
                t.clear_range(&from, &gp);
                from = keys::end_of(&gp);
            }
            t.clear_range(&from, &keys::end_of(&p));
        }
        Ok(())
    })
    .map_err(|e| e.message)?;
    Ok(pushed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips() {
        let v = encode_meta(70_000, &[7; 32]);
        assert_eq!(decode_meta(&v), Some((70_000, [7; 32])));
        assert_eq!(decode_meta(&v[1..]), None);
        let m = Meta {
            generation: 1,
            size: 70_000,
            sha256: [0; 32],
        };
        assert_eq!(m.chunks(), 2);
        assert_eq!(
            Meta {
                size: 0,
                ..m.clone()
            }
            .chunks(),
            0
        );
        assert_eq!(Meta { size: 65_536, ..m }.chunks(), 1);
    }

    #[test]
    fn generations_from_keys() {
        assert_eq!(gen_of(&keys::static_file(300, "a/b")).unwrap(), 300);
        assert_eq!(
            gen_of(&keys::static_chunks(7, "x").int(3).finish()).unwrap(),
            7
        );
        assert_eq!(
            path_of(&keys::static_file(1, "a/b.css")).unwrap(),
            "a/b.css"
        );
    }
}
