//! Sign-in methods, the credential store and the origin policy
//! (spec/auth.md).

mod common;

use common::*;
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
    // Off by default, or not implemented by this server.
    for m in ["api_token", "opaque", "passkey", "mtls"] {
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
