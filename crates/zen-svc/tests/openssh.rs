//! The SSH engine against OpenSSH's `ssh`, an independent implementation:
//! both ciphers, Ed25519 and RSA user keys, refusals, client and server
//! rekeys under load, a large download, a shell, a subsystem, exit
//! statuses, and strict kex against an injected packet. A TCP server
//! thread drives the engine, restoring it from its state at every step.
//!
//! These tests need OpenSSH's `ssh` and `ssh-keygen` (`ZEN_TEST_SSH` names
//! another `ssh`; its `ssh-keygen` is taken from the same directory).
//! `curve25519-sha256` is used here; the hybrid is in `openssh_pq.rs`.

mod sshcommon;

use sshcommon::*;
use zen_svc::ssh::{Cipher, Error, Options, reason};

const KEX: &str = "KexAlgorithms=curve25519-sha256";

fn setup(name: &str) -> (Tmp, std::path::PathBuf, zen_svc::ssh::PublicKey) {
    let t = tmp(name);
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    (t, key, public)
}

#[test]
fn exec_with_each_cipher() {
    let (t, key, public) = setup("exec");
    for (name, cipher) in [
        ("chacha20-poly1305@openssh.com", Cipher::Chacha20Poly1305),
        ("aes256-gcm@openssh.com", Cipher::Aes256Gcm),
    ] {
        let (port, h) = serve_one(config(Options::default()), App::new(vec![public.clone()]));
        let r = ssh(
            &t.0,
            port,
            &key,
            &[KEX, &format!("Ciphers={name}")],
            &["echo hello there"],
            b"",
        );
        let stats = h.join().unwrap();
        assert_eq!(r.code, Some(0), "{name}: {r:?} {stats:?}");
        assert_eq!(r.stdout, b"hello there\n");
        assert_eq!(r.stderr, "");
        assert_eq!(stats.ciphers, Some((cipher, cipher)));
        assert_eq!(stats.user.as_deref(), Some("test"));
        assert_eq!(stats.kexes, 1);
        // A query, then the signed request.
        assert_eq!(
            stats.asked,
            [("test".to_owned(), false), ("test".to_owned(), true)]
        );
    }
}

#[test]
fn rsa_user_keys() {
    let t = tmp("rsa");
    let (key, public) = user_key(&t.0, "id_rsa", &["-t", "rsa", "-b", "3072"]);
    for alg in ["rsa-sha2-256", "rsa-sha2-512"] {
        let (port, h) = serve_one(config(Options::default()), App::new(vec![public.clone()]));
        let opt = format!("PubkeyAcceptedAlgorithms={alg}");
        let r = ssh(&t.0, port, &key, &[KEX, &opt], &["echo rsa"], b"");
        let stats = h.join().unwrap();
        assert_eq!(r.code, Some(0), "{alg}: {r:?} {stats:?}");
        assert_eq!(r.stdout, b"rsa\n");
    }
    // SHA-1 signatures are refused.
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public.clone()]));
    let r = ssh(
        &t.0,
        port,
        &key,
        &[KEX, "PubkeyAcceptedAlgorithms=ssh-rsa"],
        &["echo no"],
        b"",
    );
    h.join().unwrap();
    assert_eq!(r.code, Some(255), "{r:?}");
    assert!(
        r.stderr.contains("Permission denied (publickey)"),
        "{}",
        r.stderr
    );
}

#[test]
fn an_unauthorized_key_is_refused() {
    let (t, key, _) = setup("unauth");
    let (_, other) = user_key(&t.0, "other", &["-t", "ed25519"]);
    let (port, h) = serve_one(config(Options::default()), App::new(vec![other]));
    let r = ssh(&t.0, port, &key, &[KEX], &["echo no"], b"");
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(255), "{r:?}");
    assert!(
        r.stderr.contains("Permission denied (publickey)"),
        "{}",
        r.stderr
    );
    assert_eq!(stats.user, None);
    // Only the query was asked: the key was never accepted.
    assert_eq!(stats.asked, [("test".to_owned(), false)]);
}

#[test]
fn cat_10mb_with_client_rekeys() {
    let (t, key, public) = setup("cat");
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let data = random(10 << 20);
    let r = ssh(&t.0, port, &key, &[KEX, "RekeyLimit=1M"], &["cat"], &data);
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{} {stats:?}", r.stderr);
    assert_eq!(r.stdout.len(), data.len());
    assert_eq!(sha256(&r.stdout), sha256(&data));
    // 20 MB crossed the connection: one rekey per MB or so.
    assert!(stats.kexes >= 10, "{stats:?}");
    assert!(stats.max_state < 65_536, "{stats:?}");
}

#[test]
fn the_server_rekeys() {
    let (t, key, public) = setup("srekey");
    let opts = Options {
        rekey_bytes: 1 << 20,
        ..Options::default()
    };
    let (port, h) = serve_one(config(opts), App::new(vec![public]));
    let data = random(5 << 20);
    // The client's own limit is high: the rekeys are the server's.
    let r = ssh(&t.0, port, &key, &[KEX, "RekeyLimit=1G"], &["cat"], &data);
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{} {stats:?}", r.stderr);
    assert_eq!(sha256(&r.stdout), sha256(&data));
    assert!(stats.kexes >= 5, "{stats:?}");
}

