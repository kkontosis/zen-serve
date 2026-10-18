//! TLS 1.2 (feature `tls12`): the ECDHE-ECDSA AEAD suites, for zen-svc's
//! TLS engine (svc.md §7.1). Only ECDSA suites, since this provider signs
//! with ECDSA and Ed25519 but not RSA (`TD-TLS-RSA-SERVER-KEY`).
//!
//! * AES-GCM's explicit nonce is a random 32-bit prefix, drawn when the
//!   encrypter is made, followed by the low 32 bits of the sequence number
//!   (the [`record`] IV `salt ‖ prefix ‖ 0⁴` XOR the sequence number).
//! * ChaCha20-Poly1305 follows RFC 7905.
//!
//! Servers should set `require_ems`: the suites don't require the
//! extended master secret themselves.

use super::record::{self, EXPLICIT_NONCE_LEN, RecordKey, TAG_LEN};
use super::{
    HmacSha256, HmacSha384, SHA256, SHA384, provider, record_error, record_key, traffic_secrets,
};
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, KeyBlockShape, MessageDecrypter,
    MessageEncrypter, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload,
    Tls12AeadAlgorithm, UnsupportedOperationError, make_tls12_aad,
};
use rustls::crypto::tls12::PrfUsingHmac;
use rustls::crypto::{CipherSuiteCommon, CryptoProvider, KeyExchangeAlgorithm};
use rustls::{
    CipherSuite, ConnectionTrafficSecrets, Error, SignatureScheme, SupportedCipherSuite,
    Tls12CipherSuite,
};

/// [`provider`] with the TLS 1.2 suites after the 1.3 ones.
pub fn provider_tls12() -> CryptoProvider {
    let mut p = provider();
    p.cipher_suites.extend([
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ]);
    p
}

static ECDSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::ED25519,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ECDSA_NISTP256_SHA256,
];

/// The TLS 1.2 suite `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`.
pub static TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls12(&Tls12CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: 1 << 24,
        },
        prf_provider: &PrfUsingHmac(&HmacSha256),
        kx: KeyExchangeAlgorithm::ECDHE,
        sign: ECDSA_SCHEMES,
        aead_alg: &Aead12(record::Aead::Aes128Gcm),
    });

/// The TLS 1.2 suite `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`.
pub static TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384: SupportedCipherSuite =
    SupportedCipherSuite::Tls12(&Tls12CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            hash_provider: &SHA384,
            confidentiality_limit: 1 << 24,
        },
        prf_provider: &PrfUsingHmac(&HmacSha384),
        kx: KeyExchangeAlgorithm::ECDHE,
        sign: ECDSA_SCHEMES,
        aead_alg: &Aead12(record::Aead::Aes256Gcm),
    });

/// The TLS 1.2 suite `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`.
pub static TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls12(&Tls12CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: u64::MAX,
        },
        prf_provider: &PrfUsingHmac(&HmacSha256),
        kx: KeyExchangeAlgorithm::ECDHE,
        sign: ECDSA_SCHEMES,
        aead_alg: &Aead12(record::Aead::Chacha20Poly1305),
    });

struct Aead12(record::Aead);

impl Aead12 {
    fn gcm(&self) -> bool {
        self.0 != record::Aead::Chacha20Poly1305
    }
}

/// A 12-byte IV from the key block's 4-byte salt and 8 more bytes.
fn salted(salt: &[u8], rest: &[u8]) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(salt);
    iv[4..].copy_from_slice(rest);
    iv
}

impl Tls12AeadAlgorithm for Aead12 {
    fn encrypter(&self, key: AeadKey, iv: &[u8], _extra: &[u8]) -> Box<dyn MessageEncrypter> {
        let iv = if self.gcm() {
            let mut prefix = [0u8; 8];
            // A failed RNG leaves the prefix zero; the explicit nonce is
            // still unique within the connection (the sequence number).
            let _ = getrandom::fill(&mut prefix[..4]);
            salted(iv, &prefix)
        } else {
            iv.try_into().expect("a 12-byte IV")
        };
        Box::new(Sealer12(record_key(self.0, &key, &iv)))
    }

    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        let iv = if self.gcm() {
            salted(iv, &[0; 8])
        } else {
            iv.try_into().expect("a 12-byte IV")
        };
        Box::new(Opener12(record_key(self.0, &key, &iv)))
    }

    fn key_block_shape(&self) -> KeyBlockShape {
        if self.gcm() {
            KeyBlockShape {
                enc_key_len: self.0.key_len(),
                fixed_iv_len: 4,
                explicit_nonce_len: EXPLICIT_NONCE_LEN,
            }
        } else {
            KeyBlockShape {
                enc_key_len: 32,
                fixed_iv_len: 12,
                explicit_nonce_len: 0,
            }
        }
    }

    /// For AES-GCM the IV is the salt followed by the key block's 8 extra
    /// bytes, as rustls's own providers give it; a sender picks its own
    /// explicit nonces, so only the salt matters.
    fn extract_keys(
        &self,
        key: AeadKey,
        iv: &[u8],
        explicit: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        let iv = if self.gcm() {
            salted(iv, explicit)
        } else {
            iv.try_into().map_err(|_| UnsupportedOperationError)?
        };
        Ok(traffic_secrets(self.0, key, Iv::from(iv)))
    }
}

