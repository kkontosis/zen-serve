//! Several zen-serve nodes on one FoundationDB cluster: shared sessions and
//! challenges, cross-node ephemeral pub/sub and subscriptions, fencing across
//! nodes. The multi-node cases run with `ZEN_TEST_BACKEND=fdb` only; logout
//! and challenge reuse run on every backend.

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
    assert_eq!(recv(&mut wb).await, Frame::Ok { id: Some(1) });
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
    assert_eq!(recv(&mut wb).await, Frame::Ok { id: Some(2) });

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
