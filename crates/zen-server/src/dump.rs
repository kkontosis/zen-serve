//! Logical export/import of the keyspace (operations.md §backup): works on
//! any backend, holds only ciphertext and metadata, so the operator needs
//! no keys. Also the core of `zen-serve migrate` (embedded → FoundationDB).
//!
//! File format, all integers big-endian:
//! ```text
//! "zen-serve export\n"
//! u32 len ‖ CBOR {format: 1, keyspace: 1, backend: text}
//! repeated: u32 key_len ‖ key ‖ u32 value_len ‖ value     (key order)
//! u32 0xFFFFFFFF ‖ u64 count ‖ u64 source_version ‖ BLAKE3(every frame)(32)
//! ```
//! Keys and values are copied byte for byte, versionstamps included.
//! `source_version` is at or after every versionstamp in the export; an
//! import advances the target's version clock past it, so new versionstamps
//! stay newer than imported ones.

use crate::keys;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use zen_store::{Error, KeyValue, Storage, Version, key_after};

const MAGIC: &[u8] = b"zen-serve export\n";
const END: u32 = u32::MAX;
const CHUNK: usize = 1000;
/// Import transactions stay well under FoundationDB's 10 MB limit.
const TXN_BYTES: usize = 4_000_000;

/// The export header.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    /// File format (1).
    pub format: u32,
    /// Keyspace layout version (spec/keyspace.md, 1).
    pub keyspace: u32,
    /// Source backend.
    pub backend: String,
}

/// What an export or import did.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Key-value pairs.
    pub keys: u64,
    /// Bytes of keys and values.
    pub bytes: u64,
    /// A version at or after every versionstamp copied.
    pub source_version: Version,
    /// The export spanned several read versions (FoundationDB while the
    /// cluster was serving): not one consistent snapshot.
    pub inconsistent: bool,
}

fn io(e: std::io::Error) -> String {
    format!("I/O: {e}")
}

/// Backend-private keys are never exported.
fn private(k: &[u8]) -> bool {
    k == keys::meta("version")
}

