//! Logical export/import and migrate: a round trip reproduces the keyspace
//! byte for byte, a server on the copy serves the same data (old sessions
//! included), and new versionstamps stay newer than imported ones.

mod common;

use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use zen_proto::*;
use zen_server::dump;
use zen_store::embedded::{Embedded, Options};
use zen_store::{KeyValue, Stamp, Storage, Txn, Version, Watch, key_after};

async fn everything(s: &dyn Storage) -> Vec<KeyValue> {
    // Retried: right after a version jump, storage may briefly lag (TooOld).
    for _ in 0..50 {
        match try_everything(s).await {
            Ok(v) => return v,
            Err(zen_store::Error::TooOld) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("storage kept failing");
}

/// One key, retried like [`everything`]: tests in this binary run at once,
/// and an import or migrate jumps a shared FoundationDB cluster's version.
async fn get(s: &dyn Storage, key: &[u8]) -> Option<Vec<u8>> {
    for _ in 0..50 {
        let got = match s.begin(None).await {
            Ok(mut t) => t.get(key).await,
            Err(e) => Err(e),
        };
        match got {
            Ok(v) => return v,
            Err(zen_store::Error::TooOld) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("storage kept failing");
}

async fn try_everything(s: &dyn Storage) -> zen_store::Result<Vec<KeyValue>> {
    let mut out = Vec::new();
    let mut from = Vec::new();
    let mut t = s.begin(None).await?;
    loop {
        let got = t.snapshot_get_range(&from, &[0xFF], 500, false).await?;
        let more = got.len() == 500;
        if let Some((k, _)) = got.last() {
            from = key_after(k);
        }
        out.extend(got);
        if !more {
            break;
        }
    }
    // Server metadata is private to each store (keyspace.md §3.4), and
    // checkpoint doorbells only wake streams (§3.10): neither is exported.
    let private = [
        zen_server::keys::meta_prefix(),
        zen_server::keys::ckpt_bells(),
    ];
    out.retain(|(k, _)| !private.iter().any(|p| k.starts_with(p)));
    Ok(out)
}

/// A server with some KV data and events; returns it with a session token.
async fn populated() -> (Harness, Vec<u8>, CommitResult) {
    populated_on(Harness::start().await).await
}

/// `h.call`, retried on `too_old`: tests in this binary run at once, and an
/// import or migrate jumps a shared FoundationDB cluster's version, after
/// which it refuses transactions for a few seconds.
async fn call<Q: serde::Serialize, R: serde::de::DeserializeOwned>(
    h: &Harness,
    path: &str,
    tok: &[u8],
    req: &Q,
) -> R {
    for _ in 0..50 {
        match h.call(path, Some(tok), req).await {
            Ok(r) => return r,
            Err((409, e)) if e.code == "too_old" => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("{path}: {e:?}"),
        }
    }
    panic!("{path}: storage kept failing");
}

async fn populated_on(h: Harness) -> (Harness, Vec<u8>, CommitResult) {
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let c = Commit {
        commit_id: cid(1),
        writes: (0..50u8)
            .map(|i| Write {
                fs: 1,
                key: vec![i; 16],
                value: Some(vec![i; 300]),
            })
            .collect(),
        append: (0..20u8)
            .map(|i| append(&topic(1), Some(&key(i)), &[i; 40]))
            .collect(),
        ..Default::default()
    };
    let r: CommitResult = call(&h, "/v1/commit", &tok, &c).await;
    // Integrity data (keyspace.md §3.10): a checkpoint, a registered
    // device and a fenced lease.
    let _: CheckpointVersion = call(
        &h,
        "/v1/checkpoint/put",
        &tok,
        &CheckpointPut {
            fs: 1,
            device: admin_device(),
            seq: 1,
            checkpoint: checkpoint_of(&admin.device, 1, 1),
        },
    )
    .await;
    let extra = zen_core::sig::DeviceSecret::from_seed(&[0x33; 32]);
    let cert = zen_core::sig::issue_device_cert(&admin.id, &extra.public(), 1).unwrap();
    let _: DeviceRegistered =
        call(&h, "/v1/devices/register", &tok, &DeviceRegister { cert }).await;
    let _: NamedLease = call(
        &h,
        "/v1/lease",
        &tok,
        &NamedLeasePut {
            fs: 1,
            name: b"vol".to_vec(),
            token: None,
            ttl_ms: None,
            release: false,
        },
    )
    .await;
    (h, tok, r)
}

fn admin_device() -> Vec<u8> {
    User::new(1).device.public().fingerprint().to_vec()
}

fn fresh_embedded(dir: &std::path::Path) -> Embedded {
    std::fs::create_dir_all(dir).unwrap();
    Embedded::open(dir.join("zen.redb"), Options::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn export_import_round_trip() {
    let (h, tok, first) = populated().await;
    let src = h.server.state.store.clone();
    let file = h.dir.path().join("dump.zen");
    let st = dump::export(src.as_ref(), "test", std::fs::File::create(&file).unwrap())
        .await
        .unwrap();
    assert!(st.keys > 100);

    let copy_dir = tempfile::tempdir().unwrap();
    let data = copy_dir.path().join("data");
    {
        let dst = fresh_embedded(&data);
        let (hdr, st2) = dump::import(&dst, std::fs::File::open(&file).unwrap(), false)
            .await
            .unwrap();
        assert_eq!((hdr.backend.as_str(), st2.keys), ("test", st.keys));
        assert_eq!(everything(&dst).await, everything(src.as_ref()).await);
        // Checkpoints and their heads are data; the doorbell isn't.
        let copied = everything(&dst).await;
        let has = |p: Vec<u8>| copied.iter().any(|(k, _)| k.starts_with(&p));
        assert!(has(zen_server::keys::ckpts(1, &admin_device()).finish()));
        assert!(has(zen_server::keys::ckpt_head(1, &admin_device())));
        let bell = zen_server::keys::ckpt_bell(1);
        assert!(get(src.as_ref(), &bell).await.is_some());
        assert!(get(&dst, &bell).await.is_none());
        // The pinned sign-in origin is data, not server metadata.
        let pins = zen_server::keys::origin_pins();
        assert!(everything(&dst).await.iter().any(|(k, _)| *k == pins));
        // A second import into a non-empty store needs --force.
        assert!(
            dump::import(&dst, std::fs::File::open(&file).unwrap(), false)
                .await
                .is_err()
        );
    }

    // A corrupt export is refused.
    let mut bytes = std::fs::read(&file).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 1;
    let other = fresh_embedded(&copy_dir.path().join("other"));
    assert!(dump::import(&other, &bytes[..], false).await.is_err());

    // A server on the copy serves the same data to the old session.
    if on_fdb() {
        return; // the harness would put the server on FoundationDB
    }
    let h2 = Harness::start_with(|c| c.data_dir = data.clone()).await;
    assert!(h2.server.claim_token.is_none());
    let r: KvItems = h2
        .call(
            "/v1/kv/get",
            Some(&tok),
            &KvGet {
                fs: 1,
                keys: vec![ByteBuf::from(vec![7; 16])],
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r.items[0].value.as_deref(), Some(&[7u8; 300][..]));
    let c = LogAppend {
        commit_id: cid(2),
        append: vec![append(&topic(1), None, b"new")],
    };
    let second: CommitResult = h2.call("/v1/log/append", Some(&tok), &c).await.unwrap();
    assert!(second.versionstamp > first.versionstamp);
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_copies_into_another_store() {
    // From an embedded server, whatever the test backend.
    let h = Harness::start_with(|c| c.storage = Default::default()).await;
    let (h, _, first) = populated_on(h).await;
    let src = h.server.state.store.clone();
    let target: Arc<dyn Storage> = if on_fdb() {
        // A fresh prefix on the test cluster.
        let file = std::env::var("ZEN_TEST_CLUSTER_FILE").unwrap();
        let mut cfg = zen_server::config::Config::with_data_dir(h.dir.path().join("t"));
        cfg.storage.backend = Some(zen_server::config::Backend::Fdb);
        cfg.storage.cluster_file = Some(file.into());
        let mut r = [0u8; 8];
        getrandom::fill(&mut r).unwrap();
        cfg.storage.key_prefix = format!("migrate/{r:02x?}/");
        zen_server::open_store(&cfg).unwrap()
    } else {
        Arc::new(fresh_embedded(&h.dir.path().join("t")))
    };
    let st = dump::copy(src.as_ref(), target.as_ref(), false)
        .await
        .unwrap();
    assert!(st.keys > 100);
    assert_eq!(
        everything(target.as_ref()).await,
        everything(src.as_ref()).await
    );
    let stamp = loop {
        let committed = match target.begin(None).await {
            Ok(mut t) => {
                t.set(b"z", b"1");
                t.commit().await
            }
            Err(e) => Err(e),
        };
        match committed {
            Ok(s) => break s,
            Err(zen_store::Error::TooOld) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(stamp[..] > first.versionstamp[..]);
}

/// A started server with a relayed service writes transient relay keys
/// (its node lease); they don't count, so an import needs no `--force`.
#[tokio::test(flavor = "multi_thread")]
async fn relay_keys_count_as_empty() {
    let target = Harness::start_with(|c| {
        c.service.push(zen_server::config::ServiceConfig {
            name: "echo".into(),
            fs: 1,
            id: vec![0x5E; 16],
            mode: zen_server::config::SvcMode::Tcp,
            listen: Some("127.0.0.1:0".parse().unwrap()),
            max_conns: 16,
            per_ip_rate: 0,
            per_ip_burst: 50,
            window: 262_144,
            out_window: 1_048_576,
            accept_wait_ms: 60_000,
            handshake_secs: 30,
            idle_secs: 300,
            resume: true,
            inbox: None,
        })
    })
    .await;
    let store = target.server.state.store.clone();
    let rk = zen_server::keys::node_lease(&target.server.state.svc.node);
    assert!(get(store.as_ref(), &rk).await.is_some(), "the node lease");
    assert!(dump::is_empty(store.as_ref()).await.unwrap());
}

/// A server that was only ever started holds no data: its metadata (the
/// challenge key, the embedded version clock) is private to it. So an
/// import needs no `--force`, and the target keeps its own challenge key.
#[tokio::test(flavor = "multi_thread")]
async fn a_started_server_counts_as_empty() {
    let (h, _, _) = populated().await;
    let file = h.dir.path().join("dump.zen");
    dump::export(
        h.server.state.store.as_ref(),
        "test",
        std::fs::File::create(&file).unwrap(),
    )
    .await
    .unwrap();

    let target = Harness::start().await;
    let store = target.server.state.store.clone();
    let key = zen_server::keys::meta("challenge_key");
    let mine = target.server.state.challenge_key;
    assert_eq!(get(store.as_ref(), &key).await.as_deref(), Some(&mine[..]));
    assert!(dump::is_empty(store.as_ref()).await.unwrap());
    dump::import(store.as_ref(), std::fs::File::open(&file).unwrap(), false)
        .await
        .unwrap();
    assert!(!dump::is_empty(store.as_ref()).await.unwrap());
    assert_eq!(
        everything(store.as_ref()).await,
        everything(h.server.state.store.as_ref()).await
    );
    assert_eq!(
        get(store.as_ref(), &key).await.as_deref(),
        Some(&mine[..]),
        "own challenge key kept"
    );
}

/// A store whose next `fail` range reads give `TooOld`, as a FoundationDB
/// cluster's do while it lags (zen-store maps every retryable error to it).
struct Lagging {
    inner: Arc<dyn Storage>,
    fail: Arc<AtomicU32>,
}

struct LaggingTxn {
    inner: Box<dyn Txn>,
    fail: Arc<AtomicU32>,
}

impl Lagging {
    fn new(inner: Arc<dyn Storage>, fail: u32) -> Self {
        Self {
            inner,
            fail: Arc::new(AtomicU32::new(fail)),
        }
    }

    fn left(&self) -> u32 {
        self.fail.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Storage for Lagging {
    async fn begin(&self, rv: Option<Version>) -> zen_store::Result<Box<dyn Txn>> {
        Ok(Box::new(LaggingTxn {
            inner: self.inner.begin(rv).await?,
            fail: self.fail.clone(),
        }))
    }
    async fn watch(&self, key: &[u8]) -> zen_store::Result<Watch> {
        self.inner.watch(key).await
    }
    async fn now_version(&self) -> zen_store::Result<Version> {
        self.inner.now_version().await
    }
    async fn advance_version(&self, at_least: Version) -> zen_store::Result<()> {
        self.inner.advance_version(at_least).await
    }
}

#[async_trait::async_trait]
impl Txn for LaggingTxn {
    fn read_version(&self) -> Version {
        self.inner.read_version()
    }
    async fn get(&mut self, key: &[u8]) -> zen_store::Result<Option<Vec<u8>>> {
        self.inner.get(key).await
    }
    async fn snapshot_get(&mut self, key: &[u8]) -> zen_store::Result<Option<Vec<u8>>> {
        self.inner.snapshot_get(key).await
    }
    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> zen_store::Result<Vec<KeyValue>> {
        self.inner.get_range(begin, end, limit, reverse).await
    }
    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> zen_store::Result<Vec<KeyValue>> {
        // One read at a time in these tests: load, then take one.
        if self.fail.load(Ordering::Relaxed) > 0 {
            self.fail.fetch_sub(1, Ordering::Relaxed);
            return Err(zen_store::Error::TooOld);
        }
        self.inner
            .snapshot_get_range(begin, end, limit, reverse)
            .await
    }
    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]) {
        self.inner.add_read_conflict_range(begin, end)
    }
    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.inner.set(key, value)
    }
    fn clear(&mut self, key: &[u8]) {
        self.inner.clear(key)
    }
    fn clear_range(&mut self, begin: &[u8], end: &[u8]) {
        self.inner.clear_range(begin, end)
    }
    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]) {
        self.inner.set_versionstamped_key(prefix, suffix, value)
    }
    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]) {
        self.inner.set_versionstamped_value(key, prefix, suffix)
    }
    fn atomic_add(&mut self, key: &[u8], delta: i64) {
        self.inner.atomic_add(key, delta)
    }
    async fn commit(self: Box<Self>) -> zen_store::Result<Stamp> {
        self.inner.commit().await
    }
}

/// A store that lags for a moment (`TooOld`) doesn't fail the emptiness
/// check or a copy: both read again.
#[tokio::test(flavor = "multi_thread")]
async fn a_lagging_store_is_read_again() {
    let (h, _, _) = populated().await;
    let target = Harness::start().await;
    let lagging = Lagging::new(target.server.state.store.clone(), 3);
    assert!(dump::is_empty(&lagging).await.unwrap());
    assert_eq!(lagging.left(), 0, "the lagging reads happened");

    let src = Lagging::new(h.server.state.store.clone(), 3);
    dump::copy(&src, target.server.state.store.as_ref(), false)
        .await
        .unwrap();
    assert_eq!(src.left(), 0, "the lagging reads happened");
    assert_eq!(
        everything(target.server.state.store.as_ref()).await,
        everything(h.server.state.store.as_ref()).await
    );
}
