//! The pure-Rust rustls provider (`tls::provider`) in real handshakes.

mod common;

use common::pki::*;
use rustls::pki_types::ServerName;
use rustls::server::WebPkiClientVerifier;
use rustls::{CipherSuite, NamedGroup, RootCertStore, ServerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
