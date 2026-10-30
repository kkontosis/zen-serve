//! Device checkpoints (spec/formats.md §14): a device's signed, hash-chained
//! statement of the newest state it has written or verified, per object.
//!
//! ```text
//! signed     = u32(len(body)) || body || hybrid signature("zen/v1/sig/checkpoint", body)
//! checkpoint = kind-11 sealed object of `signed`, context u32(fs) || device(32) || u64(seq)
//! ```

use crate::db::cbor::{self, Value};
use crate::encoding::Reader;
use crate::keys::FsKeys;
use crate::labels;
use crate::rng::Rng;
use crate::seal::{self, Aad, Kind};
use crate::sig::{DevicePublic, DeviceSecret, SIGNATURE_LEN};
use crate::{Error, Result};

/// Body version.
pub const CHECKPOINT_VERSION: u64 = 1;

/// A `(version, hash)` pair: the ACL or signed fs header a device verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Head {
    /// Version (0 with a zero hash: none).
    pub version: u64,
    /// `H(doc)` of the ACL, or `HH` of the signed header.
    pub hash: [u8; 32],
}

/// One of the device's own event chains on a topic (spec/formats.md §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicChain {
    /// The writer's chain id.
    pub chain_id: [u8; 12],
    /// The newest `dseq` that committed.
    pub dseq: u64,
    /// `HE(body)` of that event: the `dprev` of the next one.
    pub dhash: [u8; 32],
}

/// An object entry of a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Type 1: a topic's head and the device's chains on it.
    Topic {
        /// Topic id (16·n bytes).
        topic: Vec<u8>,
        /// Offset of the newest event read through or appended.
        head: [u8; 12],
        /// The device's own chains, sorted by `chain_id`.
        chains: Vec<TopicChain>,
    },
    /// Type 2: a filesystem tree (spec/fs.md §9).
    Tree {
        /// Tree id.
        tree: [u8; 16],
        /// Op count.
        ops: u64,
        /// Op chain at `ops`.
        chain: [u8; 32],
        /// Tree state at `ops`.
        state: [u8; 32],
    },
    /// Type 3: a zen-db authenticated namespace.
    Db {
        /// The namespace's stored-key prefix.
        ns: Vec<u8>,
        /// Its newest root's `seq`.
        seq: u64,
        /// Hash of that root.
        hash: [u8; 32],
    },
    /// Type 4: a block volume.
    Vol {
        /// The volume's stored-key prefix.
        vol: Vec<u8>,
        /// Its newest root's `seq`.
        seq: u64,
        /// Hash of that root.
        hash: [u8; 32],
    },
    /// Type 5: a relayed connection (spec/svc.md §12).
    Svc {
        /// Service id (a topic id).
        svc: Vec<u8>,
        /// Connection id.
        conn: [u8; 16],
        /// Input acked.
        in_acked: u64,
        /// Output head.
        out_head: u64,
    },
}

impl Entry {
    /// The entry type.
    pub fn type_id(&self) -> u8 {
        match self {
            Entry::Topic { .. } => 1,
            Entry::Tree { .. } => 2,
            Entry::Db { .. } => 3,
            Entry::Vol { .. } => 4,
            Entry::Svc { .. } => 5,
        }
    }

    /// Sort key: type, then the id bytes (`svc` then `conn` for type 5).
    pub fn sort_key(&self) -> (u8, &[u8], &[u8]) {
        let t = self.type_id();
        match self {
            Entry::Topic { topic, .. } => (t, topic, &[]),
            Entry::Tree { tree, .. } => (t, tree, &[]),
            Entry::Db { ns, .. } => (t, ns, &[]),
            Entry::Vol { vol, .. } => (t, vol, &[]),
            Entry::Svc { svc, conn, .. } => (t, svc, conn),
        }
    }

