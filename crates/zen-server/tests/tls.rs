//! Native TLS: the pure-Rust rustls provider (`tls::provider`) in real
//! handshakes, and the HTTPS listener (operations.md §8).

mod common;

use common::pki::*;
use common::*;
use rustls::pki_types::ServerName;
use rustls::server::WebPkiClientVerifier;
use rustls::{CipherSuite, NamedGroup, RootCertStore, ServerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zen_proto::*;
use zen_server::tls::provider::{self, SECP256R1, X25519, X25519MLKEM768};

/// What a handshake negotiated.
#[derive(Debug)]
struct Negotiated {
    group: NamedGroup,
    suite: CipherSuite,
    /// The client certificate the server verified.
    client_cert: Option<Vec<u8>>,
}

/// A server configuration on the provider, with `ident`, asking for
/// optional client certificates from `client_ca`.
fn server_config(ident: &Ident, client_ca: Option<&Ca>) -> Arc<ServerConfig> {
    let p = Arc::new(provider::provider());
    let b = ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap();
    let b = match client_ca {
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            roots.add(ca.cert.clone()).unwrap();
            let v = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), p)
                .allow_unauthenticated()
                .build()
                .unwrap();
            b.with_client_cert_verifier(v)
        }
        None => b.with_no_client_auth(),
    };
    Arc::new(
        b.with_single_cert(vec![ident.cert.clone()], ident.key.der())
            .unwrap(),
    )
}

/// Handshake over an in-memory pipe, then send a message each way.
async fn handshake(
    server: Arc<ServerConfig>,
    client: rustls::ClientConfig,
) -> Result<Negotiated, String> {
    let (a, b) = tokio::io::duplex(1 << 16);
    let acceptor = tokio_rustls::TlsAcceptor::from(server);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let name = ServerName::try_from("localhost").unwrap();
    let (s, c) = tokio::join!(acceptor.accept(a), connector.connect(name, b));
    let (mut s, mut c) = match (s, c) {
        (Ok(s), Ok(c)) => (s, c),
        (s, c) => {
            return Err(format!(
                "server: {:?}, client: {:?}",
                s.err(),
                c.err().map(|e| e.to_string())
            ));
        }
    };
    // Application data in both directions, through each AEAD.
    c.write_all(b"ping").await.unwrap();
    c.flush().await.unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    s.write_all(b"pong").await.unwrap();
    s.flush().await.unwrap();
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong");
    let (_, conn) = s.get_ref();
    let (_, cconn) = c.get_ref();
    assert_eq!(
        conn.negotiated_key_exchange_group().unwrap().name(),
        cconn.negotiated_key_exchange_group().unwrap().name()
    );
    Ok(Negotiated {
        group: conn.negotiated_key_exchange_group().unwrap().name(),
        suite: conn.negotiated_cipher_suite().unwrap().suite(),
        client_cert: conn
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.to_vec()),
    })
}

#[tokio::test]
async fn the_post_quantum_hybrid_is_preferred() {
    let ca = Ca::new("ca");
    let got = handshake(server_config(&ca.server(), None), client_config(&ca, None))
        .await
        .unwrap();
    assert_eq!(got.group, NamedGroup::X25519MLKEM768);
    assert_eq!(got.suite, CipherSuite::TLS13_AES_128_GCM_SHA256);
    assert_eq!(got.client_cert, None);
}

