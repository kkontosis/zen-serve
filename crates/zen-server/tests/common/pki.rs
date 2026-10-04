//! Test certificates: CAs, server and client certificates made with
//! rcgen, signed with RustCrypto keys (rcgen's own crypto backends are not
//! built), and HTTPS clients on the server's rustls provider.

use base64::Engine;
use ecdsa::signature::Signer as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;

/// A private key for rcgen and rustls.
#[derive(Clone)]
pub enum Key {
    P256(p256::ecdsa::SigningKey, Vec<u8>),
    P384(p384::ecdsa::SigningKey, Vec<u8>),
    Ed25519(ed25519_dalek::SigningKey, Vec<u8>),
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).unwrap();
    b
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

impl Key {
    pub fn p256() -> Key {
        let sk = loop {
            if let Ok(k) = p256::ecdsa::SigningKey::from_slice(&random::<32>()) {
                break k;
            }
        };
        let public = sk.verifying_key().to_sec1_point(false).as_bytes().to_vec();
        Key::P256(sk, public)
    }

    pub fn p384() -> Key {
        let sk = loop {
            if let Ok(k) = p384::ecdsa::SigningKey::from_slice(&random::<48>()) {
                break k;
            }
        };
        let public = sk.verifying_key().to_sec1_point(false).as_bytes().to_vec();
        Key::P384(sk, public)
    }

    pub fn ed25519() -> Key {
        let sk = ed25519_dalek::SigningKey::from_bytes(&random::<32>());
        let public = sk.verifying_key().as_bytes().to_vec();
        Key::Ed25519(sk, public)
    }

    /// The private key as rustls loads it: PKCS#8 for P-256 and Ed25519,
    /// SEC1 for P-384 (both fixed layouts, assembled by hand).
    pub fn der(&self) -> PrivateKeyDer<'static> {
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

    /// [`Key::der`] as PEM.
    pub fn pem(&self) -> String {
        match self.der() {
            PrivateKeyDer::Sec1(k) => pem("EC PRIVATE KEY", k.secret_sec1_der()),
            PrivateKeyDer::Pkcs8(k) => pem("PRIVATE KEY", k.secret_pkcs8_der()),
            _ => unreachable!(),
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

/// PEM text of one DER object.
pub fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(line).unwrap());
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

/// A random positive serial number (rcgen needs one without its own
/// crypto backend).
fn serial() -> rcgen::SerialNumber {
    let mut s = random::<16>();
    s[0] = (s[0] & 0x7f) | 0x01;
    rcgen::SerialNumber::from_slice(&s)
}

/// A certificate and its key.
#[derive(Clone)]
pub struct Ident {
    pub cert: CertificateDer<'static>,
    pub key: Key,
}

impl Ident {
    pub fn pem(&self) -> String {
        pem("CERTIFICATE", &self.cert)
    }

    /// SHA-256 of the certificate's SubjectPublicKeyInfo: its mTLS
    /// credential id (auth.md §10.3).
    pub fn spki_sha256(&self) -> Vec<u8> {
        use sha2::Digest;
        sha2::Sha256::digest(rcgen::PublicKeyData::subject_public_key_info(&self.key)).to_vec()
    }
}

/// What a certificate is for.
#[derive(Clone, Copy, PartialEq)]
pub enum Usage {
    Server,
    Client,
    /// A client certificate that expired in 2001.
    Expired,
}

/// A certificate authority.
pub struct Ca {
    issuer: rcgen::Issuer<'static, Key>,
    pub cert: CertificateDer<'static>,
}

impl Ca {
    pub fn new(name: &str) -> Ca {
        Self::with_key(name, Key::p256())
    }

    pub fn with_key(name: &str, key: Key) -> Ca {
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.serial_number = Some(serial());
        let cert = params.self_signed(&key).unwrap().der().clone();
        Ca {
            issuer: rcgen::Issuer::new(params, key),
            cert,
        }
    }

    pub fn pem(&self) -> String {
        pem("CERTIFICATE", &self.cert)
    }

    /// A certificate for `127.0.0.1` and `localhost`.
    pub fn server(&self) -> Ident {
        self.issue("zen-serve test", Usage::Server, Key::p256())
    }

    /// A client certificate with a fresh key.
    pub fn client(&self, name: &str) -> Ident {
        self.issue(name, Usage::Client, Key::p256())
    }

    pub fn issue(&self, name: &str, usage: Usage, key: Key) -> Ident {
        let sans = match usage {
            Usage::Server => vec!["127.0.0.1".to_string(), "localhost".to_string()],
            _ => Vec::new(),
        };
        let mut params = rcgen::CertificateParams::new(sans).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.extended_key_usages = vec![match usage {
            Usage::Server => rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            _ => rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        }];
        if usage == Usage::Expired {
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        params.serial_number = Some(serial());
        let cert = params.signed_by(&key, &self.issuer).unwrap().der().clone();
        Ident { cert, key }
    }
}

/// A rustls client configuration on the server's provider that trusts
/// `ca`, presenting `ident` if given.
pub fn client_config(ca: &Ca, ident: Option<&Ident>) -> rustls::ClientConfig {
    client_config_with(zen_server::tls::provider::provider(), ca, ident)
}

/// [`client_config`] with another provider (for example fewer groups).
pub fn client_config_with(
    provider: rustls::crypto::CryptoProvider,
    ca: &Ca,
    ident: Option<&Ident>,
) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.cert.clone()).unwrap();
    let b = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots);
    match ident {
        Some(i) => b
            .with_client_auth_cert(vec![i.cert.clone()], i.key.der())
            .unwrap(),
        None => b.with_no_client_auth(),
    }
}

/// An HTTPS client that trusts `ca`, presenting `ident` if given.
pub fn https_client(ca: &Ca, ident: Option<&Ident>) -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(client_config(ca, ident))
        .no_proxy()
        .build()
        .unwrap()
}
