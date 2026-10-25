//! Milestone 6 on the server: device checkpoints and the device registry
//! (api.md §15), signed fs headers (§4.4), fenced leases and `fence` (§16,
//! §6), `changes` carrying `ops`/`chain` (§12), and the test adversary
//! that later steps test detection against.

mod common;

use common::*;
use std::time::Duration;
use zen_core::header::SignedHeader;
use zen_core::integrity::{open_checkpoint, verify_checkpoint};
use zen_core::sig::{DeviceSecret, issue_device_cert};
use zen_proto::acl::FsLimit;
use zen_proto::*;
use zen_server::config::Config;

/// Admin `a` (rights on fs 1), member `b` (rights on fs 1), member `c`
/// (a topic grant on fs 2 only), and a second admin `d` (no rights).
struct World {
    h: Harness,
    a: User,
    b: User,
    c: User,
    d: User,
    ta: Vec<u8>,
    tb: Vec<u8>,
    tc: Vec<u8>,
}

async fn world_with(limits: Vec<FsLimit>, f: impl FnOnce(&mut Config)) -> World {
    let h = Harness::start_with(f).await;
    let (a, b, c, d) = (User::new(1), User::new(2), User::new(3), User::new(4));
    let mut grants = Vec::new();
    for u in [&a, &b] {
        grants.push(fs_grant(u, 1, &["read", "write"]));
        grants.push(topic_grant(u, 1, &[], &["read", "append", "consume"]));
    }
    grants.push(topic_grant(&c, 2, &[], &["read"]));
    let (signed, _) = signed_acl(&a, 1, None, &[&a, &d], &[&a, &b, &c, &d], grants, limits);
    h.put_acl(signed, h.server.claim_token.clone())
        .await
        .unwrap();
    let ta = h.sign_in(&a).await.unwrap();
    let tb = h.sign_in(&b).await.unwrap();
    let tc = h.sign_in(&c).await.unwrap();
    World {
        h,
        a,
        b,
        c,
        d,
        ta,
        tb,
        tc,
    }
}

async fn world() -> World {
    world_with(vec![], |_| {}).await
}

fn code<T: std::fmt::Debug>(r: Result<T, ApiErr>) -> (u16, String) {
    let (s, b) = r.expect_err("an error");
    (s, b.code)
}

fn fp(d: &DeviceSecret) -> Vec<u8> {
    d.public().fingerprint().to_vec()
}

fn rcid() -> Vec<u8> {
    let mut c = vec![0u8; 16];
    getrandom::fill(&mut c).unwrap();
    c
}

async fn heads(
    h: &Harness,
    tok: &[u8],
    fs: u32,
    devices: Option<Vec<Vec<u8>>>,
) -> Vec<(Vec<u8>, u64)> {
    let r: CheckpointHeadList = h
        .call(
            "/v1/checkpoint/heads",
            Some(tok),
            &CheckpointHeads {
                fs,
                devices: devices.map(|v| v.into_iter().map(ByteBuf::from).collect()),
            },
        )
        .await
        .unwrap();
    r.heads.into_iter().map(|x| (x.device, x.seq)).collect()
}

async fn ckpt_get(
    h: &Harness,
    tok: &[u8],
    device: &[u8],
    from: u64,
    to: Option<u64>,
    limit: Option<u32>,
) -> Result<CheckpointList, ApiErr> {
    h.call(
        "/v1/checkpoint/get",
        Some(tok),
        &CheckpointGet {
            fs: 1,
            device: device.to_vec(),
            from,
            to,
            limit,
        },
    )
    .await
}

