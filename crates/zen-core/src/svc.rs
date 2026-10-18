//! Relayed services (spec/svc.md, spec/formats.md §13): the sealed state of a
//! connection (kind 9), output records with their key escrow (kind 10) and the
//! reveal check, and the sealed inbox.
//!
//! ```text
//! sealed record: u8 1 || u8 flags || key_hash(32) || escrow(80) || ChaCha20-Poly1305(k_r, 0^12, aad, raw)
//! plain record:  u8 1 || u8 flags (PLAIN set) || raw
//! ```

use crate::encoding::lp;
use crate::kdf::{Key32, kdf};
use crate::keys::FsKeys;
use crate::labels;
use crate::rng::{self, Rng};
use crate::seal::{self, Aad, Kind};
use crate::token::TopicKeys;
use crate::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use x_wing::{Decapsulate, Decapsulator, KeyExport};
use zeroize::Zeroizing;

/// Record flag: shut down the socket's write side after this record.
pub const FIN: u8 = 1;
/// Record flag: close the connection after this record.
pub const CLOSE: u8 = 2;
/// Record flag: a plain record (`resume: false`), no key fields.
pub const PLAIN: u8 = 4;
/// Record flag: reset the connection when the writer reaches this record.
pub const ABORT: u8 = 8;
/// Bytes a sealed record adds to its raw bytes: version, flags, key hash,
/// escrow and tag.
pub const RECORD_OVERHEAD: usize = 2 + 32 + ESCROW_LEN + TAG_LEN;
/// Length of a kind-10 escrow: a sealed 32-byte key.
pub const ESCROW_LEN: usize = seal::HEADER_LEN + 32 + seal::TAG_LEN;
/// Length of an inbox public key (X-Wing encapsulation key).
pub const INBOX_PUBLIC_LEN: usize = x_wing::ENCAPSULATION_KEY_SIZE;
/// Length of an inbox encapsulation (X-Wing ciphertext).
pub const INBOX_CT_LEN: usize = x_wing::CIPHERTEXT_SIZE;

const TAG_LEN: usize = 16;
const RECORD_VERSION: u8 = 1;

/// The keys of one relayed service at one epoch: what a handler needs to seal
/// and open states and escrows.
pub struct SvcKeys {
    /// The fs the service belongs to.
    pub fs_id: u32,
    /// The epoch of `K_svc`.
    pub epoch: u32,
    id: Vec<u8>,
    key: Key32,
}

impl SvcKeys {
    /// The keys of service `topic` (its id is a topic id) at the fs's epoch.
    pub fn new(fs: &FsKeys, topic: &TopicKeys) -> Self {
        SvcKeys {
            fs_id: fs.fs_id,
            epoch: fs.epoch,
            id: topic.id().to_vec(),
            key: topic.svc_aead_key(),
        }
    }

    /// From a delegated `K_svc` (spec/svc.md §8).
    pub fn from_parts(fs_id: u32, epoch: u32, id: &[u8], key: [u8; 32]) -> Self {
        SvcKeys {
            fs_id,
            epoch,
            id: id.to_vec(),
            key: Zeroizing::new(key),
        }
    }

    /// The service id.
    pub fn id(&self) -> &[u8] {
        &self.id
    }

    /// `K_svc`, for delegating the service.
    pub fn key(&self) -> &Key32 {
        &self.key
    }
}

fn conn_ctx(fs_id: u32, svc: &[u8], conn: &[u8; 16]) -> Vec<u8> {
    let mut ctx = fs_id.to_be_bytes().to_vec();
    lp(&mut ctx, svc);
    ctx.extend_from_slice(conn);
    ctx
}

fn state_aad(k: &SvcKeys, conn: &[u8; 16], in_acked: u64, out_head: u64) -> Aad {
    let mut ctx = conn_ctx(k.fs_id, &k.id, conn);
    ctx.extend_from_slice(&in_acked.to_be_bytes());
    ctx.extend_from_slice(&out_head.to_be_bytes());
    Aad {
        label: labels::AAD_SVC_STATE,
        ctx,
    }
}

fn key_aad(k: &SvcKeys, conn: &[u8; 16], off: u64) -> Aad {
    let mut ctx = conn_ctx(k.fs_id, &k.id, conn);
    ctx.extend_from_slice(&off.to_be_bytes());
    Aad {
        label: labels::AAD_SVC_KEY,
        ctx,
    }
}

