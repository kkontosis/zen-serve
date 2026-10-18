//! The TLS engine against `openssl s_client`, an independent
//! implementation: every suite and version, KeyUpdates both ways (checked
//! against OpenSSL's key log), renegotiation and a client without the
//! extended master secret refused. A TCP server thread drives the engine,
//! restoring it from its state after every read.
//!
//! These tests need the `openssl` command (OpenSSL 3).

mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use zen_svc::tls::{AlertDescription, Config, Error, Options};
use zen_tls_rustcrypto::record::{self, Hash};
use zen_tls_rustcrypto::testing::{KeyKind, pem};

fn openssl() {
    let ok = Command::new("openssl")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok,
        "these tests need the openssl command line tool (OpenSSL 3): install it, e.g. apt install openssl"
    );
}

/// A temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Tmp {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).unwrap();
    let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let d = std::env::temp_dir().join(format!("zen-svc-{name}-{hex}"));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// Serves one connection with the engine; returns the harness at the end.
fn serve_one(config: Arc<Config>) -> (u16, std::thread::JoinHandle<Server>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let (mut sock, _) = l.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut server = Server::new(config);
        let mut buf = vec![0u8; 65536];
        loop {
            let n: usize = sock.read(&mut buf).unwrap_or_default();
            let out = server.feed(&buf[..n], n == 0);
            let _ = sock.write_all(&out);
            if n == 0 || !server.errors.is_empty() {
                break;
            }
            if server.peer_closed {
                let _ = sock.write_all(&server.conn.close());
                break;
            }
        }
        let _ = sock.shutdown(std::net::Shutdown::Both);
        server
    });
    (port, h)
}

struct Client {
    stdout: String,
    stderr: String,
    ok: bool,
    keylog: String,
}