    fn to_value(&self) -> Value {
        let t = Value::Int(self.type_id().into());
        match self {
            Entry::Topic {
                topic,
                head,
                chains,
            } => Value::Array(vec![
                t,
                Value::Bytes(topic.clone()),
                Value::Bytes(head.to_vec()),
                Value::Array(
                    chains
                        .iter()
                        .map(|c| {
                            Value::Array(vec![
                                Value::Bytes(c.chain_id.to_vec()),
                                c.dseq.into(),
                                Value::Bytes(c.dhash.to_vec()),
                            ])
                        })
                        .collect(),
                ),
            ]),
            Entry::Tree {
                tree,
                ops,
                chain,
                state,
            } => Value::Array(vec![
                t,
                Value::Bytes(tree.to_vec()),
                (*ops).into(),
                Value::Bytes(chain.to_vec()),
                Value::Bytes(state.to_vec()),
            ]),
            Entry::Db { ns: id, seq, hash } | Entry::Vol { vol: id, seq, hash } => {
                Value::Array(vec![
                    t,
                    Value::Bytes(id.clone()),
                    (*seq).into(),
                    Value::Bytes(hash.to_vec()),
                ])
            }
            Entry::Svc {
                svc,
                conn,
                in_acked,
                out_head,
            } => Value::Array(vec![
                t,
                Value::Bytes(svc.clone()),
                Value::Bytes(conn.to_vec()),
                (*in_acked).into(),
                (*out_head).into(),
            ]),
        }
    }

    fn from_value(v: &Value) -> Result<Self> {
        let a = array(v)?;
        let (ty, rest) = a.split_first().ok_or(Error::Format)?;
        Ok(match (uint(ty)?, rest) {
            (1, [topic, head, chains]) => Entry::Topic {
                topic: bytes(topic)?.to_vec(),
                head: fixed(head)?,
                chains: array(chains)?
                    .iter()
                    .map(|c| match array(c)? {
                        [id, dseq, dhash] => Ok(TopicChain {
                            chain_id: fixed(id)?,
                            dseq: uint(dseq)?,
                            dhash: fixed(dhash)?,
                        }),
                        _ => Err(Error::Format),
                    })
                    .collect::<Result<_>>()?,
            },
            (2, [tree, ops, chain, state]) => Entry::Tree {
                tree: fixed(tree)?,
                ops: uint(ops)?,
                chain: fixed(chain)?,
                state: fixed(state)?,
            },
            (3, [ns, seq, hash]) => Entry::Db {
                ns: bytes(ns)?.to_vec(),
                seq: uint(seq)?,
                hash: fixed(hash)?,
            },
            (4, [vol, seq, hash]) => Entry::Vol {
                vol: bytes(vol)?.to_vec(),
                seq: uint(seq)?,
                hash: fixed(hash)?,
            },
            (5, [svc, conn, in_acked, out_head]) => Entry::Svc {
                svc: bytes(svc)?.to_vec(),
                conn: fixed(conn)?,
                in_acked: uint(in_acked)?,
                out_head: uint(out_head)?,
            },
            _ => return Err(Error::Format),
        })
    }

    fn check(&self) -> Result<()> {
        match self {
            Entry::Topic { topic, chains, .. } => {
                check_topic(topic)?;
                if !chains.windows(2).all(|w| w[0].chain_id < w[1].chain_id)
                    || chains.iter().any(|c| c.dseq == 0)
                {
                    return Err(Error::Format);
                }
            }
            Entry::Svc { svc, .. } => check_topic(svc)?,
            Entry::Db { ns: id, .. } | Entry::Vol { vol: id, .. } => {
                if id.is_empty() {
                    return Err(Error::Format);
                }
            }
            Entry::Tree { .. } => {}
        }
        Ok(())
    }
}

/// Another device's head the writer has verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    /// The device.
    pub device: [u8; 32],
    /// Its head seq.
    pub seq: u64,
    /// `HC` of that body.
    pub hash: [u8; 32],
}

