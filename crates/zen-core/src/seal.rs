//! Sealed objects: KV values, events, epoch-chain records (spec/formats.md §4).
//! Filesystem objects (kinds 4–6) are sealed in [`crate::fs`].
//!
//! ```text
//! off  len  field
//!   0    1  format_version (= 1)
//!   1    1  suite          (= 1, modern)
//!   2    1  kind           (1 = kv value, 2 = event, 3 = epoch-chain record,
//!                           4–6 = filesystem meta, manifest, chunk)
//!   3    1  reserved       (= 0)
//!   4    4  key_epoch      (u32 BE)
//!   8   24  nonce          (random per seal)
//!  32    n  ciphertext || 16-byte Poly1305 tag
//! ```
//!
//! AAD = `label || 0x00 || header[0..32] || kind-specific context`. The header is
//! authenticated, so its epoch and kind cannot be altered.

use crate::encoding::{Reader, lp};
use crate::kdf::Key32;
use crate::keys::FsKeys;
use crate::labels;
use crate::rng::{self, Rng};
use crate::suite::{FORMAT_VERSION, Suite, check_prefix};
use crate::token::TopicKeys;
use crate::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

/// Header length of a sealed object.
pub const HEADER_LEN: usize = 32;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

/// Object kind, byte 2 of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// An encrypted KV value.
    KvValue = 1,
    /// An encrypted event.
    Event = 2,
    /// A backward epoch-chain record (MK_{e-1} sealed under epoch e).
    EpochChain = 3,
    /// Filesystem node meta (spec/formats.md §11.2).
    FsMeta = 4,
    /// Filesystem manifest (§11.3).
    FsManifest = 5,
    /// Filesystem chunk (§11.4).
    FsChunk = 6,
}

fn header(kind: Kind, epoch: u32, nonce: &[u8; 24]) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0] = FORMAT_VERSION;
    h[1] = Suite::Modern as u8;
    h[2] = kind as u8;
    h[4..8].copy_from_slice(&epoch.to_be_bytes());
    h[8..32].copy_from_slice(nonce);
    h
}

fn full_aad(label: &str, header: &[u8], ctx: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(label.len() + 1 + header.len() + ctx.len());
    aad.extend_from_slice(label.as_bytes());
    aad.push(0);
    aad.extend_from_slice(header);
    aad.extend_from_slice(ctx);
    aad
}

/// Kind-specific AAD context: label plus bound fields.
pub(crate) struct Aad {
    pub(crate) label: &'static str,
    pub(crate) ctx: Vec<u8>,
}

pub(crate) fn aad_epoch_chain(fs_id: u32) -> Aad {
    Aad {
        label: labels::AAD_EPOCH_CHAIN,
        ctx: fs_id.to_be_bytes().to_vec(),
    }
}

fn aad_kv(fs_id: u32, stored_key: &[u8]) -> Aad {
    let mut ctx = fs_id.to_be_bytes().to_vec();
    lp(&mut ctx, stored_key);
    Aad {
        label: labels::AAD_KV,
        ctx,
    }
}

fn aad_event(fs_id: u32, topic_id: &[u8], key_token: Option<&[u8; 16]>) -> Aad {
    let mut ctx = fs_id.to_be_bytes().to_vec();
    lp(&mut ctx, topic_id);
    lp(&mut ctx, key_token.map_or(&[][..], |k| &k[..]));
    Aad {
        label: labels::AAD_EVENT,
        ctx,
    }
}

pub(crate) fn seal(
    kind: Kind,
    epoch: u32,
    key: &Key32,
    aad: &Aad,
    plaintext: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    let nonce: [u8; 24] = rng::array(rng)?;
    let h = header(kind, epoch, &nonce);
    let cipher = XChaCha20Poly1305::new(&Key::from(**key));
    let ad = full_aad(aad.label, &h, &aad.ctx);
    let ct = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &ad,
            },
        )
        .map_err(|_| Error::Param)?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(&h);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Parse only the header: returns `(kind, key_epoch)`. Lets a client pick the
