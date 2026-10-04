//! Sign-in method 5, TLS client certificates (auth.md §10): native TLS
//! with a client CA, and a trusted TLS-terminating proxy.

mod common;

use common::pki::*;
use common::*;
use zen_proto::*;

type R<T> = Result<T, ApiErr>;

fn code<T: std::fmt::Debug>(r: R<T>) -> (u16, String) {
    let (s, e) = r.expect_err("expected an error");
    (s, e.code)
}

fn message<T: std::fmt::Debug>(r: R<T>) -> String {
    r.expect_err("expected an error").1.message
}

/// A server with native TLS and `ca` as its client CA (unless `f` changes
/// that). The harness's own client presents no certificate.
async fn native(
    ca: &Ca,
    f: impl FnOnce(&mut zen_server::config::Config),
) -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_config(dir.path(), &ca.server(), Some(ca));
    let mut h = Harness::start_with(|c| {
        c.tls = Some(tls);
        f(c);
    })
    .await;
    h.use_https(https_client(ca, None));
    (h, dir)
}

/// A plain-HTTP server that trusts `proxies` to forward certificates.
async fn proxied(proxies: &[&str]) -> Harness {
    let proxies = proxies.iter().map(|s| s.to_string()).collect();
    Harness::start_with(|c| c.auth.mtls_trusted_proxies = proxies).await
}

async fn mtls_session(h: &Harness, http: &reqwest::Client, headers: &[(&str, &str)]) -> R<Session> {
    h.call_via(http, headers, "/v1/auth/mtls/session", None, &Empty {})
        .await
}

