//! Several zen-serve nodes on one FoundationDB cluster: shared sessions and
//! challenges, cross-node ephemeral pub/sub and subscriptions, fencing across
//! nodes, a relayed connection served from another node, its input pushed
//! over a stream on that node. The multi-node cases run with
//! `ZEN_TEST_BACKEND=fdb` only; logout and challenge reuse run on every
//! backend.

mod common;

use common::*;
use std::time::{Duration, Instant};
use zen_proto::*;

fn group_def(name: &[u8]) -> GroupDef {
    GroupDef {
        fs: 1,
        group: name.to_vec(),
        topic: topic(1),
        mode: Mode::Sequential,
        partitions: None,
        key_token: None,
        max_inflight: None,
        max_attempts: None,
        on_poison: None,
        start: None,
    }
}

async fn fs_list(h: &Harness, tok: &[u8]) -> Result<FsList, ApiErr> {
    h.call("/v1/fs/list", Some(tok), &Empty {}).await
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_ends_the_session() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    fs_list(&h, &tok).await.unwrap();
    let _: Empty = h
        .call("/v1/auth/logout", Some(&tok), &Empty {})
        .await
        .unwrap();
    let e = fs_list(&h, &tok).await.unwrap_err();
    assert_eq!((e.0, e.1.code.as_str()), (401, "unauthorized"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_challenge_is_single_use() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let c: Challenge = h.call("/v1/auth/challenge", None, &Empty {}).await.unwrap();
    let origin = h.origin();
    let req = SessionRequest {
        challenge: c.challenge.clone(),
        origin: origin.clone(),
        user: admin.id.public().encode(),
        cert: admin.cert.clone(),
        sig: admin
            .device
            .signing()
            .sign(
                zen_core::labels::SIG_SESSION,
                &session_message(&c.challenge, &origin),
            )
            .unwrap(),
    };
    let _: Session = h.call("/v1/auth/session", None, &req).await.unwrap();
    let e = h
        .call::<_, Session>("/v1/auth/session", None, &req)
        .await
        .unwrap_err();
    assert_eq!(e.1.code, "unauthorized");
    // A forged challenge is refused.
    let mut forged = req.clone();
    forged.challenge[20] ^= 1;
    let e = h
        .call::<_, Session>("/v1/auth/session", None, &forged)
        .await
        .unwrap_err();
    assert_eq!(e.1.code, "unauthorized");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_and_acl_are_shared() {
    if !on_fdb() {
        return;
    }
    let a = Harness::start().await;
    let b = a.peer().await;
    let admin = User::new(1);
    a.claim(&admin, &[]).await;
    // B learns the ACL by watching it; sign-in on A works on B.
    let deadline = Instant::now() + Duration::from_secs(10);
    let tok = loop {
        match a.sign_in(&admin).await {
            Ok(t) if fs_list(&b, &t).await.is_ok() => break t,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            other => panic!("{other:?}"),
        }
    };
    // A challenge from B, used on A.
    let c: Challenge = b.call("/v1/auth/challenge", None, &Empty {}).await.unwrap();
    let origin = a.origin();
    let s: Session = a
        .call(
            "/v1/auth/session",
            None,
            &SessionRequest {
                challenge: c.challenge.clone(),
                origin: origin.clone(),
                user: admin.id.public().encode(),
                cert: admin.cert.clone(),
                sig: admin
                    .device
                    .signing()
                    .sign(
                        zen_core::labels::SIG_SESSION,
                        &session_message(&c.challenge, &origin),
                    )
                    .unwrap(),
            },
        )
        .await
        .unwrap();
    fs_list(&b, &s.token).await.unwrap();
    // Logout on A reaches B within the session cache time.
    let _: Empty = a
        .call("/v1/auth/logout", Some(&tok), &Empty {})
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while fs_list(&b, &tok).await.is_ok() {
        assert!(Instant::now() < deadline, "logout reached node B");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn two_nodes() -> Option<(Harness, Harness, Vec<u8>, Vec<u8>, User, User)> {
    if !on_fdb() {
        return None;
    }
    let a = Harness::start().await;
    let b = a.peer().await;
    let (alice, bob) = (User::new(1), User::new(2));
    a.claim(&alice, &[&bob]).await;
    let ta = a.sign_in(&alice).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let tb = loop {
        match b.sign_in(&bob).await {
            Ok(t) => break t,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => panic!("{e:?}"),
        }
    };
    Some((a, b, ta, tb, alice, bob))
}

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_and_subscriptions_cross_nodes() {
    let Some((a, b, ta, tb, alice, _)) = two_nodes().await else {
        return;
    };
    let mut wb = connect(&b, &tb).await;
    send(
        &mut wb,
        &Frame::Esub {
            id: 1,
            fs: 1,
            topic: Some(topic(3)),
            prefix: None,
        },
    )
    .await;
    assert_eq!(
        recv(&mut wb).await,
        Frame::Ok {
            id: Some(1),
            conn: None
        }
    );
    send(
        &mut wb,
        &Frame::Sub {
            id: 2,
            fs: 1,
            topic: Some(topic(1)),
            prefix: None,
            after: None,
        },
    )
    .await;
    assert_eq!(
        recv(&mut wb).await,
        Frame::Ok {
            id: Some(2),
            conn: None
        }
    );

    let mut wa = connect(&a, &ta).await;
    send(
        &mut wa,
        &Frame::Epub {
            fs: 1,
            topic: topic(3),
            data: b"typing".to_vec(),
        },
    )
    .await;
    match recv(&mut wb).await {
        Frame::Eph {
            id, data, sender, ..
        } => {
            assert_eq!((id, data), (1, b"typing".to_vec()));
            assert_eq!(sender, alice.device.public().fingerprint().to_vec());
        }
        other => panic!("{other:?}"),
    }
    let ap = LogAppend {
        commit_id: cid(1),
        append: vec![append(&topic(1), None, b"e")],
    };
    let r: CommitResult = a.call("/v1/log/append", Some(&ta), &ap).await.unwrap();
    match recv(&mut wb).await {
        Frame::Ev { id, offset, .. } => {
            assert_eq!(id, 2);
            assert_eq!(offset, r.appended[0].to_vec());
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn leases_are_fenced_across_nodes() {
    let Some((a, b, ta, tb, ..)) = two_nodes().await else {
        return;
    };
    let _: GroupCreated = a
        .call("/v1/consume/groups", Some(&ta), &group_def(b"g"))
        .await
        .unwrap();
    let req = LeaseRequest {
        fs: 1,
        group: b"g".to_vec(),
        partition: None,
        token: None,
        ttl_ms: Some(60_000),
    };
    let lease: Lease = a.call("/v1/consume/lease", Some(&ta), &req).await.unwrap();
    let e = b
        .call::<_, Lease>("/v1/consume/lease", Some(&tb), &req)
        .await
        .unwrap_err();
    assert_eq!(e.1.code, "not_leader");
    // The holder renews through the other node: the lease is in storage.
    let renewed: Lease = b
        .call(
            "/v1/consume/lease",
            Some(&ta),
            &LeaseRequest {
                token: Some(lease.token),
                ..req.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(renewed.token, lease.token);
}

/// Claiming one node spends every node's claim token: a node that is up
/// deletes its `claim-token` file when it sees the new ACL, and a node that
/// starts later deletes a stale one.
#[tokio::test(flavor = "multi_thread")]
async fn claiming_one_node_removes_every_claim_token_file() {
    if !on_fdb() {
        return;
    }
    let a = Harness::start().await;
    let b = a.peer().await;
    let file = |h: &Harness| h.cfg.data_dir.join("claim-token");
    assert!(file(&a).exists() && file(&b).exists());
    assert!(b.server.claim_token.is_some());
    a.claim(&User::new(1), &[]).await;
    assert!(!file(&a).exists(), "the claimed node deletes its file");
    let deadline = Instant::now() + Duration::from_secs(10);
    while file(&b).exists() || b.server.state.claim.lock().unwrap().is_some() {
        assert!(Instant::now() < deadline, "the other node spends its token");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A node that was down during the claim drops its stale file at start.
    let c = a
        .peer_with(|d| std::fs::write(d.join("claim-token"), "stale").unwrap())
        .await;
    assert!(c.server.claim_token.is_none());
    assert!(
        !file(&c).exists(),
        "stale claim-token file deleted at start"
    );
}

/// The credential store, the login-name index, the fake parameters and the
/// origin pin are shared by every node.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_and_pins_cross_nodes() {
    use zen_core::pwkey::PasswordKey;
    use zen_core::vectors::FIXTURE_ARGON2;
    let Some((a, b, ta, _, _, _)) = two_nodes().await else {
        return;
    };
    async fn params(h: &Harness, name: &str) -> PasswordParams {
        let req = PasswordParamsRequest { name: name.into() };
        h.call("/v1/auth/password/params", None, &req)
            .await
            .unwrap()
    }
    // Fakes agree across nodes.
    assert_eq!(params(&a, "ghost").await, params(&b, "ghost").await);

    // Register on A, sign in on B.
    let (key, salt) =
        PasswordKey::create(b"pw", FIXTURE_ARGON2, &mut zen_core::rng::OsRng).unwrap();
    let set = PasswordSet {
        name: "alice".into(),
        salt: salt.to_vec(),
        m_cost_kib: FIXTURE_ARGON2.m_cost_kib,
        t_cost: FIXTURE_ARGON2.t_cost,
        p_cost: FIXTURE_ARGON2.p_cost,
        identity: key.public().encode(),
    };
    let id: CredentialId = a
        .call("/v1/auth/password/set", Some(&ta), &set)
        .await
        .unwrap();
    assert_eq!(params(&b, "alice").await.salt, salt.to_vec());
    let c: Challenge = b.call("/v1/auth/challenge", None, &Empty {}).await.unwrap();
    let origin = b.origin();
    let req = PasswordSessionRequest {
        name: "alice".into(),
        sig: key.sign_session(&c.challenge, &origin).unwrap(),
        challenge: c.challenge,
        origin,
    };
    let s: Session = b
        .call("/v1/auth/password/session", None, &req)
        .await
        .unwrap();
    fs_list(&b, &s.token).await.unwrap();

    // Removed on A: B stops accepting the session within its cache time.
    let _: Empty = a
        .call("/v1/auth/credentials/remove", Some(&ta), &id)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while fs_list(&b, &s.token).await.is_ok() {
        assert!(Instant::now() < deadline, "B kept the session");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Both nodes see the pin set through A's first sign-in.
    let st: OriginState = b
        .call("/v1/admin/origins/get", Some(&ta), &Empty {})
        .await
        .unwrap();
    assert_eq!(st.pinned, vec![a.origin()]);
}

/// A passkey registered on one node signs in on another; its counter is
/// shared, so a replayed count is refused there too.
#[tokio::test(flavor = "multi_thread")]
async fn passkeys_cross_nodes() {
    use zen_server::webauthn::soft::Authenticator;
    let Some((a, b, ta, _, _, _)) = two_nodes().await else {
        return;
    };
    let mut key = Authenticator::p256(80);
    let opts: PasskeyCreation = a
        .call("/v1/auth/passkey/register/begin", Some(&ta), &Empty {})
        .await
        .unwrap();
    let (attestation_object, client_data_json) =
        key.create(&opts.rp_id, &opts.challenge, &a.origin());
    let reg = PasskeyRegister {
        attestation_object,
        client_data_json,
        label: None,
    };
    let id: CredentialId = a
        .call("/v1/auth/passkey/register/finish", Some(&ta), &reg)
        .await
        .unwrap();
    async fn sign_in(h: &Harness, key: &mut Authenticator) -> Result<Session, ApiErr> {
        let opts: PasskeyRequest = h
            .call(
                "/v1/auth/passkey/session/begin",
                None,
                &PasskeyBegin::default(),
            )
            .await?;
        let (authenticator_data, client_data_json, signature) =
            key.get(&opts.rp_id, &opts.challenge, &h.origin());
        let req = PasskeySession {
            credential_id: key.credential_id.clone(),
            authenticator_data,
            client_data_json,
            signature,
            user_handle: None,
        };
        h.call("/v1/auth/passkey/session", None, &req).await
    }
    let s = sign_in(&b, &mut key).await.unwrap();
    assert_eq!(s.device_fp, id.id);
    fs_list(&b, &s.token).await.unwrap();
    sign_in(&a, &mut key).await.unwrap();
    key.counter = Some(1);
    assert_eq!(sign_in(&b, &mut key).await.unwrap_err().0, 401);
}

/// OPAQUE (auth.md §8): the server setup and the sealed login state are
/// shared, so a sign-in may start on one node and finish on another, and a
/// success on one node clears the attempts another counted.
#[tokio::test(flavor = "multi_thread")]
async fn opaque_sign_in_crosses_nodes() {
    if !on_fdb() {
        return;
    }
    let a = Harness::start_with(|c| {
        opaque_cfg(c);
        c.auth.password_max_failures = 2;
    })
    .await;
    let b = a.peer().await;
    let alice = User::new(1);
    a.claim(&alice, &[]).await;
    let ta = a.sign_in(&alice).await.unwrap();
    // B has seen the ACL once alice can sign in there.
    let deadline = Instant::now() + Duration::from_secs(10);
    while b.sign_in(&alice).await.is_err() {
        assert!(Instant::now() < deadline, "B never saw the ACL");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (id, reg) = opaque_set(&a, &ta, "alice", b"pw").await.unwrap();
    let origin = a.origin();
    assert_eq!(b.origin(), origin);
    // Start on A, finish on B. Each start counts on A; without the
    // success B records, the third start would be locked.
    for _ in 0..3 {
        let (login, r) = opaque_start(&a, "alice", b"pw", &origin).await.unwrap();
        let (fin, key) = opaque_client_finish(login, b"pw", &r, &origin);
        assert_eq!(*key.unwrap(), *reg.export_key, "one server setup");
        let s = opaque_finish(&b, r.state, fin).await.unwrap();
        assert_eq!(s.device_fp, id.id);
        fs_list(&b, &s.token).await.unwrap();
        fs_list(&a, &s.token).await.unwrap();
    }
    let (s, _) = opaque_sign_in(&b, "alice", b"pw").await.unwrap();
    // A state finishes once, on any node.
    let (login, r) = opaque_start(&b, "alice", b"pw", &origin).await.unwrap();
    let (fin, _) = opaque_client_finish(login, b"pw", &r, &origin);
    opaque_finish(&a, r.state.clone(), fin.clone())
        .await
        .unwrap();
    assert_eq!(opaque_finish(&b, r.state, fin).await.unwrap_err().0, 401);

    // A password change on A ends the sessions on B within its cache time.
    opaque_set(&a, &ta, "alice", b"pw two").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while fs_list(&b, &s.token).await.is_ok() {
        assert!(Instant::now() < deadline, "B kept the session");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    opaque_sign_in(&b, "alice", b"pw two").await.unwrap();
}

const SVC: [u8; 16] = [0x5E; 16];

/// Two nodes on FoundationDB with a relayed service, Alice signed in on B.
/// Returns A, B, Alice's token on B and the service's address on A.
async fn relay_nodes() -> (Harness, Harness, Vec<u8>, std::net::SocketAddr) {
    let (a, b, tb) = relay_nodes_in(zen_server::config::SvcMode::Tcp).await;
    let addr = a.server.state.svc.local_addr("echo").unwrap();
    (a, b, tb, addr)
}

/// Two nodes with a relayed service in `mode`; Alice's token on B.
async fn relay_nodes_in(mode: zen_server::config::SvcMode) -> (Harness, Harness, Vec<u8>) {
    use zen_server::config::{ServiceConfig, SvcMode};
    let a = Harness::start_with(|c| {
        c.service.push(ServiceConfig {
            name: "echo".into(),
            fs: 1,
            id: SVC.to_vec(),
            mode,
            listen: (mode == SvcMode::Tcp).then(|| "127.0.0.1:0".parse().unwrap()),
            max_conns: 16,
            per_ip_rate: 0,
            per_ip_burst: 1,
            window: 65_536,
            out_window: 65_536,
            accept_wait_ms: 60_000,
            handshake_secs: 30,
            idle_secs: 300,
            resume: true,
        })
    })
    .await;
    let b = a.peer().await;
    let alice = User::new(1);
    a.claim(&alice, &[]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let tb = loop {
        match b.sign_in(&alice).await {
            Ok(t) => break t,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => panic!("{e:?}"),
        }
    };
    (a, b, tb)
}

/// Take the handler lease on `h`.
async fn svc_lease(h: &Harness, tok: &[u8]) {
    let _: SvcLeaseResult = h
        .call(
            "/v1/svc/lease",
            Some(tok),
            &SvcLeaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![1; 16],
                ttl_ms: None,
                release: None,
            },
        )
        .await
        .unwrap();
}

/// On `h`: claim the next connection, read `want`, answer `reply` and
/// reveal it.
async fn svc_answer(h: &Harness, tok: &[u8], want: &[u8], reply: &[u8]) {
    use zen_core::rng::OsRng;
    use zen_core::svc::{SvcKeys, seal_record};
    let mut c: SvcClaims = h
        .call(
            "/v1/svc/next",
            Some(tok),
            &SvcNextRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![1; 16],
                limit: None,
                wait_ms: Some(10_000),
            },
        )
        .await
        .unwrap();
    let c = c.conns.pop().expect("claimed");
    let conn: [u8; 16] = c.conn.as_slice().try_into().unwrap();
    let r: SvcReadResult = h
        .call(
            "/v1/svc/read",
            Some(tok),
            &SvcReadRequest {
                fs: 1,
                svc: SVC.to_vec(),
                conn: c.conn.clone(),
                token: c.token,
                from: 0,
                max_bytes: None,
                wait_ms: Some(10_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.chunks[0].data, want);
    let k = SvcKeys::from_parts(1, 1, &SVC, [0x4B; 32]);
    let (rec, k_r) = seal_record(&k, &conn, 0, 0, reply, &mut OsRng).unwrap();
    let mut commit_id = vec![0u8; 16];
    getrandom::fill(&mut commit_id).unwrap();
    let _: CommitResult = h
        .call(
            "/v1/commit",
            Some(tok),
            &Commit {
                commit_id,
                svc: vec![SvcStep {
                    fs: 1,
                    svc: SVC.to_vec(),
                    conn: c.conn.clone(),
                    token: c.token,
                    in_from: 0,
                    in_to: want.len() as u64,
                    out_from: 0,
                    out: vec![ByteBuf::from(rec)],
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r: SvcRevealResult = h
        .call(
            "/v1/svc/reveal",
            Some(tok),
            &SvcRevealRequest {
                fs: 1,
                svc: SVC.to_vec(),
                reveals: vec![SvcReveal {
                    conn: c.conn.clone(),
                    off: 0,
                    key: k_r.to_vec(),
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(r.results, [0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn relayed_connection_across_nodes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !on_fdb() {
        return;
    }
    // The socket on node A, the handler on node B.
    let (_a, b, tb, addr) = relay_nodes().await;
    svc_lease(&b, &tb).await;
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"ping").await.unwrap();
    svc_answer(&b, &tb, b"ping", b"pong").await;
    let mut got = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), peer.read_exact(&mut got))
        .await
        .expect("the echo arrives through A")
        .unwrap();
    assert_eq!(&got, b"pong");
}

/// A `tcp-auth` connector's WebSocket on node A, the handler on node B.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_auth_across_nodes() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    if !on_fdb() {
        return;
    }
    let (a, b, tb) = relay_nodes_in(zen_server::config::SvcMode::TcpAuth).await;
    let ta = a.sign_in(&User::new(1)).await.unwrap();
    svc_lease(&b, &tb).await;
    let (mut ws, _) = svc_connect(&a, ConnectAuth::Header(&ta), &SVC)
        .await
        .unwrap();
    ws.send(Message::Binary(b"ping".to_vec().into()))
        .await
        .unwrap();
    svc_answer(&b, &tb, b"ping", b"pong").await;
    let m = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("the echo arrives through A")
        .unwrap()
        .unwrap();
    assert_eq!(m, Message::Binary(b"pong".to_vec().into()));
}

/// The socket on node A; the lease, the stream subscription, the step and
/// `svc_reveal` on node B.
#[tokio::test(flavor = "multi_thread")]
async fn pushed_input_across_nodes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zen_core::rng::OsRng;
    use zen_core::svc::{SvcKeys, seal_record};
    if !on_fdb() {
        return;
    }
    let (_a, b, tb, addr) = relay_nodes().await;
    let _: SvcLeaseResult = b
        .call(
            "/v1/svc/lease",
            Some(&tb),
            &SvcLeaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![1; 16],
                ttl_ms: None,
                release: None,
            },
        )
        .await
        .unwrap();
    let mut ws = connect(&b, &tb).await;
    send(
        &mut ws,
        &Frame::SvcSub {
            id: 1,
            fs: 1,
            svc: SVC.to_vec(),
            lease: vec![1; 16],
        },
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        Frame::Ok {
            id: Some(1),
            conn: None
        }
    );
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"ping").await.unwrap();
    // Skip svc_ready frames.
    let next = async |ws: &mut Ws| loop {
        match recv(ws).await {
            Frame::SvcReady { .. } => continue,
            f => break f,
        }
    };
    let mut c: SvcClaims = b
        .call(
            "/v1/svc/next",
            Some(&tb),
            &SvcNextRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: vec![1; 16],
                limit: None,
                wait_ms: Some(10_000),
            },
        )
        .await
        .unwrap();
    let c = c.conns.pop().expect("claimed on B");
    let conn: [u8; 16] = c.conn.as_slice().try_into().unwrap();
    let mut got = Vec::new();
    while got.len() < 4 {
        match next(&mut ws).await {
            Frame::SvcIn { conn: k, data, .. } if k == c.conn => got.extend_from_slice(&data),
            f => panic!("{f:?}"),
        }
    }
    assert_eq!(got, b"ping", "pushed through B");
    let k = SvcKeys::from_parts(1, 1, &SVC, [0x4B; 32]);
    let (rec, k_r) = seal_record(&k, &conn, 0, 0, b"pong", &mut OsRng).unwrap();
    let mut commit_id = vec![0u8; 16];
    getrandom::fill(&mut commit_id).unwrap();
    let _: CommitResult = b
        .call(
            "/v1/commit",
            Some(&tb),
            &Commit {
                commit_id,
                svc: vec![SvcStep {
                    fs: 1,
                    svc: SVC.to_vec(),
                    conn: c.conn.clone(),
                    token: c.token,
                    in_from: 0,
                    in_to: 4,
                    out_from: 0,
                    out: vec![ByteBuf::from(rec)],
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    send(
        &mut ws,
        &Frame::SvcReveal {
            id: 2,
            fs: 1,
            svc: SVC.to_vec(),
            reveals: vec![SvcReveal {
                conn: c.conn.clone(),
                off: 0,
                key: k_r.to_vec(),
            }],
        },
    )
    .await;
    assert_eq!(
        next(&mut ws).await,
        Frame::SvcRevealed {
            id: 2,
            results: vec![0]
        }
    );
    let mut out = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), peer.read_exact(&mut out))
        .await
        .expect("the echo arrives through A")
        .unwrap();
    assert_eq!(&out, b"pong");
}