/// Seal a connection's state (kind 9), bound to the cursors after the step
/// that writes it.
pub fn seal_state(
    k: &SvcKeys,
    conn: &[u8; 16],
    in_acked: u64,
    out_head: u64,
    plaintext: &[u8],
    rng: &mut dyn Rng,
) -> Result<Vec<u8>> {
    seal::seal(
        Kind::SvcState,
        k.epoch,
        &k.key,
        &state_aad(k, conn, in_acked, out_head),
        plaintext,
        rng,
    )
}

/// Open a connection's state with the cursors a claim returned. `k` must be
/// at the state's epoch ([`seal::peek`]).
pub fn open_state(
    k: &SvcKeys,
    conn: &[u8; 16],
    in_acked: u64,
    out_head: u64,
    sealed: &[u8],
) -> Result<Vec<u8>> {
    let (epoch, pt) = seal::open(
        Kind::SvcState,
        &k.key,
        &state_aad(k, conn, in_acked, out_head),
        sealed,
    )?;
    if epoch != k.epoch {
        return Err(Error::Decrypt);
    }
    Ok(pt)
}

fn check_flags(flags: u8, raw_len: usize) -> Result<()> {
    let ends = flags & (FIN | CLOSE | ABORT);
    if flags & !(FIN | CLOSE | PLAIN | ABORT) != 0 || ends.count_ones() > 1 {
        return Err(Error::Format);
    }
    if raw_len == 0 && ends == 0 {
        return Err(Error::Format);
    }
    Ok(())
}

fn body_aad(fs_id: u32, svc: &[u8], conn: &[u8; 16], off: u64, flags: u8) -> Vec<u8> {
    let mut aad = labels::AAD_SVC_OUT.as_bytes().to_vec();
    aad.push(0);
    aad.extend_from_slice(&conn_ctx(fs_id, svc, conn));
    aad.extend_from_slice(&off.to_be_bytes());
    aad.push(flags);
    aad
}

/// `BLAKE3.derive_key("zen/v1/svc-reveal", k_r)`.
pub fn key_hash(k_r: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key(labels::SVC_REVEAL, k_r)
}

/// Seal an output record at output offset `off`. `flags` may hold one of
/// [`FIN`], [`CLOSE`], [`ABORT`]; an empty record must. Returns the record
/// and its key `k_r`, which is revealed only after the commit is known.
pub fn seal_record(
    k: &SvcKeys,
    conn: &[u8; 16],
    off: u64,
    flags: u8,
    raw: &[u8],
    rng: &mut dyn Rng,
) -> Result<(Vec<u8>, Key32)> {
    if flags & PLAIN != 0 {
        return Err(Error::Param);
    }
    check_flags(flags, raw.len()).map_err(|_| Error::Param)?;
    let k_r = Zeroizing::new(rng::array::<32>(rng)?);
    let escrow = seal::seal(
        Kind::SvcKey,
        k.epoch,
        &k.key,
        &key_aad(k, conn, off),
        &k_r[..],
        rng,
    )?;
    let body = ChaCha20Poly1305::new(&Key::from(*k_r))
        .encrypt(
            &Nonce::from([0u8; 12]),
            Payload {
                msg: raw,
                aad: &body_aad(k.fs_id, &k.id, conn, off, flags),
            },
        )
        .map_err(|_| Error::Param)?;
    let mut out = Vec::with_capacity(RECORD_OVERHEAD + raw.len());
    out.push(RECORD_VERSION);
    out.push(flags);
    out.extend_from_slice(&key_hash(&k_r));
    out.extend_from_slice(&escrow);
    out.extend_from_slice(&body);
    Ok((out, k_r))
}

/// A plain record (`resume: false`): [`PLAIN`] is added to `flags`.
pub fn plain_record(flags: u8, raw: &[u8]) -> Result<Vec<u8>> {
    let flags = flags | PLAIN;
    check_flags(flags, raw.len()).map_err(|_| Error::Param)?;
    let mut out = Vec::with_capacity(2 + raw.len());
    out.push(RECORD_VERSION);
    out.push(flags);
    out.extend_from_slice(raw);
    Ok(out)
}

/// A parsed output record.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// The record's flags.
    pub flags: u8,
    /// `key_hash` of a sealed record.
    pub key_hash: Option<&'a [u8; 32]>,
    /// The kind-10 escrow of a sealed record.
    pub escrow: Option<&'a [u8]>,
    /// The sealed body, or a plain record's raw bytes.
    pub body: &'a [u8],
}