/// right epoch key before decrypting.
pub fn peek(sealed: &[u8]) -> Result<(Kind, u32)> {
    check_prefix(sealed)?;
    if sealed.len() < HEADER_LEN + TAG_LEN || sealed[3] != 0 {
        return Err(Error::Format);
    }
    let kind = match sealed[2] {
        1 => Kind::KvValue,
        2 => Kind::Event,
        3 => Kind::EpochChain,
        4 => Kind::FsMeta,
        5 => Kind::FsManifest,
        6 => Kind::FsChunk,
        _ => return Err(Error::Format),
    };
    Ok((
        kind,
        u32::from_be_bytes(sealed[4..8].try_into().expect("checked")),
    ))
}

pub(crate) fn open(kind: Kind, key: &Key32, aad: &Aad, sealed: &[u8]) -> Result<(u32, Vec<u8>)> {
    let (k, epoch) = peek(sealed)?;
    if k != kind {
        return Err(Error::Format);
    }
    let (h, ct) = sealed.split_at(HEADER_LEN);
    let cipher = XChaCha20Poly1305::new(&Key::from(**key));
    let ad = full_aad(aad.label, h, &aad.ctx);
    let pt = cipher
        .decrypt(
            &XNonce::try_from(&h[8..32]).map_err(|_| Error::Format)?,
            Payload { msg: ct, aad: &ad },
        )
        .map_err(|_| Error::Decrypt)?;
    Ok((epoch, pt))
}

/// Seal a KV value. `stored_key` is the server-visible key token (see [`crate::token::kv_key`]);
/// binding it in the AAD stops the server from swapping values between keys or filesystems.
pub fn seal_value(
    fs: &FsKeys,
    stored_key: &[u8],
    plaintext: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    seal(
        Kind::KvValue,
        fs.epoch,
        &fs.kv_data_key(),
        &aad_kv(fs.fs_id, stored_key),
        plaintext,
        rng,
    )
}

/// Open a KV value. `fs` must be at the epoch recorded in the value ([`peek`]).
pub fn open_value(fs: &FsKeys, stored_key: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    let (epoch, pt) = open(
        Kind::KvValue,
        &fs.kv_data_key(),
        &aad_kv(fs.fs_id, stored_key),
        sealed,
    )?;
    if epoch != fs.epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
}

/// The authenticated plaintext of an event (spec/formats.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBody {
    /// Fingerprint of the sending device.
    pub sender: [u8; 32],
    /// Hybrid logical clock timestamp.
    pub hlc: u64,
    /// Id of the event that caused this one (empty if none).
    pub causation: Vec<u8>,
    /// Application payload.
    pub payload: Vec<u8>,
}

impl EventBody {
    /// `0x01 || sender[32] || u64(hlc) || lp(causation) || payload`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(1 + 32 + 8 + 4 + self.causation.len() + self.payload.len());
        out.push(1);
        out.extend_from_slice(&self.sender);
        out.extend_from_slice(&self.hlc.to_be_bytes());
        lp(&mut out, &self.causation);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode an event body.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        if r.u8()? != 1 {
            return Err(Error::Format);
        }
        let sender = r.array()?;
        let hlc = r.u64()?;
        let causation = r.lp()?.to_vec();
        Ok(EventBody {
            sender,
            hlc,
            causation,
            payload: r.rest().to_vec(),
        })
    }
}

/// Seal an event for `topic`, optionally bound to an event key token.
pub fn seal_event(
    fs: &FsKeys,
    topic: &TopicKeys,
    key_token: Option<&[u8; 16]>,
    body: &EventBody,
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    let aad = aad_event(fs.fs_id, topic.id(), key_token);
    seal(
        Kind::Event,
        fs.epoch,
        &topic.event_aead_key(),
        &aad,
        &body.encode(),
        rng,
    )
}

/// Open an event. `fs` and `topic` must be derived at the event's epoch.
pub fn open_event(
    fs: &FsKeys,
    topic: &TopicKeys,
    key_token: Option<&[u8; 16]>,
    sealed: &[u8],
) -> Result<EventBody> {
    let aad = aad_event(fs.fs_id, topic.id(), key_token);
    let (epoch, pt) = open(Kind::Event, &topic.event_aead_key(), &aad, sealed)?;
    if epoch != fs.epoch {
        return Err(Error::Decrypt);
    }
    EventBody::decode(&pt)
}
