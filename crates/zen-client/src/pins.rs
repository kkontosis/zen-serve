//! The pins file: what a client verified, kept across processes, one file
//! per fs. Deterministic CBOR:
//!
//! ```text
//! { 1: v = 1, 2: fs, 3?: signed ACL bytes (the pinned version),
//!   4?: [version, HH] (the header pin), 5: [[fp, seq, HC, body]] (device heads) }
//! ```
//!
//! Written atomically (a temporary file in the same directory, fsync,
//! rename), readable by its owner only. A file of another fs, or one that
//! doesn't decode, is refused: dropping pins silently would hide a rollback.

use crate::error::{Error, Result};
use std::io::Write;
use std::path::Path;
use zen_core::db::cbor::{self, Value};
use zen_core::integrity::Head;

/// One pinned device head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRecord {
    pub device: [u8; 32],
    pub seq: u64,
    pub hash: [u8; 32],
    /// The body's canonical CBOR.
    pub body: Vec<u8>,
}

/// The pins of one fs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PinsFile {
    pub fs: u32,
    pub acl: Option<Vec<u8>>,
    pub header: Option<Head>,
    pub devices: Vec<DeviceRecord>,
}

fn bad(path: &Path, why: &str) -> Error {
    Error::format(format!("pins file {}: {why}", path.display()))
}

fn uint(v: Option<&Value>) -> Option<u64> {
    match v {
        Some(Value::Int(i)) => u64::try_from(*i).ok(),
        _ => None,
    }
}

fn bytes(v: Option<&Value>) -> Option<&[u8]> {
    match v {
        Some(Value::Bytes(b)) => Some(b),
        _ => None,
    }
}

fn fixed(v: Option<&Value>) -> Option<[u8; 32]> {
    bytes(v)?.try_into().ok()
}

impl PinsFile {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut m = vec![
            (Value::Int(1), Value::Int(1)),
            (Value::Int(2), u64::from(self.fs).into()),
        ];
        if let Some(a) = &self.acl {
            m.push((Value::Int(3), Value::Bytes(a.clone())));
        }
        if let Some(h) = &self.header {
            m.push((
                Value::Int(4),
                Value::Array(vec![h.version.into(), Value::Bytes(h.hash.to_vec())]),
            ));
        }
        let devs = self
            .devices
            .iter()
            .map(|d| {
                Value::Array(vec![
                    Value::Bytes(d.device.to_vec()),
                    d.seq.into(),
                    Value::Bytes(d.hash.to_vec()),
                    Value::Bytes(d.body.clone()),
                ])
            })
            .collect();
        m.push((Value::Int(5), Value::Array(devs)));
        Ok(cbor::encode(&Value::Map(m))?)
    }

    pub fn decode(path: &Path, b: &[u8]) -> Result<Self> {
        let v = cbor::decode(b).map_err(|_| bad(path, "not CBOR"))?;
        if !matches!(v, Value::Map(_)) || uint(v.at(1)) != Some(1) {
            return Err(bad(path, "not a version-1 pins file"));
        }
        let fs = uint(v.at(2))
            .and_then(|f| u32::try_from(f).ok())
            .ok_or_else(|| bad(path, "no fs"))?;
        let acl = match v.at(3) {
            None => None,
            x => Some(bytes(x).ok_or_else(|| bad(path, "bad ACL"))?.to_vec()),
        };
        let header = match v.at(4) {
            None => None,
            Some(Value::Array(a)) if a.len() == 2 => Some(Head {
                version: uint(a.first()).ok_or_else(|| bad(path, "bad header pin"))?,
                hash: fixed(a.get(1)).ok_or_else(|| bad(path, "bad header pin"))?,
            }),
            _ => return Err(bad(path, "bad header pin")),
        };
        let Some(Value::Array(devs)) = v.at(5) else {
            return Err(bad(path, "no devices"));
        };
        let devices = devs
            .iter()
            .map(|d| match d {
                Value::Array(a) if a.len() == 4 => Some(DeviceRecord {
                    device: fixed(a.first())?,
                    seq: uint(a.get(1))?,
                    hash: fixed(a.get(2))?,
                    body: bytes(a.get(3))?.to_vec(),
                }),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| bad(path, "bad device head"))?;
        Ok(PinsFile {
            fs,
            acl,
            header,
            devices,
        })
    }

    /// Load the pins of fs `fs`; `None` when the file doesn't exist.
    pub fn load(path: &Path, fs: u32) -> Result<Option<Self>> {
        let b = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
        };
        let p = Self::decode(path, &b)?;
        if p.fs != fs {
            return Err(bad(path, &format!("pins of fs {}, not {fs}", p.fs)));
        }
        Ok(Some(p))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(path, &self.encode()?)
    }
}

/// Write `bytes` to `path` atomically, readable by its owner only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::Io(format!("{}: not a file name", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(io)?;
    // The rename itself, durable.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_refusals() {
        let dir = std::env::temp_dir().join(format!("zen-pins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p");
        let p = PinsFile {
            fs: 3,
            acl: Some(vec![1, 2, 3]),
            header: Some(Head {
                version: 2,
                hash: [7; 32],
            }),
            devices: vec![DeviceRecord {
                device: [1; 32],
                seq: 4,
                hash: [2; 32],
                body: vec![0xA0],
            }],
        };
        p.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(PinsFile::load(&path, 3).unwrap(), Some(p));
        assert!(PinsFile::load(&path, 4).unwrap_err().is("format"));
        std::fs::write(&path, b"not cbor at all").unwrap();
        assert!(PinsFile::load(&path, 3).unwrap_err().is("format"));
        assert_eq!(PinsFile::load(&dir.join("absent"), 3).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
