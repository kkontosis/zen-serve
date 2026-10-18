//! The TLS engine against rustls clients (svc.md §7.1). The harness
//! restores the engine from its state after every input call, so each
//! record boundary is a takeover point.

mod common;

use ciborium::Value;
use common::*;
use rustls::{AlertDescription, HandshakeKind, ProtocolVersion};
use std::io::Write;
use std::sync::Arc;
use zen_svc::tls::{Config, Conn, Error, Options, PLAIN_HELD, Session};
use zen_tls_rustcrypto::record::{self, Hash, RecordKey};
use zen_tls_rustcrypto::testing::KeyKind;

type Map = Vec<(Value, Value)>;

fn opts() -> Options {
    Options::default()
}

fn data(n: usize, seed: u64) -> Vec<u8> {
    Rng(seed | 1).bytes(n)
}

/// A field of a CBOR map (`a.b` reaches into a nested map).
fn field(state: &[u8], path: &str) -> Option<Value> {
    let mut v: Value = ciborium::from_reader(state).unwrap();
    for k in path.split('.') {
        let Value::Map(m) = v else { return None };
        v = m
            .into_iter()
            .find(|(x, _)| x.as_text() == Some(k))
            .map(|(_, v)| v)?;
    }
    Some(v)
}

fn bytes(state: &[u8], path: &str) -> Vec<u8> {
    field(state, path)
        .and_then(|v| v.into_bytes().ok())
        .unwrap_or_else(|| panic!("no {path}"))
}

fn upd(hash: Hash, secret: &[u8]) -> Vec<u8> {
    record::expand_label(hash, secret, b"traffic upd", b"", hash.output_len())
}

fn hash_of(s: &Session) -> Hash {
    match u16::from(s.suite()) {
        0x1302 | 0xc02c => Hash::Sha384,
        _ => Hash::Sha256,
    }
}

#[test]
fn tls13_every_suite_and_group() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in SUITES13 {
        for group in [
            &X25519MLKEM768 as &'static dyn rustls::crypto::SupportedKxGroup,
            &X25519,
            &SECP256R1,
        ] {
            let (c, secrets) = client(&cert.ca, Some(suite), Some(group));
            let mut p = Pair::new(c, config.clone(), Feed::Records);
            p.handshake();
            let s = p.server.conn.session().unwrap();
            assert_eq!(s.version(), ProtocolVersion::TLSv1_3);
            assert_eq!(s.suite(), suite.suite());
            assert_eq!(s.sni(), Some("localhost"));
            // The secrets the engine carries are the handshake's.
            let st = p.server.conn.state().unwrap();
            assert_eq!(
                bytes(&st, "tx.secret"),
                secrets.get("SERVER_TRAFFIC_SECRET_0")
            );
            assert_eq!(
                bytes(&st, "rx.secret"),
                secrets.get("CLIENT_TRAFFIC_SECRET_0")
            );
            p.echo(&data(50_000, 1));
        }
    }
}

#[test]
fn tls12_every_suite_and_group() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in SUITES12 {
        for group in [
            &X25519 as &'static dyn rustls::crypto::SupportedKxGroup,
            &SECP256R1,
        ] {
            let (c, _) = client(&cert.ca, Some(suite), Some(group));
            let mut p = Pair::new(c, config.clone(), Feed::Records);
            p.handshake();
            let s = p.server.conn.session().unwrap();
            assert_eq!(s.version(), ProtocolVersion::TLSv1_2);
            assert_eq!(s.suite(), suite.suite());
            p.echo(&data(50_000, 2));
        }
    }
}

#[test]
fn every_key_kind() {
    for kind in [KeyKind::P256, KeyKind::P384, KeyKind::Ed25519] {
        let (config, cert) = server(kind, opts());
        for suite in [
            TLS13_AES_128_GCM_SHA256,
            TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ] {
            let (c, _) = client(&cert.ca, Some(suite), None);
            let mut p = Pair::new(c, config.clone(), Feed::Whole);
            p.handshake();
            p.echo(b"hello");
        }
    }
}

/// A megabyte each way, restoring at every record boundary.
#[test]
fn a_megabyte_both_ways() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in [
        TLS13_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ] {
        let (c, _) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Records);
        p.handshake();
        let big = data(1 << 20, 3);
        p.echo(&big);
        assert!(p.server.restores > 64);
    }
}

