//! A small WebAuthn verifier for passkeys (spec/auth.md §7), on RustCrypto
//! only: no OpenSSL, no attestation trust.
//!
//! * **Registration** ([`verify_registration`]): the client data, the
//!   attestation object's `authData` (relying-party hash, flags, the
//!   attested credential and its COSE public key). The attestation
//!   statement is **not** verified: `fmt` `"none"` must carry an empty
//!   statement, and any other format's statement is ignored, so every
//!   registration counts as unattested.
//! * **Sign-in** ([`verify_assertion`]): the client data, `authData`, the
//!   signature over `authData ‖ SHA-256(clientDataJSON)` with the stored
//!   key, and the signature counter ([`check_counter`]).
//!
//! Algorithms: ES256 (COSE -7, ECDSA P-256 with SHA-256), EdDSA (COSE -8,
//! Ed25519) and RS256 (COSE -257, RSASSA-PKCS1-v1_5 with SHA-256, for
//! authenticators that only sign with RSA). RSA keys must have a modulus of
//! [`RSA_MIN_BITS`]..=[`RSA_MAX_BITS`] bits and an odd public exponent of
//! at least 65537 that fits 32 bits.
//!
//! The challenge's freshness and single use, and the origin policy, are the
//! server's (`auth::check_challenge`, `auth::issue_session`); this module
//! checks that the client data names the expected challenge and hands the
//! origin to a caller-supplied check.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ciborium::Value;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;

/// COSE algorithm: ECDSA P-256 with SHA-256.
pub const ALG_ES256: i64 = -7;
/// COSE algorithm: EdDSA (Ed25519).
pub const ALG_EDDSA: i64 = -8;
/// COSE algorithm: RSASSA-PKCS1-v1_5 with SHA-256.
pub const ALG_RS256: i64 = -257;
/// Supported COSE algorithms, in order of preference
/// (`pubKeyCredParams`). RS256 comes last: browsers pick the first one an
/// authenticator supports, and only RSA-only authenticators need it.
pub const ALGORITHMS: [i64; 3] = [ALG_EDDSA, ALG_ES256, ALG_RS256];

/// Smallest RSA modulus accepted, in bits.
pub const RSA_MIN_BITS: usize = 2048;
/// Largest RSA modulus accepted, in bits: larger keys only cost
/// verification time.
pub const RSA_MAX_BITS: usize = 4096;
/// Smallest RSA public exponent accepted (as FIPS 186-5).
pub const RSA_MIN_E: u64 = 65537;
/// Largest RSA public exponent accepted.
pub const RSA_MAX_E: u64 = u32::MAX as u64;

/// Max length of a credential id (WebAuthn Level 3).
pub const MAX_CREDENTIAL_ID: usize = 1023;

/// `authData` flag: user present.
pub const FLAG_UP: u8 = 0x01;
/// `authData` flag: user verified.
pub const FLAG_UV: u8 = 0x04;
/// `authData` flag: backup eligible.
pub const FLAG_BE: u8 = 0x08;
/// `authData` flag: backed up.
pub const FLAG_BS: u8 = 0x10;
/// `authData` flag: attested credential data included.
pub const FLAG_AT: u8 = 0x40;
/// `authData` flag: extension data included.
pub const FLAG_ED: u8 = 0x80;

/// `clientDataJSON.type` of a registration.
pub const TYPE_CREATE: &str = "webauthn.create";
/// `clientDataJSON.type` of a sign-in.
pub const TYPE_GET: &str = "webauthn.get";

/// Why a registration or an assertion was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Bytes that don't parse.
    Malformed(&'static str),
    /// `clientDataJSON.type` is not the expected one.
    Type,
    /// `clientDataJSON.challenge` is not the expected challenge.
    Challenge,
    /// The origin check refused `clientDataJSON.origin`.
    Origin,
    /// `clientDataJSON.crossOrigin` is true: a sign-in from an iframe.
    CrossOrigin,
    /// The `rpIdHash` is not SHA-256 of the relying-party id.
    RpIdHash,
    /// The user-present flag is clear.
    UserPresence,
    /// User verification is required and the flag is clear.
    UserVerification,
    /// A public key with an algorithm or curve this verifier doesn't support.
    Algorithm,
    /// A supported algorithm with a key outside the policy (RSA size or
    /// exponent).
    KeyPolicy(&'static str),
    /// The signature doesn't verify.
    Signature,
    /// The signature counter didn't increase: a possibly cloned
    /// authenticator.
    Counter {
        /// The stored counter.
        stored: u32,
        /// The counter in the assertion.
        got: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(what) => write!(f, "malformed {what}"),
            Error::Type => f.write_str("wrong client data type"),
            Error::Challenge => f.write_str("the client data names another challenge"),
            Error::Origin => f.write_str("origin not accepted"),
            Error::CrossOrigin => f.write_str("cross-origin WebAuthn requests are not accepted"),
            Error::RpIdHash => f.write_str("the relying-party id hash does not match"),
            Error::UserPresence => f.write_str("the authenticator did not confirm user presence"),
            Error::UserVerification => f.write_str(
                "the authenticator did not verify the user, and this server requires it",
            ),
            Error::Algorithm => f.write_str("unsupported public key algorithm"),
            Error::KeyPolicy(what) => write!(f, "public key refused: {what}"),
            Error::Signature => f.write_str("bad passkey signature"),
            Error::Counter { stored, got } => write!(
                f,
                "the signature counter went from {stored} to {got}: the authenticator may be cloned"
            ),
        }
    }
}

/// A verifier result.
pub type Result<T> = std::result::Result<T, Error>;

/// SHA-256.
pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

// ---------------------------------------------------------------- client data

/// The fields of `clientDataJSON` the server checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientData {
    /// `type`.
    pub typ: String,
    /// `challenge`, base64url-decoded.
    pub challenge: Vec<u8>,
    /// `origin`, as the browser serialized it.
    pub origin: String,
    /// `crossOrigin`.
    pub cross_origin: bool,
}

#[derive(Deserialize)]
struct RawClientData {
    #[serde(rename = "type")]
    typ: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: Option<bool>,
}

