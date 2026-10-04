//! OPAQUE, sign-in method 3 (spec/auth.md §8): the cipher suite that
//! clients and the server share, and the client side of registration and
//! sign-in. Feature `opaque`.
//!
//! The suite is RFC 9807's `ristretto255-SHA512` configuration with
//! Argon2id as the key stretching function:
//! * OPRF: ristretto255-SHA512 (RFC 9497);
//! * AKE: 3DH over ristretto255 with SHA-512 (RFC 9807 §6.4);
//! * KSF: Argon2id v1.3 with a salt of 16 zero bytes and a 64-byte output,
//!   with the parameters stored for the credential ([`Argon2Params`],
//!   the floors and ceilings of formats.md §6).
//!
//! The AKE context is `"zen/v1/opaque" ‖ 0x00 ‖ origin` ([`context`]): a
//! sign-in completes only if the client and the server use the same
//! origin, the one the client sees (auth.md §8.4).
//!
//! A registration or sign-in gives the client a 64-byte **export key**,
//! which the server never sees. It can open an OPAQUE keyslot
//! ([`crate::keyslot::create_opaque_export`], formats.md §6, type 5).

pub use opaque_ke;

use crate::keyslot::{Argon2Params, OPAQUE_EXPORT_KEY_LEN};
use crate::labels;
use crate::rng::Rng;
use crate::{Error, Result};
use opaque_ke::errors::{InternalError, ProtocolError};
use opaque_ke::generic_array::{ArrayLength, GenericArray};
use opaque_ke::{
    CipherSuite, ClientLoginFinishParameters, ClientRegistrationFinishParameters,
    CredentialResponse, Identifiers, RegistrationResponse, Ristretto255, TripleDh,
};
use zeroize::Zeroizing;

/// The cipher suite: ristretto255, 3DH with SHA-512, Argon2id.
pub struct Suite;

impl CipherSuite for Suite {
    type OprfCs = Ristretto255;
    type KeyExchange = TripleDh<Ristretto255, sha2::Sha512>;
    type Ksf = Ksf;
}

/// Bytes of a `RegistrationRequest`: the blinded element.
pub const REGISTRATION_REQUEST_LEN: usize = 32;
/// Bytes of a `RegistrationResponse`: the evaluated element and the
/// server's public key.
pub const REGISTRATION_RESPONSE_LEN: usize = 64;
/// Bytes of a `RegistrationUpload`, the stored record: the client's
/// public key, the masking key and the envelope (nonce and MAC).
pub const REGISTRATION_UPLOAD_LEN: usize = 192;
/// Bytes of a `CredentialRequest` (KE1).
pub const CREDENTIAL_REQUEST_LEN: usize = 96;
/// Bytes of a `CredentialResponse` (KE2).
pub const CREDENTIAL_RESPONSE_LEN: usize = 320;
/// Bytes of a `CredentialFinalization` (KE3): the client's MAC.
pub const CREDENTIAL_FINALIZATION_LEN: usize = 64;
/// Bytes of the server's static public key.
pub const SERVER_PUBLIC_KEY_LEN: usize = 32;

/// An export key, wiped on drop.
pub type ExportKey = Zeroizing<[u8; OPAQUE_EXPORT_KEY_LEN]>;

/// The key stretching function: Argon2id with the credential's parameters.
/// The default, which the protocol code never uses, is the browser
/// recommendation of formats.md §6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ksf(pub Argon2Params);

impl Default for Ksf {
    fn default() -> Self {
        Ksf(Argon2Params::BROWSER)
    }
}

impl opaque_ke::ksf::Ksf for Ksf {
    fn hash<L: ArrayLength<u8>>(
        &self,
        input: GenericArray<u8, L>,
    ) -> core::result::Result<GenericArray<u8, L>, InternalError> {
        let p = self.0;
        let params = argon2::Params::new(p.m_cost_kib, p.t_cost, p.p_cost, Some(L::USIZE))
            .map_err(|_| InternalError::KsfError)?;
        let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out = GenericArray::default();
        a.hash_password_into(&input, &[0; 16], &mut out)
            .map_err(|_| InternalError::KsfError)?;
        Ok(out)
    }
}

/// The AKE context of a sign-in: `"zen/v1/opaque" ‖ 0x00 ‖ origin`, the
/// origin as the client sees it (spec/auth.md §5).
pub fn context(origin: &str) -> Vec<u8> {
    let mut c = labels::OPAQUE_CONTEXT.as_bytes().to_vec();
    c.push(0);
    c.extend_from_slice(origin.as_bytes());
    c
}

/// A [`Rng`] as the `rand_core` 0.6 generator OPAQUE takes. Panics if the
/// underlying generator fails, as `rand`'s `OsRng` does.
pub struct RandCore<'a>(pub &'a mut dyn Rng);

