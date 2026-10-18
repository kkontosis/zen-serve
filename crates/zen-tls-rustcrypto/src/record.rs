//! TLS records at the byte level: the AEADs of the provider's suites, the
//! 1.3 and 1.2 record protections, and HKDF-Expand-Label.
//!
//! rustls's own key types can't be built from outside rustls for every key
//! length, so code that runs a record layer of its own (zen-svc's TLS
//! engine, after the handshake) uses this module. The provider's rustls
//! encrypters and decrypters call it too, so both produce the same records.
//!
//! * A [`RecordKey`] is one direction's key and IV. Its nonce for a
//!   sequence number is the IV XOR the number (RFC 8446 §5.3, RFC 7905).
//! * TLS 1.2 AES-GCM sends the nonce's last 8 bytes as the explicit nonce,
//!   and opens with the salt (the IV's first 4 bytes) and the explicit
//!   nonce received. With an IV of `salt ‖ prefix ‖ 0⁴` the explicit nonce
//!   is the 32-bit prefix followed by the low 32 bits of the sequence
//!   number (svc.md §7.1).

use aes_gcm::aead::{AeadInOut, KeyInit};
use sha2::{Sha256, Sha384};
use std::fmt;
use std::ops::Range;
use zeroize::Zeroize;

/// A record's content type: change_cipher_spec.
pub const CHANGE_CIPHER_SPEC: u8 = 20;
/// A record's content type: alert.
pub const ALERT: u8 = 21;
/// A record's content type: handshake.
pub const HANDSHAKE: u8 = 22;
/// A record's content type: application_data.
pub const APPLICATION_DATA: u8 = 23;

/// The record header's length.
pub const HEADER_LEN: usize = 5;
/// The AEAD tag's length (all three AEADs).
pub const TAG_LEN: usize = 16;
/// The 1.2 AES-GCM explicit nonce's length.
pub const EXPLICIT_NONCE_LEN: usize = 8;
/// The most plaintext a record carries (2^14).
pub const MAX_PLAIN: usize = 1 << 14;
/// The most ciphertext a 1.3 record carries (RFC 8446 §5.2).
pub const MAX_CIPHER_13: usize = MAX_PLAIN + 256;
/// The most ciphertext a 1.2 record carries (RFC 5246 §6.2.3).
pub const MAX_CIPHER_12: usize = MAX_PLAIN + 2048;

/// The AEAD of a suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aead {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
}

impl Aead {
    /// The key's length in bytes.
    pub fn key_len(self) -> usize {
        match self {
            Aead::Aes128Gcm => 16,
            Aead::Aes256Gcm | Aead::Chacha20Poly1305 => 32,
        }
    }

    /// How many records one key may protect: 2^24 for AES-GCM (RFC 8446
    /// §5.5), none for ChaCha20-Poly1305.
    pub fn confidentiality_limit(self) -> u64 {
        match self {
            Aead::Aes128Gcm | Aead::Aes256Gcm => 1 << 24,
            Aead::Chacha20Poly1305 => u64::MAX,
        }
    }
}

/// A suite's hash, for HKDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hash {
    Sha256,
    Sha384,
}

impl Hash {
    /// The output's length in bytes.
    pub fn output_len(self) -> usize {
        match self {
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
        }
    }
}

/// Why a record didn't open; each maps to a TLS alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The AEAD failed (`bad_record_mac`).
    BadRecordMac,
    /// Too long, before or after decryption (`record_overflow`).
    RecordOverflow,
    /// A 1.3 record with no content type (`unexpected_message`).
    NoContentType,
    /// A header that doesn't match its body (`decode_error`).
    Malformed,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordError::BadRecordMac => "bad record MAC",
            RecordError::RecordOverflow => "record overflow",
            RecordError::NoContentType => "no content type",
            RecordError::Malformed => "malformed record",
        })
    }
}

impl std::error::Error for RecordError {}