impl Record<'_> {
    /// Whether the record is plain.
    pub fn is_plain(&self) -> bool {
        self.flags & PLAIN != 0
    }

    /// The raw bytes the record carries, which advance `out_head`.
    pub fn raw_len(&self) -> usize {
        if self.is_plain() {
            self.body.len()
        } else {
            self.body.len() - TAG_LEN
        }
    }
}

/// Parse a record and check its shape (spec/formats.md §13.1): version,
/// flags, lengths, and that an empty record ends the stream. What a server
/// checks before storing a step's records.
pub fn parse_record(bytes: &[u8]) -> Result<Record<'_>> {
    if bytes.len() < 2 || bytes[0] != RECORD_VERSION {
        return Err(Error::Format);
    }
    let flags = bytes[1];
    let rec = if flags & PLAIN != 0 {
        Record {
            flags,
            key_hash: None,
            escrow: None,
            body: &bytes[2..],
        }
    } else {
        if bytes.len() < RECORD_OVERHEAD {
            return Err(Error::Format);
        }
        let escrow = &bytes[34..34 + ESCROW_LEN];
        if seal::peek(escrow)?.0 != Kind::SvcKey {
            return Err(Error::Format);
        }
        Record {
            flags,
            key_hash: Some(bytes[2..34].try_into().expect("32 bytes")),
            escrow: Some(escrow),
            body: &bytes[34 + ESCROW_LEN..],
        }
    };
    check_flags(flags, rec.raw_len())?;
    Ok(rec)
}

/// Open the escrow of the sealed record at `off`: its key `k_r`, for a
/// successor's re-reveal. `k` must be at the escrow's epoch.
pub fn open_escrow(k: &SvcKeys, conn: &[u8; 16], off: u64, record: &[u8]) -> Result<Key32> {
    let escrow = parse_record(record)?.escrow.ok_or(Error::Format)?;
    let (epoch, pt) = seal::open(Kind::SvcKey, &k.key, &key_aad(k, conn, off), escrow)?;
    if epoch != k.epoch {
        return Err(Error::Decrypt);
    }
    let k_r: [u8; 32] = pt.as_slice().try_into().map_err(|_| Error::Format)?;
    Ok(Zeroizing::new(k_r))
}

/// The reveal check (spec/formats.md §13.2), run by the server: returns the
/// raw bytes. [`Error::Param`] when `k_r` isn't the record's key (`key_hash`
/// differs), [`Error::Decrypt`] when it is but the body doesn't open (a bad
/// record, which can never be written).
pub fn reveal(
    fs_id: u32,
    svc: &[u8],
    conn: &[u8; 16],
    off: u64,
    record: &[u8],
    k_r: &[u8; 32],
) -> Result<Vec<u8>> {
    let rec = parse_record(record)?;
    let hash = rec.key_hash.ok_or(Error::Format)?;
    if key_hash(k_r) != *hash {
        return Err(Error::Param);
    }
    ChaCha20Poly1305::new(&Key::from(*k_r))
        .decrypt(
            &Nonce::from([0u8; 12]),
            Payload {
                msg: rec.body,
                aad: &body_aad(fs_id, svc, conn, off, rec.flags),
            },
        )
        .map_err(|_| Error::Decrypt)
}

/// The public key of an inbox from its 32-byte X-Wing decapsulation seed.
pub fn inbox_public(seed: &[u8; 32]) -> Vec<u8> {
    x_wing::DecapsulationKey::from(*seed)
        .encapsulation_key()
        .to_bytes()
        .to_vec()
}

/// The key sealing one connection's input chunks.
pub struct InboxKey(Key32);

fn inbox_key(ss: &[u8; 32], fs_id: u32, svc: &[u8], conn: &[u8; 16]) -> InboxKey {
    InboxKey(kdf(labels::SVC_INBOX, ss, &conn_ctx(fs_id, svc, conn)))
}

/// Encapsulate to an inbox public key for one connection (the owning node,
/// once per connection). Returns the ciphertext for the connection record
/// and the chunk key.
pub fn inbox_encaps(
    public: &[u8],
    fs_id: u32,
    svc: &[u8],
    conn: &[u8; 16],
    rng: &mut dyn Rng,
) -> Result<(Vec<u8>, InboxKey)> {
    let ek = x_wing::EncapsulationKey::try_from(public).map_err(|_| Error::Format)?;
    let randomness = Zeroizing::new(rng::array::<{ x_wing::ENCAPSULATION_RANDOMNESS_SIZE }>(
        rng,
    )?);
    let (ct, ss) = ek.encapsulate_deterministic(&(*randomness).into());
    let ss = Zeroizing::new(<[u8; 32]>::from(ss));
    Ok((ct.to_vec(), inbox_key(&ss, fs_id, svc, conn)))
}

