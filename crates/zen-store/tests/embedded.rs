//! The embedded backend: the conformance suite (plain and under a key
//! prefix), plus what only the embedded backend does: TooOld after the
//! window, unknown read versions, persistence.

use std::sync::Arc;
use std::time::Duration;
use zen_store::embedded::{Embedded, Options};
use zen_store::prefixed::Prefixed;
use zen_store::{Error, Storage, conformance, version_of};

fn open(dir: &tempfile::TempDir) -> Embedded {
    Embedded::open(dir.path().join("db.redb"), Options::default()).unwrap()
}

fn fresh() -> Embedded {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    // The database file stays open; the directory is removed at exit.
    std::mem::forget(dir);
    s
}

async fn put(s: &Embedded, k: &[u8], v: &[u8]) -> u64 {
    let mut t = s.begin(None).await.unwrap();
    t.set(k, v);
    version_of(&t.commit().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn conformance_embedded() {
    conformance::run_all(&|| Arc::new(fresh())).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conformance_prefixed() {
    // Two prefixes on one store must not see each other.
    let inner: Arc<dyn Storage> = Arc::new(fresh());
    let n = std::sync::atomic::AtomicU8::new(0);
    conformance::run_all(&|| {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Arc::new(Prefixed::new(inner.clone(), &[b'p', i, 0]))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_read_version_is_too_old() {
    let s = fresh();
    let rv = s.begin(None).await.unwrap().read_version();
    assert_eq!(s.begin(Some(rv + 12345)).await.err(), Some(Error::TooOld));
}

#[tokio::test(flavor = "multi_thread")]
async fn too_old_after_window() {
    let dir = tempfile::tempdir().unwrap();
    let s = Embedded::open(
        dir.path().join("db.redb"),
        Options {
            window: Duration::from_millis(50),
        },
    )
    .unwrap();
    put(&s, b"k", b"0").await;
    let mut t = s.begin(None).await.unwrap();
    t.get(b"k").await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    put(&s, b"x", b"0").await;
    put(&s, b"y", b"0").await;
    t.set(b"k", b"1");
    assert_eq!(t.commit().await, Err(Error::TooOld));
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_add_is_unreadable_in_txn() {
    let s = fresh();
    let mut t = s.begin(None).await.unwrap();
    t.atomic_add(b"ctr", 1);
    assert_eq!(t.get(b"ctr").await, Err(Error::Unreadable));
}

#[tokio::test(flavor = "multi_thread")]
async fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let v = {
        let s = open(&dir);
        put(&s, b"k", b"v").await
    };
    let s = open(&dir);
    let mut t = s.begin(None).await.unwrap();
    assert!(t.read_version() >= v);
    assert_eq!(t.get(b"k").await.unwrap(), Some(b"v".to_vec()));
    t.set(b"k2", b"v");
    assert!(version_of(&t.commit().await.unwrap()) > v);
}
