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

/// What redb's layout gives a piece of a split value for `key`, and the
/// longest value kept inline (`embedded.rs`: a 4 KiB page, a 12-byte
/// one-entry leaf header, and part keys 8 bytes longer than the key).
fn piece_for(key: &[u8]) -> usize {
    4096 - 12 - 8 - key.len()
}
fn inline_max(key: &[u8]) -> usize {
    4096 - 12 - 1 - key.len()
}

/// The pieces `live` needs.
fn pieces_needed(live: &std::collections::BTreeMap<Vec<u8>, usize>) -> u64 {
    live.iter()
        .filter(|(k, n)| **n > inline_max(k))
        .map(|(k, n)| (n / piece_for(k)) as u64)
        .sum()
}

/// A deterministic xorshift64 stream.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut x = seed | 1;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

/// Bytes allocated to the file (not its apparent size: redb grows it sparse).
fn allocated(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().blocks() * 512
}

/// Large values cost about their size on disk, not the power of two of
/// pages redb would round a one-entry leaf to: sealed fs chunks of CDC
/// lengths (8,192–89,984 B), fixed chunks of 64 KiB and 64 KiB − 256 B
/// (65,584 and 65,328 B sealed), and zen-blk blocks (4,144 B).
#[tokio::test(flavor = "multi_thread")]
async fn large_values_cost_their_size_on_disk() {
    let mut next = rng(7);
    type Len = Box<dyn FnMut(&mut dyn FnMut() -> u64) -> usize>;
    let sets: [(&str, usize, Len); 4] = [
        (
            "cdc",
            300,
            Box::new(|r| 8192 + (r() % (89_984 - 8192 + 1)) as usize),
        ),
        ("fixed 64 KiB", 300, Box::new(|_| 65_584)),
        ("fixed 64 KiB - 256", 300, Box::new(|_| 65_328)),
        ("blocks", 3000, Box::new(|_| 4144)),
    ];
    let mut over = Vec::new();
    for (name, count, mut len) in sets {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        let s = Embedded::open(&path, Options::default()).unwrap();
        put(&s, b"warm", b"x").await;
        let before = allocated(&path);
        let mut logical = 0u64;
        let mut i = 0u32;
        while (i as usize) < count {
            let mut t = s.begin(None).await.unwrap();
            // About 4 MiB a commit, as fs uploads go.
            let mut bytes = 0;
            while bytes < 4 << 20 && (i as usize) < count {
                let n = len(&mut next);
                let mut v = vec![0u8; n];
                getrandom::fill(&mut v).unwrap();
                // A chunk key's shape: a short tuple prefix and a 16-byte id.
                let mut k = b"\x02c\x00\x15\x01".to_vec();
                k.extend_from_slice(&[0x5a; 12]);
                k.extend_from_slice(&i.to_be_bytes());
                t.set(&k, &v);
                logical += n as u64 + k.len() as u64;
                bytes += n;
                i += 1;
            }
            t.commit().await.unwrap();
        }
        drop(s);
        let grew = allocated(&path) - before;
        let ratio = grew as f64 / logical as f64;
        eprintln!("{name}: {logical} B stored in {grew} B allocated, {ratio:.3}x");
        if ratio > 1.12 {
            over.push(format!("{name}: {ratio:.3}x"));
        }
    }
    assert!(over.is_empty(), "on disk at more than 1.12x: {over:?}");
}

/// Overwrites, clears, range clears and versionstamps leave exactly the
/// pieces the live values need.
#[tokio::test(flavor = "multi_thread")]
async fn no_pieces_left_behind() {
    use std::collections::BTreeMap;
    let s = fresh();
    let mut live: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let check = |s: &Embedded, live: &BTreeMap<Vec<u8>, usize>, at: &str| {
        assert_eq!(s.part_count().unwrap(), pieces_needed(live), "{at}");
    };
    let set = |live: &mut BTreeMap<Vec<u8>, usize>, k: &[u8], n: usize| {
        live.insert(k.to_vec(), n);
        (k.to_vec(), vec![0xab; n])
    };
    let mut t = s.begin(None).await.unwrap();
    for (k, n) in [
        (&b"a"[..], 20_000),
        (b"b", 100_000),
        (b"c", 10),
        (b"d", 9000),
        (b"e", inline_max(b"e")),
        (b"f", inline_max(b"f") + 1),
        (b"g", 50_000),
    ] {
        let (k, v) = set(&mut live, k, n);
        t.set(&k, &v);
    }
    t.commit().await.unwrap();
    check(&s, &live, "written");
    assert!(s.part_count().unwrap() > 30);

    for (k, n, at) in [
        (&b"a"[..], 3, "large to small"),
        (b"b", 150_000, "large to larger"),
        (b"b", 9_000, "larger to smaller"),
        (b"c", 40_000, "small to large"),
    ] {
        let (k, v) = set(&mut live, k, n);
        put(&s, &k, &v).await;
        check(&s, &live, at);
    }

    let mut t = s.begin(None).await.unwrap();
    t.clear(b"d");
    live.remove(&b"d"[..]);
    t.clear_range(b"e", b"g");
    live.retain(|k, _| !(&b"e"[..]..&b"g"[..]).contains(&k.as_slice()));
    t.commit().await.unwrap();
    check(&s, &live, "cleared");

    // A set and a clear of one key in one transaction, and a versionstamped
    // value past a page over a split one.
    let mut t = s.begin(None).await.unwrap();
    t.set(b"h", &[1; 30_000]);
    t.clear(b"h");
    t.set_versionstamped_value(b"g", &[2; 8000], b"");
    live.insert(b"g".to_vec(), 8010);
    t.commit().await.unwrap();
    check(&s, &live, "versionstamped");

    let mut t = s.begin(None).await.unwrap();
    t.clear_range(b"", b"\xff");
    t.commit().await.unwrap();
    assert_eq!(s.part_count().unwrap(), 0);
}

/// A file from before values were split (no layout marker) is refused, as
/// is one in another layout; a file of this layout reopens.
#[test]
fn another_layout_is_refused() {
    use redb::{Database, TableDefinition};
    let kv: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
    let meta = |name| zen_store::tuple::Key::new().str("meta").str(name).finish();
    for (rows, want) in [
        (
            vec![(meta("version"), 7u64.to_be_bytes().to_vec())],
            "was written by an older zen-serve; start from an empty data_dir",
        ),
        (
            vec![(
                meta("store_format"),
                [&[0u8][..], &3u64.to_be_bytes()].concat(),
            )],
            "is in store format 3, and this zen-serve reads 2",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        {
            let db = Database::create(&path).unwrap();
            let w = db.begin_write().unwrap();
            {
                let mut t = w.open_table(kv).unwrap();
                for (k, v) in &rows {
                    t.insert(k.as_slice(), v.as_slice()).unwrap();
                }
            }
            w.commit().unwrap();
        }
        let err = Embedded::open(&path, Options::default()).err().unwrap();
        assert!(matches!(&err, Error::Io(m) if m.ends_with(want)), "{err:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    drop(open(&dir));
    drop(open(&dir));
}
