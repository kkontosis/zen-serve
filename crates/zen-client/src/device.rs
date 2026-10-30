//! A device key: the user's signing identity, the device secret, and the
//! certificate the identity issued for the device. The file is zen-mount's
//! `--device-key` format: JSON `{identity_seed, device_seed, cert}` in hex.

use crate::error::{Error, Result};
use crate::{hex, unhex};
use std::path::Path;
use std::sync::Arc;
use zen_core::sig::{DevicePublic, DeviceSecret, SigningIdentity, issue_device_cert};
use zen_proto::ByteBuf;
use zen_proto::acl::Member;

/// A device key (cheap to clone: the secrets are shared).
#[derive(Clone)]
pub struct DeviceKey {
    identity_seed: [u8; 32],
    device_seed: [u8; 32],
    /// The user's identity (the ACL member identity).
    pub identity: Arc<SigningIdentity>,
    /// The device secret: signing and keyslot key.
    pub device: Arc<DeviceSecret>,
    /// The device certificate, issued by `identity`.
    pub cert: Vec<u8>,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("device", &hex(&self.fp()))
            .finish_non_exhaustive()
    }
}

impl DeviceKey {
    /// From both seeds and a certificate.
    pub fn from_parts(identity_seed: [u8; 32], device_seed: [u8; 32], cert: Vec<u8>) -> Self {
        DeviceKey {
            identity_seed,
            device_seed,
            identity: Arc::new(SigningIdentity::from_seed(&identity_seed)),
            device: Arc::new(DeviceSecret::from_seed(&device_seed)),
            cert,
        }
    }

    /// A device key whose certificate the identity issues now (for setup and tests).
    pub fn new(identity_seed: [u8; 32], device_seed: [u8; 32]) -> Result<Self> {
        let identity = SigningIdentity::from_seed(&identity_seed);
        let device = DeviceSecret::from_seed(&device_seed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let cert = issue_device_cert(&identity, &device.public(), now)?;
        Ok(DeviceKey::from_parts(identity_seed, device_seed, cert))
    }

    /// A fresh random device key.
    pub fn generate() -> Result<Self> {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        getrandom::fill(&mut a).map_err(|e| Error::local("rng", e.to_string()))?;
        getrandom::fill(&mut b).map_err(|e| Error::local("rng", e.to_string()))?;
        DeviceKey::new(a, b)
    }

    /// Read a device key file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        Self::from_json(&text)
    }

    /// Parse the JSON form.
    pub fn from_json(text: &str) -> Result<Self> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| Error::format(format!("device key: {e}")))?;
        let field = |name: &str| -> Result<Vec<u8>> {
            let s = v[name]
                .as_str()
                .ok_or_else(|| Error::format(format!("device key: no {name}")))?;
            unhex(s).ok_or_else(|| Error::format(format!("device key: {name} isn't hex")))
        };
        let seed = |name: &str| -> Result<[u8; 32]> {
            field(name)?
                .try_into()
                .map_err(|_| Error::format(format!("device key: {name} isn't 32 bytes")))
        };
        let key =
            DeviceKey::from_parts(seed("identity_seed")?, seed("device_seed")?, field("cert")?);
        zen_core::sig::verify_device_cert(&key.identity.public(), &key.cert)
            .map_err(|_| Error::format("device key: the cert isn't this identity's"))?;
        Ok(key)
    }

    /// The JSON form.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "identity_seed": hex(&self.identity_seed),
            "device_seed": hex(&self.device_seed),
            "cert": hex(&self.cert),
        })
        .to_string()
    }

    /// Write the file, readable by its owner only.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::pins::write_private(path, self.to_json().as_bytes())
    }

    /// The device's public keys.
    pub fn public(&self) -> DevicePublic {
        self.device.public()
    }

    /// The device fingerprint.
    pub fn fp(&self) -> [u8; 32] {
        self.device.public().fingerprint()
    }

    /// The user fingerprint.
    pub fn user_fp(&self) -> [u8; 32] {
        self.identity.public().fingerprint()
    }

    /// The ACL member entry: the identity with this device.
    pub fn member(&self) -> Member {
        Member {
            devices: vec![ByteBuf::from(self.cert.clone())],
            identity: self.identity.public().encode(),
        }
    }
}