/// Parse `clientDataJSON`. Unknown members (`topOrigin`, `tokenBinding`,
/// …) are ignored; a duplicated member is refused.
pub fn parse_client_data(json: &[u8]) -> Result<ClientData> {
    let raw: RawClientData =
        serde_json::from_slice(json).map_err(|_| Error::Malformed("clientDataJSON"))?;
    let challenge = URL_SAFE_NO_PAD
        .decode(raw.challenge.as_bytes())
        .map_err(|_| Error::Malformed("clientDataJSON challenge"))?;
    Ok(ClientData {
        typ: raw.typ,
        challenge,
        origin: raw.origin,
        cross_origin: raw.cross_origin.unwrap_or(false),
    })
}

/// Check a parsed client data: its type, its challenge, that it isn't
/// cross-origin, and its origin with `origin_ok`.
fn check_client_data(
    cd: &ClientData,
    typ: &str,
    challenge: &[u8],
    origin_ok: &dyn Fn(&str) -> bool,
) -> Result<()> {
    if cd.typ != typ {
        return Err(Error::Type);
    }
    if cd.challenge != challenge {
        return Err(Error::Challenge);
    }
    if cd.cross_origin {
        return Err(Error::CrossOrigin);
    }
    if !origin_ok(&cd.origin) {
        return Err(Error::Origin);
    }
    Ok(())
}

// ---------------------------------------------------------------- COSE keys

/// A credential public key, from its COSE encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoseKey {
    /// ES256: an uncompressed P-256 point.
    Es256 {
        /// x coordinate.
        x: [u8; 32],
        /// y coordinate.
        y: [u8; 32],
    },
    /// EdDSA: an Ed25519 public key.
    Ed25519([u8; 32]),
    /// RS256: an RSA public key, big-endian without leading zeros.
    Rs256 {
        /// Modulus.
        n: Vec<u8>,
        /// Public exponent.
        e: Vec<u8>,
    },
}

const COSE_KTY: i64 = 1;
const COSE_ALG: i64 = 3;
const COSE_CRV: i64 = -1;
const COSE_X: i64 = -2;
const COSE_Y: i64 = -3;
const KTY_OKP: i64 = 1;
const KTY_EC2: i64 = 2;
const KTY_RSA: i64 = 3;
const COSE_RSA_N: i64 = -1;
const COSE_RSA_E: i64 = -2;
const CRV_P256: i64 = 1;
const CRV_ED25519: i64 = 6;

fn int(v: &Value) -> Option<i64> {
    v.as_integer().and_then(|i| i64::try_from(i).ok())
}

/// A big-endian unsigned integer from a COSE byte string, without its
/// leading zeros.
fn uint_bytes(v: Option<&Value>) -> Result<Vec<u8>> {
    let b = v
        .and_then(Value::as_bytes)
        .ok_or(Error::Malformed("COSE RSA key"))?;
    let start = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    Ok(b[start..].to_vec())
}

fn bytes32(v: Option<&Value>) -> Result<[u8; 32]> {
    v.and_then(Value::as_bytes)
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or(Error::Malformed("COSE key coordinate"))
}

impl CoseKey {
    /// Parse a COSE_Key (RFC 9053) of a supported algorithm. The map must
    /// have integer labels; labels this verifier doesn't use are ignored.
    pub fn decode(cbor: &[u8]) -> Result<Self> {
        let (v, rest) = read_cbor(cbor).ok_or(Error::Malformed("COSE key"))?;
        if !rest.is_empty() {
            return Err(Error::Malformed("COSE key"));
        }
        Self::from_value(&v)
    }

    fn from_value(v: &Value) -> Result<Self> {
        let map = v.as_map().ok_or(Error::Malformed("COSE key"))?;
        let get = |label: i64| {
            let mut found = map.iter().filter(|(k, _)| int(k) == Some(label));
            match (found.next(), found.next()) {
                (Some((_, v)), None) => Ok(Some(v)),
                (None, _) => Ok(None),
                (Some(_), Some(_)) => Err(Error::Malformed("COSE key: duplicate label")),
            }
        };
        let kty = get(COSE_KTY)?.and_then(int);
        let alg = get(COSE_ALG)?.and_then(int);
        let crv = get(COSE_CRV)?.and_then(int);
        match (kty, alg, crv) {
            (Some(KTY_EC2), Some(ALG_ES256), Some(CRV_P256)) => {
                let key = CoseKey::Es256 {
                    x: bytes32(get(COSE_X)?)?,
                    y: bytes32(get(COSE_Y)?)?,
                };
                // Refuse a point that isn't on the curve now, not at sign-in.
                key.p256()?;
                Ok(key)
            }
            (Some(KTY_OKP), Some(ALG_EDDSA), Some(CRV_ED25519)) => {
                let key = CoseKey::Ed25519(bytes32(get(COSE_X)?)?);
                key.ed25519()?;
                Ok(key)
            }
            // Label -1 is `n` for RSA, not a curve.
            (Some(KTY_RSA), Some(ALG_RS256), _) => {
                let key = CoseKey::Rs256 {
                    n: uint_bytes(get(COSE_RSA_N)?)?,
                    e: uint_bytes(get(COSE_RSA_E)?)?,
                };
                key.rsa()?;
                Ok(key)
            }
            _ => Err(Error::Algorithm),
        }
    }

    /// The COSE algorithm.
    pub fn alg(&self) -> i64 {
        match self {
            CoseKey::Es256 { .. } => ALG_ES256,
            CoseKey::Ed25519(_) => ALG_EDDSA,
            CoseKey::Rs256 { .. } => ALG_RS256,
        }
    }

    /// The RSA key, checked against the policy: a modulus of
    /// [`RSA_MIN_BITS`]..=[`RSA_MAX_BITS`] bits, an odd exponent in
    /// [`RSA_MIN_E`]..=[`RSA_MAX_E`] (and below the modulus).
    fn rsa(&self) -> Result<rsa::RsaPublicKey> {
        let CoseKey::Rs256 { n, e } = self else {
            return Err(Error::Algorithm);
        };
        let bits = match n.first() {
            Some(top) => n.len() * 8 - top.leading_zeros() as usize,
            None => 0,
        };
        if !(RSA_MIN_BITS..=RSA_MAX_BITS).contains(&bits) {
            return Err(Error::KeyPolicy(
                "an RSA modulus must have 2048 to 4096 bits",
            ));
        }
        if e.len() > 8 {
            return Err(Error::KeyPolicy("RSA public exponent out of range"));
        }
        let mut e8 = [0u8; 8];
        e8[8 - e.len()..].copy_from_slice(e);
        let e64 = u64::from_be_bytes(e8);
        if e64 % 2 == 0 || !(RSA_MIN_E..=RSA_MAX_E).contains(&e64) {
            return Err(Error::KeyPolicy(
                "the RSA public exponent must be odd, at least 65537 and fit 32 bits",
            ));
        }
        let n = rsa::BoxedUint::from_be_slice_vartime(n);
        let e = rsa::BoxedUint::from(e64);
        rsa::RsaPublicKey::new_with_max_size(n, e, RSA_MAX_BITS)
            .map_err(|_| Error::Malformed("RSA public key"))
    }

