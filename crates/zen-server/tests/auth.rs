//! Sign-in methods, the credential store and the origin policy
//! (spec/auth.md).

mod common;

use common::*;

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
