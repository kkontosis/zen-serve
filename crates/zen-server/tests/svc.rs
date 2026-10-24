//! Relayed services (spec/svc.md, api.md §14): accept, claims, reads,
//! backpressure, closing, admission, leases, rights, dead nodes and export;
//! then the output side: steps, reveals, the writer, takeover and timers;
//! then pushed input and reveals over `/v1/stream`; then `tcp-auth`
//! connections over `/v1/svc/connect`; then the sealed inbox.

mod common;

use common::*;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zen_core::rng::OsRng;
use zen_core::svc::{self as zsvc, ABORT, CLOSE, FIN, SvcKeys};
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
            inbox: None,
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
    read_max(h, tok, c, from, None, wait_ms).await
}

async fn read_max(
    h: &Harness,
    tok: &[u8],
    c: &SvcClaim,
    from: u64,
    max_bytes: Option<u32>,
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
            max_bytes,
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

// ---------------------------------------------------------------- the output side

/// The service's keys, as a handler holds them.
fn svc_keys() -> SvcKeys {
    SvcKeys::from_parts(1, 1, &SVC, [0x4B; 32])
}

/// A handler's view of one connection: what its next step starts from.
#[derive(Clone, Debug)]
struct Held {
    conn: [u8; 16],
    token: u64,
    in_acked: u64,
    out_head: u64,
}

impl Held {
    fn of(c: &SvcClaim) -> Self {
        Held {
            conn: c.conn.as_slice().try_into().unwrap(),
            token: c.token,
            in_acked: c.in_acked,
            out_head: c.out_head,
        }
    }
}

/// A step acking input to `in_to` and sending sealed records `out` (flags,
/// raw), with `state` sealed at the new cursors. Returns the step and the
/// records' `(off, k_r)`.
fn sealed_step(
    k: &SvcKeys,
    c: &Held,
    in_to: u64,
    out: &[(u8, &[u8])],
    state: Option<&[u8]>,
) -> (SvcStep, Vec<(u64, [u8; 32])>) {
    let mut off = c.out_head;
    let (mut recs, mut keys) = (Vec::new(), Vec::new());
    for (flags, raw) in out {
        let (r, k_r) = zsvc::seal_record(k, &c.conn, off, *flags, raw, &mut OsRng).unwrap();
        recs.push(ByteBuf::from(r));
        keys.push((off, *k_r));
        off += raw.len() as u64;
    }
    let state = state.map(|s| zsvc::seal_state(k, &c.conn, in_to, off, s, &mut OsRng).unwrap());
    (
        SvcStep {
            fs: 1,
            svc: SVC.to_vec(),
            conn: c.conn.to_vec(),
            token: c.token,
            in_from: c.in_acked,
            in_to,
            out_from: c.out_head,
            out: recs,
            state,
            resumable: None,
            unseal: None,
        },
        keys,
    )
}

/// A step with plain records (`resume: false`).
fn plain_step(c: &Held, in_to: u64, out: &[(u8, &[u8])]) -> SvcStep {
    SvcStep {
        fs: 1,
        svc: SVC.to_vec(),
        conn: c.conn.to_vec(),
        token: c.token,
        in_from: c.in_acked,
        in_to,
        out_from: c.out_head,
        out: out
            .iter()
            .map(|(f, raw)| ByteBuf::from(zsvc::plain_record(*f, raw).unwrap()))
            .collect(),
        ..Default::default()
    }
}

fn commit_of(steps: Vec<SvcStep>) -> Commit {
    let mut commit_id = vec![0u8; 16];
    getrandom::fill(&mut commit_id).unwrap();
    Commit {
        commit_id,
        svc: steps,
        ..Default::default()
    }
}

async fn commit(h: &Harness, tok: &[u8], c: &Commit) -> Result<CommitResult, ApiErr> {
    h.call("/v1/commit", Some(tok), c).await
}

/// Commit one step, advancing `held`.
async fn step(h: &Harness, tok: &[u8], held: &mut Held, s: SvcStep) -> Result<(), ApiErr> {
    let raw: u64 = s
        .out
        .iter()
        .map(|r| zsvc::parse_record(r).unwrap().raw_len() as u64)
        .sum();
    let in_to = s.in_to;
    commit(h, tok, &commit_of(vec![s])).await?;
    held.in_acked = in_to;
    held.out_head += raw;
    Ok(())
}

async fn reveal(
    h: &Harness,
    tok: &[u8],
    conn: &[u8; 16],
    keys: &[(u64, [u8; 32])],
) -> Result<Vec<u8>, ApiErr> {
    let r: SvcRevealResult = h
        .call(
            "/v1/svc/reveal",
            Some(tok),
            &SvcRevealRequest {
                fs: 1,
                svc: SVC.to_vec(),
                reveals: keys
                    .iter()
                    .map(|(off, k)| SvcReveal {
                        conn: conn.to_vec(),
                        off: *off,
                        key: k.to_vec(),
                    })
                    .collect(),
            },
        )
        .await?;
    Ok(r.results)
}

/// Read exactly `n` bytes from the peer's socket.
async fn peer_reads(peer: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    tokio::time::timeout(Duration::from_secs(10), peer.read_exact(&mut b))
        .await
        .expect("output arrives")
        .unwrap();
    b
}

/// What the peer's next read sees within `within`: `Some(Ok(0))` is EOF,
/// `Some(Err)` a reset, `None` nothing.
async fn peer_next(peer: &mut TcpStream, within: Duration) -> Option<std::io::Result<usize>> {
    let mut b = [0u8; 64];
    tokio::time::timeout(within, peer.read(&mut b)).await.ok()
}

fn is_reset(r: &Option<std::io::Result<usize>>) -> bool {
    matches!(r, Some(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionReset)
}

fn code(e: &ApiErr) -> (u16, &str) {
    (e.0, e.1.code.as_str())
}

/// Whether a connection's keys are gone (it was reaped).
async fn gone(store: &dyn Storage, conn: &[u8; 16]) -> bool {
    let mut t = store.begin(None).await.unwrap();
    for tag in ["rc", "rh", "rw", "ra", "rm", "rz", "rs"] {
        if t.get(&keys::conn(tag, 1, &SVC, conn))
            .await
            .unwrap()
            .is_some()
        {
            return false;
        }
    }
    for tag in ["ri", "ro", "rv"] {
        let p = keys::conn_key(tag, 1, &SVC, conn).finish();
        if !t
            .get_range(&p, &keys::end_of(&p), 1, false)
            .await
            .unwrap()
            .is_empty()
        {
            return false;
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn echo() {
    let (h, tok, addr) = setup(|_| {}).await;
    let store = h.server.state.store.clone();
    let base = quota_bytes(store.as_ref()).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"hello\n").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let held = Held::of(&c);
    assert_eq!(read_n(&h, &tok, &c, 0, 6).await, b"hello\n");

    let (s, keys) = sealed_step(&k, &held, 6, &[(0, b"hello\n")], Some(b"n=1"));
    let cm = commit_of(vec![s]);
    let r1 = commit(&h, &tok, &cm).await.unwrap();
    // A replayed commit_id returns the same result and applies nothing.
    let r2 = commit(&h, &tok, &cm).await.unwrap();
    assert_eq!(r1, r2);
    let st = state(&h, &tok, &c).await.unwrap();
    assert_eq!((st.in_acked, st.out_head, st.written_to), (6, 6, 0));
    assert_eq!(st.pending.len(), 1);
    assert_eq!(st.pending[0].off, 0);
    let state_len = st.state.as_ref().unwrap().len() as i64;
    let plain = zsvc::open_state(&k, &held.conn, 6, 6, st.state.as_ref().unwrap()).unwrap();
    assert_eq!(plain, b"n=1");
    // Nothing is written before the reveal.
    assert!(
        peer_next(&mut peer, Duration::from_millis(300))
            .await
            .is_none()
    );

    assert_eq!(reveal(&h, &tok, &held.conn, &keys).await.unwrap(), [0]);
    assert_eq!(peer_reads(&mut peer, 6).await, b"hello\n");
    eventually("written", async || {
        let s = state(&h, &tok, &c).await.unwrap();
        s.written_to == 6 && s.pending.is_empty()
    })
    .await;
    assert_eq!(reveal(&h, &tok, &held.conn, &keys).await.unwrap(), [1]);
    // Input cleared and output written are given back; the state stays.
    eventually("the quota is back", async || {
        quota_bytes(store.as_ref()).await == base + state_len
    })
    .await;
    let mut t = store.begin(None).await.unwrap();
    let ri = keys::conn_key("ri", 1, &SVC, &held.conn).finish();
    let ro = keys::conn_key("ro", 1, &SVC, &held.conn).finish();
    for p in [ri, ro] {
        assert!(
            t.get_range(&p, &keys::end_of(&p), 10, false)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn step_refusals() {
    let (h, tok, addr) = setup(|s| s.out_window = 65_536).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"abc").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    read_n(&h, &tok, &c, 0, 3).await;
    let mut held = Held::of(&c);

    let try_step = async |s: SvcStep| commit(&h, &tok, &commit_of(vec![s])).await.unwrap_err();
    let stale = Held {
        token: held.token + 1,
        ..held.clone()
    };
    let e = try_step(sealed_step(&k, &stale, 1, &[], None).0).await;
    assert_eq!(code(&e), (412, "claim_lost"));
    for bad in [
        Held {
            in_acked: 1,
            ..held.clone()
        },
        Held {
            out_head: 5,
            ..held.clone()
        },
    ] {
        let e = try_step(sealed_step(&k, &bad, 2, &[], None).0).await;
        assert_eq!(code(&e), (412, "cursor_moved"));
    }
    let e = try_step(sealed_step(&k, &held, 100, &[], None).0).await;
    assert_eq!(code(&e), (412, "cursor_moved"), "in_to beyond in_head");
    let e = try_step(plain_step(&held, 1, &[(0, b"x")])).await;
    assert_eq!(
        code(&e),
        (400, "bad_request"),
        "plain record on a resume service"
    );
    let (s, _) = sealed_step(&k, &held, 1, &[], None);
    let e = commit(&h, &tok, &commit_of(vec![s.clone(), s]))
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        (400, "bad_request"),
        "two steps for one connection"
    );

    // Once there is a state, a step that moves a cursor must carry one.
    let (s, keys) = sealed_step(&k, &held, 1, &[(0, &[1u8; 60_000])], Some(b"s"));
    step(&h, &tok, &mut held, s).await.unwrap();
    let e = try_step(sealed_step(&k, &held, 2, &[], None).0).await;
    assert_eq!(code(&e), (400, "bad_request"), "no state");
    // A step that moves nothing needs none.
    let (s, _) = sealed_step(&k, &held, 1, &[], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    // 60,000 unwritten bytes: 10,000 more exceed the 65,536 window.
    let e = try_step(sealed_step(&k, &held, 1, &[(0, &[2u8; 10_000])], Some(b"s")).0).await;
    assert_eq!(code(&e), (429, "window"));
    // Written, the window has room again.
    assert_eq!(reveal(&h, &tok, &held.conn, &keys).await.unwrap(), [0]);
    assert_eq!(peer_reads(&mut peer, 60_000).await, vec![1u8; 60_000]);
    eventually("written", async || {
        state(&h, &tok, &c).await.unwrap().written_to == 60_000
    })
    .await;
    let (s, _) = sealed_step(&k, &held, 1, &[(FIN, &[2u8; 10_000])], Some(b"s"));
    step(&h, &tok, &mut held, s).await.unwrap();
    // After a FIN: acks only.
    let e = try_step(sealed_step(&k, &held, 2, &[(0, b"x")], Some(b"s")).0).await;
    assert_eq!(code(&e), (412, "conn_closed"), "records after a FIN");
    let (s, _) = sealed_step(&k, &held, 2, &[], Some(b"s"));
    step(&h, &tok, &mut held, s).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reveal_codes() {
    let (h, tok, addr) = setup(|_| {}).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(0, b"one"), (0, b"two")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    let conn = held.conn;
    assert_eq!(
        reveal(&h, &tok, &conn, &[(100, keys[0].1), (0, [9; 32])])
            .await
            .unwrap(),
        [2, 3],
        "beyond out_head; a wrong key"
    );
    // The second record waits for the first.
    assert_eq!(reveal(&h, &tok, &conn, &keys[1..]).await.unwrap(), [0]);
    assert!(
        peer_next(&mut peer, Duration::from_millis(300))
            .await
            .is_none()
    );
    assert_eq!(reveal(&h, &tok, &conn, &keys).await.unwrap(), [0, 1]);
    assert_eq!(peer_reads(&mut peer, 6).await, b"onetwo");
    // Malformed requests.
    let e = h
        .call::<_, SvcRevealResult>(
            "/v1/svc/reveal",
            Some(&tok),
            &SvcRevealRequest {
                fs: 1,
                svc: SVC.to_vec(),
                reveals: vec![
                    SvcReveal {
                        conn: conn.to_vec(),
                        off: 0,
                        key: vec![0; 32],
                    };
                    1001
                ],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.0, 413);

    // A record whose key matches but whose body was tampered with: the
    // holder's reveal aborts the connection.
    let (mut s, keys) = sealed_step(&k, &held, 0, &[(0, b"three")], None);
    let r = s.out[0].as_mut_slice();
    let last = r.len() - 1;
    r[last] ^= 1;
    step(&h, &tok, &mut held, s).await.unwrap();
    assert_eq!(reveal(&h, &tok, &conn, &keys).await.unwrap(), [3]);
    assert!(is_reset(
        &peer_next(&mut peer, Duration::from_secs(10)).await
    ));
    eventually(
        "reaped",
        async || matches!(state(&h, &tok, &c).await, Err(e) if e.1.code == "conn_closed"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fin_close_abort() {
    let (h, tok, addr) = setup(|_| {}).await;
    let store = h.server.state.store.clone();
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();

    // FIN: the peer reads EOF and can still write; once it shuts down too
    // and all input is acked, the connection ends.
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(FIN, b"bye")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    reveal(&h, &tok, &held.conn, &keys).await.unwrap();
    assert_eq!(peer_reads(&mut peer, 3).await, b"bye");
    assert!(matches!(
        peer_next(&mut peer, Duration::from_secs(10)).await,
        Some(Ok(0))
    ));
    peer.write_all(b"more").await.unwrap();
    assert_eq!(read_n(&h, &tok, &c, 0, 4).await, b"more");
    let (s, _) = sealed_step(&k, &held, 4, &[], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    assert!(
        state(&h, &tok, &c).await.is_ok(),
        "open until the peer's FIN"
    );
    peer.shutdown().await.unwrap();
    eventually(
        "ended",
        async || matches!(state(&h, &tok, &c).await, Err(e) if e.1.code == "conn_closed"),
    )
    .await;

    // Close: the bytes, then EOF.
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(0, b"do"), (CLOSE, b"ne")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    reveal(&h, &tok, &held.conn, &keys).await.unwrap();
    assert_eq!(peer_reads(&mut peer, 4).await, b"done");
    assert!(matches!(
        peer_next(&mut peer, Duration::from_secs(10)).await,
        Some(Ok(0))
    ));
    let e = commit(
        &h,
        &tok,
        &commit_of(vec![sealed_step(&k, &held, 0, &[], None).0]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), (412, "conn_closed"));

    // Abort: a reset.
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(ABORT, b"")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    reveal(&h, &tok, &held.conn, &keys).await.unwrap();
    assert!(is_reset(
        &peer_next(&mut peer, Duration::from_secs(10)).await
    ));
    eventually("reaped", async || gone(store.as_ref(), &held.conn).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn takeover_transcript() {
    let h = Harness::start_with(service(|_| {})).await;
    let (alice, bob) = (User::new(1), User::new(2));
    h.claim(&alice, &[&bob]).await;
    let a = h.sign_in(&alice).await.unwrap();
    let b = h.sign_in(&bob).await.unwrap();
    let addr = h.server.state.svc.local_addr("echo").unwrap();
    let k = svc_keys();
    lease(&h, &a, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"l0\nl1\nl2\nl3\nl4\n").await.unwrap();
    let ca = claim(&h, &a, 1).await;
    let mut held = Held::of(&ca);
    read_n(&h, &a, &ca, 0, 15).await;

    // Alice echoes three lines, counting them in the state.
    for i in 0..3u64 {
        let line = format!("l{i}\n");
        let n = format!("{}", i + 1);
        let (mut s, keys) = sealed_step(
            &k,
            &held,
            held.in_acked + 3,
            &[(0, line.as_bytes())],
            Some(n.as_bytes()),
        );
        s.resumable = Some(true);
        step(&h, &a, &mut held, s).await.unwrap();
        assert_eq!(reveal(&h, &a, &held.conn, &keys).await.unwrap(), [0]);
    }
    // The fourth step commits, but Alice dies before revealing it.
    let (s, _lost) = sealed_step(&k, &held, 12, &[(0, b"l3\n")], Some(b"4"));
    step(&h, &a, &mut held, s).await.unwrap();
    lease(&h, &a, 1, Some(300)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Bob takes over: the state opens at the cursors, the pending record's
    // key comes from its escrow.
    lease(&h, &b, 2, None).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let cb = loop {
        let mut c = next(&h, &b, 2, 2_000).await.unwrap();
        if let Some(c) = c.conns.pop() {
            break c;
        }
        assert!(Instant::now() < deadline, "the connection is requeued");
    };
    assert_eq!((cb.token, cb.in_acked, cb.out_head), (2, 12, 12));
    let mut hb = Held::of(&cb);
    let n = zsvc::open_state(&k, &hb.conn, 12, 12, cb.state.as_ref().unwrap()).unwrap();
    assert_eq!(n, b"4");
    let keys: Vec<(u64, [u8; 32])> = cb
        .pending
        .iter()
        .map(|p| {
            let k_r = zsvc::open_key_escrow(&k, &hb.conn, p.off, &p.escrow).unwrap();
            (p.off, *k_r)
        })
        .collect();
    assert_eq!(keys.len(), 1);
    assert_eq!(reveal(&h, &b, &hb.conn, &keys).await.unwrap(), [0]);
    assert_eq!(read_n(&h, &b, &cb, 12, 3).await, b"l4\n");
    let (s, keys) = sealed_step(&k, &hb, 15, &[(0, b"l4\n")], Some(b"5"));
    step(&h, &b, &mut hb, s).await.unwrap();
    assert_eq!(reveal(&h, &b, &hb.conn, &keys).await.unwrap(), [0]);
    // Alice's late step is fenced.
    let e = commit(
        &h,
        &a,
        &commit_of(vec![sealed_step(&k, &held, 15, &[], Some(b"x")).0]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), (412, "claim_lost"));

    // Every echo exactly once, in order.
    assert_eq!(peer_reads(&mut peer, 15).await, b"l0\nl1\nl2\nl3\nl4\n");
    assert!(
        peer_next(&mut peer, Duration::from_millis(300))
            .await
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_off() {
    let (h, tok, addr) = setup(|s| s.resume = false).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"hi").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    read_n(&h, &tok, &c, 0, 2).await;
    let e = commit(
        &h,
        &tok,
        &commit_of(vec![sealed_step(&k, &held, 2, &[(0, b"x")], None).0]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), (400, "bad_request"), "sealed records");
    let mut s = plain_step(&held, 2, &[]);
    s.resumable = Some(true);
    let e = commit(&h, &tok, &commit_of(vec![s])).await.unwrap_err();
    assert_eq!(code(&e), (400, "bad_request"), "never resumable");
    // Plain records are written as they commit.
    let s = plain_step(&held, 2, &[(0, b"HI")]);
    step(&h, &tok, &mut held, s).await.unwrap();
    assert_eq!(peer_reads(&mut peer, 2).await, b"HI");
    assert_eq!(
        reveal(&h, &tok, &held.conn, &[(0, [0; 32])]).await.unwrap(),
        [1]
    );

    // The claim lapses after a step: the connection is aborted.
    lease(&h, &tok, 1, Some(300)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let store = h.server.state.store.clone();
    // A lease call reaps expired leases.
    eventually("aborted", async || {
        lease(&h, &tok, 2, None).await.unwrap();
        gone(store.as_ref(), &held.conn).await
    })
    .await;
    assert!(
        next(&h, &tok, 2, 0).await.unwrap().conns.is_empty(),
        "not requeued"
    );
    assert!(is_reset(
        &peer_next(&mut peer, Duration::from_secs(10)).await
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn timers() {
    // No step within handshake_secs.
    let (h, tok, addr) = setup(|s| s.handshake_secs = 1).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    let _c = claim(&h, &tok, 1).await;
    let t0 = Instant::now();
    assert!(is_reset(
        &peer_next(&mut peer, Duration::from_secs(10)).await
    ));
    assert!(t0.elapsed() < Duration::from_secs(5));
    drop(h);

    // Idle after a step; not while input waits for the handler.
    let (h, tok, addr) = setup(|s| s.idle_secs = 1).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"a").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    read_n(&h, &tok, &c, 0, 1).await;
    let mut waiting = TcpStream::connect(addr).await.unwrap();
    waiting.write_all(b"unacked").await.unwrap();
    let cw = claim(&h, &tok, 1).await;
    read_n(&h, &tok, &cw, 0, 7).await;
    let (s, _) = sealed_step(&k, &held, 1, &[], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    let t0 = Instant::now();
    assert!(is_reset(
        &peer_next(&mut peer, Duration::from_secs(10)).await
    ));
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(
        peer_next(&mut waiting, Duration::from_millis(2_500))
            .await
            .is_none(),
        "bytes wait: not idle"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn many_connections() {
    let (h, tok, addr) = setup(|s| s.max_conns = 1100).await;
    let k = svc_keys();
    let svc = &h.server.state.svc;
    let mut idle = Vec::new();
    for _ in 0..1000 {
        idle.push(TcpStream::connect(addr).await.unwrap());
    }
    eventually("all accepted", async || svc.open_conns() == 1000).await;
    // The queue holds 1,000 unclaimed connections; an echo still completes
    // promptly on a new one, claimed directly.
    lease(&h, &tok, 1, None).await.unwrap();
    let t0 = Instant::now();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"ping").await.unwrap();
    // Claims are oldest first, so the new connection is found by draining.
    let mut c = None;
    while c.is_none() {
        let got: SvcClaims = h
            .call(
                "/v1/svc/next",
                Some(&tok),
                &SvcNextRequest {
                    fs: 1,
                    svc: SVC.to_vec(),
                    lease: vec![1; 16],
                    limit: Some(64),
                    wait_ms: Some(1_000),
                },
            )
            .await
            .unwrap();
        assert!(!got.conns.is_empty(), "the new connection is queued");
        c = got
            .conns
            .into_iter()
            .find(|c| c.peer.addr == Some(peer.local_addr().unwrap().to_string()));
    }
    let drained = t0.elapsed();
    let t1 = Instant::now();
    let c = c.unwrap();
    let mut held = Held::of(&c);
    assert_eq!(read_n(&h, &tok, &c, 0, 4).await, b"ping");
    let (s, keys) = sealed_step(&k, &held, 4, &[(0, b"pong")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    reveal(&h, &tok, &held.conn, &keys).await.unwrap();
    assert_eq!(peer_reads(&mut peer, 4).await, b"pong");
    assert!(
        t1.elapsed() < Duration::from_secs(2),
        "echo took {:?} (draining the queue {drained:?})",
        t1.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn info_lists_svc() {
    let h = Harness::start().await;
    let i: Info = h.get("/v1/info").await;
    assert!(i.features.iter().any(|f| f == "svc"));
}

// ---------------------------------------------------------------- stream push

const SUB: u32 = 9;

/// A stream socket subscribed to lease `lease`'s input.
async fn svc_sub(h: &Harness, tok: &[u8], lease: u8) -> Ws {
    let mut ws = connect(h, tok).await;
    send(
        &mut ws,
        &Frame::SvcSub {
            id: SUB,
            fs: 1,
            svc: SVC.to_vec(),
            lease: vec![lease; 16],
        },
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        Frame::Ok {
            id: Some(SUB),
            conn: None
        }
    );
    ws
}

/// The next frame within `within`, if any.
async fn frame_within(ws: &mut Ws, within: Duration) -> Option<Frame> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return Some(from_cbor(&b).unwrap()),
            Ok(Some(Ok(_))) => continue,
            Ok(_) | Err(_) => return None,
        }
    }
}

/// The next frame that isn't `svc_ready`.
async fn next_push(ws: &mut Ws) -> Frame {
    loop {
        match recv(ws).await {
            Frame::SvcReady { id } => assert_eq!(id, SUB),
            f => return f,
        }
    }
}

/// Wait for a `svc_ready`, skipping other frames.
async fn ready(ws: &mut Ws) {
    while recv(ws).await != (Frame::SvcReady { id: SUB }) {}
}

/// What a subscription pushed of each connection: the input from where it
/// was first asked for, and whether the EOF came.
#[derive(Default)]
struct Pushed {
    from: std::collections::HashMap<Vec<u8>, u64>,
    data: std::collections::HashMap<Vec<u8>, Vec<u8>>,
    eof: std::collections::HashSet<Vec<u8>>,
}

impl Pushed {
    /// Take one `svc_in`. Within a claim nothing is pushed twice, so each
    /// chunk starts where the last one ended (the first may start before
    /// the cursor asked for).
    fn take(&mut self, f: Frame) {
        let Frame::SvcIn {
            id,
            conn,
            off,
            data,
            sealed,
            flags,
        } = f
        else {
            panic!("expected svc_in, got {f:?}");
        };
        assert_eq!(id, SUB);
        assert!(!sealed);
        let from = *self.from.entry(conn.clone()).or_insert(off);
        let got = self.data.entry(conn.clone()).or_default();
        let end = from + got.len() as u64;
        if data.is_empty() {
            assert!(flags & 1 != 0, "an empty svc_in is the EOF");
            assert_eq!(off, end, "the EOF sits at in_head");
            assert!(self.eof.insert(conn), "the EOF is pushed once");
            return;
        }
        assert!(!self.eof.contains(&conn), "nothing after the EOF");
        if got.is_empty() {
            assert!(off <= from);
            got.extend_from_slice(&data[(from - off) as usize..]);
        } else {
            assert_eq!(off, end, "chunks are contiguous and not repeated");
            got.extend_from_slice(&data);
        }
    }

    /// Pushes until `conn` has `n` bytes from `from` (the first svc_in of a
    /// connection fixes `from`).
    async fn until(&mut self, ws: &mut Ws, conn: &[u8], from: u64, n: usize) -> Vec<u8> {
        self.from.entry(conn.to_vec()).or_insert(from);
        while self.data.get(conn).map_or(0, Vec::len) < n {
            let f = next_push(ws).await;
            self.take(f);
        }
        self.data[conn].clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn push_input() {
    let (h, tok, addr) = setup(|_| {}).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut ws = svc_sub(&h, &tok, 1).await;
    // The queue is empty: no svc_ready yet.
    assert_eq!(
        frame_within(&mut ws, Duration::from_millis(300)).await,
        None
    );
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"hello").await.unwrap();
    ready(&mut ws).await;
    let c = claim(&h, &tok, 1).await;
    let mut p = Pushed::default();
    assert_eq!(p.until(&mut ws, &c.conn, 0, 5).await, b"hello");

    let mut each = Vec::new();
    for i in 0..10 {
        let t0 = Instant::now();
        let line = format!("line {i}\n");
        peer.write_all(line.as_bytes()).await.unwrap();
        let have = p.data[&c.conn].len();
        let got = p.until(&mut ws, &c.conn, 0, have + line.len()).await;
        assert!(got.ends_with(line.as_bytes()));
        each.push(t0.elapsed());
    }
    let worst = each.iter().max().copied().unwrap_or_default();
    println!("push latency, worst of 10: {worst:?}");
    // All ten on failure: one stalled push, or every one slow (a loaded runner).
    assert!(
        worst < Duration::from_secs(1),
        "pushed within 1 s: {each:?}"
    );

    peer.shutdown().await.unwrap();
    let f = next_push(&mut ws).await;
    p.take(f);
    assert!(p.eof.contains(&c.conn));
    assert_eq!(
        frame_within(&mut ws, Duration::from_millis(300)).await,
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn push_resumes_after_reconnect() {
    let (h, tok, addr) = setup(|_| {}).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"0123456789").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut p = Pushed::default();
    assert_eq!(p.until(&mut ws, &c.conn, 0, 10).await, b"0123456789");
    let (s, _) = sealed_step(&k, &held, 3, &[], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    drop(ws);

    peer.write_all(b"abc").await.unwrap();
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut p = Pushed::default();
    // From the chunk holding in_acked, with no gap.
    let first = next_push(&mut ws).await;
    let Frame::SvcIn { off, ref data, .. } = first else {
        panic!("{first:?}");
    };
    assert!(
        off <= 3 && off + data.len() as u64 > 3,
        "starts at in_acked"
    );
    p.from.insert(c.conn.clone(), 3);
    p.take(first);
    assert_eq!(p.until(&mut ws, &c.conn, 3, 10).await, b"3456789abc");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_follows_claims() {
    let (h, tok, addr) = setup(|_| {}).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    lease(&h, &tok, 2, None).await.unwrap();
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"abcdef").await.unwrap();
    let c1 = claim(&h, &tok, 1).await;
    let mut p = Pushed::default();
    assert_eq!(p.until(&mut ws, &c1.conn, 0, 6).await, b"abcdef");
    let mut held = Held::of(&c1);
    let (mut s, _) = sealed_step(&k, &held, 2, &[], Some(b"s"));
    s.resumable = Some(true);
    step(&h, &tok, &mut held, s).await.unwrap();

    // Released and claimed again under the same lease: pushing restarts
    // at in_acked.
    release(&h, &tok, &c1, 1, false).await;
    let c2 = claim(&h, &tok, 1).await;
    assert_eq!(c2.token, c1.token + 1);
    let mut p = Pushed::default();
    assert_eq!(p.until(&mut ws, &c2.conn, 2, 4).await, b"cdef");

    // Claimed under another lease: no more pushes on this subscription.
    release(&h, &tok, &c2, 1, false).await;
    let c3 = claim(&h, &tok, 2).await;
    peer.write_all(b"zz").await.unwrap();
    let mut other = svc_sub(&h, &tok, 2).await;
    let mut q = Pushed::default();
    assert_eq!(q.until(&mut other, &c3.conn, 2, 6).await, b"cdefzz");
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(f) = frame_within(&mut ws, deadline - Instant::now()).await {
        assert!(matches!(f, Frame::SvcReady { .. }), "no push: {f:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn push_refusals() {
    let h = Harness::start_with(service(|_| {})).await;
    let (alice, bob, carol) = (User::new(1), User::new(2), User::new(3));
    let grants = vec![
        fs_grant(&alice, 1, &["read", "write"]),
        topic_grant(&alice, 1, &[], &["read", "append", "consume"]),
        fs_grant(&bob, 1, &["read", "write"]),
        topic_grant(&bob, 1, &SVC, &["consume"]),
        fs_grant(&carol, 1, &["read", "write"]),
        topic_grant(&carol, 1, &SVC, &["append"]),
    ];
    let (signed, _) = signed_acl(
        &alice,
        1,
        None,
        &[&alice],
        &[&alice, &bob, &carol],
        grants,
        vec![],
    );
    h.put_acl(signed, h.server.claim_token.clone())
        .await
        .unwrap();
    let a = h.sign_in(&alice).await.unwrap();
    let b = h.sign_in(&bob).await.unwrap();
    let c = h.sign_in(&carol).await.unwrap();
    let addr = h.server.state.svc.local_addr("echo").unwrap();
    lease(&h, &a, 1, None).await.unwrap();
    let refused = async |tok: &[u8], lease: u8| {
        let mut ws = connect(&h, tok).await;
        send(
            &mut ws,
            &Frame::SvcSub {
                id: SUB,
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![lease; 16],
            },
        )
        .await;
        match recv(&mut ws).await {
            Frame::Err { id, code, .. } => {
                assert_eq!(id, Some(SUB));
                code
            }
            f => panic!("{f:?}"),
        }
    };
    assert_eq!(refused(&c, 1).await, "forbidden");
    assert_eq!(refused(&a, 2).await, "not_leader");
    assert_eq!(refused(&b, 1).await, "not_leader", "another device's lease");

    // Releasing the lease ends the subscription and its input log.
    let mut ws = svc_sub(&h, &a, 1).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"x").await.unwrap();
    let cl = claim(&h, &a, 1).await;
    Pushed::default().until(&mut ws, &cl.conn, 0, 1).await;
    let _: SvcLeaseResult = h
        .call(
            "/v1/svc/lease",
            Some(&a),
            &SvcLeaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![1; 16],
                ttl_ms: None,
                release: Some(true),
            },
        )
        .await
        .unwrap();
    match next_push(&mut ws).await {
        Frame::Err { id, code, .. } => assert_eq!((id, code.as_str()), (Some(SUB), "not_leader")),
        f => panic!("{f:?}"),
    }
    let rli = Key::new().str("rli").finish();
    assert!(
        relay_data(h.server.state.store.as_ref())
            .await
            .iter()
            .all(|k| !k.starts_with(&rli)),
        "no input log left"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn push_many() {
    let (h, tok, addr) = setup(|_| {}).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut peers = Vec::new();
    for _ in 0..100 {
        peers.push(TcpStream::connect(addr).await.unwrap());
    }
    let mut conns = std::collections::HashMap::new();
    while conns.len() < 100 {
        let got: SvcClaims = h
            .call(
                "/v1/svc/next",
                Some(&tok),
                &SvcNextRequest {
                    fs: 1,
                    svc: SVC.to_vec(),
                    lease: vec![1; 16],
                    limit: Some(64),
                    wait_ms: Some(5_000),
                },
            )
            .await
            .unwrap();
        for c in got.conns {
            conns.insert(c.peer.addr.clone().unwrap(), c.conn);
        }
    }
    let t0 = Instant::now();
    for (i, peer) in peers.iter_mut().enumerate() {
        peer.write_all(format!("line {i}\n").as_bytes())
            .await
            .unwrap();
    }
    let mut p = Pushed::default();
    let want = |i: usize| format!("line {i}\n").into_bytes();
    let done = |p: &Pushed| {
        peers.iter().enumerate().all(|(i, peer)| {
            let conn = &conns[&peer.local_addr().unwrap().to_string()];
            p.data.get(conn).is_some_and(|d| *d == want(i))
        })
    };
    while !done(&p) {
        let f = next_push(&mut ws).await;
        p.take(f);
    }
    let took = t0.elapsed();
    println!("100 connections pushed in {took:?}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reveal_over_the_stream() {
    let (h, tok, addr) = setup(|_| {}).await;
    let k = svc_keys();
    lease(&h, &tok, 1, None).await.unwrap();
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"hi").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let mut held = Held::of(&c);
    Pushed::default().until(&mut ws, &c.conn, 0, 2).await;
    let (s, keys) = sealed_step(&k, &held, 2, &[(0, b"hi")], None);
    step(&h, &tok, &mut held, s).await.unwrap();
    let reveals = |n: usize| {
        (0..n)
            .map(|i| SvcReveal {
                conn: held.conn.to_vec(),
                off: if i == 0 { keys[0].0 } else { 100 + i as u64 },
                key: keys[0].1.to_vec(),
            })
            .collect::<Vec<_>>()
    };
    send(
        &mut ws,
        &Frame::SvcReveal {
            id: 5,
            fs: 1,
            svc: SVC.to_vec(),
            reveals: reveals(1),
        },
    )
    .await;
    assert_eq!(
        next_push(&mut ws).await,
        Frame::SvcRevealed {
            id: 5,
            results: vec![0]
        }
    );
    assert_eq!(peer_reads(&mut peer, 2).await, b"hi");

    send(
        &mut ws,
        &Frame::SvcReveal {
            id: 6,
            fs: 1,
            svc: SVC.to_vec(),
            reveals: reveals(1_001),
        },
    )
    .await;
    match next_push(&mut ws).await {
        Frame::Err { id, code, .. } => assert_eq!((id, code.as_str()), (Some(6), "too_large")),
        f => panic!("{f:?}"),
    }
}

// ---------------------------------------------------------------- tcp-auth

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// A `tcp` service's id, next to the `tcp-auth` one.
const TCP_SVC: [u8; 16] = [0x7C; 16];

/// A server with a `tcp-auth` service `SVC` and a `tcp` service `TCP_SVC`.
/// Alice can do anything; Bob may only `consume`. Returns their tokens.
async fn auth_setup(f: impl FnOnce(&mut ServiceConfig)) -> (Harness, Vec<u8>, Vec<u8>) {
    let h = Harness::start_with(|c| {
        service(|s| {
            s.mode = SvcMode::TcpAuth;
            s.listen = None;
            f(s);
        })(c);
        service(|s| {
            s.name = "plain".into();
            s.id = TCP_SVC.to_vec();
        })(c);
    })
    .await;
    let (alice, bob) = (User::new(1), User::new(2));
    let grants = vec![
        fs_grant(&alice, 1, &["read", "write"]),
        topic_grant(&alice, 1, &[], &["read", "append", "consume"]),
        fs_grant(&bob, 1, &["read", "write"]),
        topic_grant(&bob, 1, &[], &["consume"]),
    ];
    let (signed, _) = signed_acl(&alice, 1, None, &[&alice], &[&alice, &bob], grants, vec![]);
    h.put_acl(signed, h.server.claim_token.clone())
        .await
        .unwrap();
    let a = h.sign_in(&alice).await.unwrap();
    let b = h.sign_in(&bob).await.unwrap();
    (h, a, b)
}

/// A connector: Alice's WebSocket, authenticated by header.
async fn connector(h: &Harness, tok: &[u8]) -> (Ws, Vec<u8>) {
    svc_connect(h, ConnectAuth::Header(tok), &SVC)
        .await
        .unwrap()
}

/// The next message within `within`, skipping pings and pongs. `None`:
/// nothing came. The end of the stream or an error reads as a close
/// without a frame.
async fn ws_next(ws: &mut Ws, within: Duration) -> Option<Message> {
    let until = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(until, ws.next()).await {
            Err(_) => return None,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Ok(m))) => return Some(m),
            Ok(_) => return Some(Message::Close(None)),
        }
    }
}

/// The close code of a close message.
fn close_code(m: &Option<Message>) -> Option<u16> {
    match m {
        Some(Message::Close(Some(f))) => Some(f.code.into()),
        _ => None,
    }
}

/// Exactly `n` bytes of binary frames; each at most 64 KiB.
async fn ws_reads(ws: &mut Ws, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < n {
        match ws_next(ws, Duration::from_secs(10)).await {
            Some(Message::Binary(b)) => {
                assert!(b.len() <= 65_536, "a frame of {} bytes", b.len());
                out.extend_from_slice(&b);
            }
            m => panic!("expected data, got {m:?}"),
        }
    }
    assert_eq!(out.len(), n);
    out
}

async fn ws_write(ws: &mut Ws, data: &[u8]) {
    ws.send(Message::Binary(data.to_vec().into()))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_echo() {
    let (h, a, _) = auth_setup(|_| {}).await;
    let k = svc_keys();
    lease(&h, &a, 1, None).await.unwrap();
    let (mut ws, conn) = connector(&h, &a).await;
    ws_write(&mut ws, b"hello\n").await;
    let c = claim(&h, &a, 1).await;
    assert_eq!(c.conn, conn);
    // The peer is the connector: Alice's user and device.
    let alice = User::new(1);
    assert_eq!(c.peer.addr, None);
    assert_eq!(c.peer.user.as_deref(), Some(&alice.fp()[..]));
    assert_eq!(
        c.peer.device.as_deref(),
        Some(&alice.device.public().fingerprint()[..])
    );
    let mut held = Held::of(&c);
    assert_eq!(read_n(&h, &a, &c, 0, 6).await, b"hello\n");
    let (s, keys) = sealed_step(&k, &held, 6, &[(0, b"hello\n")], None);
    step(&h, &a, &mut held, s).await.unwrap();
    reveal(&h, &a, &held.conn, &keys).await.unwrap();
    assert_eq!(ws_reads(&mut ws, 6).await, b"hello\n");

    // The client's "fin" is the EOF flag; the handler can still write.
    ws.send(Message::Text("fin".into())).await.unwrap();
    eventually("the EOF flag", async || {
        read(&h, &a, &c, 6, 0).await.unwrap().flags & 1 == 1
    })
    .await;
    // A FIN record arrives as "fin"; with the client's FIN in and all
    // input acked, the connection ends with close 1000.
    let (s, keys) = sealed_step(&k, &held, 6, &[(FIN, b"bye\n")], None);
    step(&h, &a, &mut held, s).await.unwrap();
    reveal(&h, &a, &held.conn, &keys).await.unwrap();
    assert_eq!(ws_reads(&mut ws, 4).await, b"bye\n");
    assert!(
        matches!(ws_next(&mut ws, Duration::from_secs(10)).await, Some(Message::Text(t)) if t.as_str() == "fin")
    );
    assert_eq!(
        close_code(&ws_next(&mut ws, Duration::from_secs(10)).await),
        Some(1000)
    );
    eventually(
        "reaped",
        async || matches!(state(&h, &a, &c).await, Err(e) if e.1.code == "conn_closed"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_first_frame_auth() {
    let (h, a, _) = auth_setup(|_| {}).await;
    lease(&h, &a, 1, None).await.unwrap();
    let (mut ws, conn) = svc_connect(&h, ConnectAuth::Frame(&a), &SVC).await.unwrap();
    ws_write(&mut ws, b"x").await;
    let c = claim(&h, &a, 1).await;
    assert_eq!(c.conn, conn);
    assert_eq!(read_n(&h, &a, &c, 0, 1).await, b"x");

    // A bad token in the frame: err, then a close.
    let bad = [7u8; 32];
    match svc_connect(&h, ConnectAuth::Frame(&bad), &SVC).await {
        Err(Refused::Frame(code)) => assert_eq!(code, "unauthorized"),
        r => panic!("{:?}", r.map(|_| ())),
    }
    // Something else first.
    let hex: String = SVC.iter().map(|b| format!("{b:02x}")).collect();
    let url = format!("ws://{}/v1/svc/connect?fs=1&svc={hex}", h.server.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws_write(&mut ws, b"data before auth").await;
    match recv(&mut ws).await {
        Frame::Err { code, .. } => assert_eq!(code, "bad_request"),
        f => panic!("{f:?}"),
    }
    assert_eq!(
        close_code(&ws_next(&mut ws, Duration::from_secs(5)).await),
        Some(4000)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_refusals() {
    let (h, a, b) = auth_setup(|s| s.max_conns = 1).await;
    let http = |r: Result<(Ws, Vec<u8>), Refused>| match r {
        Err(Refused::Http(status, code)) => (status, code),
        r => panic!("{:?}", r.map(|_| ())),
    };
    let bad = [7u8; 32];
    assert_eq!(
        http(svc_connect(&h, ConnectAuth::Header(&bad), &SVC).await),
        (401, "unauthorized".into())
    );
    // Bob may consume, not append.
    assert_eq!(
        http(svc_connect(&h, ConnectAuth::Header(&b), &SVC).await),
        (403, "forbidden".into())
    );
    match svc_connect(&h, ConnectAuth::Frame(&b), &SVC).await {
        Err(Refused::Frame(code)) => assert_eq!(code, "forbidden"),
        r => panic!("{:?}", r.map(|_| ())),
    }
    // A tcp service, and no service at all.
    for svc in [TCP_SVC, [0x11; 16]] {
        assert_eq!(
            http(svc_connect(&h, ConnectAuth::Header(&a), &svc).await),
            (404, "not_found".into())
        );
    }
    // max_conns = 1: the second is refused while the first is open.
    let (ws, _) = connector(&h, &a).await;
    assert_eq!(
        http(svc_connect(&h, ConnectAuth::Header(&a), &SVC).await),
        (429, "quota".into())
    );
    drop(ws);
    eventually("the slot is free", async || {
        svc_connect(&h, ConnectAuth::Header(&a), &SVC).await.is_ok()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_close_abort_and_client_close() {
    let (h, a, _) = auth_setup(|_| {}).await;
    let store = h.server.state.store.clone();
    let k = svc_keys();
    lease(&h, &a, 1, None).await.unwrap();

    // A close record: the bytes, then close 1000.
    let (mut ws, _) = connector(&h, &a).await;
    let c = claim(&h, &a, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(0, b"do"), (CLOSE, b"ne")], None);
    step(&h, &a, &mut held, s).await.unwrap();
    reveal(&h, &a, &held.conn, &keys).await.unwrap();
    assert_eq!(ws_reads(&mut ws, 4).await, b"done");
    assert_eq!(
        close_code(&ws_next(&mut ws, Duration::from_secs(10)).await),
        Some(1000)
    );
    eventually("reaped", async || gone(store.as_ref(), &held.conn).await).await;

    // An abort record: close 4000.
    let (mut ws, _) = connector(&h, &a).await;
    let c = claim(&h, &a, 1).await;
    let mut held = Held::of(&c);
    let (s, keys) = sealed_step(&k, &held, 0, &[(ABORT, b"")], None);
    step(&h, &a, &mut held, s).await.unwrap();
    reveal(&h, &a, &held.conn, &keys).await.unwrap();
    assert_eq!(
        close_code(&ws_next(&mut ws, Duration::from_secs(10)).await),
        Some(4000)
    );
    eventually("reaped", async || gone(store.as_ref(), &held.conn).await).await;

    // The client closes: a reset, and the handler's next step is refused.
    let (mut ws, _) = connector(&h, &a).await;
    let c = claim(&h, &a, 1).await;
    let held = Held::of(&c);
    ws.close(None).await.unwrap();
    eventually("reaped", async || gone(store.as_ref(), &held.conn).await).await;
    let e = commit(
        &h,
        &a,
        &commit_of(vec![sealed_step(&k, &held, 0, &[(0, b"late")], None).0]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), (412, "conn_closed"));

    // A frame over 64 KiB: a reset.
    let (mut ws, conn) = connector(&h, &a).await;
    let c = claim(&h, &a, 1).await;
    assert_eq!(c.conn, conn);
    let _ = ws.send(Message::Binary(vec![1u8; 70_000].into())).await;
    assert_eq!(
        close_code(&ws_next(&mut ws, Duration::from_secs(10)).await),
        Some(4000)
    );
    let conn: [u8; 16] = conn.try_into().unwrap();
    eventually("reaped", async || gone(store.as_ref(), &conn).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_window() {
    let (h, a, _) = auth_setup(|s| s.window = 65_536).await;
    let k = svc_keys();
    lease(&h, &a, 1, None).await.unwrap();
    let (mut ws, _) = connector(&h, &a).await;
    let data: Vec<u8> = (0..300 * 1024).map(|i| (i * 7 + i / 1000) as u8).collect();
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        for f in sent.chunks(65_536) {
            ws_write(&mut ws, f).await;
        }
        ws
    });
    let c = claim(&h, &a, 1).await;
    let mut held = Held::of(&c);
    let mut got = Vec::new();
    while got.len() < data.len() {
        let r = read(&h, &a, &c, held.in_acked, 5_000).await.unwrap();
        assert!(
            r.in_head - held.in_acked <= 65_536,
            "input stays within the window: {} past {}",
            r.in_head - held.in_acked,
            held.in_acked
        );
        let mut to = held.in_acked;
        for ch in r.chunks {
            let skip = (to - ch.off) as usize;
            got.extend_from_slice(&ch.data[skip..]);
            to = ch.off + ch.data.len() as u64;
        }
        // Ack it slowly, so the window fills.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (s, _) = sealed_step(&k, &held, to, &[], None);
        step(&h, &a, &mut held, s).await.unwrap();
    }
    assert!(got == data, "the input arrives whole and in order");
    let _ws = writer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_takeover() {
    let (h, a, _) = auth_setup(|_| {}).await;
    // Bob handles too (topic consume), from his own lease.
    let b = h.sign_in(&User::new(2)).await.unwrap();
    let k = svc_keys();
    lease(&h, &a, 1, None).await.unwrap();
    let (mut ws, _) = connector(&h, &a).await;
    ws_write(&mut ws, b"l0\nl1\nl2\n").await;
    let ca = claim(&h, &a, 1).await;
    let mut held = Held::of(&ca);
    read_n(&h, &a, &ca, 0, 9).await;
    let (mut s, keys) = sealed_step(&k, &held, 3, &[(0, b"l0\n")], Some(b"1"));
    s.resumable = Some(true);
    step(&h, &a, &mut held, s).await.unwrap();
    reveal(&h, &a, &held.conn, &keys).await.unwrap();
    // Committed, never revealed: Alice dies.
    let (s, _lost) = sealed_step(&k, &held, 6, &[(0, b"l1\n")], Some(b"2"));
    step(&h, &a, &mut held, s).await.unwrap();
    lease(&h, &a, 1, Some(300)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    lease(&h, &b, 2, None).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let cb = loop {
        let mut c = next(&h, &b, 2, 2_000).await.unwrap();
        if let Some(c) = c.conns.pop() {
            break c;
        }
        assert!(Instant::now() < deadline, "the connection is requeued");
    };
    let mut hb = Held::of(&cb);
    assert_eq!((cb.in_acked, cb.out_head), (6, 6));
    let keys: Vec<(u64, [u8; 32])> = cb
        .pending
        .iter()
        .map(|p| {
            (
                p.off,
                *zsvc::open_key_escrow(&k, &hb.conn, p.off, &p.escrow).unwrap(),
            )
        })
        .collect();
    reveal(&h, &b, &hb.conn, &keys).await.unwrap();
    assert_eq!(read_n(&h, &b, &cb, 6, 3).await, b"l2\n");
    let (s, keys) = sealed_step(&k, &hb, 9, &[(0, b"l2\n")], Some(b"3"));
    step(&h, &b, &mut hb, s).await.unwrap();
    reveal(&h, &b, &hb.conn, &keys).await.unwrap();
    assert_eq!(ws_reads(&mut ws, 9).await, b"l0\nl1\nl2\n");
    assert!(ws_next(&mut ws, Duration::from_millis(300)).await.is_none());
}

// ---------------------------------------------------------------- sealed inbox

/// The inbox's decapsulation seed in these tests.
const INBOX_SEED: [u8; 32] = [0x1B; 32];

/// A service with an inbox.
async fn inbox_setup() -> (Harness, Vec<u8>, SocketAddr) {
    setup(|s| s.inbox = Some(zsvc::inbox_public(&INBOX_SEED))).await
}

/// The claim's inbox key, from its ciphertext.
fn inbox_key(c: &SvcClaim) -> zsvc::InboxKey {
    let ct = c.inbox_ct.as_deref().expect("the claim carries inbox_ct");
    assert_eq!(ct.len(), zsvc::INBOX_CT_LEN);
    let conn: [u8; 16] = c.conn.as_slice().try_into().unwrap();
    zsvc::inbox_decaps(&INBOX_SEED, ct, 1, &SVC, &conn).unwrap()
}

/// A chunk's input bytes: opened at its offset when sealed.
fn open_chunk(key: &zsvc::InboxKey, off: u64, data: &[u8], sealed: bool) -> Vec<u8> {
    if sealed {
        key.open_chunk(off, data)
            .expect("a sealed chunk opens at its offset")
    } else {
        data.to_vec()
    }
}

/// Read exactly `n` input bytes from `from`, opening sealed chunks; every
/// chunk must start where the last one ended. Returns the bytes and each
/// chunk's `(off, sealed)`.
async fn read_open(
    h: &Harness,
    tok: &[u8],
    c: &SvcClaim,
    key: &zsvc::InboxKey,
    from: u64,
    n: usize,
) -> (Vec<u8>, Vec<(u64, bool)>) {
    let (mut out, mut chunks) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < n {
        assert!(Instant::now() < deadline, "input arrives");
        let r = read(h, tok, c, from + out.len() as u64, 5_000)
            .await
            .unwrap();
        for ch in r.chunks {
            let at = from + out.len() as u64;
            assert_eq!(ch.off, at, "chunks are contiguous");
            out.extend(open_chunk(key, ch.off, &ch.data, ch.sealed));
            chunks.push((ch.off, ch.sealed));
        }
    }
    (out, chunks)
}

/// Every relay value in the store.
async fn relay_values(store: &dyn Storage) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for tag in [
        "rc", "rh", "rw", "ra", "rm", "rz", "ri", "ro", "rv", "rs", "rq", "rl", "rx", "rli", "rnq",
        "rnc",
    ] {
        let p = Key::new().str(tag).finish();
        let mut t = store.begin(None).await.unwrap();
        for (_, v) in t
            .snapshot_get_range(&p, &keys::end_of(&p), 10_000, false)
            .await
            .unwrap()
        {
            out.push(v);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn inbox_seals_input() {
    let (h, tok, addr) = inbox_setup().await;
    let store = h.server.state.store.clone();
    let pk = zsvc::inbox_public(&INBOX_SEED);
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
    assert_eq!(info.inbox.as_deref(), Some(&pk[..]));
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    // Written apart, so the input is several chunks.
    let mut sent = Vec::new();
    for i in 0..8 {
        let line = format!("PLAINMARKER-{i:02}\n");
        peer.write_all(line.as_bytes()).await.unwrap();
        sent.extend_from_slice(line.as_bytes());
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let c = claim(&h, &tok, 1).await;
    let key = inbox_key(&c);
    let (got, chunks) = read_open(&h, &tok, &c, &key, 0, sent.len()).await;
    assert_eq!(got, sent);
    assert!(chunks.len() > 1, "several chunks: {chunks:?}");
    assert!(chunks.iter().all(|(_, sealed)| *sealed));
    let st = state(&h, &tok, &c).await.unwrap();
    assert_eq!(st.in_head, sent.len() as u64, "in_head counts plaintext");

    // Nothing of the input is stored in plaintext.
    for v in relay_values(store.as_ref()).await {
        assert!(
            !v.windows(11).any(|w| w == b"PLAINMARKER"),
            "plaintext at rest"
        );
    }

    // Reads start at the chunk holding `from`: a chunk ending at `from`
    // isn't returned, though its data (with the tag) is longer.
    for &(off, _) in &chunks[1..] {
        let r = read(&h, &tok, &c, off, 0).await.unwrap();
        assert_eq!(r.chunks[0].off, off);
    }
    // `max_bytes` counts plaintext: two chunks' worth returns two chunks.
    let two = chunks[2].0;
    let r = read_max(&h, &tok, &c, 0, Some(two as u32), 0)
        .await
        .unwrap();
    assert_eq!(r.chunks.len(), 2);
    assert_eq!(r.chunks[1].off, chunks[1].0);

    // Pushes are contiguous in plaintext offsets.
    let mut ws = svc_sub(&h, &tok, 1).await;
    let mut pushed = Vec::new();
    while pushed.len() < sent.len() {
        let Frame::SvcIn {
            off, data, sealed, ..
        } = next_push(&mut ws).await
        else {
            panic!("expected svc_in");
        };
        assert!(sealed);
        assert_eq!(off, pushed.len() as u64, "pushes are contiguous");
        pushed.extend(open_chunk(&key, off, &data, sealed));
    }
    assert_eq!(pushed, sent);

    // A chunk doesn't open at another offset.
    let r = read(&h, &tok, &c, 0, 0).await.unwrap();
    assert!(key.open_chunk(1, &r.chunks[0].data).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn unseal_stops_sealing() {
    let (h, tok, addr) = inbox_setup().await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"STARTTLS\n").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    let key = inbox_key(&c);
    let (got, chunks) = read_open(&h, &tok, &c, &key, 0, 9).await;
    assert_eq!(got, b"STARTTLS\n");
    assert!(chunks.iter().all(|(_, sealed)| *sealed));

    // A step that acks nothing and sets `unseal`.
    let mut held = Held::of(&c);
    let (mut s, _) = sealed_step(&svc_keys(), &held, 0, &[], None);
    s.unseal = Some(true);
    step(&h, &tok, &mut held, s).await.unwrap();

    peer.write_all(b"after\n").await.unwrap();
    let (got, chunks) = read_open(&h, &tok, &c, &key, 0, 15).await;
    // One stream across the switch: the sealed chunk still opens.
    assert_eq!(got, b"STARTTLS\nafter\n");
    assert!(chunks[0].1);
    assert!(!chunks.last().unwrap().1, "stored plain after unseal");
    let r = read(&h, &tok, &c, 9, 0).await.unwrap();
    assert_eq!(r.chunks[0].data, b"after\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn unseal_without_inbox_is_a_no_op() {
    let (h, tok, addr) = setup(|_| {}).await;
    lease(&h, &tok, 1, None).await.unwrap();
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"a\n").await.unwrap();
    let c = claim(&h, &tok, 1).await;
    assert_eq!(c.inbox_ct, None);
    let mut held = Held::of(&c);
    let (mut s, _) = sealed_step(&svc_keys(), &held, 0, &[], None);
    s.unseal = Some(true);
    step(&h, &tok, &mut held, s).await.unwrap();
    peer.write_all(b"b\n").await.unwrap();
    assert_eq!(read_n(&h, &tok, &c, 0, 4).await, b"a\nb\n");
}