/// Whole flights, a record at a time, a byte at a time.
#[test]
fn every_feed() {
    let (config, cert) = server(KeyKind::P256, opts());
    for feed in [Feed::Whole, Feed::Records, Feed::Bytes] {
        for suite in [
            TLS13_CHACHA20_POLY1305_SHA256,
            TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ] {
            let (c, _) = client(&cert.ca, Some(suite), None);
            let mut p = Pair::new(c, config.clone(), feed);
            p.handshake();
            p.echo(&data(3_000, 4));
        }
    }
}

/// Application data in the client's last handshake flight reaches the
/// application (decrypted by rustls before the switch, or by the engine
/// after it).
#[test]
fn data_with_the_last_flight() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in [
        TLS13_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        let (c, _) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Whole);
        p.server.echo = false;
        // Client flights until the client is done; then it writes data and
        // its Finished and the data go in one input.
        for _ in 0..10 {
            if !p.client.is_handshaking() {
                break;
            }
            let c = p.client_out();
            let s = p.server.feed(&c, false);
            p.client_in(&s).unwrap();
        }
        p.client.writer().write_all(b"early bird").unwrap();
        let c = p.client_out();
        p.server.feed(&c, false);
        assert!(p.server.conn.is_established());
        assert_eq!(p.server.received, b"early bird");
    }
}

#[test]
fn no_tickets_no_resumption_no_early_data() {
    let (config, cert) = server(KeyKind::P256, opts());
    let (mut c, _) = client(&cert.ca, None, None);
    c.enable_early_data = true;
    let c = Arc::new(c);
    for _ in 0..2 {
        let mut p = Pair::new((*c).clone(), config.clone(), Feed::Whole);
        p.handshake();
        p.echo(b"x");
        assert_eq!(p.client.handshake_kind(), Some(HandshakeKind::Full));
        assert!(!p.client.is_early_data_accepted());
    }
}

#[test]
fn alpn_and_sni() {
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            alpn: vec![b"http/1.1".to_vec()],
            ..opts()
        },
    );
    for suite in [
        TLS13_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ] {
        let (mut c, _) = client(&cert.ca, Some(suite), None);
        c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut p = Pair::new(c, config.clone(), Feed::Whole);
        p.handshake();
        p.server.takeover();
        let s = p.server.conn.session().unwrap();
        assert_eq!(s.alpn(), Some(&b"http/1.1"[..]));
        assert_eq!(s.sni(), Some("localhost"));
        assert_eq!(p.client.alpn_protocol(), Some(&b"http/1.1"[..]));
    }
}

#[test]
fn versions_can_be_limited() {
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            tls13: false,
            ..opts()
        },
    );
    let (c, _) = client(&cert.ca, None, None);
    let mut p = Pair::new(c, config, Feed::Whole);
    p.handshake();
    assert_eq!(
        p.server.conn.session().unwrap().version(),
        ProtocolVersion::TLSv1_2
    );
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            tls12: false,
            ..opts()
        },
    );
    let (c, _) = client(
        &cert.ca,
        Some(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
        None,
    );
    let mut p = Pair::new(c, config, Feed::Whole);
    assert!(p.pump().is_err());
    assert!(!p.server.conn.is_established());
    assert!(matches!(p.server.errors[..], [Error::Tls(_)]));
}

/// The client asks for a KeyUpdate (rustls requests the server's too).
#[test]
fn key_update_from_the_client() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in SUITES13 {
        let (c, secrets) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Records);
        p.handshake();
        p.echo(b"before");
        p.client.refresh_traffic_keys().unwrap();
        p.echo(b"after");
        p.client.refresh_traffic_keys().unwrap();
        p.echo(b"again");
        let s = p.server.conn.session().unwrap();
        let h = hash_of(s);
        let st = s.state();
        let c0 = secrets.get("CLIENT_TRAFFIC_SECRET_0");
        let s0 = secrets.get("SERVER_TRAFFIC_SECRET_0");
        assert_eq!(bytes(&st, "rx.secret"), upd(h, &upd(h, &c0)));
        assert_eq!(bytes(&st, "tx.secret"), upd(h, &upd(h, &s0)));
    }
}

