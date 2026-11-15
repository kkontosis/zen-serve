//! Host keys (`ssh-mldsa44-ed25519`, `ssh-ed25519`) and user keys (those
//! two, and `ssh-rsa` with `rsa-sha2-256`/`rsa-sha2-512`).

use super::Error;
use super::wire::{Reader, Writer};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use ed25519_dalek::Signer as _;
use ml_dsa::MlDsa44;
use sha2::{Digest, Sha256, Sha512};
use std::fmt;
use zeroize::Zeroizing;

const ED25519: &str = "ssh-ed25519";
const MLDSA44_ED25519: &str = "ssh-mldsa44-ed25519";
const RSA: &str = "ssh-rsa";

/// ML-DSA-44's public key, signature, and the hybrid's sizes.
const MLDSA44_PK: usize = 1312;
const MLDSA44_SIG: usize = 2420;
const HYBRID_PK: usize = MLDSA44_PK + 32;
const HYBRID_SIG: usize = MLDSA44_SIG + 64;

/// The composite signature's prefix and label (draft-ietf-lamps-pq-
/// composite-sigs, as OpenSSH 10.6's `ssh-mldsa-eddsa.c`); the label is
/// also ML-DSA's context string.
const COMPOSITE_PREFIX: &[u8] = b"CompositeAlgorithmSignatures2025";
const COMPOSITE_LABEL: &[u8] = b"COMPSIG-MLDSA44-Ed25519-SHA512";

/// M′ = prefix ‖ label ‖ len(ctx) = 0 ‖ SHA-512(data): what both halves of
/// an `ssh-mldsa44-ed25519` signature sign.
fn composite_message(data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(COMPOSITE_PREFIX.len() + COMPOSITE_LABEL.len() + 1 + 64);
    m.extend_from_slice(COMPOSITE_PREFIX);
    m.extend_from_slice(COMPOSITE_LABEL);
    m.push(0);
    m.extend_from_slice(&Sha512::digest(data));
    m
}

/// A host-key algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAlg {
    /// `ssh-mldsa44-ed25519`: ML-DSA-44 and Ed25519, both must verify.
    MlDsa44Ed25519,
    /// `ssh-ed25519`.
    Ed25519,
}

impl HostAlg {
    /// The SSH name.
    pub fn name(self) -> &'static str {
        match self {
            HostAlg::MlDsa44Ed25519 => MLDSA44_ED25519,
            HostAlg::Ed25519 => ED25519,
        }
    }

    /// The algorithm whose SSH name is `n`.
    pub fn from_name(n: &str) -> Option<HostAlg> {
        [HostAlg::MlDsa44Ed25519, HostAlg::Ed25519]
            .into_iter()
            .find(|h| h.name() == n)
    }

    pub(crate) fn code(self) -> u64 {
        match self {
            HostAlg::MlDsa44Ed25519 => 1,
            HostAlg::Ed25519 => 2,
        }
    }

    pub(crate) fn from_code(c: u64) -> Option<HostAlg> {
        match c {
            1 => Some(HostAlg::MlDsa44Ed25519),
            2 => Some(HostAlg::Ed25519),
            _ => None,
        }
    }
}

#[derive(Clone)]
enum Secret {
    Ed25519(Box<ed25519_dalek::SigningKey>),
    Hybrid(Box<Hybrid>),
}

/// The hybrid's secret: ML-DSA-44's expanded key and its seed ξ, and the
/// Ed25519 key.
#[derive(Clone)]
struct Hybrid {
    ml: ml_dsa::ExpandedSigningKey<MlDsa44>,
    ml_seed: Zeroizing<[u8; 32]>,
    ed: ed25519_dalek::SigningKey,
}

/// A host key: `ssh-ed25519` or `ssh-mldsa44-ed25519`. Also a user key, in
/// the test client.
#[derive(Clone)]
pub struct HostKey {
    sk: Secret,
}

impl fmt::Debug for HostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostKey")
            .field("public", &self.public())
            .finish_non_exhaustive()
    }
}

impl HostKey {
    /// The `ssh-ed25519` key whose 32-byte seed (RFC 8032's private key) is
    /// `seed`.
    pub fn from_seed(seed: &[u8; 32]) -> HostKey {
        HostKey {
            sk: Secret::Ed25519(Box::new(ed25519_dalek::SigningKey::from_bytes(seed))),
        }
    }