#[test]
fn big_download() {
    let (t, key, public) = setup("big");
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let n = 20_000_000u64;
    let r = ssh(
        &t.0,
        port,
        &key,
        &[KEX, "Ciphers=aes256-gcm@openssh.com"],
        &[&format!("big {n}")],
        b"",
    );
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{} {stats:?}", r.stderr);
    assert_eq!(r.stdout.len() as u64, n);
    assert!(
        r.stdout
            .iter()
            .enumerate()
            .all(|(i, &b)| b == (i % 251) as u8)
    );
}

#[test]
fn shell_subsystem_and_exit_status() {
    let (t, key, public) = setup("shell");
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public.clone()]));
    let r = ssh(&t.0, port, &key, &[KEX], &[], b"one\ntwo\n");
    h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?}");
    assert_eq!(r.stdout, b"> one\n> two\n");

    let (port, h) = serve_one(config(Options::default()), App::new(vec![public.clone()]));
    let r = ssh(&t.0, port, &key, &[KEX], &["-s", "test-sub"], b"ping");
    h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?}");
    assert_eq!(r.stdout, b"sub:ping");

    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let r = ssh(&t.0, port, &key, &[KEX], &["exit 3"], b"");
    h.join().unwrap();
    assert_eq!(r.code, Some(3), "{r:?}");
    assert!(r.stderr.contains("bye"), "{r:?}");
}

#[test]
fn small_steps_from_openssh() {
    // The engine is fed at most 7 bytes per step: every packet arrives in
    // pieces, and the state is restored between them.
    let (t, key, public) = setup("small");
    let (port, h) = serve_with(config(Options::default()), App::new(vec![public]), 7);
    let r = ssh(&t.0, port, &key, &[KEX], &["cat"], b"small steps\n");
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?} {stats:?}");
    assert_eq!(r.stdout, b"small steps\n");
    assert!(stats.steps > 300, "{stats:?}");
}

#[test]
fn keys_match_ssh_keygen() {
    let t = tmp("keys");
    // A host key from `ssh-keygen`, read as `HostKey`; its public line
    // and fingerprint as ssh-keygen gives them.
    let (f, public) = user_key(&t.0, "host", &["-t", "ed25519"]);
    let pem = std::fs::read_to_string(&f).unwrap();
    let hk = zen_svc::ssh::HostKey::from_openssh(&pem).unwrap();
    assert_eq!(hk.public(), public);
    let line = std::fs::read_to_string(f.with_extension("pub")).unwrap();
    assert!(line.starts_with(&hk.public_line()), "{line}");
    let fp = |f: &std::path::Path| {
        let o = std::process::Command::new(keygen_bin())
            .args(["-l", "-E", "sha256", "-f"])
            .arg(f)
            .output()
            .unwrap();
        String::from_utf8(o.stdout)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned()
    };
    assert_eq!(public.fingerprint(), fp(&f.with_extension("pub")));
    let (rf, rsa) = user_key(&t.0, "rsa", &["-t", "rsa", "-b", "2048"]);
    assert_eq!(rsa.fingerprint(), fp(&rf.with_extension("pub")));
    assert_eq!(rsa.algorithm(), "ssh-rsa");
    // An encrypted key, an ECDSA key, and a 1024-bit RSA key are refused.
    let (ef, _) = user_key(&t.0, "enc", &["-t", "ed25519"]);
    let o = std::process::Command::new(keygen_bin())
        .args(["-q", "-p", "-P", "", "-N", "secret", "-f"])
        .arg(&ef)
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(zen_svc::ssh::HostKey::from_openssh(&std::fs::read_to_string(&ef).unwrap()).is_err());
    let (_, _) = (rf, ef);
    let o = std::process::Command::new(keygen_bin())
        .args(["-q", "-N", "", "-t", "ecdsa", "-f"])
        .arg(t.0.join("ec"))
        .output()
        .unwrap();
    assert!(o.status.success());
    let ec = std::fs::read_to_string(t.0.join("ec.pub")).unwrap();
    assert!(zen_svc::ssh::PublicKey::from_line(&ec).is_err());
    let o = std::process::Command::new(keygen_bin())
        .args(["-q", "-N", "", "-t", "rsa", "-b", "1024", "-f"])
        .arg(t.0.join("small"))
        .output()
        .unwrap();
    assert!(o.status.success());
    let small = std::fs::read_to_string(t.0.join("small.pub")).unwrap();
    assert!(zen_svc::ssh::PublicKey::from_line(&small).is_err());
}

/// Terrapin (CVE-2023-48795): an IGNORE injected into the client's stream
/// during the initial key exchange ends the connection under strict kex.
#[test]
fn strict_kex_refuses_an_injected_packet() {
    let (t, key, public) = setup("terrapin");
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let proxy = inject_proxy(port, plain_ignore());
    let r = ssh(&t.0, proxy, &key, &[KEX], &["echo no"], b"");
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(255), "{r:?}");
    assert_eq!(
        stats.error,
        Some(Error::Protocol {
            reason: reason::PROTOCOL_ERROR,
            detail: "strict kex: an unexpected message during the key exchange",
        })
    );
    assert!(r.stderr.contains("strict kex"), "{}", r.stderr);
}
