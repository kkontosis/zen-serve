//! A rustls [`CryptoProvider`] on RustCrypto: pure Rust, no C or assembly
//! files, the same crate generation as the rest of the server.
//!
//! * **TLS 1.3 only**, with `TLS_AES_128_GCM_SHA256`,
//!   `TLS_AES_256_GCM_SHA384` and `TLS_CHACHA20_POLY1305_SHA256`.
//! * **Key exchange**, preferred first: the post-quantum hybrid
//!   `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem), then `X25519` and
//!   `secp256r1`.
//! * **Signatures** (certificates and handshakes): ECDSA on P-256 and
//!   P-384, and Ed25519. RSA is not supported (`TD-TLS-RSA`).
//! * **Private keys**: ECDSA P-256 or P-384 (PKCS#8 or SEC1), or Ed25519
//!   (PKCS#8).
//!
//! The glue follows rustls's own providers; the primitives are RustCrypto's.

use aes_gcm::aead::{AeadInOut, KeyInit};
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, MessageDecrypter, MessageEncrypter,
    Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls13AeadAlgorithm,
    UnsupportedOperationError, make_tls13_aad,
};
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::crypto::{
    ActiveKeyExchange, CipherSuiteCommon, CompletedKeyExchange, CryptoProvider, GetRandomFailed,
    KeyProvider, SecureRandom, SharedSecret, SupportedKxGroup, WebPkiSupportedAlgorithms, hash,
    hmac,
};
use rustls::pki_types::{
    AlgorithmIdentifier, InvalidSignature, PrivateKeyDer, SignatureVerificationAlgorithm,
    SubjectPublicKeyInfoDer, alg_id,
};
use rustls::sign::{Signer, SigningKey, public_key_to_spki};
use rustls::{
    CipherSuite, ConnectionTrafficSecrets, ContentType, Error, NamedGroup, PeerMisbehaved,
    ProtocolVersion, SignatureAlgorithm, SignatureScheme, SupportedCipherSuite, Tls13CipherSuite,
};
use sha2::{Digest, Sha256, Sha384};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

/// The provider.
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![
            TLS13_AES_128_GCM_SHA256,
            TLS13_AES_256_GCM_SHA384,
            TLS13_CHACHA20_POLY1305_SHA256,
        ],
        kx_groups: vec![&X25519MLKEM768, &X25519, &SECP256R1],
        signature_verification_algorithms: SIGNATURE_ALGORITHMS,
        secure_random: &Random,
        key_provider: &Keys,
    }
}

/// Make [`provider`] the process-wide default of rustls, for code that
/// builds TLS configurations without naming one (HTTP clients in tests and
/// tools). Does nothing if a default is installed already.
pub fn install_default() {
    let _ = provider().install_default();
}

fn random(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|_| Error::FailedToGetRandomBytes)
}

#[derive(Debug)]
struct Random;

impl SecureRandom for Random {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        getrandom::fill(buf).map_err(|_| GetRandomFailed)
    }
}

// ---------------------------------------------------------------- hashes

struct HashAlg<D>(hash::HashAlgorithm, PhantomData<fn() -> D>);

struct HashCtx<D>(D);

impl<D: Digest + Clone + Send + Sync + 'static> hash::Hash for HashAlg<D> {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(HashCtx(D::new()))
    }

    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&D::digest(data))
    }

    fn output_len(&self) -> usize {
        <D as Digest>::output_size()
    }

    fn algorithm(&self) -> hash::HashAlgorithm {
        self.0
    }
}

