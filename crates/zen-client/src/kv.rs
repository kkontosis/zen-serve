//! KV reads and transactions (spec/api.md §5–6), as in the TS client:
//! short mode (one read version, read-conflict ranges) or long mode (every
//! read an `expect`, ranges hashed), commits replayed with one `commit_id`.

use crate::error::{Error, Result};
use crate::fs::UnlockedFs;
use crate::lease::Lease;
use std::collections::BTreeMap;
use std::time::Duration;
use zen_core::kdf::RangeHasher;
use zen_core::rng::OsRng;
use zen_core::seal;
use zen_proto::{
    ByteBuf, Commit, CommitResult, Expect, ExpectRange, Fence, FsRange, KvGet, KvItems, KvRange,
    Write,
};

/// One decrypted KV item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvEntry {
    /// The stored key (PRF tokens, 16 bytes per path element).
    pub key: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
    /// The value's version (versionstamp); zeros for a transaction's own write.
    pub version: Vec<u8>,
}

/// Range options.
#[derive(Debug, Clone, Copy, Default)]
pub struct RangeOptions {
    /// At most this many items.
    pub limit: Option<u32>,
    /// Newest key first.
    pub reverse: bool,
}

/// The smallest key after every key starting with `prefix` (`None`: no bound).
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last < 0xFF {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}

fn key_after(k: &[u8]) -> Vec<u8> {
    let mut v = k.to_vec();
    v.push(0);
    v
}

fn in_range(k: &[u8], begin: &[u8], end: Option<&[u8]>) -> bool {
    k >= begin && end.is_none_or(|e| k < e)
}

fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS RNG");
    b
}

async fn sleep_backoff(i: u32) {
    let ms = (10u64 << i.min(7)).min(1000);
    let jitter = u64::from(random16()[0]) * ms / 255;
    tokio::time::sleep(Duration::from_millis(ms / 2 + jitter)).await;
}

/// Send a commit, replaying it with the same `commit_id` on a network error
/// or `commit_unknown` (`attempts` in all).
pub async fn send_commit(fs: &UnlockedFs, c: &Commit, attempts: u32) -> Result<CommitResult> {
    for i in 1.. {
        match fs.session().call("/v1/commit", c).await {
            Err(e) if (e.is("network") || e.is("commit_unknown")) && i < attempts => {
                tokio::time::sleep(Duration::from_millis(50 << i)).await;
            }
            r => return r,
        }
    }
    unreachable!()
}

impl UnlockedFs {
    /// The stored key of a path (formats.md §3.1).
    pub fn key<E: AsRef<[u8]>>(&self, path: &[E]) -> Vec<u8> {
        zen_core::token::kv_key(&self.keys(), path)
    }

    /// Open a stored value (its key is the AAD context).
    pub fn open(&self, key: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        Ok(seal::open_value(&*self.keys_for(sealed)?, key, sealed)?)
    }

    /// Read one value by stored key.
    pub async fn get(&self, key: &[u8]) -> Result<Option<KvEntry>> {
        Ok(self.get_many(&[key.to_vec()]).await?.pop().flatten())
    }

