//! Per-filesystem key hierarchy (spec/formats.md §2, GAPS G2).
//!
//! * `naming_key` (NK): long-lived, never rotated on revocation. Used only to
//!   derive PRF tokens, so stored keys and topic ids stay stable across epochs.
//! * `epoch_key` (MK_e): rotated per epoch. Every AEAD key derives from it.

use crate::encoding::Reader;
use crate::kdf::{Key32, kdf};
use crate::labels;
use crate::rng::{self, Rng};
use crate::seal::{self, Kind};
use crate::{Error, Result};
use zeroize::Zeroizing;

/// Length of an encoded [`FsKeys`] bundle (the keyslot payload).
pub const BUNDLE_LEN: usize = 4 + 4 + 32 + 32;

/// The keys of one filesystem at one epoch.
pub struct FsKeys {
    /// Filesystem id (u32 on the wire; 0 is reserved for plaintext `/unencrypted`).
    pub fs_id: u32,
    /// Current key epoch.
    pub epoch: u32,
    naming_key: Key32,
    epoch_key: Key32,
}

impl FsKeys {
    /// Create keys for a new filesystem at epoch 0.
    pub fn generate(fs_id: u32, rng: &mut dyn Rng) -> Result<Self> {
        if fs_id == 0 {
            return Err(Error::Param);
        }
        Ok(FsKeys {
            fs_id,
            epoch: 0,
            naming_key: Zeroizing::new(rng::array(rng)?),
            epoch_key: Zeroizing::new(rng::array(rng)?),
        })
    }

    /// Construct from raw parts.
    pub fn from_parts(fs_id: u32, epoch: u32, naming_key: [u8; 32], epoch_key: [u8; 32]) -> Self {
        FsKeys {
            fs_id,
            epoch,
            naming_key: Zeroizing::new(naming_key),
            epoch_key: Zeroizing::new(epoch_key),
        }
    }

    fn fs_epoch_info(&self) -> [u8; 8] {
        let mut info = [0u8; 8];
        info[..4].copy_from_slice(&self.fs_id.to_be_bytes());
        info[4..].copy_from_slice(&self.epoch.to_be_bytes());
        info
    }

    /// `KDF("zen/v1/kv-data", MK_e, u32(fs) || u32(e))`.
    pub fn kv_data_key(&self) -> Key32 {
        kdf(labels::KV_DATA, &self.epoch_key, &self.fs_epoch_info())
    }

    /// `KDF("zen/v1/epoch-chain", MK_e, u32(fs) || u32(e))`.
    fn chain_key(&self) -> Key32 {
        kdf(labels::EPOCH_CHAIN, &self.epoch_key, &self.fs_epoch_info())
    }

    /// `KDF("zen/v1/kv-name", NK, u32(fs))`: root of the KV naming chain.
    pub fn kv_name_root(&self) -> Key32 {
        kdf(labels::KV_NAME, &self.naming_key, &self.fs_id.to_be_bytes())
    }

    /// `KDF("zen/v1/topic-name", NK, u32(fs))`: root of the topic naming chain.
    pub fn topic_name_root(&self) -> Key32 {
        kdf(
            labels::TOPIC_NAME,
            &self.naming_key,
            &self.fs_id.to_be_bytes(),
        )
    }

    /// `KDF("zen/v1/topic-data", MK_e, u32(fs) || u32(e))`: root of the topic data chain.
    pub fn topic_data_root(&self) -> Key32 {
        kdf(labels::TOPIC_DATA, &self.epoch_key, &self.fs_epoch_info())
    }

    /// Encode as the 72-byte keyslot payload: `u32(fs) || u32(e) || NK || MK_e`.
    pub fn to_bundle(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(BUNDLE_LEN));
        out.extend_from_slice(&self.fs_epoch_info());
        out.extend_from_slice(self.naming_key.as_ref());
        out.extend_from_slice(self.epoch_key.as_ref());
        out
    }

    /// Decode a keyslot payload.
    pub fn from_bundle(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let fs_id = r.u32()?;
        let epoch = r.u32()?;
        let nk = Zeroizing::new(r.array::<32>()?);
        let mk = Zeroizing::new(r.array::<32>()?);
        r.finish()?;
        Ok(FsKeys::from_parts(fs_id, epoch, *nk, *mk))
    }

    /// Start epoch `e+1` (revocation). Returns the new keys and the chain record
    /// `seal(chain_key(e+1), MK_e)` that lets holders of `e+1` read epoch `e`.
    /// The naming key is carried over unchanged (G2).
    pub fn rotate(&self, rng: &mut dyn Rng) -> Result<(FsKeys, Vec<u8>)> {
        let next = FsKeys {
            fs_id: self.fs_id,
            epoch: self.epoch.checked_add(1).ok_or(Error::Param)?,
            naming_key: self.naming_key.clone(),
            epoch_key: Zeroizing::new(rng::array(rng)?),
        };
        let aad = seal::aad_epoch_chain(next.fs_id);
        let record = seal::seal(
            Kind::EpochChain,
            next.epoch,
            &next.chain_key(),
            &aad,
            self.epoch_key.as_ref(),
            rng,
        )?;
        Ok((next, record))
    }

    /// Recover the previous epoch's keys from a chain record.
    pub fn previous(&self, record: &[u8]) -> Result<FsKeys> {
        if self.epoch == 0 {
            return Err(Error::Param);
        }
        let aad = seal::aad_epoch_chain(self.fs_id);
        let (epoch, mk) = seal::open(Kind::EpochChain, &self.chain_key(), &aad, record)?;
        if epoch != self.epoch {
            return Err(Error::Decrypt);
        }
        let mk: [u8; 32] = mk.as_slice().try_into().map_err(|_| Error::Format)?;
        Ok(FsKeys::from_parts(
            self.fs_id,
            self.epoch - 1,
            *self.naming_key,
            mk,
        ))
    }
}