/// The engine sends KeyUpdates, with and without asking for the client's.
#[test]
fn key_update_from_the_server() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in SUITES13 {
        let (c, secrets) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Records);
        p.handshake();
        let out = p.server.conn.key_update(false).unwrap();
        p.server.takeover();
        p.client_in(&out).unwrap();
        p.echo(b"one");
        let out = p.server.conn.key_update(true).unwrap();
        p.server.takeover();
        p.client_in(&out).unwrap();
        p.echo(b"two");
        let s = p.server.conn.session().unwrap();
        let h = hash_of(s);
        let st = s.state();
        let c0 = secrets.get("CLIENT_TRAFFIC_SECRET_0");
        let s0 = secrets.get("SERVER_TRAFFIC_SECRET_0");
        assert_eq!(bytes(&st, "tx.secret"), upd(h, &upd(h, &s0)));
        assert_eq!(bytes(&st, "rx.secret"), upd(h, &c0));
    }
}

/// At the key limit (lowered for the test), 1.3 updates its key itself.
#[test]
fn tls13_updates_its_key_at_the_limit() {
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            key_limit: Some(4),
            ..opts()
        },
    );
    let (c, secrets) = client(&cert.ca, Some(TLS13_AES_128_GCM_SHA256), None);
    let mut p = Pair::new(c, config, Feed::Records);
    p.handshake();
    for i in 0..30u8 {
        p.echo(&[i; 100]);
    }
    let s = p.server.conn.session().unwrap();
    // Each key carries 3 data records and the KeyUpdate: 30 data records
    // take 9 updates, and the last key has carried 3.
    assert_eq!(s.seqs().0, 3);
    let mut want = secrets.get("SERVER_TRAFFIC_SECRET_0");
    for _ in 0..9 {
        want = upd(Hash::Sha256, &want);
    }
    assert_eq!(bytes(&s.state(), "tx.secret"), want);
}

/// At the key limit 1.2 refuses writes; close_notify still goes out.
#[test]
fn tls12_closes_at_the_limit() {
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            key_limit: Some(6),
            ..opts()
        },
    );
    let (c, _) = client(
        &cert.ca,
        Some(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
        None,
    );
    let mut p = Pair::new(c, config, Feed::Records);
    p.handshake();
    p.server.echo = false;
    let mut sent = 0;
    loop {
        match p.server.conn.write(b"tick") {
            Ok(out) => {
                p.client_in(&out).unwrap();
                sent += 1;
            }
            Err(e) => {
                assert_eq!(e, Error::KeyLimit);
                break;
            }
        }
        p.server.takeover();
    }
    // The handshake's Finished took sequence number 0.
    assert_eq!(sent, 4);
    assert_eq!(p.received, b"tick".repeat(4));
    let out = p.server.conn.close();
    p.client_in(&out).unwrap();
    assert_eq!(p.server.conn.write(b"x"), Err(Error::Closed));
    let mut buf = [0u8; 4];
    assert_eq!(
        std::io::Read::read(&mut p.client.reader(), &mut buf).unwrap(),
        0
    );
}