    fn p256(&self) -> Result<p256::ecdsa::VerifyingKey> {
        let CoseKey::Es256 { x, y } = self else {
            return Err(Error::Algorithm);
        };
        let mut sec1 = [0u8; 65];
        sec1[0] = 0x04;
        sec1[1..33].copy_from_slice(x);
        sec1[33..].copy_from_slice(y);
        p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
            .map_err(|_| Error::Malformed("P-256 public key"))
    }

    fn ed25519(&self) -> Result<ed25519_dalek::VerifyingKey> {
        let CoseKey::Ed25519(pk) = self else {
            return Err(Error::Algorithm);
        };
        ed25519_dalek::VerifyingKey::from_bytes(pk)
            .map_err(|_| Error::Malformed("Ed25519 public key"))
    }

    /// Verify a WebAuthn signature over `msg`: ASN.1 DER for ES256 (either
    /// form of `s`, as authenticators produce both), 64 bytes for EdDSA
    /// (strict verification), and for RS256 exactly as many bytes as the
    /// modulus.
    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> Result<()> {
        match self {
            CoseKey::Es256 { .. } => {
                use p256::ecdsa::signature::Verifier;
                let sig = p256::ecdsa::Signature::from_der(sig).map_err(|_| Error::Signature)?;
                self.p256()?.verify(msg, &sig).map_err(|_| Error::Signature)
            }
            CoseKey::Ed25519(_) => {
                let sig =
                    ed25519_dalek::Signature::from_slice(sig).map_err(|_| Error::Signature)?;
                self.ed25519()?
                    .verify_strict(msg, &sig)
                    .map_err(|_| Error::Signature)
            }
            CoseKey::Rs256 { n, .. } => {
                use rsa::signature::Verifier;
                if sig.len() != n.len() {
                    return Err(Error::Signature);
                }
                let sig = rsa::pkcs1v15::Signature::try_from(sig).map_err(|_| Error::Signature)?;
                rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(self.rsa()?)
                    .verify(msg, &sig)
                    .map_err(|_| Error::Signature)
            }
        }
    }
}

/// Decode one CBOR item from the front of `b`; the item and the rest.
fn read_cbor(b: &[u8]) -> Option<(Value, &[u8])> {
    let mut rest = b;
    let v: Value = ciborium::from_reader(&mut rest).ok()?;
    Some((v, rest))
}

// ---------------------------------------------------------------- authData

/// A credential as registered: `attestedCredentialData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedCredential {
    /// The authenticator model (AAGUID); zero for most passkey providers.
    pub aaguid: [u8; 16],
    /// The credential id.
    pub credential_id: Vec<u8>,
    /// The public key.
    pub public_key: CoseKey,
    /// The public key's COSE encoding, as the authenticator sent it.
    pub public_key_cose: Vec<u8>,
}

/// Parsed `authenticatorData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthData {
    /// SHA-256 of the relying-party id the authenticator scoped the
    /// credential to.
    pub rp_id_hash: [u8; 32],
    /// Flags.
    pub flags: u8,
    /// The signature counter.
    pub sign_count: u32,
    /// The attested credential, when `AT` is set.
    pub attested: Option<AttestedCredential>,
}

/// Parse `authenticatorData`: `rpIdHash(32) ‖ flags ‖ u32 signCount`, then
/// the attested credential when `AT` is set and an extension map when
/// `ED` is set, and nothing else.
pub fn parse_auth_data(b: &[u8]) -> Result<AuthData> {
    const BAD: Error = Error::Malformed("authenticator data");
    if b.len() < 37 {
        return Err(BAD);
    }
    let rp_id_hash: [u8; 32] = b[..32].try_into().expect("32");
    let flags = b[32];
    let sign_count = u32::from_be_bytes(b[33..37].try_into().expect("4"));
    let mut rest = &b[37..];
    let attested = if flags & FLAG_AT != 0 {
        if rest.len() < 18 {
            return Err(BAD);
        }
        let aaguid: [u8; 16] = rest[..16].try_into().expect("16");
        let n = u16::from_be_bytes([rest[16], rest[17]]) as usize;
        rest = &rest[18..];
        if n == 0 || n > MAX_CREDENTIAL_ID || rest.len() < n {
            return Err(Error::Malformed("credential id"));
        }
        let credential_id = rest[..n].to_vec();
        rest = &rest[n..];
        let (v, after) = read_cbor(rest).ok_or(Error::Malformed("COSE key"))?;
        let public_key_cose = rest[..rest.len() - after.len()].to_vec();
        rest = after;
        Some(AttestedCredential {
            aaguid,
            credential_id,
            public_key: CoseKey::from_value(&v)?,
            public_key_cose,
        })
    } else {
        None
    };
    if flags & FLAG_ED != 0 {
        let (v, after) = read_cbor(rest).ok_or(BAD)?;
        if v.as_map().is_none() {
            return Err(BAD);
        }
        rest = after;
    }
    if !rest.is_empty() {
        return Err(BAD);
    }
    Ok(AuthData {
        rp_id_hash,
        flags,
        sign_count,
        attested,
    })
}

/// The checks shared by registration and sign-in: `rpIdHash`, user
/// presence, user verification per policy, and the backup flags.
fn check_auth_data(ad: &AuthData, rp_id: &str, require_uv: bool) -> Result<()> {
    if ad.rp_id_hash != sha256(rp_id.as_bytes()) {
        return Err(Error::RpIdHash);
    }
    if ad.flags & FLAG_UP == 0 {
        return Err(Error::UserPresence);
    }
    if require_uv && ad.flags & FLAG_UV == 0 {
        return Err(Error::UserVerification);
    }
    // "Backed up" without "backup eligible" is invalid.
    if ad.flags & FLAG_BS != 0 && ad.flags & FLAG_BE == 0 {
        return Err(Error::Malformed("authenticator data flags"));
    }
    Ok(())
}

// ---------------------------------------------------------------- ceremonies

