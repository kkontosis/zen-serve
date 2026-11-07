//! The embedded single-node backend on `redb`.
//!
//! * **Snapshots.** Each read version handed out by `begin` keeps a redb
//!   read transaction open for the MVCC window (~5 s), so every read at that
//!   version sees one consistent snapshot.
//! * **Commits** are serialized under one writer. They are validated
//!   **backward**: a commit fails with [`Error::Conflict`] if any of its read
//!   conflict ranges intersects a write committed after its read version.
//!   Recent committed writes are kept in memory for the window. A read
//!   version older than that gives [`Error::TooOld`].
//! * **Versions** come from a clock of ≈1,000,000 per second:
//!   `max(last + 1, unix_micros)`, persisted so they increase across restarts.
//! * **Large values are split** across whole pages. redb keeps an entry
//!   larger than a page in a leaf of its own, rounded up to a power of two
//!   of pages, so a 65,584-byte value would take 32 pages of 4 KiB rather
//!   than 17. Here a value whose entry doesn't fit one page goes to the
//!   `parts` table in pieces that each fill a one-entry leaf exactly, and
//!   its `kv` entry is a record of the rest, short of a piece
//!   (`0x01 ‖ u32 len ‖ u32 pieces ‖ tail`), which shares a leaf with other
//!   small entries. Smaller values are kept inline (`0x00 ‖ bytes`).

