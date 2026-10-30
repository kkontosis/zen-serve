//! The Rust client against an in-process server: sign-in, the ACL walk, the
//! header and keyslots, KV transactions, leases, TLS.

mod common;

use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use zen_client::acl::verify_next;
use zen_client::kv::send_commit;
use zen_client::{
    AclVersion, Client, ClientOptions, DeviceKey, Lease, Mode, RangeOptions, TlsOptions,
    Transaction, TxnOptions,
};
use zen_proto::{ApiToken, ApiTokenCreate, Empty, ReadVersion};

async fn grv(s: &zen_client::Session) -> zen_client::Result<ReadVersion> {
    s.call("/v1/grv", &Empty {}).await
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_with_a_device_key_file() {
    let srv = Server::start_with(|c| c.auth.api_tokens = true).await;
    let (admin, bob) = (key(1), key(2));
    srv.claim(&admin, &[&bob]).await;

    // The key file round trip, mode 0600.
    let path = srv.dir.path().join("bob.key");
    bob.save(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let loaded = DeviceKey::load(&path).unwrap();
    assert_eq!(loaded.fp(), bob.fp());
    assert_eq!(loaded.to_json(), bob.to_json());

    let s = srv.client().sign_in_device(&loaded).await.unwrap();
    assert_eq!(s.device_fp(), bob.fp().to_vec());
    assert!(s.info().features.iter().any(|f| f == "checkpoints"));
    grv(&s).await.unwrap();
    assert_eq!(s.acl().head().await.unwrap().version, 1);

    // A device whose cert isn't in the ACL.
    let stranger = second_key(2);
    let e = srv.client().sign_in_device(&stranger).await.unwrap_err();
    assert_eq!(e.status(), 401, "{e}");

    // Another origin than the server's.
    let other = Client::connect(
        &srv.url,
        ClientOptions {
            origin: Some("https://evil.example".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let e = other.sign_in_device(&bob).await.unwrap_err();
    assert!(matches!(e.status(), 400..=403), "{e}");

    // A revoked session signs in again once, on its own.
    let before = s.bearer();
    let _: Empty = s.call("/v1/auth/logout", &Empty {}).await.unwrap();
    grv(&s).await.unwrap();
    assert_ne!(s.bearer(), before);

    // An API token session (no device key: it can't sign in again).
    let a = srv.session(&admin).await;
    let t: ApiToken = a
        .call(
            "/v1/auth/tokens/create",
            &ApiTokenCreate {
                user: bob.user_fp().to_vec(),
                label: None,
                expires_unix: None,
            },
        )
        .await
        .unwrap();
    let ts = srv
        .client()
        .session_from_token(t.token.into_bytes())
        .await
        .unwrap();
    grv(&ts).await.unwrap();
    assert!(ts.bearer().starts_with("zen_at_"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_acl_is_walked_and_pinned() {
    let srv = Server::start().await;
    let (admin, bob, carol) = (key(1), key(2), key(3));
    srv.claim(&admin, &[&bob]).await;
    // A copy left at version 1.
    let old = srv.fork().await;

    let a = srv.session(&admin).await;
    a.acl()
        .update(&admin.identity, |d| d.members.push(carol.member()))
        .await
        .unwrap();
    let v3 = a
        .acl()
        .update(&admin.identity, |d| {
            d.members.retain(|m| m.identity != carol.member().identity)
        })
        .await
        .unwrap();
    assert_eq!(v3.version, 3);
    let b = srv.session(&bob).await;
    assert_eq!(b.acl().head().await.unwrap().version, 3);

    // verify_next refuses a skip, a wrong prev_hash, a non-admin signer, a bad signature.
    let v1 = AclVersion::parse(&b.acl().entries(Some(1)).await.unwrap().entries[0]).unwrap();
    let v2 = AclVersion::parse(&b.acl().entries(Some(2)).await.unwrap().entries[0]).unwrap();
    verify_next(Some(&v1), &v2).unwrap();
    assert!(verify_next(Some(&v1), &v3).unwrap_err().is("acl_chain"));
    let resign = |signer: &DeviceKey, edit: &dyn Fn(&mut zen_proto::acl::AclDoc)| {
        let mut doc = v2.doc.clone();
        edit(&mut doc);
        AclVersion::parse(&zen_client::acl::sign_doc(&doc, &signer.identity).unwrap()).unwrap()
    };
    let wrong_prev = resign(&admin, &|d| d.prev_hash = vec![7; 32]);
    assert!(
        verify_next(Some(&v1), &wrong_prev)
            .unwrap_err()
            .is("acl_chain")
    );
    let by_bob = resign(&bob, &|_| {});
    assert!(verify_next(Some(&v1), &by_bob).unwrap_err().is("acl_chain"));
    let mut bad_sig = v2.clone();
    bad_sig.signed.sig[10] ^= 1;
    assert!(
        verify_next(Some(&v1), &bad_sig)
            .unwrap_err()
            .is("acl_chain")
    );

    // A client pinned at 3, talking to the copy at 1.
    let c = old.session(&bob).await;
    c.acl().pin(&v3.bytes).unwrap();
    let e = c.acl().head().await.unwrap_err();
    assert!(e.is("acl_chain") && e.to_string().contains("below"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn headers_keyslots_and_epochs() {
    let srv = Server::start_with(|c| c.auth.api_tokens = true).await;
    let (admin, bob, carol) = (key(1), key(2), key(3));
    srv.claim(&admin, &[&bob, &carol]).await;
    let a = srv.session(&admin).await;
    let b = srv.session(&bob).await;
    let c = srv.session(&carol).await;

    // Unsigned: init with the admin's slot, then a slot for bob.
    let u = a.fs(1).init_for(&[admin.public()], false).await.unwrap();
    let k = u.key(&["greeting"]);
    u.set(&k, b"hello").await.unwrap();
    u.add_device_slot(&bob.public()).await.unwrap();
    assert_eq!(u.stored().header.slots.len(), 2);
    let ub = b.fs(1).unlock_device(&bob.device).await.unwrap();
    assert_eq!(ub.get(&k).await.unwrap().unwrap().value, b"hello");
    let e = c.fs(1).unlock_device(&carol.device).await.err().unwrap();
    assert!(e.is("no_slot"), "{e}");

    // Rotation: epoch-0 values still open, new ones seal in epoch 1.
    let u1 = u
        .rotate(|keys| {
            [admin.public(), bob.public()]
                .iter()
                .map(|d| {
                    Ok(zen_core::keyslot::create_device(
                        keys,
                        d,
                        &mut zen_core::rng::OsRng,
                    )?)
                })
                .collect()
        })
        .await
        .unwrap();
    assert_eq!(u1.keys().epoch, 1);
    let k2 = u1.key(&["after"]);
    u1.set(&k2, b"epoch one").await.unwrap();
    let ub = b.fs(1).unlock_device(&bob.device).await.unwrap();
    assert_eq!(ub.get(&k).await.unwrap().unwrap().value, b"hello");
    assert_eq!(ub.get(&k2).await.unwrap().unwrap().value, b"epoch one");
    let raw = ub.get(&k2).await.unwrap().unwrap();
    assert_eq!(raw.value, b"epoch one");
    assert!(u.seal_keys().err().unwrap().is("stale_slot"));

    // Signed from version 1; edits keep signing; another session verifies.
    let s = a.fs(2).init_for(&[admin.public()], true).await.unwrap();
    assert_eq!(s.stored().signed.unwrap().version, 1);
    s.add_device_slot(&bob.public()).await.unwrap();
    s.add_device_slot(&carol.public()).await.unwrap();
    assert_eq!(s.stored().signed.unwrap().version, 3);
    let sb = b.fs(2).unlock_device(&bob.device).await.unwrap();
    let st = sb.stored().signed.unwrap();
    assert_eq!((st.version, st.signer), (3, admin.user_fp()));

    // A session without an identity can't write a signed successor.
    let t: ApiToken = a
        .call(
            "/v1/auth/tokens/create",
            &zen_proto::ApiTokenCreate {
                user: admin.user_fp().to_vec(),
                label: None,
                expires_unix: None,
            },
        )
        .await
        .unwrap();
    let ts = srv
        .client()
        .session_from_token(t.token.into_bytes())
        .await
        .unwrap();
    let tu = ts.fs(2).unlock_device(&admin.device).await.unwrap();
    let e = tu.edit_header(|_| Ok(true)).await.unwrap_err();
    assert!(e.is("signer_needed"), "{e}");
    // A member that isn't an admin: refused before anything is sent.
    let e = sb.edit_header(|_| Ok(true)).await.unwrap_err();
    assert!(e.is("param"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_reads_and_transactions() {
    let srv = Server::start_with(|c| c.limits.max_range_items = 3).await;
    let admin = key(1);
    srv.claim(&admin, &[]).await;
    let a = srv.session(&admin).await;
    let u = a.fs(1).init_for(&[admin.public()], false).await.unwrap();

    // Ten items under a prefix, read in pages of 3.
    let dir = u.key(&["items"]);
    u.transaction(TxnOptions::default(), async |tx: &mut Transaction| {
        for i in 0..10u8 {
            tx.set(&u.key(&[b"items".as_slice(), &[i]]), &[i])?;
        }
        tx.set(&u.key(&["other"]), b"x")
    })
    .await
    .unwrap();
    let all = u.range_prefix(&dir, RangeOptions::default()).await.unwrap();
    assert_eq!(all.len(), 10);
    let mut sorted = all.clone();
    sorted.sort_by(|x, y| x.key.cmp(&y.key));
    assert_eq!(all, sorted);
    let rev = u
        .range_prefix(
            &dir,
            RangeOptions {
                limit: Some(5),
                reverse: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(rev.len(), 5);
    assert_eq!(rev[0].key, all[9].key);
    assert_eq!(rev[4].key, all[5].key);

    // Own writes and clears are seen inside the transaction (a clear spans
    // at most max_range_items keys: three here).
    let (b0, b3) = (all[0].key.clone(), all[3].key.clone());
    u.transaction(TxnOptions::default(), async |tx: &mut Transaction| {
        tx.clear_range(&b0, Some(&b3));
        tx.set(&all[1].key, b"kept")?;
        let seen = tx.range(&b0, Some(&b3), None).await?;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].value, b"kept");
        assert_eq!(tx.get(&all[2].key).await?, None);
        assert_eq!(tx.get(&all[4].key).await?, Some(vec![all[4].value[0]]));
        Ok(())
    })
    .await
    .unwrap();
    let left = u.range_prefix(&dir, RangeOptions::default()).await.unwrap();
    assert_eq!(left.len(), 8);
    u.delete(&all[1].key).await.unwrap();
    assert!(u.get(&all[1].key).await.unwrap().is_none());

    // Short mode: concurrent increments, none lost.
    let counter = u.key(&["counter"]);
    let runs = Arc::new(AtomicU32::new(0));
    let incr = |n: u32| {
        let (u, counter, runs) = (u.clone(), counter.clone(), runs.clone());
        async move {
            for _ in 0..n {
                u.transaction(
                    TxnOptions {
                        attempts: 50,
                        ..Default::default()
                    },
                    async |tx: &mut Transaction| {
                        runs.fetch_add(1, Ordering::Relaxed);
                        let v = tx
                            .get(&counter)
                            .await?
                            .map_or(0, |v| u32::from_be_bytes(v.try_into().unwrap()));
                        tx.set(&counter, &(v + 1).to_be_bytes())
                    },
                )
                .await
                .unwrap();
            }
        }
    };
    tokio::join!(incr(20), incr(20));
    let v = u.get(&counter).await.unwrap().unwrap().value;
    assert_eq!(u32::from_be_bytes(v.try_into().unwrap()), 40);
    assert!(runs.load(Ordering::Relaxed) > 40, "no conflict happened");

    // Long mode: a read becomes an expect, which a concurrent write breaks.
    let mut tx = Transaction::new(u.clone(), Mode::Long);
    tx.get(&counter).await.unwrap();
    tx.set(&u.key(&["derived"]), b"1").unwrap();
    u.set(&counter, b"changed").await.unwrap();
    let e = tx.commit().await.unwrap_err();
    assert!(e.retryable(), "{e}");
    // …and a range hash, which a concurrent insert breaks.
    let mut tx = Transaction::new(u.clone(), Mode::Long);
    let fresh = u.key(&["fresh"]);
    assert_eq!(tx.range_prefix(&fresh).await.unwrap().len(), 0);
    tx.set(&u.key(&["derived"]), b"2").unwrap();
    u.set(&u.key(&["fresh", "one"]), b"new").await.unwrap();
    assert!(tx.commit().await.unwrap_err().retryable());

    // A commit resent with its commit_id applies once.
    let mut tx = Transaction::new(u.clone(), Mode::Short);
    tx.set(&u.key(&["once"]), b"1").unwrap();
    let first = tx.commit().await.unwrap().unwrap();
    let again = send_commit(&u, &tx.build(), 1).await.unwrap();
    assert_eq!(again.commit_version, first.commit_version);
    assert_eq!(again.versionstamp, first.versionstamp);
}

#[tokio::test(flavor = "multi_thread")]
async fn leases_and_fences() {
    let srv = Server::start().await;
    let (admin, bob) = (key(1), key(2));
    srv.claim(&admin, &[&bob]).await;
    let a = srv.session(&admin).await;
    let b = srv.session(&bob).await;
    let ua = a
        .fs(1)
        .init_for(&[admin.public(), bob.public()], false)
        .await
        .unwrap();
    let ub = b.fs(1).unlock_device(&bob.device).await.unwrap();

    let mut la = Lease::acquire(ua.fs(), b"vol", 10_000).await.unwrap();
    assert_eq!(la.token(), 1);
    let e = Lease::acquire(ub.fs(), b"vol", 10_000).await.err().unwrap();
    assert!(e.is("not_leader"), "{e}");
    la.renew().await.unwrap();
    assert_eq!(la.token(), 1);

    // A fenced commit applies while the lease holds.
    let k = ua.key(&["block"]);
    ua.transaction(TxnOptions::default(), async |tx: &mut Transaction| {
        tx.fence_lease(&la);
        tx.set(&k, b"a")
    })
    .await
    .unwrap();

    // Released, the other device takes it with the next token…
    let stale = la.clone();
    la.release().await.unwrap();
    let lb = Lease::acquire(ub.fs(), b"vol", 10_000).await.unwrap();
    assert_eq!(lb.token(), 2);
    // …and the old holder's fenced commit is refused, writing nothing.
    let e = ua
        .transaction(TxnOptions::default(), async |tx: &mut Transaction| {
            tx.fence_lease(&stale);
            tx.set(&k, b"stale")
        })
        .await
        .unwrap_err();
    assert!(e.is("claim_lost"), "{e}");
    assert_eq!(ua.get(&k).await.unwrap().unwrap().value, b"a");
    lb.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn https_with_a_ca() {
    use zen_tls_rustcrypto::testing::{KeyKind, pem, server_cert};
    let cert = server_cert(KeyKind::P256);
    let dir = tempfile::tempdir().unwrap();
    let chain: String = cert.chain.iter().map(|c| pem("CERTIFICATE", c)).collect();
    let key_pem = match &cert.key {
        rustls::pki_types::PrivateKeyDer::Pkcs8(k) => pem("PRIVATE KEY", k.secret_pkcs8_der()),
        rustls::pki_types::PrivateKeyDer::Sec1(k) => pem("EC PRIVATE KEY", k.secret_sec1_der()),
        _ => unreachable!(),
    };
    std::fs::write(dir.path().join("cert.pem"), chain).unwrap();
    std::fs::write(dir.path().join("key.pem"), key_pem).unwrap();
    let tls = zen_server::config::TlsConfig {
        cert: dir.path().join("cert.pem"),
        key: dir.path().join("key.pem"),
        client_ca: None,
    };
    let srv = Server::start_with(|c| c.tls = Some(tls)).await;
    assert!(srv.url.starts_with("https://"));
    let admin = key(1);
    // Claim over HTTPS, trusting the test CA.
    let ca = pem("CERTIFICATE", &cert.ca).into_bytes();
    let opts = || ClientOptions {
        origin: None,
        tls: TlsOptions {
            ca: Some(ca.clone()),
        },
    };
    claim_over(&srv, &admin, opts()).await;
    let s = Client::connect(&srv.url, opts())
        .unwrap()
        .sign_in_device(&admin)
        .await
        .unwrap();
    grv(&s).await.unwrap();
    // Without the CA, the platform roots don't know the test issuer.
    let e = Client::connect(&srv.url, ClientOptions::default())
        .unwrap()
        .info()
        .await
        .unwrap_err();
    assert!(e.is("network"), "{e}");
}

async fn claim_over(srv: &Server, admin: &DeviceKey, opts: ClientOptions) {
    use zen_proto::acl::AclDoc;
    let doc = AclDoc {
        admins: vec![zen_proto::ByteBuf::from(admin.user_fp().to_vec())],
        grants: vec![],
        limits: vec![],
        members: vec![admin.member()],
        origins: vec![],
        version: 1,
        prev_hash: vec![0; 32],
    };
    let t = zen_client::transport::Transport::new(&srv.url, &opts.tls).unwrap();
    let _: zen_proto::AclVersion = t
        .call(
            "/v1/acl/put",
            None,
            &zen_proto::AclPut {
                acl: zen_client::acl::sign_doc(&doc, &admin.identity).unwrap(),
                claim: srv.server.claim_token.clone(),
                origin: None,
            },
        )
        .await
        .unwrap();
}
