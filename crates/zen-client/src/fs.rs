//! A filesystem: its header (keyslots, the epoch chain, the signed form of
//! spec/formats.md §12.1) and unlocking it with a device keyslot (§6).

use crate::error::{Alarm, AlarmKind, AlarmObject, Error, Result};
use crate::integrity::{Integrity, IntegrityOptions};
use crate::session::Session;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use zen_core::header::{FsHeader, SignedHeader, is_signed, validate_header_successor};
use zen_core::integrity::Head;
use zen_core::keys::FsKeys;
use zen_core::keyslot::{self, SLOT_DEVICE, Unlock};
use zen_core::rng::OsRng;
use zen_core::seal;
use zen_core::sig::{DevicePublic, DeviceSecret};
use zen_proto::{Header, HeaderGet, HeaderPut, HeaderVersion};

/// A signed header's chain position and signer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedInfo {
    /// Signed version, from 1.
    pub version: u64,
    /// Its `HH`.
    pub hash: [u8; 32],
    /// The signing admin's user fingerprint.
    pub signer: [u8; 32],
}

impl SignedInfo {
    fn head(&self) -> Head {
        Head {
            version: self.version,
            hash: self.hash,
        }
    }
}

/// The header as stored.
#[derive(Debug, Clone)]
pub struct StoredHeader {
    /// Decoded.
    pub header: FsHeader,
    /// Its version (versionstamp), for compare-and-set.
    pub version: Vec<u8>,
    /// The signed form's position, when signed.
    pub signed: Option<SignedInfo>,
    /// The bytes as stored (either form).
    pub bytes: Vec<u8>,
}

/// A filesystem, before unlocking (cheap to clone).
#[derive(Clone)]
pub struct Fs {
    pub(crate) session: Session,
    pub(crate) id: u32,
    integrity: Arc<OnceLock<Integrity>>,
    ring: Arc<Mutex<Option<Arc<KeyRing>>>>,
}

impl Fs {
    pub(crate) fn new(session: Session, id: u32) -> Self {
        Fs {
            session,
            id,
            integrity: Arc::new(OnceLock::new()),
            ring: Arc::new(Mutex::new(None)),
        }
    }

    /// The fs id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Attach integrity (spec/formats.md §14): pins, this device's
    /// checkpoints, and the checks of what the server serves. One per `Fs`
    /// (clones share it); a second call returns the first. Attach it before
    /// unlocking, so the header is checked too.
    pub async fn integrity(&self, opts: IntegrityOptions) -> Result<Integrity> {
        if let Some(i) = self.integrity.get() {
            return Ok(i.clone());
        }
        let i = Integrity::attach(self.session.clone(), self.id, opts)?;
        let i = self.integrity.get_or_init(|| i).clone();
        if let Some(r) = self.ring.lock().expect("fs").clone() {
            i.bind(r);
        }
        Ok(i)
    }

    /// The attached integrity, if any.
    pub fn attached(&self) -> Option<Integrity> {
        self.integrity.get().cloned()
    }

    /// The stored header, or `None` if the fs has none yet. A signed one must
    /// be signed by an admin of the verified ACL; with integrity attached,
    /// its chain position is checked against the pin.
    pub async fn header(&self) -> Result<Option<StoredHeader>> {
        read_header(&self.session, self.id, self.attached().as_ref()).await
    }