use crate::tuple::Key;
use crate::{
    Error, KeyValue, Result, STAMP_LEN, Stamp, Storage, Txn, Version, Watch, key_after, stamp_of,
};
use redb::{Database, ReadOnlyTable, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// `[begin, end)` key range.
type Range = (Vec<u8>, Vec<u8>);

/// Every key, to its value tagged: [`INLINE`] or [`SPLIT`].
const KV: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
/// A split value's pieces, at [`part_key`].
const PARTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("parts");

/// `0x00 ‖ bytes`: the value itself.
const INLINE: u8 = 0;
/// `0x01 ‖ u32le len ‖ u32le pieces ‖ tail`: the value is its pieces in
/// `PARTS`, then the tail.
const SPLIT: u8 = 1;
/// redb's page size: its default, which only its own tests can change.
const PAGE: usize = 4096;
/// A one-entry leaf's header: 4 bytes, plus a `u32` key end and a `u32`
/// value end (redb `RawLeafBuilder::required_bytes`).
const LEAF_ONE: usize = 12;
/// No split for keys so long that a page would hold less than this of a
/// value: stored inline (in a larger leaf) instead.
const MIN_PIECE: usize = 1024;
/// The layout of this file, kept at [`format_key`].
const FORMAT: u64 = 2;

type Table<'t> = redb::Table<'t, &'static [u8], &'static [u8]>;

/// `u32be(len(key)) ‖ key ‖ u32be(i)`: one key's pieces are contiguous,
/// in order, and apart from every other key's.
fn part_key(key: &[u8], i: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(key.len() + 8);
    k.extend_from_slice(&(key.len() as u32).to_be_bytes());
    k.extend_from_slice(key);
    k.extend_from_slice(&i.to_be_bytes());
    k
}

/// The piece size for `key`: what a one-entry leaf of `PARTS` holds, so
/// every piece but the last fills one page. `None` keeps the value inline.
fn piece_len(key: &[u8], value_len: usize) -> Option<usize> {
    if LEAF_ONE + key.len() + 1 + value_len <= PAGE {
        return None;
    }
    PAGE.checked_sub(LEAF_ONE + key.len() + 8)
        .filter(|&n| n >= MIN_PIECE)
}

fn corrupt(key: &[u8], what: &str) -> Error {
    Error::Io(format!(
        "embedded store: the value of key {key:02x?} {what}"
    ))
}

/// A `kv` entry decoded: the value, read from `parts` if split.
fn decode<P: ReadableTable<&'static [u8], &'static [u8]>>(
    key: &[u8],
    raw: &[u8],
    parts: &P,
) -> Result<Vec<u8>> {
    match raw.split_first() {
        Some((&INLINE, v)) => Ok(v.to_vec()),
        Some((&SPLIT, h)) if h.len() >= 8 => {
            let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes")) as usize;
            let n = u32::from_le_bytes(h[4..8].try_into().expect("4 bytes"));
            let mut out = Vec::with_capacity(len);
            let mut got = 0u32;
            let (b, e) = (part_key(key, 0), part_key(key, n));
            for p in parts
                .range::<&[u8]>(b.as_slice()..e.as_slice())
                .map_err(io)?
            {
                let (_, v) = p.map_err(io)?;
                out.extend_from_slice(v.value());
                got += 1;
            }
            out.extend_from_slice(&h[8..]);
            if got != n || out.len() != len {
                return Err(corrupt(
                    key,
                    &format!("has {got} of {n} pieces, {} of {len} bytes", out.len()),
                ));
            }
            Ok(out)
        }
        _ => Err(corrupt(key, "has no valid tag")),
    }
}

/// The piece count of a `kv` entry: 0 if inline.
fn pieces_of(raw: &[u8]) -> u32 {
    match raw {
        [SPLIT, h @ ..] if h.len() >= 8 => u32::from_le_bytes(h[4..8].try_into().expect("4 bytes")),
        _ => 0,
    }
}

/// Remove pieces `from..to` of `key`.
fn drop_pieces(parts: &mut Table<'_>, key: &[u8], from: u32, to: u32) -> Result<()> {
    for i in from..to {
        parts.remove(part_key(key, i).as_slice()).map_err(io)?;
    }
    Ok(())
}

/// Write `value` at `key`, inline or split, and remove what the old value
/// left in `parts`.
fn put(kv: &mut Table<'_>, parts: &mut Table<'_>, key: &[u8], value: &[u8]) -> Result<()> {
    let (record, n) = match piece_len(key, value.len()) {
        None => {
            let mut r = Vec::with_capacity(1 + value.len());
            r.push(INLINE);
            r.extend_from_slice(value);
            (r, 0)
        }
        Some(piece) => {
            let mut n = 0u32;
            let mut whole = value.chunks_exact(piece);
            for c in &mut whole {
                parts.insert(part_key(key, n).as_slice(), c).map_err(io)?;
                n += 1;
            }
            let tail = whole.remainder();
            let mut r = Vec::with_capacity(9 + tail.len());
            r.push(SPLIT);
            r.extend_from_slice(&(value.len() as u32).to_le_bytes());
            r.extend_from_slice(&n.to_le_bytes());
            r.extend_from_slice(tail);
            (r, n)
        }
    };
    let old = kv
        .insert(key, record.as_slice())
        .map_err(io)?
        .map(|g| pieces_of(g.value()))
        .unwrap_or(0);
    drop_pieces(parts, key, n, old)
}

/// Remove `key` and its pieces.
fn remove(kv: &mut Table<'_>, parts: &mut Table<'_>, key: &[u8]) -> Result<()> {
    let old = kv
        .remove(key)
        .map_err(io)?
        .map(|g| pieces_of(g.value()))
        .unwrap_or(0);
    drop_pieces(parts, key, 0, old)
}

/// One read transaction's tables.
struct Tables {
    kv: ReadOnlyTable<&'static [u8], &'static [u8]>,
    parts: ReadOnlyTable<&'static [u8], &'static [u8]>,
}

/// Options for [`Embedded`].
#[derive(Clone, Debug)]
pub struct Options {
    /// MVCC window: how long a read version stays valid. Default 5 s.
    pub window: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            window: Duration::from_secs(5),
        }
    }
}

/// A snapshot handed out at some read version.
#[derive(Clone)]
struct Snapshot {
    version: Version,
    table: Arc<Tables>,
}

struct State {
    last: Version,
    /// `(commit version, written ranges)`, oldest first.
    recent: VecDeque<(Version, Vec<Range>)>,
    /// Writes at or below this version have been forgotten.
    pruned_up_to: Version,
    /// Highest read version handed out; commits get a higher version.
    max_rv: Version,
    /// Table snapshot of the latest commit, created lazily.
    latest: Option<Arc<Tables>>,
    /// Read versions handed out within the window.
    snapshots: BTreeMap<Version, Snapshot>,
}

struct Inner {
    db: Database,
    window: u64,
    state: Mutex<State>,
    watches: Mutex<BTreeMap<Vec<u8>, Weak<Notify>>>,
}