impl<D: Digest + Clone + Send + Sync + 'static> hash::Context for HashCtx<D> {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize())
    }

    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(HashCtx(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

static SHA256: HashAlg<Sha256> = HashAlg(hash::HashAlgorithm::SHA256, PhantomData);
static SHA384: HashAlg<Sha384> = HashAlg(hash::HashAlgorithm::SHA384, PhantomData);

// ---------------------------------------------------------------- HMAC

macro_rules! hmac_alg {
    ($alg:ident, $key:ident, $digest:ty) => {
        struct $alg;

        struct $key(::hmac::Hmac<$digest>);

        impl hmac::Hmac for $alg {
            fn with_key(&self, key: &[u8]) -> Box<dyn hmac::Key> {
                use ::hmac::KeyInit as _;
                Box::new($key(
                    ::hmac::Hmac::<$digest>::new_from_slice(key).expect("HMAC takes any key"),
                ))
            }

            fn hash_output_len(&self) -> usize {
                <$digest as Digest>::output_size()
            }
        }

        impl hmac::Key for $key {
            fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> hmac::Tag {
                use ::hmac::Mac as _;
                let mut m = self.0.clone();
                m.update(first);
                for x in middle {
                    m.update(x);
                }
                m.update(last);
                hmac::Tag::new(&m.finalize().into_bytes())
            }

            fn tag_len(&self) -> usize {
                <$digest as Digest>::output_size()
            }
        }
    };
}

hmac_alg!(HmacSha256, HmacSha256Key, Sha256);
hmac_alg!(HmacSha384, HmacSha384Key, Sha384);

// ---------------------------------------------------------------- AEADs

/// The TLS 1.3 suite `TLS_AES_128_GCM_SHA256`.
pub static TLS13_AES_128_GCM_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: 1 << 24,
        },
        hkdf_provider: &HkdfUsingHmac(&HmacSha256),
        aead_alg: &Aead::<aes_gcm::Aes128Gcm>(Kind::Aes128Gcm, PhantomData),
        quic: None,
    });

/// The TLS 1.3 suite `TLS_AES_256_GCM_SHA384`.
pub static TLS13_AES_256_GCM_SHA384: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_256_GCM_SHA384,
            hash_provider: &SHA384,
            confidentiality_limit: 1 << 24,
        },
        hkdf_provider: &HkdfUsingHmac(&HmacSha384),
        aead_alg: &Aead::<aes_gcm::Aes256Gcm>(Kind::Aes256Gcm, PhantomData),
        quic: None,
    });

/// The TLS 1.3 suite `TLS_CHACHA20_POLY1305_SHA256`.
pub static TLS13_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: u64::MAX,
        },
        hkdf_provider: &HkdfUsingHmac(&HmacSha256),
        aead_alg: &Aead::<chacha20poly1305::ChaCha20Poly1305>(Kind::Chacha20Poly1305, PhantomData),
        quic: None,
    });

#[derive(Clone, Copy)]
enum Kind {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
}

/// All three AEADs have 12-byte nonces and 16-byte tags.
const TAG_LEN: usize = 16;

struct Aead<A>(Kind, PhantomData<fn() -> A>);

impl<A: KeyInit + AeadInOut + Send + Sync + 'static> Tls13AeadAlgorithm for Aead<A> {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Sealer {
            key: A::new_from_slice(key.as_ref()).expect("rustls passes key_len() bytes"),
            iv,
        })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Opener {
            key: A::new_from_slice(key.as_ref()).expect("rustls passes key_len() bytes"),
            iv,
        })
    }

    fn key_len(&self) -> usize {
        A::key_size()
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(match self.0 {
            Kind::Aes128Gcm => ConnectionTrafficSecrets::Aes128Gcm { key, iv },
            Kind::Aes256Gcm => ConnectionTrafficSecrets::Aes256Gcm { key, iv },
            Kind::Chacha20Poly1305 => ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv },
        })
    }
}

struct Sealer<A> {
    key: A,
    iv: Iv,
}

struct Opener<A> {
    key: A,
    iv: Iv,
}

fn nonce<A: AeadInOut>(iv: &Iv, seq: u64) -> aes_gcm::aead::Nonce<A> {
    aes_gcm::aead::Nonce::<A>::try_from(&Nonce::new(iv, seq).0[..]).expect("12-byte nonces")
}

impl<A: AeadInOut + Send + Sync> MessageEncrypter for Sealer<A> {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let aad = make_tls13_aad(total_len);
        let tag = self
            .key
            .encrypt_inout_detached(&nonce::<A>(&self.iv, seq), &aad, payload.as_mut().into())
            .map_err(|_| Error::EncryptError)?;
        payload.extend_from_slice(&tag);
        // TLS 1.3 records carry the legacy version 0x0303 (RFC 8446 §5.1).
        Ok(OutboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            payload,
        ))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

impl<A: AeadInOut + Send + Sync> MessageDecrypter for Opener<A> {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut msg.payload;
        let len = payload.len();
        let plain_len = len.checked_sub(TAG_LEN).ok_or(Error::DecryptError)?;
        let aad = make_tls13_aad(len);
        let (body, tag) = payload.split_at_mut(plain_len);
        let tag = aes_gcm::aead::Tag::<A>::try_from(&*tag).map_err(|_| Error::DecryptError)?;
        self.key
            .decrypt_inout_detached(&nonce::<A>(&self.iv, seq), &aad, body.into(), &tag)
            .map_err(|_| Error::DecryptError)?;
        payload.truncate(plain_len);
        msg.into_tls13_unpadded_message()
    }
}

