//! The binary packet protocol (RFC 4253 §6) and its two protections,
//! `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`
//! (svc.md §7.2).

use aes_gcm::aead::{AeadInOut, KeyInit};
use chacha20::ChaCha20Legacy;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

/// Longest `packet_length` accepted: RFC 4253 §6.1's minimum.
pub(crate) const MAX_PACKET: usize = 35_000;
const TAG: usize = 16;

/// A packet cipher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cipher {
    /// `chacha20-poly1305@openssh.com`.
    Chacha20Poly1305,
    /// `aes256-gcm@openssh.com`.
    Aes256Gcm,
}

impl Cipher {
    /// The SSH name.
    pub fn name(self) -> &'static str {
        match self {
            Cipher::Chacha20Poly1305 => "chacha20-poly1305@openssh.com",
            Cipher::Aes256Gcm => "aes256-gcm@openssh.com",
        }
    }

    pub(crate) fn from_name(n: &str) -> Option<Cipher> {
        [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm]
            .into_iter()
            .find(|c| c.name() == n)
    }

    pub(crate) fn key_len(self) -> usize {
        match self {
            Cipher::Chacha20Poly1305 => 64,
            Cipher::Aes256Gcm => 32,
        }
    }

    pub(crate) fn iv_len(self) -> usize {
        match self {
            Cipher::Chacha20Poly1305 => 0,
            Cipher::Aes256Gcm => 12,
        }
    }

    fn block(self) -> usize {
        match self {
            Cipher::Chacha20Poly1305 => 8,
            Cipher::Aes256Gcm => 16,
        }
    }

    /// The state's code (svc.md §7.2 `keys.c`).
    pub(crate) fn code(self) -> u64 {
        match self {
            Cipher::Chacha20Poly1305 => 1,
            Cipher::Aes256Gcm => 2,
        }
    }

    pub(crate) fn from_code(c: u64) -> Option<Cipher> {
        match c {
            1 => Some(Cipher::Chacha20Poly1305),
            2 => Some(Cipher::Aes256Gcm),
            _ => None,
        }
    }
}

/// One direction's key and IV.
#[derive(Clone)]
pub(crate) struct Keys {
    pub(crate) cipher: Cipher,
    pub(crate) key: Zeroizing<Vec<u8>>,
    /// aes256-gcm: the fixed 4 bytes and the 8-byte packet counter.
    pub(crate) iv: Zeroizing<Vec<u8>>,
    aes: Option<Box<aes_gcm::Aes256Gcm>>,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keys")
            .field("cipher", &self.cipher)
            .finish_non_exhaustive()
    }
}

impl Keys {
    /// `None` when a length is wrong.
    pub(crate) fn new(cipher: Cipher, key: &[u8], iv: &[u8]) -> Option<Keys> {
        if key.len() != cipher.key_len() || iv.len() != cipher.iv_len() {
            return None;
        }
        let aes = match cipher {
            Cipher::Aes256Gcm => Some(Box::new(aes_gcm::Aes256Gcm::new_from_slice(key).ok()?)),
            Cipher::Chacha20Poly1305 => None,
        };
        Some(Keys {
            cipher,
            key: Zeroizing::new(key.to_vec()),
            iv: Zeroizing::new(iv.to_vec()),
            aes,
        })
    }

    fn chacha(&self, main: bool, seq: u32) -> ChaCha20Legacy {
        let k = if main {
            &self.key[..32]
        } else {
            &self.key[32..]
        };
        let nonce = u64::from(seq).to_be_bytes();
        ChaCha20Legacy::new(k.try_into().expect("32 bytes"), &nonce.into())
    }

    fn gcm_nonce(&self) -> aes_gcm::Nonce<aes_gcm::aes::cipher::consts::U12> {
        aes_gcm::Nonce::try_from(&self.iv[..]).expect("12 bytes")
    }

