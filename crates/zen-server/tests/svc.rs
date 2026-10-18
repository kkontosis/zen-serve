//! Relayed services, input side (spec/svc.md, api.md §14): accept, claims,
//! reads, backpressure, closing, admission, leases, rights, dead nodes and
//! export.

mod common;

use common::*;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zen_proto::*;
use zen_server::config::{Config, ServiceConfig, SvcMode};
use zen_server::keys;
use zen_server::svc::{Acks, ClaimRec, ConnRec, RH_EOF, encode_head};
use zen_store::Storage;
use zen_store::tuple::Key;

const SVC: [u8; 16] = [0x5E; 16];

fn service(f: impl FnOnce(&mut ServiceConfig)) -> impl FnOnce(&mut Config) {
    move |c| {
        let mut s = ServiceConfig {
            name: "echo".into(),
            fs: 1,
            id: SVC.to_vec(),
            mode: SvcMode::Tcp,
            listen: Some("127.0.0.1:0".parse().unwrap()),
            max_conns: 1024,
            per_ip_rate: 0,
            per_ip_burst: 50,
            window: 262_144,
            out_window: 1_048_576,
            accept_wait_ms: 60_000,
            handshake_secs: 30,
            idle_secs: 300,
            resume: true,
        };
        f(&mut s);
        c.service.push(s);
    }
}

/// A server with the service, claimed by Alice. Returns Alice's token and
/// the service's address.
async fn setup(f: impl FnOnce(&mut ServiceConfig)) -> (Harness, Vec<u8>, SocketAddr) {
    let h = Harness::start_with(service(f)).await;
    let alice = User::new(1);
    h.claim(&alice, &[]).await;
    let tok = h.sign_in(&alice).await.unwrap();
    let addr = h.server.state.svc.local_addr("echo").unwrap();
    (h, tok, addr)
}

async fn lease(
    h: &Harness,
    tok: &[u8],
    id: u8,
    ttl_ms: Option<u32>,
) -> Result<SvcLeaseResult, ApiErr> {
    h.call(
        "/v1/svc/lease",
        Some(tok),
        &SvcLeaseRequest {
            fs: 1,
            svc: SVC.to_vec(),
            lease: vec![id; 16],
            ttl_ms,
            release: None,
        },
    )
    .await
}

async fn next(h: &Harness, tok: &[u8], id: u8, wait_ms: u32) -> Result<SvcClaims, ApiErr> {
    h.call(
        "/v1/svc/next",
        Some(tok),
        &SvcNextRequest {
            fs: 1,
            svc: SVC.to_vec(),
            lease: vec![id; 16],
            limit: None,
            wait_ms: Some(wait_ms),
        },
    )
    .await
}

/// The next connection, waiting for it.
async fn claim(h: &Harness, tok: &[u8], id: u8) -> SvcClaim {
    let mut c = next(h, tok, id, 10_000).await.unwrap();
    assert_eq!(c.conns.len(), 1, "a connection to claim");
    c.conns.remove(0)
}

async fn read(
    h: &Harness,
    tok: &[u8],
    c: &SvcClaim,
    from: u64,
    wait_ms: u32,
) -> Result<SvcReadResult, ApiErr> {
    h.call(
        "/v1/svc/read",
        Some(tok),
        &SvcReadRequest {
            fs: 1,
            svc: SVC.to_vec(),
            conn: c.conn.clone(),
            token: c.token,
            from,
            max_bytes: None,
            wait_ms: Some(wait_ms),
        },
    )
    .await
}

async fn state(h: &Harness, tok: &[u8], c: &SvcClaim) -> Result<SvcClaim, ApiErr> {
    h.call(
        "/v1/svc/state",
        Some(tok),
        &SvcStateRequest {
            fs: 1,
            svc: SVC.to_vec(),
            conn: c.conn.clone(),
            token: c.token,
        },
    )
    .await
}

async fn release(h: &Harness, tok: &[u8], c: &SvcClaim, id: u8, close: bool) {
    let _: Empty = h
        .call(
            "/v1/svc/release",
            Some(tok),
            &SvcReleaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![id; 16],
                conn: c.conn.clone(),
                token: c.token,
                close: Some(close),
            },
        )
        .await
        .unwrap();
}