    /// The `ssh-mldsa44-ed25519` key of the ML-DSA-44 seed ξ (FIPS 204's
    /// KeyGen_internal) and the Ed25519 seed, as OpenSSH keeps it.
    pub fn hybrid_from_seeds(ml_seed: &[u8; 32], ed_seed: &[u8; 32]) -> HostKey {
        HostKey {
            sk: Secret::Hybrid(Box::new(Hybrid {
                ml: ml_dsa::ExpandedSigningKey::<MlDsa44>::from_seed(&(*ml_seed).into()),
                ml_seed: Zeroizing::new(*ml_seed),
                ed: ed25519_dalek::SigningKey::from_bytes(ed_seed),
            })),
        }
    }

    /// A new random key of `alg`.
    pub fn generate(alg: HostAlg) -> HostKey {
        let mut s = Zeroizing::new([0u8; 64]);
        getrandom::fill(&mut s[..]).expect("random bytes");
        let (a, b) = s.split_at(32);
        match alg {
            HostAlg::Ed25519 => HostKey::from_seed(a.try_into().expect("32 bytes")),
            HostAlg::MlDsa44Ed25519 => HostKey::hybrid_from_seeds(
                a.try_into().expect("32 bytes"),
                b.try_into().expect("32 bytes"),
            ),
        }
    }

    /// The algorithm.
    pub fn alg(&self) -> HostAlg {
        match self.sk {
            Secret::Ed25519(_) => HostAlg::Ed25519,
            Secret::Hybrid(_) => HostAlg::MlDsa44Ed25519,
        }
    }

    /// An unencrypted `openssh-key-v1` key of either kind, as `ssh-keygen
    /// -t ed25519 -N ''` or (OpenSSH 10.6 and later) `ssh-keygen -t
    /// mldsa44-ed25519 -N ''` writes it.
    pub fn from_openssh(pem: &str) -> Result<HostKey, Error> {
        let bad = || Error::Key("not an unencrypted OpenSSH Ed25519 or ML-DSA-44-Ed25519 key");
        let body = pem
            .trim()
            .strip_prefix("-----BEGIN OPENSSH PRIVATE KEY-----")
            .and_then(|s| s.strip_suffix("-----END OPENSSH PRIVATE KEY-----"))
            .ok_or_else(bad)?;
        let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let der = Zeroizing::new(STANDARD.decode(b64).map_err(|_| bad())?);
        let mut r = Reader::new(&der);
        if r.raw(15).map_err(|_| bad())? != b"openssh-key-v1\0" {
            return Err(bad());
        }
        let fields = (|| {
            let cipher = r.string()?;
            let kdf = r.string()?;
            let kdf_opts = r.string()?;
            let n = r.u32()?;
            let public = r.string()?;
            let private = r.string()?;
            r.done()?;
            Ok::<_, super::wire::Malformed>((cipher, kdf, kdf_opts, n, public, private))
        })();
        let (cipher, kdf, kdf_opts, n, public, private) = fields.map_err(|_| bad())?;
        if cipher != b"none" || kdf != b"none" || !kdf_opts.is_empty() {
            return Err(Error::Key("an encrypted OpenSSH key"));
        }
        if n != 1 {
            return Err(bad());
        }
        let mut p = Reader::new(private);
        let key = (|| {
            let (c1, c2) = (p.u32()?, p.u32()?);
            let typ = p.string()?;
            let pk = p.string()?;
            let sk = p.string()?;
            p.string()?;
            let pad = p.rest();
            Ok::<_, super::wire::Malformed>((c1, c2, typ, pk, sk, pad))
        })();
        let (c1, c2, typ, pk, sk, pad) = key.map_err(|_| bad())?;
        let padded = pad.iter().enumerate().all(|(i, &b)| b as usize == i + 1);
        if c1 != c2 || !padded {
            return Err(bad());
        }
        let k = if typ == ED25519.as_bytes() {
            // RFC 8032's private key, then the public key.
            if pk.len() != 32 || sk.len() != 64 || &sk[32..] != pk {
                return Err(bad());
            }
            HostKey::from_seed(sk[..32].try_into().expect("32 bytes"))
        } else if typ == MLDSA44_ED25519.as_bytes() {
            // ML-DSA-44's seed ξ, then Ed25519's seed.
            if pk.len() != HYBRID_PK || sk.len() != 64 {
                return Err(bad());
            }
            HostKey::hybrid_from_seeds(
                sk[..32].try_into().expect("32 bytes"),
                sk[32..].try_into().expect("32 bytes"),
            )
        } else {
            return Err(bad());
        };
        let b = k.blob();
        if b != public || Reader::new(&b).string().ok() != Some(typ) || k.key_bytes() != pk {
            return Err(Error::Key("an OpenSSH key whose public half doesn't match"));
        }
        Ok(k)
    }