    /// Bumps the GCM packet counter (the IV's last 8 bytes).
    fn gcm_next(&mut self) {
        let mut c = u64::from_be_bytes(self.iv[4..].try_into().expect("8 bytes"));
        c = c.wrapping_add(1);
        self.iv[4..].copy_from_slice(&c.to_be_bytes());
    }

    /// The packet length from the first 4 bytes.
    fn length(&self, seq: u32, head: &[u8; 4]) -> u32 {
        let mut h = *head;
        if self.cipher == Cipher::Chacha20Poly1305 {
            self.chacha(false, seq).apply_keystream(&mut h);
        }
        u32::from_be_bytes(h)
    }

    /// Encrypts `pkt` (length ‖ the rest) in place and appends the tag.
    fn seal(&mut self, seq: u32, pkt: &mut Vec<u8>) {
        match self.cipher {
            Cipher::Chacha20Poly1305 => {
                let mut main = self.chacha(true, seq);
                let mut pk = Zeroizing::new([0u8; 64]);
                main.apply_keystream(&mut pk[..]);
                self.chacha(false, seq).apply_keystream(&mut pkt[..4]);
                main.seek(64u64);
                main.apply_keystream(&mut pkt[4..]);
                let tag = poly(&pk[..32], pkt);
                pkt.extend_from_slice(&tag);
            }
            Cipher::Aes256Gcm => {
                let nonce = self.gcm_nonce();
                let (aad, body) = pkt.split_at_mut(4);
                let tag = self
                    .aes
                    .as_ref()
                    .expect("an AES key")
                    .encrypt_inout_detached(&nonce, aad, body.into())
                    .expect("packets are far below GCM's limit");
                pkt.extend_from_slice(&tag);
                self.gcm_next();
            }
        }
    }

    /// Checks the tag of `pkt` (length ‖ body ‖ tag) and decrypts the body
    /// in place; `false` when it doesn't authenticate.
    fn open(&mut self, seq: u32, pkt: &mut [u8]) -> bool {
        let (data, tag) = pkt.split_at_mut(pkt.len() - TAG);
        match self.cipher {
            Cipher::Chacha20Poly1305 => {
                let mut main = self.chacha(true, seq);
                let mut pk = Zeroizing::new([0u8; 64]);
                main.apply_keystream(&mut pk[..]);
                if !ct_eq(&poly(&pk[..32], data), tag) {
                    return false;
                }
                main.seek(64u64);
                main.apply_keystream(&mut data[4..]);
                true
            }
            Cipher::Aes256Gcm => {
                let nonce = self.gcm_nonce();
                let (aad, body) = data.split_at_mut(4);
                let Ok(tag) = aes_gcm::Tag::try_from(&tag[..]) else {
                    return false;
                };
                let ok = self
                    .aes
                    .as_ref()
                    .expect("an AES key")
                    .decrypt_inout_detached(&nonce, aad, body.into(), &tag)
                    .is_ok();
                if ok {
                    self.gcm_next();
                }
                ok
            }
        }
    }
}

