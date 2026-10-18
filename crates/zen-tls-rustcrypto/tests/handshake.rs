//! Handshakes between this provider and itself (rustls client and server,
//! in memory): every TLS 1.2 suite and group, and 1.3 still preferred by
//! `provider_tls12`.

use rustls::pki_types::ServerName;
use rustls::{
    ClientConfig, ClientConnection, ServerConfig, ServerConnection, SupportedCipherSuite,
};
use std::io::{Read, Write};
use std::sync::Arc;
use zen_tls_rustcrypto::testing::{KeyKind, server_cert};
use zen_tls_rustcrypto::{
    SECP256R1, TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, X25519, provider, provider_tls12,
};

fn server_config(kind: KeyKind) -> (ServerConfig, rustls::pki_types::CertificateDer<'static>) {
    let cert = server_cert(kind);
    let mut c = ServerConfig::builder_with_provider(Arc::new(provider_tls12()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert.chain, cert.key)
        .unwrap();
    c.require_ems = true;
    (c, cert.ca)
}

fn client_config(
    ca: rustls::pki_types::CertificateDer<'static>,
    suite: Option<SupportedCipherSuite>,
    group: Option<&'static dyn rustls::crypto::SupportedKxGroup>,
) -> ClientConfig {
    let mut p = provider_tls12();
    let mut versions = vec![&rustls::version::TLS13, &rustls::version::TLS12];
    if let Some(s) = suite {
        p.cipher_suites = vec![s];
        versions = vec![&rustls::version::TLS12];
    }
    if let Some(g) = group {
        p.kx_groups = vec![g];
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    ClientConfig::builder_with_provider(Arc::new(p))
        .with_protocol_versions(&versions)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn pump(c: &mut ClientConnection, s: &mut ServerConnection) {
    for _ in 0..20 {
        let mut buf = Vec::new();
        while c.wants_write() {
            c.write_tls(&mut buf).unwrap();
        }
        let mut r = &buf[..];
        while !r.is_empty() {
            s.read_tls(&mut r).unwrap();
            s.process_new_packets().unwrap();
        }
        let mut buf = Vec::new();
        while s.wants_write() {
            s.write_tls(&mut buf).unwrap();
        }
        let mut r = &buf[..];
        while !r.is_empty() {
            c.read_tls(&mut r).unwrap();
            c.process_new_packets().unwrap();
        }
        if !c.is_handshaking() && !s.is_handshaking() && !c.wants_write() && !s.wants_write() {
            return;
        }
    }
}

fn connect(server: &ServerConfig, client: ClientConfig) -> (ClientConnection, ServerConnection) {
    let mut c = ClientConnection::new(Arc::new(client), ServerName::try_from("localhost").unwrap())
        .unwrap();
    let mut s = ServerConnection::new(Arc::new(server.clone())).unwrap();
    pump(&mut c, &mut s);
    assert!(!c.is_handshaking() && !s.is_handshaking());
    (c, s)
}

fn exchange(c: &mut ClientConnection, s: &mut ServerConnection) {
    let big: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
    let mut got = Vec::new();
    for chunk in big.chunks(8192) {
        c.writer().write_all(chunk).unwrap();
        pump(c, s);
        let mut buf = [0; 16384];
        while let Ok(n) = s.reader().read(&mut buf) {
            got.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(got, big);
    s.writer().write_all(b"pong").unwrap();
    pump(c, s);
    let mut got = [0; 4];
    c.reader().read_exact(&mut got).unwrap();
    assert_eq!(&got, b"pong");
}

#[test]
fn tls12_every_suite_and_group() {
    for kind in [KeyKind::P256, KeyKind::P384, KeyKind::Ed25519] {
        let (server, ca) = server_config(kind);
        for suite in [
            TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        ] {
            for group in [
                &X25519 as &'static dyn rustls::crypto::SupportedKxGroup,
                &SECP256R1,
            ] {
                let (mut c, mut s) =
                    connect(&server, client_config(ca.clone(), Some(suite), Some(group)));
                assert_eq!(s.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_2));
                assert_eq!(s.negotiated_cipher_suite(), Some(suite));
                assert_eq!(
                    s.negotiated_key_exchange_group().map(|g| g.name()),
                    Some(group.name())
                );
                exchange(&mut c, &mut s);
            }
        }
    }
}

#[test]
fn tls13_is_preferred() {
    let (server, ca) = server_config(KeyKind::P256);
    let (mut c, mut s) = connect(&server, client_config(ca, None, None));
    assert_eq!(s.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
    assert_eq!(
        s.negotiated_key_exchange_group().map(|g| g.name()),
        Some(rustls::NamedGroup::X25519MLKEM768)
    );
    exchange(&mut c, &mut s);
}

/// `provider()` still offers no TLS 1.2 suite.
#[test]
fn provider_is_tls13_only() {
    assert!(
        provider()
            .cipher_suites
            .iter()
            .all(|s| s.version() == &rustls::version::TLS13)
    );
    assert_eq!(provider_tls12().cipher_suites.len(), 6);
}