    /// The key as an unencrypted `openssh-key-v1` PEM, as `ssh-keygen -N ''`
    /// writes it (with an empty comment): what [`HostKey::from_openssh`]
    /// reads, and OpenSSH too.
    pub fn to_openssh(&self) -> Zeroizing<String> {
        let pk = self.key_bytes();
        let sk = Zeroizing::new(match &self.sk {
            // RFC 8032's private key, then the public key.
            Secret::Ed25519(k) => [&k.to_bytes()[..], &pk[..]].concat(),
            // ML-DSA-44's seed ξ, then Ed25519's seed.
            Secret::Hybrid(h) => [&h.ml_seed[..], &h.ed.to_bytes()[..]].concat(),
        });
        let mut check = [0u8; 4];
        getrandom::fill(&mut check).expect("random bytes");
        let mut private = Writer::default();
        private
            .raw(&check)
            .raw(&check)
            .string(self.alg().name().as_bytes())
            .string(&pk)
            .string(&sk)
            .string(b"");
        let mut private = Zeroizing::new(private.finish());
        // Padded to the cipher's block size, 8 for "none": 1, 2, 3, ….
        let mut i = 1u8;
        while private.len() % 8 != 0 {
            private.push(i);
            i += 1;
        }
        let mut der = Writer::default();
        der.raw(b"openssh-key-v1\0")
            .string(b"none")
            .string(b"none")
            .string(b"")
            .u32(1)
            .string(&self.blob())
            .string(&private);
        let der = Zeroizing::new(der.finish());
        let b64 = Zeroizing::new(STANDARD.encode(&der[..]));
        let mut pem = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
        for line in b64.as_bytes().chunks(70) {
            pem.push_str(std::str::from_utf8(line).expect("base64"));
            pem.push('\n');
        }
        pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
        Zeroizing::new(pem)
    }

    /// The public key bytes inside the blob.
    fn key_bytes(&self) -> Vec<u8> {
        match &self.sk {
            Secret::Ed25519(k) => k.verifying_key().as_bytes().to_vec(),
            Secret::Hybrid(h) => {
                let mut v = h.ml.verifying_key().encode().to_vec();
                v.extend_from_slice(h.ed.verifying_key().as_bytes());
                v
            }
        }
    }

    /// The public key.
    pub fn public(&self) -> PublicKey {
        PublicKey::from_blob(&self.blob()).expect("our own key")
    }

    /// `ssh-ed25519 AAAA…` or `ssh-mldsa44-ed25519 AAAA…`, for
    /// `known_hosts` (prefixed with the host).
    pub fn public_line(&self) -> String {
        self.public().to_line()
    }

    pub(crate) fn blob(&self) -> Vec<u8> {
        Writer::default()
            .string(self.alg().name().as_bytes())
            .string(&self.key_bytes())
            .finish()
    }

    /// The signature blob over `data`. ML-DSA signs hedged (FIPS 204's
    /// randomized variant), as OpenSSH does.
    pub(crate) fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sig = match &self.sk {
            Secret::Ed25519(k) => k.sign(data).to_bytes().to_vec(),
            Secret::Hybrid(h) => {
                let m = composite_message(data);
                let ml =
                    h.ml.sign_randomized(&m, COMPOSITE_LABEL, &mut super::kex::rng())
                        .expect("a context under 256 bytes, and random bytes");
                let mut v = ml.encode().to_vec();
                v.extend_from_slice(&h.ed.sign(&m).to_bytes());
                v
            }
        };
        Writer::default()
            .string(self.alg().name().as_bytes())
            .string(&sig)
            .finish()
    }
}

#[derive(Clone)]
enum Kind {
    Ed25519(Box<ed25519_dalek::VerifyingKey>),
    Hybrid(Box<(ml_dsa::VerifyingKey<MlDsa44>, ed25519_dalek::VerifyingKey)>),
    Rsa(Box<rsa::RsaPublicKey>),
}

/// A user's or a host's public key: `ssh-mldsa44-ed25519`, `ssh-ed25519`,
/// or `ssh-rsa` within the RSA key policy (2,048 to 4,096 bits). Equal when
/// their blobs are.
#[derive(Clone)]
pub struct PublicKey {
    blob: Vec<u8>,
    kind: Kind,
}

