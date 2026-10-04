//! The rustls [`CryptoProvider`] of builds with the `ring` feature (the
//! default): rustls's own provider on ring, with zen-serve's post-quantum
//! hybrid key exchange put first, since ring has no ML-KEM.
//!
//! * **TLS 1.3 only**, with `TLS_AES_128_GCM_SHA256`,
//!   `TLS_AES_256_GCM_SHA384` and `TLS_CHACHA20_POLY1305_SHA256`: ring's,
//!   in the order of the pure-Rust provider.
//! * **Key exchange**, preferred first: `X25519MLKEM768`
//!   (draft-ietf-tls-ecdhe-mlkem; ML-KEM-768 and X25519 on RustCrypto,
//!   [`super::rustcrypto::X25519MLKEM768`]), then ring's `X25519`,
//!   `secp256r1` and `secp384r1`.
//! * **Signatures** verified: ring's, through webpki: ECDSA on P-256 and
//!   P-384, Ed25519, and RSA (PKCS#1 v1.5 for certificates, PSS for both,
//!   with SHA-256/384/512). RSA keys also pass the [`crate::rsakey`]
//!   policy first, as with the pure-Rust provider: ring alone takes 2048
//!   to 8192 bits and exponents from 3.
//! * **Private keys** (signing): ring's: ECDSA P-256 or P-384 (PKCS#8 or
//!   SEC1), Ed25519 (PKCS#8), and RSA (PKCS#1 or PKCS#8). ring's RSA
//!   signing is constant-time, and takes 2048- to 4096-bit keys with an
//!   exponent of at least 65537; the [`crate::rsakey`] policy then checks
//!   the public key, which also caps the exponent at 2³² − 1.

use super::rustcrypto::X25519MLKEM768;
use crate::rsakey::{self, Refused};
use rustls::crypto::ring as r;
use rustls::crypto::{CryptoProvider, KeyProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{
    AlgorithmIdentifier, InvalidSignature, PrivateKeyDer, SignatureVerificationAlgorithm,
};
use rustls::sign::SigningKey;
use rustls::{Error, SignatureAlgorithm, SignatureScheme};
use std::sync::Arc;
use webpki::ring as algs;

pub use rustls::crypto::ring::kx_group::{SECP256R1, SECP384R1, X25519};

/// The provider.
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![
            r::cipher_suite::TLS13_AES_128_GCM_SHA256,
            r::cipher_suite::TLS13_AES_256_GCM_SHA384,
            r::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        ],
        // Trait objects: ours and ring's mix freely.
        kx_groups: vec![&X25519MLKEM768, X25519, SECP256R1, SECP384R1],
        signature_verification_algorithms: SIGNATURE_ALGORITHMS,
        secure_random: r::default_provider().secure_random,
        key_provider: &Keys,
    }
}

// ---------------------------------------------------------------- verification

/// A ring RSA algorithm behind the [`crate::rsakey`] policy: 2048 to 4096
/// bits, an odd exponent from 65537 to 2³² − 1.
#[derive(Debug)]
struct RsaPolicy(&'static dyn SignatureVerificationAlgorithm);

impl SignatureVerificationAlgorithm for RsaPolicy {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        // As in the pure-Rust provider, the subjectPublicKey of
        // rsaEncryption is a PKCS#1 RSAPublicKey.
        rsakey::check_pkcs1_der(public_key).map_err(|_| InvalidSignature)?;
        self.0.verify_signature(public_key, message, signature)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.0.public_key_alg_id()
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.0.signature_alg_id()
    }
}

// rsaEncryption keys only (`_LEGACY_KEY` for PSS), and PKCS#1 v1.5 with
// the NULL parameters: the set of the pure-Rust provider.
static RSA_PKCS1_SHA256: RsaPolicy = RsaPolicy(algs::RSA_PKCS1_2048_8192_SHA256);
static RSA_PKCS1_SHA384: RsaPolicy = RsaPolicy(algs::RSA_PKCS1_2048_8192_SHA384);
static RSA_PKCS1_SHA512: RsaPolicy = RsaPolicy(algs::RSA_PKCS1_2048_8192_SHA512);
static RSA_PSS_SHA256: RsaPolicy = RsaPolicy(algs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY);
static RSA_PSS_SHA384: RsaPolicy = RsaPolicy(algs::RSA_PSS_2048_8192_SHA384_LEGACY_KEY);
static RSA_PSS_SHA512: RsaPolicy = RsaPolicy(algs::RSA_PSS_2048_8192_SHA512_LEGACY_KEY);

