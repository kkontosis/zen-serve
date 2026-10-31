//! A volume: opening (freshness, the lease), reads verified against the
//! tree, buffered writes, and commits (spec/blk.md §4–§6).

use crate::pin::{self, Pin};
use crate::tree::{Plan, Pos, Tree};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use zen_client::kv::send_commit;
use zen_client::{
    Alarm, AlarmKind, AlarmObject, Entry, Error, Integrity, Lease, Result, UnlockedFs,
};
use zen_core::blk::{
    self, BLOCK_SIZE, Hash, Node, RootRecord, VolumeKeys, ZERO, node_bytes, node_from_bytes,
};
use zen_core::rng::OsRng;
use zen_core::seal;
use zen_proto::{ByteBuf, Commit, Expect, Fence, KvGet, KvItems, Write};

/// Options of a [`Volume`].
#[derive(Debug, Clone)]
pub struct VolumeOptions {
    /// The writer's lease (default 30 s), renewed every third of it.
    pub lease_ttl: Duration,
    /// Blocks per commit, and the dirty blocks that make a write flush
    /// (default 256: 1 MiB).
    pub batch_blocks: usize,
    /// Flush this long after the last write (default 5 s; `None`: only on
    /// [`Volume::flush`] and full batches).
    pub idle_flush: Option<Duration>,
    /// Level-1 tree nodes kept (default 4,096, 16 MiB, covering 2 GiB of
    /// blocks); higher levels are always kept.
    pub cache_nodes: usize,
    /// The writer's local root pin (spec/blk.md §6); `None`: none.
    pub pin: Option<PathBuf>,
}

impl Default for VolumeOptions {
    fn default() -> Self {
        VolumeOptions {
            lease_ttl: Duration::from_secs(30),
            batch_blocks: 256,
            idle_flush: Some(Duration::from_secs(5)),
            cache_nodes: 4096,
            pin: None,
        }
    }
}

/// A block's data in memory: `None` for a discarded block (zeros).
type Data = Option<Arc<Vec<u8>>>;

struct State {
    /// Written, not yet flushing.
    dirty: BTreeMap<u64, Data>,
    /// Being committed (reads see these after `dirty`).
    flushing: BTreeMap<u64, Data>,
    /// `(seq, RH)` of the tree's root record.
    root: (u64, Hash),
    /// The lease's token (a writer).
    token: u64,
    /// The writer lost its lease, or saw another writer's root.
    lost: bool,
    /// A reader's root moved.
    changed: bool,
    closed: bool,
    last_write: Instant,
    idle_armed: bool,
    renewals_paused: bool,
    /// When the last lease request that held it was sent.
    renewed: Instant,
    /// A commit whose outcome stayed unknown: `(RH before, RH it wrote)`.
    /// The next read or commit settles it from the stored root first.
    unknown: Option<(Hash, Hash)>,
}

struct Inner {
    fs: UnlockedFs,
    integrity: Integrity,
    keys: VolumeKeys,
    vol_id: Vec<u8>,
    blocks: u64,
    writer: bool,
    opts: VolumeOptions,
    tree: tokio::sync::RwLock<Tree>,
    st: Mutex<State>,
    flush_lock: tokio::sync::Mutex<()>,
    /// Held by a write or discard from its edge reads until its blocks are
    /// buffered, so two partial writes into one block can't lose each other.
    write_lock: tokio::sync::Mutex<()>,
    lease: tokio::sync::Mutex<Option<Lease>>,
}

/// A block volume of an fs (cheap to clone: clones share it).
#[derive(Clone)]
pub struct Volume(Arc<Inner>);

enum Mode {
    Create(u64),
    Open,
    ReadOnly,
}

fn err(code: &'static str, m: impl Into<String>) -> Error {
    Error::local(code, m)
}

fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS RNG");
    b
}