impl PartialEq for PublicKey {
    fn eq(&self, o: &PublicKey) -> bool {
        self.blob == o.blob
    }
}

impl Eq for PublicKey {}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({} {})", self.algorithm(), self.fingerprint())
    }
}

impl PublicKey {
    /// The key in its SSH encoding (RFC 4253 §6.6, RFC 8709).
    pub fn from_blob(blob: &[u8]) -> Result<PublicKey, Error> {
        let bad = Error::Key("a malformed public key");
        let mut r = Reader::new(blob);
        let typ = r.string().map_err(|_| bad.clone())?;
        let kind = if typ == ED25519.as_bytes() {
            let pk = r.string().map_err(|_| bad.clone())?;
            let pk: &[u8; 32] = pk.try_into().map_err(|_| bad.clone())?;
            Kind::Ed25519(Box::new(
                ed25519_dalek::VerifyingKey::from_bytes(pk).map_err(|_| bad.clone())?,
            ))
        } else if typ == MLDSA44_ED25519.as_bytes() {
            let pk = r.string().map_err(|_| bad.clone())?;
            if pk.len() != HYBRID_PK {
                return Err(bad);
            }
            let (ml, ed) = pk.split_at(MLDSA44_PK);
            let ml =
                ml_dsa::EncodedVerifyingKey::<MlDsa44>::try_from(ml).map_err(|_| bad.clone())?;
            let ed: &[u8; 32] = ed.try_into().map_err(|_| bad.clone())?;
            Kind::Hybrid(Box::new((
                ml_dsa::VerifyingKey::decode(&ml),
                ed25519_dalek::VerifyingKey::from_bytes(ed).map_err(|_| bad.clone())?,
            )))
        } else if typ == RSA.as_bytes() {
            let e = r.mpint().map_err(|_| bad.clone())?;
            let n = r.mpint().map_err(|_| bad.clone())?;
            let k = zen_tls_rustcrypto::rsakey::from_components(n, e).map_err(|e| match e {
                zen_tls_rustcrypto::rsakey::Refused::Policy(p) => Error::Key(p),
                zen_tls_rustcrypto::rsakey::Refused::Malformed => bad.clone(),
            })?;
            Kind::Rsa(Box::new(k))
        } else {
            return Err(Error::Key("an unsupported key type"));
        };
        r.done().map_err(|_| bad)?;
        Ok(PublicKey {
            blob: blob.to_vec(),
            kind,
        })
    }

    /// A line of `authorized_keys` without options, or of a `.pub` file:
    /// `<type> <base64> [comment]`.
    pub fn from_line(line: &str) -> Result<PublicKey, Error> {
        let mut parts = line.split_ascii_whitespace();
        let (Some(typ), Some(b64)) = (parts.next(), parts.next()) else {
            return Err(Error::Key("not a public key line"));
        };
        let blob = STANDARD
            .decode(b64)
            .map_err(|_| Error::Key("not a public key line"))?;
        let k = PublicKey::from_blob(&blob)?;
        if k.algorithm() != typ {
            return Err(Error::Key("a key whose type doesn't match its line"));
        }
        Ok(k)
    }