/// Decapsulate a connection's inbox ciphertext with the inbox seed.
pub fn inbox_decaps(
    seed: &[u8; 32],
    ct: &[u8],
    fs_id: u32,
    svc: &[u8],
    conn: &[u8; 16],
) -> Result<InboxKey> {
    let ct = x_wing::Ciphertext::try_from(ct).map_err(|_| Error::Format)?;
    let ss = Zeroizing::new(<[u8; 32]>::from(
        x_wing::DecapsulationKey::from(*seed).decapsulate(&ct),
    ));
    Ok(inbox_key(&ss, fs_id, svc, conn))
}

fn chunk_nonce(off: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&off.to_be_bytes());
    Nonce::from(n)
}

fn chunk_aad() -> Vec<u8> {
    let mut aad = labels::AAD_SVC_IN.as_bytes().to_vec();
    aad.push(0);
    aad
}

impl InboxKey {
    /// Seal the input chunk at plaintext offset `off`: ciphertext and tag.
    pub fn seal_chunk(&self, off: u64, data: &[u8]) -> Result<Vec<u8>> {
        ChaCha20Poly1305::new(&Key::from(*self.0))
            .encrypt(
                &chunk_nonce(off),
                Payload {
                    msg: data,
                    aad: &chunk_aad(),
                },
            )
            .map_err(|_| Error::Param)
    }

