//! The fs header: an fs's keyslots and its epoch chain, stored by the server as
//! opaque bytes (spec/formats.md §12, api.md §4.4).
//!
//! ```text
//! u8 header_version (1 or 2) || u8 suite || u32 fs || u32 current_epoch
//! || u16 n_slots || n_slots × lp(keyslot)
//! || u16 n_chain || n_chain × lp(epoch-chain record)
//! || lp(policy)                                   (version 2 only)
//! ```
//!
//! `chain[i]` is the record written when the fs rotated to epoch `i + 1`: the
//! keys of epoch `i + 1` open it to the keys of epoch `i`. So
//! `n_chain == current_epoch`.

use crate::db::cbor::{self, Value};
use crate::encoding::{Reader, lp};
use crate::keys::FsKeys;
use crate::keyslot;
use crate::rng::Rng;
use crate::seal::{self, Kind};
use crate::suite::Suite;
use crate::{Error, Result};

/// Version of the header layout without a policy.
pub const HEADER_VERSION: u8 = 1;

/// Version of the header layout with a policy.
pub const HEADER_VERSION_POLICY: u8 = 2;

/// When a file keeps its file key after a rotation (formats.md §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RekeyAfterRotate {
    /// A fresh key when the whole file is rewritten (no `previous`).
    #[default]
    Rewrite = 0,
    /// A fresh key on the next write, every chunk re-uploaded.
    Eager = 1,
    /// Never a fresh key.
    Keep = 2,
}

/// The fs policy (formats.md §12), kept in a version-2 header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsPolicy {
    /// New files get a file key of their own (`key_mode 1`).
    pub file_keys: bool,
    /// When a file key is replaced after a rotation.
    pub rekey_after_rotate: RekeyAfterRotate,
    /// Entries this version doesn't know, as a CBOR map (empty when none);
    /// kept on re-encode.
    pub unknown: Vec<u8>,
}

impl FsPolicy {
    /// Whether this is the policy a version-1 header means.
    pub fn is_default(&self) -> bool {
        *self == FsPolicy::default()
    }

    /// The CBOR map `{1: file_keys, 2: rekey_after_rotate, …unknown}`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut items = match self.unknown.is_empty() {
            true => Vec::new(),
            false => match cbor::decode(&self.unknown)? {
                Value::Map(m) => m,
                _ => return Err(Error::Param),
            },
        };
        items.retain(|(k, _)| !matches!(k, Value::Int(1 | 2)));
        items.push((Value::Int(1), Value::Int(self.file_keys as i128)));
        items.push((Value::Int(2), Value::Int(self.rekey_after_rotate as i128)));
        cbor::encode(&Value::Map(items))
    }

    /// Decode. A known key with a value this version doesn't know is refused.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let Value::Map(items) = cbor::decode(bytes).map_err(|_| Error::Format)? else {
            return Err(Error::Format);
        };
        let mut p = FsPolicy::default();
        let mut rest = Vec::new();
        for (k, v) in items {
            match (&k, &v) {
                (Value::Int(1), Value::Int(0 | 1)) => p.file_keys = v == Value::Int(1),
                (Value::Int(2), Value::Int(0)) => p.rekey_after_rotate = RekeyAfterRotate::Rewrite,
                (Value::Int(2), Value::Int(1)) => p.rekey_after_rotate = RekeyAfterRotate::Eager,
                (Value::Int(2), Value::Int(2)) => p.rekey_after_rotate = RekeyAfterRotate::Keep,
                (Value::Int(1 | 2), _) => return Err(Error::Format),
                _ => rest.push((k, v)),
            }
        }
        if !rest.is_empty() {
            p.unknown = cbor::encode(&Value::Map(rest)).map_err(|_| Error::Format)?;
        }
        Ok(p)
    }
}

/// Most keyslots a header holds.
pub const MAX_SLOTS: usize = 256;

/// A decoded fs header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsHeader {
    /// The fs id.
    pub fs_id: u32,
    /// The epoch new data is sealed under.
    pub current_epoch: u32,
    /// Keyslots (formats.md §6), in order. Unknown slot types are kept as is.
    pub slots: Vec<Vec<u8>>,
    /// Epoch-chain records; `chain[i]` opens epoch `i` from epoch `i + 1`.
    pub chain: Vec<Vec<u8>>,
    /// The policy; the default in a version-1 header.
    pub policy: FsPolicy,
}