/// Read exactly `n` bytes of input from `from`, long-polling.
async fn read_n(h: &Harness, tok: &[u8], c: &SvcClaim, from: u64, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < n {
        assert!(Instant::now() < deadline, "input arrives");
        let r = read(h, tok, c, from + out.len() as u64, 5_000)
            .await
            .unwrap();
        for ch in r.chunks {
            assert!(!ch.sealed);
            let at = from + out.len() as u64;
            assert!(ch.off <= at, "chunks are contiguous");
            out.extend_from_slice(&ch.data[(at - ch.off) as usize..]);
        }
    }
    out
}

/// Wait until `f` holds, polling.
async fn eventually<F: AsyncFnMut() -> bool>(what: &str, mut f: F) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether the server closed the peer's connection (FIN or reset).
async fn closed(peer: &mut TcpStream, within: Duration) -> bool {
    let mut b = [0u8; 64];
    match tokio::time::timeout(within, peer.read(&mut b)).await {
        Ok(Ok(0)) | Ok(Err(_)) => true,
        Ok(Ok(_)) | Err(_) => false,
    }
}

/// Whether a new connection is refused: reset or closed at once (the reset
/// may even beat `connect`).
async fn refused(addr: SocketAddr) -> bool {
    match TcpStream::connect(addr).await {
        Ok(mut c) => closed(&mut c, Duration::from_secs(5)).await,
        Err(_) => true,
    }
}

/// Relay keys holding connection or lease data (doorbells and node leases
/// excepted).
async fn relay_data(store: &dyn Storage) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for tag in [
        "rc", "rh", "rw", "ra", "rm", "rz", "ri", "ro", "rv", "rs", "rq", "rl", "rx", "rli", "rnq",
        "rnc",
    ] {
        let p = Key::new().str(tag).finish();
        let mut t = store.begin(None).await.unwrap();
        for (k, _) in t
            .snapshot_get_range(&p, &keys::end_of(&p), 1000, false)
            .await
            .unwrap()
        {
            out.push(k);
        }
    }
    out
}

async fn quota_bytes(store: &dyn Storage) -> i64 {
    let mut t = store.begin(None).await.unwrap();
    t.snapshot_get(&keys::quota(1, "bytes"))
        .await
        .unwrap()
        .map(|v| i64::from_le_bytes(v.try_into().unwrap()))
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn accept_claim_read() {
    let (h, tok, addr) = setup(|_| {}).await;
    let info: SvcInfo = h
        .call(
            "/v1/svc/info",
            Some(&tok),
            &SvcInfoRequest {
                fs: 1,
                svc: SVC.to_vec(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (info.mode.as_str(), info.window, info.resume),
        ("tcp", 262_144, true)
    );
    lease(&h, &tok, 1, None).await.unwrap();
    // Nothing queued: an empty claim list.
    assert!(next(&h, &tok, 1, 0).await.unwrap().conns.is_empty());

    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"hello").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    assert_eq!(c.token, 1);
    assert_eq!(c.peer.addr, Some(peer.local_addr().unwrap().to_string()));
    assert_eq!((c.in_acked, c.out_head, c.written_to), (0, 0, 0));
    assert!(c.state.is_none() && c.pending.is_empty());
    assert_eq!(read_n(&h, &tok, &c, 0, 5).await, b"hello");
    // A chunk that starts before `from` is returned whole.
    let r = read(&h, &tok, &c, 2, 0).await.unwrap();
    assert_eq!((r.chunks[0].off, r.in_head, r.flags), (0, 5, 0));

    // More input wakes a waiting read.
    let (h2, tok2, c2) = (&h, tok.clone(), c.clone());
    let waiting = async move { read(h2, &tok2, &c2, 5, 10_000).await.unwrap() };
    let writing = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        peer.write_all(b" world").await.unwrap();
    };
    let t0 = Instant::now();
    let (r, ()) = tokio::join!(waiting, writing);
    assert!(t0.elapsed() < Duration::from_secs(5));
    let got: Vec<u8> = r.chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert!(b" world".starts_with(&got) && !got.is_empty(), "{got:?}");
    assert_eq!(read_n(&h, &tok, &c, 5, 6).await, b" world");
    let s = state(&h, &tok, &c).await.unwrap();
    assert_eq!((s.token, s.in_head), (1, 11));

    // A waiting `next` wakes on a new connection.
    let waiting = async { next(&h, &tok, 1, 10_000).await.unwrap() };
    let connecting = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        TcpStream::connect(addr).await.unwrap()
    };
    let t0 = Instant::now();
    let (claims, _peer2) = tokio::join!(waiting, connecting);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(claims.conns.len(), 1);
    assert_ne!(claims.conns[0].conn, c.conn);
}

