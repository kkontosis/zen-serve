//! Test certificates (feature `test-utils`): a CA and a server certificate
//! for `localhost` and `127.0.0.1`, made with rcgen and signed with
//! RustCrypto keys (rcgen's own crypto backends are not built).

use ecdsa::signature::Signer as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The server key's algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    P256,
    P384,
    Ed25519,
}

/// A CA certificate, and a server certificate chain with its key.
pub struct TestCert {
    pub ca: CertificateDer<'static>,
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

enum Key {
    P256(p256::ecdsa::SigningKey, Vec<u8>),
    P384(p384::ecdsa::SigningKey, Vec<u8>),
    Ed25519(ed25519_dalek::SigningKey, Vec<u8>),
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("random bytes");
    b
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

impl Key {
    fn new(kind: KeyKind) -> Key {
        match kind {
            KeyKind::P256 => {
                let sk = loop {
                    if let Ok(k) = p256::ecdsa::SigningKey::from_slice(&random::<32>()) {
                        break k;
                    }
                };
                let public = sk.verifying_key().to_sec1_point(false).as_bytes().to_vec();
                Key::P256(sk, public)
            }
            KeyKind::P384 => {
                let sk = loop {
                    if let Ok(k) = p384::ecdsa::SigningKey::from_slice(&random::<48>()) {
                        break k;
                    }
                };
                let public = sk.verifying_key().to_sec1_point(false).as_bytes().to_vec();
                Key::P384(sk, public)
            }
            KeyKind::Ed25519 => {
                let sk = ed25519_dalek::SigningKey::from_bytes(&random::<32>());
                let public = sk.verifying_key().as_bytes().to_vec();
                Key::Ed25519(sk, public)
            }
        }
    }

    /// PKCS#8 for P-256 and Ed25519, SEC1 for P-384 (fixed layouts).
    fn der(&self) -> PrivateKeyDer<'static> {
        match self {
            Key::P256(sk, public) => {
                let mut v =
                    hex("308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420");
                v.extend_from_slice(&sk.to_bytes());
                v.extend_from_slice(&hex("a144034200"));
                v.extend_from_slice(public);
                PrivateKeyDer::Pkcs8(v.into())
            }
            Key::P384(sk, public) => {
                let mut v = hex("3081a40201010430");
                v.extend_from_slice(&sk.to_bytes());
                v.extend_from_slice(&hex("a00706052b81040022a164036200"));
                v.extend_from_slice(public);
                PrivateKeyDer::Sec1(v.into())
            }
            Key::Ed25519(sk, _) => {
                let mut v = hex("302e020100300506032b657004220420");
                v.extend_from_slice(sk.as_bytes());
                PrivateKeyDer::Pkcs8(v.into())
            }
        }
    }
}

impl rcgen::PublicKeyData for Key {
    fn der_bytes(&self) -> &[u8] {
        match self {
            Key::P256(_, p) | Key::P384(_, p) | Key::Ed25519(_, p) => p,
        }
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        match self {
            Key::P256(..) => &rcgen::PKCS_ECDSA_P256_SHA256,
            Key::P384(..) => &rcgen::PKCS_ECDSA_P384_SHA384,
            Key::Ed25519(..) => &rcgen::PKCS_ED25519,
        }
    }
}

impl rcgen::SigningKey for Key {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        Ok(match self {
            Key::P256(k, _) => {
                let s: p256::ecdsa::Signature = k.sign(msg);
                s.to_der().as_bytes().to_vec()
            }
            Key::P384(k, _) => {
                let s: p384::ecdsa::Signature = k.sign(msg);
                s.to_der().as_bytes().to_vec()
            }
            Key::Ed25519(k, _) => ed25519_dalek::Signer::sign(k, msg).to_bytes().to_vec(),
        })
    }
}

fn serial() -> rcgen::SerialNumber {
    let mut s = random::<16>();
    s[0] &= 0x7f;
    rcgen::SerialNumber::from_slice(&s)
}

/// A fresh CA (P-256) and a server certificate it issued for `localhost`
/// and `127.0.0.1`, with a key of `kind`.
pub fn server_cert(kind: KeyKind) -> TestCert {
    let ca_key = Key::new(KeyKind::P256);
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "zen test CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params.serial_number = Some(serial());
    let ca = params.self_signed(&ca_key).expect("CA").der().clone();
    let issuer = rcgen::Issuer::new(params, ca_key);

    let key = Key::new(kind);
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("names");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "zen test server");
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.serial_number = Some(serial());
    let cert = params.signed_by(&key, &issuer).expect("cert").der().clone();
    TestCert {
        ca: ca.clone(),
        chain: vec![cert, ca],
        key: key.der(),
    }
}

/// PEM text of a DER item.
pub fn pem(label: &str, der: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::new();
    for chunk in der.chunks(3) {
        let n = chunk.len();
        let v = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= n {
                b64.push(B64[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                b64.push('=');
            }
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}