impl FsHeader {
    /// A header for a new fs at epoch 0, with no slots.
    pub fn new(keys: &FsKeys) -> Result<Self> {
        if keys.epoch != 0 {
            return Err(Error::Param);
        }
        Ok(FsHeader {
            fs_id: keys.fs_id,
            current_epoch: 0,
            slots: Vec::new(),
            chain: Vec::new(),
            policy: FsPolicy::default(),
        })
    }

    /// Encode. Fails if the header breaks the layout's rules. A default
    /// policy is written as version 1, so older clients still read it.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        let version = match self.policy.is_default() {
            true => HEADER_VERSION,
            false => HEADER_VERSION_POLICY,
        };
        let mut out = vec![version, Suite::Modern as u8];
        out.extend_from_slice(&self.fs_id.to_be_bytes());
        out.extend_from_slice(&self.current_epoch.to_be_bytes());
        for list in [&self.slots, &self.chain] {
            let n = u16::try_from(list.len()).map_err(|_| Error::Param)?;
            out.extend_from_slice(&n.to_be_bytes());
            for item in list {
                lp(&mut out, item);
            }
        }
        if version == HEADER_VERSION_POLICY {
            lp(&mut out, &self.policy.encode()?);
        }
        Ok(out)
    }

    /// Decode and check the layout. It doesn't decrypt anything.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let version = r.u8()?;
        if version != HEADER_VERSION && version != HEADER_VERSION_POLICY {
            return Err(Error::Format);
        }
        Suite::from_id(r.u8()?)?;
        let fs_id = r.u32()?;
        let current_epoch = r.u32()?;
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            let n = u16::from_be_bytes(r.array()?);
            for _ in 0..n {
                list.push(r.lp()?.to_vec());
            }
        }
        let policy = match version {
            HEADER_VERSION_POLICY => FsPolicy::decode(r.lp()?)?,
            _ => FsPolicy::default(),
        };
        r.finish()?;
        let [slots, chain] = lists;
        let h = FsHeader {
            fs_id,
            current_epoch,
            slots,
            chain,
            policy,
        };
        h.check().map_err(|_| Error::Format)?;
        Ok(h)
    }

    fn check(&self) -> Result<()> {
        if self.fs_id == 0 || self.slots.len() > MAX_SLOTS {
            return Err(Error::Param);
        }
        if self.chain.len() != self.current_epoch as usize {
            return Err(Error::Param);
        }
        for slot in &self.slots {
            keyslot::slot_info(slot)?;
        }
        for (i, record) in self.chain.iter().enumerate() {
            if seal::peek(record)? != (Kind::EpochChain, i as u32 + 1) {
                return Err(Error::Param);
            }
        }
        Ok(())
    }

    /// Add a keyslot.
    pub fn add_slot(&mut self, slot: Vec<u8>) -> Result<()> {
        keyslot::slot_info(&slot)?;
        if self.slots.len() >= MAX_SLOTS {
            return Err(Error::Param);
        }
        self.slots.push(slot);
        Ok(())
    }

    /// Remove the keyslot with this id. Returns whether one was removed.
    pub fn remove_slot(&mut self, slot_id: &[u8; 16]) -> bool {
        let before = self.slots.len();
        self.slots
            .retain(|s| keyslot::slot_info(s).map(|i| i.slot_id) != Ok(*slot_id));
        self.slots.len() != before
    }

    /// The slots of a type, as `(index, slot)`.
    pub fn slots_of_type(&self, slot_type: u8) -> impl Iterator<Item = (usize, &[u8])> {
        self.slots
            .iter()
            .enumerate()
            .filter(move |(_, s)| keyslot::slot_info(s).map(|i| i.slot_type) == Ok(slot_type))
            .map(|(i, s)| (i, s.as_slice()))
    }

    /// Start a new epoch (revocation): returns the keys of `current_epoch + 1`
    /// and appends the chain record. `keys` must be the current epoch's.
    ///
    /// The existing slots still wrap the old epoch. The caller re-wraps the
    /// ones that should keep access and removes the rest
    /// (spec/formats.md §12).
    pub fn rotate(&mut self, keys: &FsKeys, rng: &mut dyn Rng) -> Result<FsKeys> {
        self.check_keys(keys)?;
        if keys.epoch != self.current_epoch {
            return Err(Error::Param);
        }
        let (next, record) = keys.rotate(rng)?;
        self.chain.push(record);
        self.current_epoch = next.epoch;
        Ok(next)
    }

    /// The keys of an earlier `epoch`, walking the chain back from `keys`.
    pub fn keys_at(&self, keys: &FsKeys, epoch: u32) -> Result<FsKeys> {
        self.check_keys(keys)?;
        if epoch > keys.epoch {
            return Err(Error::Param);
        }
        let mut k = FsKeys::from_bundle(&keys.to_bundle())?;
        while k.epoch > epoch {
            let record = self.chain.get(k.epoch as usize - 1).ok_or(Error::Format)?;
            k = k.previous(record)?;
        }
        Ok(k)
    }

    fn check_keys(&self, keys: &FsKeys) -> Result<()> {
        if keys.fs_id != self.fs_id || keys.epoch > self.current_epoch {
            return Err(Error::Param);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::OsRng;

    fn header() -> (FsKeys, FsHeader) {
        let keys = FsKeys::generate(3, &mut OsRng).unwrap();
        let mut h = FsHeader::new(&keys).unwrap();
        h.add_slot(keyslot::create_recovery(&keys, &mut OsRng).unwrap().0)
            .unwrap();
        (keys, h)
    }

    #[test]
    fn unknown_slot_types_are_kept() {
        let (_, mut h) = header();
        let mut future = vec![1, 1, 99, 0];
        future.extend_from_slice(&[7; 16]);
        future.extend_from_slice(b"params of a later slot type");
        h.add_slot(future.clone()).unwrap();
        let back = FsHeader::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(back.slots[1], future);
        assert_eq!(back.slots_of_type(99).count(), 1);
        assert!(h.remove_slot(&[7; 16]));
        assert!(!h.remove_slot(&[7; 16]));
    }

    #[test]
    fn chain_must_match_the_epoch() {
        let (keys, mut h) = header();
        let k1 = h.rotate(&keys, &mut OsRng).unwrap();
        let k2 = h.rotate(&k1, &mut OsRng).unwrap();
        assert_eq!(h.current_epoch, 2);
        assert_eq!(*h.keys_at(&k2, 0).unwrap().to_bundle(), *keys.to_bundle());
        // Rotating from a stale epoch is refused.
        assert!(h.rotate(&k1, &mut OsRng).is_err());

        let mut bytes = h.encode().unwrap();
        bytes[9] = 3; // current_epoch = 3 with two records
        assert_eq!(FsHeader::decode(&bytes), Err(Error::Format));
        // Records out of order are refused.
        let mut swapped = h.clone();
        swapped.chain.swap(0, 1);
        assert_eq!(swapped.encode(), Err(Error::Param));
        // Trailing bytes are refused.
        let mut long = h.encode().unwrap();
        long.push(0);
        assert_eq!(FsHeader::decode(&long), Err(Error::Format));
    }

    #[test]
    fn policy_round_trips_and_defaults_stay_version_1() {
        let (_, mut h) = header();
        let v1 = h.encode().unwrap();
        assert_eq!(v1[0], 1);
        assert!(FsHeader::decode(&v1).unwrap().policy.is_default());

        h.policy.file_keys = true;
        h.policy.rekey_after_rotate = RekeyAfterRotate::Keep;
        let v2 = h.encode().unwrap();
        assert_eq!(v2[0], 2);
        assert_eq!(FsHeader::decode(&v2).unwrap(), h);

        // Unknown keys are kept, and keep the header at version 2.
        let extra = Value::map(vec![(Value::Int(9), Value::text("later"))]);
        h.policy = FsPolicy {
            unknown: cbor::encode(&extra).unwrap(),
            ..FsPolicy::default()
        };
        let back = FsHeader::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(back.policy.unknown, cbor::encode(&extra).unwrap());
        assert_eq!(h.encode().unwrap()[0], 2);

        // A known key with an unknown value is refused.
        let bad = Value::map(vec![(Value::Int(2), Value::Int(7))]);
        assert_eq!(
            FsPolicy::decode(&cbor::encode(&bad).unwrap()),
            Err(Error::Format)
        );
        assert_eq!(FsPolicy::decode(&[0x01]), Err(Error::Format));
    }

    #[test]
    fn keys_of_another_fs_are_refused() {
        let (_, h) = header();
        let other = FsKeys::generate(4, &mut OsRng).unwrap();
        assert!(h.keys_at(&other, 0).is_err());
    }
}