    /// Read several values at one snapshot.
    pub async fn get_many(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<KvEntry>>> {
        let r: KvItems = self
            .session()
            .call(
                "/v1/kv/get",
                &KvGet {
                    fs: self.id(),
                    keys: keys.iter().cloned().map(ByteBuf::from).collect(),
                    read_version: None,
                },
            )
            .await?;
        r.items
            .into_iter()
            .map(|it| match (it.value, it.version) {
                (Some(v), Some(version)) => Ok(Some(KvEntry {
                    value: self.open(&it.key, &v)?,
                    key: it.key,
                    version,
                })),
                _ => Ok(None),
            })
            .collect()
    }

    /// Every entry of the stored-key range `[begin, end)`, paging through `more`.
    pub async fn range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
        opts: RangeOptions,
    ) -> Result<Vec<KvEntry>> {
        let mut out = Vec::new();
        let (mut lo, mut hi) = (begin.to_vec(), end.map(<[u8]>::to_vec));
        let mut left = opts.limit;
        loop {
            if left == Some(0) {
                return Ok(out);
            }
            let r: KvItems = self
                .session()
                .call(
                    "/v1/kv/range",
                    &KvRange {
                        fs: self.id(),
                        begin: lo.clone(),
                        end: hi.clone(),
                        limit: left,
                        reverse: opts.reverse.then_some(true),
                        read_version: None,
                    },
                )
                .await?;
            let last = r.items.last().map(|it| it.key.clone());
            for it in r.items {
                let v = it
                    .value
                    .ok_or_else(|| Error::format("a range item without a value"))?;
                out.push(KvEntry {
                    value: self.open(&it.key, &v)?,
                    key: it.key,
                    version: it.version.unwrap_or_default(),
                });
                left = left.map(|l| l - 1);
            }
            let Some(last) = last else { return Ok(out) };
            if !r.more {
                return Ok(out);
            }
            if opts.reverse {
                hi = Some(last);
            } else {
                lo = key_after(&last);
            }
        }
    }

    /// Every entry under a stored-key prefix (empty: the whole fs).
    pub async fn range_prefix(&self, prefix: &[u8], opts: RangeOptions) -> Result<Vec<KvEntry>> {
        self.range(prefix, prefix_end(prefix).as_deref(), opts)
            .await
    }

    /// Write one value (a one-write transaction).
    pub async fn set(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.transaction(TxnOptions::default(), async |tx: &mut Transaction| {
            tx.set(key, value)
        })
        .await
    }

    /// Delete one value.
    pub async fn delete(&self, key: &[u8]) -> Result<()> {
        self.transaction(TxnOptions::default(), async |tx: &mut Transaction| {
            tx.delete(key);
            Ok(())
        })
        .await
    }