/// A key or IV of the wrong length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadLength;

impl fmt::Display for BadLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("wrong key or IV length")
    }
}

impl std::error::Error for BadLength {}

#[derive(Clone)]
enum Cipher {
    Aes128(Box<aes_gcm::Aes128Gcm>),
    Aes256(Box<aes_gcm::Aes256Gcm>),
    Chacha(Box<chacha20poly1305::ChaCha20Poly1305>),
}

/// One direction's record key and IV. The ciphers zeroize their keys on
/// drop, and so does the IV.
#[derive(Clone)]
pub struct RecordKey {
    aead: Aead,
    cipher: Cipher,
    iv: [u8; 12],
}

impl Drop for RecordKey {
    fn drop(&mut self) {
        self.iv.zeroize();
    }
}

impl fmt::Debug for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordKey")
            .field("aead", &self.aead)
            .finish_non_exhaustive()
    }
}

fn seal_with<A: AeadInOut>(c: &A, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; TAG_LEN] {
    let nonce = aes_gcm::aead::Nonce::<A>::try_from(&nonce[..]).expect("12-byte nonces");
    let tag = c
        .encrypt_inout_detached(&nonce, aad, buf.into())
        .expect("records are far below the AEADs' length limits");
    tag.as_slice().try_into().expect("16-byte tags")
}

fn open_with<A: AeadInOut>(
    c: &A,
    nonce: &[u8; 12],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8],
) -> Result<(), RecordError> {
    let nonce = aes_gcm::aead::Nonce::<A>::try_from(&nonce[..]).expect("12-byte nonces");
    let tag = aes_gcm::aead::Tag::<A>::try_from(tag).map_err(|_| RecordError::BadRecordMac)?;
    c.decrypt_inout_detached(&nonce, aad, buf.into(), &tag)
        .map_err(|_| RecordError::BadRecordMac)
}

impl RecordKey {
    /// A key of `aead.key_len()` bytes and a 12-byte IV.
    pub fn new(aead: Aead, key: &[u8], iv: &[u8]) -> Result<RecordKey, BadLength> {
        if key.len() != aead.key_len() {
            return Err(BadLength);
        }
        let iv: [u8; 12] = iv.try_into().map_err(|_| BadLength)?;
        let cipher = match aead {
            Aead::Aes128Gcm => Cipher::Aes128(Box::new(
                aes_gcm::Aes128Gcm::new_from_slice(key).map_err(|_| BadLength)?,
            )),
            Aead::Aes256Gcm => Cipher::Aes256(Box::new(
                aes_gcm::Aes256Gcm::new_from_slice(key).map_err(|_| BadLength)?,
            )),
            Aead::Chacha20Poly1305 => Cipher::Chacha(Box::new(
                chacha20poly1305::ChaCha20Poly1305::new_from_slice(key).map_err(|_| BadLength)?,
            )),
        };
        Ok(RecordKey { aead, cipher, iv })
    }

    /// The AEAD.
    pub fn aead(&self) -> Aead {
        self.aead
    }

    /// Replaces the IV (a 1.2 AES-GCM sender's new explicit-nonce prefix).
    pub fn set_iv(&mut self, iv: [u8; 12]) {
        self.iv = iv;
    }

    /// The nonce for `seq`: the IV XOR the sequence number.
    pub fn nonce(&self, seq: u64) -> [u8; 12] {
        let mut n = self.iv;
        for (a, b) in n[4..].iter_mut().zip(seq.to_be_bytes()) {
            *a ^= b;
        }
        n
    }

    /// Encrypts `buf` in place; returns the tag.
    pub fn seal_detached(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; TAG_LEN] {
        match &self.cipher {
            Cipher::Aes128(c) => seal_with(&**c, nonce, aad, buf),
            Cipher::Aes256(c) => seal_with(&**c, nonce, aad, buf),
            Cipher::Chacha(c) => seal_with(&**c, nonce, aad, buf),
        }
    }