#[test]
fn tls12_has_no_key_update() {
    let (config, cert) = server(KeyKind::P256, opts());
    let (c, _) = client(
        &cert.ca,
        Some(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
        None,
    );
    let mut p = Pair::new(c, config, Feed::Whole);
    p.handshake();
    assert!(matches!(
        p.server.conn.key_update(true),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn close_notify_both_ways() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in [
        TLS13_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ] {
        let (c, _) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Records);
        p.handshake();
        p.echo(b"bye");
        p.server.echo = false;
        p.client.writer().write_all(b"last").unwrap();
        p.client.send_close_notify();
        let c = p.client_out();
        // Bytes after close_notify are ignored.
        let mut junk = c.clone();
        junk.extend_from_slice(&[23, 3, 3, 0, 1, 0]);
        p.server.feed(&junk, true);
        assert!(p.server.peer_closed);
        assert!(!p.server.truncated);
        assert!(p.server.pending.is_empty());
        assert_eq!(&p.server.received[3..], b"last");
        // The engine may still write, then closes.
        let mut out = p.server.conn.write(b"reply").unwrap();
        out.extend(p.server.conn.close());
        p.client_in(&out).unwrap();
        assert!(p.received.ends_with(b"reply"));
        assert!(p.server.conn.close().is_empty());
        let st = p.server.conn.state().unwrap();
        assert_eq!(field(&st, "flags"), Some(Value::Integer(3.into())));
    }
}

#[test]
fn truncation() {
    let (config, cert) = server(KeyKind::P256, opts());
    let (c, _) = client(&cert.ca, None, None);
    let mut p = Pair::new(c, config.clone(), Feed::Whole);
    p.handshake();
    p.client.writer().write_all(b"cut").unwrap();
    let c = p.client_out();
    p.server.echo = false;
    p.server.feed(&c[..c.len() - 1], true);
    assert!(p.server.truncated);
    assert!(p.server.received.is_empty());
    assert_eq!(p.server.pending.len(), c.len() - 1);

    // EOF mid-handshake fails the handshake.
    let (c, _) = client(&cert.ca, None, None);
    let mut p = Pair::new(c, config, Feed::Whole);
    let hello = p.client_out();
    p.server.feed(&hello[..10], true);
    assert!(p.server.truncated);
    assert!(!p.server.conn.is_established());
    assert!(p.server.conn.state().is_none());
}

#[test]
fn garbage_instead_of_a_handshake() {
    let (config, _) = server(KeyKind::P256, opts());
    for junk in [
        &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..],
        &[22, 3, 1, 0xff, 0xff, 0][..],
        &[22, 3, 1, 0, 4, 1, 0, 0, 0][..],
    ] {
        let mut s = Server::new(config.clone());
        s.feed(junk, false);
        assert!(!s.conn.is_established());
        assert!(matches!(s.errors[..], [Error::Tls(_)]), "{:?}", s.errors);
        assert!(s.conn.write(b"x").is_err());
        // Further input is ignored.
        s.feed(b"more", true);
    }
}

#[test]
fn rsa_and_mismatched_keys_are_refused() {
    let cert = zen_tls_rustcrypto::testing::server_cert(KeyKind::P256);
    let rsa = zen_tls_rustcrypto::rsakey::testing::rsa_key(1, 2048);
    let der = zen_tls_rustcrypto::rsakey::testing::pkcs8_der(&rsa);
    let e = Config::new(
        cert.chain.clone(),
        rustls::pki_types::PrivateKeyDer::Pkcs8(der.into()),
        opts(),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Config(_)), "{e:?}");
    let other = zen_tls_rustcrypto::testing::server_cert(KeyKind::P256);
    let e = Config::new(cert.chain, other.key, opts()).unwrap_err();
    assert!(matches!(e, Error::Config(_)), "{e:?}");
    let e = Config::new(
        Vec::new(),
        other_key(),
        Options {
            tls12: false,
            tls13: false,
            ..opts()
        },
    )
    .unwrap_err();
    assert!(matches!(e, Error::Config(_)));
}

fn other_key() -> rustls::pki_types::PrivateKeyDer<'static> {
    zen_tls_rustcrypto::testing::server_cert(KeyKind::P256).key
}

/// The engine keeps what the application hasn't consumed, up to 16 KiB
/// plus a record, and stops reading (acking) beyond.
#[test]
fn plaintext_is_held_and_bounded() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in [
        TLS13_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ] {
        let (c, _) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Whole);
        p.handshake();
        p.server.consume = false;
        p.client.set_buffer_limit(None);
        let big = data(100_000, 5);
        p.client.writer().write_all(&big).unwrap();
        let c = p.client_out();
        p.server.feed(&c, false);
        let held = p.server.conn.plaintext().len();
        assert!((PLAIN_HELD..PLAIN_HELD + 16_384).contains(&held), "{held}");
        assert!(p.server.pending.len() > 50_000);
        let st = p.server.conn.state().unwrap();
        assert!(st.len() < PLAIN_HELD + 16_384 + 512, "{}", st.len());
        // Consuming lets the rest in, in order.
        let mut got = Vec::new();
        while got.len() < big.len() {
            let plain = p.server.conn.plaintext().to_vec();
            p.server.conn.consume(plain.len());
            got.extend(plain);
            p.server.feed(&[], false);
        }
        assert_eq!(got, big);
        assert!(p.server.pending.is_empty());
    }
}

