//! Key exchange (RFC 4253 §7–§8): KEXINIT and its negotiation, the two
//! methods, the exchange hash and the keys.

use super::cipher::{Cipher, Keys};
use super::wire::{self, Malformed, Reader, Writer};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// A key-exchange method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kex {
    /// `mlkem768x25519-sha256`: ML-KEM-768 and X25519.
    MlKem768X25519,
    /// `curve25519-sha256` (or `curve25519-sha256@libssh.org`).
    Curve25519,
}

impl Kex {
    /// The SSH names, preferred first.
    pub(crate) fn names(self) -> &'static [&'static str] {
        match self {
            Kex::MlKem768X25519 => &["mlkem768x25519-sha256"],
            Kex::Curve25519 => &["curve25519-sha256", "curve25519-sha256@libssh.org"],
        }
    }

    /// The SSH name.
    pub fn name(self) -> &'static str {
        self.names()[0]
    }

    fn from_name(n: &str) -> Option<Kex> {
        [Kex::MlKem768X25519, Kex::Curve25519]
            .into_iter()
            .find(|k| k.names().contains(&n))
    }

    pub(crate) fn code(self) -> u64 {
        match self {
            Kex::MlKem768X25519 => 1,
            Kex::Curve25519 => 2,
        }
    }

    pub(crate) fn from_code(c: u64) -> Option<Kex> {
        match c {
            1 => Some(Kex::MlKem768X25519),
            2 => Some(Kex::Curve25519),
            _ => None,
        }
    }
}

pub(crate) const STRICT_S: &str = "kex-strict-s-v00@openssh.com";
pub(crate) const STRICT_C: &str = "kex-strict-c-v00@openssh.com";
pub(crate) const EXT_INFO_C: &str = "ext-info-c";
pub(crate) const HOST_KEY: &str = "ssh-ed25519";
/// Offered so negotiation succeeds; AEAD ciphers use no MAC.
const MACS: [&str; 2] = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-256"];

pub(crate) const MSG_KEXINIT: u8 = 20;
pub(crate) const MSG_NEWKEYS: u8 = 21;
pub(crate) const MSG_KEX_INIT: u8 = 30;
pub(crate) const MSG_KEX_REPLY: u8 = 31;

/// Longest client KEXINIT accepted.
pub(crate) const MAX_KEXINIT: usize = 8192;

const MLKEM768_EK: usize = 1184;
#[cfg(feature = "test-utils")]
const MLKEM768_CT: usize = 1088;

/// A KEXINIT payload. `server`: the server's lists (with strict kex on the
/// first one); otherwise a client's (with `ext-info-c` and strict kex).
pub(crate) fn kexinit(kex: &[Kex], ciphers: &[Cipher], server: bool, first: bool) -> Vec<u8> {
    let mut cookie = [0u8; 16];
    getrandom::fill(&mut cookie).expect("random bytes");
    let mut kn: Vec<&str> = kex.iter().flat_map(|k| k.names().iter().copied()).collect();
    if first {
        if server {
            kn.push(STRICT_S);
        } else {
            kn.extend([EXT_INFO_C, STRICT_C]);
        }
    }
    let cn: Vec<&str> = ciphers.iter().map(|c| c.name()).collect();
    let mut w = Writer::new(MSG_KEXINIT);
    w.raw(&cookie)
        .names(&kn)
        .names(&[HOST_KEY])
        .names(&cn)
        .names(&cn)
        .names(&MACS)
        .names(&MACS)
        .names(&["none"])
        .names(&["none"])
        .names(&[])
        .names(&[])
        .bool(false)
        .u32(0);
    w.finish()
}

/// A parsed KEXINIT.
pub(crate) struct Proposal<'a> {
    pub(crate) kex: Vec<&'a str>,
    host_key: Vec<&'a str>,
    enc_c2s: Vec<&'a str>,
    enc_s2c: Vec<&'a str>,
    comp_c2s: Vec<&'a str>,
    comp_s2c: Vec<&'a str>,
    follows: bool,
}

