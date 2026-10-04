//! Sign-in method 2, passkeys (spec/auth.md §7), end to end with a
//! software authenticator.

mod common;

use common::*;
use zen_proto::*;
use zen_server::webauthn::soft::Authenticator;
use zen_server::webauthn::{FLAG_UP, FLAG_UV};

type R<T> = Result<T, ApiErr>;

fn code<T: std::fmt::Debug>(r: R<T>) -> (u16, String) {
    let (s, e) = r.expect_err("expected an error");
    (s, e.code)
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

async fn register_begin(h: &Harness, tok: &[u8]) -> R<PasskeyCreation> {
    h.call("/v1/auth/passkey/register/begin", Some(tok), &Empty {})
        .await
}

/// Register `a` for the session `tok`, the browser at `origin`.
async fn register_at(
    h: &Harness,
    tok: &[u8],
    a: &mut Authenticator,
    origin: &str,
    label: Option<&str>,
) -> R<CredentialId> {
    let opts = register_begin(h, tok).await?;
    let (attestation_object, client_data_json) = a.create(&opts.rp_id, &opts.challenge, origin);
    h.call(
        "/v1/auth/passkey/register/finish",
        Some(tok),
        &PasskeyRegister {
            attestation_object,
            client_data_json,
            label: label.map(str::to_owned),
        },
    )
    .await
}

async fn register(
    h: &Harness,
    tok: &[u8],
    a: &mut Authenticator,
    label: Option<&str>,
) -> R<CredentialId> {
    register_at(h, tok, a, &h.origin(), label).await
}

async fn begin(h: &Harness, user: Option<&User>) -> R<PasskeyRequest> {
    let req = PasskeyBegin {
        user: user.map(|u| u.fp()),
    };
    h.call("/v1/auth/passkey/session/begin", None, &req).await
}

/// The assertion `a` makes for a fresh challenge, the browser at `origin`.
async fn assertion(h: &Harness, a: &mut Authenticator, origin: &str) -> PasskeySession {
    let opts = begin(h, None).await.unwrap();
    let (authenticator_data, client_data_json, signature) =
        a.get(&opts.rp_id, &opts.challenge, origin);
    PasskeySession {
        credential_id: a.credential_id.clone(),
        authenticator_data,
        client_data_json,
        signature,
        user_handle: None,
    }
}

async fn send(h: &Harness, req: &PasskeySession) -> R<Session> {
    h.call("/v1/auth/passkey/session", None, req).await
}

async fn sign_in(h: &Harness, a: &mut Authenticator) -> R<Session> {
    let req = assertion(h, a, &h.origin()).await;
    send(h, &req).await
}

#[tokio::test(flavor = "multi_thread")]
async fn register_and_sign_in() {
    let h = Harness::start().await;
    let (admin, bob) = (User::new(1), User::new(2));
    h.claim(&admin, &[&bob]).await;
    let info: Info = h.get("/v1/info").await;
    let auth = info.auth.unwrap();
    assert!(auth.methods.contains(&"passkey".to_string()));
    assert_eq!(auth.default.as_deref(), Some("password_key"));
    let pk = auth.passkey.unwrap();
    assert_eq!(pk.algorithms, [-8, -7, -257]);
    assert_eq!(pk.user_verification, "required");
    // Nothing is pinned until the first sign-in, so no relying-party id.
    assert_eq!(pk.rp_id, None);

    let bob_dev = h.sign_in(&bob).await.unwrap();
    let info: Info = h.get("/v1/info").await;
    let rp = info.auth.unwrap().passkey.unwrap().rp_id;
    assert_eq!(rp.as_deref(), Some("127.0.0.1"));

    for (n, mut a) in [
        Authenticator::p256(20),
        Authenticator::ed25519(21),
        Authenticator::rsa(22),
    ]
    .into_iter()
    .enumerate()
    {
        let opts = register_begin(&h, &bob_dev).await.unwrap();
        assert_eq!(opts.rp_id, "127.0.0.1");
        assert_eq!(opts.user_handle, bob.fp());
        assert_eq!(opts.exclude.len(), n, "the passkeys already registered");
        let id = register(&h, &bob_dev, &mut a, Some("laptop"))
            .await
            .unwrap();

        // Discoverable: no user named.
        let s = sign_in(&h, &mut a).await.unwrap();
        assert_eq!(s.method.as_deref(), Some("passkey"));
        assert_eq!(s.user_fp, bob.fp());
        assert_eq!(s.device_fp, id.id, "the credential id is the device");
        grv(&h, &s.token).await.unwrap();

        // With the user handle the authenticator returns.
        let mut req = assertion(&h, &mut a, &h.origin()).await;
        req.user_handle = Some(bob.fp());
        send(&h, &req).await.unwrap();
        // A replay is refused.
        assert_eq!(code(send(&h, &req).await), (401, "unauthorized".into()));
        // A user handle of another user is refused.
        let mut req = assertion(&h, &mut a, &h.origin()).await;
        req.user_handle = Some(admin.fp());
        assert_eq!(code(send(&h, &req).await).0, 401);
    }

    // Non-discoverable: the user's credential ids, for this rp id only.
    let opts = begin(&h, Some(&bob)).await.unwrap();
    assert_eq!(opts.allow.len(), 3);
    assert!(begin(&h, Some(&admin)).await.unwrap().allow.is_empty());
    assert!(
        begin(&h, Some(&User::new(9)))
            .await
            .unwrap()
            .allow
            .is_empty()
    );

    // Listed through the generic endpoint, with the last use; metadata
    // only.
    let list = creds(&h, &bob_dev, None).await.unwrap().credentials;
    assert_eq!(list.len(), 3);
    for c in &list {
        assert_eq!(c.method, "passkey");
        assert_eq!(c.label.as_deref(), Some("laptop"));
        assert!(c.last_used_unix.is_some());
    }

    // The same passkey can't be registered twice, by anyone.
    let mut again = Authenticator::p256(20);
    assert_eq!(code(register(&h, &bob_dev, &mut again, None).await).0, 400);
    let admin_dev = h.sign_in(&admin).await.unwrap();
    assert_eq!(
        code(register(&h, &admin_dev, &mut again, None).await).0,
        400
    );
    // An unknown credential.
    let mut stranger = Authenticator::p256(99);
    assert_eq!(code(sign_in(&h, &mut stranger).await).0, 401);
}

/// An RSA-only authenticator (RS256): registered, signed in with, its
/// counter enforced, and refused when its key is outside the policy.
#[tokio::test(flavor = "multi_thread")]
async fn rs256_passkeys() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let mut a = Authenticator::rsa(23);
    let id = register(&h, &dev, &mut a, Some("windows hello"))
        .await
        .unwrap();
    let s = sign_in(&h, &mut a).await.unwrap();
    assert_eq!(s.device_fp, id.id);
    grv(&h, &s.token).await.unwrap();
    // Another key under the same credential id is refused.
    let mut wrong = Authenticator::rsa(24);
    wrong.credential_id = a.credential_id.clone();
    wrong.counter = Some(100);
    assert_eq!(code(sign_in(&h, &mut wrong).await).0, 401);
    a.counter = Some(0);
    assert_eq!(code(sign_in(&h, &mut a).await).0, 401);

    // A 1024-bit key is refused at registration.
    let mut weak = Authenticator::rsa(25);
    weak.key = zen_server::webauthn::soft::Key::Rsa(Box::new(zen_server::webauthn::soft::rsa_key(
        25, 1024,
    )));
    let (s, e) = register(&h, &dev, &mut weak, None).await.unwrap_err();
    assert_eq!(s, 400);
    assert!(e.message.contains("2048"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn counter_regression_is_refused() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let mut a = Authenticator::p256(30);
    register(&h, &dev, &mut a, None).await.unwrap();
    sign_in(&h, &mut a).await.unwrap();
    sign_in(&h, &mut a).await.unwrap();
    // A clone still at an older count.
    a.counter = Some(1);
    let (s, e) = sign_in(&h, &mut a).await.unwrap_err();
    assert_eq!(s, 401);
    assert!(e.message.contains("cloned"), "{}", e.message);
    // The original carries on.
    a.counter = Some(10);
    sign_in(&h, &mut a).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn user_verification_policy() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let mut a = Authenticator::p256(31);
    a.flags = FLAG_UP;
    assert_eq!(code(register(&h, &dev, &mut a, None).await).0, 400);
    a.flags = FLAG_UP | FLAG_UV;
    register(&h, &dev, &mut a, None).await.unwrap();
    a.flags = FLAG_UP;
    assert_eq!(code(sign_in(&h, &mut a).await).0, 401);
    a.flags = 0;
    assert_eq!(code(sign_in(&h, &mut a).await).0, 401);

    // Only presence is required when UV is not.
    let h = Harness::start_with(|c| c.auth.passkey_require_uv = false).await;
    h.claim(&admin, &[]).await;
    let info: Info = h.get("/v1/info").await;
    assert_eq!(
        info.auth.unwrap().passkey.unwrap().user_verification,
        "preferred"
    );
    let dev = h.sign_in(&admin).await.unwrap();
    let mut a = Authenticator::ed25519(32);
    a.flags = FLAG_UP;
    register(&h, &dev, &mut a, None).await.unwrap();
    sign_in(&h, &mut a).await.unwrap();
    a.flags = 0;
    assert_eq!(code(sign_in(&h, &mut a).await).0, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn passkeys_can_be_turned_off() {
    let h = Harness::start_with(|c| c.auth.passkeys = false).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let info: Info = h.get("/v1/info").await;
    let auth = info.auth.unwrap();
    assert!(!auth.methods.contains(&"passkey".to_string()));
    assert_eq!(auth.passkey, None);
    let dev = h.sign_in(&admin).await.unwrap();
    let off = (403, "method_disabled".to_string());
    assert_eq!(code(register_begin(&h, &dev).await), off);
    let r: R<CredentialId> = h
        .call(
            "/v1/auth/passkey/register/finish",
            Some(&dev),
            &PasskeyRegister {
                attestation_object: vec![],
                client_data_json: vec![],
                label: None,
            },
        )
        .await;
    assert_eq!(code(r), off);
    assert_eq!(code(begin(&h, None).await), off);
    let a = Authenticator::p256(40);
    let req = PasskeySession {
        credential_id: a.credential_id.clone(),
        authenticator_data: vec![],
        client_data_json: vec![],
        signature: vec![],
        user_handle: None,
    };
    assert_eq!(code(send(&h, &req).await), off);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_origin_policy_applies() {
    let h =
        Harness::start_with(|c| c.public_origins = vec!["https://zen.example.org".into()]).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let info: Info = h.get("/v1/info").await;
    assert_eq!(
        info.auth.unwrap().passkey.unwrap().rp_id.as_deref(),
        Some("zen.example.org")
    );
    let good = "https://zen.example.org";
    let dev = h.sign_in_at(&admin, good, &h.host_header()).await.unwrap();
    let mut a = Authenticator::p256(50);
    // Registration from another origin, even the Host one, is refused.
    let r = register_at(&h, &dev, &mut a, "https://evil.example", None).await;
    assert_eq!(code(r), (401, "unauthorized".into()));
    let r = register_at(&h, &dev, &mut a, &h.origin(), None).await;
    assert_eq!(code(r), (401, "unauthorized".into()));
    register_at(&h, &dev, &mut a, good, None).await.unwrap();
    // A sign-in relayed through another origin is refused.
    for o in ["https://evil.example", &h.origin()] {
        let req = assertion(&h, &mut a, o).await;
        let (s, e) = send(&h, &req).await.unwrap_err();
        assert_eq!(s, 401);
        assert!(e.message.contains("origin"), "{}", e.message);
    }
    let req = assertion(&h, &mut a, good).await;
    send(&h, &req).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn passkeys_need_a_known_origin() {
    // No public_origins and no pinning: the server knows no origin of its
    // own, so there is no relying-party id.
    let h = Harness::start_with(|c| c.auth.origin_pinning = false).await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let info: Info = h.get("/v1/info").await;
    assert_eq!(info.auth.unwrap().passkey.unwrap().rp_id, None);
    let (s, e) = register_begin(&h, &dev).await.unwrap_err();
    assert_eq!(s, 400);
    assert!(e.message.contains("origin"), "{}", e.message);
    assert_eq!(code(begin(&h, None).await).0, 400);

    // `passkey_rp_id` provides one.
    let h = Harness::start_with(|c| {
        c.auth.origin_pinning = false;
        c.auth.passkey_rp_id = Some("127.0.0.1".into());
    })
    .await;
    h.claim(&admin, &[]).await;
    let dev = h.sign_in(&admin).await.unwrap();
    let mut a = Authenticator::ed25519(60);
    register(&h, &dev, &mut a, None).await.unwrap();
    sign_in(&h, &mut a).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn removal_and_leaving_the_acl_end_passkey_sessions() {
    let h = Harness::start().await;
    let (admin, bob) = (User::new(1), User::new(2));
    let doc1 = h.claim(&admin, &[&bob]).await;
    let bob_dev = h.sign_in(&bob).await.unwrap();
    let mut a = Authenticator::p256(70);
    let id = register(&h, &bob_dev, &mut a, None).await.unwrap();
    let tok = sign_in(&h, &mut a).await.unwrap().token;

    // A passkey session can manage credentials, and register passkeys.
    assert_eq!(creds(&h, &tok, None).await.unwrap().credentials.len(), 1);
    let mut b = Authenticator::ed25519(71);
    register(&h, &tok, &mut b, None).await.unwrap();

    // Removing the passkey ends its sessions and its sign-ins.
    let r: R<Empty> = h
        .call(
            "/v1/auth/credentials/remove",
            Some(&bob_dev),
            &CredentialId { id: id.id.clone() },
        )
        .await;
    r.unwrap();
    assert_eq!(code(grv(&h, &tok).await).0, 401);
    assert_eq!(code(sign_in(&h, &mut a).await).0, 401);
    // Registering it again gives it back.
    let mut a = Authenticator::p256(70);
    register(&h, &bob_dev, &mut a, None).await.unwrap();

    // Leaving the ACL ends every passkey session and deletes the passkeys.
    let tok_b = sign_in(&h, &mut b).await.unwrap().token;
    grv(&h, &tok_b).await.unwrap();
    let (v2, _) = signed_acl(&admin, 2, Some(&doc1), &[&admin], &[&admin], vec![], vec![]);
    h.put_acl(v2, None).await.unwrap();
    assert_eq!(code(grv(&h, &tok_b).await).0, 401);
    let admin_dev = h.sign_in(&admin).await.unwrap();
    assert!(
        creds(&h, &admin_dev, Some(&bob))
            .await
            .unwrap()
            .credentials
            .is_empty()
    );
    assert_eq!(code(sign_in(&h, &mut b).await).0, 401);
}