/// What a ceremony must match.
pub struct Expected<'a> {
    /// The challenge the server issued.
    pub challenge: &'a [u8],
    /// The relying-party id.
    pub rp_id: &'a str,
    /// Refuse an authenticator that didn't verify the user.
    pub require_uv: bool,
    /// The origin check (the server's origin policy).
    pub origin_ok: &'a dyn Fn(&str) -> bool,
}

/// A verified registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// The attestation format the client sent (not verified).
    pub fmt: String,
    /// The client data.
    pub client_data: ClientData,
    /// The authenticator data; `attested` is set.
    pub auth_data: AuthData,
}

impl Registration {
    /// The registered credential.
    pub fn credential(&self) -> &AttestedCredential {
        self.auth_data.attested.as_ref().expect("checked")
    }
}

/// Verify a registration (`navigator.credentials.create`): the
/// attestation object and the client data. The attestation statement is
/// not verified (module docs).
pub fn verify_registration(
    attestation_object: &[u8],
    client_data_json: &[u8],
    want: &Expected<'_>,
) -> Result<Registration> {
    let client_data = parse_client_data(client_data_json)?;
    check_client_data(&client_data, TYPE_CREATE, want.challenge, want.origin_ok)?;

    const BAD: Error = Error::Malformed("attestation object");
    let (v, rest) = read_cbor(attestation_object).ok_or(BAD)?;
    if !rest.is_empty() {
        return Err(BAD);
    }
    let map = v.as_map().ok_or(BAD)?;
    let field = |name: &str| {
        let mut found = map.iter().filter(|(k, _)| k.as_text() == Some(name));
        match (found.next(), found.next()) {
            (Some((_, v)), None) => Ok(v),
            _ => Err(BAD),
        }
    };
    let fmt = field("fmt")?.as_text().ok_or(BAD)?.to_owned();
    let att_stmt = field("attStmt")?.as_map().ok_or(BAD)?;
    let auth_data = parse_auth_data(field("authData")?.as_bytes().ok_or(BAD)?)?;
    // "none" carries an empty statement. Other formats are accepted
    // without verifying their statement: this server keeps no attestation
    // roots, so every passkey counts as unattested.
    if fmt == "none" && !att_stmt.is_empty() {
        return Err(Error::Malformed("attestation statement of fmt \"none\""));
    }
    check_auth_data(&auth_data, want.rp_id, want.require_uv)?;
    if auth_data.attested.is_none() {
        return Err(Error::Malformed("authenticator data without a credential"));
    }
    Ok(Registration {
        fmt,
        client_data,
        auth_data,
    })
}

/// A stored credential, as sign-in needs it.
pub struct Stored<'a> {
    /// The public key.
    pub key: &'a CoseKey,
    /// The relying-party id the credential was registered under.
    pub rp_id: &'a str,
    /// The last signature counter seen.
    pub sign_count: u32,
}

/// Verify an assertion (`navigator.credentials.get`) with a stored
/// credential. `want.rp_id` is ignored: the credential's own relying-party
/// id is what the authenticator signs. Returns the authenticator data, with
/// the new counter.
pub fn verify_assertion(
    cred: &Stored<'_>,
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature: &[u8],
    want: &Expected<'_>,
) -> Result<AuthData> {
    let client_data = parse_client_data(client_data_json)?;
    check_client_data(&client_data, TYPE_GET, want.challenge, want.origin_ok)?;
    let auth_data = parse_auth_data(authenticator_data)?;
    check_auth_data(&auth_data, cred.rp_id, want.require_uv)?;
    let mut msg = Vec::with_capacity(authenticator_data.len() + 32);
    msg.extend_from_slice(authenticator_data);
    msg.extend_from_slice(&sha256(client_data_json));
    cred.key.verify(&msg, signature)?;
    check_counter(cred.sign_count, auth_data.sign_count)?;
    Ok(auth_data)
}

/// The signature-counter rule: when either counter is non-zero, the new
/// one must be greater than the stored one. Authenticators that don't
/// count (most synced passkeys) always send 0.
pub fn check_counter(stored: u32, got: u32) -> Result<()> {
    if (stored != 0 || got != 0) && got <= stored {
        return Err(Error::Counter { stored, got });
    }
    Ok(())
}

// ---------------------------------------------------------------- tests

/// A software authenticator that builds real-format WebAuthn messages, for
/// tests.
#[cfg(any(test, feature = "test-utils"))]
pub mod soft {
    use super::*;
    use p256::ecdsa::signature::Signer as _;

    /// A key the software authenticator signs with.
    pub enum Key {
        /// ES256.
        P256(p256::ecdsa::SigningKey),
        /// EdDSA.
        Ed25519(ed25519_dalek::SigningKey),
        /// RS256.
        Rsa(Box<rsa::RsaPrivateKey>),
    }

    /// A deterministic RNG for RSA key generation (BLAKE3 in XOF mode).
    struct XofRng(blake3::OutputReader);

    impl rsa::rand_core::TryRng for XofRng {
        type Error = std::convert::Infallible;
        fn try_next_u32(&mut self) -> std::result::Result<u32, Self::Error> {
            let mut b = [0; 4];
            self.0.fill(&mut b);
            Ok(u32::from_le_bytes(b))
        }
        fn try_next_u64(&mut self) -> std::result::Result<u64, Self::Error> {
            let mut b = [0; 8];
            self.0.fill(&mut b);
            Ok(u64::from_le_bytes(b))
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> std::result::Result<(), Self::Error> {
            self.0.fill(dst);
            Ok(())
        }
    }

    impl rsa::rand_core::TryCryptoRng for XofRng {}

    /// An RSA key of `bits` from a seed byte. Generated once per process:
    /// key generation is slow.
    pub fn rsa_key(seed: u8, bits: usize) -> rsa::RsaPrivateKey {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        static KEYS: OnceLock<Mutex<HashMap<(u8, usize), rsa::RsaPrivateKey>>> = OnceLock::new();
        let keys = KEYS.get_or_init(Default::default);
        if let Some(k) = keys.lock().expect("keys").get(&(seed, bits)) {
            return k.clone();
        }
        let mut h = blake3::Hasher::new_derive_key("zen/test/webauthn-rsa");
        h.update(&[seed]);
        h.update(&(bits as u64).to_be_bytes());
        let k = rsa::RsaPrivateKey::new(&mut XofRng(h.finalize_xof()), bits).expect("RSA key");
        keys.lock().expect("keys").insert((seed, bits), k.clone());
        k
    }