async fn register(
    h: &Harness,
    http: &reqwest::Client,
    headers: &[(&str, &str)],
    tok: &[u8],
    req: &MtlsRegister,
) -> R<CredentialId> {
    h.call_via(http, headers, "/v1/auth/mtls/register", Some(tok), req)
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

async fn remove(h: &Harness, tok: &[u8], id: &[u8]) -> R<Empty> {
    let req = CredentialId { id: id.to_vec() };
    h.call("/v1/auth/credentials/remove", Some(tok), &req).await
}

async fn methods(h: &Harness) -> Vec<String> {
    let info: Info = h.get("/v1/info").await;
    info.auth.unwrap().methods
}

/// nginx's `$ssl_client_escaped_cert`: the PEM, URL-escaped.
fn escaped_pem(i: &Ident) -> String {
    i.pem()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn base64_der(i: &Ident) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(&i.cert)
}

#[tokio::test(flavor = "multi_thread")]
async fn native_sign_in_end_to_end() {
    let ca = Ca::new("ca");
    let (h, _dir) = native(&ca, |_| {}).await;
    assert!(methods(&h).await.contains(&"mtls".to_string()));
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let cert = ca.client("admin laptop");
    let with_cert = https_client(&ca, Some(&cert));

    // Nothing registered yet; no certificate at all.
    let r = mtls_session(&h, &with_cert, &[]).await;
    assert_eq!(code(r).0, 401);
    let r = mtls_session(&h, &h.http, &[]).await;
    assert!(message(r).contains("no client certificate"));

    // Other methods work on the same port, with or without a certificate.
    let tok = h.sign_in(&admin).await.unwrap();
    let r = register(&h, &h.http, &[], &tok, &MtlsRegister::default()).await;
    assert_eq!(code(r).0, 400, "no certificate on this connection");
    let req = MtlsRegister {
        label: Some("laptop".into()),
        ..Default::default()
    };
    let id = register(&h, &with_cert, &[], &tok, &req).await.unwrap();
    assert_eq!(id.id, cert.spki_sha256(), "the id is SHA-256(SPKI)");
    let r = register(&h, &with_cert, &[], &tok, &req).await;
    assert_eq!(code(r).0, 400, "registered already");

    // Sign in.
    let s = mtls_session(&h, &with_cert, &[]).await.unwrap();
    assert_eq!(s.method.as_deref(), Some("mtls"));
    assert_eq!(s.device_fp, id.id);
    assert_eq!(s.user_fp, admin.fp());
    grv(&h, &s.token).await.unwrap();
    let list = creds(&h, &s.token, None).await.unwrap().credentials;
    let c = list.iter().find(|c| c.id == id.id).unwrap();
    assert_eq!(c.method, "mtls");
    assert_eq!(c.label.as_deref(), Some("laptop"));
    assert!(c.last_used_unix.is_some());

    // A renewed certificate with the same key signs in as the same
    // credential.
    let renewed = ca.issue("admin laptop 2027", Usage::Client, cert.key.clone());
    assert_ne!(renewed.cert, cert.cert);
    let s2 = mtls_session(&h, &https_client(&ca, Some(&renewed)), &[])
        .await
        .unwrap();
    assert_eq!(s2.device_fp, id.id);

    // A certificate of the same CA that isn't registered.
    let other = https_client(&ca, Some(&ca.client("stranger")));
    let r = mtls_session(&h, &other, &[]).await;
    assert!(message(r).contains("unregistered"));

    // The proxy header means nothing without proxy mode: a client without
    // a certificate can't name one.
    let header = escaped_pem(&cert);
    let r = mtls_session(&h, &h.http, &[("x-client-cert", &header)]).await;
    assert!(message(r).contains("no client certificate"));

    // Removing the credential ends its sessions and its sign-ins.
    remove(&h, &tok, &id.id).await.unwrap();
    assert_eq!(code(grv(&h, &s.token).await).0, 401);
    assert_eq!(code(grv(&h, &s2.token).await).0, 401);
    assert_eq!(code(mtls_session(&h, &with_cert, &[]).await).0, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_and_expired_certificates_fail_the_handshake() {
    let ca = Ca::new("ca");
    let (h, _dir) = native(&ca, |_| {}).await;
    let url = format!("{}/v1/info", h.base);
    let rogue = https_client(&ca, Some(&Ca::new("rogue").client("mallory")));
    assert!(rogue.get(&url).send().await.is_err());
    let expired = https_client(&ca, Some(&ca.issue("old", Usage::Expired, Key::p256())));
    assert!(expired.get(&url).send().await.is_err());
    // A client without a certificate is fine.
    assert!(h.http.get(&url).send().await.unwrap().status().is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn admins_bind_certificates_and_members_leave() {
    let ca = Ca::new("ca");
    let (h, _dir) = native(&ca, |_| {}).await;
    let (admin, bob, carol) = (User::new(1), User::new(2), User::new(3));
    let doc = h.claim(&admin, &[&bob]).await;
    let admin_tok = h.sign_in(&admin).await.unwrap();
    let bob_tok = h.sign_in(&bob).await.unwrap();
    let bob_cert = ca.client("bob");
    let pem = bob_cert.pem().into_bytes();

    // Uploading takes an admin, as does binding for someone else.
    let upload = |user: Option<&User>| MtlsRegister {
        user: user.map(|u| u.fp()),
        cert: Some(pem.clone()),
        label: Some("issued by IT".into()),
    };
    let r = register(&h, &h.http, &[], &bob_tok, &upload(None)).await;
    assert_eq!(code(r), (403, "forbidden".into()));
    let bob_http = https_client(&ca, Some(&bob_cert));
    let r = register(
        &h,
        &bob_http,
        &[],
        &bob_tok,
        &MtlsRegister {
            user: Some(admin.fp()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(code(r).0, 403);
    // Only members.
    let r = register(&h, &h.http, &[], &admin_tok, &upload(Some(&carol))).await;
    assert_eq!(code(r).0, 400);
    let r = register(
        &h,
        &h.http,
        &[],
        &admin_tok,
        &MtlsRegister {
            user: Some(bob.fp()),
            cert: Some(b"not a certificate".to_vec()),
            label: None,
        },
    )
    .await;
    assert_eq!(code(r).0, 400);
    // DER works as well as PEM.
    let id = register(&h, &h.http, &[], &admin_tok, &upload(Some(&bob)))
        .await
        .unwrap();
    assert_eq!(id.id, bob_cert.spki_sha256());
    let list = creds(&h, &bob_tok, None).await.unwrap().credentials;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].label.as_deref(), Some("issued by IT"));
    let dave_cert = ca.client("dave");
    let der = MtlsRegister {
        user: Some(bob.fp()),
        cert: Some(dave_cert.cert.to_vec()),
        label: None,
    };
    register(&h, &h.http, &[], &admin_tok, &der).await.unwrap();

    // Bob signs in with it.
    let s = mtls_session(&h, &bob_http, &[]).await.unwrap();
    assert_eq!(s.user_fp, bob.fp());
    grv(&h, &s.token).await.unwrap();

    // Leaving the ACL ends the session and deletes the credentials.
    let (v2, _) = signed_acl(&admin, 2, Some(&doc), &[&admin], &[&admin], vec![], vec![]);
    h.put_acl(v2, None).await.unwrap();
    assert_eq!(code(grv(&h, &s.token).await).0, 401);
    assert_eq!(code(mtls_session(&h, &bob_http, &[]).await).0, 401);
    assert!(
        creds(&h, &admin_tok, Some(&bob))
            .await
            .unwrap()
            .credentials
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_proxy_forwards_the_certificate() {
    let h = proxied(&["127.0.0.0/8"]).await;
    assert!(methods(&h).await.contains(&"mtls".to_string()));
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let ca = Ca::new("proxy's ca");
    let cert = ca.client("admin");
    let pem = escaped_pem(&cert);
    let hdr = [("x-client-cert", pem.as_str())];

    // Registered from the forwarded certificate.
    let id = register(&h, &h.http, &hdr, &tok, &MtlsRegister::default())
        .await
        .unwrap();
    assert_eq!(id.id, cert.spki_sha256());
    let s = mtls_session(&h, &h.http, &hdr).await.unwrap();
    assert_eq!(s.device_fp, id.id);
    grv(&h, &s.token).await.unwrap();

    // Base64 DER, as Caddy or HAProxy send it, URL-escaped or not, and a
    // chain whose first certificate counts.
    let b64 = base64_der(&cert);
    for v in [
        b64.clone(),
        b64.replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D"),
        format!("{b64},{}", base64_der(&ca.client("intermediate"))),
    ] {
        let s = mtls_session(&h, &h.http, &[("x-client-cert", &v)]).await;
        assert_eq!(s.unwrap().device_fp, id.id, "{v}");
    }

    // No header, or an empty one: no certificate. Garbage: refused.
    assert!(message(mtls_session(&h, &h.http, &[]).await).contains("no client certificate"));
    let r = mtls_session(&h, &h.http, &[("x-client-cert", "")]).await;
    assert!(message(r).contains("no client certificate"));
    let r = mtls_session(&h, &h.http, &[("x-client-cert", "%%%")]).await;
    assert!(message(r).contains("doesn't parse"));
    // Another certificate isn't registered.
    let other = escaped_pem(&ca.client("other"));
    let r = mtls_session(&h, &h.http, &[("x-client-cert", &other)]).await;
    assert!(message(r).contains("unregistered"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_header_name_is_configurable() {
    let h = Harness::start_with(|c| {
        c.auth.mtls_trusted_proxies = vec!["127.0.0.1".into(), "::1".into()];
        c.auth.mtls_proxy_header = "ssl-client-cert".into();
    })
    .await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let cert = Ca::new("ca").client("admin");
    let pem = escaped_pem(&cert);
    register(
        &h,
        &h.http,
        &[("ssl-client-cert", &pem)],
        &tok,
        &MtlsRegister::default(),
    )
    .await
    .unwrap();
    mtls_session(&h, &h.http, &[("ssl-client-cert", &pem)])
        .await
        .unwrap();
    let r = mtls_session(&h, &h.http, &[("x-client-cert", &pem)]).await;
    assert!(message(r).contains("no client certificate"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_header_from_an_untrusted_address_is_refused() {
    // Proxy mode is on, but the test client is not the proxy.
    let h = proxied(&["10.0.0.5", "fd00::/8"]).await;
    assert!(methods(&h).await.contains(&"mtls".to_string()));
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let cert = Ca::new("ca").client("admin");
    // An admin can still bind it by upload ...
    let up = MtlsRegister {
        cert: Some(cert.pem().into_bytes()),
        ..Default::default()
    };
    register(&h, &h.http, &[], &tok, &up).await.unwrap();
    // ... but nobody can present it through the header from here.
    let pem = escaped_pem(&cert);
    let hdr = [("x-client-cert", pem.as_str())];
    let r = mtls_session(&h, &h.http, &hdr).await;
    let (status, e) = r.unwrap_err();
    assert_eq!(status, 401);
    assert!(e.message.contains("not a trusted proxy"), "{}", e.message);
    let r = register(&h, &h.http, &hdr, &tok, &MtlsRegister::default()).await;
    assert_eq!(code(r).0, 401);
    // Without it: simply no certificate.
    assert!(message(mtls_session(&h, &h.http, &[]).await).contains("no client certificate"));
}

#[tokio::test(flavor = "multi_thread")]
async fn turned_off_or_dormant() {
    // Off: 403 method_disabled, not offered, and no certificates requested,
    // so even a foreign certificate connects.
    let ca = Ca::new("ca");
    let (h, _dir) = native(&ca, |c| c.auth.mtls = false).await;
    assert!(!methods(&h).await.contains(&"mtls".to_string()));
    let rogue = https_client(&ca, Some(&Ca::new("rogue").client("x")));
    let r = mtls_session(&h, &rogue, &[]).await;
    assert_eq!(code(r), (403, "method_disabled".into()));
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let r = register(&h, &h.http, &[], &tok, &MtlsRegister::default()).await;
    assert_eq!(code(r), (403, "method_disabled".into()));
    drop(h);

    // On but dormant (no client CA, no trusted proxy): not offered, and
    // sign-in gets 401 saying why.
    let h = Harness::start().await;
    assert!(h.cfg.auth.mtls);
    assert!(!methods(&h).await.contains(&"mtls".to_string()));
    let r = mtls_session(&h, &h.http, &[]).await;
    let (status, e) = r.unwrap_err();
    assert_eq!(status, 401);
    assert!(e.message.contains("not set up"), "{}", e.message);
    // Native TLS without a client CA is dormant too.
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_config(dir.path(), &ca.server(), None);
    let mut h = Harness::start_with(|c| c.tls = Some(tls)).await;
    h.use_https(https_client(&ca, Some(&ca.client("a"))));
    assert!(!methods(&h).await.contains(&"mtls".to_string()));
    assert_eq!(code(mtls_session(&h, &h.http, &[]).await).0, 401);
}
