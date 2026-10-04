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