    /// Decrypts `buf` in place, checking `tag`.
    pub fn open_detached(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8],
    ) -> Result<(), RecordError> {
        match &self.cipher {
            Cipher::Aes128(c) => open_with(&**c, nonce, aad, buf, tag),
            Cipher::Aes256(c) => open_with(&**c, nonce, aad, buf, tag),
            Cipher::Chacha(c) => open_with(&**c, nonce, aad, buf, tag),
        }
    }

    /// A TLS 1.3 record (header included) carrying `plain` of content type
    /// `typ`, without padding. `plain` is at most [`MAX_PLAIN`] bytes.
    pub fn seal13(&self, seq: u64, typ: u8, plain: &[u8]) -> Vec<u8> {
        debug_assert!(plain.len() <= MAX_PLAIN);
        let len = plain.len() + 1 + TAG_LEN;
        let mut out = Vec::with_capacity(HEADER_LEN + len);
        out.extend_from_slice(&header(APPLICATION_DATA, len));
        out.extend_from_slice(plain);
        out.push(typ);
        let aad: [u8; 5] = out[..HEADER_LEN].try_into().expect("5 bytes");
        let tag = self.seal_detached(&self.nonce(seq), &aad, &mut out[HEADER_LEN..]);
        out.extend_from_slice(&tag);
        out
    }

    /// Opens a TLS 1.3 record (header included): its inner content type and
    /// plaintext, with the padding removed. The caller has checked the outer
    /// type (application_data).
    pub fn open13(&self, seq: u64, record: &[u8]) -> Result<(u8, Vec<u8>), RecordError> {
        let (aad, body) = split_record(record)?;
        let mut body = body.to_vec();
        let (typ, len) = self.open13_in_place(seq, aad, &mut body)?;
        body.truncate(len);
        Ok((typ, body))
    }

    /// [`RecordKey::open13`] in place: `body` is the record after its
    /// header `aad`. Returns the inner content type and the content's
    /// length, which starts `body`.
    pub fn open13_in_place(
        &self,
        seq: u64,
        aad: &[u8],
        body: &mut [u8],
    ) -> Result<(u8, usize), RecordError> {
        if body.len() > MAX_CIPHER_13 {
            return Err(RecordError::RecordOverflow);
        }
        let n = body
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(RecordError::BadRecordMac)?;
        let (plain, tag) = body.split_at_mut(n);
        self.open_detached(&self.nonce(seq), aad, plain, tag)?;
        // TLSInnerPlaintext is at most 2^14 + 1 bytes (RFC 8446 §5.4).
        if plain.len() > MAX_PLAIN + 1 {
            return Err(RecordError::RecordOverflow);
        }
        // The inner content type is the last non-zero byte.
        let at = plain
            .iter()
            .rposition(|b| *b != 0)
            .ok_or(RecordError::NoContentType)?;
        if at > MAX_PLAIN {
            return Err(RecordError::RecordOverflow);
        }
        Ok((plain[at], at))
    }

    /// A TLS 1.2 record (header included, version 0x0303) carrying `plain`
    /// of content type `typ`. AES-GCM puts the nonce's last 8 bytes in front
    /// as the explicit nonce; ChaCha20-Poly1305 has none (RFC 7905).
    pub fn seal12(&self, seq: u64, typ: u8, plain: &[u8]) -> Vec<u8> {
        debug_assert!(plain.len() <= MAX_PLAIN);
        let nonce = self.nonce(seq);
        let explicit = self.explicit_len();
        let len = explicit + plain.len() + TAG_LEN;
        let mut out = Vec::with_capacity(HEADER_LEN + len);
        out.extend_from_slice(&header(typ, len));
        out.extend_from_slice(&nonce[12 - explicit..]);
        out.extend_from_slice(plain);
        let aad = aad12(seq, typ, plain.len());
        let tag = self.seal_detached(&nonce, &aad, &mut out[HEADER_LEN + explicit..]);
        out.extend_from_slice(&tag);
        out
    }

    /// Opens a TLS 1.2 record (header included): its plaintext. The content
    /// type and version in the AAD are the header's.
    pub fn open12(&self, seq: u64, record: &[u8]) -> Result<Vec<u8>, RecordError> {
        let (head, body) = split_record(record)?;
        let mut body = body.to_vec();
        let r = self.open12_in_place(seq, head[0], [head[1], head[2]], &mut body)?;
        Ok(body[r].to_vec())
    }

    /// [`RecordKey::open12`] in place: `body` is the record after its
    /// header, whose type and version are given. Returns where the
    /// plaintext is in `body`.
    pub fn open12_in_place(
        &self,
        seq: u64,
        typ: u8,
        version: [u8; 2],
        body: &mut [u8],
    ) -> Result<Range<usize>, RecordError> {
        if body.len() > MAX_CIPHER_12 {
            return Err(RecordError::RecordOverflow);
        }
        let explicit = self.explicit_len();
        let n = body
            .len()
            .checked_sub(explicit + TAG_LEN)
            .ok_or(RecordError::BadRecordMac)?;
        let nonce = if explicit == 0 {
            self.nonce(seq)
        } else {
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&self.iv[..4]);
            nonce[4..].copy_from_slice(&body[..EXPLICIT_NONCE_LEN]);
            nonce
        };
        let mut aad = aad12(seq, typ, n);
        aad[9..11].copy_from_slice(&version);
        let (plain, tag) = body[explicit..].split_at_mut(n);
        self.open_detached(&nonce, &aad, plain, tag)?;
        if n > MAX_PLAIN {
            return Err(RecordError::RecordOverflow);
        }
        Ok(explicit..explicit + n)
    }

    fn explicit_len(&self) -> usize {
        match self.aead {
            Aead::Aes128Gcm | Aead::Aes256Gcm => EXPLICIT_NONCE_LEN,
            Aead::Chacha20Poly1305 => 0,
        }
    }
}