    /// Write the header (admins). `expect` is the version read (`None`: a
    /// new header). With `sign`, the signed form as the successor of the
    /// given position (`None`: version 1), by the session's identity.
    async fn write_header(
        &self,
        header: &FsHeader,
        current: Option<&StoredHeader>,
        sign: Option<Option<SignedInfo>>,
    ) -> Result<StoredHeader> {
        let mut bytes = header.encode()?;
        let mut signed = None;
        if let Some(after) = sign {
            let id = self.session.identity().ok_or_else(signer_needed)?;
            let (version, prev) = after.map_or((1, [0; 32]), |a| (a.version + 1, a.hash));
            let h = SignedHeader::sign(&id, self.id, version, prev, bytes)?;
            // The server checks the same rules (formats.md §12.1): fail here first.
            let acl = self.session.acl().head().await?;
            validate_header_successor(
                self.id,
                current.map(|c| c.bytes.as_slice()),
                &h.encode(),
                |fp| acl.admin(fp),
            )
            .map_err(|_| Error::local("param", "not a valid successor of the stored header"))?;
            signed = Some(SignedInfo {
                version,
                hash: h.hash(),
                signer: h.signer,
            });
            bytes = h.encode();
        }
        let r: HeaderVersion = self
            .session
            .call(
                "/v1/fs/header/put",
                &HeaderPut {
                    fs: self.id,
                    header: bytes.clone(),
                    expect: current.map(|c| c.version.clone()),
                },
            )
            .await?;
        if let (Some(s), Some(i)) = (signed, self.attached()) {
            i.check_header(Some(s.head()))?;
        }
        Ok(StoredHeader {
            header: header.clone(),
            version: r.version,
            signed,
            bytes,
        })
    }

    /// Create the fs's keys and header (admins), with the slots `slots` makes
    /// for the new keys. 409 `version_mismatch` if a header exists. With
    /// `signed`, the header is signed from version 1 by the session's identity.
    pub async fn init(
        &self,
        slots: impl FnOnce(&FsKeys) -> Result<Vec<Vec<u8>>>,
        signed: bool,
    ) -> Result<UnlockedFs> {
        if signed && self.session.identity().is_none() {
            return Err(signer_needed());
        }
        let keys = FsKeys::generate(self.id, &mut OsRng)?;
        let mut header = FsHeader::new(&keys)?;
        for s in slots(&keys)? {
            header.add_slot(s)?;
        }
        let stored = self
            .write_header(&header, None, signed.then_some(None))
            .await?;
        Ok(UnlockedFs::new(self.clone(), keys, stored))
    }

    /// [`Fs::init`] with a device keyslot for each of `devices`.
    pub async fn init_for(&self, devices: &[DevicePublic], signed: bool) -> Result<UnlockedFs> {
        self.init(
            |k| {
                devices
                    .iter()
                    .map(|d| Ok(keyslot::create_device(k, d, &mut OsRng)?))
                    .collect()
            },
            signed,
        )
        .await
    }

    /// Unlock with the device keyslot whose recipient is `device`.
    pub async fn unlock_device(&self, device: &DeviceSecret) -> Result<UnlockedFs> {
        let stored = self
            .header()
            .await?
            .ok_or_else(|| Error::local("not_found", format!("fs {} has no header", self.id)))?;
        let fp = device.public().fingerprint();
        let keys = stored
            .header
            .slots_of_type(SLOT_DEVICE)
            .filter(|(_, s)| keyslot::device_recipient(s).is_ok_and(|r| r == fp))
            .find_map(|(_, s)| keyslot::open(s, Unlock::Device(device)).ok())
            .ok_or_else(|| {
                Error::local("no_slot", "no keyslot of this fs opens with this device")
            })?;
        if keys.fs_id != self.id {
            return Err(Error::format("a keyslot of another fs"));
        }
        Ok(UnlockedFs::new(self.clone(), keys, stored))
    }
}

fn signer_needed() -> Error {
    Error::local(
        "signer_needed",
        "a signed header needs the session's identity (session.use_identity)",
    )
}

