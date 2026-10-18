//! The hybrid key exchange `mlkem768x25519-sha256` against OpenSSH, which
//! has it from 9.9. The runners' OpenSSH (Ubuntu 24.04: 9.6) doesn't, so
//! these tests are ignored by default; CI's `openssh-pq` job runs them
//! against OpenSSH built from source (`scripts/build-openssh.sh`):
//!
//! ```sh
//! ZEN_TEST_SSH=/path/to/ssh cargo test -p zen-svc --test openssh_pq -- --ignored
//! ```

mod sshcommon;

use sshcommon::*;
use zen_svc::ssh::{Cipher, Options};

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
