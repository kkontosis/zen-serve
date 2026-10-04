//! The FoundationDB backend (feature `fdb`): a thin adapter from
//! [`Storage`]/[`Txn`] to FoundationDB 7.3 transactions.
//!
//! * **Errors.** `not_committed` → [`Error::Conflict`];
//!   `commit_unknown_result` → [`Error::CommitUnknown`]; `accessed_unreadable`
//!   → [`Error::Unreadable`]; every other retryable error (too old, future
//!   version, process behind, throttling, recovery) → [`Error::TooOld`], which
//!   callers retry with a fresh read version; anything else → [`Error::Io`].
//! * **Watches** go through a per-process hub: one FoundationDB watch per key,
//!   shared by every waiter and re-armed after it fires, so the process stays
//!   far below the client's 10,000-watch limit.
//! * **Version clock.** [`Storage::now_version`] is a read version cached for
//!   up to 100 ms.

#![allow(unsafe_code)]

use crate::{Error, KeyValue, Result, STAMP_LEN, Stamp, Storage, Txn, Version, Watch, stamp_of};
use foundationdb::options::{MutationType, StreamingMode};
use foundationdb::{Database, FdbError, RangeOption, Transaction};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// TLS files for the client: certificate, key, CA bundle.
#[derive(Clone, Debug)]
pub struct Tls {
    /// Certificate chain (PEM).
    pub cert: String,
    /// Private key (PEM).
    pub key: String,
    /// CA bundle (PEM).
    pub ca: String,
}

static TLS: OnceLock<Option<Tls>> = OnceLock::new();

/// Use TLS for every connection of this process. Call before the first
/// [`Fdb::open`]; returns false if the network already started.
pub fn set_tls(tls: Tls) -> bool {
    TLS.set(Some(tls)).is_ok()
}

/// Start the FoundationDB client network thread once per process. It runs
/// until the process exits. Called by [`Fdb::open`].
pub fn network() -> Result<()> {
    static BOOTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    BOOTED
        .get_or_init(|| {
            let start = || -> std::result::Result<(), FdbError> {
                let mut b = foundationdb::api::FdbApiBuilder::default().build()?;
                if let Some(t) = TLS.get_or_init(|| None) {
                    use foundationdb::options::NetworkOption as N;
                    b = b.set_option(N::TLSCertPath(t.cert.clone()))?;
                    b = b.set_option(N::TLSKeyPath(t.key.clone()))?;
                    b = b.set_option(N::TLSCaPath(t.ca.clone()))?;
                }
                // SAFETY: runs once per process (guarded by the OnceLock); the
                // network is never stopped, so the guard is leaked on purpose.
                let guard = unsafe { b.boot()? };
                std::mem::forget(guard);
                Ok(())
            };
            start().map_err(|e| format!("start the FoundationDB client: {}", e.message()))
        })
        .clone()
        .map_err(Error::Io)
}

fn map_err(e: FdbError) -> Error {
    match e.code() {
        1020 => Error::Conflict,
        1021 => Error::CommitUnknown,
        1036 => Error::Unreadable,
        _ if e.is_maybe_committed() => Error::CommitUnknown,
        _ if e.is_retryable() => Error::TooOld,
        c => Error::Io(format!("fdb {c}: {}", e.message())),
    }
}

/// The FoundationDB backend. Cheap to clone.
#[derive(Clone)]
pub struct Fdb(Arc<Inner>);

struct Inner {
    db: Arc<Database>,
    hub: Arc<Hub>,
    clock: tokio::sync::Mutex<(Instant, Version)>,
}

impl Fdb {
    /// Connect with a cluster file (`None`: the platform default,
    /// `/etc/foundationdb/fdb.cluster`).
    pub fn open(cluster_file: Option<&str>) -> Result<Self> {
        network()?;
        let db = Arc::new(Database::new(cluster_file).map_err(map_err)?);
        Ok(Fdb(Arc::new(Inner {
            hub: Arc::new(Hub {
                db: db.clone(),
                keys: Mutex::new(HashMap::new()),
            }),
            db,
            clock: tokio::sync::Mutex::new((Instant::now() - Duration::from_secs(1), 0)),
        })))
    }

    /// The underlying database, for operations outside the [`Storage`]
    /// model (status, special keys).
    pub fn database(&self) -> &Database {
        &self.0.db
    }

    /// Read a key with system-key access (`\xff…`), e.g.
    /// `\xff\xff/status/json`.
    pub async fn read_system_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let trx = self.0.db.create_trx().map_err(map_err)?;
        trx.set_option(foundationdb::options::TransactionOption::ReadSystemKeys)
            .map_err(map_err)?;
        Ok(trx
            .get(key, true)
            .await
            .map_err(map_err)?
            .map(|v| v.to_vec()))
    }
}

