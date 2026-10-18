//! The host key (`ssh-ed25519`) and user keys (`ssh-ed25519`, `ssh-rsa`
//! with `rsa-sha2-256`/`rsa-sha2-512`).

use super::Error;
use super::wire::{Reader, Writer};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use ed25519_dalek::Signer as _;
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

const ED25519: &str = "ssh-ed25519";
const RSA: &str = "ssh-rsa";

/// The server's `ssh-ed25519` host key.
#[derive(Clone)]
pub struct HostKey {
    sk: ed25519_dalek::SigningKey,
}

impl fmt::Debug for HostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostKey")
            .field("public", &self.public().fingerprint())
            .finish_non_exhaustive()
    }
}

impl HostKey {
    /// The key whose 32-byte seed (RFC 8032's private key) is `seed`.
    pub fn from_seed(seed: &[u8; 32]) -> HostKey {
        HostKey {
            sk: ed25519_dalek::SigningKey::from_bytes(seed),
        }
    }

    /// An unencrypted `openssh-key-v1` Ed25519 key, as `ssh-keygen -t
    /// ed25519 -N ''` writes it.
    pub fn from_openssh(pem: &str) -> Result<HostKey, Error> {
        let bad = || Error::Key("not an unencrypted OpenSSH Ed25519 key");
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
        if c1 != c2 || typ != ED25519.as_bytes() || pk.len() != 32 || sk.len() != 64 || !padded {
            return Err(bad());
        }
        if &sk[32..] != pk {
            return Err(bad());
        }
        let k = HostKey::from_seed(sk[..32].try_into().expect("32 bytes"));
        if k.blob() != public || k.sk.verifying_key().as_bytes() != pk {
            return Err(Error::Key("an OpenSSH key whose public half doesn't match"));
        }
        Ok(k)
    }

    /// The public key.
    pub fn public(&self) -> PublicKey {
        PublicKey::from_blob(&self.blob()).expect("our own key")
    }

    /// `ssh-ed25519 AAAA…`, for `known_hosts` (prefixed with the host).
    pub fn public_line(&self) -> String {
        self.public().to_line()
    }

    pub(crate) fn blob(&self) -> Vec<u8> {
        Writer::default()
            .string(ED25519.as_bytes())
            .string(self.sk.verifying_key().as_bytes())
            .finish()
    }

    /// The signature blob over `data`.
    pub(crate) fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sig = self.sk.sign(data);
        Writer::default()
            .string(ED25519.as_bytes())
            .string(&sig.to_bytes())
            .finish()
    }
}

#[derive(Clone)]
enum Kind {
    Ed25519(Box<ed25519_dalek::VerifyingKey>),
    Rsa(Box<rsa::RsaPublicKey>),
}

/// A user's public key: `ssh-ed25519`, or `ssh-rsa` within the RSA key
/// policy (2,048 to 4,096 bits). Equal when their blobs are.
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

    /// `ssh-ed25519` or `ssh-rsa`.
    pub fn algorithm(&self) -> &'static str {
        match self.kind {
            Kind::Ed25519(_) => ED25519,
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

    /// Whether `alg` may sign with this key: `ssh-ed25519`, or
    /// `rsa-sha2-256`/`rsa-sha2-512` for RSA (not SHA-1's `ssh-rsa`).
    pub(crate) fn allows(&self, alg: &str) -> bool {
        match self.kind {
            Kind::Ed25519(_) => alg == ED25519,
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
}
