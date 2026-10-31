//! The writer's local pin of a volume (spec/blk.md §6): its newest root
//! `(seq, RH)`, CBOR `{1: v = 1, 2: vol id, 3: seq, 4: RH}`, written
//! atomically after each commit. A file of another volume, or one that
//! doesn't decode, is refused: dropping the pin silently would hide a rollback.

use std::path::Path;
use zen_client::{Error, Result};
use zen_core::blk::Hash;
use zen_core::db::cbor::{self, Value};

/// A pinned root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pin {
    pub seq: u64,
    pub hash: Hash,
}

fn bad(path: &Path, why: &str) -> Error {
    Error::local("format", format!("{}: {why}", path.display()))
}

/// The pin in `path`, `None` when there is no file.
pub(crate) fn load(path: &Path, vol: &[u8]) -> Result<Option<Pin>> {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    let Ok(Value::Map(m)) = cbor::decode(&b) else {
        return Err(bad(path, "not a volume pin"));
    };
    let mut f: [Option<&Value>; 4] = [None; 4];
    for (k, x) in &m {
        match k {
            Value::Int(i @ 1..=4) => f[*i as usize - 1] = Some(x),
            _ => return Err(bad(path, "not a volume pin")),
        }
    }
    let [
        Some(Value::Int(1)),
        Some(Value::Bytes(id)),
        Some(Value::Int(seq)),
        Some(Value::Bytes(hash)),
    ] = f
    else {
        return Err(bad(path, "not a volume pin"));
    };
    if id != vol {
        return Err(bad(path, "the pin of another volume"));
    }
    let pin = Pin {
        seq: u64::try_from(*seq).map_err(|_| bad(path, "a bad seq"))?,
        hash: Hash::try_from(hash.as_slice()).map_err(|_| bad(path, "a bad hash"))?,
    };
    if encode(vol, &pin) != b {
        return Err(bad(path, "not a volume pin"));
    }
    Ok(Some(pin))
}

fn encode(vol: &[u8], pin: &Pin) -> Vec<u8> {
    let m = vec![
        (Value::Int(1), Value::Int(1)),
        (Value::Int(2), Value::Bytes(vol.to_vec())),
        (Value::Int(3), Value::Int(i128::from(pin.seq))),
        (Value::Int(4), Value::Bytes(pin.hash.to_vec())),
    ];
    cbor::encode(&Value::Map(m)).expect("valid pin")
}

/// Write the pin (atomically, readable by its owner only).
pub(crate) fn save(path: &Path, vol: &[u8], pin: &Pin) -> Result<()> {
    zen_client::write_private(path, &encode(vol, pin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_refusals() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("v.pin");
        assert_eq!(load(&p, b"vol").unwrap(), None);
        let pin = Pin {
            seq: 7,
            hash: [7; 32],
        };
        save(&p, b"vol", &pin).unwrap();
        assert_eq!(load(&p, b"vol").unwrap(), Some(pin));
        assert!(load(&p, b"other").unwrap_err().is("format"));
        std::fs::write(&p, b"junk").unwrap();
        assert!(load(&p, b"vol").unwrap_err().is("format"));
    }
}
