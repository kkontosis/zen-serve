//! Native TLS: the build's rustls provider (`tls::provider`: ring's, or
//! the pure-Rust one without the `ring` feature) in real handshakes, the
//! two providers against each other, and the HTTPS listener
//! (operations.md §8).

mod common;

use common::pki::*;
use common::*;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::server::WebPkiClientVerifier;
use rustls::{CipherSuite, NamedGroup, RootCertStore, ServerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zen_proto::*;
use zen_server::tls;

/// What a handshake negotiated.
#[derive(Debug)]
struct Negotiated {
    group: NamedGroup,
    suite: CipherSuite,
    /// The client certificate the server verified.
    client_cert: Option<Vec<u8>>,
}

/// A server configuration on the build's provider, with `ident`, asking
/// for optional client certificates from `client_ca`.
fn server_config(ident: &Ident, client_ca: Option<&Ca>) -> Arc<ServerConfig> {
    server_config_with(tls::provider(), ident, client_ca)
}

/// [`server_config`] on another provider.
fn server_config_with(
    p: CryptoProvider,
    ident: &Ident,
    client_ca: Option<&Ca>,
) -> Arc<ServerConfig> {
    let p = Arc::new(p);
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
    for group in tls::provider().kx_groups {
        for suite in tls::provider().cipher_suites {
            let mut p = tls::provider();
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
    let mut p = tls::provider();
    p.kx_groups
        .retain(|g| g.name() != NamedGroup::X25519MLKEM768);
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
async fn rsa_chains_and_client_keys_verify() {
    // An RSA CA: the client verifies its signature on the (EC) server
    // certificate, the server its signature on client certificates.
    let ca = Ca::with_key("rsa ca", Key::rsa(1, 2048));
    let server = server_config(&ca.server(), Some(&ca));
    // An RSA client key: certificate and RSA-PSS handshake signature.
    let rsa_client = ca.issue("rsa client", Usage::Client, Key::rsa(2, 2048));
    let got = handshake(server.clone(), client_config(&ca, Some(&rsa_client)))
        .await
        .unwrap();
    assert_eq!(got.client_cert, Some(rsa_client.cert.to_vec()));
    // An EC client key under the RSA CA.
    let ec_client = ca.client("ec client");
    let got = handshake(server.clone(), client_config(&ca, Some(&ec_client)))
        .await
        .unwrap();
    assert_eq!(got.client_cert, Some(ec_client.cert.to_vec()));
    // PKCS#1 v1.5 with SHA-512, and a 3072-bit RSA key under an EC CA.
    let ca512 = Ca::with_key("rsa ca sha512", Key::rsa_sha512(3, 3072));
    let ec_ca = Ca::new("ec ca");
    for (ca, client) in [
        (&ca512, ca512.client("under sha512")),
        (
            &ec_ca,
            ec_ca.issue("rsa 3072", Usage::Client, Key::rsa(4, 3072)),
        ),
    ] {
        let got = handshake(
            server_config(&ca.server(), Some(ca)),
            client_config(ca, Some(&client)),
        )
        .await
        .unwrap();
        assert_eq!(got.client_cert, Some(client.cert.to_vec()));
    }
}

#[tokio::test]
async fn small_rsa_keys_are_refused() {
    let ca = Ca::new("ca");
    let small_ca = Ca::with_key("small ca", Key::rsa(5, 1024));
    // The server refuses a client chain signed by a 1024-bit CA (the
    // client trusts the server's certificate here).
    let client = small_ca.client("client");
    let server = server_config(&ca.server(), Some(&small_ca));
    let r = handshake(server.clone(), client_config(&ca, Some(&client))).await;
    let e = r.unwrap_err();
    assert!(
        e.contains("BadSignature") && e.ends_with("client: None"),
        "{e}"
    );
    // ... and a 1024-bit client key under a good CA.
    let small = ca.issue("small", Usage::Client, Key::rsa(6, 1024));
    let server = server_config(&ca.server(), Some(&ca));
    let e = handshake(server, client_config(&ca, Some(&small)))
        .await
        .unwrap_err();
    assert!(
        e.contains("BadSignature") && e.ends_with("client: None"),
        "{e}"
    );
    // The client refuses a server certificate from a 1024-bit CA.
    let r = handshake(
        server_config(&small_ca.server(), None),
        client_config(&small_ca, None),
    )
    .await;
    let e = r.unwrap_err();
    assert!(
        e.contains(r#"client: Some("invalid peer certificate: BadSignature")"#),
        "{e}"
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

// ---------------------------------------------------------------- ring

/// The ring provider and the pure-Rust one, each the other's peer.
#[cfg(feature = "ring")]
fn both_ways() -> [(&'static str, CryptoProvider, CryptoProvider); 2] {
    [
        (
            "ring server",
            tls::ring::provider(),
            tls::rustcrypto::provider(),
        ),
        (
            "ring client",
            tls::rustcrypto::provider(),
            tls::ring::provider(),
        ),
    ]
}

#[cfg(feature = "ring")]
#[tokio::test]
async fn the_hybrid_is_negotiated_with_ring() {
    // Our X25519MLKEM768 group, first among ring's own.
    let ca = Ca::new("ca");
    let p = tls::ring::provider();
    assert_eq!(p.kx_groups[0].name(), NamedGroup::X25519MLKEM768);
    let got = handshake(
        server_config_with(tls::ring::provider(), &ca.server(), None),
        client_config_with(p, &ca, None),
    )
    .await
    .unwrap();
    assert_eq!(got.group, NamedGroup::X25519MLKEM768);
    // ring's P-384, which the pure-Rust provider doesn't have.
    let mut p = tls::ring::provider();
    p.kx_groups.retain(|g| g.name() == NamedGroup::secp384r1);
    let got = handshake(
        server_config(&ca.server(), None),
        client_config_with(p, &ca, None),
    )
    .await
    .unwrap();
    assert_eq!(got.group, NamedGroup::secp384r1);
}

#[cfg(feature = "ring")]
#[tokio::test]
async fn the_two_providers_interoperate() {
    // Independent implementations on each side, except the hybrid: every
    // group both have, every suite, and every key type.
    let ca = Ca::new("ca");
    for (what, server_p, client_p) in both_ways() {
        let server = server_config_with(server_p, &ca.server(), None);
        for group in tls::rustcrypto::provider().kx_groups {
            for suite in tls::rustcrypto::provider().cipher_suites {
                let mut p = client_p.clone();
                p.kx_groups.retain(|g| g.name() == group.name());
                p.cipher_suites.retain(|s| s.suite() == suite.suite());
                let got = handshake(server.clone(), client_config_with(p, &ca, None))
                    .await
                    .unwrap_or_else(|e| panic!("{what}, {:?}: {e}", group.name()));
                assert_eq!(got.group, group.name(), "{what}");
                assert_eq!(got.suite, suite.suite(), "{what}");
            }
        }
    }
    for ca_key in [Key::p256(), Key::p384(), Key::ed25519(), Key::rsa(1, 2048)] {
        let ca = Ca::with_key("ca", ca_key);
        for key in [Key::p256(), Key::p384(), Key::ed25519()] {
            let server = ca.issue("server", Usage::Server, key.clone());
            for client in [
                ca.issue("client", Usage::Client, key.clone()),
                ca.issue("rsa client", Usage::Client, Key::rsa(2, 2048)),
            ] {
                for (what, server_p, client_p) in both_ways() {
                    let got = handshake(
                        server_config_with(server_p, &server, Some(&ca)),
                        client_config_with(client_p, &ca, Some(&client)),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert_eq!(got.client_cert, Some(client.cert.to_vec()), "{what}");
                }
            }
        }
    }
}

#[cfg(feature = "ring")]
#[tokio::test]
async fn rsa_server_keys_sign_with_ring() {
    // RSA server keys of 2048 and 3072 bits, PKCS#8 and PKCS#1, under an
    // RSA and an EC CA. ring signs (RSA-PSS); both providers verify.
    let rsa_ca = Ca::with_key("rsa ca", Key::rsa(1, 2048));
    let ec_ca = Ca::new("ec ca");
    for (ca, key) in [(&rsa_ca, Key::rsa(3, 2048)), (&ec_ca, Key::rsa(4, 3072))] {
        let server = ca.issue("rsa server", Usage::Server, key);
        let Key::Rsa(k, ..) = &server.key else {
            unreachable!()
        };
        let pkcs1 = rustls::pki_types::PrivateKeyDer::Pkcs1(
            zen_server::rsakey::testing::private_der(k).into(),
        );
        for key_der in [server.key.der(), pkcs1] {
            let cfg = ServerConfig::builder_with_provider(Arc::new(tls::provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![server.cert.clone()], key_der)
                .unwrap();
            let cfg = Arc::new(cfg);
            for client_p in [tls::ring::provider(), tls::rustcrypto::provider()] {
                let got = handshake(cfg.clone(), client_config_with(client_p, ca, None))
                    .await
                    .unwrap();
                assert_eq!(got.group, NamedGroup::X25519MLKEM768);
            }
        }
        // With client certificates, as for mTLS sign-in.
        let alice = ca.client("alice");
        let got = handshake(
            server_config(&server, Some(ca)),
            client_config(ca, Some(&alice)),
        )
        .await
        .unwrap();
        assert_eq!(got.client_cert, Some(alice.cert.to_vec()));
    }
}

// ---------------------------------------------------------------- listener

/// A server with `[tls]` (no client CA), and the harness talking HTTPS.
async fn https_harness(ca: &Ca) -> (Harness, tempfile::TempDir) {
    https_harness_with(ca, &ca.server()).await
}

/// [`https_harness`] with the server's certificate and key `server`.
async fn https_harness_with(ca: &Ca, server: &Ident) -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_config(dir.path(), server, None);
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

#[cfg(feature = "ring")]
#[tokio::test]
async fn the_api_is_served_with_an_rsa_server_key() {
    let ca = Ca::with_key("rsa ca", Key::rsa(1, 2048));
    let server = ca.issue("zen-serve test", Usage::Server, Key::rsa(3, 2048));
    let (h, _dir) = https_harness_with(&ca, &server).await;
    let info: Info = h.get("/v1/info").await;
    assert_eq!(info.api, 1);
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
        match zen_server::start(cfg).await {
            Ok(server) => {
                server.abort();
                String::new()
            }
            Err(e) => e,
        }
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
    // An RSA server key (PKCS#8 and PKCS#1): without the ring feature,
    // refused, naming the reason; with it, started.
    let rsa_server = ca.issue("rsa server", Usage::Server, Key::rsa(7, 2048));
    let rsa = tls_config(dir.path(), &rsa_server, None);
    let pkcs1 = dir.path().join("rsa1.key");
    let Key::Rsa(k, ..) = &rsa_server.key else {
        unreachable!()
    };
    std::fs::write(
        &pkcs1,
        pem(
            "RSA PRIVATE KEY",
            &zen_server::rsakey::testing::private_der(k),
        ),
    )
    .unwrap();
    let mut rsa1 = rsa.clone();
    rsa1.key = pkcs1;
    for t in [rsa, rsa1] {
        let e = start(t).await;
        if cfg!(feature = "ring") {
            assert_eq!(e, "");
        } else {
            assert!(e.contains("TD-TLS-RSA-SERVER-KEY"), "{e}");
            assert!(e.contains("built without the ring feature"), "{e}");
        }
    }
    // A 1024-bit RSA server key: refused either way.
    let small = ca.issue("small server", Usage::Server, Key::rsa(8, 1024));
    let e = start(tls_config(dir.path(), &small, None)).await;
    assert!(e.contains("tls.cert / tls.key"), "{e}");
    if cfg!(feature = "ring") {
        assert!(e.contains("2048 to 4096 bits"), "{e}");
    }
    // A client CA file without certificates.
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let mut t = good.clone();
    t.client_ca = Some(empty);
    assert!(start(t).await.contains("tls.client_ca"));
}
