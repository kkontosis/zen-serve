//! Behaviour of the embedded backend: OCC conflicts (points and phantoms),
//! snapshots, versionstamps, TooOld, watches, persistence.

use std::time::Duration;
use zen_store::embedded::{Embedded, Options};
use zen_store::{Error, Storage, version_of};

fn open(dir: &tempfile::TempDir) -> Embedded {
    Embedded::open(dir.path().join("db.redb"), Options::default()).unwrap()
}

async fn put(s: &Embedded, k: &[u8], v: &[u8]) -> u64 {
    let mut t = s.begin(None).await.unwrap();
    t.set(k, v);
    t.commit().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn read_your_writes_and_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    put(&s, b"a1", b"x").await;
    put(&s, b"a3", b"x").await;
    put(&s, b"a5", b"x").await;
    let mut t = s.begin(None).await.unwrap();
    t.set(b"a2", b"y");
    t.clear(b"a3");
    t.set(b"a4", b"y");
    let keys = |kv: Vec<(Vec<u8>, Vec<u8>)>| kv.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(
        got,
        vec![
            b"a1".to_vec(),
            b"a2".to_vec(),
            b"a4".to_vec(),
            b"a5".to_vec()
        ]
    );
    let got = keys(t.get_range(b"a", b"b", 2, true).await.unwrap());
    assert_eq!(got, vec![b"a5".to_vec(), b"a4".to_vec()]);
    t.clear_range(b"a2", b"a5");
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(got, vec![b"a1".to_vec(), b"a5".to_vec()]);
    t.set(b"a3", b"z");
    assert_eq!(t.get(b"a3").await.unwrap(), Some(b"z".to_vec()));
    t.commit().await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(got, vec![b"a1".to_vec(), b"a3".to_vec(), b"a5".to_vec()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn point_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    put(&s, b"k", b"0").await;
    let mut t1 = s.begin(None).await.unwrap();
    let mut t2 = s.begin(None).await.unwrap();
    assert_eq!(t1.get(b"k").await.unwrap(), Some(b"0".to_vec()));
    assert_eq!(t2.get(b"k").await.unwrap(), Some(b"0".to_vec()));
    t1.set(b"k", b"1");
    t2.set(b"k", b"2");
    t1.commit().await.unwrap();
    assert_eq!(t2.commit().await, Err(Error::Conflict));
}

#[tokio::test(flavor = "multi_thread")]
async fn blind_writes_and_snapshot_reads_do_not_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    let mut t1 = s.begin(None).await.unwrap();
    let mut t2 = s.begin(None).await.unwrap();
    t2.snapshot_get(b"k").await.unwrap();
    t1.set(b"k", b"1");
    t2.set(b"k", b"2");
    t1.commit().await.unwrap();
    t2.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn phantom_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    put(&s, b"r1", b"x").await;
    let mut t1 = s.begin(None).await.unwrap();
    assert_eq!(t1.get_range(b"r", b"s", 100, false).await.unwrap().len(), 1);
    put(&s, b"r2", b"x").await; // insert into the scanned range
    t1.set(b"other", b"y");
    assert_eq!(t1.commit().await, Err(Error::Conflict));

    // A write outside a limited scan's covered part does not conflict.
    let mut t2 = s.begin(None).await.unwrap();
    assert_eq!(t2.get_range(b"r", b"s", 1, false).await.unwrap().len(), 1);
    put(&s, b"r9", b"x").await;
    t2.set(b"other", b"z");
    t2.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_isolation_at_read_version() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    put(&s, b"k", b"old").await;
    let rv = s.begin(None).await.unwrap().read_version();
    put(&s, b"k", b"new").await;
    let mut t = s.begin(Some(rv)).await.unwrap();
    assert_eq!(t.get(b"k").await.unwrap(), Some(b"old".to_vec()));
    t.set(b"j", b"1");
    assert_eq!(t.commit().await, Err(Error::Conflict));
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
async fn versionstamps_increase() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    let mut last = 0;
    for i in 0u8..5 {
        let mut t = s.begin(None).await.unwrap();
        t.set_versionstamped_key(b"log/", &[i], b"e");
        t.set_versionstamped_value(b"head", b"", b"");
        let v = t.commit().await.unwrap();
        assert!(v > last);
        last = v;
    }
    let mut t = s.begin(None).await.unwrap();
    let log = t.get_range(b"log/", b"log0", 100, false).await.unwrap();
    assert_eq!(log.len(), 5);
    let versions: Vec<u64> = log.iter().map(|(k, _)| version_of(&k[4..])).collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(log[4].0[14], 4);
    let head = t.get(b"head").await.unwrap().unwrap();
    assert_eq!(version_of(&head), last);
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_add_and_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    for d in [5i64, -2, 10] {
        let mut t = s.begin(None).await.unwrap();
        t.atomic_add(b"ctr", d);
        assert_eq!(t.get(b"ctr").await, Err(Error::Unreadable));
        t.commit().await.unwrap();
    }
    let mut t = s.begin(None).await.unwrap();
    let v = t.get(b"ctr").await.unwrap().unwrap();
    assert_eq!(i64::from_le_bytes(v.try_into().unwrap()), 13);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_wakes_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir);
    let w = s.watch(b"head");
    let other = s.watch(b"other");
    put(&s, b"head", b"1").await;
    tokio::time::timeout(Duration::from_secs(2), w)
        .await
        .expect("watch fired");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), other)
            .await
            .is_err()
    );
    let w = s.watch(b"r5");
    let mut t = s.begin(None).await.unwrap();
    t.clear_range(b"r", b"s");
    t.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), w)
        .await
        .expect("range clear fired");
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
    assert!(t.commit().await.unwrap() > v);
}