// ---------------------------------------------------------------- key exchange

const INVALID_SHARE: Error = Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare);

/// X25519 (RFC 7748).
#[derive(Debug)]
pub struct X25519Group;

/// The `X25519` group.
pub static X25519: X25519Group = X25519Group;

struct X25519Active {
    secret: x25519_dalek::StaticSecret,
    public: [u8; 32],
}

impl X25519Active {
    fn new() -> Result<Self, Error> {
        let mut b = [0u8; 32];
        random(&mut b)?;
        let secret = x25519_dalek::StaticSecret::from(b);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        Ok(X25519Active { secret, public })
    }

    fn agree(&self, peer: &[u8]) -> Result<[u8; 32], Error> {
        let peer: [u8; 32] = peer.try_into().map_err(|_| INVALID_SHARE)?;
        let ss = self
            .secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(peer));
        // RFC 8446 §7.4.2: an all-zero result must be refused.
        if !ss.was_contributory() {
            return Err(INVALID_SHARE);
        }
        Ok(ss.to_bytes())
    }
}

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        Ok(Box::new(X25519Active::new()?))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

impl ActiveKeyExchange for X25519Active {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        Ok(SharedSecret::from(&self.agree(peer)?[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

/// ECDHE on P-256.
#[derive(Debug)]
pub struct Secp256r1Group;

/// The `secp256r1` group.
pub static SECP256R1: Secp256r1Group = Secp256r1Group;

struct Secp256r1Active {
    secret: p256::NonZeroScalar,
    public: Vec<u8>,
}

impl SupportedKxGroup for Secp256r1Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let secret = loop {
            let mut b = [0u8; 32];
            random(&mut b)?;
            if let Ok(k) = p256::SecretKey::from_slice(&b) {
                break k;
            }
        };
        use p256::elliptic_curve::sec1::ToSec1Point as _;
        let public = secret.public_key().to_sec1_point(false).as_bytes().to_vec();
        Ok(Box::new(Secp256r1Active {
            secret: secret.to_nonzero_scalar(),
            public,
        }))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }
}

impl ActiveKeyExchange for Secp256r1Active {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        // Only uncompressed points (RFC 8446 §4.2.8.2).
        if peer.first() != Some(&4) {
            return Err(INVALID_SHARE);
        }
        let peer = p256::PublicKey::from_sec1_bytes(peer).map_err(|_| INVALID_SHARE)?;
        let ss = p256::ecdh::diffie_hellman(self.secret, peer.as_affine());
        Ok(SharedSecret::from(&ss.raw_secret_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }
}

/// The post-quantum hybrid `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem):
/// the client share is the ML-KEM-768 encapsulation key ‖ an X25519 key,
/// the server share the ML-KEM ciphertext ‖ an X25519 key, and the secret
/// the ML-KEM shared key ‖ the X25519 shared secret.
#[derive(Debug)]
pub struct X25519MlKem768Group;

/// The `X25519MLKEM768` group.
pub static X25519MLKEM768: X25519MlKem768Group = X25519MlKem768Group;

const MLKEM768_EK: usize = 1184;
const MLKEM768_CT: usize = 1088;

struct HybridActive {
    dk: ml_kem::ml_kem_768::DecapsulationKey,
    x25519: X25519Active,
    public: Vec<u8>,
}

impl SupportedKxGroup for X25519MlKem768Group {
    /// The client side: a fresh ML-KEM key pair and an X25519 key.
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        use ml_kem::{FromSeed as _, KeyExport as _};
        let mut seed = ml_kem::Seed::default();
        random(&mut seed)?;
        let (dk, ek) = ml_kem::MlKem768::from_seed(&seed);
        let x25519 = X25519Active::new()?;
        let mut public = ek.to_bytes().to_vec();
        public.extend_from_slice(&x25519.public);
        Ok(Box::new(HybridActive { dk, x25519, public }))
    }

    /// The server side: encapsulate to the client's ML-KEM key, and answer
    /// its X25519 share.
    fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
        if client_share.len() != MLKEM768_EK + 32 {
            return Err(INVALID_SHARE);
        }
        let (ek, x) = client_share.split_at(MLKEM768_EK);
        let ek =
            ml_kem::ml_kem_768::EncapsulationKey::new(&ek.try_into().map_err(|_| INVALID_SHARE)?)
                .map_err(|_| INVALID_SHARE)?;
        let mut m = ml_kem::B32::default();
        random(&mut m)?;
        let (ct, ss_m) = ek.encapsulate_deterministic(&m);
        let x25519 = X25519Active::new()?;
        let ss_x = x25519.agree(x)?;
        let mut pub_key = ct.to_vec();
        pub_key.extend_from_slice(&x25519.public);
        let mut secret = ss_m.to_vec();
        secret.extend_from_slice(&ss_x);
        Ok(CompletedKeyExchange {
            group: NamedGroup::X25519MLKEM768,
            pub_key,
            secret: SharedSecret::from(secret),
        })
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }

    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        None
    }

