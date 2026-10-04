//! Hybrid signatures (Ed25519 + ML-DSA-65), identities and device certificates
//! (spec/formats.md §6–§7, GAPS G1).
//!
//! A message is signed as `SM = "zen/v1/sig" || 0x00 || lp(purpose) || msg`.
//! Both halves sign `SM` deterministically. **Both** must verify.

use crate::encoding::{Reader, lp};
use crate::kdf::{Key32, fingerprint, kdf};
use crate::labels;
use crate::rng::{self, Rng};
use crate::suite::{FORMAT_VERSION, Suite, check_prefix};
use crate::{Error, Result};
use ed25519_dalek::Signer as _;
use ml_dsa::signature::Keypair as _;
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, MlDsa65};
use x_wing::{Decapsulator, KeyExport};
use zeroize::Zeroizing;

/// Ed25519 signature length.
pub const ED25519_SIG_LEN: usize = 64;
/// ML-DSA-65 signature length.
pub const ML_DSA_65_SIG_LEN: usize = 3309;
/// ML-DSA-65 verifying key length.
pub const ML_DSA_65_VK_LEN: usize = 1952;
/// Encoded hybrid signature: `format || suite || ed25519[64] || ml-dsa-65[3309]`.
pub const SIGNATURE_LEN: usize = 2 + ED25519_SIG_LEN + ML_DSA_65_SIG_LEN;
/// Encoded public identity: `format || suite || ed25519_pk[32] || ml-dsa-65_vk[1952]`.
pub const PUBLIC_IDENTITY_LEN: usize = 2 + 32 + ML_DSA_65_VK_LEN;
/// X-Wing encapsulation key length.
pub const XWING_EK_LEN: usize = x_wing::ENCAPSULATION_KEY_SIZE;
/// Encoded device public key: `public_identity || xwing_ek[1216]`.
pub const DEVICE_PUBLIC_LEN: usize = PUBLIC_IDENTITY_LEN + XWING_EK_LEN;

fn signed_message(purpose: &str, msg: &[u8]) -> Result<Vec<u8>> {
    if !labels::SIG_PURPOSES.contains(&purpose) {
        return Err(Error::Param);
    }
    let mut sm = Vec::with_capacity(labels::SIG_DOMAIN.len() + 5 + purpose.len() + msg.len());
    sm.extend_from_slice(labels::SIG_DOMAIN.as_bytes());
    sm.push(0);
    lp(&mut sm, purpose.as_bytes());
    sm.extend_from_slice(msg);
    Ok(sm)
}

/// A hybrid signing identity (a user identity, or a device's signing half),
/// derived from one 32-byte seed.
pub struct SigningIdentity {
    ed: ed25519_dalek::SigningKey,
    ml: ml_dsa::SigningKey<MlDsa65>,
}

impl SigningIdentity {
    /// Derive from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let ed_seed = kdf(labels::SIG_ED25519, seed, &[]);
        let ml_seed = kdf(labels::SIG_ML_DSA_65, seed, &[]);
        SigningIdentity {
            ed: ed25519_dalek::SigningKey::from_bytes(&ed_seed),
            ml: ml_dsa::SigningKey::<MlDsa65>::from_seed(&(*ml_seed).into()),
        }
    }

    /// Generate a fresh identity. Returns the seed, which must be stored securely.
    pub fn generate(rng: &mut dyn Rng) -> Result<(Self, Key32)> {
        let seed = Zeroizing::new(rng::array::<32>(rng)?);
        Ok((Self::from_seed(&seed), seed))
    }

    /// The public half.
    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            ed: self.ed.verifying_key(),
            ml: self.ml.verifying_key(),
        }
    }

    /// Sign `msg` for `purpose` (one of [`labels::SIG_PURPOSES`]).
    pub fn sign(&self, purpose: &str, msg: &[u8]) -> Result<Vec<u8>> {
        let sm = signed_message(purpose, msg)?;
        let ed = self.ed.sign(&sm);
        let ml = self
            .ml
            .expanded_key()
            .sign_deterministic(&sm, &[])
            .map_err(|_| Error::Param)?;
        let mut out = Vec::with_capacity(SIGNATURE_LEN);
        out.extend_from_slice(&[FORMAT_VERSION, Suite::Modern as u8]);
        out.extend_from_slice(&ed.to_bytes());
        out.extend_from_slice(&ml.encode());
        Ok(out)
    }
}

/// The public half of a [`SigningIdentity`].
#[derive(Clone, Debug, PartialEq)]
pub struct PublicIdentity {
    ed: ed25519_dalek::VerifyingKey,
    ml: ml_dsa::VerifyingKey<MlDsa65>,
}

