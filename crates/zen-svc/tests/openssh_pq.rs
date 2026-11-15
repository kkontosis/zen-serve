//! The post-quantum algorithms OpenSSH has only recently: the key exchange
//! `mlkem768x25519-sha256` (from 9.9), `sntrup761x25519-sha512` under its
//! new name (9.9), and `ssh-mldsa44-ed25519` host and user keys (10.6).
//! The runners' OpenSSH (Ubuntu 24.04: 9.6) has none of them, so these
//! tests are ignored by default; CI's `openssh-pq` job runs them against
//! OpenSSH 10.6p1 built from source (`scripts/build-openssh.sh`):
//!
//! ```sh
//! ZEN_TEST_SSH=/path/to/ssh cargo test -p zen-svc --test openssh_pq -- --ignored
//! ```

mod sshcommon;

use sshcommon::*;
use zen_svc::ssh::{Cipher, HostAlg, HostKey, Options};

const KEX: &str = "KexAlgorithms=mlkem768x25519-sha256";

#[test]
#[ignore = "needs OpenSSH 9.9+ (ZEN_TEST_SSH)"]
fn hybrid_exec_with_each_cipher() {
    let t = tmp("pq");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
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
            &["echo hybrid"],
            b"",
        );
        let stats = h.join().unwrap();
        assert_eq!(r.code, Some(0), "{name}: {r:?} {stats:?}");
        assert_eq!(r.stdout, b"hybrid\n");
        assert_eq!(stats.ciphers, Some((cipher, cipher)));
    }
}

#[test]
#[ignore = "needs OpenSSH 9.9+ (ZEN_TEST_SSH)"]
fn hybrid_cat_with_rekeys_both_ways() {
    let t = tmp("pqcat");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    let opts = Options {
        rekey_bytes: 512 << 10,
        ..Options::default()
    };
    let (port, h) = serve_one(config(opts), App::new(vec![public]));
    let data = random(1 << 20);
    let r = ssh(&t.0, port, &key, &[KEX, "RekeyLimit=256K"], &["cat"], &data);
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{} {stats:?}", r.stderr);
    assert_eq!(sha256(&r.stdout), sha256(&data));
    assert!(stats.kexes >= 4, "{stats:?}");
}
const HYBRID: &str = "HostKeyAlgorithms=ssh-mldsa44-ed25519";

