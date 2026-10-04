//! Keyslots: the fs key bundle wrapped many times, as in LUKS (spec/formats.md §8).
//!
//! ```text
//! off  len   field
//!   0    1   format_version (= 1)
//!   1    1   suite          (= 1)
//!   2    1   slot_type      (1 = passphrase, 2 = recovery key, 3 = X-Wing device)
//!   3    1   reserved       (= 0)
//!   4   16   slot_id        (random)
//!  20    …   type params:
//!              passphrase: u32 m_cost_kib || u32 t_cost || u32 p_cost || salt[32]
//!              recovery:   (none)
//!              device:     recipient_fp[32] || xwing_ciphertext[1120]
//!   …   24   nonce
//!   …   88   AEAD(KEK, bundle[72]) incl. 16-byte tag
//! ```
//!
//! AAD = `"zen/v1/aad/keyslot" || 0x00 || everything before the nonce`.
//! KEK = `KDF("zen/v1/keyslot-kek", secret, slot_id)` where `secret` is the
//! Argon2id output, the recovery key, or the X-Wing shared secret.

use crate::encoding::Reader;
use crate::kdf::{Key32, kdf};
use crate::keys::{BUNDLE_LEN, FsKeys};
use crate::labels;
use crate::rng::{self, Rng};
use crate::sig::{DevicePublic, DeviceSecret};
use crate::suite::{FORMAT_VERSION, Suite, check_prefix};
use crate::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use x_wing::Decapsulate;
use zeroize::Zeroizing;

const TYPE_PASSPHRASE: u8 = 1;
const TYPE_RECOVERY: u8 = 2;
const TYPE_DEVICE: u8 = 3;

/// Argon2id parameters stored in a passphrase keyslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory cost in KiB.
    pub m_cost_kib: u32,
    /// Iterations.
    pub t_cost: u32,
    /// Parallelism (lanes).
    pub p_cost: u32,
}

impl Argon2Params {
    /// Minimum memory accepted when creating a slot (G25): 64 MiB.
    pub const MIN_M_COST_KIB: u32 = 64 * 1024;

    /// Recommended native parameters: 1 GiB, 4 passes.
    pub const NATIVE: Argon2Params = Argon2Params {
        m_cost_kib: 1024 * 1024,
        t_cost: 4,
        p_cost: 1,
    };
    /// Recommended browser parameters: 256 MiB, 3 passes.
    pub const BROWSER: Argon2Params = Argon2Params {
        m_cost_kib: 256 * 1024,
        t_cost: 3,
        p_cost: 1,
    };

    /// Maximum memory accepted when opening a slot: 4 GiB. Stored parameters are
    /// server-supplied bytes, so without a ceiling a malicious server could make
    /// unlocking hang or exhaust memory.
    pub const MAX_M_COST_KIB: u32 = 4 * 1024 * 1024;
    /// Maximum iterations accepted when opening a slot.
    pub const MAX_T_COST: u32 = 16;
    /// Maximum lanes accepted when creating or opening a slot.
    pub const MAX_P_COST: u32 = 4;

    fn validate_for_open(&self) -> Result<()> {
        if self.m_cost_kib > Self::MAX_M_COST_KIB
            || !(1..=Self::MAX_T_COST).contains(&self.t_cost)
            || !(1..=Self::MAX_P_COST).contains(&self.p_cost)
        {
            return Err(Error::Param);
        }
        Ok(())
    }

    fn validate_for_create(&self) -> Result<()> {
        self.validate_for_open()?;
        if self.m_cost_kib < Self::MIN_M_COST_KIB {
            return Err(Error::Param);
        }
        Ok(())
    }

    fn derive(&self, passphrase: &[u8], salt: &[u8; 32]) -> Result<Key32> {
        let params = argon2::Params::new(self.m_cost_kib, self.t_cost, self.p_cost, Some(32))
            .map_err(|_| Error::Param)?;
        let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out = Zeroizing::new([0u8; 32]);
        a.hash_password_into(passphrase, salt, out.as_mut())
            .map_err(|_| Error::Param)?;
        Ok(out)
    }
}

/// A 32-byte recovery key, shown to the user once.
pub type RecoveryKey = Key32;

/// How to unlock a keyslot.
pub enum Unlock<'a> {
    /// A passphrase.
    Passphrase(&'a [u8]),
    /// A recovery key.
    Recovery(&'a [u8; 32]),
    /// A device's X-Wing key.
    Device(&'a DeviceSecret),
}

fn slot_prefix(slot_type: u8, slot_id: &[u8; 16]) -> Vec<u8> {
    let mut out = vec![FORMAT_VERSION, Suite::Modern as u8, slot_type, 0];
    out.extend_from_slice(slot_id);
    out
}

fn wrap(
    mut slot: Vec<u8>,
    secret: &[u8; 32],
    slot_id: &[u8; 16],
    fs: &FsKeys,
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    let kek = kdf(labels::KEYSLOT_KEK, secret, slot_id);
    let nonce: [u8; 24] = rng::array(rng)?;
    let aad = keyslot_aad(&slot);
    let ct = XChaCha20Poly1305::new(&Key::from(*kek))
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &fs.to_bundle(),
                aad: &aad,
            },
        )
        .map_err(|_| Error::Param)?;
    slot.extend_from_slice(&nonce);
    slot.extend_from_slice(&ct);
    Ok(slot)
}