/// The embedded backend. Cheap to clone.
#[derive(Clone)]
pub struct Embedded(Arc<Inner>);

fn io<E: std::fmt::Display>(e: E) -> Error {
    Error::Io(e.to_string())
}

/// A blocking task that didn't finish: cancelled (the runtime is shutting
/// down before it ran) or panicked.
fn joined(e: tokio::task::JoinError) -> Error {
    if e.is_cancelled() {
        Error::Shutdown
    } else {
        io(e)
    }
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn meta_version_key() -> Vec<u8> {
    Key::new().str("meta").str("version").finish()
}

fn format_key() -> Vec<u8> {
    Key::new().str("meta").str("store_format").finish()
}

fn version_value(v: &[u8]) -> u64 {
    u64::from_be_bytes(v.try_into().unwrap_or([0; 8]))
}

impl Embedded {
    /// Open or create the database file at `path`. A file written in
    /// another layout (an older zen-serve's) is refused.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Self> {
        let path = path.as_ref();
        let db = Database::create(path).map_err(io)?;
        let last = {
            let w = db.begin_write().map_err(io)?;
            let last = {
                let mut kv = w.open_table(KV).map_err(io)?;
                let mut parts = w.open_table(PARTS).map_err(io)?;
                let format = kv
                    .get(format_key().as_slice())
                    .map_err(io)?
                    .map(|g| g.value().to_vec());
                match format {
                    Some(f) => {
                        let f = decode(&format_key(), &f, &parts)?;
                        if f != FORMAT.to_be_bytes() {
                            return Err(Error::Io(format!(
                                "{} is in store format {}, and this zen-serve reads {FORMAT}",
                                path.display(),
                                version_value(&f)
                            )));
                        }
                    }
                    None if kv.first().map_err(io)?.is_some() => {
                        return Err(Error::Io(format!(
                            "{} was written by an older zen-serve; start from an empty data_dir",
                            path.display()
                        )));
                    }
                    None => put(&mut kv, &mut parts, &format_key(), &FORMAT.to_be_bytes())?,
                }
                match kv.get(meta_version_key().as_slice()).map_err(io)? {
                    Some(g) => version_value(&decode(&meta_version_key(), g.value(), &parts)?),
                    None => 0,
                }
            };
            w.commit().map_err(io)?;
            last
        };
        Ok(Embedded(Arc::new(Inner {
            db,
            window: opts.window.as_micros() as u64,
            state: Mutex::new(State {
                last,
                recent: VecDeque::new(),
                pruned_up_to: last,
                max_rv: last,
                latest: None,
                snapshots: BTreeMap::new(),
            }),
            watches: Mutex::new(BTreeMap::new()),
        })))
    }

    /// How many pieces of split values the file holds (for tests: what
    /// overwrites and clears leave behind).
    #[cfg(feature = "testing")]
    pub fn part_count(&self) -> Result<u64> {
        let r = self.0.db.begin_read().map_err(io)?;
        redb::ReadableTableMetadata::len(&r.open_table(PARTS).map_err(io)?).map_err(io)
    }

    fn snapshot(&self, read_version: Option<Version>) -> Result<Snapshot> {
        let mut st = self.0.state.lock().expect("state lock");
        let now = unix_micros().max(st.last).max(st.max_rv);
        let floor = now.saturating_sub(self.0.window);
        st.snapshots.retain(|&v, _| v >= floor);
        match read_version {
            Some(rv) => st.snapshots.get(&rv).cloned().ok_or(Error::TooOld),
            None => {
                // Like FoundationDB, the read version advances with the clock
                // even when nothing commits.
                let table = match &st.latest {
                    Some(t) => t.clone(),
                    None => {
                        let r = self.0.db.begin_read().map_err(io)?;
                        let t = Arc::new(Tables {
                            kv: r.open_table(KV).map_err(io)?,
                            parts: r.open_table(PARTS).map_err(io)?,
                        });
                        st.latest = Some(t.clone());
                        t
                    }
                };
                let s = Snapshot {
                    version: now,
                    table,
                };
                st.max_rv = now;
                st.snapshots.insert(now, s.clone());
                Ok(s)
            }
        }
    }

    fn notify(&self, written: &[(Vec<u8>, Vec<u8>)]) {
        let mut w = self.0.watches.lock().expect("watch lock");
        for (b, e) in written {
            for (_, n) in w.range::<[u8], _>((Bound::Included(&b[..]), Bound::Excluded(&e[..]))) {
                if let Some(n) = n.upgrade() {
                    n.notify_waiters();
                }
            }
        }
        if w.len() > 1024 {
            w.retain(|_, n| n.strong_count() > 0);
        }
    }

    fn commit_blocking(&self, t: EmbeddedTxn) -> Result<Stamp> {
        let mut st = self.0.state.lock().expect("state lock");
        let rv = t.snap.version;
        let now = unix_micros();
        if rv < st.pruned_up_to || rv.saturating_add(self.0.window) < now {
            return Err(Error::TooOld);
        }
        for (v, ranges) in st.recent.iter().rev() {
            if *v <= rv {
                break;
            }
            for (wb, we) in ranges {
                for (rb, re) in &t.read_conflicts {
                    if wb < re && rb < we {
                        return Err(Error::Conflict);
                    }
                }
            }
        }
        let version = (st.last + 1).max(st.max_rv + 1).max(now);
        let stamp = stamp_of(version);
        let mut written = Vec::new();
        let w = self.0.db.begin_write().map_err(io)?;
        {
            let mut kv = w.open_table(KV).map_err(io)?;
            let mut parts = w.open_table(PARTS).map_err(io)?;
            for op in t.ops {
                match op {
                    Op::Set(k, v) => {
                        put(&mut kv, &mut parts, &k, &v)?;
                        written.push((key_after(&k), k));
                    }
                    Op::Clear(k) => {
                        remove(&mut kv, &mut parts, &k)?;
                        written.push((key_after(&k), k));
                    }
                    Op::ClearRange(b, e) => {
                        if b < e {
                            let mut split = Vec::new();
                            kv.retain_in::<&[u8], _>(b.as_slice()..e.as_slice(), |k, v| {
                                let n = pieces_of(v);
                                if n > 0 {
                                    split.push((k.to_vec(), n));
                                }
                                false
                            })
                            .map_err(io)?;
                            for (k, n) in split {
                                drop_pieces(&mut parts, &k, 0, n)?;
                            }
                            written.push((e, b));
                        }
                    }
                    Op::VsKey(prefix, suffix, v) => {
                        let mut k = prefix;
                        k.extend_from_slice(&stamp);
                        k.extend_from_slice(&suffix);
                        put(&mut kv, &mut parts, &k, &v)?;
                        written.push((key_after(&k), k));
                    }
                    Op::VsValue(k, prefix, suffix) => {
                        let mut v = prefix;
                        v.extend_from_slice(&stamp);
                        v.extend_from_slice(&suffix);
                        put(&mut kv, &mut parts, &k, &v)?;
                        written.push((key_after(&k), k));
                    }
                    Op::Add(k, delta) => {
                        let cur = match kv.get(k.as_slice()).map_err(io)? {
                            Some(g) => {
                                let b = decode(&k, g.value(), &parts)?;
                                let mut a = [0u8; 8];
                                let n = b.len().min(8);
                                a[..n].copy_from_slice(&b[..n]);
                                i64::from_le_bytes(a)
                            }
                            None => 0,
                        };
                        let next = cur.wrapping_add(delta).to_le_bytes();
                        put(&mut kv, &mut parts, &k, &next)?;
                        written.push((key_after(&k), k));
                    }
                }
            }
            put(
                &mut kv,
                &mut parts,
                &meta_version_key(),
                &version.to_be_bytes(),
            )?;
        }
        w.commit().map_err(io)?;
        // `written` holds (end, begin) pairs; flip them to (begin, end).
        let written: Vec<(Vec<u8>, Vec<u8>)> = written.into_iter().map(|(e, b)| (b, e)).collect();
        st.last = version;
        st.latest = None;
        st.recent.push_back((version, written.clone()));
        let floor = version.saturating_sub(self.0.window);
        while let Some((v, _)) = st.recent.front() {
            if *v >= floor {
                break;
            }
            st.pruned_up_to = *v;
            st.recent.pop_front();
        }
        drop(st);
        self.notify(&written);
        Ok(stamp)
    }
}