#[tokio::test]
async fn every_group_and_suite_works() {
    let ca = Ca::new("ca");
    let server = server_config(&ca.server(), None);
    let groups: [&'static dyn rustls::crypto::SupportedKxGroup; 3] =
        [&X25519MLKEM768, &X25519, &SECP256R1];
    for group in groups {
        for suite in provider::provider().cipher_suites {
            let mut p = provider::provider();
            p.kx_groups = vec![group];
            p.cipher_suites = vec![suite];
            let got = handshake(server.clone(), client_config_with(p, &ca, None))
                .await
                .unwrap();
            assert_eq!(got.group, group.name());
            assert_eq!(got.suite, suite.suite());
        }
    }
}

#[tokio::test]
async fn a_classical_client_still_connects() {
    // A client that offers X25519 only gets X25519, without a retry.
    let ca = Ca::new("ca");
    let mut p = provider::provider();
    p.kx_groups = vec![&X25519, &SECP256R1];
    let got = handshake(
        server_config(&ca.server(), None),
        client_config_with(p, &ca, None),
    )
    .await
    .unwrap();
    assert_eq!(got.group, NamedGroup::X25519);
}

#[tokio::test]
async fn every_key_type_signs_and_verifies() {
    // CAs and server keys of every supported kind: certificate signatures
    // (verified by the client) and handshake signatures (by the server's
    // key), including a P-384 key loaded from SEC1.
    for ca_key in [Key::p256(), Key::p384(), Key::ed25519()] {
        let ca = Ca::with_key("ca", ca_key);
        for key in [Key::p256(), Key::p384(), Key::ed25519()] {
            let server = ca.issue("server", Usage::Server, key.clone());
            let client = ca.issue("client", Usage::Client, key);
            let got = handshake(
                server_config(&server, Some(&ca)),
                client_config(&ca, Some(&client)),
            )
            .await
            .unwrap();
            assert_eq!(got.client_cert, Some(client.cert.to_vec()));
        }
    }
}

#[tokio::test]
async fn client_certificates_are_optional_and_verified() {
    let ca = Ca::new("ca");
    let server = server_config(&ca.server(), Some(&ca));
    // Without one: the handshake succeeds, with no client certificate.
    let got = handshake(server.clone(), client_config(&ca, None))
        .await
        .unwrap();
    assert_eq!(got.client_cert, None);
    // One from the CA is verified and visible.
    let alice = ca.client("alice");
    let got = handshake(server.clone(), client_config(&ca, Some(&alice)))
        .await
        .unwrap();
    assert_eq!(got.client_cert, Some(alice.cert.to_vec()));
    // One from another CA fails the handshake.
    let rogue = Ca::new("rogue").client("mallory");
    assert!(
        handshake(server.clone(), client_config(&ca, Some(&rogue)))
            .await
            .is_err()
    );
    // An expired one fails too.
    let old = ca.issue("old", Usage::Expired, Key::p256());
    assert!(
        handshake(server.clone(), client_config(&ca, Some(&old)))
            .await
            .is_err()
    );
    // A server certificate is not a client certificate (EKU).
    let wrong = ca.server();
    assert!(
        handshake(server, client_config(&ca, Some(&wrong)))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_client_checks_the_server_certificate() {
    let ca = Ca::new("ca");
    let other = Ca::new("other");
    assert!(
        handshake(
            server_config(&other.server(), None),
            client_config(&ca, None)
        )
        .await
        .is_err()
    );
}

// ---------------------------------------------------------------- listener

/// A server with `[tls]` (no client CA), and the harness talking HTTPS.
async fn https_harness(ca: &Ca) -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_config(dir.path(), &ca.server(), None);
    let mut h = Harness::start_with(|c| c.tls = Some(tls)).await;
    h.use_https(https_client(ca, None));
    (h, dir)
}

#[tokio::test]
async fn the_api_is_served_over_https() {
    let ca = Ca::new("ca");
    let (h, _dir) = https_harness(&ca).await;
    let info: Info = h.get("/v1/info").await;
    assert_eq!(info.api, 1);
    // Claim and sign in, as over HTTP; the origin is https://.
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    assert!(h.origin().starts_with("https://"));
    let tok = h.sign_in(&admin).await.unwrap();
    let _: ReadVersion = h.call("/v1/grv", Some(&tok), &Empty {}).await.unwrap();
    // Plain HTTP on the TLS port gets nowhere.
    let plain = reqwest::Client::builder().no_proxy().build().unwrap();
    let r = plain
        .get(format!("http://{}/v1/info", h.server.addr))
        .send()
        .await;
    assert!(r.is_err() || !r.unwrap().status().is_success());
}

#[tokio::test]
async fn bad_tls_settings_stop_the_start() {
    let ca = Ca::new("ca");
    let dir = tempfile::tempdir().unwrap();
    let good = tls_config(dir.path(), &ca.server(), None);
    let start = |tls: zen_server::config::TlsConfig| async {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = zen_server::config::Config::with_data_dir(d.path().join("data"));
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.tls = Some(tls);
        zen_server::start(cfg).await.err().unwrap_or_default()
    };
    // A missing file.
    let mut t = good.clone();
    t.cert = dir.path().join("missing.pem");
    assert!(start(t).await.contains("tls.cert"));
    // A key that isn't the certificate's.
    let other = dir.path().join("other.key");
    std::fs::write(&other, Key::p256().pem()).unwrap();
    let mut t = good.clone();
    t.key = other;
    assert!(start(t).await.contains("tls.cert / tls.key"));
    // A client CA file without certificates.
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let mut t = good.clone();
    t.client_ca = Some(empty);
    assert!(start(t).await.contains("tls.client_ca"));
}