fn keyslot_aad(prefix: &[u8]) -> Vec<u8> {
    let mut aad = labels::AAD_KEYSLOT.as_bytes().to_vec();
    aad.push(0);
    aad.extend_from_slice(prefix);
    aad
}

/// Create a passphrase keyslot. Rejects Argon2id parameters below the floor.
pub fn create_passphrase(
    fs: &FsKeys,
    passphrase: &[u8],
    params: Argon2Params,
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    params.validate_for_create()?;

    let slot_id: [u8; 16] = rng::array(rng)?;
    let salt: [u8; 32] = rng::array(rng)?;
    let mut slot = slot_prefix(TYPE_PASSPHRASE, &slot_id);
    slot.extend_from_slice(&params.m_cost_kib.to_be_bytes());
    slot.extend_from_slice(&params.t_cost.to_be_bytes());
    slot.extend_from_slice(&params.p_cost.to_be_bytes());
    slot.extend_from_slice(&salt);
    let secret = params.derive(passphrase, &salt)?;
    wrap(slot, &secret, &slot_id, fs, rng)
}

/// Create a recovery-key keyslot. Returns the slot and the new recovery key.
pub fn create_recovery(fs: &FsKeys, rng: &mut dyn Rng) -> Result<(Vec<u8>, RecoveryKey)> {
    let slot_id: [u8; 16] = rng::array(rng)?;
    let key = Zeroizing::new(rng::array::<32>(rng)?);
    let slot = wrap(
        slot_prefix(TYPE_RECOVERY, &slot_id),
        &key,
        &slot_id,
        fs,
        rng,
    )?;
    Ok((slot, key))
}

/// Create a keyslot for a device, by X-Wing encapsulation to its public key.
/// The device's key must already be verified (certificate + fingerprint, G1).
pub fn create_device(fs: &FsKeys, recipient: &DevicePublic, rng: &mut dyn Rng) -> Result<Vec<u8>> {
    let slot_id: [u8; 16] = rng::array(rng)?;
    let randomness = Zeroizing::new(rng::array::<{ x_wing::ENCAPSULATION_RANDOMNESS_SIZE }>(
        rng,
    )?);
    let (ct, ss) = recipient
        .kem()
        .encapsulate_deterministic(&(*randomness).into());
    let mut slot = slot_prefix(TYPE_DEVICE, &slot_id);
    slot.extend_from_slice(&recipient.fingerprint());
    slot.extend_from_slice(&ct);
    let ss = Zeroizing::new(<[u8; 32]>::from(ss));
    wrap(slot, &ss, &slot_id, fs, rng)
}

/// Unlock a keyslot and return the fs keys it wraps.
pub fn open(slot: &[u8], unlock: Unlock<'_>) -> Result<FsKeys> {
    check_prefix(slot)?;
    let mut r = Reader::new(&slot[2..]);
    let slot_type = r.u8()?;
    if r.u8()? != 0 {
        return Err(Error::Format);
    }
    let slot_id: [u8; 16] = r.array()?;
    let secret: Key32 = match (slot_type, unlock) {
        (TYPE_PASSPHRASE, Unlock::Passphrase(pw)) => {
            let params = Argon2Params {
                m_cost_kib: r.u32()?,
                t_cost: r.u32()?,
                p_cost: r.u32()?,
            };
            params.validate_for_open()?;
            let salt: [u8; 32] = r.array()?;
            params.derive(pw, &salt)?
        }
        (TYPE_RECOVERY, Unlock::Recovery(key)) => Zeroizing::new(*key),
        (TYPE_DEVICE, Unlock::Device(dev)) => {
            let fp: [u8; 32] = r.array()?;
            if fp != dev.public().fingerprint() {
                return Err(Error::Decrypt);
            }
            let ct = x_wing::Ciphertext::try_from(r.take(x_wing::CIPHERTEXT_SIZE)?)
                .map_err(|_| Error::Format)?;
            Zeroizing::new(<[u8; 32]>::from(dev.kem().decapsulate(&ct)))
        }
        (TYPE_PASSPHRASE | TYPE_RECOVERY | TYPE_DEVICE, _) => return Err(Error::Param),
        _ => return Err(Error::Format),
    };
    let rest = r.rest();
    let prefix = &slot[..slot.len() - rest.len()];
    let mut r = Reader::new(rest);
    let nonce: [u8; 24] = r.array()?;
    let ct = r.rest();
    if ct.len() != BUNDLE_LEN + 16 {
        return Err(Error::Format);
    }
    let kek = kdf(labels::KEYSLOT_KEK, &secret, &slot_id);
    let bundle = Zeroizing::new(
        XChaCha20Poly1305::new(&Key::from(*kek))
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: &keyslot_aad(prefix),
                },
            )
            .map_err(|_| Error::Decrypt)?,
    );
    FsKeys::from_bundle(&bundle)
}