#[async_trait::async_trait]
impl Storage for Embedded {
    async fn begin(&self, read_version: Option<Version>) -> Result<Box<dyn Txn>> {
        let snap = self.snapshot(read_version)?;
        Ok(Box::new(EmbeddedTxn {
            store: self.clone(),
            snap,
            overlay: BTreeMap::new(),
            cleared: Vec::new(),
            ops: Vec::new(),
            read_conflicts: Vec::new(),
        }))
    }

    async fn watch(&self, key: &[u8]) -> Result<Watch> {
        let n = {
            let mut w = self.0.watches.lock().expect("watch lock");
            match w.get(key).and_then(Weak::upgrade) {
                Some(n) => n,
                None => {
                    let n = Arc::new(Notify::new());
                    w.insert(key.to_vec(), Arc::downgrade(&n));
                    n
                }
            }
        };
        let mut fut = Box::pin(n.clone().notified_owned());
        fut.as_mut().enable();
        Ok(Watch::new(async move {
            fut.await;
            drop(n);
        }))
    }

    async fn now_version(&self) -> Result<Version> {
        let st = self.0.state.lock().expect("state lock");
        Ok(st.last.max(unix_micros()))
    }

    async fn advance_version(&self, at_least: Version) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut st = store.0.state.lock().expect("state lock");
            if st.last >= at_least {
                return Ok(());
            }
            let w = store.0.db.begin_write().map_err(io)?;
            {
                let mut kv = w.open_table(KV).map_err(io)?;
                let mut parts = w.open_table(PARTS).map_err(io)?;
                put(
                    &mut kv,
                    &mut parts,
                    &meta_version_key(),
                    &at_least.to_be_bytes(),
                )?;
            }
            w.commit().map_err(io)?;
            st.last = at_least;
            st.max_rv = st.max_rv.max(at_least);
            st.latest = None;
            Ok(())
        })
        .await
        .map_err(joined)?
    }
}

