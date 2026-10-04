//! Sign-in method 3, a password via OPAQUE (auth.md §8): registration,
//! sign-in with an OPAQUE client, fake records for unknown names, the
//! limiter, the origin binding, the export-key keyslot, and the method
//! being off by default.

mod common;

use common::*;
use zen_core::keyslot::{self, Unlock};
use zen_core::pwkey::PasswordKey;
use zen_core::rng::OsRng;
use zen_core::vectors::{FIXTURE_ARGON2, fixture_fs};
use zen_proto::*;

type R<T> = Result<T, ApiErr>;

fn code<T>(r: R<T>) -> (u16, String) {
    match r {
        Ok(_) => panic!("expected an error"),
        Err((s, e)) => (s, e.code),
    }
}

async fn grv(h: &Harness, tok: &[u8]) -> R<ReadVersion> {
    h.call("/v1/grv", Some(tok), &Empty {}).await
}

async fn creds(h: &Harness, tok: &[u8]) -> R<Credentials> {
    h.call(
        "/v1/auth/credentials/list",
        Some(tok),
        &CredentialsList { user: None },
    )
    .await
}

/// A sign-in that must fail: its status and message.
async fn failed_sign_in(h: &Harness, name: &str, pw: &[u8]) -> (u16, String, String) {
    let origin = h.origin();
    let (login, r) = opaque_start(h, name, pw, &origin).await.unwrap();
    let (fin, key) = opaque_client_finish(login, pw, &r, &origin);
    assert!(key.is_none(), "the client could not finish");
    let (s, e) = opaque_finish(h, r.state, fin).await.err().unwrap();
    (s, e.code, e.message)
}

/// Method 6's registration, for the shared login names.
async fn set_password_key(h: &Harness, tok: &[u8], name: &str, pw: &[u8]) -> R<CredentialId> {
    let (key, salt) = PasswordKey::create(pw, FIXTURE_ARGON2, &mut OsRng).unwrap();
    let req = PasswordSet {
        name: name.into(),
        salt: salt.to_vec(),
        m_cost_kib: FIXTURE_ARGON2.m_cost_kib,
        t_cost: FIXTURE_ARGON2.t_cost,
        p_cost: FIXTURE_ARGON2.p_cost,
        identity: key.public().encode(),
    };
    h.call("/v1/auth/password/set", Some(tok), &req).await
}