#[test]
fn state_round_trips_and_refuses_the_unexpected() {
    let (config, cert) = server(
        KeyKind::P256,
        Options {
            alpn: vec![b"x".to_vec()],
            ..opts()
        },
    );
    for suite in [
        TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ] {
        let (mut c, _) = client(&cert.ca, Some(suite), None);
        c.alpn_protocols = vec![b"x".to_vec()];
        let mut p = Pair::new(c, config.clone(), Feed::Whole);
        p.handshake();
        p.server.consume = false;
        p.client.writer().write_all(b"held").unwrap();
        let c = p.client_out();
        p.server.feed(&c, false);
        let st = p.server.conn.state().unwrap();
        let again = Conn::restore(&config, &st).unwrap();
        assert_eq!(again.state().unwrap(), st);
        assert_eq!(again.plaintext(), b"held");
        assert!(again.is_established());

        let v: Value = ciborium::from_reader(&st[..]).unwrap();
        let edit = |f: &dyn Fn(&mut Map)| -> Vec<u8> {
            let Value::Map(mut m) = v.clone() else {
                unreachable!()
            };
            f(&mut m);
            let mut out = Vec::new();
            ciborium::into_writer(&Value::Map(m), &mut out).unwrap();
            out
        };
        let set = |k: &'static str, val: Value| {
            edit(&move |m| {
                for (x, y) in m.iter_mut() {
                    if x.as_text() == Some(k) {
                        *y = val.clone();
                    }
                }
            })
        };
        let bad = [
            set("v", Value::Integer(2.into())),
            set("ver", Value::Integer(0x0302.into())),
            set(
                "ver",
                Value::Integer(if suite == SUITES12[0] { 0x0304 } else { 0x0303 }.into()),
            ),
            set("suite", Value::Integer(0x1304.into())),
            set("flags", Value::Integer(8.into())),
            set("plain", Value::Bytes(vec![0; PLAIN_HELD + 16_385])),
            set("plain", Value::Text("x".into())),
            set("alpn", Value::Bytes(Vec::new())),
            set("tx", Value::Map(Vec::new())),
            set("rx", Value::Integer(1.into())),
            edit(&|m| m.retain(|(k, _)| k.as_text() != Some("flags"))),
            edit(&|m| m.push((Value::Text("extra".into()), Value::Null))),
            edit(&|m| m.push((Value::Text("v".into()), Value::Integer(1.into())))),
            edit(&|m| m.push((Value::Integer(1.into()), Value::Null))),
            st[..st.len() - 1].to_vec(),
            b"\xa0".to_vec(),
            Vec::new(),
        ];
        for (i, b) in bad.iter().enumerate() {
            assert!(
                matches!(Conn::restore(&config, b), Err(Error::BadState(_))),
                "case {i}"
            );
        }
        // A wrong key or secret length.
        let dir_edit = |name: &'static str, len: usize| {
            edit(&move |m| {
                for (x, y) in m.iter_mut() {
                    if x.as_text() == Some("tx") {
                        let Value::Map(d) = y else { unreachable!() };
                        for (a, b) in d.iter_mut() {
                            if a.as_text() == Some(name) {
                                *b = Value::Bytes(vec![1; len]);
                            }
                        }
                    }
                }
            })
        };
        let wrong = if suite == SUITES12[0] {
            vec![dir_edit("key", 32), dir_edit("iv", 8)]
        } else {
            vec![dir_edit("secret", 48), dir_edit("secret", 31)]
        };
        for b in wrong {
            assert!(matches!(
                Conn::restore(&config, &b),
                Err(Error::BadState(_))
            ));
        }
    }
}

/// A step can run on a copy of the session; the original is unchanged.
#[test]
fn a_session_copy_is_independent() {
    let (config, cert) = server(KeyKind::P256, opts());
    let (c, _) = client(&cert.ca, None, None);
    let mut p = Pair::new(c, config, Feed::Whole);
    p.handshake();
    let s = p.server.conn.session().unwrap().clone();
    let st = s.state();
    let mut copy = s.clone();
    let out = copy.write(b"tentative").unwrap();
    assert_ne!(copy.state(), st);
    assert_eq!(s.state(), st);
    // The original sends the same sequence number again; the client takes it.
    let mut orig = s;
    let out2 = orig.write(b"committed").unwrap();
    assert_eq!(out.len(), out2.len());
    p.client_in(&out2).unwrap();
    assert_eq!(p.received, b"committed");
}

