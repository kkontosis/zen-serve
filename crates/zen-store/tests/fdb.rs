//! The FoundationDB backend against a real cluster: the conformance suite,
//! each case under a fresh random key prefix. Runs with `--features fdb`
//! when `ZEN_TEST_CLUSTER_FILE` names a cluster file; skipped otherwise.
#![cfg(feature = "fdb")]

use std::sync::Arc;
use std::time::Duration;
use zen_store::fdb::Fdb;
use zen_store::prefixed::Prefixed;
use zen_store::{Error, Storage, conformance};

fn cluster() -> Option<Fdb> {
    let file = std::env::var("ZEN_TEST_CLUSTER_FILE").ok()?;
    Some(Fdb::open(Some(&file)).expect("open fdb"))
}

fn random_root() -> Vec<u8> {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).unwrap();
    // A tuple byte-string element, so roots never prefix one another.
    zen_store::tuple::Key::new()
        .str("zen-test")
        .bytes(&r)
        .finish()
}

#[tokio::test(flavor = "multi_thread")]
async fn conformance_fdb() {
    let Some(db) = cluster() else {
        eprintln!("ZEN_TEST_CLUSTER_FILE not set; skipping");
        return;
    };
    let db: Arc<dyn Storage> = Arc::new(db);
    conformance::run_all(&|| Arc::new(Prefixed::new(db.clone(), &random_root()))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn future_read_version_fails() {
    let Some(db) = cluster() else { return };
    let rv = db.begin(None).await.unwrap().read_version();
    let mut t = db.begin(Some(rv + 3_600_000_000)).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(30), t.get(b"x")).await;
    assert_eq!(r.expect("fails within 30 s"), Err(Error::TooOld));
}

#[tokio::test(flavor = "multi_thread")]
async fn now_version_tracks_commits() {
    let Some(db) = cluster() else { return };
    let mut t = db.begin(None).await.unwrap();
    t.set(&random_root(), b"1");
    let v = zen_store::version_of(&t.commit().await.unwrap());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(db.now_version().await.unwrap() >= v);
}