/// Read and check the header of fs `id` (spec/formats.md §12.1, clients).
pub(crate) async fn read_header(
    session: &Session,
    id: u32,
    integrity: Option<&Integrity>,
) -> Result<Option<StoredHeader>> {
    let r: Header = session
        .call("/v1/fs/header/get", &HeaderGet { fs: id })
        .await?;
    let (Some(bytes), Some(version)) = (r.header, r.version) else {
        // No header at all after a pinned signed one: withheld.
        if let Some(i) = integrity {
            i.check_header(None)?;
        }
        return Ok(None);
    };
    let mut inner = bytes.clone();
    let mut signed = None;
    if is_signed(&bytes) {
        let h = SignedHeader::decode(&bytes)?;
        let acl = session.acl();
        let mut admin = acl.pinned().and_then(|a| a.admin(&h.signer));
        if admin.is_none() {
            admin = acl.head().await?.admin(&h.signer);
        }
        let ok = h.fs == id && admin.is_some_and(|a| h.verify(&a).is_ok());
        if !ok {
            let alarm = Alarm {
                kind: AlarmKind::Invalid,
                object: AlarmObject::Header,
                id: None,
                detail: format!("header version {} isn't signed by an admin", h.version),
            };
            return Err(match integrity {
                Some(i) => i.raise(alarm),
                None => Error::Integrity(alarm),
            });
        }
        signed = Some(SignedInfo {
            version: h.version,
            hash: h.hash(),
            signer: h.signer,
        });
        inner = h.header;
    }
    if let Some(i) = integrity {
        i.check_header(signed.map(|s| s.head()))?;
    }
    let header = FsHeader::decode(&inner)?;
    if header.fs_id != id {
        return Err(Error::format(format!(
            "header of fs {} served for fs {id}",
            header.fs_id
        )));
    }
    Ok(Some(StoredHeader {
        header,
        version,
        signed,
        bytes,
    }))
}

/// The keys of an unlocked fs: those the slot opened, the header for older
/// epochs, and the epochs opened so far.
pub(crate) struct KeyRing {
    slot: Arc<FsKeys>,
    stored: Mutex<StoredHeader>,
    epochs: Mutex<HashMap<u32, Arc<FsKeys>>>,
}

impl KeyRing {
    /// The keys of `epoch`, walking the chain back. A newer epoch than the
    /// slot's needs a new unlock (`epoch_ahead`).
    pub(crate) fn keys_at(&self, epoch: u32) -> Result<Arc<FsKeys>> {
        if let Some(k) = self.epochs.lock().expect("keys").get(&epoch) {
            return Ok(k.clone());
        }
        if epoch > self.slot.epoch {
            return Err(Error::local(
                "epoch_ahead",
                format!(
                    "data of epoch {epoch}; these keys reach {}: unlock again",
                    self.slot.epoch
                ),
            ));
        }
        let k = Arc::new(
            self.stored
                .lock()
                .expect("keys")
                .header
                .keys_at(&self.slot, epoch)?,
        );
        self.epochs.lock().expect("keys").insert(epoch, k.clone());
        Ok(k)
    }

    /// The keys that open a sealed object (by its epoch).
    pub(crate) fn keys_for(&self, sealed: &[u8]) -> Result<Arc<FsKeys>> {
        self.keys_at(seal::peek(sealed)?.1)
    }

    /// The keys to seal new data with: the slot's, if it wraps the current epoch.
    pub(crate) fn seal_keys(&self) -> Result<Arc<FsKeys>> {
        let current = self.stored.lock().expect("keys").header.current_epoch;
        if self.slot.epoch != current {
            return Err(Error::local(
                "stale_slot",
                format!(
                    "keyslot wraps epoch {}; the fs is at {current}",
                    self.slot.epoch
                ),
            ));
        }
        Ok(self.slot.clone())
    }
}

/// An unlocked filesystem (cheap to clone).
#[derive(Clone)]
pub struct UnlockedFs {
    pub(crate) fs: Fs,
    pub(crate) ring: Arc<KeyRing>,
}

impl UnlockedFs {
    fn new(fs: Fs, keys: FsKeys, stored: StoredHeader) -> Self {
        let slot = Arc::new(keys);
        let ring = Arc::new(KeyRing {
            epochs: Mutex::new(HashMap::from([(slot.epoch, slot.clone())])),
            slot,
            stored: Mutex::new(stored),
        });
        *fs.ring.lock().expect("fs") = Some(ring.clone());
        if let Some(i) = fs.attached() {
            i.bind(ring.clone());
        }
        UnlockedFs { fs, ring }
    }