    fn usable_for_version(&self, version: ProtocolVersion) -> bool {
        version == ProtocolVersion::TLSv1_3
    }
}

impl ActiveKeyExchange for HybridActive {
    /// The client side: decapsulate the server's ciphertext.
    fn complete(self: Box<Self>, server_share: &[u8]) -> Result<SharedSecret, Error> {
        use ml_kem::Decapsulate as _;
        if server_share.len() != MLKEM768_CT + 32 {
            return Err(INVALID_SHARE);
        }
        let (ct, x) = server_share.split_at(MLKEM768_CT);
        let ct: ml_kem::ml_kem_768::Ciphertext = ct.try_into().map_err(|_| INVALID_SHARE)?;
        let ss_m = self.dk.decapsulate(&ct);
        let ss_x = self.x25519.agree(x)?;
        let mut secret = ss_m.to_vec();
        secret.extend_from_slice(&ss_x);
        Ok(SharedSecret::from(secret))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }
}

// ---------------------------------------------------------------- verification

#[derive(Clone, Copy, Debug)]
enum Curve {
    P256,
    P384,
}

#[derive(Clone, Copy, Debug)]
enum HashKind {
    Sha256,
    Sha384,
}

#[derive(Debug)]
struct EcdsaVerify {
    curve: Curve,
    hash: HashKind,
}

impl SignatureVerificationAlgorithm for EcdsaVerify {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        use ecdsa::signature::hazmat::PrehashVerifier as _;
        let digest = match self.hash {
            HashKind::Sha256 => Sha256::digest(message).to_vec(),
            HashKind::Sha384 => Sha384::digest(message).to_vec(),
        };
        match self.curve {
            Curve::P256 => {
                let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
                    .map_err(|_| InvalidSignature)?;
                let sig =
                    p256::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                vk.verify_prehash(&digest, &sig)
            }
            Curve::P384 => {
                let vk = p384::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
                    .map_err(|_| InvalidSignature)?;
                let sig =
                    p384::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                vk.verify_prehash(&digest, &sig)
            }
        }
        .map_err(|_| InvalidSignature)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        match self.curve {
            Curve::P256 => alg_id::ECDSA_P256,
            Curve::P384 => alg_id::ECDSA_P384,
        }
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        match self.hash {
            HashKind::Sha256 => alg_id::ECDSA_SHA256,
            HashKind::Sha384 => alg_id::ECDSA_SHA384,
        }
    }
}

#[derive(Debug)]
struct Ed25519Verify;

impl SignatureVerificationAlgorithm for Ed25519Verify {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        let pk: &[u8; 32] = public_key.try_into().map_err(|_| InvalidSignature)?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(pk).map_err(|_| InvalidSignature)?;
        let sig = ed25519_dalek::Signature::from_slice(signature).map_err(|_| InvalidSignature)?;
        vk.verify_strict(message, &sig)
            .map_err(|_| InvalidSignature)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED25519
    }
}

static ECDSA_P256_SHA256: EcdsaVerify = EcdsaVerify {
    curve: Curve::P256,
    hash: HashKind::Sha256,
};
static ECDSA_P256_SHA384: EcdsaVerify = EcdsaVerify {
    curve: Curve::P256,
    hash: HashKind::Sha384,
};
static ECDSA_P384_SHA256: EcdsaVerify = EcdsaVerify {
    curve: Curve::P384,
    hash: HashKind::Sha256,
};
static ECDSA_P384_SHA384: EcdsaVerify = EcdsaVerify {
    curve: Curve::P384,
    hash: HashKind::Sha384,
};
static ED25519: Ed25519Verify = Ed25519Verify;