    /// A software authenticator holding one credential.
    pub struct Authenticator {
        /// The signing key.
        pub key: Key,
        /// The credential id.
        pub credential_id: Vec<u8>,
        /// The signature counter; `None` for one that doesn't count.
        pub counter: Option<u32>,
        /// Flags to set on every message (`UP | UV` by default).
        pub flags: u8,
    }

    impl Authenticator {
        /// An ES256 credential from a seed byte.
        pub fn p256(seed: u8) -> Self {
            let mut b = [seed; 32];
            b[0] = 1;
            Authenticator {
                key: Key::P256(p256::ecdsa::SigningKey::from_slice(&b).expect("scalar")),
                credential_id: vec![seed; 20],
                counter: Some(0),
                flags: FLAG_UP | FLAG_UV,
            }
        }

        /// An EdDSA credential from a seed byte.
        pub fn ed25519(seed: u8) -> Self {
            Authenticator {
                key: Key::Ed25519(ed25519_dalek::SigningKey::from_bytes(&[seed; 32])),
                credential_id: vec![seed; 16],
                counter: None,
                flags: FLAG_UP | FLAG_UV | FLAG_BE | FLAG_BS,
            }
        }

        /// An RS256 credential (2048 bits) from a seed byte.
        pub fn rsa(seed: u8) -> Self {
            Authenticator {
                key: Key::Rsa(Box::new(rsa_key(seed, 2048))),
                credential_id: vec![seed; 24],
                counter: Some(0),
                flags: FLAG_UP | FLAG_UV,
            }
        }

        /// The COSE public key.
        pub fn cose_key(&self) -> Vec<u8> {
            let i = |n: i64| Value::Integer(n.into());
            let map = match &self.key {
                Key::P256(k) => {
                    let p = k.verifying_key().to_sec1_point(false);
                    let b = p.as_bytes();
                    vec![
                        (i(COSE_KTY), i(KTY_EC2)),
                        (i(COSE_ALG), i(ALG_ES256)),
                        (i(COSE_CRV), i(CRV_P256)),
                        (i(COSE_X), Value::Bytes(b[1..33].to_vec())),
                        (i(COSE_Y), Value::Bytes(b[33..65].to_vec())),
                    ]
                }
                Key::Ed25519(k) => vec![
                    (i(COSE_KTY), i(KTY_OKP)),
                    (i(COSE_ALG), i(ALG_EDDSA)),
                    (i(COSE_CRV), i(CRV_ED25519)),
                    (
                        i(COSE_X),
                        Value::Bytes(k.verifying_key().to_bytes().to_vec()),
                    ),
                ],
                Key::Rsa(k) => {
                    use rsa::traits::PublicKeyParts as _;
                    let strip = |b: Box<[u8]>| {
                        let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
                        b[i..].to_vec()
                    };
                    vec![
                        (i(COSE_KTY), i(KTY_RSA)),
                        (i(COSE_ALG), i(ALG_RS256)),
                        (i(COSE_RSA_N), Value::Bytes(strip(k.n().to_be_bytes()))),
                        (i(COSE_RSA_E), Value::Bytes(strip(k.e().to_be_bytes()))),
                    ]
                }
            };
            zen_proto::to_cbor(&Value::Map(map))
        }

        fn auth_data(&mut self, rp_id: &str, attested: bool) -> Vec<u8> {
            let count = match &mut self.counter {
                Some(c) => {
                    *c += 1;
                    *c
                }
                None => 0,
            };
            let mut ad = sha256(rp_id.as_bytes()).to_vec();
            ad.push(self.flags | if attested { FLAG_AT } else { 0 });
            ad.extend_from_slice(&count.to_be_bytes());
            if attested {
                ad.extend_from_slice(&[0xAA; 16]);
                ad.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
                ad.extend_from_slice(&self.credential_id);
                ad.extend_from_slice(&self.cose_key());
            }
            ad
        }

        /// `clientDataJSON` as a browser would write it.
        pub fn client_data(typ: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
            format!(
                r#"{{"type":"{typ}","challenge":"{}","origin":"{origin}","crossOrigin":false}}"#,
                URL_SAFE_NO_PAD.encode(challenge)
            )
            .into_bytes()
        }