/// Stream every pair in key order to `f`, reading in chunks. One
/// transaction is used as long as the backend allows; after `TooOld` the
/// scan continues at a new read version.
async fn scan(
    store: &dyn Storage,
    stats: &mut Stats,
    mut f: impl FnMut(&KeyValue) -> Result<(), String>,
) -> Result<Version, String> {
    let end = vec![0xFF];
    let mut from = Vec::new();
    let mut t = store.begin(None).await.map_err(|e| e.to_string())?;
    let first_rv = t.read_version();
    let mut last_rv = first_rv;
    loop {
        let got = match t.snapshot_get_range(&from, &end, CHUNK, false).await {
            Ok(g) => g,
            Err(Error::TooOld) => {
                t = store.begin(None).await.map_err(|e| e.to_string())?;
                last_rv = t.read_version();
                stats.inconsistent = true;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        for kv in &got {
            if !private(&kv.0) {
                f(kv)?;
            }
        }
        match got.last() {
            Some((k, _)) if got.len() == CHUNK => from = key_after(k),
            _ => return Ok(last_rv.max(first_rv)),
        }
    }
}

/// Write the whole keyspace of `store` to `w`.
pub async fn export(store: &dyn Storage, backend: &str, w: impl Write) -> Result<Stats, String> {
    let mut w = std::io::BufWriter::new(w);
    let header = zen_proto::to_cbor(&Header {
        format: 1,
        keyspace: 1,
        backend: backend.into(),
    });
    w.write_all(MAGIC).map_err(io)?;
    w.write_all(&(header.len() as u32).to_be_bytes())
        .map_err(io)?;
    w.write_all(&header).map_err(io)?;
    let mut stats = Stats::default();
    let mut hash = blake3::Hasher::new();
    let mut keys = 0u64;
    let mut bytes = 0u64;
    let rv = scan(store, &mut stats, |(k, v)| {
        let mut frame = Vec::with_capacity(8 + k.len() + v.len());
        frame.extend_from_slice(&(k.len() as u32).to_be_bytes());
        frame.extend_from_slice(k);
        frame.extend_from_slice(&(v.len() as u32).to_be_bytes());
        frame.extend_from_slice(v);
        hash.update(&frame);
        keys += 1;
        bytes += (k.len() + v.len()) as u64;
        w.write_all(&frame).map_err(io)
    })
    .await?;
    stats.keys = keys;
    stats.bytes = bytes;
    stats.source_version = rv;
    w.write_all(&END.to_be_bytes()).map_err(io)?;
    w.write_all(&stats.keys.to_be_bytes()).map_err(io)?;
    w.write_all(&rv.to_be_bytes()).map_err(io)?;
    w.write_all(hash.finalize().as_bytes()).map_err(io)?;
    w.flush().map_err(io)?;
    Ok(stats)
}

/// Whether `store` holds any (non-private) key.
pub async fn is_empty(store: &dyn Storage) -> Result<bool, String> {
    let mut t = store.begin(None).await.map_err(|e| e.to_string())?;
    let got = t
        .snapshot_get_range(&[], &[0xFF], 2, false)
        .await
        .map_err(|e| e.to_string())?;
    Ok(got.iter().all(|(k, _)| private(k)))
}

/// Writes pairs in bounded transactions.
struct Sink<'a> {
    store: &'a dyn Storage,
    batch: Vec<KeyValue>,
    bytes: usize,
}

impl Sink<'_> {
    async fn push(&mut self, kv: KeyValue) -> Result<(), String> {
        self.bytes += kv.0.len() + kv.1.len();
        self.batch.push(kv);
        if self.bytes > TXN_BYTES || self.batch.len() >= 10_000 {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), String> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let mut attempt = 0;
        loop {
            let mut t = self.store.begin(None).await.map_err(|e| e.to_string())?;
            for (k, v) in &self.batch {
                t.set(k, v);
            }
            match t.commit().await {
                Ok(_) => break,
                // Blind writes of fixed values: replaying them is harmless.
                Err(Error::TooOld | Error::Conflict | Error::CommitUnknown) if attempt < 10 => {
                    attempt += 1;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        self.batch.clear();
        self.bytes = 0;
        Ok(())
    }
}

fn read_exact(r: &mut impl Read, n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).map_err(io)?;
    Ok(b)
}

fn read_u32(r: &mut impl Read) -> Result<u32, String> {
    Ok(u32::from_be_bytes(read_exact(r, 4)?.try_into().expect("4")))
}

/// Load an export into `store`. Refuses a store that holds data unless
/// `force`.
pub async fn import(
    store: &dyn Storage,
    r: impl Read,
    force: bool,
) -> Result<(Header, Stats), String> {
    let mut r = std::io::BufReader::new(r);
    if read_exact(&mut r, MAGIC.len())? != MAGIC {
        return Err("not a zen-serve export".into());
    }
    let n = read_u32(&mut r)? as usize;
    if n > 1 << 20 {
        return Err("bad export header".into());
    }
    let header: Header =
        zen_proto::from_cbor(&read_exact(&mut r, n)?).map_err(|e| format!("export header: {e}"))?;
    if header.format != 1 || header.keyspace != 1 {
        return Err(format!(
            "unsupported export (format {}, keyspace {})",
            header.format, header.keyspace
        ));
    }
    if !force && !is_empty(store).await? {
        return Err("the target holds data (use --force to merge into it)".into());
    }
    let mut sink = Sink {
        store,
        batch: Vec::new(),
        bytes: 0,
    };
    let mut stats = Stats::default();
    let mut hash = blake3::Hasher::new();
    loop {
        let kl = read_u32(&mut r)?;
        if kl == END {
            break;
        }
        if kl > 1 << 20 {
            return Err("bad frame".into());
        }
        let k = read_exact(&mut r, kl as usize)?;
        let vl = read_u32(&mut r)?;
        if vl > 1 << 24 {
            return Err("bad frame".into());
        }
        let v = read_exact(&mut r, vl as usize)?;
        hash.update(&kl.to_be_bytes());
        hash.update(&k);
        hash.update(&vl.to_be_bytes());
        hash.update(&v);
        stats.keys += 1;
        stats.bytes += (k.len() + v.len()) as u64;
        if !private(&k) {
            sink.push((k, v)).await?;
        }
    }
    let count = u64::from_be_bytes(read_exact(&mut r, 8)?.try_into().expect("8"));
    stats.source_version = u64::from_be_bytes(read_exact(&mut r, 8)?.try_into().expect("8"));
    let digest = read_exact(&mut r, 32)?;
    if count != stats.keys || digest != hash.finalize().as_bytes() {
        return Err("export is truncated or corrupt (nothing past the last full batch is lost; re-run with --force)".into());
    }
    sink.flush().await?;
    store
        .advance_version(stats.source_version)
        .await
        .map_err(|e| e.to_string())?;
    Ok((header, stats))
}

/// Copy every pair from `from` to `to` (no file in between), e.g. an
/// embedded store into FoundationDB. Run it with the servers stopped.
pub async fn copy(from: &dyn Storage, to: &dyn Storage, force: bool) -> Result<Stats, String> {
    if !force && !is_empty(to).await? {
        return Err("the target holds data (use --force to merge into it)".into());
    }
    let mut stats = Stats::default();
    let mut sink = Sink {
        store: to,
        batch: Vec::new(),
        bytes: 0,
    };
    let mut from_key = Vec::new();
    loop {
        let mut t = from.begin(None).await.map_err(|e| e.to_string())?;
        stats.source_version = stats.source_version.max(t.read_version());
        let got = t
            .snapshot_get_range(&from_key, &[0xFF], CHUNK, false)
            .await
            .map_err(|e| e.to_string())?;
        let more = got.len() == CHUNK;
        if let Some((k, _)) = got.last() {
            from_key = key_after(k);
        }
        for kv in got {
            if !private(&kv.0) {
                stats.keys += 1;
                stats.bytes += (kv.0.len() + kv.1.len()) as u64;
                sink.push(kv).await?;
            }
        }
        if !more {
            break;
        }
    }
    sink.flush().await?;
    to.advance_version(stats.source_version)
        .await
        .map_err(|e| e.to_string())?;
    Ok(stats)
}
