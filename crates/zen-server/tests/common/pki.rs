//! Test certificates: CAs, server and client certificates made with
//! rcgen, signed with RustCrypto keys (rcgen's own crypto backends are not
//! built), and HTTPS clients on the server's rustls provider.

use base64::Engine;
use ecdsa::signature::Signer as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zen_server::config::TlsConfig;

/// A private key for rcgen and rustls.
#[derive(Clone)]
pub enum Key {
    P256(p256::ecdsa::SigningKey, Vec<u8>),
    P384(p384::ecdsa::SigningKey, Vec<u8>),
    Ed25519(ed25519_dalek::SigningKey, Vec<u8>),
    /// An RSA key (PKCS#1 `RSAPublicKey`), signing certificates with
    /// PKCS#1 v1.5 and the given algorithm's hash. The server refuses RSA
    /// private keys; test clients sign handshakes with it ([`RsaClientKey`]).
    Rsa(
        Arc<rsa::RsaPrivateKey>,
        Vec<u8>,
        &'static rcgen::SignatureAlgorithm,
    ),
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

    /// An RSA key of `bits` (generated once per process and seed),
    /// signing certificates with SHA-256.
    pub fn rsa(seed: u8, bits: usize) -> Key {
        Self::rsa_with(seed, bits, &rcgen::PKCS_RSA_SHA256)
    }

    /// [`Key::rsa`], signing certificates with SHA-512.
    pub fn rsa_sha512(seed: u8, bits: usize) -> Key {
        Self::rsa_with(seed, bits, &rcgen::PKCS_RSA_SHA512)
    }

    fn rsa_with(seed: u8, bits: usize, alg: &'static rcgen::SignatureAlgorithm) -> Key {
        let k = zen_server::webauthn::soft::rsa_key(seed, bits);
        let public = zen_server::rsakey::testing::public_der(&k.to_public_key());
        Key::Rsa(Arc::new(k), public, alg)
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
            Key::Rsa(k, ..) => {
                PrivateKeyDer::Pkcs8(zen_server::rsakey::testing::pkcs8_der(k).into())
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
            Key::P256(_, p) | Key::P384(_, p) | Key::Ed25519(_, p) | Key::Rsa(_, p, _) => p,
        }
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        match self {
            Key::P256(..) => &rcgen::PKCS_ECDSA_P256_SHA256,
            Key::P384(..) => &rcgen::PKCS_ECDSA_P384_SHA384,
            Key::Ed25519(..) => &rcgen::PKCS_ED25519,
            Key::Rsa(_, _, alg) => alg,
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
            Key::Rsa(k, _, alg) => {
                use rsa::signature::{SignatureEncoding as _, Signer as _};
                let k = (**k).clone();
                if std::ptr::eq(*alg, &rcgen::PKCS_RSA_SHA512) {
                    rsa::pkcs1v15::SigningKey::<sha2::Sha512>::new(k)
                        .sign(msg)
                        .to_vec()
                } else {
                    rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(k)
                        .sign(msg)
                        .to_vec()
                }
            }
        })
    }
}

/// A test client's RSA key for TLS 1.3 handshakes: RSA-PSS with SHA-256.
/// (The server's own provider refuses RSA private keys.)
#[derive(Clone)]
pub struct RsaClientKey(Arc<rsa::RsaPrivateKey>, Vec<u8>);

impl std::fmt::Debug for RsaClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsaClientKey")
    }
}

impl rustls::sign::SigningKey for RsaClientKey {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        offered
            .contains(&rustls::SignatureScheme::RSA_PSS_SHA256)
            .then(|| Box::new(self.clone()) as Box<dyn rustls::sign::Signer>)
    }

    fn public_key(&self) -> Option<rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
        Some(rustls::sign::public_key_to_spki(
            &rustls::pki_types::alg_id::RSA_ENCRYPTION,
            &self.1,
        ))
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::RSA
    }
}

impl rustls::sign::Signer for RsaClientKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _};
        let k = rsa::pss::SigningKey::<sha2::Sha256>::new((*self.0).clone());
        Ok(
            k.sign_with_rng(&mut zen_server::rsakey::testing::OsRng, message)
                .to_vec(),
        )
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        rustls::SignatureScheme::RSA_PSS_SHA256
    }
}

/// Always present one certificate and key.
#[derive(Debug)]
struct Fixed(Arc<rustls::sign::CertifiedKey>);

impl rustls::client::ResolvesClientCert for Fixed {
    fn resolve(
        &self,
        _: &[&[u8]],
        _: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
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

/// `[tls]` for a test server, its files in `dir`: `server`'s certificate
/// and key, and `client_ca` if given.
pub fn tls_config(dir: &Path, server: &Ident, client_ca: Option<&Ca>) -> TlsConfig {
    let write = |name: &str, text: String| -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    };
    TlsConfig {
        cert: write("server.pem", server.pem()),
        key: write("server.key", server.key.pem()),
        client_ca: client_ca.map(|ca| write("client-ca.pem", ca.pem())),
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
        Some(Ident {
            cert,
            key: Key::Rsa(k, public, _),
        }) => {
            let key = Arc::new(RsaClientKey(k.clone(), public.clone()));
            let ck = rustls::sign::CertifiedKey::new(vec![cert.clone()], key);
            b.with_client_cert_resolver(Arc::new(Fixed(Arc::new(ck))))
        }
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