#[async_trait::async_trait]
impl Storage for Fdb {
    async fn begin(&self, read_version: Option<Version>) -> Result<Box<dyn Txn>> {
        let trx = self.0.db.create_trx().map_err(map_err)?;
        let rv = match read_version {
            Some(rv) => {
                let rv_i = i64::try_from(rv).map_err(|_| Error::TooOld)?;
                trx.set_read_version(rv_i);
                rv
            }
            None => trx.get_read_version().await.map_err(map_err)? as Version,
        };
        Ok(Box::new(FdbTxn {
            trx,
            rv,
            wrote: false,
        }))
    }

    async fn watch(&self, key: &[u8]) -> Result<Watch> {
        self.0.hub.watch(key).await
    }

    async fn advance_version(&self, at_least: Version) -> Result<()> {
        // What `fdbcli advanceversion` does: the cluster recovers at a
        // version of at least `\xff/minRequiredCommitVersion`.
        let target = i64::try_from(at_least.saturating_add(1)).map_err(|_| Error::TooOld)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let trx = self.0.db.create_trx().map_err(map_err)?;
            let rv = trx.get_read_version().await.map_err(map_err)?;
            if rv >= target {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Io("advance_version: timed out".into()));
            }
            trx.set_option(foundationdb::options::TransactionOption::AccessSystemKeys)
                .map_err(map_err)?;
            trx.set(b"\xff/minRequiredCommitVersion", &target.to_le_bytes());
            if let Err(e) = trx.commit().await {
                // The recovery the write triggers can fail the commit itself.
                let e = FdbError::from(e);
                if !e.is_retryable() && !e.is_maybe_committed() {
                    return Err(map_err(e));
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn now_version(&self) -> Result<Version> {
        let mut c = self.0.clock.lock().await;
        if c.0.elapsed() > Duration::from_millis(100) {
            let trx = self.0.db.create_trx().map_err(map_err)?;
            let v = trx.get_read_version().await.map_err(map_err)? as Version;
            *c = (Instant::now(), v.max(c.1));
        }
        Ok(c.1)
    }
}

struct FdbTxn {
    trx: Transaction,
    rv: Version,
    wrote: bool,
}

impl FdbTxn {
    async fn range(
        &self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
        snapshot: bool,
    ) -> Result<Vec<KeyValue>> {
        if begin >= end || limit == 0 {
            return Ok(Vec::new());
        }
        let mut opt = RangeOption::from((begin.to_vec(), end.to_vec()));
        opt.limit = Some(limit);
        opt.reverse = reverse;
        opt.mode = StreamingMode::WantAll;
        let mut out = Vec::new();
        loop {
            let got = self
                .trx
                .get_range(&opt, 1, snapshot)
                .await
                .map_err(map_err)?;
            out.extend(
                got.iter()
                    .map(|kv| (kv.key().to_vec(), kv.value().to_vec())),
            );
            match opt.next_range(&got) {
                Some(next) => opt = next,
                None => break,
            }
        }
        Ok(out)
    }
}

/// `prefix ‖ 10 placeholder bytes ‖ suffix ‖ u32le(offset)`: the operand of
/// a versionstamp mutation.
fn vs_operand(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(prefix.len() + STAMP_LEN + suffix.len() + 4);
    b.extend_from_slice(prefix);
    b.extend_from_slice(&[0; STAMP_LEN]);
    b.extend_from_slice(suffix);
    b.extend_from_slice(&(prefix.len() as u32).to_le_bytes());
    b
}

#[async_trait::async_trait]
impl Txn for FdbTxn {
    fn read_version(&self) -> Version {
        self.rv
    }

    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .trx
            .get(key, false)
            .await
            .map_err(map_err)?
            .map(|v| v.to_vec()))
    }