/// A record header with version 0x0303.
pub fn header(typ: u8, len: usize) -> [u8; HEADER_LEN] {
    let len = u16::try_from(len).expect("records are below 64 KiB");
    let [a, b] = len.to_be_bytes();
    [typ, 3, 3, a, b]
}

fn split_record(record: &[u8]) -> Result<(&[u8], &[u8]), RecordError> {
    if record.len() < HEADER_LEN {
        return Err(RecordError::Malformed);
    }
    let (head, body) = record.split_at(HEADER_LEN);
    if usize::from(u16::from_be_bytes([head[3], head[4]])) != body.len() {
        return Err(RecordError::Malformed);
    }
    Ok((head, body))
}

/// The 1.2 AEAD's additional data: `seq ‖ type ‖ version ‖ length`.
fn aad12(seq: u64, typ: u8, len: usize) -> [u8; 13] {
    let mut aad = [0u8; 13];
    aad[..8].copy_from_slice(&seq.to_be_bytes());
    aad[8] = typ;
    aad[9..11].copy_from_slice(&[3, 3]);
    aad[11..].copy_from_slice(&(len as u16).to_be_bytes());
    aad
}

/// HKDF-Expand-Label (RFC 8446 §7.1): `label` without the `"tls13 "`
/// prefix. `len` is at most 255 hash lengths.
pub fn expand_label(hash: Hash, secret: &[u8], label: &[u8], ctx: &[u8], len: usize) -> Vec<u8> {
    let mut info = Vec::with_capacity(4 + 6 + label.len() + ctx.len());
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push((6 + label.len()) as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label);
    info.push(ctx.len() as u8);
    info.extend_from_slice(ctx);
    match hash {
        Hash::Sha256 => expand::<hmac::Hmac<Sha256>>(secret, &info, len),
        Hash::Sha384 => expand::<hmac::Hmac<Sha384>>(secret, &info, len),
    }
}