enum Op {
    Set(Vec<u8>, Vec<u8>),
    Clear(Vec<u8>),
    ClearRange(Vec<u8>, Vec<u8>),
    VsKey(Vec<u8>, Vec<u8>, Vec<u8>),
    VsValue(Vec<u8>, Vec<u8>, Vec<u8>),
    Add(Vec<u8>, i64),
}

/// Overlay entry for read-your-writes.
#[derive(Clone)]
enum Pending {
    Value(Vec<u8>),
    Cleared,
    Unreadable,
}

struct EmbeddedTxn {
    store: Embedded,
    snap: Snapshot,
    overlay: BTreeMap<Vec<u8>, Pending>,
    cleared: Vec<(Vec<u8>, Vec<u8>)>,
    ops: Vec<Op>,
    read_conflicts: Vec<(Vec<u8>, Vec<u8>)>,
}

impl EmbeddedTxn {
    fn in_cleared(&self, key: &[u8]) -> bool {
        self.cleared
            .iter()
            .any(|(b, e)| b.as_slice() <= key && key < e.as_slice())
    }

    fn read(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.overlay.get(key) {
            Some(Pending::Value(v)) => return Ok(Some(v.clone())),
            Some(Pending::Cleared) => return Ok(None),
            Some(Pending::Unreadable) => return Err(Error::Unreadable),
            None => {}
        }
        if self.in_cleared(key) {
            return Ok(None);
        }
        let t = &self.snap.table;
        match t.kv.get(key).map_err(io)? {
            Some(g) => Ok(Some(decode(key, g.value(), &t.parts)?)),
            None => Ok(None),
        }
    }