pub(crate) fn parse_kexinit(p: &[u8]) -> wire::Result<Proposal<'_>> {
    let mut r = Reader::new(p);
    if r.u8()? != MSG_KEXINIT {
        return Err(Malformed);
    }
    r.raw(16)?;
    let kex = r.names()?;
    let host_key = r.names()?;
    let enc_c2s = r.names()?;
    let enc_s2c = r.names()?;
    r.names()?;
    r.names()?;
    let comp_c2s = r.names()?;
    let comp_s2c = r.names()?;
    r.names()?;
    r.names()?;
    let follows = r.bool()?;
    r.u32()?;
    r.done()?;
    Ok(Proposal {
        kex,
        host_key,
        enc_c2s,
        enc_s2c,
        comp_c2s,
        comp_s2c,
        follows,
    })
}

/// What a negotiation chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chosen {
    pub(crate) kex: Kex,
    pub(crate) c2s: Cipher,
    pub(crate) s2c: Cipher,
    /// The client guessed and was wrong: its next packet is ignored.
    pub(crate) wrong_guess: bool,
}

/// RFC 4253 §7.1: in each list, the client's first choice the server has.
pub(crate) fn negotiate(client: &Proposal, kex: &[Kex], ciphers: &[Cipher]) -> Option<Chosen> {
    let ki = client
        .kex
        .iter()
        .position(|n| Kex::from_name(n).is_some_and(|k| kex.contains(&k)))?;
    let k = Kex::from_name(client.kex[ki]).expect("found");
    let hi = client.host_key.iter().position(|n| *n == HOST_KEY)?;
    let pick = |l: &[&str]| {
        l.iter()
            .find_map(|n| Cipher::from_name(n).filter(|c| ciphers.contains(c)))
    };
    let c2s = pick(&client.enc_c2s)?;
    let s2c = pick(&client.enc_s2c)?;
    if !client.comp_c2s.contains(&"none") || !client.comp_s2c.contains(&"none") {
        return None;
    }
    Some(Chosen {
        kex: k,
        c2s,
        s2c,
        wrong_guess: client.follows && (ki != 0 || hi != 0),
    })
}

/// Why a key exchange failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KexFailed;

fn x25519_pair() -> (x25519_dalek::StaticSecret, [u8; 32]) {
    let mut b = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut b[..]).expect("random bytes");
    let s = x25519_dalek::StaticSecret::from(*b);
    let p = x25519_dalek::PublicKey::from(&s).to_bytes();
    (s, p)
}

fn x25519_agree(
    s: &x25519_dalek::StaticSecret,
    peer: &[u8],
) -> Result<Zeroizing<[u8; 32]>, KexFailed> {
    let peer: [u8; 32] = peer.try_into().map_err(|_| KexFailed)?;
    let ss = s.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
    // RFC 8731 §3: an all-zero result is refused.
    if !ss.was_contributory() {
        return Err(KexFailed);
    }
    Ok(Zeroizing::new(ss.to_bytes()))
}

/// The hybrid's K: SHA-256(ML-KEM key ‖ X25519 secret), as a string.
fn hybrid_k(ss_m: &[u8], ss_x: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut h = Sha256::new();
    h.update(ss_m);
    h.update(ss_x);
    let mut d: [u8; 32] = h.finalize().into();
    let k = Zeroizing::new(wire::string(&d));
    zeroize::Zeroize::zeroize(&mut d);
    k
}