impl rand_core::RngCore for RandCore<'_> {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill(dest).expect("random number generator failed");
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), rand_core::Error> {
        self.0.fill(dest).map_err(|_| {
            rand_core::Error::from(
                core::num::NonZeroU32::new(rand_core::Error::CUSTOM_START).expect("non-zero"),
            )
        })
    }
}

impl rand_core::CryptoRng for RandCore<'_> {}

fn protocol(e: ProtocolError) -> Error {
    match e {
        ProtocolError::InvalidLoginError | ProtocolError::ReflectedValueError => Error::Decrypt,
        ProtocolError::LibraryError(InternalError::KsfError) => Error::Param,
        _ => Error::Format,
    }
}

/// `bytes`, if exactly `len` long: protocol messages have fixed sizes.
pub fn exact(bytes: &[u8], len: usize) -> Result<&[u8]> {
    if bytes.len() == len {
        Ok(bytes)
    } else {
        Err(Error::Format)
    }
}

fn export(key: &[u8]) -> ExportKey {
    let mut k = Zeroizing::new([0; OPAQUE_EXPORT_KEY_LEN]);
    k.copy_from_slice(key);
    k
}

/// What a finished registration or sign-in gives the client.
pub struct Finished {
    /// The message for the server: the `RegistrationUpload` of a
    /// registration, the `CredentialFinalization` of a sign-in.
    pub message: Vec<u8>,
    /// The export key (formats.md §6, type 5).
    pub export_key: ExportKey,
    /// The server's static public key. A client can remember it at
    /// registration and compare it at sign-in.
    pub server_public_key: [u8; SERVER_PUBLIC_KEY_LEN],
}

/// The client side of a registration (a password set or change).
pub struct Registration(opaque_ke::ClientRegistration<Suite>);

impl Registration {
    /// Blind the password. Returns the state and the `RegistrationRequest`.
    pub fn start(password: &[u8], rng: &mut dyn Rng) -> Result<(Self, Vec<u8>)> {
        let r = opaque_ke::ClientRegistration::<Suite>::start(&mut RandCore(rng), password)
            .map_err(protocol)?;
        Ok((Registration(r.state), r.message.serialize().to_vec()))
    }

    /// Finish with the server's `RegistrationResponse` and the Argon2id
    /// parameters to register with, which must meet the registration
    /// floors (formats.md §6).
    pub fn finish(
        self,
        password: &[u8],
        response: &[u8],
        params: Argon2Params,
        rng: &mut dyn Rng,
    ) -> Result<Finished> {
        params.validate_for_create()?;
        let response =
            RegistrationResponse::deserialize(exact(response, REGISTRATION_RESPONSE_LEN)?)
                .map_err(protocol)?;
        let ksf = Ksf(params);
        let r = self
            .0
            .finish(
                &mut RandCore(rng),
                password,
                response,
                ClientRegistrationFinishParameters::new(Identifiers::default(), Some(&ksf)),
            )
            .map_err(protocol)?;
        Ok(Finished {
            message: r.message.serialize().to_vec(),
            export_key: export(&r.export_key),
            server_public_key: r.server_s_pk.serialize().into(),
        })
    }
}

/// The client side of a sign-in.
pub struct Login(opaque_ke::ClientLogin<Suite>);

impl Login {
    /// Blind the password and start the AKE. Returns the state and the
    /// `CredentialRequest` (KE1).
    pub fn start(password: &[u8], rng: &mut dyn Rng) -> Result<(Self, Vec<u8>)> {
        let r = opaque_ke::ClientLogin::<Suite>::start(&mut RandCore(rng), password)
            .map_err(protocol)?;
        Ok((Login(r.state), r.message.serialize().to_vec()))
    }

    /// Finish with the server's `CredentialResponse` (KE2), the Argon2id
    /// parameters it returned, and the origin the client sees. Only the
    /// opening ceilings apply to the parameters: they come from the server,
    /// and lower ones only weaken the user's own record.
    ///
    /// A wrong password, an unknown login name (the server answered with a
    /// fake record) and a server that used another origin all fail alike,
    /// with [`Error::Decrypt`]; the client then has nothing to send.
    pub fn finish(
        self,
        password: &[u8],
        response: &[u8],
        params: Argon2Params,
        origin: &str,
        rng: &mut dyn Rng,
    ) -> Result<Finished> {
        params.validate_for_open()?;
        let response = CredentialResponse::deserialize(exact(response, CREDENTIAL_RESPONSE_LEN)?)
            .map_err(protocol)?;
        let ksf = Ksf(params);
        let ctx = context(origin);
        let r = self
            .0
            .finish(
                &mut RandCore(rng),
                password,
                response,
                ClientLoginFinishParameters::new(Some(&ctx), Identifiers::default(), Some(&ksf)),
            )
            .map_err(protocol)?;
        Ok(Finished {
            message: r.message.serialize().to_vec(),
            export_key: export(&r.export_key),
            server_public_key: r.server_s_pk.serialize().into(),
        })
    }
}