/// What certificates and handshake signatures may use: the algorithms
/// and mapping of the pure-Rust provider, on ring.
static SIGNATURE_ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        algs::ECDSA_P256_SHA256,
        algs::ECDSA_P256_SHA384,
        algs::ECDSA_P384_SHA256,
        algs::ECDSA_P384_SHA384,
        algs::ED25519,
        &RSA_PKCS1_SHA256,
        &RSA_PKCS1_SHA384,
        &RSA_PKCS1_SHA512,
        &RSA_PSS_SHA256,
        &RSA_PSS_SHA384,
        &RSA_PSS_SHA512,
    ],
    mapping: &[
        (
            SignatureScheme::ECDSA_NISTP384_SHA384,
            &[algs::ECDSA_P384_SHA384, algs::ECDSA_P256_SHA384],
        ),
        (
            SignatureScheme::ECDSA_NISTP256_SHA256,
            &[algs::ECDSA_P256_SHA256, algs::ECDSA_P384_SHA256],
        ),
        (SignatureScheme::ED25519, &[algs::ED25519]),
        // rustls allows only the PSS schemes in TLS 1.3 handshakes; the
        // PKCS#1 ones serve certificate chains.
        (SignatureScheme::RSA_PSS_SHA512, &[&RSA_PSS_SHA512]),
        (SignatureScheme::RSA_PSS_SHA384, &[&RSA_PSS_SHA384]),
        (SignatureScheme::RSA_PSS_SHA256, &[&RSA_PSS_SHA256]),
        (SignatureScheme::RSA_PKCS1_SHA512, &[&RSA_PKCS1_SHA512]),
        (SignatureScheme::RSA_PKCS1_SHA384, &[&RSA_PKCS1_SHA384]),
        (SignatureScheme::RSA_PKCS1_SHA256, &[&RSA_PKCS1_SHA256]),
    ],
};

// ---------------------------------------------------------------- signing

/// ring's key loading, with the [`crate::rsakey`] policy on RSA keys.
#[derive(Debug)]
struct Keys;

impl KeyProvider for Keys {
    fn load_private_key(&self, der: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        let rsa = match &der {
            PrivateKeyDer::Pkcs1(_) => true,
            PrivateKeyDer::Pkcs8(k) => rsakey::pkcs8_is_rsa(k.secret_pkcs8_der()),
            _ => false,
        };
        let key = r::default_provider()
            .key_provider
            .load_private_key(der)
            .map_err(|e| if rsa { rsa_refused(&e.to_string()) } else { e })?;
        if key.algorithm() == SignatureAlgorithm::RSA {
            let spki = key
                .public_key()
                .ok_or_else(|| rsa_refused("no public key"))?;
            rsakey::check_spki_der(spki.as_ref()).map_err(|e| {
                rsa_refused(match e {
                    Refused::Policy(why) => why,
                    Refused::Malformed => "malformed public key",
                })
            })?;
        }
        Ok(key)
    }
}