/// The plaintext body of a device checkpoint (spec/formats.md §14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointBody {
    /// The fs.
    pub fs: u32,
    /// The signing device's fingerprint.
    pub device: [u8; 32],
    /// 1, 2, 3, … per (fs, device).
    pub seq: u64,
    /// `HC` of the body of `seq − 1`; zeros for seq 1.
    pub prev: [u8; 32],
    /// The device's clock, informative.
    pub hlc: u64,
    /// The ACL head the device verified.
    pub acl: Head,
    /// The signed fs header it verified (`Head::default()` if none).
    pub header: Head,
    /// Object entries, sorted by [`Entry::sort_key`], one per object.
    pub objects: Vec<Entry>,
    /// Other devices' heads, sorted by device, never the signer.
    pub seen: Vec<Seen>,
}

impl CheckpointBody {
    fn check(&self) -> Result<()> {
        if self.seq == 0 || (self.seq == 1 && self.prev != [0; 32]) {
            return Err(Error::Format);
        }
        for e in &self.objects {
            e.check()?;
        }
        if !self
            .objects
            .windows(2)
            .all(|w| w[0].sort_key() < w[1].sort_key())
        {
            return Err(Error::Format);
        }
        if !self.seen.windows(2).all(|w| w[0].device < w[1].device)
            || self
                .seen
                .iter()
                .any(|s| s.device == self.device || s.seq == 0)
        {
            return Err(Error::Format);
        }
        Ok(())
    }

    /// Sort `objects` and `seen` into canonical order. Duplicates stay and
    /// make [`CheckpointBody::encode`] fail.
    pub fn sort(&mut self) {
        self.objects.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        for e in &mut self.objects {
            if let Entry::Topic { chains, .. } = e {
                chains.sort_by_key(|c| c.chain_id);
            }
        }
        self.seen.sort_by_key(|s| s.device);
    }

