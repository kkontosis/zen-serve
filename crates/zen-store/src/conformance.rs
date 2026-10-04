//! Backend conformance suite (feature `testing`): the behaviour every
//! [`Storage`] must share, run against each backend by its tests.
//!
//! `make` returns an empty store per case (a fresh database, or a fresh
//! [`crate::prefixed::Prefixed`] root on a shared cluster).

use crate::{Error, Storage, key_after, stamp_of};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

type Case = fn(Arc<dyn Storage>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        /// Every case, by name.
        pub const CASES: &[(&str, Case)] = &[$((stringify!($name), |s| Box::pin($name(s)))),*];
    };
}

cases!(
    read_your_writes_and_ranges,
    point_conflict,
    blind_writes_and_snapshot_reads_do_not_conflict,
    phantom_conflict,
    explicit_read_conflict_range,
    snapshot_isolation_at_read_version,
    versionstamps_increase,
    commit_stamp_matches_versionstamps,
    atomic_add,
    versionstamped_value_unreadable,
    watch_wakes_on_write,
    bank_transfers_keep_total,
);

/// Run every case, each on a fresh store from `make`.
pub async fn run_all(make: &(dyn Fn() -> Arc<dyn Storage> + Sync)) {
    for (name, case) in CASES {
        eprintln!("conformance: {name}");
        case(make()).await;
    }
}

async fn put(s: &dyn Storage, k: &[u8], v: &[u8]) -> [u8; 10] {
    let mut t = s.begin(None).await.unwrap();
    t.set(k, v);
    t.commit().await.unwrap()
}