/// The server's side: its public value for the reply and K, encoded as
/// the exchange hash takes it. The ephemeral secrets are dropped here.
pub(crate) fn server_reply(
    kex: Kex,
    client_pub: &[u8],
) -> Result<(Vec<u8>, Zeroizing<Vec<u8>>), KexFailed> {
    match kex {
        Kex::Curve25519 => {
            let (s, p) = x25519_pair();
            let ss = x25519_agree(&s, client_pub)?;
            let k = Zeroizing::new(Writer::default().mpint(&ss[..]).finish());
            Ok((p.to_vec(), k))
        }
        Kex::MlKem768X25519 => {
            if client_pub.len() != MLKEM768_EK + 32 {
                return Err(KexFailed);
            }
            let (ek, x) = client_pub.split_at(MLKEM768_EK);
            let ek =
                ml_kem::ml_kem_768::EncapsulationKey::new(&ek.try_into().map_err(|_| KexFailed)?)
                    .map_err(|_| KexFailed)?;
            let mut m = ml_kem::B32::default();
            getrandom::fill(&mut m).expect("random bytes");
            let (ct, ss_m) = ek.encapsulate_deterministic(&m);
            zeroize::Zeroize::zeroize(&mut m[..]);
            let (s, p) = x25519_pair();
            let ss_x = x25519_agree(&s, x)?;
            let mut reply = ct.to_vec();
            reply.extend_from_slice(&p);
            Ok((reply, hybrid_k(&ss_m, &ss_x[..])))
        }
    }
}

/// The client's side (tests): its secrets and public value.
#[cfg(feature = "test-utils")]
pub(crate) struct ClientKex {
    kex: Kex,
    x: x25519_dalek::StaticSecret,
    dk: Option<Box<ml_kem::ml_kem_768::DecapsulationKey>>,
}

#[cfg(feature = "test-utils")]
impl ClientKex {
    pub(crate) fn start(kex: Kex) -> (ClientKex, Vec<u8>) {
        use ml_kem::{FromSeed as _, KeyExport as _};
        let (x, xp) = x25519_pair();
        match kex {
            Kex::Curve25519 => (ClientKex { kex, x, dk: None }, xp.to_vec()),
            Kex::MlKem768X25519 => {
                let mut seed = ml_kem::Seed::default();
                getrandom::fill(&mut seed).expect("random bytes");
                let (dk, ek) = ml_kem::MlKem768::from_seed(&seed);
                let mut p = ek.to_bytes().to_vec();
                p.extend_from_slice(&xp);
                (
                    ClientKex {
                        kex,
                        x,
                        dk: Some(Box::new(dk)),
                    },
                    p,
                )
            }
        }
    }

    pub(crate) fn finish(self, server_pub: &[u8]) -> Result<Zeroizing<Vec<u8>>, KexFailed> {
        use ml_kem::Decapsulate as _;
        match self.kex {
            Kex::Curve25519 => {
                let ss = x25519_agree(&self.x, server_pub)?;
                Ok(Zeroizing::new(Writer::default().mpint(&ss[..]).finish()))
            }
            Kex::MlKem768X25519 => {
                if server_pub.len() != MLKEM768_CT + 32 {
                    return Err(KexFailed);
                }
                let (ct, x) = server_pub.split_at(MLKEM768_CT);
                let ct: ml_kem::ml_kem_768::Ciphertext = ct.try_into().map_err(|_| KexFailed)?;
                let ss_m = self.dk.expect("a hybrid key").decapsulate(&ct);
                let ss_x = x25519_agree(&self.x, x)?;
                Ok(hybrid_k(&ss_m, &ss_x[..]))
            }
        }
    }
}

/// The inputs of the exchange hash (RFC 4253 §8, RFC 8731 §3.1).
pub(crate) struct HashInput<'a> {
    pub(crate) v_c: &'a [u8],
    pub(crate) v_s: &'a [u8],
    pub(crate) i_c: &'a [u8],
    pub(crate) i_s: &'a [u8],
    pub(crate) k_s: &'a [u8],
    pub(crate) q_c: &'a [u8],
    pub(crate) q_s: &'a [u8],
    /// K, already encoded (mpint or string).
    pub(crate) k: &'a [u8],
}

pub(crate) fn exchange_hash(i: &HashInput) -> [u8; 32] {
    let mut h = Sha256::new();
    for s in [i.v_c, i.v_s, i.i_c, i.i_s, i.k_s, i.q_c, i.q_s] {
        h.update((s.len() as u32).to_be_bytes());
        h.update(s);
    }
    h.update(i.k);
    h.finalize().into()
}