        /// `navigator.credentials.create`: the attestation object (`fmt`
        /// `"none"`) and the client data.
        pub fn create(
            &mut self,
            rp_id: &str,
            challenge: &[u8],
            origin: &str,
        ) -> (Vec<u8>, Vec<u8>) {
            let ad = self.auth_data(rp_id, true);
            let att = Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("none".into())),
                (Value::Text("attStmt".into()), Value::Map(vec![])),
                (Value::Text("authData".into()), Value::Bytes(ad)),
            ]);
            (
                zen_proto::to_cbor(&att),
                Self::client_data(TYPE_CREATE, challenge, origin),
            )
        }

        /// Sign `authData ‖ SHA-256(clientDataJSON)`.
        pub fn sign(&self, auth_data: &[u8], client_data: &[u8]) -> Vec<u8> {
            let mut msg = auth_data.to_vec();
            msg.extend_from_slice(&sha256(client_data));
            match &self.key {
                Key::P256(k) => {
                    let s: p256::ecdsa::Signature = k.sign(&msg);
                    s.to_der().as_bytes().to_vec()
                }
                Key::Ed25519(k) => {
                    use ed25519_dalek::Signer as _;
                    k.sign(&msg).to_bytes().to_vec()
                }
                Key::Rsa(k) => {
                    use rsa::signature::SignatureEncoding as _;
                    let k = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new((**k).clone());
                    k.sign(&msg).to_vec()
                }
            }
        }

        /// `navigator.credentials.get`: authenticator data, client data and
        /// signature.
        pub fn get(
            &mut self,
            rp_id: &str,
            challenge: &[u8],
            origin: &str,
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let ad = self.auth_data(rp_id, false);
            let cd = Self::client_data(TYPE_GET, challenge, origin);
            let sig = self.sign(&ad, &cd);
            (ad, cd, sig)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::soft::Authenticator;
    use super::*;

    const RP: &str = "zen.example.org";
    const ORIGIN: &str = "https://zen.example.org";
    const CH: [u8; 32] = [7; 32];

    fn origin_ok(o: &str) -> bool {
        o == ORIGIN
    }

    fn want(challenge: &[u8]) -> Expected<'_> {
        Expected {
            challenge,
            rp_id: RP,
            require_uv: true,
            origin_ok: &origin_ok,
        }
    }

    fn register(a: &mut Authenticator) -> Registration {
        let (att, cd) = a.create(RP, &CH, ORIGIN);
        verify_registration(&att, &cd, &want(&CH)).unwrap()
    }

    fn att_object(fmt: &str, stmt: Vec<(Value, Value)>, ad: Vec<u8>) -> Vec<u8> {
        zen_proto::to_cbor(&Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text(fmt.into())),
            (Value::Text("attStmt".into()), Value::Map(stmt)),
            (Value::Text("authData".into()), Value::Bytes(ad)),
        ]))
    }

    fn auth_data_of(att: &[u8]) -> Vec<u8> {
        let (v, _) = read_cbor(att).unwrap();
        v.as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("authData"))
            .unwrap()
            .1
            .as_bytes()
            .unwrap()
            .clone()
    }

    #[test]
    fn register_and_sign_in_es256_eddsa_and_rs256() {
        for mut a in [
            Authenticator::p256(3),
            Authenticator::ed25519(4),
            Authenticator::rsa(5),
        ] {
            let r = register(&mut a);
            let c = r.credential();
            assert_eq!(c.credential_id, a.credential_id);
            assert_eq!(c.public_key_cose, a.cose_key());
            assert_eq!(CoseKey::decode(&c.public_key_cose).unwrap(), c.public_key);
            assert_eq!(r.fmt, "none");
            let mut stored = r.auth_data.sign_count;
            for _ in 0..3 {
                let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
                let s = Stored {
                    key: &c.public_key,
                    rp_id: RP,
                    sign_count: stored,
                };
                let got = verify_assertion(&s, &ad, &cd, &sig, &want(&CH)).unwrap();
                stored = got.sign_count;
            }
        }
    }

    #[test]
    fn high_s_ecdsa_signatures_verify() {
        let mut a = Authenticator::p256(5);
        let r = register(&mut a);
        let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
        let s = p256::ecdsa::Signature::from_der(&sig).unwrap();
        let low = s.normalize_s();
        let high = p256::ecdsa::Signature::from_scalars(low.r(), -*low.s()).unwrap();
        assert_ne!(high, low);
        let stored = Stored {
            key: &r.credential().public_key,
            rp_id: RP,
            sign_count: 0,
        };
        for s in [low, high] {
            let der = s.to_der().as_bytes().to_vec();
            verify_assertion(&stored, &ad, &cd, &der, &want(&CH)).unwrap();
        }
    }

    #[test]
    fn tampered_assertions_are_refused() {
        let mut a = Authenticator::p256(6);
        let r = register(&mut a);
        let key = r.credential().public_key.clone();
        let stored = Stored {
            key: &key,
            rp_id: RP,
            sign_count: 1,
        };
        let check = |ad: &[u8], cd: &[u8], sig: &[u8]| {
            verify_assertion(&stored, ad, cd, sig, &want(&CH)).unwrap_err()
        };
        let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
        verify_assertion(&stored, &ad, &cd, &sig, &want(&CH)).unwrap();

        // A flipped bit anywhere in the signed data or the signature.
        let mut bad = ad.clone();
        bad[36] ^= 1;
        assert_eq!(check(&bad, &cd, &sig), Error::Signature);
        let mut bad = sig.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(check(&ad, &cd, &bad), Error::Signature);
        let bad = String::from_utf8(cd.clone())
            .unwrap()
            .replace("\"crossOrigin\":false", "\"crossOrigin\":false ");
        assert_eq!(check(&ad, bad.as_bytes(), &sig), Error::Signature);
        // Another key's signature.
        let other = Authenticator::p256(9);
        assert_eq!(check(&ad, &cd, &other.sign(&ad, &cd)), Error::Signature);

        // Wrong type, challenge, origin, cross-origin.
        let resign = |cd: Vec<u8>| {
            let sig = a.sign(&ad, &cd);
            verify_assertion(&stored, &ad, &cd, &sig, &want(&CH)).unwrap_err()
        };
        assert_eq!(
            resign(Authenticator::client_data(TYPE_CREATE, &CH, ORIGIN)),
            Error::Type
        );
        assert_eq!(
            resign(Authenticator::client_data(TYPE_GET, &[8; 32], ORIGIN)),
            Error::Challenge
        );
        assert_eq!(
            resign(Authenticator::client_data(
                TYPE_GET,
                &CH,
                "https://evil.example"
            )),
            Error::Origin
        );
        let cross = String::from_utf8(Authenticator::client_data(TYPE_GET, &CH, ORIGIN))
            .unwrap()
            .replace("false", "true");
        assert_eq!(resign(cross.into_bytes()), Error::CrossOrigin);
        assert!(matches!(
            resign(b"{\"type\":\"webauthn.get\"}".to_vec()),
            Error::Malformed(_)
        ));
        // A duplicated member is refused, not resolved either way.
        let dup = format!(
            r#"{{"type":"webauthn.get","type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}"}}"#,
            URL_SAFE_NO_PAD.encode(CH)
        );
        assert!(matches!(resign(dup.into_bytes()), Error::Malformed(_)));
    }

    #[test]
    fn rp_id_hash_and_flags_are_checked() {
        let mut a = Authenticator::ed25519(7);
        let r = register(&mut a);
        let key = r.credential().public_key.clone();
        let stored = Stored {
            key: &key,
            rp_id: RP,
            sign_count: 0,
        };
        // Signed for another relying party.
        let (ad, cd, sig) = a.get("evil.example", &CH, ORIGIN);
        assert_eq!(
            verify_assertion(&stored, &ad, &cd, &sig, &want(&CH)).unwrap_err(),
            Error::RpIdHash
        );
        // No user presence; no user verification (refused only when
        // required); BS without BE.
        for (flags, require_uv, err) in [
            (FLAG_UV, true, Some(Error::UserPresence)),
            (FLAG_UP, true, Some(Error::UserVerification)),
            (FLAG_UP, false, None),
            (
                FLAG_UP | FLAG_UV | FLAG_BS,
                true,
                Some(Error::Malformed("authenticator data flags")),
            ),
        ] {
            a.flags = flags;
            let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
            let w = Expected {
                require_uv,
                ..want(&CH)
            };
            assert_eq!(
                verify_assertion(&stored, &ad, &cd, &sig, &w).err(),
                err,
                "{flags:#x}"
            );
        }
        // The same at registration.
        let mut a = Authenticator::p256(8);
        a.flags = FLAG_UV;
        let (att, cd) = a.create(RP, &CH, ORIGIN);
        assert_eq!(
            verify_registration(&att, &cd, &want(&CH)).unwrap_err(),
            Error::UserPresence
        );
        a.flags = FLAG_UP | FLAG_UV;
        let (att, cd) = a.create("evil.example", &CH, ORIGIN);
        assert_eq!(
            verify_registration(&att, &cd, &want(&CH)).unwrap_err(),
            Error::RpIdHash
        );
    }

    #[test]
    fn counter_rule() {
        assert!(check_counter(0, 0).is_ok());
        assert!(check_counter(0, 1).is_ok());
        assert!(check_counter(5, 6).is_ok());
        assert!(check_counter(5, 5).is_err());
        assert!(check_counter(5, 4).is_err());
        assert!(check_counter(5, 0).is_err());

        // A regressed counter in a validly signed assertion.
        let mut a = Authenticator::p256(10);
        let r = register(&mut a);
        let key = r.credential().public_key.clone();
        let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
        let stored = Stored {
            key: &key,
            rp_id: RP,
            sign_count: 10,
        };
        assert_eq!(
            verify_assertion(&stored, &ad, &cd, &sig, &want(&CH)).unwrap_err(),
            Error::Counter { stored: 10, got: 2 }
        );
    }

    #[test]
    fn registration_checks_client_data_and_attestation() {
        let mut a = Authenticator::p256(11);
        // Wrong type: an assertion's client data.
        let (att, _) = a.create(RP, &CH, ORIGIN);
        let cd = Authenticator::client_data(TYPE_GET, &CH, ORIGIN);
        assert_eq!(
            verify_registration(&att, &cd, &want(&CH)).unwrap_err(),
            Error::Type
        );
        let cd = Authenticator::client_data(TYPE_CREATE, &CH, "https://evil.example");
        assert_eq!(
            verify_registration(&att, &cd, &want(&CH)).unwrap_err(),
            Error::Origin
        );
        let (att, cd) = a.create(RP, &CH, ORIGIN);
        assert_eq!(
            verify_registration(&att, &cd, &want(&[0; 32])).unwrap_err(),
            Error::Challenge
        );

        // "none" with a statement is malformed; other formats' statements
        // are ignored.
        let ad = auth_data_of(&att);
        let stmt = vec![(Value::Text("sig".into()), Value::Bytes(vec![1]))];
        let bad = att_object("none", stmt.clone(), ad.clone());
        assert!(matches!(
            verify_registration(&bad, &cd, &want(&CH)).unwrap_err(),
            Error::Malformed(_)
        ));
        let packed = att_object("packed", stmt, ad.clone());
        assert_eq!(
            verify_registration(&packed, &cd, &want(&CH)).unwrap().fmt,
            "packed"
        );

        // No attested credential.
        let mut no_at = ad.clone();
        no_at[32] &= !FLAG_AT;
        no_at.truncate(37);
        let bad = att_object("none", vec![], no_at);
        assert!(matches!(
            verify_registration(&bad, &cd, &want(&CH)).unwrap_err(),
            Error::Malformed(_)
        ));
        // Trailing bytes after the authenticator data or the object.
        let mut long = ad.clone();
        long.push(0);
        let bad = att_object("none", vec![], long);
        assert!(verify_registration(&bad, &cd, &want(&CH)).is_err());
        let mut bad = att.clone();
        bad.push(0);
        assert!(verify_registration(&bad, &cd, &want(&CH)).is_err());
        // An extension map is allowed after the key.
        let mut ext = ad.clone();
        ext[32] |= FLAG_ED;
        ext.extend_from_slice(&zen_proto::to_cbor(&Value::Map(vec![(
            Value::Text("credProtect".into()),
            Value::Integer(2.into()),
        )])));
        let ok = att_object("none", vec![], ext);
        let r = verify_registration(&ok, &cd, &want(&CH)).unwrap();
        assert_eq!(r.credential().public_key_cose, a.cose_key());
        // Truncated anywhere.
        for n in [0, 10, 37, 54, 60, ad.len() - 1] {
            let bad = att_object("none", vec![], ad[..n].to_vec());
            assert!(verify_registration(&bad, &cd, &want(&CH)).is_err(), "{n}");
        }
    }

    /// A COSE RSA key with these components.
    fn rsa_cose(n: &[u8], e: &[u8]) -> Vec<u8> {
        let i = |n: i64| Value::Integer(n.into());
        zen_proto::to_cbor(&Value::Map(vec![
            (i(COSE_KTY), i(KTY_RSA)),
            (i(COSE_ALG), i(ALG_RS256)),
            (i(COSE_RSA_N), Value::Bytes(n.to_vec())),
            (i(COSE_RSA_E), Value::Bytes(e.to_vec())),
        ]))
    }

    /// An odd modulus-shaped number of exactly `bits` bits.
    fn modulus(bits: usize) -> Vec<u8> {
        let mut n = vec![0xA5; bits.div_ceil(8)];
        let top = bits % 8;
        n[0] = if top == 0 { 0xC5 } else { (1 << (top - 1)) | 1 };
        *n.last_mut().unwrap() |= 1;
        n
    }

    #[test]
    fn rs256_signatures_and_tampering() {
        let mut a = Authenticator::rsa(12);
        let r = register(&mut a);
        let c = r.credential();
        assert_eq!(c.public_key.alg(), ALG_RS256);
        let key = c.public_key.clone();
        let stored = Stored {
            key: &key,
            rp_id: RP,
            sign_count: 0,
        };
        let (ad, cd, sig) = a.get(RP, &CH, ORIGIN);
        assert_eq!(sig.len(), 256);
        verify_assertion(&stored, &ad, &cd, &sig, &want(&CH)).unwrap();
        let check = |ad: &[u8], cd: &[u8], sig: &[u8]| {
            verify_assertion(&stored, ad, cd, sig, &want(&CH)).unwrap_err()
        };
        // Tampered data, signature; a shorter or longer signature.
        let mut bad = ad.clone();
        bad[33] ^= 0x80;
        assert_eq!(check(&bad, &cd, &sig), Error::Signature);
        let mut bad = cd.clone();
        bad[2] ^= 1;
        assert!(verify_assertion(&stored, &ad, &bad, &sig, &want(&CH)).is_err());
        let mut bad = sig.clone();
        bad[100] ^= 1;
        assert_eq!(check(&ad, &cd, &bad), Error::Signature);
        assert_eq!(check(&ad, &cd, &sig[1..]), Error::Signature);
        let mut long = vec![0];
        long.extend_from_slice(&sig);
        assert_eq!(check(&ad, &cd, &long), Error::Signature);
        assert_eq!(check(&ad, &cd, &[]), Error::Signature);
        // Another RSA key's signature.
        let other = Authenticator::rsa(13);
        assert_eq!(check(&ad, &cd, &other.sign(&ad, &cd)), Error::Signature);
        // An ECDSA signature for an RSA key.
        let ec = Authenticator::p256(14);
        assert_eq!(check(&ad, &cd, &ec.sign(&ad, &cd)), Error::Signature);
    }

    #[test]
    fn rsa_key_policy() {
        let e = [1, 0, 1];
        // Modulus size: 2048 to 4096 bits, leading zero bytes ignored.
        for (bits, ok) in [
            (1024, false),
            (2047, false),
            (2048, true),
            (3072, true),
            (4096, true),
            (4097, false),
            (8192, false),
        ] {
            let r = CoseKey::decode(&rsa_cose(&modulus(bits), &e));
            assert_eq!(r.is_ok(), ok, "{bits} bits: {r:?}");
            if !ok {
                assert!(matches!(r.unwrap_err(), Error::KeyPolicy(_)), "{bits}");
            }
        }
        let mut padded = vec![0, 0];
        padded.extend(modulus(2048));
        assert!(CoseKey::decode(&rsa_cose(&padded, &e)).is_ok());
        let mut padded = vec![0];
        padded.extend(modulus(2040));
        assert!(CoseKey::decode(&rsa_cose(&padded, &e)).is_err());
        // An even modulus.
        let mut even = modulus(2048);
        *even.last_mut().unwrap() &= !1;
        assert!(CoseKey::decode(&rsa_cose(&even, &e)).is_err());
        // Exponent: odd, 65537 ..= 2^32 - 1.
        let n = modulus(2048);
        for (e, ok) in [
            (vec![1, 0, 1], true),
            (vec![0, 1, 0, 1], true),
            (vec![0xFF, 0xFF, 0xFF, 0xFF], true),
            (vec![3], false),
            (vec![0xFF, 0xFF], false),
            (vec![1, 0, 0], false),
            (vec![1, 0, 0, 0, 1], false),
            (vec![1; 9], false),
            (vec![], false),
            (vec![1], false),
        ] {
            let r = CoseKey::decode(&rsa_cose(&n, &e));
            assert_eq!(r.is_ok(), ok, "e = {e:02x?}: {r:?}");
        }
        // Missing or mistyped components.
        let i = |n: i64| Value::Integer(n.into());
        let no_e = zen_proto::to_cbor(&Value::Map(vec![
            (i(COSE_KTY), i(KTY_RSA)),
            (i(COSE_ALG), i(ALG_RS256)),
            (i(COSE_RSA_N), Value::Bytes(n.clone())),
        ]));
        assert!(matches!(
            CoseKey::decode(&no_e).unwrap_err(),
            Error::Malformed(_)
        ));
        let int_e = zen_proto::to_cbor(&Value::Map(vec![
            (i(COSE_KTY), i(KTY_RSA)),
            (i(COSE_ALG), i(ALG_RS256)),
            (i(COSE_RSA_N), Value::Bytes(n)),
            (i(COSE_RSA_E), i(65537)),
        ]));
        assert!(matches!(
            CoseKey::decode(&int_e).unwrap_err(),
            Error::Malformed(_)
        ));
        // A real key round-trips.
        let a = Authenticator::rsa(5);
        assert_eq!(CoseKey::decode(&a.cose_key()).unwrap().alg(), ALG_RS256);
    }

    #[test]
    fn unsupported_and_bad_keys_are_refused() {
        let i = |n: i64| Value::Integer(n.into());
        let key = |pairs: Vec<(i64, Value)>| {
            zen_proto::to_cbor(&Value::Map(
                pairs.into_iter().map(|(k, v)| (i(k), v)).collect(),
            ))
        };
        // RSA with another algorithm: PS256 (-37), RS512 (-259).
        for alg in [-37, -259] {
            let rsa = key(vec![
                (COSE_KTY, i(KTY_RSA)),
                (COSE_ALG, i(alg)),
                (COSE_RSA_N, Value::Bytes(vec![0xC1; 256])),
                (COSE_RSA_E, Value::Bytes(vec![1, 0, 1])),
            ]);
            assert_eq!(CoseKey::decode(&rsa).unwrap_err(), Error::Algorithm);
        }
        // ES256 on another curve, a mismatched algorithm, a point off the
        // curve, a short coordinate, a duplicate label.
        let ec = |crv: i64, alg: i64, x: Vec<u8>, y: Vec<u8>| {
            key(vec![
                (COSE_KTY, i(KTY_EC2)),
                (COSE_ALG, i(alg)),
                (COSE_CRV, i(crv)),
                (COSE_X, Value::Bytes(x)),
                (COSE_Y, Value::Bytes(y)),
            ])
        };
        assert_eq!(
            CoseKey::decode(&ec(2, ALG_ES256, vec![1; 48], vec![1; 48])).unwrap_err(),
            Error::Algorithm
        );
        assert_eq!(
            CoseKey::decode(&ec(CRV_P256, ALG_EDDSA, vec![1; 32], vec![1; 32])).unwrap_err(),
            Error::Algorithm
        );
        assert!(matches!(
            CoseKey::decode(&ec(CRV_P256, ALG_ES256, vec![1; 32], vec![1; 32])).unwrap_err(),
            Error::Malformed(_)
        ));
        assert!(matches!(
            CoseKey::decode(&ec(CRV_P256, ALG_ES256, vec![1; 31], vec![1; 32])).unwrap_err(),
            Error::Malformed(_)
        ));
        let good = Authenticator::ed25519(1).cose_key();
        assert!(CoseKey::decode(&good).is_ok());
        let (v, _) = read_cbor(&good).unwrap();
        let mut m = v.as_map().unwrap().clone();
        m.push((i(COSE_ALG), i(ALG_EDDSA)));
        assert!(matches!(
            CoseKey::decode(&zen_proto::to_cbor(&Value::Map(m))).unwrap_err(),
            Error::Malformed(_)
        ));
        assert!(CoseKey::decode(b"\xa0").is_err());
        assert!(CoseKey::decode(b"").is_err());
    }
}