struct Sealer12(RecordKey);

struct Opener12(RecordKey);

impl Sealer12 {
    fn explicit_len(&self) -> usize {
        if self.0.aead() == record::Aead::Chacha20Poly1305 {
            0
        } else {
            EXPLICIT_NONCE_LEN
        }
    }
}

impl MessageEncrypter for Sealer12 {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let explicit = self.explicit_len();
        let mut payload =
            PrefixedPayload::with_capacity(self.encrypted_payload_len(msg.payload.len()));
        let nonce = self.0.nonce(seq);
        payload.extend_from_slice(&nonce[12 - explicit..]);
        payload.extend_from_chunks(&msg.payload);
        let aad = make_tls12_aad(seq, msg.typ, msg.version, msg.payload.len());
        let tag = self
            .0
            .seal_detached(&nonce, &aad, &mut payload.as_mut()[explicit..]);
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(msg.typ, msg.version, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        self.explicit_len() + payload_len + TAG_LEN
    }
}

impl MessageDecrypter for Opener12 {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let range = self
            .0
            .open12_in_place(
                seq,
                u8::from(msg.typ),
                msg.version.to_array(),
                &mut msg.payload,
            )
            .map_err(record_error)?;
        Ok(msg.into_plain_message_range(range))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::ContentType;
    use rustls::ProtocolVersion;
    use rustls::crypto::cipher::OutboundChunks;

    /// rustls's 1.2 encrypters and the record API agree both ways, and AES-GCM's
    /// explicit nonce is a fixed prefix followed by the sequence number.
    /// (Only 32-byte keys: rustls builds 16-byte `AeadKey`s itself.)
    #[test]
    fn records_agree_with_the_record_api() {
        for aead in [record::Aead::Aes256Gcm, record::Aead::Chacha20Poly1305] {
            let alg = Aead12(aead);
            let shape = alg.key_block_shape();
            let key = AeadKey::from([5u8; 32]);
            let salt = vec![6u8; shape.fixed_iv_len];
            let mut enc = alg.encrypter(key, &salt, &[0; 8][..shape.explicit_nonce_len]);
            let rx_iv = if alg.gcm() {
                salted(&salt, &[0; 8])
            } else {
                salt.clone().try_into().unwrap()
            };
            let rx = RecordKey::new(aead, &[5; 32], &rx_iv).unwrap();
            let mut prefixes = Vec::new();
            for seq in [0u64, 1, (1 << 24) - 1] {
                let msg = OutboundPlainMessage {
                    typ: ContentType::ApplicationData,
                    version: ProtocolVersion::TLSv1_2,
                    payload: OutboundChunks::Single(b"twelve"),
                };
                let bytes = enc.encrypt(msg, seq).unwrap().encode();
                assert_eq!(rx.open12(seq, &bytes).unwrap(), b"twelve");
                if alg.gcm() {
                    prefixes.push(bytes[5..9].to_vec());
                    assert_eq!(&bytes[9..13], &(seq as u32).to_be_bytes());
                }
                let mut dec = alg.decrypter(AeadKey::from([5u8; 32]), &salt);
                let mut sealed = rx.seal12(seq, record::APPLICATION_DATA, b"back")[5..].to_vec();
                let got = dec
                    .decrypt(
                        InboundOpaqueMessage::new(
                            ContentType::ApplicationData,
                            ProtocolVersion::TLSv1_2,
                            &mut sealed,
                        ),
                        seq,
                    )
                    .unwrap();
                assert_eq!(got.payload, b"back");
            }
            prefixes.dedup();
            assert!(prefixes.len() <= 1);
        }
    }
}