    /// Run `f` in a transaction and commit it, re-running it on `conflict`
    /// or `too_old` (up to `opts.attempts`). A transaction that only reads
    /// commits nothing.
    pub async fn transaction<T>(
        &self,
        opts: TxnOptions,
        mut f: impl AsyncFnMut(&mut Transaction) -> Result<T>,
    ) -> Result<T> {
        for i in 1.. {
            let mut tx = Transaction::new(self.clone(), opts.mode);
            let out = match f(&mut tx).await {
                Ok(v) => v,
                Err(e) if e.retryable() && i < opts.attempts => {
                    sleep_backoff(i).await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            match tx.commit().await {
                Ok(_) => return Ok(out),
                Err(e) if e.retryable() && i < opts.attempts => sleep_backoff(i).await,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

/// How a transaction reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// One read version, serializable through read-conflict ranges; valid
    /// for about 5 s.
    #[default]
    Short,
    /// No time limit: every read is an `expect` (ranges: a hash) checked at commit.
    Long,
}

/// Transaction options.
#[derive(Debug, Clone, Copy)]
pub struct TxnOptions {
    /// Short (default) or long mode.
    pub mode: Mode,
    /// Attempts on `conflict` / `too_old` (default 8).
    pub attempts: u32,
}

impl Default for TxnOptions {
    fn default() -> Self {
        TxnOptions {
            mode: Mode::Short,
            attempts: 8,
        }
    }
}

type CommitHook = Box<dyn FnOnce(&CommitResult) + Send>;

/// One attempt of a transaction: reads, buffered writes, fences.
pub struct Transaction {
    fs: UnlockedFs,
    mode: Mode,
    rv0: Option<u64>,
    conflicts: Vec<FsRange>,
    expects: BTreeMap<Vec<u8>, Expect>,
    expect_ranges: Vec<ExpectRange>,
    /// By key: the write and its plaintext (`None`: a delete).
    writes: BTreeMap<Vec<u8>, (Write, Option<Vec<u8>>)>,
    clears: Vec<FsRange>,
    fences: Vec<Fence>,
    commit_id: [u8; 16],
    hooks: Vec<CommitHook>,
    /// The commit's result, once committed.
    pub result: Option<CommitResult>,
}

impl Transaction {
    /// A transaction on `fs` (one attempt; [`UnlockedFs::transaction`] retries).
    pub fn new(fs: UnlockedFs, mode: Mode) -> Self {
        Transaction {
            fs,
            mode,
            rv0: None,
            conflicts: Vec::new(),
            expects: BTreeMap::new(),
            expect_ranges: Vec::new(),
            writes: BTreeMap::new(),
            clears: Vec::new(),
            fences: Vec::new(),
            commit_id: random16(),
            hooks: Vec::new(),
            result: None,
        }
    }

    /// The fs.
    pub fn fs(&self) -> &UnlockedFs {
        &self.fs
    }

    /// The read version of a short transaction, once it read.
    pub fn read_version(&self) -> Option<u64> {
        (self.mode == Mode::Short).then_some(self.rv0).flatten()
    }

    fn rv(&self) -> Option<u64> {
        self.read_version()
    }

    fn note_rv(&mut self, v: u64) {
        if self.mode == Mode::Short && self.rv0.is_none() {
            self.rv0 = Some(v);
        }
    }

    /// This transaction's own value for a key: a write, a clear, or nothing.
    fn own(&self, key: &[u8]) -> Option<Option<Vec<u8>>> {
        if let Some((_, plain)) = self.writes.get(key) {
            return Some(plain.clone());
        }
        if self
            .clears
            .iter()
            .any(|c| in_range(key, &c.begin, c.end.as_deref()))
        {
            return Some(None);
        }
        None
    }

    /// Read a value (sees this transaction's own writes).
    pub async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .get_many(&[key.to_vec()])
            .await?
            .pop()
            .flatten()
            .map(|e| e.value))
    }

    /// Read several values in one request, each recorded: a read conflict
    /// in short mode, an `expect` in long mode.
    pub async fn get_many(&mut self, keys: &[Vec<u8>]) -> Result<Vec<Option<KvEntry>>> {
        let mut out: Vec<Option<KvEntry>> = vec![None; keys.len()];
        let mut need = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match self.own(k) {
                None => need.push(i),
                Some(Some(v)) => {
                    out[i] = Some(KvEntry {
                        key: k.clone(),
                        value: v,
                        version: vec![0; 10],
                    })
                }
                Some(None) => {}
            }
        }
        if need.is_empty() {
            return Ok(out);
        }
        let r: KvItems = self
            .fs
            .session()
            .call(
                "/v1/kv/get",
                &KvGet {
                    fs: self.fs.id(),
                    keys: need
                        .iter()
                        .map(|&i| ByteBuf::from(keys[i].clone()))
                        .collect(),
                    read_version: self.rv(),
                },
            )
            .await?;
        self.note_rv(r.read_version);
        for (&i, it) in need.iter().zip(r.items) {
            let key = &keys[i];
            if self.mode == Mode::Short {
                self.conflicts.push(FsRange {
                    fs: self.fs.id(),
                    begin: key.clone(),
                    end: Some(key_after(key)),
                });
            } else if !self.expects.contains_key(key) {
                self.expects.insert(
                    key.clone(),
                    Expect {
                        fs: self.fs.id(),
                        key: key.clone(),
                        version: it.version.clone(),
                    },
                );
            }
            if let (Some(v), Some(version)) = (it.value, it.version) {
                out[i] = Some(KvEntry {
                    value: self.fs.open(key, &v)?,
                    key: key.clone(),
                    version,
                });
            }
        }
        Ok(out)
    }

    /// Read values at the transaction's read version without recording
    /// them (no read conflict, no `expect`). Sees own writes.
    pub async fn snapshot_get(&mut self, keys: &[Vec<u8>]) -> Result<Vec<Option<KvEntry>>> {
        let mut out: Vec<Option<KvEntry>> = vec![None; keys.len()];
        let mut ask = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match self.own(k) {
                None => ask.push(i),
                Some(Some(v)) => {
                    out[i] = Some(KvEntry {
                        key: k.clone(),
                        value: v,
                        version: vec![0; 10],
                    })
                }
                Some(None) => {}
            }
        }
        if ask.is_empty() {
            return Ok(out);
        }
        let r: KvItems = self
            .fs
            .session()
            .call(
                "/v1/kv/get",
                &KvGet {
                    fs: self.fs.id(),
                    keys: ask
                        .iter()
                        .map(|&i| ByteBuf::from(keys[i].clone()))
                        .collect(),
                    read_version: self.rv(),
                },
            )
            .await?;
        self.note_rv(r.read_version);
        for (&i, it) in ask.iter().zip(r.items) {
            if let (Some(v), Some(version)) = (it.value, it.version) {
                out[i] = Some(KvEntry {
                    value: self.fs.open(&keys[i], &v)?,
                    key: keys[i].clone(),
                    version,
                });
            }
        }
        Ok(out)
    }

    /// Read every entry under a stored-key prefix, recorded like `get`; in
    /// long mode the range is hashed into an `expect_ranges` check, so it
    /// must fit one response.
    pub async fn range_prefix(&mut self, prefix: &[u8]) -> Result<Vec<KvEntry>> {
        let end = prefix_end(prefix);
        self.range(prefix, end.as_deref(), None).await
    }

    /// Read the stored-key range `[begin, end)`, recorded. With `limit`,
    /// only the part up to the last item read is recorded.
    pub async fn range(
        &mut self,
        begin: &[u8],
        end: Option<&[u8]>,
        limit: Option<u32>,
    ) -> Result<Vec<KvEntry>> {
        let mut out = Vec::new();
        let mut hasher = (self.mode == Mode::Long).then(RangeHasher::new);
        let mut left = limit;
        let mut lo = begin.to_vec();
        let mut hi = end.map(<[u8]>::to_vec);
        while left != Some(0) {
            let r: KvItems = self
                .fs
                .session()
                .call(
                    "/v1/kv/range",
                    &KvRange {
                        fs: self.fs.id(),
                        begin: lo.clone(),
                        end: end.map(<[u8]>::to_vec),
                        limit: left,
                        reverse: None,
                        read_version: self.rv(),
                    },
                )
                .await?;
            self.note_rv(r.read_version);
            let last = r.items.last().map(|it| it.key.clone());
            for it in r.items {
                let v = it
                    .value
                    .ok_or_else(|| Error::format("a range item without a value"))?;
                let version = it.version.unwrap_or_default();
                if let Some(h) = &mut hasher {
                    let stamp: [u8; 10] = version
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::format("a version that isn't 10 bytes"))?;
                    h.update(&it.key, &stamp);
                }
                out.push(KvEntry {
                    value: self.fs.open(&it.key, &v)?,
                    key: it.key,
                    version,
                });
                left = left.map(|l| l - 1);
            }
            let Some(last) = last else { break };
            if left == Some(0) {
                // The limit stopped the read: record the range up to the last item.
                hi = Some(key_after(&last));
                break;
            }
            if !r.more {
                break;
            }
            if hasher.is_some() {
                return Err(Error::local(
                    "too_large",
                    "a long-mode range must fit one read",
                ));
            }
            lo = key_after(&last);
        }
        let range = FsRange {
            fs: self.fs.id(),
            begin: begin.to_vec(),
            end: hi.clone(),
        };
        match hasher {
            Some(h) => self.expect_ranges.push(ExpectRange {
                fs: range.fs,
                begin: range.begin,
                end: range.end,
                hash: h.finalize().to_vec(),
            }),
            None => self.conflicts.push(range),
        }
        // Overlay this transaction's own writes and clears.
        let mut merged: BTreeMap<Vec<u8>, KvEntry> =
            out.into_iter().map(|e| (e.key.clone(), e)).collect();
        for c in &self.clears {
            merged.retain(|k, _| !in_range(k, &c.begin, c.end.as_deref()));
        }
        for (k, (_, plain)) in &self.writes {
            if !in_range(k, begin, hi.as_deref()) {
                continue;
            }
            match plain {
                Some(v) => {
                    merged.insert(
                        k.clone(),
                        KvEntry {
                            key: k.clone(),
                            value: v.clone(),
                            version: vec![0; 10],
                        },
                    );
                }
                None => {
                    merged.remove(k);
                }
            }
        }
        Ok(merged.into_values().collect())
    }