    fn read_range(
        &self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        if begin >= end || limit == 0 {
            return Ok(Vec::new());
        }
        let parts = &self.snap.table.parts;
        let base = self.snap.table.kv.range::<&[u8]>(begin..end).map_err(io)?;
        let mut base: Box<dyn Iterator<Item = _>> = if reverse {
            Box::new(base.rev())
        } else {
            Box::new(base)
        };
        let over = self
            .overlay
            .range::<[u8], _>((Bound::Included(begin), Bound::Excluded(end)));
        let mut over: Box<dyn Iterator<Item = (&Vec<u8>, &Pending)>> = if reverse {
            Box::new(over.rev())
        } else {
            Box::new(over)
        };
        let mut out = Vec::new();
        let mut b_next = base.next().transpose().map_err(io)?;
        let mut o_next = over.next();
        while out.len() < limit {
            let take_over = match (&b_next, &o_next) {
                (None, None) => break,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some((bk, _)), Some((ok, _))) => {
                    let bk = bk.value();
                    if bk == ok.as_slice() {
                        // Overlay shadows the base entry.
                        b_next = base.next().transpose().map_err(io)?;
                        true
                    } else {
                        (ok.as_slice() < bk) != reverse
                    }
                }
            };
            if take_over {
                let (k, p) = o_next.take().expect("overlay entry");
                match p {
                    Pending::Value(v) => out.push((k.clone(), v.clone())),
                    Pending::Cleared => {}
                    Pending::Unreadable => return Err(Error::Unreadable),
                }
                o_next = over.next();
            } else {
                let (k, v) = b_next.take().expect("base entry");
                let k = k.value();
                if !self.in_cleared(k) {
                    out.push((k.to_vec(), decode(k, v.value(), parts)?));
                }
                b_next = base.next().transpose().map_err(io)?;
            }
        }
        Ok(out)
    }

    fn conflict_for_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
        got: &[KeyValue],
    ) {
        if got.len() < limit || got.is_empty() {
            self.add_read_conflict_range(begin, end);
        } else if reverse {
            let last = got.last().expect("non-empty").0.clone();
            self.add_read_conflict_range(&last, end);
        } else {
            let last = key_after(&got.last().expect("non-empty").0);
            self.add_read_conflict_range(begin, &last);
        }
    }
}

#[async_trait::async_trait]
impl Txn for EmbeddedTxn {
    fn read_version(&self) -> Version {
        self.snap.version
    }

    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let v = self.read(key)?;
        self.add_read_conflict_range(key, &key_after(key));
        Ok(v)
    }

    async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read(key)
    }

    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        let got = self.read_range(begin, end, limit, reverse)?;
        self.conflict_for_range(begin, end, limit, reverse, &got);
        Ok(got)
    }

    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        self.read_range(begin, end, limit, reverse)
    }

    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]) {
        if begin < end {
            self.read_conflicts.push((begin.to_vec(), end.to_vec()));
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.overlay
            .insert(key.to_vec(), Pending::Value(value.to_vec()));
        self.ops.push(Op::Set(key.to_vec(), value.to_vec()));
    }

    fn clear(&mut self, key: &[u8]) {
        self.overlay.insert(key.to_vec(), Pending::Cleared);
        self.ops.push(Op::Clear(key.to_vec()));
    }

    fn clear_range(&mut self, begin: &[u8], end: &[u8]) {
        if begin >= end {
            return;
        }
        let doomed: Vec<Vec<u8>> = self
            .overlay
            .range::<[u8], _>((Bound::Included(begin), Bound::Excluded(end)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in doomed {
            self.overlay.remove(&k);
        }
        self.cleared.push((begin.to_vec(), end.to_vec()));
        self.ops.push(Op::ClearRange(begin.to_vec(), end.to_vec()));
    }

    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]) {
        self.ops
            .push(Op::VsKey(prefix.to_vec(), suffix.to_vec(), value.to_vec()));
    }

    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]) {
        debug_assert!(prefix.len() + STAMP_LEN + suffix.len() > 0);
        self.overlay.insert(key.to_vec(), Pending::Unreadable);
        self.ops
            .push(Op::VsValue(key.to_vec(), prefix.to_vec(), suffix.to_vec()));
    }

    fn atomic_add(&mut self, key: &[u8], delta: i64) {
        self.overlay.insert(key.to_vec(), Pending::Unreadable);
        self.ops.push(Op::Add(key.to_vec(), delta));
    }

    async fn commit(self: Box<Self>) -> Result<Stamp> {
        if self.ops.is_empty() {
            return Ok(stamp_of(self.snap.version));
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.commit_blocking(*self))
            .await
            .map_err(joined)?
    }
}