    /// Deterministic CBOR. Fails on a body that breaks the rules of §14.1.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        let head = |h: &Head| Value::Array(vec![h.version.into(), Value::Bytes(h.hash.to_vec())]);
        let v = Value::Map(vec![
            (Value::Int(1), CHECKPOINT_VERSION.into()),
            (Value::Int(2), u64::from(self.fs).into()),
            (Value::Int(3), Value::Bytes(self.device.to_vec())),
            (Value::Int(4), self.seq.into()),
            (Value::Int(5), Value::Bytes(self.prev.to_vec())),
            (Value::Int(6), self.hlc.into()),
            (Value::Int(7), head(&self.acl)),
            (Value::Int(8), head(&self.header)),
            (
                Value::Int(9),
                Value::Array(self.objects.iter().map(Entry::to_value).collect()),
            ),
            (
                Value::Int(10),
                Value::Array(
                    self.seen
                        .iter()
                        .map(|s| {
                            Value::Array(vec![
                                Value::Bytes(s.device.to_vec()),
                                s.seq.into(),
                                Value::Bytes(s.hash.to_vec()),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        cbor::encode(&v)
    }

    /// Decode a body, refusing anything but its canonical encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let v = cbor::decode(bytes)?;
        let Value::Map(m) = &v else {
            return Err(Error::Format);
        };
        if m.len() != 10 || v.at(1).map(uint).transpose()? != Some(CHECKPOINT_VERSION) {
            return Err(Error::Format);
        }
        let get = |k: i128| v.at(k).ok_or(Error::Format);
        let head = |x: &Value| -> Result<Head> {
            match array(x)? {
                [version, hash] => Ok(Head {
                    version: uint(version)?,
                    hash: fixed(hash)?,
                }),
                _ => Err(Error::Format),
            }
        };
        let body = CheckpointBody {
            fs: u32::try_from(uint(get(2)?)?).map_err(|_| Error::Format)?,
            device: fixed(get(3)?)?,
            seq: uint(get(4)?)?,
            prev: fixed(get(5)?)?,
            hlc: uint(get(6)?)?,
            acl: head(get(7)?)?,
            header: head(get(8)?)?,
            objects: array(get(9)?)?
                .iter()
                .map(Entry::from_value)
                .collect::<Result<_>>()?,
            seen: array(get(10)?)?
                .iter()
                .map(|s| match array(s)? {
                    [device, seq, hash] => Ok(Seen {
                        device: fixed(device)?,
                        seq: uint(seq)?,
                        hash: fixed(hash)?,
                    }),
                    _ => Err(Error::Format),
                })
                .collect::<Result<_>>()?,
        };
        // Canonical form only: the checks pass and the bytes re-encode exactly.
        if body.encode()? != bytes {
            return Err(Error::Format);
        }
        Ok(body)
    }
}

/// `HC(body) = BLAKE3.derive_key("zen/v1/checkpoint-hash", body)`.
pub fn checkpoint_hash(body: &[u8]) -> [u8; 32] {
    blake3::derive_key(labels::CHECKPOINT_HASH, body)
}

/// Sign a body with the device's identity: `u32(len) || body || sig`.
/// `body.device` must be the device's fingerprint.
pub fn sign_checkpoint(device: &DeviceSecret, body: &CheckpointBody) -> Result<Vec<u8>> {
    if body.device != device.public().fingerprint() {
        return Err(Error::Param);
    }
    let bytes = body.encode()?;
    let sig = device.signing().sign(labels::SIG_CHECKPOINT, &bytes)?;
    let len = u32::try_from(bytes.len()).map_err(|_| Error::Param)?;
    let mut out = Vec::with_capacity(4 + bytes.len() + sig.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes);
    out.extend_from_slice(&sig);
    Ok(out)
}

/// Split `signed` into `(body bytes, signature)` without verifying.
pub fn split_signed(signed: &[u8]) -> Result<(&[u8], &[u8])> {
    let mut r = Reader::new(signed);
    let n = r.u32()? as usize;
    let body = r.take(n)?;
    let sig = r.rest();
    if sig.len() != SIGNATURE_LEN {
        return Err(Error::Format);
    }
    Ok((body, sig))
}

/// Verify a signed checkpoint with the device's public keys (from a
/// certificate the caller has verified), and decode its body. The body must
/// name that device.
pub fn verify_checkpoint(device: &DevicePublic, signed: &[u8]) -> Result<CheckpointBody> {
    let (bytes, sig) = split_signed(signed)?;
    device
        .signing()
        .verify(labels::SIG_CHECKPOINT, bytes, sig)?;
    let body = CheckpointBody::decode(bytes)?;
    if body.device != device.fingerprint() {
        return Err(Error::Signature);
    }
    Ok(body)
}

fn aad_checkpoint(fs_id: u32, device: &[u8; 32], seq: u64) -> Aad {
    let mut ctx = Vec::with_capacity(44);
    ctx.extend_from_slice(&fs_id.to_be_bytes());
    ctx.extend_from_slice(device);
    ctx.extend_from_slice(&seq.to_be_bytes());
    Aad {
        label: labels::AAD_CHECKPOINT,
        ctx,
    }
}

/// Seal a signed checkpoint (kind 11) under the KV key of `fs`'s epoch.
pub fn seal_checkpoint(
    fs: &FsKeys,
    device: &[u8; 32],
    seq: u64,
    signed: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    seal::seal(
        Kind::Checkpoint,
        fs.epoch,
        &fs.kv_data_key(),
        &aad_checkpoint(fs.fs_id, device, seq),
        signed,
        rng,
    )
}

/// Open a sealed checkpoint. `fs` must be at the epoch in its header
/// ([`seal::peek`]).
pub fn open_checkpoint(fs: &FsKeys, device: &[u8; 32], seq: u64, sealed: &[u8]) -> Result<Vec<u8>> {
    let (epoch, pt) = seal::open(
        Kind::Checkpoint,
        &fs.kv_data_key(),
        &aad_checkpoint(fs.fs_id, device, seq),
        sealed,
    )?;
    if epoch != fs.epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
}

fn check_topic(t: &[u8]) -> Result<()> {
    if t.is_empty() || !t.len().is_multiple_of(16) || t.len() > 256 {
        return Err(Error::Format);
    }
    Ok(())
}

fn array(v: &Value) -> Result<&[Value]> {
    match v {
        Value::Array(a) => Ok(a),
        _ => Err(Error::Format),
    }
}

fn bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Bytes(b) => Ok(b),
        _ => Err(Error::Format),
    }
}

fn fixed<const N: usize>(v: &Value) -> Result<[u8; N]> {
    bytes(v)?.try_into().map_err(|_| Error::Format)
}

fn uint(v: &Value) -> Result<u64> {
    match v {
        Value::Int(i) => u64::try_from(*i).map_err(|_| Error::Format),
        _ => Err(Error::Format),
    }
}

/// Version byte of [`ExportedPins`].
pub const PINS_VERSION: u8 = 1;

/// A client's pins, exported for an out-of-band comparison (spec/formats.md §14.5):
///
/// ```text
/// u8 1 || u32 fs || acl(u64 version || hash32) || header(u64 version || hash32)
///      || u32 n || n × (device(32) || u64 seq || HC(32))
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedPins {
    /// The fs.
    pub fs: u32,
    /// The pinned ACL head (`Head::default()` if none).
    pub acl: Head,
    /// The pinned signed header (`Head::default()` if none).
    pub header: Head,
    /// Every pinned device head, sorted by device.
    pub devices: Vec<Seen>,
}

impl ExportedPins {
    /// Encode. Fails when `devices` is out of order or repeats one.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if !self.devices.windows(2).all(|w| w[0].device < w[1].device) {
            return Err(Error::Format);
        }
        let n = u32::try_from(self.devices.len()).map_err(|_| Error::Format)?;
        let mut out = Vec::with_capacity(89 + 72 * self.devices.len());
        out.push(PINS_VERSION);
        out.extend_from_slice(&self.fs.to_be_bytes());
        for h in [&self.acl, &self.header] {
            out.extend_from_slice(&h.version.to_be_bytes());
            out.extend_from_slice(&h.hash);
        }
        out.extend_from_slice(&n.to_be_bytes());
        for d in &self.devices {
            out.extend_from_slice(&d.device);
            out.extend_from_slice(&d.seq.to_be_bytes());
            out.extend_from_slice(&d.hash);
        }
        Ok(out)
    }

    /// Decode, refusing another version, a wrong length and unsorted devices.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != PINS_VERSION {
            return Err(Error::Format);
        }
        let fs = r.u32()?;
        let mut head = || -> Result<Head> {
            Ok(Head {
                version: r.u64()?,
                hash: r.array()?,
            })
        };
        let (acl, header) = (head()?, head()?);
        let n = r.u32()? as usize;
        if bytes.len() != 89 + 72 * n {
            return Err(Error::Format);
        }
        let mut devices = Vec::with_capacity(n);
        for _ in 0..n {
            devices.push(Seen {
                device: r.array()?,
                seq: r.u64()?,
                hash: r.array()?,
            });
        }
        r.finish()?;
        let p = ExportedPins {
            fs,
            acl,
            header,
            devices,
        };
        p.encode()?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::DetRng;

    pub(crate) fn body(device: [u8; 32]) -> CheckpointBody {
        CheckpointBody {
            fs: 7,
            device,
            seq: 1,
            prev: [0; 32],
            hlc: 1_790_000_000_000 << 16,
            acl: Head {
                version: 3,
                hash: [0xA1; 32],
            },
            header: Head::default(),
            objects: vec![
                Entry::Topic {
                    topic: vec![0x10; 32],
                    head: [0x01; 12],
                    chains: vec![TopicChain {
                        chain_id: [0x0C; 12],
                        dseq: 5,
                        dhash: [0xC5; 32],
                    }],
                },
                Entry::Tree {
                    tree: [0x11; 16],
                    ops: 42,
                    chain: [0x42; 32],
                    state: [0x43; 32],
                },
                Entry::Svc {
                    svc: vec![0x50; 16],
                    conn: [0x51; 16],
                    in_acked: 10,
                    out_head: 20,
                },
            ],
            seen: vec![Seen {
                device: [0x0B; 32],
                seq: 9,
                hash: [0xB9; 32],
            }],
        }
    }

    #[test]
    fn round_trip_and_canonical() {
        let b = body([0xDD; 32]);
        let bytes = b.encode().unwrap();
        assert_eq!(CheckpointBody::decode(&bytes).unwrap(), b);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(CheckpointBody::decode(&trailing).is_err());
    }

    #[test]
    fn refuses_bad_bodies() {
        let good = body([0xDD; 32]);
        let mut unsorted = good.clone();
        unsorted.objects.swap(0, 1);
        assert_eq!(unsorted.encode(), Err(Error::Format));
        let mut sorted = unsorted.clone();
        sorted.sort();
        assert_eq!(sorted, good);
        let mut dup = good.clone();
        dup.objects.push(dup.objects[2].clone());
        assert_eq!(dup.encode(), Err(Error::Format));
        let mut own = good.clone();
        own.seen[0].device = own.device;
        assert_eq!(own.encode(), Err(Error::Format));
        let mut prev = good.clone();
        prev.prev = [1; 32];
        assert_eq!(prev.encode(), Err(Error::Format));
        let mut topic = good.clone();
        if let Entry::Topic { topic, .. } = &mut topic.objects[0] {
            topic.push(0);
        }
        assert_eq!(topic.encode(), Err(Error::Format));
        // Another version, through a hand-built map.
        let bytes = good.encode().unwrap();
        let mut v = cbor::decode(&bytes).unwrap();
        if let Value::Map(m) = &mut v {
            m[0].1 = Value::Int(2);
        }
        assert!(CheckpointBody::decode(&cbor::encode(&v).unwrap()).is_err());
    }

    #[test]
    fn sign_verify_seal_open() {
        let dev = DeviceSecret::generate(&mut DetRng::new("ck-dev"))
            .unwrap()
            .0;
        let other = DeviceSecret::generate(&mut DetRng::new("ck-other"))
            .unwrap()
            .0;
        let fp = dev.public().fingerprint();
        let b = body(fp);
        let signed = sign_checkpoint(&dev, &b).unwrap();
        assert_eq!(verify_checkpoint(&dev.public(), &signed).unwrap(), b);
        // Wrong device key, flipped body bit, signer not the body's device.
        assert!(verify_checkpoint(&other.public(), &signed).is_err());
        let mut flipped = signed.clone();
        flipped[10] ^= 1;
        assert!(verify_checkpoint(&dev.public(), &flipped).is_err());
        assert_eq!(sign_checkpoint(&other, &b), Err(Error::Param));

        let fs = FsKeys::generate(7, &mut DetRng::new("fs")).unwrap();
        let sealed = seal_checkpoint(&fs, &fp, 1, &signed, &mut DetRng::new("ck-seal")).unwrap();
        assert_eq!(seal::peek(&sealed).unwrap(), (Kind::Checkpoint, 0));
        assert_eq!(open_checkpoint(&fs, &fp, 1, &sealed).unwrap(), signed);
        assert!(open_checkpoint(&fs, &fp, 2, &sealed).is_err());
        assert!(open_checkpoint(&fs, &[0; 32], 1, &sealed).is_err());
    }

    #[test]
    fn exported_pins_round_trip_and_strict() {
        let p = ExportedPins {
            fs: 7,
            acl: Head {
                version: 3,
                hash: [0xAC; 32],
            },
            header: Head::default(),
            devices: vec![
                Seen {
                    device: [0x01; 32],
                    seq: 2,
                    hash: [0x12; 32],
                },
                Seen {
                    device: [0x02; 32],
                    seq: 9,
                    hash: [0x29; 32],
                },
            ],
        };
        let b = p.encode().unwrap();
        assert_eq!(b.len(), 89 + 2 * 72);
        assert_eq!(ExportedPins::decode(&b).unwrap(), p);
        let mut bad = b.clone();
        bad[0] = 2;
        assert!(ExportedPins::decode(&bad).is_err());
        assert!(ExportedPins::decode(&b[..b.len() - 1]).is_err());
        let mut longer = b.clone();
        longer.push(0);
        assert!(ExportedPins::decode(&longer).is_err());
        let mut swapped = p.clone();
        swapped.devices.reverse();
        assert!(swapped.encode().is_err());
        let mut unsorted = b.clone();
        unsorted[89..89 + 32].fill(0x03);
        assert!(ExportedPins::decode(&unsorted).is_err());
    }
}