/// HKDF-Expand (RFC 5869 §2.3).
fn expand<M: hmac::Mac + hmac::KeyInit + Clone>(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mac = <M as hmac::KeyInit>::new_from_slice(prk).expect("HMAC takes any key");
    let mut out = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        let mut m = mac.clone();
        m.update(&t);
        m.update(info);
        m.update(&[i]);
        t = m.finalize().into_bytes().to_vec();
        out.extend_from_slice(&t[..t.len().min(len - out.len())]);
        i = i.checked_add(1).expect("at most 255 blocks");
    }
    t.zeroize();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 8448 §3: the traffic keys and IVs of the simple 1-RTT handshake.
    #[test]
    fn expand_label_known_answers() {
        let cases = [
            // server handshake traffic secret
            (
                "b67b7d690cc16c4e75e54213cb2d37b4e9c912bcded9105d42befd59d391ad38",
                "3fce516009c21727d0f2e4e86ee403bc",
                "5d313eb2671276ee13000b30",
            ),
            // server application traffic secret 0
            (
                "a11af9f05531f856ad47116b45a950328204b4f44bfb6b3a4b4f1f3fcb631643",
                "9f02283b6c9c07efc26bb9f2ac92e356",
                "cf782b88dd83549aadf1e984",
            ),
        ];
        for (secret, key, iv) in cases {
            let s = unhex(secret);
            assert_eq!(expand_label(Hash::Sha256, &s, b"key", b"", 16), unhex(key));
            assert_eq!(expand_label(Hash::Sha256, &s, b"iv", b"", 12), unhex(iv));
        }
        // "traffic upd", and SHA-384 (two HMAC blocks), from an independent
        // implementation (Python's hmac).
        let s = unhex("a11af9f05531f856ad47116b45a950328204b4f44bfb6b3a4b4f1f3fcb631643");
        assert_eq!(
            expand_label(Hash::Sha256, &s, b"traffic upd", b"", 32),
            unhex("51921b8aa3001976eb401d0a4319a8516416a6c56001a357e5d162031e84f916")
        );
        let s: Vec<u8> = (0..48).collect();
        assert_eq!(
            expand_label(Hash::Sha384, &s, b"traffic upd", b"", 48),
            unhex(
                "401331b63e9d59f202e8f041042d9516f4cd7fa2e2ee14631d3b49fc340d7af3\
                 7fc2c0c9f252d8036f81ec5b85cbe5db"
            )
        );
        assert_eq!(
            expand_label(Hash::Sha384, &s, b"key", b"", 32),
            unhex("6877d022f1c61d24ebb7487c16752d9a4798e40431c75b39320e537c90e23225")
        );
    }

    /// Records sealed here equal those of an independent implementation
    /// (Python's `cryptography`), and open again.
    #[test]
    fn records_known_answers() {
        let k = RecordKey::new(
            Aead::Aes128Gcm,
            &unhex("9f02283b6c9c07efc26bb9f2ac92e356"),
            &unhex("cf782b88dd83549aadf1e984"),
        )
        .unwrap();
        let r = k.seal13(0, APPLICATION_DATA, b"hello world");
        assert_eq!(
            r,
            unhex("170303001c560ee3352e6ae0a71d26fa5509411dd2d9de236015bfe39c2f138954")
        );
        assert_eq!(
            k.open13(0, &r).unwrap(),
            (APPLICATION_DATA, b"hello world".to_vec())
        );

        let key: Vec<u8> = (0..32).collect();
        let iv: Vec<u8> = (100..112).collect();
        let k = RecordKey::new(Aead::Chacha20Poly1305, &key, &iv).unwrap();
        let r = k.seal13(5, ALERT, &[1, 0]);
        assert_eq!(r, unhex("17030300132d3bba54f876e9489ec15d6b2fa22b564b9f25"));
        assert_eq!(k.open13(5, &r).unwrap(), (ALERT, vec![1, 0]));

        let r = k.seal12(9, APPLICATION_DATA, b"abc");
        assert_eq!(r, unhex("17030300130875429acd2d6a0ac3245e2e9a526d1509ebb4"));
        assert_eq!(k.open12(9, &r).unwrap(), b"abc");

        // 1.2 AES-GCM: salt 0a0b0c0d, prefix 11223344; the explicit nonce is
        // the prefix and the low 32 bits of the sequence number.
        let mut iv = unhex("0a0b0c0d11223344");
        iv.extend_from_slice(&[0; 4]);
        let k = RecordKey::new(Aead::Aes256Gcm, &key, &iv).unwrap();
        let r = k.seal12(7, APPLICATION_DATA, b"abc");
        assert_eq!(
            r,
            unhex("170303001b11223344000000070e0bb45541c5e874b5e88892407af4e268b78e")
        );
        assert_eq!(&r[5..13], &unhex("1122334400000007")[..]);
        // A receiver knows only the salt.
        let mut salt = unhex("0a0b0c0d");
        salt.extend_from_slice(&[0; 8]);
        let rx = RecordKey::new(Aead::Aes256Gcm, &key, &salt).unwrap();
        assert_eq!(rx.open12(7, &r).unwrap(), b"abc");
        assert_eq!(rx.open12(8, &r), Err(RecordError::BadRecordMac));
    }

    #[test]
    fn records_refuse_tampering_and_bad_framing() {
        for aead in [Aead::Aes128Gcm, Aead::Aes256Gcm, Aead::Chacha20Poly1305] {
            let k = RecordKey::new(aead, &vec![7; aead.key_len()], &[9; 12]).unwrap();
            let r = k.seal13(3, HANDSHAKE, b"x");
            for i in 0..r.len() {
                let mut t = r.clone();
                t[i] ^= 0x80;
                assert!(k.open13(3, &t).is_err(), "{aead:?} byte {i}");
            }
            assert_eq!(k.open13(4, &r), Err(RecordError::BadRecordMac));
            assert_eq!(k.open13(3, &r[..r.len() - 1]), Err(RecordError::Malformed));

            // Padding is removed; a record of only zeros has no type.
            let pad = |plain: &[u8]| {
                let len = plain.len() + TAG_LEN;
                let mut out = header(APPLICATION_DATA, len).to_vec();
                out.extend_from_slice(plain);
                let aad: [u8; 5] = out[..5].try_into().unwrap();
                let tag = k.seal_detached(&k.nonce(1), &aad, &mut out[5..]);
                out.extend_from_slice(&tag);
                out
            };
            assert_eq!(
                k.open13(1, &pad(&[b'a', APPLICATION_DATA, 0, 0, 0]))
                    .unwrap(),
                (APPLICATION_DATA, b"a".to_vec())
            );
            assert_eq!(k.open13(1, &pad(&[0; 9])), Err(RecordError::NoContentType));
            // 2^14 + 1 bytes of content is too much, even if it decrypts.
            let mut big = vec![1u8; MAX_PLAIN + 1];
            big.push(APPLICATION_DATA);
            assert_eq!(k.open13(1, &pad(&big)), Err(RecordError::RecordOverflow));

            let r = k.seal12(3, APPLICATION_DATA, b"xyz");
            for i in 0..r.len() {
                let mut t = r.clone();
                t[i] ^= 0x80;
                assert!(k.open12(3, &t).is_err(), "{aead:?} byte {i}");
            }
            assert!(k.open12(3, &[23, 3, 3, 0, 4, 0, 0, 0, 0]).is_err());
        }
        assert!(RecordKey::new(Aead::Aes128Gcm, &[0; 32], &[0; 12]).is_err());
        assert!(RecordKey::new(Aead::Aes256Gcm, &[0; 32], &[0; 8]).is_err());
    }
}