/// Runs `s_client` against `port`, typing `lines` (with pauses, so each is
/// its own read), then EOF on stdin, which makes it close.
fn s_client(port: u16, ca: &Path, args: &[&str], lines: &[&str], conf: Option<&Path>) -> Client {
    let dir = ca.parent().unwrap();
    let keylog = dir.join(format!("keylog-{port}.txt"));
    let mut cmd = Command::new("openssl");
    cmd.args(["s_client", "-connect"])
        .arg(format!("127.0.0.1:{port}"))
        .args([
            "-servername",
            "localhost",
            "-verify_return_error",
            "-CAfile",
        ])
        .arg(ca)
        .arg("-keylogfile")
        .arg(&keylog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(c) = conf {
        cmd.env("OPENSSL_CONF", c);
    }
    let mut child = cmd.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    {
        let mut stdin = child.stdin.take().unwrap();
        for l in lines {
            let _ = stdin.write_all(format!("{l}\n").as_bytes());
            let _ = stdin.flush();
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    let out = child.wait_with_output().unwrap();
    Client {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
        keylog: std::fs::read_to_string(&keylog).unwrap_or_default(),
    }
}

impl Client {
    fn secret(&self, label: &str) -> Vec<u8> {
        let line = self
            .keylog
            .lines()
            .find(|l| l.starts_with(&format!("{label} ")))
            .unwrap_or_else(|| panic!("no {label} in {}", self.keylog));
        let hex = line.split(' ').nth(2).unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }
}

/// The engine's configuration, and a directory holding its CA (`ca.pem`).
fn setup(name: &str) -> (Arc<Config>, Tmp) {
    openssl();
    let (config, cert) = server(KeyKind::P256, Options::default());
    let dir = tmp(name);
    std::fs::write(dir.0.join("ca.pem"), pem("CERTIFICATE", &cert.ca)).unwrap();
    (config, dir)
}

fn upd(secret: &[u8]) -> Vec<u8> {
    record::expand_label(Hash::Sha256, secret, b"traffic upd", b"", 32)
}

fn state_secret(server: &Server, dir: &str) -> Vec<u8> {
    let st = server.conn.state().unwrap();
    let v: ciborium::Value = ciborium::from_reader(&st[..]).unwrap();
    let find = |v: &ciborium::Value, k: &str| -> ciborium::Value {
        v.as_map()
            .unwrap()
            .iter()
            .find(|(x, _)| x.as_text() == Some(k))
            .unwrap()
            .1
            .clone()
    };
    find(&find(&v, dir), "secret").into_bytes().unwrap()
}

#[test]
fn s_client_tls13_every_suite_and_group() {
    let (config, dir) = setup("tls13");
    let ca = dir.0.join("ca.pem");
    for suite in [
        "TLS_AES_128_GCM_SHA256",
        "TLS_AES_256_GCM_SHA384",
        "TLS_CHACHA20_POLY1305_SHA256",
    ] {
        for group in ["X25519", "P-256"] {
            let (port, h) = serve_one(config.clone());
            let c = s_client(
                port,
                &ca,
                &["-tls1_3", "-ciphersuites", suite, "-groups", group],
                &["hello", "world"],
                None,
            );
            let server = h.join().unwrap();
            assert!(server.errors.is_empty(), "{:?} {}", server.errors, c.stderr);
            assert_eq!(server.received, b"hello\nworld\n", "{suite} {group}");
            assert!(c.stdout.contains("hello\nworld\n"), "{}", c.stdout);
            assert!(c.ok, "{}", c.stderr);
            assert!(server.peer_closed);
            assert!(server.restores > 2);
        }
    }
}

#[test]
fn s_client_tls12_every_suite_and_group() {
    let (config, dir) = setup("tls12");
    let ca = dir.0.join("ca.pem");
    for suite in [
        "ECDHE-ECDSA-AES128-GCM-SHA256",
        "ECDHE-ECDSA-AES256-GCM-SHA384",
        "ECDHE-ECDSA-CHACHA20-POLY1305",
    ] {
        // In 1.2 the certificate's curve (P-256) must be among the
        // client's groups too (RFC 8422 §5.1); the first is used for ECDHE.
        for (group, temp) in [("X25519:P-256", "X25519"), ("P-256", "ECDH, prime256v1")] {
            let (port, h) = serve_one(config.clone());
            let c = s_client(
                port,
                &ca,
                &["-tls1_2", "-cipher", suite, "-groups", group],
                &["hello", "world"],
                None,
            );
            let server = h.join().unwrap();
            assert!(server.errors.is_empty(), "{:?} {}", server.errors, c.stderr);
            assert_eq!(server.received, b"hello\nworld\n", "{suite} {group}");
            assert!(c.stdout.contains("hello\nworld\n"), "{}", c.stdout);
            assert!(c.stdout.contains("Extended master secret: yes"));
            assert!(
                c.stdout.contains(&format!("Server Temp Key: {temp}")),
                "{}",
                c.stdout
            );
            assert!(c.ok, "{}", c.stderr);
        }
    }
}

/// `k` (KeyUpdate) and `K` (KeyUpdate, requesting ours): the engine's
/// secrets follow OpenSSL's key log.
#[test]
fn s_client_key_updates() {
    let (config, dir) = setup("keyupdate");
    let ca = dir.0.join("ca.pem");
    for suite in ["TLS_AES_128_GCM_SHA256", "TLS_CHACHA20_POLY1305_SHA256"] {
        let (port, h) = serve_one(config.clone());
        let c = s_client(
            port,
            &ca,
            &["-tls1_3", "-ciphersuites", suite],
            &["one", "k", "two", "K", "three", "k", "four"],
            None,
        );
        let server = h.join().unwrap();
        assert!(server.errors.is_empty(), "{:?} {}", server.errors, c.stderr);
        assert_eq!(server.received, b"one\ntwo\nthree\nfour\n");
        assert!(c.stdout.contains("one\ntwo\n"), "{}", c.stdout);
        assert!(c.stdout.contains("three\nfour\n"), "{}", c.stdout);
        let c0 = c.secret("CLIENT_TRAFFIC_SECRET_0");
        let s0 = c.secret("SERVER_TRAFFIC_SECRET_0");
        assert_eq!(state_secret(&server, "rx"), upd(&upd(&upd(&c0))));
        assert_eq!(state_secret(&server, "tx"), upd(&s0));
    }
}

/// `R` in 1.2: the engine answers no_renegotiation and stays sound;
/// OpenSSL then gives up on the connection with a fatal alert.
#[test]
fn s_client_renegotiation_is_refused() {
    let (config, dir) = setup("reneg");
    let ca = dir.0.join("ca.pem");
    let (port, h) = serve_one(config);
    let c = s_client(
        port,
        &ca,
        &["-tls1_2", "-cipher", "ECDHE-ECDSA-AES128-GCM-SHA256"],
        &["one", "R", "two"],
        None,
    );
    let server = h.join().unwrap();
    // The engine answered no_renegotiation (OpenSSL reports it), and
    // OpenSSL then ended the connection with handshake_failure, which the
    // engine took as the peer's fatal alert.
    assert!(c.stderr.contains("no renegotiation"), "{}", c.stderr);
    assert_eq!(
        server.errors,
        [Error::AlertReceived(AlertDescription::HandshakeFailure)]
    );
    assert_eq!(server.received, b"one\n");
    assert!(c.stdout.contains("one\n"));
}

/// A 1.2 client without the extended master secret is refused.
#[test]
fn s_client_without_ems_is_refused() {
    let (config, dir) = setup("noems");
    let ca = dir.0.join("ca.pem");
    let conf = ca.parent().unwrap().join("noems.cnf");
    std::fs::write(
        &conf,
        "openssl_conf = openssl_init\n[openssl_init]\nssl_conf = ssl_sect\n\
         [ssl_sect]\nsystem_default = system_default_sect\n\
         [system_default_sect]\nOptions = -ExtendedMasterSecret\n",
    )
    .unwrap();
    let (port, h) = serve_one(config.clone());
    let c = s_client(port, &ca, &["-tls1_2"], &["hello"], Some(&conf));
    let server = h.join().unwrap();
    assert!(!server.conn.is_established());
    assert!(
        matches!(server.errors[..], [Error::Tls(_)]),
        "{:?}",
        server.errors
    );
    assert!(server.received.is_empty());
    assert!(!c.ok);
    // With EMS (the same configuration otherwise), it works.
    let (port, h) = serve_one(config);
    let c = s_client(port, &ca, &["-tls1_2"], &["hello"], None);
    let server = h.join().unwrap();
    assert_eq!(server.received, b"hello\n", "{}", c.stderr);
}

/// A stream larger than a record each way, through OpenSSL.
#[test]
fn s_client_large_lines() {
    let (config, dir) = setup("large");
    let ca = dir.0.join("ca.pem");
    for v in ["-tls1_3", "-tls1_2"] {
        let (port, h) = serve_one(config.clone());
        // Numbers only: s_client takes a stdin read that starts with Q, R, k
        // or K as a command.
        let line: String = (0..10_000).map(|i| format!("{i:05} ")).collect();
        let c = s_client(port, &ca, &[v], &[&line, &line], None);
        let server = h.join().unwrap();
        assert!(server.errors.is_empty(), "{:?} {}", server.errors, c.stderr);
        let want = format!("{line}\n{line}\n");
        assert_eq!(server.received.len(), want.len(), "{v}");
        assert!(server.received == want.as_bytes(), "{v}");
        assert!(c.stdout.contains(&want));
    }
}