/// What certificates and handshake signatures may use. In TLS 1.3 a
/// signature scheme fixes the curve; certificates may mix them.
static SIGNATURE_ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        &ECDSA_P256_SHA256,
        &ECDSA_P256_SHA384,
        &ECDSA_P384_SHA256,
        &ECDSA_P384_SHA384,
        &ED25519,
    ],
    mapping: &[
        (
            SignatureScheme::ECDSA_NISTP384_SHA384,
            &[&ECDSA_P384_SHA384, &ECDSA_P256_SHA384],
        ),
        (
            SignatureScheme::ECDSA_NISTP256_SHA256,
            &[&ECDSA_P256_SHA256, &ECDSA_P384_SHA256],
        ),
        (SignatureScheme::ED25519, &[&ED25519]),
    ],
};

// ---------------------------------------------------------------- signing

#[derive(Debug)]
struct Keys;

impl KeyProvider for Keys {
    fn load_private_key(&self, der: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        use p256::pkcs8::DecodePrivateKey as _;
        let key = match &der {
            PrivateKeyDer::Pkcs8(k) => {
                let k = k.secret_pkcs8_der();
                if let Ok(s) = p256::ecdsa::SigningKey::from_pkcs8_der(k) {
                    Key::P256(s)
                } else if let Ok(s) = p384::ecdsa::SigningKey::from_pkcs8_der(k) {
                    Key::P384(s)
                } else if let Ok(s) = ed25519_dalek::SigningKey::from_pkcs8_der(k) {
                    Key::Ed25519(s)
                } else {
                    return Err(unsupported_key());
                }
            }
            PrivateKeyDer::Sec1(k) => {
                let k = k.secret_sec1_der();
                if let Ok(s) = p256::SecretKey::from_sec1_der(k) {
                    Key::P256(s.into())
                } else if let Ok(s) = p384::SecretKey::from_sec1_der(k) {
                    Key::P384(s.into())
                } else {
                    return Err(unsupported_key());
                }
            }
            _ => return Err(unsupported_key()),
        };
        Ok(Arc::new(key))
    }
}

fn unsupported_key() -> Error {
    Error::General(
        "unsupported private key: use ECDSA P-256 or P-384 (PKCS#8 or SEC1) or Ed25519 (PKCS#8)"
            .into(),
    )
}

#[derive(Clone)]
enum Key {
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    Ed25519(ed25519_dalek::SigningKey),
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Key::P256(_) => "Key::P256",
            Key::P384(_) => "Key::P384",
            Key::Ed25519(_) => "Key::Ed25519",
        })
    }
}

impl Key {
    fn scheme(&self) -> SignatureScheme {
        match self {
            Key::P256(_) => SignatureScheme::ECDSA_NISTP256_SHA256,
            Key::P384(_) => SignatureScheme::ECDSA_NISTP384_SHA384,
            Key::Ed25519(_) => SignatureScheme::ED25519,
        }
    }
}

impl SigningKey for Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&self.scheme())
            .then(|| Box::new(self.clone()) as Box<dyn Signer>)
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(match self {
            Key::P256(k) => public_key_to_spki(
                &alg_id::ECDSA_P256,
                k.verifying_key().to_sec1_point(false).as_bytes(),
            ),
            Key::P384(k) => public_key_to_spki(
                &alg_id::ECDSA_P384,
                k.verifying_key().to_sec1_point(false).as_bytes(),
            ),
            Key::Ed25519(k) => public_key_to_spki(&alg_id::ED25519, k.verifying_key().as_bytes()),
        })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        match self {
            Key::P256(_) | Key::P384(_) => SignatureAlgorithm::ECDSA,
            Key::Ed25519(_) => SignatureAlgorithm::ED25519,
        }
    }
}