/// RFC 4253 §7.2: `HASH(K ‖ H ‖ letter ‖ session_id)`, extended with
/// `HASH(K ‖ H ‖ K1 ‖ …)` to `len` bytes.
fn derive(k: &[u8], h: &[u8], sid: &[u8], letter: u8, len: usize) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(len + 32));
    let mut d = Sha256::new();
    d.update(k);
    d.update(h);
    d.update([letter]);
    d.update(sid);
    out.extend_from_slice(&d.finalize());
    while out.len() < len {
        let mut d = Sha256::new();
        d.update(k);
        d.update(h);
        d.update(&out[..]);
        out.extend_from_slice(&d.finalize());
    }
    out.truncate(len);
    out
}

/// One direction's keys: client to server (`c2s`, IV "A" and key "C") or
/// server to client ("B" and "D").
pub(crate) fn keys(k: &[u8], h: &[u8], sid: &[u8], cipher: Cipher, c2s: bool) -> Keys {
    let (iv_l, key_l) = if c2s { (b'A', b'C') } else { (b'B', b'D') };
    let iv = derive(k, h, sid, iv_l, cipher.iv_len());
    let key = derive(k, h, sid, key_l, cipher.key_len());
    Keys::new(cipher, &key, &iv).expect("derived lengths")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_follows_the_client() {
        let ours = [Kex::MlKem768X25519, Kex::Curve25519];
        let ciphers = [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm];
        let mut w = Writer::new(MSG_KEXINIT);
        w.raw(&[0; 16])
            .names(&[
                "sntrup761x25519-sha512",
                "curve25519-sha256@libssh.org",
                "ext-info-c",
            ])
            .names(&["ecdsa-sha2-nistp256", HOST_KEY])
            .names(&["aes128-ctr", "aes256-gcm@openssh.com"])
            .names(&["chacha20-poly1305@openssh.com"])
            .names(&["hmac-sha1"])
            .names(&["hmac-sha1"])
            .names(&["zlib", "none"])
            .names(&["none"])
            .names(&[])
            .names(&[])
            .bool(true)
            .u32(0);
        let p = w.finish();
        let c = negotiate(&parse_kexinit(&p).unwrap(), &ours, &ciphers).unwrap();
        assert_eq!(
            c,
            Chosen {
                kex: Kex::Curve25519,
                c2s: Cipher::Aes256Gcm,
                s2c: Cipher::Chacha20Poly1305,
                wrong_guess: true,
            }
        );
        assert!(
            negotiate(
                &parse_kexinit(&p).unwrap(),
                &[Kex::MlKem768X25519],
                &ciphers
            )
            .is_none()
        );
        // Trailing bytes are refused.
        let mut q = p.clone();
        q.push(0);
        assert!(parse_kexinit(&q).is_err());
    }

    #[test]
    fn both_methods_agree() {
        for kex in [Kex::Curve25519, Kex::MlKem768X25519] {
            let (c, q_c) = ClientKex::start(kex);
            let (q_s, k_s) = server_reply(kex, &q_c).unwrap();
            let k_c = c.finish(&q_s).unwrap();
            assert_eq!(k_s, k_c);
            assert!(server_reply(kex, &q_c[1..]).is_err());
        }
        // X25519's all-zero point is refused.
        assert!(server_reply(Kex::Curve25519, &[0; 32]).is_err());
    }

    #[test]
    fn derivation_extends() {
        let a = derive(b"k", b"h", b"s", b'C', 64);
        let b = derive(b"k", b"h", b"s", b'C', 32);
        assert_eq!(&a[..32], &b[..]);
        let mut d = Sha256::new();
        d.update(b"k");
        d.update(b"h");
        d.update(&a[..32]);
        assert_eq!(&a[32..], &d.finalize()[..]);
    }
}