impl PublicIdentity {
    /// Encode as `format || suite || ed25519_pk || ml-dsa-65_vk`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PUBLIC_IDENTITY_LEN);
        out.extend_from_slice(&[FORMAT_VERSION, Suite::Modern as u8]);
        out.extend_from_slice(self.ed.as_bytes());
        out.extend_from_slice(&self.ml.encode());
        out
    }

    /// Decode an encoded public identity.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        check_prefix(bytes)?;
        let mut r = Reader::new(&bytes[2..]);
        let ed = ed25519_dalek::VerifyingKey::from_bytes(&r.array()?).map_err(|_| Error::Format)?;
        let vk = EncodedVerifyingKey::<MlDsa65>::try_from(r.take(ML_DSA_65_VK_LEN)?)
            .map_err(|_| Error::Format)?;
        r.finish()?;
        Ok(PublicIdentity {
            ed,
            ml: ml_dsa::VerifyingKey::decode(&vk),
        })
    }

    /// `FP(encode())`.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.encode())
    }

    /// Verify a hybrid signature. Fails unless **both** halves verify.
    pub fn verify(&self, purpose: &str, msg: &[u8], signature: &[u8]) -> Result<()> {
        check_prefix(signature).map_err(|_| Error::Signature)?;
        if signature.len() != SIGNATURE_LEN {
            return Err(Error::Signature);
        }
        let sm = signed_message(purpose, msg)?;
        let (ed_bytes, ml_bytes) = signature[2..].split_at(ED25519_SIG_LEN);
        let ed_sig =
            ed25519_dalek::Signature::from_slice(ed_bytes).map_err(|_| Error::Signature)?;
        let ed_ok = self.ed.verify_strict(&sm, &ed_sig).is_ok();
        let ml_ok = EncodedSignature::<MlDsa65>::try_from(ml_bytes)
            .ok()
            .and_then(|enc| ml_dsa::Signature::<MlDsa65>::decode(&enc))
            .is_some_and(|sig| self.ml.verify_with_context(&sm, &[], &sig));
        if ed_ok && ml_ok {
            Ok(())
        } else {
            Err(Error::Signature)
        }
    }
}

/// A device's secret keys: a signing identity and an X-Wing decapsulation key,
/// both derived from one 32-byte device secret.
pub struct DeviceSecret {
    sig: SigningIdentity,
    kem: x_wing::DecapsulationKey,
}

impl DeviceSecret {
    /// Derive from a 32-byte device secret.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let sig_seed = kdf(labels::DEVICE_SIG, seed, &[]);
        let kem_seed = kdf(labels::DEVICE_KEM, seed, &[]);
        DeviceSecret {
            sig: SigningIdentity::from_seed(&sig_seed),
            kem: x_wing::DecapsulationKey::from(*kem_seed),
        }
    }

    /// Generate a fresh device. Returns the seed, which must be stored securely.
    pub fn generate(rng: &mut dyn Rng) -> Result<(Self, Key32)> {
        let seed = Zeroizing::new(rng::array::<32>(rng)?);
        Ok((Self::from_seed(&seed), seed))
    }

    /// The device's signing identity.
    pub fn signing(&self) -> &SigningIdentity {
        &self.sig
    }

    pub(crate) fn kem(&self) -> &x_wing::DecapsulationKey {
        &self.kem
    }

    /// The public half.
    pub fn public(&self) -> DevicePublic {
        DevicePublic {
            sig: self.sig.public(),
            kem: self.kem.encapsulation_key().clone(),
        }
    }
}

/// A device's public keys.
#[derive(Clone, Debug, PartialEq)]
pub struct DevicePublic {
    sig: PublicIdentity,
    kem: x_wing::EncapsulationKey,
}

impl DevicePublic {
    /// Encode as `public_identity || xwing_ek`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.sig.encode();
        out.extend_from_slice(&self.kem.to_bytes());
        out
    }

    /// Decode an encoded device public key.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != DEVICE_PUBLIC_LEN {
            return Err(Error::Format);
        }
        let (sig, kem) = bytes.split_at(PUBLIC_IDENTITY_LEN);
        Ok(DevicePublic {
            sig: PublicIdentity::decode(sig)?,
            kem: x_wing::EncapsulationKey::try_from(kem).map_err(|_| Error::Format)?,
        })
    }

    /// `FP(encode())`: the device id.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.encode())
    }

    /// The device's signature verification key.
    pub fn signing(&self) -> &PublicIdentity {
        &self.sig
    }

    pub(crate) fn kem(&self) -> &x_wing::EncapsulationKey {
        &self.kem
    }
}

/// Issue a device certificate: the user identity vouches for a device key (G1).
///
/// `body = format || suite || user_fp[32] || device_public || u64(created_unix)`;
/// the certificate is `body || hybrid_signature(SIG_DEVICE_CERT, body)`.
pub fn issue_device_cert(
    user: &SigningIdentity,
    device: &DevicePublic,
    created_unix: u64,
) -> Result<Vec<u8>> {
    let mut body = vec![FORMAT_VERSION, Suite::Modern as u8];
    body.extend_from_slice(&user.public().fingerprint());
    body.extend_from_slice(&device.encode());
    body.extend_from_slice(&created_unix.to_be_bytes());
    let sig = user.sign(labels::SIG_DEVICE_CERT, &body)?;
    body.extend_from_slice(&sig);
    Ok(body)
}

/// Verify a device certificate against the expected user identity.
/// Returns the certified device key and the creation time.
pub fn verify_device_cert(user: &PublicIdentity, cert: &[u8]) -> Result<(DevicePublic, u64)> {
    const BODY_LEN: usize = 2 + 32 + DEVICE_PUBLIC_LEN + 8;
    check_prefix(cert)?;
    if cert.len() != BODY_LEN + SIGNATURE_LEN {
        return Err(Error::Format);
    }
    let (body, sig) = cert.split_at(BODY_LEN);
    user.verify(labels::SIG_DEVICE_CERT, body, sig)?;
    let mut r = Reader::new(&body[2..]);
    if r.array::<32>()? != user.fingerprint() {
        return Err(Error::Signature);
    }
    let device = DevicePublic::decode(r.take(DEVICE_PUBLIC_LEN)?)?;
    let created = r.u64()?;
    r.finish()?;
    Ok((device, created))
}