#[tokio::test(flavor = "multi_thread")]
async fn backpressure() {
    let (h, tok, addr) = setup(|s| s.window = 65_536).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let writer = tokio::spawn(async move {
        let data = vec![7u8; 16 << 20];
        peer.write_all(&data).await.unwrap();
        peer
    });
    eventually("the window fills", async || {
        state(&h, &tok, &c).await.unwrap().in_head >= 65_536
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let s = state(&h, &tok, &c).await.unwrap();
    assert!(s.in_head <= 65_536 + 32 * 1024, "{}", s.in_head);
    assert!(!writer.is_finished(), "the peer's writes block");
    writer.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_and_reset() {
    let (h, tok, addr) = setup(|_| {}).await;
    lease(&h, &tok, 1, None).await.unwrap();

    // A FIN sets EOF; the socket stays open for output.
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"x").await.unwrap();
    peer.shutdown().await.unwrap();
    let c = claim(&h, &tok, 1).await;
    assert_eq!(read_n(&h, &tok, &c, 0, 1).await, b"x");
    eventually("EOF", async || {
        read(&h, &tok, &c, 1, 0).await.unwrap().flags & RH_EOF != 0
    })
    .await;
    // At the end with EOF, a read doesn't wait.
    let t0 = Instant::now();
    let r = read(&h, &tok, &c, 1, 10_000).await.unwrap();
    assert!(r.chunks.is_empty() && t0.elapsed() < Duration::from_secs(5));
    assert!(!closed(&mut peer, Duration::from_millis(300)).await);

    // A reset ends and reaps the connection.
    let peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    peer.set_zero_linger().unwrap();
    drop(peer);
    eventually(
        "reaped",
        async || matches!(state(&h, &tok, &c).await, Err(e) if e.1.code == "conn_closed"),
    )
    .await;
    let e = read(&h, &tok, &c, 0, 0).await.unwrap_err();
    assert_eq!((e.0, e.1.code.as_str()), (412, "conn_closed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unclaimed_connections_close() {
    let (h, _tok, addr) = setup(|s| s.accept_wait_ms = 300).await;
    let before = quota_bytes(h.server.state.store.as_ref()).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"anyone?").await.unwrap();
    assert!(closed(&mut peer, Duration::from_secs(10)).await);
    let store = h.server.state.store.clone();
    eventually("its keys are gone", async || {
        relay_data(store.as_ref()).await.is_empty()
    })
    .await;
    assert_eq!(quota_bytes(store.as_ref()).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn admission() {
    let (h, _tok, addr) = setup(|s| s.max_conns = 2).await;
    let svc = &h.server.state.svc;
    let mut a = TcpStream::connect(addr).await.unwrap();
    let _b = TcpStream::connect(addr).await.unwrap();
    eventually("two open", async || svc.open_conns() == 2).await;
    assert!(refused(addr).await, "over max_conns");
    assert!(!closed(&mut a, Duration::from_millis(200)).await);
    a.set_zero_linger().unwrap();
    drop(a);
    eventually("one open", async || svc.open_conns() == 1).await;
    let mut d = TcpStream::connect(addr).await.unwrap();
    assert!(
        !closed(&mut d, Duration::from_millis(300)).await,
        "room again"
    );
    drop(h);

    // The per-address bucket: a burst of 3, refilled slowly.
    let (h, _tok, addr) = setup(|s| {
        s.per_ip_rate = 1;
        s.per_ip_burst = 3;
    })
    .await;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    assert!(refused(addr).await, "over the burst");
    for p in &mut held {
        assert!(!closed(p, Duration::from_millis(100)).await);
    }
    drop(h);
}

#[tokio::test(flavor = "multi_thread")]
async fn leases_and_takeover() {
    let h = Harness::start_with(service(|_| {})).await;
    let (alice, bob) = (User::new(1), User::new(2));
    h.claim(&alice, &[&bob]).await;
    let a = h.sign_in(&alice).await.unwrap();
    let b = h.sign_in(&bob).await.unwrap();
    let addr = h.server.state.svc.local_addr("echo").unwrap();

    // A lease id held by another device.
    lease(&h, &a, 9, None).await.unwrap();
    let e = lease(&h, &b, 9, None).await.unwrap_err();
    assert_eq!(e.1.code, "not_leader");
    let e = next(&h, &b, 9, 0).await.unwrap_err();
    assert_eq!(e.1.code, "not_leader");

    // Alice's lease expires; her fresh connection goes to Bob.
    lease(&h, &a, 1, Some(300)).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"ping").await.unwrap();
    let ca = claim(&h, &a, 1).await;
    assert_eq!(ca.token, 1);
    assert_eq!(read_n(&h, &a, &ca, 0, 4).await, b"ping");
    tokio::time::sleep(Duration::from_millis(400)).await;
    lease(&h, &b, 2, None).await.unwrap();
    // Expiry follows the version clock, which on an idle FoundationDB
    // cluster advances in steps of up to ~2 s.
    let deadline = Instant::now() + Duration::from_secs(15);
    let cb = loop {
        let mut c = next(&h, &b, 2, 2_000).await.unwrap();
        if let Some(c) = c.conns.pop() {
            break c;
        }
        assert!(Instant::now() < deadline, "the connection is requeued");
    };
    assert_eq!((cb.conn.clone(), cb.token), (ca.conn.clone(), 2));
    assert_eq!(read_n(&h, &b, &cb, 0, 4).await, b"ping");
    let e = read(&h, &a, &ca, 0, 0).await.unwrap_err();
    assert_eq!((e.0, e.1.code.as_str()), (412, "claim_lost"));
    // Alice's expired lease is gone; she can take it again.
    lease(&h, &a, 1, None).await.unwrap();

    // Releasing requeues a fresh connection.
    release(&h, &b, &cb, 2, false).await;
    let cb3 = claim(&h, &b, 2).await;
    assert_eq!(cb3.token, 3);
    assert_eq!(state(&h, &b, &cb).await.unwrap_err().1.code, "claim_lost");
    // Releasing with close aborts it: the peer sees a reset.
    release(&h, &b, &cb3, 2, true).await;
    assert!(closed(&mut peer, Duration::from_secs(10)).await);
    eventually(
        "reaped",
        async || matches!(state(&h, &b, &cb3).await, Err(e) if e.1.code == "conn_closed"),
    )
    .await;

    // A released lease requeues its connections at once.
    let _peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &b, 2).await;
    let r: SvcLeaseResult = h
        .call(
            "/v1/svc/lease",
            Some(&b),
            &SvcLeaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![2; 16],
                ttl_ms: None,
                release: Some(true),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.expires_version, 0);
    let again = claim(&h, &a, 1).await;
    assert_eq!(
        (again.conn.clone(), again.token),
        (c.conn.clone(), c.token + 1)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rights() {
    let h = Harness::start_with(service(|_| {})).await;
    let (alice, bob) = (User::new(1), User::new(2));
    let grants = vec![
        fs_grant(&alice, 1, &["read", "write"]),
        topic_grant(&alice, 1, &[], &["read", "append", "consume"]),
        fs_grant(&bob, 1, &["read", "write"]),
        topic_grant(&bob, 1, &SVC, &["append"]),
    ];
    let (signed, _) = signed_acl(&alice, 1, None, &[&alice], &[&alice, &bob], grants, vec![]);
    h.put_acl(signed, h.server.claim_token.clone())
        .await
        .unwrap();
    let b = h.sign_in(&bob).await.unwrap();
    let info = SvcInfoRequest {
        fs: 1,
        svc: SVC.to_vec(),
    };
    let _: SvcInfo = h.call("/v1/svc/info", Some(&b), &info).await.unwrap();
    assert_eq!(lease(&h, &b, 1, None).await.unwrap_err().0, 403);
    assert_eq!(next(&h, &b, 1, 0).await.unwrap_err().0, 403);
    // An unknown service, or one of another fs.
    for (fs, svc) in [(1, vec![0x11; 16]), (2, SVC.to_vec())] {
        let e = h
            .call::<_, SvcInfo>("/v1/svc/info", Some(&b), &SvcInfoRequest { fs, svc })
            .await
            .unwrap_err();
        assert_eq!(e.0, 404);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_nodes_are_reaped() {
    let (h, _tok, addr) = setup(|_| {}).await;
    let st = &h.server.state;
    let store = st.store.clone();
    let before = quota_bytes(store.as_ref()).await;

    // A connection of a node whose lease expired, claimed under a lease.
    let (node, conn, lse) = ([0xDE; 16], [0xC1; 16], [0x1E; 16]);
    let k = |tag| keys::conn(tag, 1, &SVC, &conn);
    let mut t = store.begin(None).await.unwrap();
    let rc = ConnRec {
        node: ByteBuf::from(node.to_vec()),
        peer: SvcPeer {
            addr: Some("192.0.2.1:4000".into()),
            ..Default::default()
        },
        accepted_ms: 1,
        mode: "tcp".into(),
        window: 65_536,
        out_window: 65_536,
        resume: true,
        inbox: None,
    };
    t.set(&keys::node_lease(&node), &1u64.to_be_bytes());
    t.set(&k("rc"), &to_cbor(&rc));
    t.set(&k("rh"), &encode_head(10, 0));
    t.set(&k("ra"), &Acks::default().encode());
    t.set(&k("rw"), &0u64.to_be_bytes());
    t.set(&k("rm"), &ClaimRec::claimed(&lse, &[1; 32], 1));
    t.set(&keys::svc_claims(1, &SVC, &lse).bytes(&conn).finish(), &[]);
    t.set(
        &keys::conn_key("ri", 1, &SVC, &conn).int(0).finish(),
        &[0; 11],
    );
    t.atomic_add(&keys::quota(1, "bytes"), 10);
    t.set(
        &keys::node_conns(&node)
            .int(1)
            .bytes(&SVC)
            .bytes(&conn)
            .finish(),
        &[],
    );
    t.commit().await.unwrap();
    assert!(!relay_data(store.as_ref()).await.is_empty());

    zen_server::sweep_once(st).await.unwrap();
    assert_eq!(relay_data(store.as_ref()).await, Vec::<Vec<u8>>::new());
    assert_eq!(quota_bytes(store.as_ref()).await, before);
    let mut t = store.begin(None).await.unwrap();
    assert!(t.get(&keys::node_lease(&node)).await.unwrap().is_none());
    // This node's own lease is live and kept.
    assert!(
        t.get(&keys::node_lease(&st.svc.node))
            .await
            .unwrap()
            .is_some()
    );
    drop(t);

    // A node that finds its lease gone aborts its sockets.
    let mut peer = TcpStream::connect(addr).await.unwrap();
    eventually("accepted", async || st.svc.open_conns() == 1).await;
    let mut t = store.begin(None).await.unwrap();
    t.clear(&keys::node_lease(&st.svc.node));
    t.commit().await.unwrap();
    assert!(closed(&mut peer, Duration::from_secs(10)).await);
    eventually("reaped", async || {
        relay_data(store.as_ref()).await.is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn export_skips_relay_keys() {
    let (h, tok, addr) = setup(|_| {}).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"secret").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    assert_eq!(read_n(&h, &tok, &c, 0, 6).await, b"secret");
    let store = h.server.state.store.clone();
    assert!(!relay_data(store.as_ref()).await.is_empty());

    let file = h.dir.path().join("dump.zen");
    zen_server::dump::export(
        store.as_ref(),
        "test",
        std::fs::File::create(&file).unwrap(),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    let dst = zen_store::embedded::Embedded::open(
        dir.path().join("zen.redb"),
        zen_store::embedded::Options::default(),
    )
    .unwrap();
    zen_server::dump::import(&dst, std::fs::File::open(&file).unwrap(), false)
        .await
        .unwrap();
    let relay = keys::relay_prefixes();
    let mut t = dst.begin(None).await.unwrap();
    let all = t.get_range(&[], &[0xFF], 100_000, false).await.unwrap();
    assert!(!all.is_empty());
    for (k, _) in all {
        assert!(!relay.iter().any(|p| k.starts_with(p)), "{k:?}");
    }
}