/// Raw stored values of `keys` (sealed), in order.
async fn get_raw(fs: &UnlockedFs, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>> {
    let max = (fs.session().info().limits.max_range_items as usize).clamp(1, 1024);
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(max) {
        let r: KvItems = fs
            .session()
            .call(
                "/v1/kv/get",
                &KvGet {
                    fs: fs.id(),
                    keys: chunk.iter().cloned().map(ByteBuf::from).collect(),
                    read_version: None,
                },
            )
            .await?;
        if r.items.len() != chunk.len() {
            return Err(err("format", "a get answered with another number of items"));
        }
        out.extend(r.items.into_iter().map(|it| it.value));
    }
    Ok(out)
}

/// The volume's root record and its `RH`, as stored now.
async fn read_root(fs: &UnlockedFs, keys: &VolumeKeys) -> Result<Option<(RootRecord, Hash)>> {
    let key = keys.root();
    let Some(sealed) = get_raw(fs, std::slice::from_ref(&key))
        .await?
        .pop()
        .flatten()
    else {
        return Ok(None);
    };
    let plain = fs.open(&key, &sealed)?;
    let r = RootRecord::decode(&plain).map_err(|_| err("format", "not a volume root record"))?;
    Ok(Some((r, blk::root_hash(&plain))))
}

fn vol_entry(vol: &[u8], seq: u64, hash: Hash) -> Entry {
    Entry::Vol {
        vol: vol.to_vec(),
        seq,
        hash,
    }
}

impl Volume {
    /// Create volume `name` (1–64 bytes) of `size` bytes (a multiple of
    /// 4,096) and open it as its writer: `exists` if it does.
    pub async fn create(
        fs: &UnlockedFs,
        name: &[u8],
        size: u64,
        o: VolumeOptions,
    ) -> Result<Volume> {
        Self::open_as(fs, name, o, Mode::Create(size)).await
    }

    /// Open volume `name` as its writer: `not_found`, or `busy` while
    /// another device holds its lease.
    pub async fn open(fs: &UnlockedFs, name: &[u8], o: VolumeOptions) -> Result<Volume> {
        Self::open_as(fs, name, o, Mode::Open).await
    }

    /// Open volume `name` to read it, at its current root. Once a writer
    /// moves the root, reads of what it changed fail with `volume_changed`.
    pub async fn open_read_only(fs: &UnlockedFs, name: &[u8], o: VolumeOptions) -> Result<Volume> {
        Self::open_as(fs, name, o, Mode::ReadOnly).await
    }

    async fn open_as(fs: &UnlockedFs, name: &[u8], o: VolumeOptions, mode: Mode) -> Result<Volume> {
        if name.is_empty() || name.len() > 64 {
            return Err(err("param", "a volume name is 1 to 64 bytes"));
        }
        if o.batch_blocks == 0 {
            return Err(err("param", "batch_blocks is at least 1"));
        }
        let writer = !matches!(mode, Mode::ReadOnly);
        let integrity = fs.integrity().ok_or_else(|| {
            err(
                "integrity_needed",
                "a volume needs integrity attached to its fs",
            )
        })?;
        if writer && integrity.device_fp().is_none() {
            return Err(err("integrity_needed", "a volume's writer needs a signer"));
        }
        let keys = VolumeKeys::new(&fs.keys(), name);
        // The floors first: a sync can take long, and needs no lease.
        integrity.sync().await?;
        let acquired = Instant::now();
        let lease = if writer {
            let ttl = u32::try_from(o.lease_ttl.as_millis()).unwrap_or(u32::MAX);
            match Lease::acquire(fs.fs(), &keys.lease(), ttl).await {
                Ok(l) => Some(l),
                Err(e) if e.is("not_leader") => {
                    return Err(err("busy", "another writer holds the volume"));
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let token = lease.as_ref().map_or(0, Lease::token);
        match Self::verify_open(fs, &integrity, &keys, &o, &mode, token).await {
            Ok((record, rh)) => {
                let v = Volume(Arc::new(Inner {
                    fs: fs.clone(),
                    integrity,
                    vol_id: keys.vol_id(),
                    keys,
                    blocks: record.blocks,
                    writer,
                    st: Mutex::new(State {
                        dirty: BTreeMap::new(),
                        flushing: BTreeMap::new(),
                        root: (record.seq, rh),
                        token,
                        lost: false,
                        changed: false,
                        closed: false,
                        last_write: Instant::now(),
                        idle_armed: false,
                        renewals_paused: false,
                        unknown: None,
                        renewed: acquired,
                    }),
                    tree: tokio::sync::RwLock::new(Tree::new(record, rh, o.cache_nodes)),
                    opts: o,
                    flush_lock: tokio::sync::Mutex::new(()),
                    write_lock: tokio::sync::Mutex::new(()),
                    lease: tokio::sync::Mutex::new(lease),
                }));
                if writer {
                    v.renewals();
                }
                Ok(v)
            }
            Err(e) => {
                if let Some(l) = lease {
                    let _ = l.release().await;
                }
                Err(e)
            }
        }
    }

    /// Read (or create) the root record and check it against the floor
    /// and the local pin (spec/blk.md §6).
    async fn verify_open(
        fs: &UnlockedFs,
        integrity: &Integrity,
        keys: &VolumeKeys,
        o: &VolumeOptions,
        mode: &Mode,
        token: u64,
    ) -> Result<(RootRecord, Hash)> {
        let vol = keys.vol_id();
        let alarm = |kind, detail: String| {
            integrity.raise(Alarm {
                kind,
                object: AlarmObject::Vol,
                id: Some(vol.clone()),
                detail,
            })
        };
        let floor_at = integrity.floor(&vol_entry(&vol, 0, ZERO));
        let floor = match &floor_at {
            Some(Entry::Vol { seq, hash, .. }) => Some((*seq, *hash)),
            _ => None,
        };
        let pinned = match &o.pin {
            Some(p) => pin::load(p, &vol)?,
            None => None,
        };
        let mut found = read_root(fs, keys).await?;
        if let Mode::Create(size) = *mode {
            if found.is_some() {
                return Err(err("exists", "the volume exists"));
            }
            if size == 0 || !size.is_multiple_of(BLOCK_SIZE as u64) {
                return Err(err(
                    "param",
                    "a volume's size is a positive multiple of 4,096",
                ));
            }
            if floor.is_none() && pinned.is_none() {
                let record = RootRecord {
                    seq: 1,
                    prev: ZERO,
                    blocks: size / BLOCK_SIZE as u64,
                    root: ZERO,
                    writer: integrity.device_fp().expect("a writer's signer"),
                    hlc: fs.session().hlc(),
                };
                let key = keys.root();
                let sealed =
                    seal::seal_value(&*fs.seal_keys()?, &key, &record.encode(), &mut OsRng)?;
                let c = Commit {
                    commit_id: random16().to_vec(),
                    expect: vec![Expect {
                        fs: fs.id(),
                        key: key.clone(),
                        version: None,
                    }],
                    writes: vec![Write {
                        fs: fs.id(),
                        key,
                        value: Some(sealed),
                    }],
                    fence: vec![Fence {
                        fs: fs.id(),
                        name: keys.lease(),
                        token,
                    }],
                    ..Default::default()
                };
                match send_commit(fs, &c, 4).await {
                    Ok(_) => {}
                    Err(e) if e.is("conflict") => return Err(err("exists", "the volume exists")),
                    Err(e) => return Err(e),
                }
                let rh = record.hash();
                integrity.note(vol_entry(&vol, 1, rh), true)?;
                if let Some(p) = &o.pin {
                    pin::save(p, &vol, &Pin { seq: 1, hash: rh })?;
                }
                return Ok((record, rh));
            }
            found = None;
        }
        let Some((record, rh)) = found else {
            if let Some((s, _)) = floor {
                return Err(alarm(
                    AlarmKind::Rollback,
                    format!("the volume's root is missing, below the checkpointed seq {s}"),
                ));
            }
            if let Some(p) = pinned {
                return Err(alarm(
                    AlarmKind::Rollback,
                    format!(
                        "the volume's root is missing, below the pinned seq {}",
                        p.seq
                    ),
                ));
            }
            return Err(err("not_found", "no such volume"));
        };
        if let Some((s, h)) = floor
            && record.seq == s + 1
            && record.prev != h
        {
            return Err(alarm(
                AlarmKind::Fork,
                format!(
                    "root {} doesn't follow the checkpointed root {s}",
                    record.seq
                ),
            ));
        }
        integrity.observe_at(vol_entry(&vol, record.seq, rh), floor_at, false)?;
        if let Some(p) = pinned {
            if record.seq < p.seq {
                return Err(alarm(
                    AlarmKind::Rollback,
                    format!("root {} is below the pinned root {}", record.seq, p.seq),
                ));
            }
            if record.seq == p.seq && rh != p.hash {
                return Err(alarm(
                    AlarmKind::Fork,
                    format!("another root at the pinned seq {}", p.seq),
                ));
            }
            if record.seq == p.seq + 1 && record.prev != p.hash {
                return Err(alarm(
                    AlarmKind::Fork,
                    format!(
                        "root {} doesn't follow the pinned root {}",
                        record.seq, p.seq
                    ),
                ));
            }
        }
        if let Some(p) = &o.pin
            && pinned.is_none_or(|x| x.seq < record.seq)
        {
            pin::save(
                p,
                &vol,
                &Pin {
                    seq: record.seq,
                    hash: rh,
                },
            )?;
        }
        Ok((record, rh))
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.st.lock().expect("volume state")
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.0.blocks * BLOCK_SIZE as u64
    }

    /// The volume id: the stored key of `("blk", name)`, its checkpoint entry's id.
    pub fn vol_id(&self) -> &[u8] {
        &self.0.vol_id
    }

    /// The root this volume holds: `(seq, RH)`.
    pub fn root(&self) -> (u64, Hash) {
        self.st().root
    }

    /// Whether this is the writer.
    pub fn is_writer(&self) -> bool {
        self.0.writer
    }

    /// Stop (or resume) renewing the lease, for tests of a stale fence.
    #[doc(hidden)]
    pub fn pause_renewals(&self, paused: bool) {
        self.st().renewals_paused = paused;
    }

    fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size() => Ok(()),
            _ => Err(err("range", "past the end of the volume")),
        }
    }

    fn writable(&self) -> Result<()> {
        let st = self.st();
        if !self.0.writer {
            return Err(err("read_only", "the volume is open read-only"));
        }
        if st.closed {
            return Err(err("closed", "the volume is closed"));
        }
        if st.lost {
            return Err(err("lease_lost", "the writer lost its lease"));
        }
        Ok(())
    }

    // -------------------------------------------------------------- reading

    /// Read `buf.len()` bytes at `offset`, checked against the tree.
    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(offset, buf.len() as u64)?;
        if buf.is_empty() {
            return Ok(());
        }
        let bs = BLOCK_SIZE as u64;
        let first = offset / bs;
        let last = (offset + buf.len() as u64 - 1) / bs;
        let idx: Vec<u64> = (first..=last).collect();
        let data = self.read_blocks(&idx).await?;
        let mut at = 0usize;
        for (i, d) in idx.iter().zip(data) {
            let from = if *i == first {
                (offset % bs) as usize
            } else {
                0
            };
            let n = (BLOCK_SIZE - from).min(buf.len() - at);
            match d {
                Some(d) => buf[at..at + n].copy_from_slice(&d[from..from + n]),
                None => buf[at..at + n].fill(0),
            }
            at += n;
        }
        Ok(())
    }

    /// The plaintexts of blocks (`None`: zeros), newest writes first.
    async fn read_blocks(&self, idx: &[u64]) -> Result<Vec<Data>> {
        if self.st().unknown.is_some() {
            let mut tree = self.0.tree.write().await;
            self.settle(&mut tree).await?;
        }
        let tree = self.0.tree.read().await;
        let mut out: Vec<Data> = vec![None; idx.len()];
        let mut need = Vec::new();
        {
            let st = self.st();
            if st.changed {
                return Err(err(
                    "volume_changed",
                    "the volume changed under this reader",
                ));
            }
            for (k, i) in idx.iter().enumerate() {
                match st.dirty.get(i).or_else(|| st.flushing.get(i)) {
                    Some(d) => out[k] = d.clone(),
                    None => need.push(k),
                }
            }
        }
        for chunk in need.chunks(256) {
            let want: Vec<u64> = chunk.iter().map(|&k| idx[k]).collect();
            let fetched = match self.resolve(&tree, &want, true).await? {
                Ok(f) => f,
                Err(why) => {
                    drop(tree);
                    return Err(self.mismatch(why).await);
                }
            };
            for (&k, i) in chunk.iter().zip(&want) {
                if let Some(Some(sealed)) = fetched.blocks.get(i) {
                    let key = self.0.keys.block(*i);
                    let plain = self.0.fs.open(&key, sealed)?;
                    if plain.len() != BLOCK_SIZE {
                        return Err(err("format", format!("block {i} isn't 4,096 bytes")));
                    }
                    out[k] = Some(Arc::new(plain));
                }
            }
        }
        Ok(out)
    }

    /// Fetch what `blocks` need (their nodes, and with `with_blocks` the
    /// blocks themselves) and check them against the tree: the outer error
    /// is the transport's, the inner one a mismatch.
    async fn resolve(
        &self,
        tree: &Tree,
        blocks: &[u64],
        with_blocks: bool,
    ) -> Result<std::result::Result<Fetched, String>> {
        let mut plan = tree.plan(blocks);
        let ask_blocks: Vec<u64> = if with_blocks {
            blocks
                .iter()
                .copied()
                .filter(|i| !plan.absent.contains(i))
                .collect()
        } else {
            Vec::new()
        };
        let mut keys: Vec<Vec<u8>> = plan
            .need
            .iter()
            .map(|&(l, j)| self.0.keys.node(l, j))
            .collect();
        keys.extend(ask_blocks.iter().map(|&i| self.0.keys.block(i)));
        let values = if keys.is_empty() {
            Vec::new()
        } else {
            get_raw(&self.0.fs, &keys).await?
        };
        let mut fetched: HashMap<Pos, Option<Node>> = HashMap::new();
        for (&pos, (k, v)) in plan.need.iter().zip(keys.iter().zip(&values)) {
            let node = match v {
                Some(sealed) => {
                    let plain = self.0.fs.open(k, sealed)?;
                    match node_from_bytes(&plain) {
                        Ok(n) => Some(n),
                        Err(_) => {
                            return Ok(Err(format!("node ({}, {}) isn't a node", pos.0, pos.1)));
                        }
                    }
                }
                None => None,
            };
            fetched.insert(pos, node);
        }
        let stored: HashMap<u64, Option<Vec<u8>>> = ask_blocks
            .iter()
            .copied()
            .zip(values[plan.need.len()..].iter().cloned())
            .collect();
        let mut verified = Vec::new();
        for &i in blocks {
            if plan.absent.contains(&i) {
                continue;
            }
            let b = with_blocks.then(|| stored.get(&i).and_then(|b| b.as_deref()));
            if let Err(why) = tree.verify(i, &mut plan, &fetched, b, &mut verified) {
                return Ok(Err(why));
            }
        }
        tree.keep(verified);
        Ok(Ok(Fetched {
            blocks: stored,
            nodes: plan,
        }))
    }

    /// A read or path that doesn't match the tree (spec/blk.md §5).
    async fn mismatch(&self, why: String) -> Error {
        let held = self.st().root.1;
        match read_root(&self.0.fs, &self.0.keys).await {
            Ok(Some((_, rh))) if rh != held => {
                let mut st = self.st();
                if self.0.writer {
                    st.lost = true;
                    err("lease_lost", "another writer moved the volume's root")
                } else {
                    st.changed = true;
                    err("volume_changed", "the volume changed under this reader")
                }
            }
            Ok(_) => self.0.integrity.raise(Alarm {
                kind: AlarmKind::Rollback,
                object: AlarmObject::Vol,
                id: Some(self.0.vol_id.clone()),
                detail: format!("{why}, at the same root"),
            }),
            Err(e) => e,
        }
    }

    // -------------------------------------------------------------- writing

    /// Write `data` at `offset` (buffered until a flush). Writes are
    /// ordered: each one's blocks are buffered before the next one starts.
    pub async fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.writable()?;
        self.check_range(offset, data.len() as u64)?;
        if data.is_empty() {
            return Ok(());
        }
        let full = {
            let _order = self.0.write_lock.lock().await;
            self.write_locked(offset, data).await?
        };
        if full { self.flush().await } else { Ok(()) }
    }

    /// [`Volume::write`] under the write lock: whether the batch is full.
    async fn write_locked(&self, offset: u64, data: &[u8]) -> Result<bool> {
        let bs = BLOCK_SIZE as u64;
        let first = offset / bs;
        let last = (offset + data.len() as u64 - 1) / bs;
        let mut blocks: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        // The edges a write covers in part are read first.
        let mut edges = Vec::new();
        if !offset.is_multiple_of(bs) || (first == last && data.len() < BLOCK_SIZE) {
            edges.push(first);
        }
        let end = offset + data.len() as u64;
        if last != first && !end.is_multiple_of(bs) {
            edges.push(last);
        }
        if !edges.is_empty() {
            for (i, d) in edges.iter().zip(self.read_blocks(&edges).await?) {
                blocks.insert(*i, d.map_or_else(|| vec![0; BLOCK_SIZE], |d| (*d).clone()));
            }
        }
        for i in first..=last {
            let b = blocks.entry(i).or_insert_with(|| vec![0; BLOCK_SIZE]);
            let start = (i * bs).max(offset);
            let stop = ((i + 1) * bs).min(end);
            b[(start - i * bs) as usize..(stop - i * bs) as usize]
                .copy_from_slice(&data[(start - offset) as usize..(stop - offset) as usize]);
        }
        Ok(self.buffer(blocks.into_iter().map(|(i, b)| (i, Some(Arc::new(b))))))
    }

    /// Discard `[offset, offset + len)`: whole blocks are deleted (they read
    /// as zeros), the edges are written with zeros.
    pub async fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.writable()?;
        self.check_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        let bs = BLOCK_SIZE as u64;
        let end = offset + len;
        let whole_from = offset.div_ceil(bs);
        let whole_to = end / bs;
        let full = {
            let _order = self.0.write_lock.lock().await;
            if whole_from >= whole_to {
                self.write_locked(offset, &vec![0; len as usize]).await?
            } else {
                let mut full = false;
                if offset < whole_from * bs {
                    full |= self
                        .write_locked(offset, &vec![0; (whole_from * bs - offset) as usize])
                        .await?;
                }
                if end > whole_to * bs {
                    full |= self
                        .write_locked(whole_to * bs, &vec![0; (end - whole_to * bs) as usize])
                        .await?;
                }
                full | self.buffer((whole_from..whole_to).map(|i| (i, None)))
            }
        };
        if full { self.flush().await } else { Ok(()) }
    }

    /// Buffer blocks: whether the batch is full.
    fn buffer(&self, blocks: impl Iterator<Item = (u64, Data)>) -> bool {
        let mut st = self.st();
        st.dirty.extend(blocks);
        st.last_write = Instant::now();
        self.arm_idle(&mut st);
        st.dirty.len() >= self.0.opts.batch_blocks
    }

    fn arm_idle(&self, st: &mut State) {
        let Some(idle) = self.0.opts.idle_flush else {
            return;
        };
        if st.idle_armed {
            return;
        }
        st.idle_armed = true;
        let weak: Weak<Inner> = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                let wait = {
                    let Some(inner) = weak.upgrade() else { return };
                    let st = inner.st.lock().expect("volume state");
                    if st.closed {
                        return;
                    }
                    idle.checked_sub(st.last_write.elapsed())
                        .filter(|d| !d.is_zero())
                };
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break,
                }
            }
            let Some(inner) = weak.upgrade() else { return };
            inner.st.lock().expect("volume state").idle_armed = false;
            let _ = Volume(inner).flush().await;
        });
    }

    /// Commit every buffered write; returns once they applied.
    pub async fn flush(&self) -> Result<()> {
        self.writable()?;
        let _one = self.0.flush_lock.lock().await;
        let items: Vec<(u64, Data)> = {
            let mut st = self.st();
            let d = std::mem::take(&mut st.dirty);
            st.flushing = d.clone();
            d.into_iter().collect()
        };
        let per = self.per_commit();
        for (n, chunk) in items.chunks(per).enumerate() {
            if let Err(e) = self.commit(chunk).await {
                // What didn't commit goes back, under newer writes.
                let mut st = self.st();
                for (i, d) in items[n * per..].iter() {
                    st.dirty.entry(*i).or_insert_with(|| d.clone());
                }
                st.flushing.clear();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Blocks per commit: `batch_blocks`, within the server's commit limits
    /// for a block and a full path of new nodes each.
    fn per_commit(&self) -> usize {
        let l = &self.0.fs.session().info().limits;
        let d = blk::depth(self.0.blocks) as usize;
        let per_block = (BLOCK_SIZE + 64 + 512 + 64) * (1 + d);
        let by_bytes = (l.max_commit_bytes as usize).saturating_sub(16_384) / per_block;
        let by_ops = (l.max_commit_ops as usize).saturating_sub(4) / (1 + d);
        self.0.opts.batch_blocks.min(by_bytes).min(by_ops).max(1)
    }

    /// One commit: the blocks, their changed nodes, the next root (spec/blk.md §4).
    async fn commit(&self, chunk: &[(u64, Data)]) -> Result<()> {
        let mut tree = self.0.tree.write().await;
        self.settle(&mut tree).await?;
        let idx: Vec<u64> = chunk.iter().map(|(i, _)| *i).collect();
        let fetched = match self.resolve(&tree, &idx, false).await? {
            Ok(f) => f,
            Err(why) => {
                drop(tree);
                return Err(self.mismatch(why).await);
            }
        };
        // A delete of a block the tree shows absent changes nothing (mkfs
        // discards a whole device): left out, and a chunk of nothing else
        // commits nothing.
        let chunk: Vec<&(u64, Data)> = chunk
            .iter()
            .filter(|(i, d)| d.is_some() || tree.leaf_in(*i, &fetched.nodes.have) != ZERO)
            .collect();
        if chunk.is_empty() {
            drop(tree);
            let mut st = self.st();
            for i in &idx {
                st.flushing.remove(i);
            }
            return Ok(());
        }
        let fs = &self.0.fs;
        let keys = &self.0.keys;
        let seal_keys = fs.seal_keys()?;
        let mut writes = Vec::new();
        let mut leaves = BTreeMap::new();
        for (i, d) in chunk {
            let key = keys.block(*i);
            match d {
                Some(d) => {
                    let sealed = seal::seal_value(&seal_keys, &key, d, &mut OsRng)?;
                    leaves.insert(*i, blk::leaf(*i, &sealed));
                    writes.push(Write {
                        fs: fs.id(),
                        key,
                        value: Some(sealed),
                    });
                }
                None => {
                    leaves.insert(*i, ZERO);
                    writes.push(Write {
                        fs: fs.id(),
                        key,
                        value: None,
                    });
                }
            }
        }
        let applied = tree.apply(&fetched.nodes.have, &leaves);
        for ((l, j), n) in &applied.set {
            let key = keys.node(*l, *j);
            let sealed = seal::seal_value(&seal_keys, &key, &node_bytes(n), &mut OsRng)?;
            writes.push(Write {
                fs: fs.id(),
                key,
                value: Some(sealed),
            });
        }
        for (l, j) in &applied.delete {
            writes.push(Write {
                fs: fs.id(),
                key: keys.node(*l, *j),
                value: None,
            });
        }
        let record = RootRecord {
            seq: tree.record.seq + 1,
            prev: tree.rh,
            blocks: self.0.blocks,
            root: applied.root,
            writer: self.0.integrity.device_fp().expect("a writer's signer"),
            hlc: fs.session().hlc(),
        };
        let rh = record.hash();
        let root_key = keys.root();
        writes.push(Write {
            fs: fs.id(),
            value: Some(seal::seal_value(
                &seal_keys,
                &root_key,
                &record.encode(),
                &mut OsRng,
            )?),
            key: root_key,
        });
        let token = self.st().token;
        let c = Commit {
            commit_id: random16().to_vec(),
            writes,
            fence: vec![Fence {
                fs: fs.id(),
                name: keys.lease(),
                token,
            }],
            ..Default::default()
        };
        self.send(&c, tree.rh, rh).await?;
        let seq = record.seq;
        tree.advance(applied, record, rh);
        {
            let mut st = self.st();
            st.root = (seq, rh);
            for i in &idx {
                st.flushing.remove(i);
            }
        }
        drop(tree);
        if let Some(p) = &self.0.opts.pin {
            pin::save(p, &self.0.vol_id, &Pin { seq, hash: rh })?;
        }
        self.0
            .integrity
            .note(vol_entry(&self.0.vol_id, seq, rh), true)
    }

    /// Settle a commit whose outcome stayed unknown, from the stored root:
    /// the root before it means it didn't apply (its blocks are buffered
    /// again); the root it wrote means it did, so the tree starts from that
    /// root (the buffered blocks are written again on top); any other root
    /// is another writer's. Without this, the next commit would write a
    /// second root record at the same seq.
    async fn settle(&self, tree: &mut Tree) -> Result<()> {
        let Some((old, new)) = self.st().unknown else {
            return Ok(());
        };
        match read_root(&self.0.fs, &self.0.keys).await? {
            Some((_, rh)) if rh == old => {}
            Some((record, rh)) if rh == new => {
                let seq = record.seq;
                *tree = Tree::new(record, rh, self.0.opts.cache_nodes);
                self.st().root = (seq, rh);
                if let Some(p) = &self.0.opts.pin {
                    pin::save(p, &self.0.vol_id, &Pin { seq, hash: rh })?;
                }
                self.0
                    .integrity
                    .note(vol_entry(&self.0.vol_id, seq, rh), true)?;
            }
            _ => {
                let mut st = self.st();
                st.lost = true;
                st.unknown = None;
                return Err(err("lease_lost", "another writer moved the volume's root"));
            }
        }
        self.st().unknown = None;
        Ok(())
    }

    /// Send a commit; on an unknown outcome, the root record says whether
    /// it applied (spec/blk.md §4), and when it can't be read either, the
    /// next read or commit settles it ([`Volume::settle`]).
    async fn send(&self, c: &Commit, old: Hash, new: Hash) -> Result<()> {
        let mut last = match send_commit(&self.0.fs, c, 4).await {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        for _ in 0..3 {
            if last.is("claim_lost") {
                self.st().lost = true;
                return Err(err("lease_lost", "the writer lost its lease"));
            }
            if !(last.is("network") || last.is("commit_unknown")) {
                return Err(last);
            }
            match read_root(&self.0.fs, &self.0.keys).await {
                Ok(Some((_, rh))) if rh == new => return Ok(()),
                Ok(Some((_, rh))) if rh == old => {}
                Ok(_) => {
                    self.st().lost = true;
                    return Err(err("lease_lost", "another writer moved the volume's root"));
                }
                Err(e) => {
                    last = e;
                    continue;
                }
            }
            last = match send_commit(&self.0.fs, c, 4).await {
                Ok(_) => return Ok(()),
                Err(e) => e,
            };
        }
        if last.is("network") || last.is("commit_unknown") {
            self.st().unknown = Some((old, new));
        }
        Err(last)
    }

    /// Renew the lease every third of its ttl, counted from when the last
    /// request that held it was sent (a renewal the server answers later
    /// only holds it longer). A renewal that gets another token came after
    /// the lease lapsed, when another writer may have written: lost.
    fn renewals(&self) {
        let weak: Weak<Inner> = Arc::downgrade(&self.0);
        let every = (self.0.opts.lease_ttl / 3).max(Duration::from_millis(30));
        tokio::spawn(async move {
            loop {
                let due = {
                    let Some(inner) = weak.upgrade() else { return };
                    let st = inner.st.lock().expect("volume state");
                    if st.closed || st.lost {
                        return;
                    }
                    (st.renewed + every).saturating_duration_since(Instant::now())
                };
                tokio::time::sleep(due).await;
                let Some(inner) = weak.upgrade() else { return };
                let token = {
                    let st = inner.st.lock().expect("volume state");
                    if st.closed || st.lost {
                        return;
                    }
                    (!st.renewals_paused).then_some(st.token)
                };
                let Some(token) = token else {
                    tokio::time::sleep(every).await;
                    continue;
                };
                let mut l = inner.lease.lock().await;
                let Some(lease) = l.as_mut() else { return };
                let sent = Instant::now();
                match lease.renew().await {
                    Ok(()) if lease.token() == token => {
                        inner.st.lock().expect("volume state").renewed = sent;
                    }
                    Ok(()) => {
                        inner.st.lock().expect("volume state").lost = true;
                        return;
                    }
                    Err(e) if e.is("not_leader") => {
                        inner.st.lock().expect("volume state").lost = true;
                        return;
                    }
                    // A transient failure: try again soon, within the ttl.
                    Err(_) => {
                        drop(l);
                        tokio::time::sleep(every / 4).await;
                    }
                }
            }
        });
    }

    /// Flush, write a checkpoint, and release the lease.
    pub async fn close(self) -> Result<()> {
        let mut out = Ok(());
        if self.0.writer && !self.st().lost {
            out = self.flush().await;
        }
        let ck = self.0.integrity.checkpoint().await.map(|_| ());
        self.st().closed = true;
        if let Some(l) = self.0.lease.lock().await.take()
            && !self.st().lost
        {
            let _ = l.release().await;
        }
        out.and(ck)
    }
}

/// What [`Volume::resolve`] fetched.
struct Fetched {
    /// The stored bytes of the blocks asked for (`None`: absent).
    blocks: HashMap<u64, Option<Vec<u8>>>,
    /// The plan, with every verified node in `have`.
    nodes: Plan,
}