fn rsa_refused(why: &str) -> Error {
    Error::General(format!(
        "RSA private key refused ({why}): it must have 2048 to 4096 bits and an odd public \
         exponent from 65537 to 2^32 - 1, in PKCS#1 or PKCS#8"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rsakey::testing::{pkcs8_der, private_der, public_der};
    use rustls::NamedGroup;

    #[test]
    fn the_hybrid_comes_first_then_ring() {
        let p = provider();
        let names: Vec<_> = p.kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(
            names,
            [
                NamedGroup::X25519MLKEM768,
                NamedGroup::X25519,
                NamedGroup::secp256r1,
                NamedGroup::secp384r1
            ]
        );
        // The classical groups are ring's own.
        let ring = r::default_provider();
        let debug = |g: &[&dyn rustls::crypto::SupportedKxGroup]| -> Vec<String> {
            g.iter().map(|g| format!("{g:?}")).collect()
        };
        assert_eq!(debug(&p.kx_groups[1..]), debug(&ring.kx_groups));
        assert_ne!(
            debug(&p.kx_groups[1..2]),
            debug(&[&super::super::rustcrypto::X25519])
        );
        let suites: Vec<_> = p.cipher_suites.iter().map(|s| s.suite()).collect();
        let pure: Vec<_> = super::super::rustcrypto::provider()
            .cipher_suites
            .iter()
            .map(|s| s.suite())
            .collect();
        assert_eq!(suites, pure);
    }

    #[test]
    fn the_same_signature_schemes_as_the_pure_rust_provider() {
        let pure = super::super::rustcrypto::provider().signature_verification_algorithms;
        let ours = SIGNATURE_ALGORITHMS;
        assert_eq!(ours.supported_schemes(), pure.supported_schemes());
        let ids = |a: &WebPkiSupportedAlgorithms| -> Vec<_> {
            a.all
                .iter()
                .map(|x| (x.public_key_alg_id(), x.signature_alg_id()))
                .collect()
        };
        assert_eq!(ids(&ours), ids(&pure));
    }

    fn pss_sha256(k: &rsa::RsaPrivateKey, msg: &[u8]) -> Vec<u8> {
        use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _};
        rsa::pss::SigningKey::<sha2::Sha256>::new(k.clone())
            .sign_with_rng(&mut rsakey::testing::OsRng, msg)
            .to_vec()
    }

    #[test]
    fn rsa_verification_keeps_the_policy() {
        let msg = b"the transcript";
        let good = crate::webauthn::soft::rsa_key(7, 2048);
        let sig = pss_sha256(&good, msg);
        let public = public_der(&good.to_public_key());
        RSA_PSS_SHA256.verify_signature(&public, msg, &sig).unwrap();
        assert!(
            RSA_PSS_SHA256
                .verify_signature(&public, b"x", &sig)
                .is_err()
        );
        // ring alone would take these.
        let e3 = rsa_key_with_exp("e3", 3);
        let e3_public = public_der(&e3.to_public_key());
        let e3_sig = pss_sha256(&e3, msg);
        algs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY
            .verify_signature(&e3_public, msg, &e3_sig)
            .unwrap();
        assert!(
            RSA_PSS_SHA256
                .verify_signature(&e3_public, msg, &e3_sig)
                .is_err()
        );
        // A modulus over 4096 bits, which ring alone would take, is
        // refused by the policy.
        let mut n = vec![0xff; 513];
        n[0] = 1;
        let big = [
            &[0x30, 0x82, 0x02, 0x0a, 0x02, 0x82, 0x02, 0x01][..],
            &n,
            &[0x02, 0x03, 1, 0, 1],
        ]
        .concat();
        assert_eq!(
            rsakey::check_pkcs1_der(&big),
            Err(Refused::Policy(
                "an RSA modulus must have 2048 to 4096 bits"
            ))
        );
        // ring itself refuses a 1024-bit key.
        let small = crate::webauthn::soft::rsa_key(7, 1024);
        let small_public = public_der(&small.to_public_key());
        let small_sig = pss_sha256(&small, msg);
        assert!(
            algs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY
                .verify_signature(&small_public, msg, &small_sig)
                .is_err()
        );
        assert!(
            RSA_PSS_SHA256
                .verify_signature(&small_public, msg, &small_sig)
                .is_err()
        );
    }

    /// A 2048-bit key with the public exponent `e`, from a seed text.
    fn rsa_key_with_exp(seed: &str, e: u64) -> rsa::RsaPrivateKey {
        let mut h = blake3::Hasher::new_derive_key("zen/test/tls-ring-rsa");
        h.update(seed.as_bytes());
        rsa::RsaPrivateKey::new_with_exp(&mut Seeded(h.finalize_xof()), 2048, e.into())
            .expect("RSA key")
    }

    /// A deterministic RNG for RSA key generation.
    struct Seeded(blake3::OutputReader);

    impl rsa::rand_core::TryRng for Seeded {
        type Error = std::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            let mut b = [0; 4];
            self.0.fill(&mut b);
            Ok(u32::from_le_bytes(b))
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            let mut b = [0; 8];
            self.0.fill(&mut b);
            Ok(u64::from_le_bytes(b))
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            self.0.fill(dst);
            Ok(())
        }
    }

    impl rsa::rand_core::TryCryptoRng for Seeded {}

    #[test]
    fn rsa_server_keys_load_and_sign() {
        let k = crate::webauthn::soft::rsa_key(7, 2048);
        let public = public_der(&k.to_public_key());
        for der in [
            PrivateKeyDer::Pkcs1(private_der(&k).into()),
            PrivateKeyDer::Pkcs8(pkcs8_der(&k).into()),
        ] {
            let key = Keys.load_private_key(der).unwrap();
            assert_eq!(key.algorithm(), SignatureAlgorithm::RSA);
            // TLS 1.3 offers PSS only; the signature verifies.
            let signer = key
                .choose_scheme(&[SignatureScheme::RSA_PSS_SHA256])
                .unwrap();
            let sig = signer.sign(b"the transcript").unwrap();
            RSA_PSS_SHA256
                .verify_signature(&public, b"the transcript", &sig)
                .unwrap();
        }
    }

    #[test]
    fn rsa_server_keys_outside_the_policy_are_refused() {
        // Under 2048 bits: ring refuses it, and the error says why.
        let small = crate::webauthn::soft::rsa_key(7, 1024);
        let e = Keys
            .load_private_key(PrivateKeyDer::Pkcs8(pkcs8_der(&small).into()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("RSA private key refused"), "{e}");
        // An exponent over 2^32 - 1: ring takes it, the policy doesn't.
        let big_e = rsa_key_with_exp("big e", (1 << 32) + 15);
        let e = Keys
            .load_private_key(PrivateKeyDer::Pkcs1(private_der(&big_e).into()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("exponent"), "{e}");
        // Not RSA: ring's own error.
        let e = Keys
            .load_private_key(PrivateKeyDer::Pkcs8(vec![0x30, 0].into()))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("RSA private key"), "{e}");
    }
}