fn seqs(l: &CheckpointList) -> Vec<u64> {
    l.items.iter().map(|i| i.seq).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn info_lists_integrity_features() {
    let h = Harness::start().await;
    let info: Info = h.get("/v1/info").await;
    for f in ["checkpoints", "signed_header", "lease", "devices"] {
        assert!(info.features.iter().any(|x| x == f), "{f}");
    }
    assert_eq!(info.limits.checkpoint_keep, 1024);
    let h = Harness::start_with(|c| c.limits.checkpoint_keep = 7).await;
    let info: Info = h.get("/v1/info").await;
    assert_eq!(info.limits.checkpoint_keep, 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_log() {
    let w = world_with(vec![], |c| c.limits.checkpoint_keep = 3).await;
    let (h, dev) = (&w.h, &w.a.device);
    let mut v3 = Vec::new();
    for seq in 1..=3 {
        v3 = put_checkpoint(h, &w.ta, dev, seq).await.unwrap().version;
    }
    // Resending the head (after an unknown result) returns its version;
    // another checkpoint at the head doesn't go in.
    let again = put_checkpoint(h, &w.ta, dev, 3).await.unwrap();
    assert_eq!(again.version, v3);
    let other = CheckpointPut {
        fs: 1,
        device: fp(dev),
        seq: 3,
        checkpoint: checkpoint_of(dev, 1, 4),
    };
    let r: Result<CheckpointVersion, _> = h.call("/v1/checkpoint/put", Some(&w.ta), &other).await;
    assert_eq!(code(r), (409, "version_mismatch".into()));
    // Only head + 1 goes in.
    for seq in [2, 5] {
        assert_eq!(
            code(put_checkpoint(h, &w.ta, dev, seq).await),
            (409, "version_mismatch".into())
        );
    }
    put_checkpoint(h, &w.tb, &w.b.device, 1).await.unwrap();
    let mut all = vec![(fp(dev), 3), (fp(&w.b.device), 1)];
    all.sort();
    assert_eq!(heads(h, &w.tb, 1, None).await, all);
    let unknown = vec![0xEE; 32];
    assert_eq!(
        heads(h, &w.tb, 1, Some(vec![unknown, fp(dev)])).await,
        vec![(fp(dev), 3)]
    );

    // Ranges, limits, and what comes back opens and verifies.
    let l = ckpt_get(h, &w.tb, &fp(dev), 1, None, None).await.unwrap();
    assert_eq!((seqs(&l), l.more), (vec![1, 2, 3], false));
    let l = ckpt_get(h, &w.tb, &fp(dev), 1, None, Some(2))
        .await
        .unwrap();
    assert_eq!((seqs(&l), l.more), (vec![1, 2], true));
    let l = ckpt_get(h, &w.tb, &fp(dev), 2, Some(2), None)
        .await
        .unwrap();
    assert_eq!(seqs(&l), vec![2]);
    let l = ckpt_get(h, &w.tb, &fp(dev), 4, None, None).await.unwrap();
    assert!(l.items.is_empty() && !l.more);
    let l = ckpt_get(h, &w.tb, &fp(dev), u64::MAX, None, None)
        .await
        .unwrap();
    assert!(l.items.is_empty());
    let keys = ckpt_keys();
    let d32: [u8; 32] = fp(dev).try_into().unwrap();
    for item in ckpt_get(h, &w.tb, &fp(dev), 1, None, None)
        .await
        .unwrap()
        .items
    {
        let signed = open_checkpoint(&keys, &d32, item.seq, &item.checkpoint).unwrap();
        let body = verify_checkpoint(&dev.public(), &signed).unwrap();
        assert_eq!(body.seq, item.seq);
    }

    // Pruning keeps the newest `checkpoint_keep`.
    for seq in 4..=5 {
        put_checkpoint(h, &w.ta, dev, seq).await.unwrap();
    }
    let l = ckpt_get(h, &w.tb, &fp(dev), 1, None, None).await.unwrap();
    assert_eq!(seqs(&l), vec![3, 4, 5]);

    // Malformed puts.
    let put = |device: &[u8], seq, checkpoint: Vec<u8>| CheckpointPut {
        fs: 1,
        device: device.to_vec(),
        seq,
        checkpoint,
    };
    let kv_sealed = {
        let mut rng = zen_core::rng::DetRng::new("kv");
        zen_core::seal::seal_value(&keys, b"k", b"v", &mut rng).unwrap()
    };
    for (req, want) in [
        (put(&fp(dev), 6, kv_sealed), (400, "bad_request")),
        (put(&fp(dev), 6, vec![0; 100_000]), (413, "too_large")),
        (
            put(&fp(dev), 0, checkpoint_of(dev, 1, 1)),
            (400, "bad_request"),
        ),
        (
            put(&[1; 31], 6, checkpoint_of(dev, 1, 6)),
            (400, "bad_request"),
        ),
    ] {
        let r: Result<CheckpointVersion, _> = h.call("/v1/checkpoint/put", Some(&w.ta), &req).await;
        let (s, c) = code(r);
        assert_eq!((s, c.as_str()), want);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_ownership() {
    let w = world().await;
    let h = &w.h;
    // Another member's device, and a device nobody registered.
    assert_eq!(
        code(put_checkpoint(h, &w.ta, &w.b.device, 1).await),
        (403, "forbidden".into())
    );
    let extra = DeviceSecret::from_seed(&[0x55; 32]);
    assert_eq!(
        code(put_checkpoint(h, &w.ta, &extra, 1).await),
        (403, "forbidden".into())
    );
    // Once the user registers it, it can.
    let cert = issue_device_cert(&w.a.id, &extra.public(), 1_700_000_000).unwrap();
    let r: DeviceRegistered = h
        .call(
            "/v1/devices/register",
            Some(&w.ta),
            &DeviceRegister { cert },
        )
        .await
        .unwrap();
    assert_eq!(r.device, fp(&extra));
    put_checkpoint(h, &w.ta, &extra, 1).await.unwrap();
    // ... but not for another user.
    assert_eq!(
        code(put_checkpoint(h, &w.tb, &extra, 2).await),
        (403, "forbidden".into())
    );

    // Any right on the fs: c holds only a topic grant on fs 2.
    let on2 = |seq| CheckpointPut {
        fs: 2,
        device: fp(&w.c.device),
        seq,
        checkpoint: checkpoint_of(&w.c.device, 2, seq),
    };
    let _: CheckpointVersion = h
        .call("/v1/checkpoint/put", Some(&w.tc), &on2(1))
        .await
        .unwrap();
    assert_eq!(heads(h, &w.tc, 2, None).await, vec![(fp(&w.c.device), 1)]);
    // Without a right: c on fs 1.
    assert_eq!(
        code(put_checkpoint(h, &w.tc, &w.c.device, 1).await),
        (403, "forbidden".into())
    );
    let r: Result<CheckpointHeadList, _> = h
        .call(
            "/v1/checkpoint/heads",
            Some(&w.tc),
            &CheckpointHeads {
                fs: 1,
                devices: None,
            },
        )
        .await;
    assert_eq!(code(r), (403, "forbidden".into()));
    assert_eq!(
        code(ckpt_get(h, &w.tc, &fp(&w.a.device), 1, None, None).await),
        (403, "forbidden".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_quota() {
    let len = checkpoint_of(&User::new(1).device, 1, 1).len() as u64;
    let limit = FsLimit {
        fs: 1,
        max_keys: None,
        max_bytes: Some(2 * len + len / 2),
    };
    let w = world_with(vec![limit], |c| c.limits.checkpoint_keep = 2).await;
    let h = &w.h;
    for seq in 1..=4 {
        // From 3 on, each put prunes one of the same size.
        put_checkpoint(h, &w.ta, &w.a.device, seq).await.unwrap();
    }
    assert_eq!(
        code(put_checkpoint(h, &w.tb, &w.b.device, 1).await),
        (429, "quota".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_register_and_get() {
    let w = world().await;
    let h = &w.h;
    let dev = DeviceSecret::from_seed(&[0x60; 32]);
    let cert = issue_device_cert(&w.b.id, &dev.public(), 1_700_000_000).unwrap();
    let reg = |cert: &[u8]| DeviceRegister {
        cert: cert.to_vec(),
    };
    for _ in 0..2 {
        let r: DeviceRegistered = h
            .call("/v1/devices/register", Some(&w.tb), &reg(&cert))
            .await
            .unwrap();
        assert_eq!(r.device, fp(&dev));
    }
    // A certificate by another identity.
    let r: Result<DeviceRegistered, _> = h
        .call("/v1/devices/register", Some(&w.ta), &reg(&cert))
        .await;
    assert_eq!(code(r), (400, "bad_request".into()));
    let got: DeviceList = h
        .call(
            "/v1/devices/get",
            Some(&w.ta),
            &DevicesGet {
                devices: vec![ByteBuf::from(vec![9; 32]), ByteBuf::from(fp(&dev))],
            },
        )
        .await
        .unwrap();
    assert_eq!(got.devices.len(), 1);
    assert_eq!(
        (
            &got.devices[0].device,
            &got.devices[0].user,
            &got.devices[0].cert
        ),
        (&fp(&dev), &w.b.fp(), &cert)
    );
    // 64 per user: b has one, so 63 more fit and the next doesn't.
    for i in 0..64u8 {
        let d = DeviceSecret::from_seed(&[i; 32]);
        let cert = issue_device_cert(&w.b.id, &d.public(), 1_700_000_000).unwrap();
        let r: Result<DeviceRegistered, _> = h
            .call("/v1/devices/register", Some(&w.tb), &reg(&cert))
            .await;
        if i < 63 {
            r.unwrap();
        } else {
            assert_eq!(code(r), (429, "quota".into()));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cksub_pushes_heads() {
    let w = world().await;
    let h = &w.h;
    let mut ws = connect(h, &w.tb).await;
    send(&mut ws, &Frame::Cksub { id: 1, fs: 1 }).await;
    assert_eq!(
        recv(&mut ws).await,
        Frame::Ok {
            id: Some(1),
            conn: None
        }
    );
    let ckh = |d: &DeviceSecret, seq| Frame::Ckh {
        id: 1,
        device: fp(d),
        seq,
    };
    put_checkpoint(h, &w.ta, &w.a.device, 1).await.unwrap();
    assert_eq!(recv(&mut ws).await, ckh(&w.a.device, 1));
    // A refused put moves nothing.
    put_checkpoint(h, &w.ta, &w.a.device, 3).await.unwrap_err();
    put_checkpoint(h, &w.ta, &w.a.device, 2).await.unwrap();
    assert_eq!(recv(&mut ws).await, ckh(&w.a.device, 2));
    put_checkpoint(h, &w.tb, &w.b.device, 1).await.unwrap();
    assert_eq!(recv(&mut ws).await, ckh(&w.b.device, 1));

    // Without a right on the fs.
    let mut ws = connect(h, &w.tc).await;
    send(&mut ws, &Frame::Cksub { id: 2, fs: 1 }).await;
    match recv(&mut ws).await {
        Frame::Err {
            id: Some(2), code, ..
        } => assert_eq!(code, "forbidden"),
        f => panic!("{f:?}"),
    }
}

async fn header_put(
    h: &Harness,
    tok: &[u8],
    header: &[u8],
    expect: Option<Vec<u8>>,
) -> Result<HeaderVersion, ApiErr> {
    h.call(
        "/v1/fs/header/put",
        Some(tok),
        &HeaderPut {
            fs: 1,
            header: header.to_vec(),
            expect,
        },
    )
    .await
}

/// A header's bytes as far as the signed form checks them: version, suite
/// and fs, then the rest (formats.md §12).
fn hdr(fs: u32, rest: &[u8]) -> Vec<u8> {
    let mut v = vec![1, 1];
    v.extend_from_slice(&fs.to_be_bytes());
    v.extend_from_slice(rest);
    v
}

async fn header_get(h: &Harness, tok: &[u8]) -> Header {
    h.call("/v1/fs/header/get", Some(tok), &HeaderGet { fs: 1 })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_header_chain() {
    let w = world().await;
    let (h, t) = (&w.h, &w.ta);
    let v0 = header_put(h, t, &hdr(1, b"plain"), None)
        .await
        .unwrap()
        .version;
    let s1 = SignedHeader::sign(&w.a.id, 1, 1, [0; 32], hdr(1, b"one")).unwrap();
    let v1 = header_put(h, t, &s1.encode(), Some(v0))
        .await
        .unwrap()
        .version;
    assert_eq!(header_get(h, t).await.header, Some(s1.encode()));

    let sign = |u: &User, fs, version, prev| {
        SignedHeader::sign(&u.id, fs, version, prev, hdr(fs, b"two"))
            .unwrap()
            .encode()
    };
    let mut flipped = sign(&w.d, 1, 2, s1.hash());
    let n = flipped.len();
    flipped[n - 1] ^= 1;
    for bad in [
        hdr(1, b"plain again"),      // unsigned after signed
        sign(&w.a, 1, 3, s1.hash()), // a version skip
        sign(&w.a, 1, 2, [1; 32]),   // a wrong prev
        sign(&w.a, 2, 2, s1.hash()), // another fs
        sign(&w.b, 1, 2, s1.hash()), // a member who isn't an admin
        flipped,                     // a bad signature
    ] {
        assert_eq!(
            code(header_put(h, t, &bad, Some(v1.clone())).await),
            (400, "bad_request".into())
        );
    }
    assert_eq!(header_get(h, t).await.version, Some(v1.clone()));
    // Another admin signs the next version.
    let s2 = sign(&w.d, 1, 2, s1.hash());
    header_put(h, t, &s2, Some(v1)).await.unwrap();
    assert_eq!(header_get(h, t).await.header, Some(s2));
}

async fn lease(
    h: &Harness,
    tok: &[u8],
    fs: u32,
    name: &[u8],
    token: Option<u64>,
    ttl_ms: Option<u32>,
    release: bool,
) -> Result<NamedLease, ApiErr> {
    h.call(
        "/v1/lease",
        Some(tok),
        &NamedLeasePut {
            fs,
            name: name.to_vec(),
            token,
            ttl_ms,
            release,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn named_lease() {
    let w = world().await;
    let (h, ta, tb) = (&w.h, &w.ta, &w.tb);
    let l = lease(h, ta, 1, b"vol", None, None, false).await.unwrap();
    assert_eq!(l.token, 1);
    assert_eq!(
        code(lease(h, tb, 1, b"vol", None, None, false).await),
        (412, "not_leader".into())
    );
    assert_eq!(
        code(lease(h, tb, 1, b"vol", Some(1), None, false).await),
        (412, "not_leader".into())
    );
    let renewed = lease(h, ta, 1, b"vol", Some(1), None, false).await.unwrap();
    assert_eq!(renewed.token, 1);
    assert!(renewed.expires_version >= l.expires_version);
    // Only the holder releases.
    assert_eq!(
        code(lease(h, tb, 1, b"vol", Some(1), None, true).await),
        (412, "not_leader".into())
    );
    let r = lease(h, ta, 1, b"vol", Some(1), None, true).await.unwrap();
    assert_eq!((r.token, r.expires_version), (1, 0));
    assert_eq!(
        lease(h, tb, 1, b"vol", None, None, false)
            .await
            .unwrap()
            .token,
        2
    );

    // Expiry.
    let l = lease(h, ta, 1, b"short", None, Some(100), false)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        lease(h, tb, 1, b"short", None, None, false)
            .await
            .unwrap()
            .token,
        l.token + 1
    );

    // fs `write`, and the name's length.
    assert_eq!(
        code(lease(h, &w.tc, 2, b"vol", None, None, false).await),
        (403, "forbidden".into())
    );
    for name in [vec![], vec![7; 65]] {
        assert_eq!(
            code(lease(h, ta, 1, &name, None, None, false).await),
            (400, "bad_request".into())
        );
    }
}

fn fenced(key: u8, token: u64, read_version: Option<u64>) -> Commit {
    Commit {
        commit_id: rcid(),
        read_version,
        writes: vec![Write {
            fs: 1,
            key: vec![key; 16],
            value: Some(vec![key; 8]),
        }],
        fence: vec![Fence {
            fs: 1,
            name: b"vol".to_vec(),
            token,
        }],
        ..Default::default()
    }
}

async fn value(h: &Harness, tok: &[u8], key: u8) -> Option<Vec<u8>> {
    let r: KvItems = h
        .call(
            "/v1/kv/get",
            Some(tok),
            &KvGet {
                fs: 1,
                keys: vec![ByteBuf::from(vec![key; 16])],
                read_version: None,
            },
        )
        .await
        .unwrap();
    r.items[0].value.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn fence() {
    let w = world().await;
    let (h, ta, tb) = (&w.h, &w.ta, &w.tb);
    lease(h, ta, 1, b"vol", None, None, false).await.unwrap();
    let first = fenced(1, 1, None);
    let r1: CommitResult = h.call("/v1/commit", Some(ta), &first).await.unwrap();
    assert_eq!(value(h, ta, 1).await, Some(vec![1; 8]));

    // Another device takes the lease: the old holder's commits fail whole.
    lease(h, ta, 1, b"vol", Some(1), None, true).await.unwrap();
    lease(h, tb, 1, b"vol", None, None, false).await.unwrap();
    let r: Result<CommitResult, _> = h.call("/v1/commit", Some(ta), &fenced(2, 1, None)).await;
    assert_eq!(code(r), (412, "claim_lost".into()));
    assert_eq!(value(h, ta, 2).await, None);
    // The holder's token works only for the holder.
    let r: Result<CommitResult, _> = h.call("/v1/commit", Some(ta), &fenced(2, 2, None)).await;
    assert_eq!(code(r), (412, "claim_lost".into()));
    let _: CommitResult = h
        .call("/v1/commit", Some(tb), &fenced(3, 2, None))
        .await
        .unwrap();
    // A replayed commit returns its stored result.
    let again: CommitResult = h.call("/v1/commit", Some(ta), &first).await.unwrap();
    assert_eq!(again.versionstamp, r1.versionstamp);
    // Released: no more fenced commits.
    lease(h, tb, 1, b"vol", Some(2), None, true).await.unwrap();
    let r: Result<CommitResult, _> = h.call("/v1/commit", Some(tb), &fenced(4, 2, None)).await;
    assert_eq!(code(r), (412, "claim_lost".into()));

    // A commit reading before another device acquired conflicts: the
    // lease is in its read set.
    lease(h, tb, 1, b"vol", None, None, false).await.unwrap(); // token 3
    let rv: ReadVersion = h.call("/v1/grv", Some(tb), &Empty {}).await.unwrap();
    lease(h, tb, 1, b"vol", Some(3), None, true).await.unwrap();
    lease(h, ta, 1, b"vol", None, None, false).await.unwrap(); // token 4
    let r: Result<CommitResult, _> = h
        .call("/v1/commit", Some(tb), &fenced(5, 3, Some(rv.read_version)))
        .await;
    assert_eq!(code(r), (409, "conflict".into()));
    assert_eq!(value(h, ta, 5).await, None);

    // Fences need fs `write`; names are 1–64 bytes.
    let mut on2 = fenced(6, 4, None);
    on2.fence[0].fs = 2;
    let r: Result<CommitResult, _> = h.call("/v1/commit", Some(ta), &on2).await;
    assert_eq!(code(r), (403, "forbidden".into()));
    let mut long = fenced(6, 4, None);
    long.fence[0].name = vec![1; 65];
    let r: Result<CommitResult, _> = h.call("/v1/commit", Some(ta), &long).await;
    assert_eq!(code(r), (400, "bad_request".into()));
}

const TREE: [u8; 16] = [0x7E; 16];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn create(h: &Harness, tok: &[u8], node: u8) {
    let c = Commit {
        commit_id: rcid(),
        crdt_ops: vec![CrdtOp::Move {
            fs: 1,
            tree: TREE.to_vec(),
            node: vec![node; 16],
            parent: zen_core::fs::ROOT.to_vec(),
            hlc: zen_core::fs::hlc(now_ms(), node as u16),
            meta: Some(vec![node]),
        }],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(tok), &c).await.unwrap();
}

async fn changes(h: &Harness, tok: &[u8], after: Option<Vec<u8>>, limit: Option<u32>) -> Changes {
    h.call(
        "/v1/fs/tree/changes",
        Some(tok),
        &TreeChanges {
            fs: 1,
            tree: TREE.to_vec(),
            after,
            limit,
            wait_ms: None,
        },
    )
    .await
    .unwrap()
}

async fn chain(h: &Harness, tok: &[u8]) -> TreeChain {
    h.call(
        "/v1/fs/tree/chain",
        Some(tok),
        &TreeRef {
            fs: 1,
            tree: TREE.to_vec(),
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_carry_ops_and_chain() {
    let w = world().await;
    let (h, t) = (&w.h, &w.ta);
    for n in 1..=3 {
        create(h, t, n).await;
    }
    let c = chain(h, t).await;
    assert_eq!(c.ops, 3);
    let all = changes(h, t, None, None).await;
    assert_eq!(all.changes.len(), 3);
    assert_eq!(
        (all.more, all.ops, all.chain.clone()),
        (false, Some(3), Some(c.chain.clone()))
    );
    // Cut short: no header.
    let part = changes(h, t, None, Some(1)).await;
    assert_eq!((part.more, part.ops, part.chain), (true, None, None));
    // The rest after the cursor, then nothing: each complete, with the header.
    let rest = changes(h, t, part.cursor, None).await;
    assert_eq!((rest.changes.len(), rest.ops), (2, Some(3)));
    let none = changes(h, t, rest.cursor, None).await;
    assert!(none.changes.is_empty());
    assert_eq!((none.ops, none.chain), (Some(3), Some(c.chain)));
}

async fn tell(
    h: &Harness,
    tok: &[u8],
    lies: serde_json::Value,
    clear: bool,
) -> Result<Empty, ApiErr> {
    // Lies are CBOR maps; build them from JSON-like values with byte strings.
    #[derive(serde::Serialize)]
    struct Tell {
        lies: Vec<ciborium::Value>,
        clear: bool,
    }
    let lies = match lies {
        serde_json::Value::Array(v) => v.into_iter().map(to_cbor_value).collect(),
        _ => panic!("lies are a list"),
    };
    h.call("/v1/test/adversary", Some(tok), &Tell { lies, clear })
        .await
}

/// JSON to CBOR, with `{"$b": "hex"}` as a byte string.
fn to_cbor_value(v: serde_json::Value) -> ciborium::Value {
    use ciborium::Value as C;
    match v {
        serde_json::Value::Object(m) if m.contains_key("$b") => {
            let hex = m["$b"].as_str().unwrap();
            C::Bytes(
                (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect(),
            )
        }
        serde_json::Value::Object(m) => C::Map(
            m.into_iter()
                .map(|(k, v)| (C::Text(k), to_cbor_value(v)))
                .collect(),
        ),
        serde_json::Value::Number(n) => C::Integer(n.as_u64().unwrap().into()),
        serde_json::Value::String(s) => C::Text(s),
        _ => panic!("unsupported"),
    }
}

fn b(bytes: &[u8]) -> serde_json::Value {
    let hex: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
    serde_json::json!({ "$b": hex })
}

async fn write(h: &Harness, tok: &[u8], key: u8, v: u8) -> CommitResult {
    let c = Commit {
        commit_id: rcid(),
        writes: vec![Write {
            fs: 1,
            key: vec![key; 16],
            value: Some(vec![v; 8]),
        }],
        ..Default::default()
    };
    h.call("/v1/commit", Some(tok), &c).await.unwrap()
}

async fn range(h: &Harness, tok: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let r: KvItems = h
        .call(
            "/v1/kv/range",
            Some(tok),
            &KvRange {
                fs: 1,
                begin: vec![],
                end: None,
                limit: None,
                reverse: None,
                read_version: None,
            },
        )
        .await
        .unwrap();
    r.items
        .into_iter()
        .map(|i| (i.key, i.value.unwrap()))
        .collect()
}

async fn adversary_world() -> World {
    world_with(vec![], |c| c.adversary = true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn adversary_kv() {
    let w = adversary_world().await;
    let (h, t) = (&w.h, &w.ta);
    write(h, t, 1, 1).await;
    let v1: KvItems = h
        .call(
            "/v1/kv/get",
            Some(t),
            &KvGet {
                fs: 1,
                keys: vec![ByteBuf::from(vec![1; 16])],
                read_version: None,
            },
        )
        .await
        .unwrap();
    tell(
        h,
        t,
        serde_json::json!([
            {"kind": "kv", "fs": 1, "key": b(&[1; 16])},
            {"kind": "kv", "fs": 1, "key": b(&[2; 16])},
        ]),
        false,
    )
    .await
    .unwrap();
    write(h, t, 1, 9).await;
    write(h, t, 2, 2).await;
    write(h, t, 3, 3).await;
    let got: KvItems = h
        .call(
            "/v1/kv/get",
            Some(t),
            &KvGet {
                fs: 1,
                keys: vec![ByteBuf::from(vec![1; 16]), ByteBuf::from(vec![2; 16])],
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(got.items[0].value, Some(vec![1; 8]));
    assert_eq!(got.items[0].version, v1.items[0].version);
    assert_eq!(got.items[1].value, None);
    assert_eq!(
        range(h, t).await,
        vec![(vec![1; 16], vec![1; 8]), (vec![3; 16], vec![3; 8])]
    );
    // The truth again.
    tell(h, t, serde_json::json!([]), true).await.unwrap();
    assert_eq!(
        range(h, t).await,
        vec![
            (vec![1; 16], vec![9; 8]),
            (vec![2; 16], vec![2; 8]),
            (vec![3; 16], vec![3; 8])
        ]
    );
}

async fn log_read(h: &Harness, tok: &[u8], t: &[u8]) -> Vec<Vec<u8>> {
    let r: LogEvents = h
        .call(
            "/v1/log/read",
            Some(tok),
            &LogRead {
                fs: 1,
                topic: t.to_vec(),
                after: None,
                key_token: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    r.events.into_iter().map(|e| e.envelope).collect()
}

async fn append_one(h: &Harness, tok: &[u8], t: &[u8], body: &[u8]) {
    let c = Commit {
        commit_id: rcid(),
        append: vec![append(t, Some(&key(1)), body)],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(tok), &c).await.unwrap();
}

async fn quiet(ws: &mut Ws) {
    use futures::StreamExt;
    let r = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    assert!(r.is_err(), "unexpected frame {r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn adversary_topic() {
    let w = adversary_world().await;
    let (h, t) = (&w.h, &w.ta);
    let (t1, t2) = (topic(1), topic(2));
    append_one(h, t, &t1, b"e1").await;
    let mut ws = connect(h, t).await;
    send(
        &mut ws,
        &Frame::Sub {
            id: 1,
            fs: 1,
            topic: Some(t1.clone()),
            prefix: None,
            after: Some(ZERO_OFFSET.to_vec()),
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, Frame::Ok { id: Some(1), .. }));
    assert!(matches!(recv(&mut ws).await, Frame::Ev { envelope, .. } if envelope == b"e1"));

    tell(
        h,
        t,
        serde_json::json!([{"kind": "topic", "fs": 1, "topic": b(&t1)}]),
        false,
    )
    .await
    .unwrap();
    append_one(h, t, &t1, b"e2").await;
    append_one(h, t, &t2, b"other").await;
    assert_eq!(log_read(h, t, &t1).await, vec![b"e1".to_vec()]);
    assert_eq!(log_read(h, t, &t2).await, vec![b"other".to_vec()]);
    quiet(&mut ws).await;

    // Lifted: the stream catches up with no gap.
    tell(h, t, serde_json::json!([]), true).await.unwrap();
    append_one(h, t, &t1, b"e3").await;
    for want in [&b"e2"[..], b"e3"] {
        assert!(matches!(recv(&mut ws).await, Frame::Ev { envelope, .. } if envelope == want));
    }
    assert_eq!(log_read(h, t, &t1).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn adversary_checkpoints() {
    let w = adversary_world().await;
    let (h, t) = (&w.h, &w.ta);
    let dev = &w.a.device;
    put_checkpoint(h, t, dev, 1).await.unwrap();
    tell(
        h,
        t,
        serde_json::json!([{"kind": "checkpoints", "fs": 1, "device": b(&fp(dev))}]),
        false,
    )
    .await
    .unwrap();
    let mut ws = connect(h, &w.tb).await;
    send(&mut ws, &Frame::Cksub { id: 1, fs: 1 }).await;
    assert!(matches!(recv(&mut ws).await, Frame::Ok { id: Some(1), .. }));
    // The device's puts succeed (the server's state is honest) ...
    put_checkpoint(h, t, dev, 2).await.unwrap();
    put_checkpoint(h, t, dev, 3).await.unwrap();
    // ... but readers see seq 1.
    assert_eq!(heads(h, &w.tb, 1, None).await, vec![(fp(dev), 1)]);
    let l = ckpt_get(h, &w.tb, &fp(dev), 1, None, None).await.unwrap();
    assert_eq!(seqs(&l), vec![1]);
    quiet(&mut ws).await;

    tell(h, t, serde_json::json!([]), true).await.unwrap();
    put_checkpoint(h, t, dev, 4).await.unwrap();
    assert_eq!(
        recv(&mut ws).await,
        Frame::Ckh {
            id: 1,
            device: fp(dev),
            seq: 4
        }
    );
    let l = ckpt_get(h, &w.tb, &fp(dev), 1, None, None).await.unwrap();
    assert_eq!(seqs(&l), vec![1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn adversary_header_and_tree() {
    let w = adversary_world().await;
    let (h, t) = (&w.h, &w.ta);
    let v1 = header_put(h, t, b"header 1", None).await.unwrap().version;
    create(h, t, 1).await;
    create(h, t, 2).await;
    let c2 = chain(h, t).await;
    tell(
        h,
        t,
        serde_json::json!([
            {"kind": "header", "fs": 1},
            {"kind": "tree", "fs": 1, "tree": b(&TREE)},
        ]),
        false,
    )
    .await
    .unwrap();
    header_put(h, t, b"header 2", Some(v1.clone()))
        .await
        .unwrap();
    create(h, t, 3).await;
    let got = header_get(h, t).await;
    assert_eq!(
        (got.header, got.version),
        (Some(b"header 1".to_vec()), Some(v1))
    );
    let c = chain(h, t).await;
    assert_eq!((c.ops, &c.chain), (2, &c2.chain));
    let all = changes(h, t, None, None).await;
    assert_eq!((all.changes.len(), all.ops), (2, Some(2)));

    tell(h, t, serde_json::json!([]), true).await.unwrap();
    assert_eq!(chain(h, t).await.ops, 3);
    assert_eq!(header_get(h, t).await.header, Some(b"header 2".to_vec()));
    assert_eq!(changes(h, t, None, None).await.changes.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn adversary_needs_config_and_an_admin() {
    let w = adversary_world().await;
    let r = tell(&w.h, &w.tb, serde_json::json!([]), false).await;
    assert_eq!(code(r), (403, "forbidden".into()));
    let honest = world().await;
    let resp = honest
        .h
        .http
        .post(format!("{}/v1/test/adversary", honest.h.base))
        .header("content-type", CBOR)
        .body(to_cbor(&Empty {}))
        .send()
        .await
        .unwrap();
    // No such endpoint: the static-file fallback answers only GETs.
    assert_eq!(resp.status().as_u16(), 405);
}