/// Malformed records end in a specific alert or wait for more input, never
/// a panic; afterwards the state still restores.
#[test]
fn malformed_records() {
    let (config, cert) = server(KeyKind::P256, opts());
    for suite in [
        TLS13_AES_128_GCM_SHA256,
        TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        let tls13 = SUITES13.iter().any(|s| s.suite() == suite.suite());
        let (c, secrets) = client(&cert.ca, Some(suite), None);
        let mut p = Pair::new(c, config.clone(), Feed::Whole);
        p.handshake();
        let base = p.server.conn.state().unwrap();
        let mut transcript = Vec::new();
        for i in 0..4u8 {
            p.client
                .writer()
                .write_all(&vec![i; 300 * usize::from(i + 1)])
                .unwrap();
            transcript.extend(p.client_out());
        }
        let recs: Vec<Vec<u8>> = records(&transcript).iter().map(|r| r.to_vec()).collect();
        assert_eq!(recs.len(), 4);
        let run = |input: &[u8], eof: bool| {
            let mut conn = Conn::restore(&config, &base).unwrap();
            let pr = conn.input(input, eof);
            assert!(pr.consumed <= input.len());
            // The state always restores, whatever happened.
            let st = conn.state().unwrap();
            Conn::restore(&config, &st).unwrap();
            (pr, conn)
        };
        let fatal = |input: &[u8]| -> AlertDescription {
            let (pr, conn) = run(input, false);
            assert!(conn.session().unwrap().is_closed());
            assert!(!pr.out.is_empty(), "an alert is sent");
            match pr.error {
                Some(Error::AlertSent(a)) => a,
                e => panic!("{e:?}"),
            }
        };

        // Bit flips anywhere in a record's body: bad_record_mac.
        let mut rng = Rng(0x5eed);
        for _ in 0..200 {
            let mut r = recs[rng.below(4)].clone();
            let i = 5 + rng.below(r.len() - 5);
            r[i] ^= 1 << rng.below(8);
            let at = recs.iter().position(|x| x.len() == r.len()).unwrap();
            let mut input: Vec<u8> = recs[..at].concat();
            input.extend(&r);
            assert_eq!(fatal(&input), AlertDescription::BadRecordMac);
        }
        // A changed header type or version: the AAD differs, or the type is
        // refused.
        for (i, x) in [(0, 22u8), (0, 21), (0, 99), (1, 4), (2, 1)] {
            let mut r = recs[0].clone();
            r[i] = x;
            let a = fatal(&r);
            assert!(
                matches!(
                    a,
                    AlertDescription::BadRecordMac | AlertDescription::UnexpectedMessage
                ),
                "{a:?}"
            );
        }
        // Too long a record: record_overflow, before its body arrives.
        let mut r = recs[0][..5].to_vec();
        r[3..5].copy_from_slice(&20_000u16.to_be_bytes());
        assert_eq!(fatal(&r), AlertDescription::RecordOverflow);
        // A ChangeCipherSpec after the handshake: unexpected_message.
        assert_eq!(
            fatal(&[20, 3, 3, 0, 1, 1]),
            AlertDescription::UnexpectedMessage
        );
        // Truncation at every length: progress up to the last whole
        // record, then waiting.
        let all = recs.concat();
        for cut in (0..all.len()).step_by(37) {
            let (pr, conn) = run(&all[..cut], false);
            assert!(pr.error.is_none());
            let ends: Vec<usize> = recs
                .iter()
                .scan(0, |n, r| {
                    *n += r.len();
                    Some(*n)
                })
                .collect();
            let whole = ends.iter().rfind(|e| **e <= cut).copied().unwrap_or(0);
            assert_eq!(pr.consumed, whole);
            assert!(!conn.session().unwrap().is_closed());
            let (pr, _) = run(&all[..cut], true);
            assert!(pr.truncated);
        }
        // Random bytes after the transcript: never a panic.
        for n in 0..100 {
            let mut input = all.clone();
            input.extend(Rng(n + 1).bytes(n as usize * 3));
            run(&input, n % 2 == 0);
        }
        if tls13 {
            // A handshake message other than KeyUpdate, a bad KeyUpdate, data
            // after a KeyUpdate, an empty record: sealed with the client's
            // key at its next sequence number.
            let h = if suite.suite() == TLS13_AES_256_GCM_SHA384.suite() {
                Hash::Sha384
            } else {
                Hash::Sha256
            };
            let c0 = secrets.get("CLIENT_TRAFFIC_SECRET_0");
            let aead = match u16::from(suite.suite()) {
                0x1301 => record::Aead::Aes128Gcm,
                0x1302 => record::Aead::Aes256Gcm,
                _ => record::Aead::Chacha20Poly1305,
            };
            let key = record::expand_label(h, &c0, b"key", b"", aead.key_len());
            let iv = record::expand_label(h, &c0, b"iv", b"", 12);
            let k = RecordKey::new(aead, &key, &iv).unwrap();
            let seq = 0;
            for (typ, body, want) in [
                (22, vec![4, 0, 0, 0], AlertDescription::UnexpectedMessage),
                (22, vec![24, 0, 0, 1, 2], AlertDescription::IllegalParameter),
                (22, vec![24, 0, 0, 2, 0, 0], AlertDescription::DecodeError),
                (22, vec![24, 0, 0], AlertDescription::UnexpectedMessage),
                (
                    22,
                    vec![24, 0, 0, 1, 0, 24],
                    AlertDescription::UnexpectedMessage,
                ),
                (
                    22,
                    vec![24, 0, 0, 5, 0],
                    AlertDescription::UnexpectedMessage,
                ),
                (22, Vec::new(), AlertDescription::UnexpectedMessage),
                (21, vec![1], AlertDescription::DecodeError),
                (21, vec![1, 42], AlertDescription::DecodeError),
                (20, vec![1], AlertDescription::UnexpectedMessage),
                (99, vec![1], AlertDescription::UnexpectedMessage),
            ] {
                assert_eq!(fatal(&k.seal13(seq, typ, &body)), want, "{typ} {body:?}");
            }
            // A fatal alert from the peer closes without an answer.
            let (pr, conn) = run(&k.seal13(seq, 21, &[2, 40]), false);
            assert_eq!(
                pr.error,
                Some(Error::AlertReceived(AlertDescription::HandshakeFailure))
            );
            assert!(pr.out.is_empty());
            assert!(conn.session().unwrap().is_closed());
            // user_canceled is ignored.
            let (pr, _) = run(&k.seal13(seq, 21, &[1, 90]), false);
            assert!(pr.error.is_none());
        }
    }
}