    async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .trx
            .get(key, true)
            .await
            .map_err(map_err)?
            .map(|v| v.to_vec()))
    }

    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        self.range(begin, end, limit, reverse, false).await
    }

    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        self.range(begin, end, limit, reverse, true).await
    }

    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]) {
        if begin < end {
            // Fails only for invalid ranges, which `begin < end` rules out.
            let _ = self.trx.add_conflict_range(
                begin,
                end,
                foundationdb::options::ConflictRangeType::Read,
            );
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.wrote = true;
        self.trx.set(key, value);
    }

    fn clear(&mut self, key: &[u8]) {
        self.wrote = true;
        self.trx.clear(key);
    }

    fn clear_range(&mut self, begin: &[u8], end: &[u8]) {
        if begin < end {
            self.wrote = true;
            self.trx.clear_range(begin, end);
        }
    }

    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]) {
        self.wrote = true;
        self.trx.atomic_op(
            &vs_operand(prefix, suffix),
            value,
            MutationType::SetVersionstampedKey,
        );
    }

    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]) {
        self.wrote = true;
        self.trx.atomic_op(
            key,
            &vs_operand(prefix, suffix),
            MutationType::SetVersionstampedValue,
        );
    }

    fn atomic_add(&mut self, key: &[u8], delta: i64) {
        self.wrote = true;
        self.trx
            .atomic_op(key, &delta.to_le_bytes(), MutationType::Add);
    }

    async fn commit(self: Box<Self>) -> Result<Stamp> {
        if !self.wrote {
            return Ok(stamp_of(self.rv));
        }
        let stamp = self.trx.get_versionstamp();
        let committed = self.trx.commit().await.map_err(|e| map_err(e.into()))?;
        match stamp.await {
            Ok(s) if s.len() == STAMP_LEN => Ok(s[..].try_into().expect("10 bytes")),
            // Optimized into a read-only commit: no versionstamp.
            _ => {
                let v = committed.committed_version().map_err(map_err)?;
                Ok(stamp_of(if v > 0 { v as Version } else { self.rv }))
            }
        }
    }
}

/// One shared FoundationDB watch per key.
struct Hub {
    db: Arc<Database>,
    keys: Mutex<HashMap<Vec<u8>, watch::Receiver<WatchState>>>,
}

/// `fired` counts firings; `armed` is true while a watch armed after the
/// last firing is registered with the cluster.
#[derive(Clone, Copy)]
struct WatchState {
    fired: u64,
    armed: bool,
}

/// How often an idle key's watcher checks whether anyone still waits.
const IDLE_CHECK: Duration = Duration::from_secs(30);

impl Hub {
    async fn watch(self: &Arc<Self>, key: &[u8]) -> Result<Watch> {
        let mut rx = {
            let mut keys = self.keys.lock().expect("hub lock");
            match keys.get(key) {
                Some(rx) => rx.clone(),
                None => {
                    let (tx, rx) = watch::channel(WatchState {
                        fired: 0,
                        armed: false,
                    });
                    keys.insert(key.to_vec(), rx.clone());
                    tokio::spawn(self.clone().run(key.to_vec(), tx));
                    rx
                }
            }
        };
        // Wait until a watch armed after this call is registered: then any
        // write after a read version obtained later fires it.
        let state = *rx
            .wait_for(|s| s.armed)
            .await
            .map_err(|_| Error::Io("watch hub stopped".into()))?;
        Ok(Watch::new(async move {
            let _ = rx.wait_for(|s| s.fired > state.fired).await;
        }))
    }

    async fn run(self: Arc<Self>, key: Vec<u8>, tx: watch::Sender<WatchState>) {
        let mut backoff = Duration::from_millis(10);
        loop {
            match self.arm(&key).await {
                Ok(fut) => {
                    backoff = Duration::from_millis(10);
                    tx.send_modify(|s| s.armed = true);
                    let mut fut = std::pin::pin!(fut);
                    let fired = loop {
                        tokio::select! {
                            r = &mut fut => break r.is_ok(),
                            _ = tokio::time::sleep(IDLE_CHECK) => {
                                if self.idle(&key, &tx) {
                                    return;
                                }
                            }
                        }
                    };
                    tx.send_modify(|s| {
                        s.fired += 1;
                        s.armed = false;
                    });
                    if !fired {
                        tokio::time::sleep(backoff).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "fdb watch failed; retrying");
                    // Wake waiters (spurious is allowed) and retry.
                    tx.send_modify(|s| s.fired += 1);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
            if self.idle(&key, &tx) {
                return;
            }
        }
    }

    /// Remove the key when nobody but the hub map holds a receiver.
    fn idle(&self, key: &[u8], tx: &watch::Sender<WatchState>) -> bool {
        let mut keys = self.keys.lock().expect("hub lock");
        if tx.receiver_count() <= 1 {
            keys.remove(key);
            true
        } else {
            false
        }
    }

    async fn arm(
        &self,
        key: &[u8],
    ) -> std::result::Result<
        impl std::future::Future<Output = std::result::Result<(), FdbError>> + use<>,
        Error,
    > {
        let trx = self.db.create_trx().map_err(map_err)?;
        // Read first, so the watch compares against a read at this version.
        trx.get(key, false).await.map_err(map_err)?;
        let fut = trx.watch(key);
        trx.commit().await.map_err(|e| map_err(e.into()))?;
        Ok(fut)
    }
}