    /// Open the sealed chunk at plaintext offset `off`.
    pub fn open_chunk(&self, off: u64, sealed: &[u8]) -> Result<Vec<u8>> {
        ChaCha20Poly1305::new(&Key::from(*self.0))
            .decrypt(
                &chunk_nonce(off),
                Payload {
                    msg: sealed,
                    aad: &chunk_aad(),
                },
            )
            .map_err(|_| Error::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::DetRng;

    fn keys() -> (SvcKeys, SvcKeys) {
        let fs = FsKeys::from_parts(7, 0, [1; 32], [2; 32]);
        (
            SvcKeys::new(&fs, &TopicKeys::new(&fs, &["svc", "ssh"])),
            SvcKeys::new(&fs, &TopicKeys::new(&fs, &["svc", "smtp"])),
        )
    }

    const C1: [u8; 16] = [1; 16];
    const C2: [u8; 16] = [2; 16];

    #[test]
    fn state_is_bound_to_connection_and_cursors() {
        let (a, b) = keys();
        let s = seal_state(&a, &C1, 10, 20, b"st", &mut DetRng::new("s")).unwrap();
        assert_eq!(s[2], Kind::SvcState as u8);
        assert_eq!(open_state(&a, &C1, 10, 20, &s).unwrap(), b"st");
        assert!(open_state(&a, &C1, 11, 20, &s).is_err());
        assert!(open_state(&a, &C1, 10, 21, &s).is_err());
        assert!(open_state(&a, &C2, 10, 20, &s).is_err());
        assert!(open_state(&b, &C1, 10, 20, &s).is_err());
        // A later epoch's keys don't open it.
        let later = SvcKeys::from_parts(7, 1, a.id(), **a.key());
        assert!(open_state(&later, &C1, 10, 20, &s).is_err());
    }

    #[test]
    fn records_reveal_only_with_their_key_and_place() {
        let (a, b) = keys();
        let mut rng = DetRng::new("r");
        let (r, k_r) = seal_record(&a, &C1, 5, 0, b"hello", &mut rng).unwrap();
        assert_eq!(r.len(), RECORD_OVERHEAD + 5);
        assert_eq!(parse_record(&r).unwrap().raw_len(), 5);
        assert_eq!(reveal(7, a.id(), &C1, 5, &r, &k_r).unwrap(), b"hello");
        assert_eq!(*open_escrow(&a, &C1, 5, &r).unwrap(), *k_r);
        // Another key: refused by key_hash.
        assert_eq!(reveal(7, a.id(), &C1, 5, &r, &[9; 32]), Err(Error::Param));
        // Moved to another offset, connection, service or fs: the body fails.
        assert_eq!(reveal(7, a.id(), &C1, 6, &r, &k_r), Err(Error::Decrypt));
        assert_eq!(reveal(7, a.id(), &C2, 5, &r, &k_r), Err(Error::Decrypt));
        assert_eq!(reveal(7, b.id(), &C1, 5, &r, &k_r), Err(Error::Decrypt));
        assert_eq!(reveal(8, a.id(), &C1, 5, &r, &k_r), Err(Error::Decrypt));
        assert!(open_escrow(&a, &C1, 6, &r).is_err());
        assert!(open_escrow(&b, &C1, 5, &r).is_err());
        // A flag flipped by the server: no longer opens.
        let mut t = r.clone();
        t[1] = FIN;
        assert_eq!(reveal(7, a.id(), &C1, 5, &t, &k_r), Err(Error::Decrypt));
        // Fresh keys per attempt: the same bytes seal differently.
        let (r2, k2) = seal_record(&a, &C1, 5, 0, b"hello", &mut rng).unwrap();
        assert_ne!(r, r2);
        assert_ne!(*k_r, *k2);
    }

    #[test]
    fn kinds_are_not_confused() {
        let (a, _) = keys();
        let mut rng = DetRng::new("k");
        let (r, _) = seal_record(&a, &C1, 0, 0, b"x", &mut rng).unwrap();
        let escrow = parse_record(&r).unwrap().escrow.unwrap().to_vec();
        // An escrow is not a state, and a state is not an escrow.
        assert!(open_state(&a, &C1, 0, 0, &escrow).is_err());
        let s = seal_state(&a, &C1, 0, 0, &[7; 32], &mut rng).unwrap();
        let mut forged = r.clone();
        forged[34..34 + ESCROW_LEN].copy_from_slice(&s);
        assert!(parse_record(&forged).is_err());
    }

    #[test]
    fn record_shapes() {
        let (a, _) = keys();
        let mut rng = DetRng::new("f");
        // Empty records must end the stream; one end flag at most.
        assert!(seal_record(&a, &C1, 0, 0, b"", &mut rng).is_err());
        assert!(seal_record(&a, &C1, 0, FIN | CLOSE, b"x", &mut rng).is_err());
        assert!(seal_record(&a, &C1, 0, PLAIN, b"x", &mut rng).is_err());
        assert!(seal_record(&a, &C1, 0, 0x10, b"x", &mut rng).is_err());
        let (fin, _) = seal_record(&a, &C1, 0, FIN, b"", &mut rng).unwrap();
        assert_eq!(parse_record(&fin).unwrap().raw_len(), 0);
        assert!(plain_record(0, b"").is_err());
        let p = plain_record(CLOSE, b"bye").unwrap();
        let rec = parse_record(&p).unwrap();
        assert!(rec.is_plain());
        assert_eq!(
            (rec.flags, rec.raw_len(), rec.body),
            (PLAIN | CLOSE, 3, &b"bye"[..])
        );
        assert!(reveal(7, a.id(), &C1, 0, &p, &[0; 32]).is_err());
        // Truncated, wrong version, unknown flags.
        assert!(parse_record(&fin[..fin.len() - 1]).is_err());
        let mut v = fin.clone();
        v[0] = 2;
        assert!(parse_record(&v).is_err());
        let mut u = p.clone();
        u[1] |= 0x40;
        assert!(parse_record(&u).is_err());
        let mut e = plain_record(0, b"x").unwrap();
        e.truncate(2);
        assert!(parse_record(&e).is_err());
    }

    #[test]
    fn inbox_round_trip() {
        let seed = [3u8; 32];
        let pk = inbox_public(&seed);
        assert_eq!(pk.len(), INBOX_PUBLIC_LEN);
        let (ct, k) = inbox_encaps(&pk, 7, b"svc", &C1, &mut DetRng::new("i")).unwrap();
        assert_eq!(ct.len(), INBOX_CT_LEN);
        let c = k.seal_chunk(100, b"MAIL FROM").unwrap();
        let k2 = inbox_decaps(&seed, &ct, 7, b"svc", &C1).unwrap();
        assert_eq!(k2.open_chunk(100, &c).unwrap(), b"MAIL FROM");
        assert!(k2.open_chunk(101, &c).is_err());
        // Another connection derives another key.
        let k3 = inbox_decaps(&seed, &ct, 7, b"svc", &C2).unwrap();
        assert!(k3.open_chunk(100, &c).is_err());
        assert!(inbox_decaps(&seed, &ct[1..], 7, b"svc", &C1).is_err());
    }
}