/// A 1.2 renegotiation attempt gets a no_renegotiation warning; the
/// connection carries on.
#[test]
fn tls12_renegotiation_is_refused() {
    let (config, cert) = server(KeyKind::P256, opts());
    let (c, _) = client(
        &cert.ca,
        Some(TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
        None,
    );
    let mut p = Pair::new(c, config.clone(), Feed::Whole);
    p.handshake();
    // A ClientHello-shaped handshake record, sealed with the client's key
    // (the engine's rx key, from its state).
    let st = p.server.conn.state().unwrap();
    let key = bytes(&st, "rx.key");
    let iv = bytes(&st, "rx.iv");
    let k = RecordKey::new(record::Aead::Chacha20Poly1305, &key, &iv).unwrap();
    let (_, rx) = p.server.conn.session().unwrap().seqs();
    let out = p.server.feed(&k.seal12(rx, 22, &[1, 0, 0, 0]), false);
    assert!(p.server.errors.is_empty());
    // The client sees a warning alert, which rustls ignores.
    p.client_in(&out).unwrap();
    // The engine read one more record than the real client sent, so only
    // the engine's side is checked from here.
    let (_, rx2) = p.server.conn.session().unwrap().seqs();
    assert_eq!(rx2, rx + 1);
    let w = p.server.conn.write(b"still here").unwrap();
    p.client_in(&w).unwrap();
    assert!(p.received.ends_with(b"still here"));
}