impl Signer for Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        use ecdsa::signature::Signer as _;
        Ok(match self {
            Key::P256(k) => {
                let s: p256::ecdsa::Signature = k.sign(message);
                s.to_der().as_bytes().to_vec()
            }
            Key::P384(k) => {
                let s: p384::ecdsa::Signature = k.sign(message);
                s.to_der().as_bytes().to_vec()
            }
            Key::Ed25519(k) => {
                use ed25519_dalek::Signer as _;
                k.sign(message).to_bytes().to_vec()
            }
        })
    }

    fn scheme(&self) -> SignatureScheme {
        Key::scheme(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_key_exchange_agrees() {
        let client = X25519MLKEM768.start().unwrap();
        assert_eq!(client.pub_key().len(), MLKEM768_EK + 32);
        let server = X25519MLKEM768.start_and_complete(client.pub_key()).unwrap();
        assert_eq!(server.pub_key.len(), MLKEM768_CT + 32);
        let ours = client.complete(&server.pub_key).unwrap();
        assert_eq!(ours.secret_bytes(), server.secret.secret_bytes());
        assert_eq!(ours.secret_bytes().len(), 64);
        assert!(X25519MLKEM768.start_and_complete(&[0; 32]).is_err());
    }

    #[test]
    fn classical_key_exchanges_agree() {
        for g in [&X25519 as &dyn SupportedKxGroup, &SECP256R1] {
            let a = g.start().unwrap();
            let b = g.start_and_complete(a.pub_key()).unwrap();
            assert_eq!(
                a.complete(&b.pub_key).unwrap().secret_bytes(),
                b.secret.secret_bytes()
            );
        }
        // A low-order X25519 point gives an all-zero secret.
        assert!(X25519.start().unwrap().complete(&[0; 32]).is_err());
    }

    fn round_trip<A: KeyInit + AeadInOut + Send + Sync + 'static>() {
        use rustls::crypto::cipher::OutboundChunks;
        let key = || A::new_from_slice(&[7u8; 32][..A::key_size()]).unwrap();
        let iv = || Iv::new([9; 12]);
        let mut enc = Sealer {
            key: key(),
            iv: iv(),
        };
        let mut dec = Opener {
            key: key(),
            iv: iv(),
        };
        let plain = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_3,
            payload: OutboundChunks::Single(b"hello"),
        };
        let mut bytes = enc.encrypt(plain, 3).unwrap().encode();
        assert_eq!(bytes.len(), 5 + 5 + 1 + TAG_LEN);
        let mut tampered = bytes.clone();
        let msg = |b: &mut Vec<u8>| -> Vec<u8> { b[5..].to_vec() };
        let mut body = msg(&mut bytes);
        let got = dec
            .decrypt(
                InboundOpaqueMessage::new(
                    ContentType::ApplicationData,
                    ProtocolVersion::TLSv1_2,
                    &mut body,
                ),
                3,
            )
            .unwrap();
        assert_eq!(got.payload, b"hello");
        assert_eq!(got.typ, ContentType::ApplicationData);
        tampered[7] ^= 1;
        let mut body = msg(&mut tampered);
        let mut dec = Opener {
            key: key(),
            iv: iv(),
        };
        let r = dec.decrypt(
            InboundOpaqueMessage::new(
                ContentType::ApplicationData,
                ProtocolVersion::TLSv1_2,
                &mut body,
            ),
            3,
        );
        assert!(r.is_err());
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hmac_and_hkdf_known_answers() {
        use rustls::crypto::hmac::Hmac as _;
        use rustls::crypto::tls13::Hkdf as _;
        // RFC 4231, test case 2.
        let tag = HmacSha256
            .with_key(b"Jefe")
            .sign(&[b"what do ya want for nothing?"]);
        assert_eq!(
            tag.as_ref(),
            unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        let tag = HmacSha384
            .with_key(b"Jefe")
            .sign(&[b"what do ya ", b"want for nothing?"]);
        assert_eq!(
            tag.as_ref(),
            unhex(
                "af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e\
                 8e2240ca5e69e2c78b3239ecfab21649"
            )
        );
        // RFC 5869, test case 1.
        let ikm = [0x0b; 22];
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        let mut okm = [0u8; 42];
        HkdfUsingHmac(&HmacSha256)
            .extract_from_secret(Some(&salt), &ikm)
            .expand_slice(&[&info], &mut okm)
            .unwrap();
        assert_eq!(
            okm.to_vec(),
            unhex(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
                 34007208d5b887185865"
            )
        );
    }

    #[test]
    fn aeads_round_trip() {
        round_trip::<aes_gcm::Aes128Gcm>();
        round_trip::<aes_gcm::Aes256Gcm>();
        round_trip::<chacha20poly1305::ChaCha20Poly1305>();
    }
}