fn keys(kv: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<Vec<u8>> {
    kv.into_iter().map(|(k, _)| k).collect()
}

/// Read-your-writes over points and ranges, both directions.
pub async fn read_your_writes_and_ranges(s: Arc<dyn Storage>) {
    put(&*s, b"a1", b"x").await;
    put(&*s, b"a3", b"x").await;
    put(&*s, b"a5", b"x").await;
    let mut t = s.begin(None).await.unwrap();
    t.set(b"a2", b"y");
    t.clear(b"a3");
    t.set(b"a4", b"y");
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(got, [&b"a1"[..], b"a2", b"a4", b"a5"]);
    let got = keys(t.get_range(b"a", b"b", 2, true).await.unwrap());
    assert_eq!(got, [&b"a5"[..], b"a4"]);
    t.clear_range(b"a2", b"a5");
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(got, [&b"a1"[..], b"a5"]);
    t.set(b"a3", b"z");
    assert_eq!(t.get(b"a3").await.unwrap(), Some(b"z".to_vec()));
    t.commit().await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    let got = keys(t.get_range(b"a", b"b", 100, false).await.unwrap());
    assert_eq!(got, [&b"a1"[..], b"a3", b"a5"]);
    // Large values and many keys come back whole (the backend may page).
    let big = vec![7u8; 90_000];
    let mut t = s.begin(None).await.unwrap();
    for i in 0..40u8 {
        t.set(&[b'm', i], &big);
    }
    t.commit().await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    let got = t.get_range(b"m", b"n", 1000, false).await.unwrap();
    assert_eq!(got.len(), 40);
    assert!(got.iter().all(|(_, v)| v.len() == big.len()));
    assert_eq!(t.get_range(b"m", b"n", 25, true).await.unwrap().len(), 25);
}

/// Two read-modify-writes of one key: the second commit conflicts.
pub async fn point_conflict(s: Arc<dyn Storage>) {
    put(&*s, b"k", b"0").await;
    let mut t1 = s.begin(None).await.unwrap();
    let mut t2 = s.begin(None).await.unwrap();
    assert_eq!(t1.get(b"k").await.unwrap(), Some(b"0".to_vec()));
    assert_eq!(t2.get(b"k").await.unwrap(), Some(b"0".to_vec()));
    t1.set(b"k", b"1");
    t2.set(b"k", b"2");
    t1.commit().await.unwrap();
    assert_eq!(t2.commit().await, Err(Error::Conflict));
}

/// Blind writes and snapshot reads add no conflict ranges.
pub async fn blind_writes_and_snapshot_reads_do_not_conflict(s: Arc<dyn Storage>) {
    let mut t1 = s.begin(None).await.unwrap();
    let mut t2 = s.begin(None).await.unwrap();
    t2.snapshot_get(b"k").await.unwrap();
    t2.snapshot_get_range(b"a", b"z", 10, false).await.unwrap();
    t1.set(b"k", b"1");
    t2.set(b"k", b"2");
    t1.commit().await.unwrap();
    t2.commit().await.unwrap();
}

/// An insert into a scanned range conflicts; outside a limited scan's
/// covered part it does not.
pub async fn phantom_conflict(s: Arc<dyn Storage>) {
    put(&*s, b"r1", b"x").await;
    let mut t1 = s.begin(None).await.unwrap();
    assert_eq!(t1.get_range(b"r", b"s", 100, false).await.unwrap().len(), 1);
    put(&*s, b"r2", b"x").await;
    t1.set(b"other", b"y");
    assert_eq!(t1.commit().await, Err(Error::Conflict));

    let mut t2 = s.begin(None).await.unwrap();
    assert_eq!(t2.get_range(b"r", b"s", 1, false).await.unwrap().len(), 1);
    put(&*s, b"r9", b"x").await;
    t2.set(b"other", b"z");
    t2.commit().await.unwrap();
}

/// `add_read_conflict_range` conflicts without reading.
pub async fn explicit_read_conflict_range(s: Arc<dyn Storage>) {
    let mut t = s.begin(None).await.unwrap();
    t.add_read_conflict_range(b"c", &key_after(b"c"));
    put(&*s, b"c", b"1").await;
    t.set(b"x", b"1");
    assert_eq!(t.commit().await, Err(Error::Conflict));
}

/// A transaction at an earlier read version sees that snapshot, and
/// conflicts with what committed since.
pub async fn snapshot_isolation_at_read_version(s: Arc<dyn Storage>) {
    put(&*s, b"k", b"old").await;
    let rv = s.begin(None).await.unwrap().read_version();
    put(&*s, b"k", b"new").await;
    let mut t = s.begin(Some(rv)).await.unwrap();
    assert_eq!(t.read_version(), rv);
    assert_eq!(t.get(b"k").await.unwrap(), Some(b"old".to_vec()));
    t.set(b"j", b"1");
    assert_eq!(t.commit().await, Err(Error::Conflict));
}

/// Versionstamped keys sort in commit order, the user suffix lands after
/// the stamp, and a read-only commit returns the read version's stamp.
pub async fn versionstamps_increase(s: Arc<dyn Storage>) {
    let mut last = [0u8; 10];
    for i in 0u8..5 {
        let mut t = s.begin(None).await.unwrap();
        t.set_versionstamped_key(b"log/", &[i], b"e");
        t.set_versionstamped_value(b"head", b"", b"");
        let stamp = t.commit().await.unwrap();
        assert!(stamp > last);
        last = stamp;
    }
    let mut t = s.begin(None).await.unwrap();
    let log = t.get_range(b"log/", b"log0", 100, false).await.unwrap();
    assert_eq!(log.len(), 5);
    assert!(log.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(log[4].0[14], 4);
    assert_eq!(&log[4].0[4..14], &last);
    let head = t.get(b"head").await.unwrap().unwrap();
    assert_eq!(head, last);
    let rv = t.read_version();
    assert_eq!(t.commit().await.unwrap(), stamp_of(rv));
}

/// The stamp `commit` returns is the one written by versionstamped
/// mutations, with prefix and suffix around it.
pub async fn commit_stamp_matches_versionstamps(s: Arc<dyn Storage>) {
    let mut t = s.begin(None).await.unwrap();
    t.set_versionstamped_value(b"v", b"<", b">");
    t.set(b"plain", b"1");
    let stamp = t.commit().await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    let v = t.get(b"v").await.unwrap().unwrap();
    assert_eq!(v[0], b'<');
    assert_eq!(&v[1..11], &stamp);
    assert_eq!(v[11], b'>');
    assert!(crate::version_of(&stamp) <= t.read_version());
}

/// Atomic add on little-endian i64, missing = 0, concurrent adds commute.
pub async fn atomic_add(s: Arc<dyn Storage>) {
    for d in [5i64, -2, 10] {
        let mut t = s.begin(None).await.unwrap();
        t.atomic_add(b"ctr", d);
        t.commit().await.unwrap();
    }
    let mut t1 = s.begin(None).await.unwrap();
    let mut t2 = s.begin(None).await.unwrap();
    t1.atomic_add(b"ctr", 100);
    t2.atomic_add(b"ctr", 1000);
    t1.commit().await.unwrap();
    t2.commit().await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    let v = t.get(b"ctr").await.unwrap().unwrap();
    assert_eq!(i64::from_le_bytes(v.try_into().unwrap()), 1113);
}

/// A key with a pending versionstamped value cannot be read back.
pub async fn versionstamped_value_unreadable(s: Arc<dyn Storage>) {
    let mut t = s.begin(None).await.unwrap();
    t.set_versionstamped_value(b"h", b"", b"");
    assert_eq!(t.get(b"h").await, Err(Error::Unreadable));
}

/// A watch fires on a later write of its key, not of another key; it also
/// fires on a range clear that removes the key.
pub async fn watch_wakes_on_write(s: Arc<dyn Storage>) {
    let w = s.watch(b"head").await.unwrap();
    let other = s.watch(b"other").await.unwrap();
    put(&*s, b"head", b"1").await;
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("watch fired");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), other)
            .await
            .is_err()
    );
    put(&*s, b"r5", b"x").await;
    let w = s.watch(b"r5").await.unwrap();
    let mut t = s.begin(None).await.unwrap();
    t.clear_range(b"r", b"s");
    t.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("range clear fired");
    // Many waiters on one key all wake.
    let ws: Vec<_> = watchers(&*s, b"many", 20).await;
    put(&*s, b"many", b"1").await;
    for w in ws {
        tokio::time::timeout(Duration::from_secs(5), w)
            .await
            .expect("every waiter fired");
    }
}