fn poly(key: &[u8], data: &[u8]) -> [u8; 16] {
    use poly1305::universal_hash::KeyInit as _;
    let p = poly1305::Poly1305::new_from_slice(key).expect("32-byte key");
    p.compute_unpadded(data).into()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Why a packet was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PacketError {
    /// Too long, misaligned, or with bad padding.
    Malformed,
    /// It doesn't authenticate.
    Mac,
}

/// One direction: its keys (none before the first NEWKEYS), the sequence
/// number, and what passed since the keys were set.
#[derive(Clone, Debug, Default)]
pub(crate) struct Dir {
    pub(crate) keys: Option<Keys>,
    pub(crate) seq: u32,
    pub(crate) bytes: u64,
    pub(crate) packets: u64,
}

impl Dir {
    fn shape(&self) -> (usize, usize, usize) {
        match &self.keys {
            None => (8, 0, 0),
            Some(k) => (k.cipher.block(), 4, TAG),
        }
    }

    /// New keys (`None`: none), as NEWKEYS does; `reset` restarts the
    /// sequence number (strict kex).
    pub(crate) fn set_keys(&mut self, keys: Option<Keys>, reset: bool) {
        self.keys = keys;
        if reset {
            self.seq = 0;
        }
        self.bytes = 0;
        self.packets = 0;
    }

    /// Appends `payload` as a packet to `out`.
    pub(crate) fn seal(&mut self, payload: &[u8], out: &mut Vec<u8>) {
        let (block, aad, _) = self.shape();
        let len = 5 + payload.len();
        let mut pad = block - (len - aad) % block;
        if pad < 4 {
            pad += block;
        }
        let mut pkt = Vec::with_capacity(len + pad + TAG);
        pkt.extend_from_slice(&((1 + payload.len() + pad) as u32).to_be_bytes());
        pkt.push(pad as u8);
        pkt.extend_from_slice(payload);
        let at = pkt.len();
        pkt.resize(at + pad, 0);
        getrandom::fill(&mut pkt[at..]).expect("random bytes");
        let seq = self.seq;
        if let Some(k) = &mut self.keys {
            k.seal(seq, &mut pkt);
        }
        self.seq = self.seq.wrapping_add(1);
        self.bytes += pkt.len() as u64;
        self.packets += 1;
        out.extend_from_slice(&pkt);
    }

    /// The whole length the next packet in `buf` will take (with its tag),
    /// once 4 bytes are there; an error if it can't be a packet.
    pub(crate) fn next_len(&self, buf: &[u8]) -> Result<Option<usize>, PacketError> {
        let Some(head) = buf.get(..4) else {
            return Ok(None);
        };
        let head: &[u8; 4] = head.try_into().expect("4 bytes");
        let len = match &self.keys {
            None => u32::from_be_bytes(*head),
            Some(k) => k.length(self.seq, head),
        } as usize;
        let (block, aad, tag) = self.shape();
        if !(5..=MAX_PACKET).contains(&len) || !(len + 4 - aad).is_multiple_of(block) {
            return Err(PacketError::Malformed);
        }
        Ok(Some(4 + len + tag))
    }

    /// The payload of the next packet in `buf` and the bytes it took, or
    /// `None` while it is incomplete.
    pub(crate) fn open(&mut self, buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, PacketError> {
        let Some(total) = self.next_len(buf)? else {
            return Ok(None);
        };
        if buf.len() < total {
            return Ok(None);
        }
        let mut pkt = buf[..total].to_vec();
        let seq = self.seq;
        if let Some(k) = &mut self.keys
            && !k.open(seq, &mut pkt)
        {
            pkt.zeroize();
            return Err(PacketError::Mac);
        }
        let (_, _, tag) = self.shape();
        let body = &pkt[4..total - tag];
        let pad = body[0] as usize;
        if pad < 4 || pad + 1 >= body.len() {
            pkt.zeroize();
            return Err(PacketError::Malformed);
        }
        let payload = body[1..body.len() - pad].to_vec();
        pkt.zeroize();
        self.seq = self.seq.wrapping_add(1);
        self.bytes += total as u64;
        self.packets += 1;
        Ok(Some((payload, total)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(c: Cipher) -> (Dir, Dir) {
        let key: Vec<u8> = (0..c.key_len() as u8).collect();
        let iv: Vec<u8> = (100..100 + c.iv_len() as u8).collect();
        let d = Dir {
            keys: Keys::new(c, &key, &iv),
            ..Dir::default()
        };
        (d.clone(), d)
    }

    #[test]
    fn packets_round_trip() {
        for c in [
            None,
            Some(Cipher::Chacha20Poly1305),
            Some(Cipher::Aes256Gcm),
        ] {
            let (mut tx, mut rx) = match c {
                Some(c) => pair(c),
                None => (Dir::default(), Dir::default()),
            };
            let mut wire = Vec::new();
            for n in [1usize, 2, 7, 8, 15, 16, 17, 1000, 32_768] {
                let p: Vec<u8> = (0..n).map(|i| i as u8).collect();
                tx.seal(&p, &mut wire);
                let (got, used) = rx.open(&wire).unwrap().unwrap();
                assert_eq!(got, p);
                assert_eq!(used, wire.len());
                wire.clear();
            }
            assert_eq!(tx.seq, rx.seq);
        }
    }

    #[test]
    fn tampering_and_truncation() {
        for c in [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm] {
            let (mut tx, rx) = pair(c);
            let mut wire = Vec::new();
            tx.seal(b"hello", &mut wire);
            for cut in 0..wire.len() {
                assert_eq!(rx.clone().open(&wire[..cut]), Ok(None));
            }
            for i in 4..wire.len() {
                let mut w = wire.clone();
                w[i] ^= 1;
                assert_eq!(rx.clone().open(&w), Err(PacketError::Mac), "{c:?} byte {i}");
            }
            // Nor under another nonce: the sequence number, or GCM's counter.
            let mut r = rx.clone();
            match c {
                Cipher::Chacha20Poly1305 => r.seq = 1,
                Cipher::Aes256Gcm => r.keys.as_mut().unwrap().gcm_next(),
            }
            assert!(r.open(&wire).is_err());
        }
    }

    /// chacha20-poly1305@openssh.com against a packet made by OpenSSH's
    /// construction written out by hand: the length under the header key,
    /// the Poly1305 key from block 0 of the main key, the payload from
    /// block 1.
    #[test]
    fn chacha_layout() {
        let key: Vec<u8> = (0..64u8).collect();
        let k = Keys::new(Cipher::Chacha20Poly1305, &key, &[]).unwrap();
        let mut d = Dir {
            keys: Some(k),
            seq: 7,
            ..Dir::default()
        };
        let mut wire = Vec::new();
        d.seal(b"abc", &mut wire);
        let nonce = 7u64.to_be_bytes();
        let mut len = wire[..4].to_vec();
        ChaCha20Legacy::new(key[32..].try_into().unwrap(), &nonce.into()).apply_keystream(&mut len);
        assert_eq!(
            u32::from_be_bytes(len.try_into().unwrap()) as usize,
            wire.len() - 4 - 16
        );
        let mut ks = vec![0u8; 64 + wire.len()];
        ChaCha20Legacy::new(key[..32].try_into().unwrap(), &nonce.into()).apply_keystream(&mut ks);
        let n = wire.len() - 16;
        assert_eq!(poly(&ks[..32], &wire[..n]), wire[n..]);
        let body: Vec<u8> = wire[4..n]
            .iter()
            .zip(&ks[64..])
            .map(|(a, b)| a ^ b)
            .collect();
        assert_eq!(&body[1..4], b"abc");
    }

    #[test]
    fn gcm_counter_advances() {
        let (mut tx, mut rx) = pair(Cipher::Aes256Gcm);
        let mut wire = Vec::new();
        tx.seal(b"one", &mut wire);
        tx.seal(b"two", &mut wire);
        let iv = tx.keys.as_ref().unwrap().iv.clone();
        let start = u64::from_be_bytes([104, 105, 106, 107, 108, 109, 110, 111]);
        assert_eq!(&iv[..4], &[100, 101, 102, 103]);
        assert_eq!(&iv[4..], &(start + 2).to_be_bytes()[..]);
        let (a, n) = rx.open(&wire).unwrap().unwrap();
        let (b, _) = rx.open(&wire[n..]).unwrap().unwrap();
        assert_eq!((a.as_slice(), b.as_slice()), (&b"one"[..], &b"two"[..]));
    }

    #[test]
    fn bad_lengths_are_refused() {
        let d = Dir::default();
        for len in [0u32, 4, 35_004, 13] {
            let mut b = len.to_be_bytes().to_vec();
            b.resize(64, 0);
            assert_eq!(d.next_len(&b), Err(PacketError::Malformed), "{len}");
        }
        // Padding under 4 bytes.
        let mut b = 12u32.to_be_bytes().to_vec();
        b.push(3);
        b.resize(16, 0);
        assert_eq!(d.clone().open(&b), Err(PacketError::Malformed));
    }
}