#[tokio::test(flavor = "multi_thread")]
async fn opaque_register_sign_in_and_credentials() {
    let h = Harness::start_with(opaque_cfg).await;
    let (admin, bob) = (User::new(1), User::new(2));
    let doc1 = h.claim(&admin, &[&bob]).await;
    let auth = h.get::<Info>("/v1/info").await.auth.unwrap();
    assert!(auth.methods.contains(&"opaque".to_string()));
    assert_eq!(auth.default.as_deref(), Some("password_key"));

    let bob_dev = h.sign_in(&bob).await.unwrap();
    let before = failed_sign_in(&h, "bob@example.org", b"pw one").await;
    assert_eq!((before.0, before.1.as_str()), (401, "unauthorized"));

    // Register, then sign in end to end; the name is normalized.
    let (id1, reg) = opaque_set(&h, &bob_dev, "Bob@Example.org", b"pw one")
        .await
        .unwrap();
    let (s, export_key) = opaque_sign_in(&h, " bob@EXAMPLE.org", b"pw one")
        .await
        .unwrap();
    assert_eq!(s.method.as_deref(), Some("opaque"));
    assert_eq!(s.device_fp, id1.id, "the credential id is the device");
    assert_eq!(s.user_fp, bob.fp());
    assert_eq!(*export_key, *reg.export_key, "the same export key");
    let tok1 = s.token;
    grv(&h, &tok1).await.unwrap();

    // The export key opens a type-5 keyslot made at registration.
    let fs = fixture_fs();
    let cred_id: [u8; 32] = id1.id.clone().try_into().unwrap();
    let slot = keyslot::create_opaque_export(&fs, &cred_id, &reg.export_key, &mut OsRng).unwrap();
    assert_eq!(keyslot::opaque_export_credential(&slot).unwrap(), cred_id);
    let opened = keyslot::open(&slot, Unlock::OpaqueExport(&export_key)).unwrap();
    assert_eq!(*opened.to_bundle(), *fs.to_bundle());

    // A wrong password and an unknown name get the same 401.
    let wrong = failed_sign_in(&h, "bob@example.org", b"pw two").await;
    let unknown = failed_sign_in(&h, "nobody@example.org", b"pw one").await;
    assert_eq!(wrong, unknown, "no hint which part was wrong");
    assert_eq!(wrong.0, 401);

    // Listing: metadata only, with the last sign-in.
    let own = creds(&h, &tok1).await.unwrap().credentials;
    assert_eq!(own.len(), 1);
    assert_eq!(
        (own[0].id.clone(), own[0].method.as_str()),
        (id1.id.clone(), "opaque")
    );
    assert!(own[0].last_used_unix.is_some());

    // Names are unique across users and methods; one user may use the same
    // name for both password methods, each with its own credential.
    let admin_dev = h.sign_in(&admin).await.unwrap();
    for r in [
        opaque_set(&h, &admin_dev, "bob@example.org", b"x")
            .await
            .map(|(id, _)| id),
        set_password_key(&h, &admin_dev, "bob@example.org", b"x").await,
    ] {
        assert_eq!(code(r), (409, "name_taken".into()));
    }
    let pk_id = set_password_key(&h, &bob_dev, "bob@example.org", b"pw six")
        .await
        .unwrap();
    assert_ne!(pk_id.id, id1.id);
    opaque_sign_in(&h, "bob@example.org", b"pw one")
        .await
        .unwrap();
    // The method-6 holder of a name blocks OPAQUE registrations by others.
    let carol = User::new(3);
    let (v2, doc2) = signed_acl(
        &admin,
        2,
        Some(&doc1),
        &[&admin],
        &[&admin, &bob, &carol],
        vec![],
        vec![],
    );
    h.put_acl(v2, None).await.unwrap();
    let carol_dev = h.sign_in(&carol).await.unwrap();
    set_password_key(&h, &carol_dev, "carol", b"c")
        .await
        .unwrap();
    assert_eq!(
        code(
            opaque_set(&h, &bob_dev, "carol", b"x")
                .await
                .map(|(id, _)| id)
        ),
        (409, "name_taken".into())
    );

    // A password change replaces the credential and ends its sessions; the
    // method-6 credential under the same name stays.
    let (id2, reg2) = opaque_set(&h, &tok1, "bob@example.org", b"pw two")
        .await
        .unwrap();
    assert_ne!(id1.id, id2.id);
    assert_ne!(*reg2.export_key, *reg.export_key);
    assert_eq!(code(grv(&h, &tok1).await).0, 401);
    assert_eq!(
        failed_sign_in(&h, "bob@example.org", b"pw one").await.0,
        401
    );
    let (s, _) = opaque_sign_in(&h, "bob@example.org", b"pw two")
        .await
        .unwrap();
    let tok2 = s.token;
    let methods: Vec<String> = creds(&h, &bob_dev)
        .await
        .unwrap()
        .credentials
        .into_iter()
        .map(|c| c.method)
        .collect();
    assert_eq!(methods.len(), 2);
    assert!(methods.contains(&"opaque".into()) && methods.contains(&"password_key".into()));

    // Removing it ends its sessions and its sign-ins.
    let r: R<Empty> = h
        .call(
            "/v1/auth/credentials/remove",
            Some(&bob_dev),
            &CredentialId { id: id2.id.clone() },
        )
        .await;
    r.unwrap();
    assert_eq!(code(grv(&h, &tok2).await).0, 401);
    assert_eq!(
        failed_sign_in(&h, "bob@example.org", b"pw two").await.0,
        401
    );

    // Leaving the ACL ends the sessions and deletes the credentials, which
    // frees the login name.
    opaque_set(&h, &bob_dev, "bob@example.org", b"pw three")
        .await
        .unwrap();
    let (s, _) = opaque_sign_in(&h, "bob@example.org", b"pw three")
        .await
        .unwrap();
    let (v3, _) = signed_acl(
        &admin,
        3,
        Some(&doc2),
        &[&admin],
        &[&admin, &carol],
        vec![],
        vec![],
    );
    h.put_acl(v3, None).await.unwrap();
    assert_eq!(code(grv(&h, &s.token).await).0, 401);
    assert_eq!(
        failed_sign_in(&h, "bob@example.org", b"pw three").await.0,
        401
    );
    opaque_set(&h, &carol_dev, "bob@example.org", b"carol's now")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_names_get_responses_of_the_same_shape() {
    let h = Harness::start_with(opaque_cfg).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    opaque_set(&h, &tok, "ada", b"pw").await.unwrap();
    let origin = h.origin();
    let (_, known) = opaque_start(&h, "ada", b"pw", &origin).await.unwrap();
    let (_, unknown) = opaque_start(&h, "ghost", b"pw", &origin).await.unwrap();
    assert_eq!(known.response.len(), 320);
    assert_eq!(unknown.response.len(), known.response.len());
    assert_eq!(unknown.state.len(), known.state.len());
    assert_eq!(
        (unknown.m_cost_kib, unknown.t_cost, unknown.p_cost),
        (known.m_cost_kib, known.t_cost, known.p_cost)
    );
    // Fresh randomness every time, for real and fake records alike.
    let (_, again) = opaque_start(&h, "ghost", b"pw", &origin).await.unwrap();
    assert_ne!(again.response, unknown.response);
    // The server key in the masked response can't be read without the
    // password, so nothing in the response is stable for a name.
    assert_ne!(again.response[64..96], unknown.response[64..96]);

    // The server setup is data, exported with the rest, not metadata.
    let mut t = h.server.state.store.begin(None).await.unwrap();
    let stored = t
        .get(&zen_server::keys::opaque_setup())
        .await
        .unwrap()
        .unwrap();
    let setup = zen_server::opaque::setup(&h.server.state).await.unwrap();
    assert_eq!(stored, setup.serialize().to_vec());
    assert!(!zen_server::keys::opaque_setup().starts_with(&zen_server::keys::meta_prefix()));
}

#[tokio::test(flavor = "multi_thread")]
async fn opaque_sign_in_checks_state_origin_and_sizes() {
    let h = Harness::start_with(opaque_cfg).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    opaque_set(&h, &tok, "ada", b"pw").await.unwrap();
    let origin = h.origin();

    // A state finishes once.
    let (login, r) = opaque_start(&h, "ada", b"pw", &origin).await.unwrap();
    let (fin, _) = opaque_client_finish(login, b"pw", &r, &origin);
    opaque_finish(&h, r.state.clone(), fin.clone())
        .await
        .unwrap();
    assert_eq!(
        code(opaque_finish(&h, r.state.clone(), fin.clone()).await).0,
        401
    );
    // A tampered state or finalization is refused.
    let (login, r) = opaque_start(&h, "ada", b"pw", &origin).await.unwrap();
    let (fin, _) = opaque_client_finish(login, b"pw", &r, &origin);
    for i in [40, 60, r.state.len() - 1] {
        let mut bad = r.state.clone();
        bad[i] ^= 1;
        assert_eq!(code(opaque_finish(&h, bad, fin.clone()).await).0, 401);
    }
    let mut bad = fin.clone();
    bad[0] ^= 1;
    assert_eq!(code(opaque_finish(&h, r.state.clone(), bad).await).0, 401);
    assert_eq!(
        code(opaque_finish(&h, r.state.clone(), vec![1; 3]).await).0,
        401
    );
    opaque_finish(&h, r.state, fin).await.unwrap();

    // The origin is in the OPAQUE context: a relay that names the real
    // origin to the server while its victim sees another can't finish.
    let relay = "https://relay.example";
    let (login, r) = opaque_start(&h, "ada", b"pw", &origin).await.unwrap();
    let (_, key) = opaque_client_finish(login, b"pw", &r, relay);
    assert!(key.is_none());
    // A relay that passes its own origin on is refused by the origin policy.
    let (login, r) = opaque_start(&h, "ada", b"pw", relay).await.unwrap();
    let (fin, key) = opaque_client_finish(login, b"pw", &r, relay);
    assert!(key.is_some(), "the client and server agree on the context");
    assert_eq!(code(opaque_finish(&h, r.state, fin).await).0, 401);

    // Malformed input.
    let start = |name: &str, origin: &str, request: Vec<u8>| OpaqueLoginStart {
        name: name.into(),
        origin: origin.into(),
        request,
    };
    let hr = &h;
    let call = |req: OpaqueLoginStart| async move {
        hr.call::<_, OpaqueLoginResponse>("/v1/auth/opaque/login/start", None, &req)
            .await
    };
    let (_, good) = zen_core::opaque::Login::start(b"pw", &mut OsRng).unwrap();
    assert_eq!(
        code(call(start("ada", "https://x/", good.clone())).await).0,
        401
    );
    assert_eq!(
        code(call(start("bad name", &origin, good.clone())).await).0,
        400
    );
    assert_eq!(
        code(call(start("ada", &origin, good[..95].to_vec())).await).0,
        400
    );
    assert_eq!(code(call(start("ada", &origin, vec![0; 96])).await).0, 400);
    let r: R<CredentialId> = h
        .call(
            "/v1/auth/opaque/register/finish",
            Some(&tok),
            &OpaqueRegisterFinish {
                name: "ada".into(),
                upload: vec![0; 192],
                m_cost_kib: FIXTURE_ARGON2.m_cost_kib,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .await;
    assert_eq!(code(r).0, 400);
    // Parameters below the registration floor.
    let (reg, request) = zen_core::opaque::Registration::start(b"pw", &mut OsRng).unwrap();
    let resp: OpaqueRegistration = h
        .call(
            "/v1/auth/opaque/register/start",
            Some(&tok),
            &OpaqueRegisterStart {
                name: "ada".into(),
                request,
            },
        )
        .await
        .unwrap();
    let done = reg
        .finish(b"pw", &resp.response, FIXTURE_ARGON2, &mut OsRng)
        .unwrap();
    let r: R<CredentialId> = h
        .call(
            "/v1/auth/opaque/register/finish",
            Some(&tok),
            &OpaqueRegisterFinish {
                name: "ada".into(),
                upload: done.message,
                m_cost_kib: 1024,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .await;
    assert_eq!(code(r).0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn opaque_attempts_lock_the_name() {
    let h = Harness::start_with(|c| {
        opaque_cfg(c);
        c.auth.password_max_failures = 3;
        c.auth.password_lockout_secs = 2;
    })
    .await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    opaque_set(&h, &tok, "ada", b"right").await.unwrap();
    for _ in 0..3 {
        assert_eq!(failed_sign_in(&h, "ada", b"wrong").await.0, 401);
    }
    // Locked at the start, even for the right password.
    let origin = h.origin();
    assert_eq!(
        code(
            opaque_start(&h, "ada", b"right", &origin)
                .await
                .map(|(_, r)| r)
        ),
        (429, "quota".into())
    );
    // Unknown names lock the same way.
    for _ in 0..3 {
        assert_eq!(failed_sign_in(&h, "ghost", b"x").await.0, 401);
    }
    assert_eq!(
        code(
            opaque_start(&h, "ghost", b"x", &origin)
                .await
                .map(|(_, r)| r)
        )
        .0,
        429
    );
    // Every start counts, finished or not: a credential response lets
    // the client test one guess offline.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    for _ in 0..3 {
        opaque_start(&h, "ada", b"guess", &origin).await.unwrap();
    }
    assert_eq!(
        code(
            opaque_start(&h, "ada", b"right", &origin)
                .await
                .map(|(_, r)| r)
        )
        .0,
        429
    );
    // The lock lifts after the lockout; a success clears the count.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    opaque_sign_in(&h, "ada", b"right").await.unwrap();
    for _ in 0..2 {
        assert_eq!(failed_sign_in(&h, "ada", b"wrong").await.0, 401);
    }
    opaque_sign_in(&h, "ada", b"right").await.unwrap();
    for _ in 0..2 {
        assert_eq!(failed_sign_in(&h, "ada", b"wrong").await.0, 401);
    }
    opaque_sign_in(&h, "ada", b"right").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn opaque_is_off_by_default() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let off = (403, "method_disabled".to_string());
    assert_eq!(
        code(opaque_set(&h, &tok, "ada", b"pw").await.map(|(id, _)| id)),
        off
    );
    let r: R<CredentialId> = h
        .call(
            "/v1/auth/opaque/register/finish",
            Some(&tok),
            &OpaqueRegisterFinish {
                name: "ada".into(),
                upload: vec![0; 192],
                m_cost_kib: FIXTURE_ARGON2.m_cost_kib,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .await;
    assert_eq!(code(r), off);
    assert_eq!(
        code(
            opaque_start(&h, "ada", b"pw", &h.origin())
                .await
                .map(|(_, r)| r)
        ),
        off
    );
    assert_eq!(
        code(opaque_finish(&h, vec![0; 100], vec![0; 64]).await),
        off
    );
    let auth = h.get::<Info>("/v1/info").await.auth.unwrap();
    assert!(!auth.methods.contains(&"opaque".to_string()));
}

/// A whole method-6 sign-in (auth.md §11.3).
async fn password_key_sign_in(h: &Harness, name: &str, pw: &[u8]) -> R<Session> {
    let p: PasswordParams = h
        .call(
            "/v1/auth/password/params",
            None,
            &PasswordParamsRequest { name: name.into() },
        )
        .await?;
    let core = zen_core::keyslot::Argon2Params {
        m_cost_kib: p.m_cost_kib,
        t_cost: p.t_cost,
        p_cost: p.p_cost,
    };
    let key = PasswordKey::derive(pw, &p.salt.clone().try_into().unwrap(), core).unwrap();
    let c: Challenge = h.call("/v1/auth/challenge", None, &Empty {}).await?;
    let origin = h.origin();
    let sig = key.sign_session(&c.challenge, &origin).unwrap();
    h.call(
        "/v1/auth/password/session",
        None,
        &PasswordSessionRequest {
            name: name.into(),
            challenge: c.challenge,
            origin,
            sig,
        },
    )
    .await
}

/// Methods 6 and 3 count failures on a name separately, each with the
/// full cap (auth.md §8.5): a lock of one leaves the other working, and a
/// success of one doesn't clear the other's count.
#[tokio::test(flavor = "multi_thread")]
async fn each_password_method_has_its_own_limit() {
    let h = Harness::start_with(|c| {
        opaque_cfg(c);
        c.auth.password_max_failures = 3;
        c.auth.password_lockout_secs = 60;
    })
    .await;
    let users: Vec<User> = (1..=4).map(User::new).collect();
    let refs: Vec<&User> = users.iter().collect();
    h.claim(refs[0], &refs[1..]).await;
    let names = ["ada", "bea", "cy", "dee"];
    for (u, name) in users.iter().zip(names) {
        let tok = h.sign_in(u).await.unwrap();
        set_password_key(&h, &tok, name, b"right").await.unwrap();
        opaque_set(&h, &tok, name, b"right").await.unwrap();
    }
    let origin = h.origin();
    let opaque_locked = |name: &'static str| {
        let (h, origin) = (&h, origin.clone());
        async move {
            code(
                opaque_start(h, name, b"right", &origin)
                    .await
                    .map(|(_, r)| r),
            ) == (429, "quota".into())
        }
    };
    let key_locked = |name: &'static str| {
        let h = &h;
        async move { code(password_key_sign_in(h, name, b"right").await) == (429, "quota".into()) }
    };

    // Locking method 6 leaves method 3 working for the same name.
    for _ in 0..3 {
        assert_eq!(code(password_key_sign_in(&h, "ada", b"wrong").await).0, 401);
    }
    assert!(key_locked("ada").await);
    opaque_sign_in(&h, "ada", b"right").await.unwrap();
    assert!(
        key_locked("ada").await,
        "an OPAQUE success doesn't unlock method 6"
    );

    // And the reverse.
    for _ in 0..3 {
        assert_eq!(failed_sign_in(&h, "bea", b"wrong").await.0, 401);
    }
    assert!(opaque_locked("bea").await);
    password_key_sign_in(&h, "bea", b"right").await.unwrap();
    assert!(
        opaque_locked("bea").await,
        "a method-6 success doesn't unlock OPAQUE"
    );

    // A success with one method doesn't clear the other's count.
    for _ in 0..2 {
        assert_eq!(code(password_key_sign_in(&h, "cy", b"wrong").await).0, 401);
    }
    opaque_sign_in(&h, "cy", b"right").await.unwrap();
    assert_eq!(code(password_key_sign_in(&h, "cy", b"wrong").await).0, 401);
    assert!(key_locked("cy").await);
    for _ in 0..2 {
        assert_eq!(failed_sign_in(&h, "dee", b"wrong").await.0, 401);
    }
    password_key_sign_in(&h, "dee", b"right").await.unwrap();
    assert_eq!(failed_sign_in(&h, "dee", b"wrong").await.0, 401);
    assert!(opaque_locked("dee").await);

    // Unknown names are counted per method too.
    for _ in 0..3 {
        assert_eq!(code(password_key_sign_in(&h, "ghost", b"x").await).0, 401);
    }
    assert_eq!(code(password_key_sign_in(&h, "ghost", b"x").await).0, 429);
    assert_eq!(failed_sign_in(&h, "ghost", b"x").await.0, 401);
}