#[test]
#[ignore = "needs OpenSSH 9.9+ (ZEN_TEST_SSH)"]
fn sntrup761_under_its_new_name() {
    let t = tmp("sntrupnew");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let r = ssh(
        &t.0,
        port,
        &key,
        &["KexAlgorithms=sntrup761x25519-sha512"],
        &["echo sntrup"],
        b"",
    );
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?} {stats:?}");
    assert_eq!(stats.sid_len, Some(64));
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn hybrid_host_key_with_each_kex() {
    let t = tmp("pqhk");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    for kex in [
        "mlkem768x25519-sha256",
        "sntrup761x25519-sha512",
        "curve25519-sha256",
    ] {
        // Only the hybrid held, and both: the client asks for the hybrid.
        for keys in [vec![hybrid_host_key()], vec![host_key(), hybrid_host_key()]] {
            let (port, h) = serve_one(
                config_keys(keys, Options::default()),
                App::new(vec![public.clone()]),
            );
            let r = ssh(
                &t.0,
                port,
                &key,
                &[&format!("KexAlgorithms={kex}"), HYBRID],
                &["echo pq"],
                b"",
            );
            let stats = h.join().unwrap();
            assert_eq!(r.code, Some(0), "{kex}: {r:?} {stats:?}");
            assert_eq!(r.stdout, b"pq\n");
            assert_eq!(stats.host_alg, Some(HostAlg::MlDsa44Ed25519));
        }
    }
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn a_wrong_hybrid_host_key_is_refused() {
    // known_hosts holds the service's keys; this service has another.
    let t = tmp("pqwrong");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    let other = HostKey::hybrid_from_seeds(&[1; 32], &[2; 32]);
    let (port, h) = serve_one(
        config_keys(vec![other], Options::default()),
        App::new(vec![public]),
    );
    let r = ssh(&t.0, port, &key, &[HYBRID], &["echo no"], b"");
    h.join().unwrap();
    assert_eq!(r.code, Some(255), "{r:?}");
    assert!(
        r.stderr.contains("HOST IDENTIFICATION HAS CHANGED")
            || r.stderr.contains("Host key verification failed"),
        "{}",
        r.stderr
    );
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn hybrid_user_key() {
    let t = tmp("pquser");
    let (key, public) = user_key(&t.0, "id_mldsa", &["-t", "mldsa44-ed25519"]);
    assert_eq!(public.algorithm(), "ssh-mldsa44-ed25519");
    let (port, h) = serve_one(
        config_keys(vec![host_key(), hybrid_host_key()], Options::default()),
        App::new(vec![public.clone()]),
    );
    let r = ssh(&t.0, port, &key, &[], &["echo user"], b"");
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?} {stats:?}");
    assert_eq!(r.stdout, b"user\n");
    assert_eq!(stats.user.as_deref(), Some("test"));
    assert_eq!(
        stats.asked,
        [("test".to_owned(), false), ("test".to_owned(), true)]
    );
    // Another hybrid key isn't authorized.
    let (other, _) = user_key(&t.0, "id_other", &["-t", "mldsa44-ed25519"]);
    let (port, h) = serve_one(config(Options::default()), App::new(vec![public]));
    let r = ssh(&t.0, port, &other, &[], &["echo no"], b"");
    h.join().unwrap();
    assert_eq!(r.code, Some(255), "{r:?}");
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn hybrid_keys_match_ssh_keygen() {
    let t = tmp("pqkeys");
    let (f, public) = user_key(&t.0, "host", &["-t", "mldsa44-ed25519"]);
    let hk = HostKey::from_openssh(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(hk.alg(), HostAlg::MlDsa44Ed25519);
    assert_eq!(hk.public(), public);
    let line = std::fs::read_to_string(f.with_extension("pub")).unwrap();
    assert!(line.starts_with(&hk.public_line()), "{line}");
    let o = std::process::Command::new(keygen_bin())
        .args(["-l", "-E", "sha256", "-f"])
        .arg(f.with_extension("pub"))
        .output()
        .unwrap();
    let fp = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        fp.split_whitespace().nth(1),
        Some(&public.fingerprint()[..])
    );
    // `ssh-keygen -y` derives the same public key from the private one.
    let o = std::process::Command::new(keygen_bin())
        .args(["-y", "-f"])
        .arg(&f)
        .output()
        .unwrap();
    assert!(
        String::from_utf8(o.stdout)
            .unwrap()
            .starts_with(&hk.public_line())
    );
    // And the reverse: OpenSSH reads the PEM `to_openssh` writes.
    let ours = HostKey::generate(HostAlg::MlDsa44Ed25519);
    assert!(keygen_reads(&t.0, "ours", &ours).starts_with(&ours.public_line()));
    // The key, loaded, serves as a host key OpenSSH accepts.
    let (key, user) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    let (port, h) = serve_one(
        config_keys(vec![hk.clone()], Options::default()),
        App::new(vec![user]),
    );
    let r = ssh_known(
        &t.0,
        port,
        &key,
        &[hk.public_line()],
        &[HYBRID],
        &["echo loaded"],
        b"",
    );
    h.join().unwrap();
    assert_eq!(r.code, Some(0), "{r:?}");
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn hybrid_host_key_through_rekeys() {
    // OpenSSH checks the host key again at every rekey; the server offers
    // only the hybrid then.
    let t = tmp("pqrekey");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    let opts = Options {
        rekey_bytes: 512 << 10,
        ..Options::default()
    };
    let (port, h) = serve_one(
        config_keys(vec![host_key(), hybrid_host_key()], opts),
        App::new(vec![public]),
    );
    let data = random(1 << 20);
    let r = ssh(
        &t.0,
        port,
        &key,
        &[HYBRID, "RekeyLimit=256K"],
        &["cat"],
        &data,
    );
    let stats = h.join().unwrap();
    assert_eq!(r.code, Some(0), "{} {stats:?}", r.stderr);
    assert_eq!(sha256(&r.stdout), sha256(&data));
    assert!(stats.kexes >= 4, "{stats:?}");
    assert_eq!(stats.host_alg, Some(HostAlg::MlDsa44Ed25519));
}

#[test]
#[ignore = "needs OpenSSH 10.6+ (ZEN_TEST_SSH)"]
fn a_known_hybrid_line_is_preferred() {
    // OpenSSH 10.6 lists the hybrid last by default, but moves the
    // algorithms of a host's known_hosts lines first: a client that knows
    // only the hybrid key asks for it, with no options.
    let t = tmp("pqknown");
    let (key, public) = user_key(&t.0, "id_ed25519", &["-t", "ed25519"]);
    for (known, want) in [
        (
            vec![hybrid_host_key().public_line()],
            HostAlg::MlDsa44Ed25519,
        ),
        (vec![host_key().public_line()], HostAlg::Ed25519),
    ] {
        let (port, h) = serve_one(
            config_keys(vec![host_key(), hybrid_host_key()], Options::default()),
            App::new(vec![public.clone()]),
        );
        let r = ssh_known(&t.0, port, &key, &known, &[], &["echo known"], b"");
        let stats = h.join().unwrap();
        assert_eq!(r.code, Some(0), "{r:?} {stats:?}");
        assert_eq!(stats.host_alg, Some(want));
    }
}
