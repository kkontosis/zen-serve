//! Sign-in methods, the credential store and the origin policy
//! (spec/auth.md).

mod common;

use common::*;
use zen_core::pwkey::PasswordKey;
use zen_core::rng::OsRng;
use zen_core::vectors::FIXTURE_ARGON2;
use zen_proto::*;
use zen_server::state::SessionInfo;

/// Store a session record as an older server, or another method, would
/// have written it. Returns the bearer token.
async fn plant_session(h: &Harness, n: u8, record: Vec<u8>) -> Vec<u8> {
    let token = [n; 32];
    let key = zen_server::keys::session(&zen_server::auth::token_hash(&token));
    let mut t = h.server.state.store.begin(None).await.unwrap();
    t.set(&key, &record);
    t.commit().await.unwrap();
    token.to_vec()
}

fn far_future() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}

type R<T> = Result<T, ApiErr>;

fn code<T: std::fmt::Debug>(r: R<T>) -> (u16, String) {
    let (s, e) = r.expect_err("expected an error");
    (s, e.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn acl_origins_are_validated() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let mut doc = acl_doc(1, None, &[&admin], &[&admin], vec![], vec![]);
    for bad in [
        "https://zen.example.org/",
        "zen.example.org",
        "https://Zen.example",
    ] {
        doc.origins = vec![bad.into()];
        let (signed, _) = sign_doc(&admin, &doc);
        assert_eq!(
            code(h.put_acl(signed, h.server.claim_token.clone()).await),
            (400, "bad_request".into()),
            "{bad}"
        );
    }
    doc.origins = vec!["https://a.example".into(), "https://a.example".into()];
    let (signed, _) = sign_doc(&admin, &doc);
    assert_eq!(
        code(h.put_acl(signed, h.server.claim_token.clone()).await).0,
        400
    );
    doc.origins = vec!["https://a.example".into(), "http://127.0.0.1:8080".into()];
    let (signed, _) = sign_doc(&admin, &doc);
    h.put_acl(signed, h.server.claim_token.clone())
        .await
        .unwrap();
    assert_eq!(h.server.state.acl().origins, doc.origins);
}

#[tokio::test(flavor = "multi_thread")]
async fn info_lists_the_methods_that_are_on() {
    let h = Harness::start().await;
    let info: Info = h.get("/v1/info").await;
    let auth = info.auth.unwrap();
    assert!(auth.methods.contains(&"device_key".to_string()));
    assert!(auth.methods.contains(&"passkey".to_string()));
    // Off by default, or not implemented by this server.
    for m in ["api_token", "opaque", "mtls"] {
        assert!(!auth.methods.contains(&m.to_string()), "{m}");
    }
    assert!(auth.default.is_some());

    let h = Harness::start_with(|c| c.auth.device_keys = false).await;
    let info: Info = h.get("/v1/info").await;
    assert!(
        !info
            .auth
            .unwrap()
            .methods
            .contains(&"device_key".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_device_keys_refuse_sign_in() {
    let h = Harness::start_with(|c| c.auth.device_keys = false).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    assert_eq!(
        code(h.sign_in(&admin).await),
        (403, "method_disabled".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_records_carry_the_method() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let r: R<ReadVersion> = h.call("/v1/grv", Some(&tok), &Empty {}).await;
    assert!(r.is_ok());

    // A record from before sign-in methods (72 bytes) is a device session.
    let mut old = admin.fp();
    old.extend_from_slice(&admin.device.public().fingerprint());
    old.extend_from_slice(&far_future().to_be_bytes());
    let tok = plant_session(&h, 1, old).await;
    let r: R<ReadVersion> = h.call("/v1/grv", Some(&tok), &Empty {}).await;
    assert!(r.is_ok());

    // A session whose method is now off is refused, but not deleted.
    let info = SessionInfo {
        user: admin.fp().try_into().unwrap(),
        cred: [7; 32],
        method: AuthMethod::Opaque,
        expires_unix: far_future(),
    };
    let tok = plant_session(&h, 2, zen_server::auth::encode_session(&info)).await;
    let (status, e) = h
        .call::<_, ReadVersion>("/v1/grv", Some(&tok), &Empty {})
        .await
        .unwrap_err();
    assert_eq!(status, 401);
    assert!(e.message.contains("disabled"), "{}", e.message);
}

// ---- origin policy (auth.md §5)

/// `localhost:<port>`: a second `Host` (and origin) for the same server.
fn localhost(h: &Harness) -> (String, String) {
    let host = format!("localhost:{}", h.server.addr.port());
    (format!("http://{host}"), host)
}

async fn origin_state(h: &Harness, tok: &[u8]) -> OriginState {
    h.call("/v1/admin/origins/get", Some(tok), &Empty {})
        .await
        .unwrap()
}

async fn set_pins(h: &Harness, tok: &[u8], pinned: &[&str]) -> R<Empty> {
    let req = OriginPins {
        pinned: pinned.iter().map(|s| s.to_string()).collect(),
    };
    h.call("/v1/admin/origins/set", Some(tok), &req).await
}

async fn origin_info(h: &Harness) -> OriginInfo {
    let info: Info = h.get("/v1/info").await;
    info.auth.unwrap().origins.unwrap()
}

/// Claim with `admin` and `bob` as members, the claim naming `origin`.
async fn claim_with_origin(
    h: &Harness,
    admin: &User,
    bob: &User,
    origin: Option<&str>,
) -> R<AclVersion> {
    let (signed, _) = signed_acl(admin, 1, None, &[admin], &[admin, bob], vec![], vec![]);
    h.call(
        "/v1/acl/put",
        None,
        &AclPut {
            acl: signed,
            claim: h.server.claim_token.clone(),
            origin: origin.map(String::from),
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn first_contact_origin_is_pinned() {
    let h = Harness::start().await;
    let (admin, bob) = (User::new(1), User::new(2));
    claim_with_origin(&h, &admin, &bob, None).await.unwrap();
    let o = origin_info(&h).await;
    assert!(o.pinning && o.host_fallback && o.origins.is_empty());

    // The first sign-in after the claim pins its origin…
    let (lo, lh) = localhost(&h);
    let tok = h.sign_in_at(&admin, &lo, &lh).await.unwrap();
    let o = origin_info(&h).await;
    assert_eq!(o.origins, vec![lo.clone()]);
    assert!(!o.host_fallback);
    // …and after that the Host header no longer vouches for another one.
    assert_eq!(code(h.sign_in(&bob).await), (401, "unauthorized".into()));
    h.sign_in_at(&bob, &lo, &lh).await.unwrap();
    let s = origin_state(&h, &tok).await;
    assert_eq!(s.pinned, vec![lo.clone()]);
    assert_eq!(s.accepted, vec![lo.clone()]);
    assert!(s.pinning && !s.pinning_always && !s.acl && !s.host_fallback);

    // Only admins see or replace the pins.
    let bob_tok = h.sign_in_at(&bob, &lo, &lh).await.unwrap();
    let r: R<OriginState> = h
        .call("/v1/admin/origins/get", Some(&bob_tok), &Empty {})
        .await;
    assert_eq!(code(r).0, 403);
    assert_eq!(code(set_pins(&h, &bob_tok, &[]).await).0, 403);
    assert_eq!(
        code(set_pins(&h, &tok, &["https://x.example/"]).await).0,
        400
    );

    // An admin replaces the pinned set.
    set_pins(&h, &tok, &[&h.origin()]).await.unwrap();
    h.sign_in(&bob).await.unwrap();
    assert_eq!(code(h.sign_in_at(&bob, &lo, &lh).await).0, 401);
    // An empty set unpins: the next sign-in pins again.
    set_pins(&h, &tok, &[]).await.unwrap();
    assert!(origin_info(&h).await.host_fallback);
    h.sign_in_at(&bob, &lo, &lh).await.unwrap();
    assert_eq!(origin_info(&h).await.origins, vec![lo]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_claim_pins_its_origin() {
    let h = Harness::start().await;
    let (admin, bob) = (User::new(1), User::new(2));
    assert_eq!(
        code(claim_with_origin(&h, &admin, &bob, Some("https://app.example/")).await).0,
        400
    );
    claim_with_origin(&h, &admin, &bob, Some("https://app.example"))
        .await
        .unwrap();
    assert_eq!(
        origin_info(&h).await.origins,
        vec!["https://app.example".to_string()]
    );
    // The Host header is no longer enough: the first contact was the claim.
    assert_eq!(code(h.sign_in(&admin).await).0, 401);
    h.sign_in_at(&admin, "https://app.example", &h.host_header())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn public_origins_turn_pinning_off() {
    let h = Harness::start_with(|c| c.public_origins = vec!["https://app.example".into()]).await;
    assert!(h.server.warnings.is_empty());
    let (admin, bob) = (User::new(1), User::new(2));
    // The claim's origin is not pinned while public_origins is set.
    claim_with_origin(&h, &admin, &bob, Some("https://other.example"))
        .await
        .unwrap();
    assert_eq!(code(h.sign_in(&admin).await).0, 401);
    assert_eq!(
        code(
            h.sign_in_at(&admin, "https://other.example", &h.host_header())
                .await
        )
        .0,
        401
    );
    let tok = h
        .sign_in_at(&admin, "https://app.example", &h.host_header())
        .await
        .unwrap();
    let s = origin_state(&h, &tok).await;
    assert!(!s.pinning && !s.host_fallback);
    assert!(s.pinned.is_empty());
    assert_eq!(s.accepted, vec!["https://app.example".to_string()]);
    let o = origin_info(&h).await;
    assert!(!o.pinning && !o.host_fallback);
}

#[tokio::test(flavor = "multi_thread")]
async fn pinning_always_adds_the_first_contact() {
    let h = Harness::start_with(|c| {
        c.public_origins = vec!["https://app.example".into()];
        c.auth.origin_pinning_always = true;
    })
    .await;
    let (admin, bob) = (User::new(1), User::new(2));
    claim_with_origin(&h, &admin, &bob, None).await.unwrap();
    // Unpinned: the Host header is trusted once, and pinned.
    let tok = h.sign_in(&admin).await.unwrap();
    h.sign_in_at(&bob, "https://app.example", &h.host_header())
        .await
        .unwrap();
    let (lo, lh) = localhost(&h);
    assert_eq!(code(h.sign_in_at(&bob, &lo, &lh).await).0, 401);
    let s = origin_state(&h, &tok).await;
    assert!(s.pinning && s.pinning_always);
    assert_eq!(
        s.accepted,
        vec!["https://app.example".to_string(), h.origin()]
    );
}

/// With `origin_pinning_always`, the first sign-in pins its origin even
/// when that origin is already in `public_origins` (auth.md §5.2): the
/// `Host` origin is then no longer accepted.
#[tokio::test(flavor = "multi_thread")]
async fn pinning_always_pins_a_listed_first_origin() {
    let h = Harness::start_with(|c| {
        c.public_origins = vec!["https://app.example".into()];
        c.auth.origin_pinning_always = true;
    })
    .await;
    let (admin, bob) = (User::new(1), User::new(2));
    claim_with_origin(&h, &admin, &bob, None).await.unwrap();
    let tok = h
        .sign_in_at(&admin, "https://app.example", &h.host_header())
        .await
        .unwrap();
    let s = origin_state(&h, &tok).await;
    assert_eq!(s.pinned, vec!["https://app.example".to_string()]);
    assert_eq!(s.accepted, vec!["https://app.example".to_string()]);
    assert!(!s.host_fallback);
    assert_eq!(code(h.sign_in(&bob).await).0, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn acl_origins_are_accepted_when_turned_on() {
    let acl_origin = "https://acl.example";
    async fn claim(h: &Harness, admin: &User, bob: &User) -> Vec<u8> {
        let mut doc = acl_doc(1, None, &[admin], &[admin, bob], vec![], vec![]);
        doc.origins = vec!["https://acl.example".into()];
        let (signed, bytes) = sign_doc(admin, &doc);
        h.put_acl(signed, h.server.claim_token.clone())
            .await
            .unwrap();
        bytes
    }
    let (admin, bob) = (User::new(1), User::new(2));

    // 7c alone: the ACL's origins replace the Host fallback.
    let h = Harness::start_with(|c| {
        c.auth.acl_origins = true;
        c.auth.origin_pinning = false;
    })
    .await;
    let doc1 = claim(&h, &admin, &bob).await;
    assert_eq!(code(h.sign_in(&admin).await).0, 401);
    h.sign_in_at(&admin, acl_origin, &h.host_header())
        .await
        .unwrap();
    // A version without origins brings the fallback back.
    let (v2, _) = signed_acl(
        &admin,
        2,
        Some(&doc1),
        &[&admin],
        &[&admin, &bob],
        vec![],
        vec![],
    );
    h.put_acl(v2, None).await.unwrap();
    h.sign_in(&admin).await.unwrap();
    assert_eq!(
        code(h.sign_in_at(&admin, acl_origin, &h.host_header()).await).0,
        401
    );

    // Off (the default): the ACL's origins are ignored.
    let h = Harness::start_with(|c| c.auth.origin_pinning = false).await;
    claim(&h, &admin, &bob).await;
    assert_eq!(
        code(h.sign_in_at(&admin, acl_origin, &h.host_header()).await).0,
        401
    );
    h.sign_in(&admin).await.unwrap();

    // With pinning: the union of the pin and the ACL's origins.
    let h = Harness::start_with(|c| c.auth.acl_origins = true).await;
    claim(&h, &admin, &bob).await;
    let tok = h.sign_in(&admin).await.unwrap();
    h.sign_in_at(&bob, acl_origin, &h.host_header())
        .await
        .unwrap();
    let (lo, lh) = localhost(&h);
    assert_eq!(code(h.sign_in_at(&bob, &lo, &lh).await).0, 401);
    let s = origin_state(&h, &tok).await;
    assert_eq!(s.accepted, vec![h.origin(), acl_origin.to_string()]);
    assert_eq!(s.acl_origins, vec![acl_origin.to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_public_origins_warn_at_start_up() {
    let h = Harness::start().await;
    assert_eq!(h.server.warnings.len(), 1);
    let w = &h.server.warnings[0];
    assert!(w.contains("WARNING: public_origins is empty"), "{w}");
    assert!(w.contains("public_origins = [") && w.contains("pinning is on"));
    assert!(w.lines().count() > 10, "a banner, not a line");

    let mut cfg = h.cfg.clone();
    cfg.auth.origin_pinning = false;
    let w = zen_server::origin::startup_warning(&cfg).unwrap();
    assert!(w.contains("NO protection"), "{w}");
    cfg.public_origins = vec!["https://zen.example.org".into()];
    assert_eq!(zen_server::origin::startup_warning(&cfg), None);
}

// ---- method 6: password-derived keys (auth.md §11)

/// Register (or replace) the caller's password key, at the Argon2id floor.
async fn set_password(h: &Harness, tok: &[u8], name: &str, pw: &[u8]) -> R<CredentialId> {
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

async fn pw_params(h: &Harness, name: &str) -> R<PasswordParams> {
    h.call(
        "/v1/auth/password/params",
        None,
        &PasswordParamsRequest { name: name.into() },
    )
    .await
}

/// The whole client side: params, derive, challenge, sign, session.
async fn password_sign_in(h: &Harness, name: &str, pw: &[u8]) -> R<Session> {
    let p = pw_params(h, name).await?;
    let core = zen_core::keyslot::Argon2Params {
        m_cost_kib: p.m_cost_kib,
        t_cost: p.t_cost,
        p_cost: p.p_cost,
    };
    // Unknown names get the configured (expensive) fakes: don't run them.
    let key = if core == FIXTURE_ARGON2 {
        PasswordKey::derive(pw, &p.salt.clone().try_into().unwrap(), core).unwrap()
    } else {
        PasswordKey::create(pw, FIXTURE_ARGON2, &mut OsRng)
            .unwrap()
            .0
    };
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

async fn grv(h: &Harness, tok: &[u8]) -> R<ReadVersion> {
    h.call("/v1/grv", Some(tok), &Empty {}).await
}

async fn creds(h: &Harness, tok: &[u8], user: Option<&User>) -> R<Credentials> {
    let req = CredentialsList {
        user: user.map(|u| u.fp()),
    };
    h.call("/v1/auth/credentials/list", Some(tok), &req).await
}

#[tokio::test(flavor = "multi_thread")]
async fn password_sign_in_and_credentials() {
    let h = Harness::start().await;
    let (admin, bob) = (User::new(1), User::new(2));
    let doc1 = h.claim(&admin, &[&bob]).await;
    let info: Info = h.get("/v1/info").await;
    let auth = info.auth.unwrap();
    assert_eq!(auth.default.as_deref(), Some("password_key"));
    assert!(auth.methods.contains(&"password_key".to_string()));
    assert_eq!(auth.password_params.unwrap().m_cost_kib, 256 * 1024);

    let bob_dev = h.sign_in(&bob).await.unwrap();
    assert_eq!(
        code(password_sign_in(&h, "bob@example.org", b"pw one").await),
        (401, "unauthorized".into())
    );
    let id1 = set_password(&h, &bob_dev, "Bob@Example.org", b"pw one")
        .await
        .unwrap();
    // The login name is normalized, the password is not.
    let s = password_sign_in(&h, " bob@EXAMPLE.org", b"pw one")
        .await
        .unwrap();
    assert_eq!(s.method.as_deref(), Some("password_key"));
    assert_eq!(s.device_fp, id1.id, "the credential id is the device");
    assert_eq!(s.user_fp, bob.fp());
    let pw_tok = s.token;
    grv(&h, &pw_tok).await.unwrap();
    let wrong = password_sign_in(&h, "bob@example.org", b"pw two").await;
    let unknown = password_sign_in(&h, "nobody@example.org", b"pw one").await;
    let (w, u) = (wrong.unwrap_err(), unknown.unwrap_err());
    assert_eq!((w.0, &w.1.code), (401, &"unauthorized".to_string()));
    assert_eq!(w.1.message, u.1.message, "no hint which part was wrong");

    // Names are unique across users.
    let admin_dev = h.sign_in(&admin).await.unwrap();
    assert_eq!(
        code(set_password(&h, &admin_dev, "bob@example.org", b"x").await),
        (409, "name_taken".into())
    );
    assert_eq!(
        code(set_password(&h, &admin_dev, "bad name", b"x").await).0,
        400
    );

    // Listing: own, or any member's as an admin; metadata only.
    let own = creds(&h, &pw_tok, None).await.unwrap().credentials;
    assert_eq!(own.len(), 1);
    assert_eq!(
        (own[0].id.clone(), own[0].method.as_str()),
        (id1.id.clone(), "password_key")
    );
    assert_eq!(
        creds(&h, &admin_dev, Some(&bob)).await.unwrap().credentials,
        own
    );
    assert_eq!(code(creds(&h, &bob_dev, Some(&admin)).await).0, 403);

    // A password change replaces the credential and ends its sessions.
    let id2 = set_password(&h, &pw_tok, "bob@example.org", b"pw two")
        .await
        .unwrap();
    assert_ne!(id1.id, id2.id);
    assert_eq!(code(grv(&h, &pw_tok).await).0, 401);
    assert_eq!(
        code(password_sign_in(&h, "bob@example.org", b"pw one").await).0,
        401
    );
    let pw_tok = password_sign_in(&h, "bob@example.org", b"pw two")
        .await
        .unwrap()
        .token;

    // Removing it: only the owner or an admin; others see "not found".
    let rm = |id: &[u8]| CredentialId { id: id.to_vec() };
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
    let carol_tok = h.sign_in(&carol).await.unwrap();
    let r: R<Empty> = h
        .call(
            "/v1/auth/credentials/remove",
            Some(&carol_tok),
            &rm(&id2.id),
        )
        .await;
    assert_eq!(code(r), (404, "not_found".into()));
    let r: R<Empty> = h
        .call("/v1/auth/credentials/remove", Some(&bob_dev), &rm(&id2.id))
        .await;
    r.unwrap();
    assert_eq!(code(grv(&h, &pw_tok).await).0, 401);
    assert_eq!(
        code(password_sign_in(&h, "bob@example.org", b"pw two").await).0,
        401
    );
    assert!(
        creds(&h, &bob_dev, None)
            .await
            .unwrap()
            .credentials
            .is_empty()
    );
    // An admin removes another member's.
    let id3 = set_password(&h, &bob_dev, "bob@example.org", b"pw three")
        .await
        .unwrap();
    let r: R<Empty> = h
        .call(
            "/v1/auth/credentials/remove",
            Some(&admin_dev),
            &rm(&id3.id),
        )
        .await;
    r.unwrap();

    // Leaving the ACL ends the sessions and deletes the credentials, which
    // frees the login name.
    set_password(&h, &bob_dev, "bob@example.org", b"pw four")
        .await
        .unwrap();
    let pw_tok = password_sign_in(&h, "bob@example.org", b"pw four")
        .await
        .unwrap()
        .token;
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
    assert_eq!(code(grv(&h, &pw_tok).await).0, 401);
    assert!(
        creds(&h, &admin_dev, Some(&bob))
            .await
            .unwrap()
            .credentials
            .is_empty()
    );
    set_password(&h, &carol_tok, "bob@example.org", b"carol's now")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn password_sign_in_checks_challenge_origin_and_purpose() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let (key, salt) = PasswordKey::create(b"pw", FIXTURE_ARGON2, &mut OsRng).unwrap();
    let set = PasswordSet {
        name: "ada".into(),
        salt: salt.to_vec(),
        m_cost_kib: FIXTURE_ARGON2.m_cost_kib,
        t_cost: FIXTURE_ARGON2.t_cost,
        p_cost: FIXTURE_ARGON2.p_cost,
        identity: key.public().encode(),
    };
    let r: R<CredentialId> = h.call("/v1/auth/password/set", Some(&dev), &set).await;
    r.unwrap();
    // Registration enforces the Argon2id floor and a 32-byte salt.
    let weak = PasswordSet {
        m_cost_kib: 1024,
        ..set.clone()
    };
    let r: R<CredentialId> = h.call("/v1/auth/password/set", Some(&dev), &weak).await;
    assert_eq!(code(r).0, 400);
    let short = PasswordSet {
        salt: vec![1; 16],
        ..set.clone()
    };
    let r: R<CredentialId> = h.call("/v1/auth/password/set", Some(&dev), &short).await;
    assert_eq!(code(r).0, 400);
    // No session, no registration.
    let r: R<CredentialId> = h.call("/v1/auth/password/set", None, &set).await;
    assert_eq!(code(r).0, 401);

    // Sign in as "ada" with `sig_for(challenge, origin)`.
    async fn attempt(
        h: &Harness,
        origin: &str,
        challenge: Option<Vec<u8>>,
        sig_for: impl Fn(&[u8], &str) -> Vec<u8>,
    ) -> R<Vec<u8>> {
        let c = match challenge {
            Some(c) => c,
            None => {
                let c: Challenge = h.call("/v1/auth/challenge", None, &Empty {}).await?;
                c.challenge
            }
        };
        let req = PasswordSessionRequest {
            name: "ADA".into(),
            challenge: c.clone(),
            origin: origin.into(),
            sig: sig_for(&c, origin),
        };
        let _: Session = h.call("/v1/auth/password/session", None, &req).await?;
        Ok(c)
    }
    let good = |c: &[u8], o: &str| key.sign_session(c, o).unwrap();
    let used = attempt(&h, &h.origin(), None, good).await.unwrap();
    // A challenge is spent once.
    assert_eq!(
        code(attempt(&h, &h.origin(), Some(used), good).await).0,
        401
    );
    // The origin is bound and checked against the policy (pinned above).
    assert_eq!(
        code(attempt(&h, "https://evil.example", None, good).await).0,
        401
    );
    // A device's session signature is not a password signature.
    let device = |c: &[u8], o: &str| {
        admin
            .device
            .signing()
            .sign(zen_core::labels::SIG_SESSION, &session_message(c, o))
            .unwrap()
    };
    assert_eq!(code(attempt(&h, &h.origin(), None, device).await).0, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_login_names_get_stable_fake_params() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let a = pw_params(&h, "nobody@example.org").await.unwrap();
    // Stable across calls and spellings of the same name…
    assert_eq!(pw_params(&h, " NOBODY@example.org").await.unwrap(), a);
    // …different per name, and shaped like a real answer.
    let b = pw_params(&h, "somebody@example.org").await.unwrap();
    assert_ne!(a.salt, b.salt);
    assert_eq!(a.salt.len(), 32);
    let cfg = h.cfg.auth.password_params();
    assert_eq!(
        (a.m_cost_kib, a.t_cost, a.p_cost),
        (cfg.m_cost_kib, cfg.t_cost, cfg.p_cost)
    );
    // A registered name answers with its own salt and parameters.
    let fake = pw_params(&h, "ada").await.unwrap();
    let tok = h.sign_in(&admin).await.unwrap();
    set_password(&h, &tok, "ada", b"pw").await.unwrap();
    let real = pw_params(&h, "ada").await.unwrap();
    assert_eq!(real.m_cost_kib, FIXTURE_ARGON2.m_cost_kib);
    assert_ne!(real.salt, fake.salt);
    // The fakes come from a key stored with the data (not server
    // metadata), so every node, and a restored copy, answers the same.
    let mut t = h.server.state.store.begin(None).await.unwrap();
    let stored = t.get(&zen_server::keys::params_key()).await.unwrap();
    let key = zen_server::cred::params_key(&h.server.state).await.unwrap();
    assert_eq!(stored.as_deref(), Some(&key[..]));
    assert!(!zen_server::keys::params_key().starts_with(&zen_server::keys::meta_prefix()));
    assert_eq!(code(pw_params(&h, "no spaces allowed").await).0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_password_sign_ins_lock_the_name() {
    let h = Harness::start_with(|c| {
        c.auth.password_max_failures = 3;
        c.auth.password_lockout_secs = 2;
    })
    .await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    set_password(&h, &tok, "ada", b"right").await.unwrap();
    for _ in 0..3 {
        assert_eq!(code(password_sign_in(&h, "ada", b"wrong").await).0, 401);
    }
    // Locked, even for the right password.
    assert_eq!(
        code(password_sign_in(&h, "ada", b"right").await),
        (429, "quota".into())
    );
    // Unknown names lock the same way, so locking reveals nothing.
    for _ in 0..3 {
        assert_eq!(code(password_sign_in(&h, "ghost", b"x").await).0, 401);
    }
    assert_eq!(code(password_sign_in(&h, "ghost", b"x").await).0, 429);
    // Other names are unaffected; the lock lifts after the lockout.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    password_sign_in(&h, "ada", b"right").await.unwrap();
    // A success clears the count.
    for _ in 0..2 {
        assert_eq!(code(password_sign_in(&h, "ada", b"wrong").await).0, 401);
    }
    password_sign_in(&h, "ada", b"right").await.unwrap();
    for _ in 0..2 {
        assert_eq!(code(password_sign_in(&h, "ada", b"wrong").await).0, 401);
    }
    password_sign_in(&h, "ada", b"right").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn password_keys_can_be_turned_off() {
    let h = Harness::start_with(|c| c.auth.password_keys = false).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let off = (403, "method_disabled".to_string());
    assert_eq!(code(pw_params(&h, "ada").await), off);
    assert_eq!(code(set_password(&h, &tok, "ada", b"pw").await), off);
    let req = PasswordSessionRequest {
        name: "ada".into(),
        challenge: vec![0; 32],
        origin: h.origin(),
        sig: vec![],
    };
    let r: R<Session> = h.call("/v1/auth/password/session", None, &req).await;
    assert_eq!(code(r), off);
    let auth = h.get::<Info>("/v1/info").await.auth.unwrap();
    assert!(!auth.methods.contains(&"password_key".to_string()));
    // Next in the order of auth.md §2.
    assert_eq!(auth.default.as_deref(), Some("passkey"));
    assert_eq!(auth.password_params, None);
}

// ---- method 4: API tokens (auth.md §9)

/// A request with a raw `Authorization: Bearer` value.
async fn bearer<Q: serde::Serialize, T: serde::de::DeserializeOwned>(
    h: &Harness,
    path: &str,
    token: &str,
    req: &Q,
) -> R<T> {
    let resp = h
        .http
        .post(format!("{}{}", h.base, path))
        .header("content-type", CBOR)
        .header("authorization", format!("Bearer {token}"))
        .body(to_cbor(req))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap();
    if status == 200 {
        Ok(from_cbor(&body).unwrap())
    } else {
        Err((status, from_cbor(&body).unwrap()))
    }
}

async fn create_token(h: &Harness, tok: &[u8], user: &User, expires: Option<u64>) -> R<ApiToken> {
    let req = ApiTokenCreate {
        user: user.fp(),
        label: Some("ci bot".into()),
        expires_unix: expires,
    };
    h.call("/v1/auth/tokens/create", Some(tok), &req).await
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn api_tokens_are_off_by_default() {
    let h = Harness::start().await;
    let (admin, bot) = (User::new(1), User::new(2));
    h.claim(&admin, &[&bot]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    assert_eq!(
        code(create_token(&h, &tok, &bot, None).await),
        (403, "method_disabled".into())
    );
    let auth = h.get::<Info>("/v1/info").await.auth.unwrap();
    assert!(!auth.methods.contains(&"api_token".to_string()));

    // A token stored while the method was on is refused while it is off.
    let secret = [9u8; 32];
    let id = zen_server::token::token_id(&secret);
    let user: [u8; 32] = bot.fp().try_into().unwrap();
    let rec = zen_server::cred::CredRecord {
        method: AuthMethod::ApiToken.id(),
        created_unix: now(),
        ..Default::default()
    };
    let mut t = h.server.state.store.begin(None).await.unwrap();
    zen_server::cred::put(&mut t, &user, &id, &rec);
    t.commit().await.unwrap();
    use base64::Engine;
    let text = format!(
        "{API_TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)
    );
    let (status, e) = bearer::<_, ReadVersion>(&h, "/v1/grv", &text, &Empty {})
        .await
        .unwrap_err();
    assert_eq!(status, 401);
    assert!(e.message.contains("disabled"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_tokens_create_use_revoke_expire() {
    let h = Harness::start_with(|c| c.auth.api_tokens = true).await;
    let (admin, bot, carol) = (User::new(1), User::new(2), User::new(3));
    let doc1 = h.claim(&admin, &[&bot, &carol]).await;
    let admin_tok = h.sign_in(&admin).await.unwrap();
    let carol_tok = h.sign_in(&carol).await.unwrap();
    let auth = h.get::<Info>("/v1/info").await.auth.unwrap();
    assert!(auth.methods.contains(&"api_token".to_string()));
    assert_ne!(auth.default.as_deref(), Some("api_token"));

    // Only admins issue tokens, only for members, never already expired.
    assert_eq!(code(create_token(&h, &carol_tok, &bot, None).await).0, 403);
    assert_eq!(
        code(create_token(&h, &admin_tok, &User::new(9), None).await).0,
        400
    );
    assert_eq!(
        code(create_token(&h, &admin_tok, &bot, Some(now() - 1)).await).0,
        400
    );
    let t = create_token(&h, &admin_tok, &bot, None).await.unwrap();
    assert!(t.token.starts_with("zen_at_") && t.token.len() == 50);

    // Used directly as a bearer token, with the member's rights.
    let c = Commit {
        commit_id: cid(1),
        writes: vec![Write {
            fs: 1,
            key: vec![1; 16],
            value: Some(vec![2; 64]),
        }],
        ..Default::default()
    };
    let r: CommitResult = bearer(&h, "/v1/commit", &t.token, &c).await.unwrap();
    // The token id is the device: a replay from the same token is
    // idempotent, from another device it is refused.
    let again: CommitResult = bearer(&h, "/v1/commit", &t.token, &c).await.unwrap();
    assert_eq!(again.versionstamp, r.versionstamp);
    let r2: R<CommitResult> = h.call("/v1/commit", Some(&carol_tok), &c).await;
    assert_eq!(code(r2), (409, "commit_id_reused".into()));
    // …and on the stream, as the `auth` token.
    let ws = connect(&h, t.token.as_bytes()).await;
    drop(ws);

    // Listed as metadata; never the secret.
    let list = creds(&h, &admin_tok, Some(&bot)).await.unwrap().credentials;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, t.id);
    assert_eq!(list[0].method, "api_token");
    assert_eq!(list[0].label.as_deref(), Some("ci bot"));

    // A token can't manage sign-in, and is not a session.
    let r: R<Credentials> = bearer(
        &h,
        "/v1/auth/credentials/list",
        &t.token,
        &CredentialsList::default(),
    )
    .await;
    assert_eq!(code(r).0, 403);
    let r: R<Empty> = bearer(&h, "/v1/auth/logout", &t.token, &Empty {}).await;
    assert_eq!(code(r).0, 400);
    let own = create_token(&h, &admin_tok, &admin, None).await.unwrap();
    let r: R<OriginState> = bearer(&h, "/v1/admin/origins/get", &own.token, &Empty {}).await;
    assert_eq!(code(r).0, 403);
    let r: R<ApiToken> = bearer(
        &h,
        "/v1/auth/tokens/create",
        &own.token,
        &ApiTokenCreate {
            user: bot.fp(),
            label: None,
            expires_unix: None,
        },
    )
    .await;
    assert_eq!(code(r).0, 403);

    // A wrong or malformed token.
    let mut wrong = t.token.clone();
    wrong.replace_range(10..11, if &wrong[10..11] == "A" { "B" } else { "A" });
    let r: R<ReadVersion> = bearer(&h, "/v1/grv", &wrong, &Empty {}).await;
    assert_eq!(code(r).0, 401);

    // Revoked by an admin: refused at once.
    let rm = CredentialId { id: t.id.clone() };
    let r: R<Empty> = h
        .call("/v1/auth/credentials/remove", Some(&admin_tok), &rm)
        .await;
    r.unwrap();
    let r: R<ReadVersion> = bearer(&h, "/v1/grv", &t.token, &Empty {}).await;
    assert_eq!(code(r).0, 401);

    // Expiry: refused once expired, and swept.
    let short = create_token(&h, &admin_tok, &bot, Some(now() + 2))
        .await
        .unwrap();
    let r: R<ReadVersion> = bearer(&h, "/v1/grv", &short.token, &Empty {}).await;
    r.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(3100)).await;
    let r: R<ReadVersion> = bearer(&h, "/v1/grv", &short.token, &Empty {}).await;
    assert_eq!(code(r).0, 401);
    assert_eq!(
        creds(&h, &admin_tok, Some(&bot))
            .await
            .unwrap()
            .credentials
            .len(),
        1
    );
    zen_server::sweep_once(&h.server.state).await.unwrap();
    assert!(
        creds(&h, &admin_tok, Some(&bot))
            .await
            .unwrap()
            .credentials
            .is_empty()
    );

    // Leaving the ACL ends the member's tokens.
    let t = create_token(&h, &admin_tok, &bot, None).await.unwrap();
    let (v2, _) = signed_acl(
        &admin,
        2,
        Some(&doc1),
        &[&admin],
        &[&admin, &carol],
        vec![],
        vec![],
    );
    h.put_acl(v2, None).await.unwrap();
    let r: R<ReadVersion> = bearer(&h, "/v1/grv", &t.token, &Empty {}).await;
    assert_eq!(code(r).0, 401);
}
