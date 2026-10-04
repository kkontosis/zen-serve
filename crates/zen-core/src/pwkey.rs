//! Password-derived signing keys: sign-in method 6 (spec/formats.md §7.5,
//! spec/auth.md §11).
//!
//! ```text
//! root     = Argon2id(password, salt[32], m_cost_kib, t_cost, p_cost, out = 32)
//! seed     = KDF("zen/v1/password-sig", root, "")
//! identity = the hybrid identity of `seed` (formats.md §7.1)
//! ```
//!
//! To sign in, the client signs `lp(challenge) || lp(origin)` with purpose
//! `zen/v1/sig/password-session`. The password never leaves the client: the
//! server stores the salt, the Argon2id parameters and the public identity.

use crate::Result;
use crate::encoding::lp;
use crate::kdf::{Key32, kdf};
use crate::keyslot::Argon2Params;
use crate::labels;
use crate::rng::{self, Rng};
use crate::sig::{PublicIdentity, SigningIdentity};

/// Salt length.
pub const SALT_LEN: usize = 32;

/// A password-derived hybrid signing key.
pub struct PasswordKey {
    id: SigningIdentity,
}

impl PasswordKey {
    /// Registration: a fresh salt, and the creation floors of the Argon2id
    /// parameters (formats.md §6). Returns the key and the salt to store.
    pub fn create(
        password: &[u8],
        params: Argon2Params,
        rng: &mut dyn Rng,
    ) -> Result<(Self, [u8; SALT_LEN])> {
        params.validate_for_create()?;
        let salt: [u8; SALT_LEN] = rng::array(rng)?;
        Ok((Self::from_root(&*root(password, &salt, params)?), salt))
    }

    /// Sign-in: derive from the salt and parameters the server returned.
    /// Only the ceilings apply, as when opening a passphrase keyslot: the
    /// parameters are untrusted, and lower ones only weaken the user's own
    /// key.
    pub fn derive(password: &[u8], salt: &[u8; SALT_LEN], params: Argon2Params) -> Result<Self> {
        params.validate_for_open()?;
        Ok(Self::from_root(&*root(password, salt, params)?))
    }

    fn from_root(root: &[u8; 32]) -> Self {
        PasswordKey {
            id: SigningIdentity::from_seed(&seed(root)),
        }
    }

    /// The public identity the server stores.
    pub fn public(&self) -> PublicIdentity {
        self.id.public()
    }

    /// The sign-in signature over `challenge` and `origin`.
    pub fn sign_session(&self, challenge: &[u8], origin: &str) -> Result<Vec<u8>> {
        self.id.sign(
            labels::SIG_PASSWORD_SESSION,
            &session_message(challenge, origin),
        )
    }
}

/// `Argon2id(password, salt, params)`, 32 bytes. No parameter checks.
pub(crate) fn root(password: &[u8], salt: &[u8; SALT_LEN], params: Argon2Params) -> Result<Key32> {
    params.derive(password, salt)
}

/// `KDF("zen/v1/password-sig", root, "")`: the identity seed.
pub(crate) fn seed(root: &[u8; 32]) -> Key32 {
    kdf(labels::PASSWORD_SIG, root, &[])
}

/// The signed message: `lp(challenge) || lp(origin)` (formats.md §10).
pub fn session_message(challenge: &[u8], origin: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + challenge.len() + origin.len());
    lp(&mut m, challenge);
    lp(&mut m, origin.as_bytes());
    m
}

/// Verify a sign-in signature (server side).
pub fn verify_session(
    public: &PublicIdentity,
    challenge: &[u8],
    origin: &str,
    signature: &[u8],
) -> Result<()> {
    public.verify(
        labels::SIG_PASSWORD_SESSION,
        &session_message(challenge, origin),
        signature,
    )
}
