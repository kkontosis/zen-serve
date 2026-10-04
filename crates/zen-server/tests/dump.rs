//! Logical export/import and migrate: a round trip reproduces the keyspace
//! byte for byte, a server on the copy serves the same data (old sessions
//! included), and new versionstamps stay newer than imported ones.

mod common;

use common::*;
use std::sync::Arc;
use zen_proto::*;
use zen_server::dump;
use zen_store::embedded::{Embedded, Options};
use zen_store::{KeyValue, Storage, key_after};

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
    let private = zen_store::tuple::Key::new()
        .str("meta")
        .str("version")
        .finish();
    out.retain(|(k, _)| *k != private);
    Ok(out)
}

/// A server with some KV data and events; returns it with a session token.
async fn populated() -> (Harness, Vec<u8>, CommitResult) {
    populated_on(Harness::start().await).await
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
    let r: CommitResult = h.call("/v1/commit", Some(&tok), &c).await.unwrap();
    (h, tok, r)
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
        let mut t = target.begin(None).await.unwrap();
        t.set(b"z", b"1");
        match t.commit().await {
            Ok(s) => break s,
            Err(zen_store::Error::TooOld) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(stamp[..] > first.versionstamp[..]);
}