    /// The fs id.
    pub fn id(&self) -> u32 {
        self.fs.id
    }

    /// The locked fs.
    pub fn fs(&self) -> &Fs {
        &self.fs
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.fs.session
    }

    /// The attached integrity, if any.
    pub fn integrity(&self) -> Option<Integrity> {
        self.fs.attached()
    }

    /// The keys the slot opened.
    pub fn keys(&self) -> Arc<FsKeys> {
        self.ring.slot.clone()
    }

    /// The header as last read or written here.
    pub fn stored(&self) -> StoredHeader {
        self.ring.stored.lock().expect("keys").clone()
    }

    /// The keys to seal new data with (`stale_slot` for an older epoch's slot).
    pub fn seal_keys(&self) -> Result<Arc<FsKeys>> {
        self.ring.seal_keys()
    }

    /// The keys that open a sealed object.
    pub fn keys_for(&self, sealed: &[u8]) -> Result<Arc<FsKeys>> {
        self.ring.keys_for(sealed)
    }

    /// Change the header (admins): `edit` gets the freshly read header and
    /// returns whether to write it. Once the stored header is signed, every
    /// successor is signed, with the session's identity. Retries a
    /// concurrent change twice.
    pub async fn edit_header(&self, edit: impl FnMut(&mut FsHeader) -> Result<bool>) -> Result<()> {
        self.edit(edit, false).await
    }

    /// Sign the header from now on (admins, formats.md §12.1).
    pub async fn sign_header(&self) -> Result<()> {
        self.edit(|_| Ok(true), true).await
    }

    async fn edit(
        &self,
        mut edit: impl FnMut(&mut FsHeader) -> Result<bool>,
        sign: bool,
    ) -> Result<()> {
        for i in 1.. {
            let mut fresh = self
                .fs
                .header()
                .await?
                .ok_or_else(|| Error::local("not_found", "the header is gone"))?;
            let signing = sign || fresh.signed.is_some();
            if signing && self.session().identity().is_none() {
                return Err(signer_needed());
            }
            if !edit(&mut fresh.header)? {
                return Ok(());
            }
            let header = fresh.header.clone();
            match self
                .fs
                .write_header(&header, Some(&fresh), signing.then_some(fresh.signed))
                .await
            {
                Ok(stored) => {
                    *self.ring.stored.lock().expect("keys") = stored;
                    return Ok(());
                }
                Err(e) if e.is("version_mismatch") && i < 3 => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Add a keyslot for a device (its public keys, verified through the ACL).
    pub async fn add_device_slot(&self, device: &DevicePublic) -> Result<()> {
        let slot = keyslot::create_device(&*self.seal_keys()?, device, &mut OsRng)?;
        self.edit_header(|h| {
            h.add_slot(slot.clone())?;
            Ok(true)
        })
        .await
    }

    /// Rotate to a new epoch (revocation, formats.md §12): every old slot is
    /// removed and `slots` makes the new ones. Returns the fs unlocked at the
    /// new epoch; old data stays readable through the chain.
    pub async fn rotate(
        &self,
        mut slots: impl FnMut(&FsKeys) -> Result<Vec<Vec<u8>>>,
    ) -> Result<UnlockedFs> {
        let current = self.seal_keys()?;
        let mut next: Option<FsKeys> = None;
        self.edit_header(|h| {
            if h.current_epoch != current.epoch {
                return Err(Error::local("stale_slot", "rotate from the current epoch"));
            }
            let k = h.rotate(&current, &mut OsRng)?;
            h.slots.clear();
            for s in slots(&k)? {
                h.add_slot(s)?;
            }
            next = Some(k);
            Ok(true)
        })
        .await?;
        let keys = next.expect("rotated");
        Ok(UnlockedFs::new(self.fs.clone(), keys, self.stored()))
    }
}