    /// The SSH encoding.
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    /// `ssh-mldsa44-ed25519`, `ssh-ed25519` or `ssh-rsa`.
    pub fn algorithm(&self) -> &'static str {
        match self.kind {
            Kind::Ed25519(_) => ED25519,
            Kind::Hybrid(_) => MLDSA44_ED25519,
            Kind::Rsa(_) => RSA,
        }
    }

    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> String {
        format!(
            "SHA256:{}",
            STANDARD_NO_PAD.encode(Sha256::digest(&self.blob))
        )
    }

    /// `<type> <base64>`.
    pub fn to_line(&self) -> String {
        format!("{} {}", self.algorithm(), STANDARD.encode(&self.blob))
    }

    /// Whether `alg` may sign with this key: its own type, or
    /// `rsa-sha2-256`/`rsa-sha2-512` for RSA (not SHA-1's `ssh-rsa`).
    pub(crate) fn allows(&self, alg: &str) -> bool {
        match self.kind {
            Kind::Ed25519(_) => alg == ED25519,
            Kind::Hybrid(_) => alg == MLDSA44_ED25519,
            Kind::Rsa(_) => alg == "rsa-sha2-256" || alg == "rsa-sha2-512",
        }
    }

    /// Checks the signature blob `sig` by `alg` over `data`.
    pub(crate) fn verify(&self, alg: &str, data: &[u8], sig: &[u8]) -> bool {
        if !self.allows(alg) {
            return false;
        }
        let mut r = Reader::new(sig);
        let (Ok(salg), Ok(s)) = (r.string(), r.string()) else {
            return false;
        };
        if salg != alg.as_bytes() || r.done().is_err() {
            return false;
        }
        match &self.kind {
            Kind::Ed25519(vk) => {
                let Ok(s) = <&[u8; 64]>::try_from(s) else {
                    return false;
                };
                vk.verify_strict(data, &ed25519_dalek::Signature::from_bytes(s))
                    .is_ok()
            }
            Kind::Hybrid(h) => {
                // Both halves must verify, over the same M′.
                if s.len() != HYBRID_SIG {
                    return false;
                }
                let (ml, ed) = s.split_at(MLDSA44_SIG);
                let m = composite_message(data);
                let ml_ok = ml_dsa::EncodedSignature::<MlDsa44>::try_from(ml)
                    .ok()
                    .and_then(|e| ml_dsa::Signature::<MlDsa44>::decode(&e))
                    .is_some_and(|sig| h.0.verify_with_context(&m, COMPOSITE_LABEL, &sig));
                let ed: &[u8; 64] = ed.try_into().expect("64 bytes");
                let ed_ok =
                    h.1.verify_strict(&m, &ed25519_dalek::Signature::from_bytes(ed))
                        .is_ok();
                ml_ok && ed_ok
            }
            Kind::Rsa(k) => {
                use rsa::signature::Verifier as _;
                // RFC 8332 §3: the signature is as long as the modulus;
                // OpenSSH also accepts a shorter one, left-padded.
                let size = rsa::traits::PublicKeyParts::size(&**k);
                if s.len() > size {
                    return false;
                }
                let mut full = vec![0u8; size - s.len()];
                full.extend_from_slice(s);
                let Ok(sig) = rsa::pkcs1v15::Signature::try_from(&full[..]) else {
                    return false;
                };
                let key = (**k).clone();
                if alg == "rsa-sha2-256" {
                    rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(key)
                        .verify(data, &sig)
                        .is_ok()
                } else {
                    rsa::pkcs1v15::VerifyingKey::<sha2::Sha512>::new(key)
                        .verify(data, &sig)
                        .is_ok()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_lines() {
        let k = HostKey::from_seed(&[7; 32]);
        let line = k.public_line();
        assert!(line.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        let p = PublicKey::from_line(&format!("{line} a comment")).unwrap();
        assert_eq!(p, k.public());
        let sig = k.sign(b"data");
        assert!(p.verify("ssh-ed25519", b"data", &sig));
        assert!(!p.verify("ssh-ed25519", b"datb", &sig));
        assert!(!p.verify("rsa-sha2-256", b"data", &sig));
        assert!(PublicKey::from_line("ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAI").is_err());
        assert!(PublicKey::from_line(&line.replace("ssh-ed25519", "ssh-rsa")).is_err());
    }

    fn hybrid() -> HostKey {
        HostKey::hybrid_from_seeds(&[3; 32], &[4; 32])
    }

    #[test]
    fn hybrid_lines_and_sizes() {
        let k = hybrid();
        assert_eq!(k.alg(), HostAlg::MlDsa44Ed25519);
        // string(name) ‖ string(1,312 + 32 bytes).
        assert_eq!(k.blob().len(), 4 + MLDSA44_ED25519.len() + 4 + HYBRID_PK);
        let line = k.public_line();
        assert!(line.starts_with("ssh-mldsa44-ed25519 AAAAE3NzaC1tbGRzYTQ0LWVkMjU1MTk"));
        let p = PublicKey::from_line(&format!("{line} a comment")).unwrap();
        assert_eq!(p, k.public());
        assert_eq!(p.algorithm(), MLDSA44_ED25519);
        assert_eq!(p.fingerprint(), k.public().fingerprint());
        assert!(p.allows(MLDSA44_ED25519) && !p.allows(ED25519));
        let sig = k.sign(b"data");
        assert_eq!(sig.len(), 4 + MLDSA44_ED25519.len() + 4 + HYBRID_SIG);
        assert!(p.verify(MLDSA44_ED25519, b"data", &sig));
        assert!(!p.verify(MLDSA44_ED25519, b"datb", &sig));
        assert!(!p.verify(ED25519, b"data", &sig));
        // Hedged: two signatures of the same data differ, and both verify.
        let sig2 = k.sign(b"data");
        assert_ne!(sig, sig2);
        assert!(p.verify(MLDSA44_ED25519, b"data", &sig2));
        // The same seeds give the same key; a short or long key is refused.
        assert_eq!(HostKey::hybrid_from_seeds(&[3; 32], &[4; 32]).public(), p);
        let mut b = k.blob();
        b.pop();
        assert!(PublicKey::from_blob(&b).is_err());
        // A line whose type names the other kind is refused.
        assert!(PublicKey::from_line(&line.replace(MLDSA44_ED25519, ED25519)).is_err());
    }

    /// A signature blob over `data` whose halves come from different keys
    /// or messages.
    fn mixed(ml_from: &HostKey, ed_from: &HostKey, data: &[u8], ed_data: &[u8]) -> Vec<u8> {
        let half = |k: &HostKey, d: &[u8]| {
            let s = k.sign(d);
            let mut r = Reader::new(&s);
            r.string().unwrap();
            r.string().unwrap().to_vec()
        };
        let a = half(ml_from, data);
        let b = half(ed_from, ed_data);
        let mut v = a[..MLDSA44_SIG].to_vec();
        v.extend_from_slice(&b[MLDSA44_SIG..]);
        Writer::default()
            .string(MLDSA44_ED25519.as_bytes())
            .string(&v)
            .finish()
    }

    #[test]
    fn hybrid_needs_both_halves() {
        let k = hybrid();
        let other = HostKey::hybrid_from_seeds(&[5; 32], &[6; 32]);
        let p = k.public();
        assert!(p.verify(MLDSA44_ED25519, b"d", &mixed(&k, &k, b"d", b"d")));
        // A good Ed25519 half and an ML-DSA half from another key.
        assert!(!p.verify(MLDSA44_ED25519, b"d", &mixed(&other, &k, b"d", b"d")));
        // A good ML-DSA half and an Ed25519 half from another key.
        assert!(!p.verify(MLDSA44_ED25519, b"d", &mixed(&k, &other, b"d", b"d")));
        // An Ed25519 half over other data.
        assert!(!p.verify(MLDSA44_ED25519, b"d", &mixed(&k, &k, b"d", b"e")));
        // A flipped bit in the ML-DSA half.
        let mut s = k.sign(b"d");
        s[20] ^= 1;
        assert!(!p.verify(MLDSA44_ED25519, b"d", &s));
        // Trailing bytes, or a short signature.
        let mut s = k.sign(b"d");
        s.push(0);
        assert!(!p.verify(MLDSA44_ED25519, b"d", &s));
    }

    #[test]
    fn openssh_pems_round_trip() {
        for k in [HostKey::from_seed(&[7; 32]), hybrid()] {
            let pem = k.to_openssh();
            assert!(pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n"));
            let back = HostKey::from_openssh(&pem).unwrap();
            assert_eq!(back.public(), k.public());
            assert_eq!(back.alg(), k.alg());
            // The secret came through: the reloaded key's signatures verify.
            assert!(k.public().verify(k.alg().name(), b"m", &back.sign(b"m")));
        }
        // A hybrid PEM whose seed doesn't match its public key is refused.
        let a = hybrid().to_openssh();
        let b = HostKey::hybrid_from_seeds(&[8; 32], &[4; 32]).to_openssh();
        let decode = |p: &str| {
            let b64: String = p.lines().filter(|l| !l.starts_with("-----")).collect();
            STANDARD.decode(b64).unwrap()
        };
        let (da, db) = (decode(&a), decode(&b));
        // The public blob of `a` with the private section of `b`.
        // The magic, "none", "none", "", the key count, then the blob's
        // length, name and key.
        let pub_end = 15 + (4 + 4) * 2 + 4 + 4 + 4 + (4 + MLDSA44_ED25519.len()) + (4 + HYBRID_PK);
        let mut mixed = da[..pub_end].to_vec();
        mixed.extend_from_slice(&db[pub_end..]);
        let pem = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
            STANDARD.encode(&mixed)
        );
        assert_eq!(
            HostKey::from_openssh(&pem).unwrap_err(),
            Error::Key("an OpenSSH key whose public half doesn't match")
        );
    }

    #[test]
    fn composite_message_layout() {
        let m = composite_message(b"x");
        assert_eq!(
            &m[..62],
            b"CompositeAlgorithmSignatures2025COMPSIG-MLDSA44-Ed25519-SHA512"
        );
        assert_eq!(m[62], 0);
        assert_eq!(&m[63..], &Sha512::digest(b"x")[..]);
    }
}