async fn watchers(s: &dyn Storage, key: &[u8], n: usize) -> Vec<crate::Watch> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(s.watch(key).await.unwrap());
    }
    out
}

/// Concurrent conflicting transfers between accounts keep the total.
pub async fn bank_transfers_keep_total(s: Arc<dyn Storage>) {
    const ACCOUNTS: u8 = 8;
    const START: i64 = 1000;
    let acct = |i: u8| vec![b'b', i];
    let mut t = s.begin(None).await.unwrap();
    for i in 0..ACCOUNTS {
        t.set(&acct(i), &START.to_le_bytes());
    }
    t.commit().await.unwrap();
    let mut tasks = Vec::new();
    for w in 0..8u64 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let mut seed = w.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut done = 0;
            let mut conflicts = 0;
            while done < 25 {
                let from = (next() % ACCOUNTS as u64) as u8;
                let to = (next() % ACCOUNTS as u64) as u8;
                let amount = (next() % 50) as i64;
                let mut t = s.begin(None).await.unwrap();
                let read = |v: Option<Vec<u8>>| i64::from_le_bytes(v.unwrap().try_into().unwrap());
                let a = read(t.get(&acct(from)).await.unwrap());
                let b = read(t.get(&acct(to)).await.unwrap());
                if from != to {
                    t.set(&acct(from), &(a - amount).to_le_bytes());
                    t.set(&acct(to), &(b + amount).to_le_bytes());
                }
                match t.commit().await {
                    Ok(_) => done += 1,
                    Err(Error::Conflict) => conflicts += 1,
                    Err(e) => panic!("transfer: {e}"),
                }
            }
            conflicts
        }));
    }
    let mut conflicts = 0;
    for t in tasks {
        conflicts += t.await.unwrap();
    }
    let mut t = s.begin(None).await.unwrap();
    let all = t.get_range(b"b", b"c", 100, false).await.unwrap();
    assert_eq!(all.len(), ACCOUNTS as usize);
    let total: i64 = all
        .iter()
        .map(|(_, v)| i64::from_le_bytes(v[..].try_into().unwrap()))
        .sum();
    assert_eq!(
        total,
        START * ACCOUNTS as i64,
        "after {conflicts} conflicts"
    );
}