    /// Make the commit depend on a key's version: it conflicts unless the
    /// key is still at `version` (`None`: still absent). The first version
    /// expected of a key stands.
    pub fn expect_key(&mut self, key: &[u8], version: Option<Vec<u8>>) {
        self.expects.entry(key.to_vec()).or_insert(Expect {
            fs: self.fs.id(),
            key: key.to_vec(),
            version,
        });
    }

    /// Write a value.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let sealed = seal::seal_value(&*self.fs.seal_keys()?, key, value, &mut OsRng)?;
        self.writes.insert(
            key.to_vec(),
            (
                Write {
                    fs: self.fs.id(),
                    key: key.to_vec(),
                    value: Some(sealed),
                },
                Some(value.to_vec()),
            ),
        );
        Ok(())
    }

    /// Delete a value.
    pub fn delete(&mut self, key: &[u8]) {
        self.writes.insert(
            key.to_vec(),
            (
                Write {
                    fs: self.fs.id(),
                    key: key.to_vec(),
                    value: None,
                },
                None,
            ),
        );
    }

    /// Delete the stored-key range `[begin, end)` (applied before the writes).
    pub fn clear_range(&mut self, begin: &[u8], end: Option<&[u8]>) {
        self.writes.retain(|k, _| !in_range(k, begin, end));
        self.clears.push(FsRange {
            fs: self.fs.id(),
            begin: begin.to_vec(),
            end: end.map(<[u8]>::to_vec),
        });
    }

    /// Delete everything under a stored-key prefix.
    pub fn clear_prefix(&mut self, prefix: &[u8]) {
        let end = prefix_end(prefix);
        self.clear_range(prefix, end.as_deref());
    }

    /// Commit only while lease `name` of this fs holds `token` (api.md §16):
    /// otherwise 412 `claim_lost`, and nothing is written.
    pub fn fence(&mut self, name: &[u8], token: u64) {
        self.fences.push(Fence {
            fs: self.fs.id(),
            name: name.to_vec(),
            token,
        });
    }

    /// [`Transaction::fence`] with a held lease.
    pub fn fence_lease(&mut self, lease: &Lease) {
        self.fence(lease.name(), lease.token());
    }

    /// Run `f` with the result once this attempt commits.
    pub fn on_commit(&mut self, f: impl FnOnce(&CommitResult) + Send + 'static) {
        self.hooks.push(Box::new(f));
    }

    /// Whether the commit would carry nothing.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.clears.is_empty()
    }

    /// The commit body of this attempt (the same `commit_id` each time).
    pub fn build(&self) -> Commit {
        let short = self.mode == Mode::Short && self.rv0.is_some();
        Commit {
            commit_id: self.commit_id.to_vec(),
            read_version: if short { self.rv0 } else { None },
            read_conflicts: if short {
                self.conflicts.clone()
            } else {
                Vec::new()
            },
            expect: self.expects.values().cloned().collect(),
            expect_ranges: self.expect_ranges.clone(),
            writes: self.writes.values().map(|(w, _)| w.clone()).collect(),
            clear_ranges: self.clears.clone(),
            fence: self.fences.clone(),
            ..Default::default()
        }
    }

    /// Commit this attempt: `None` when there is nothing to send. Sending
    /// it again (after a lost answer) replays the same `commit_id`.
    pub async fn commit(&mut self) -> Result<Option<CommitResult>> {
        if self.is_empty() {
            return Ok(None);
        }
        let r = send_commit(&self.fs, &self.build(), 4).await?;
        for h in self.hooks.drain(..) {
            h(&r);
        }
        self.result = Some(r.clone());
        Ok(Some(r))
    }
}
