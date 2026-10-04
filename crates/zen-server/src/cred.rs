//! The credential store (auth.md §4): the credentials of the sign-in
//! methods that don't live in the signed ACL, the login-name index, and
//! the `/v1/auth/credentials/*` endpoints.
//!
//! The signed ACL stays the source of truth for membership: a credential
//! only works while its user is a member, and an ACL version that removes
//! a member deletes their credentials.

use crate::acl::Fp;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::random32;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use zen_proto::{
    AuthMethod, ByteBuf, Credential, CredentialId, Credentials, CredentialsList, Empty, from_cbor,
    to_cbor,
};
use zen_store::Txn;
use zen_store::tuple::{Elem, unpack_prefix};

/// Max credentials one user may hold in the store.
pub const MAX_PER_USER: usize = 100;

/// A stored credential (keyspace.md §3.7): CBOR, with optional fields per
/// method. Fields a server doesn't know are ignored, so methods can add
/// their own without breaking older readers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CredRecord {
    /// Method id ([`AuthMethod::id`]).
    pub method: u8,
    /// Creation time, unix seconds.
    pub created_unix: u64,
    /// Expiry, unix seconds (API tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<u64>,
    /// A label chosen at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Hash of the normalized login name (`password_key`, later `opaque`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hash: Option<ByteBuf>,
    /// Argon2id salt (`password_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<ByteBuf>,
    /// Argon2id parameters (`password_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<zen_proto::Argon2Params>,
    /// Public identity, formats.md §7.2 (`password_key`). Never sent to
    /// clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ByteBuf>,
    /// The admin who issued it (API tokens) or bound it to another member
    /// (`mtls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<ByteBuf>,
    /// The WebAuthn credential id (`passkey`); the store id is its hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webauthn_id: Option<ByteBuf>,
    /// The credential public key, COSE (`passkey`). Never sent to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cose_key: Option<ByteBuf>,
    /// The COSE algorithm of `cose_key` (`passkey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<i64>,
    /// The last signature counter seen (`passkey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_count: Option<u32>,
    /// The relying-party id it was registered under (`passkey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rp_id: Option<String>,
    /// The last sign-in with it, unix seconds (`passkey`, `mtls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_unix: Option<u64>,
}

impl CredRecord {
    /// The method, if this server knows it.
    pub fn method(&self) -> Option<AuthMethod> {
        AuthMethod::from_id(self.method)
    }

    /// The login-name hash, if the method has a login name.
    pub fn name_hash(&self) -> Option<[u8; 32]> {
        self.name_hash
            .as_deref()
            .and_then(|h| h.as_slice().try_into().ok())
    }

    fn summary(&self, id: &Fp) -> Credential {
        Credential {
            id: id.to_vec(),
            method: self
                .method()
                .map_or_else(|| format!("unknown:{}", self.method), |m| m.name().into()),
            created_unix: self.created_unix,
            expires_unix: self.expires_unix,
            label: self.label.clone(),
            last_used_unix: self.last_used_unix,
        }
    }
}

/// `H(normalized login name)` (auth.md §4.2): the store keeps no names.
pub fn name_hash(normalized: &str) -> [u8; 32] {
    blake3::derive_key("zen-serve 2026 login name", normalized.as_bytes())
}

/// 400 unless `name` normalizes to a login name; its hash.
pub fn login_hash(name: &str) -> ApiResult<[u8; 32]> {
    let n = zen_proto::normalize_login(name).ok_or_else(|| {
        bad_request(
            "a login name is 1–128 characters of a-z, 0-9 and . _ - @ + (letters in any case)",
        )
    })?;
    Ok(name_hash(&n))
}

/// Read one credential.
pub async fn get(t: &mut Box<dyn Txn>, user: &Fp, id: &Fp) -> ApiResult<Option<CredRecord>> {
    match t.get(&keys::cred(user, id)).await? {
        Some(v) => Ok(Some(from_cbor(&v).map_err(internal)?)),
        None => Ok(None),
    }
}

/// The owner of credential `id`.
pub async fn owner(t: &mut Box<dyn Txn>, id: &Fp) -> ApiResult<Option<Fp>> {
    Ok(t.get(&keys::cred_owner(id))
        .await?
        .and_then(|v| v.try_into().ok()))
}

/// The methods whose credentials have a login name (auth.md §4.2).
pub const LOGIN_METHODS: [AuthMethod; 2] = [AuthMethod::PasswordKey, AuthMethod::Opaque];

/// Who holds a login name for `method`: `(user, credential id)`.
pub async fn login(
    t: &mut Box<dyn Txn>,
    method: AuthMethod,
    name_hash: &[u8; 32],
) -> ApiResult<Option<(Fp, Fp)>> {
    Ok(t.get(&keys::login(method.id(), name_hash))
        .await?
        .and_then(|v| {
            (v.len() == 64).then(|| {
                (
                    v[..32].try_into().expect("32"),
                    v[32..].try_into().expect("32"),
                )
            })
        }))
}

/// The user who holds a login name, for any method: a name belongs to
/// one user across methods (auth.md §4.2).
pub async fn name_holder(t: &mut Box<dyn Txn>, name_hash: &[u8; 32]) -> ApiResult<Option<Fp>> {
    for m in LOGIN_METHODS {
        if let Some((user, _)) = login(t, m, name_hash).await? {
            return Ok(Some(user));
        }
    }
    Ok(None)
}

/// Every credential of `user`, by id.
pub async fn list(t: &mut Box<dyn Txn>, user: &Fp) -> ApiResult<Vec<(Fp, CredRecord)>> {
    let p = keys::creds_of(user);
    let got = t
        .get_range(&p, &keys::end_of(&p), MAX_PER_USER + 1, false)
        .await?;
    got.into_iter()
        .map(|(k, v)| {
            let id = match unpack_prefix(&k[p.len()..], 1) {
                Ok((e, _)) => match e.into_iter().next() {
                    Some(Elem::Bytes(b)) => <Fp>::try_from(b.as_slice()).ok(),
                    _ => None,
                },
                Err(_) => None,
            }
            .ok_or_else(|| internal("bad credential key"))?;
            Ok((id, from_cbor(&v).map_err(internal)?))
        })
        .collect()
}

/// Store a credential: the record, its owner entry and, for a method with
/// a login name, the name index. The caller has checked that the name is
/// free (or its own).
pub fn put(t: &mut Box<dyn Txn>, user: &Fp, id: &Fp, rec: &CredRecord) {
    t.set(&keys::cred(user, id), &to_cbor(rec));
    t.set(&keys::cred_owner(id), user);
    if let Some(h) = rec.name_hash() {
        let mut v = user.to_vec();
        v.extend_from_slice(id);
        t.set(&keys::login(rec.method, &h), &v);
    }
}

/// Delete a credential, with its owner and name-index entries.
pub async fn delete(t: &mut Box<dyn Txn>, user: &Fp, id: &Fp, rec: &CredRecord) -> ApiResult<()> {
    t.clear(&keys::cred(user, id));
    t.clear(&keys::cred_owner(id));
    if let (Some(h), Some(m)) = (rec.name_hash(), rec.method()) {
        // The name may already point at a newer credential.
        if login(t, m, &h).await? == Some((*user, *id)) {
            t.clear(&keys::login(rec.method, &h));
        }
    }
    Ok(())
}

/// Delete every credential of `user` (the member left the ACL). Returns
/// their ids.
pub async fn delete_user(t: &mut Box<dyn Txn>, user: &Fp) -> ApiResult<Vec<Fp>> {
    let creds = list(t, user).await?;
    for (id, rec) in &creds {
        delete(t, user, id, rec).await?;
    }
    Ok(creds.into_iter().map(|(id, _)| id).collect())
}

/// A fresh credential id.
pub fn new_id() -> Fp {
    random32()
}

/// Stop this node's cached sessions of credential `id` at once; other
/// nodes drop them within their session cache time.
pub fn evict(st: &Shared, user: &Fp, id: &Fp) {
    st.sessions
        .lock()
        .expect("session lock")
        .retain(|_, c| !(c.info.user == *user && c.info.cred == *id));
}

/// The key of the fake sign-in parameters (auth.md §4.2), read or created
/// on first use. It is data, so it is created only once the cluster is
/// claimed: a server that was only started stays empty for `import`. Before
/// the claim no account exists, and a per-process key does.
pub async fn params_key(st: &Shared) -> ApiResult<[u8; 32]> {
    if let Some(k) = *st.params_key.lock().expect("params key lock") {
        return Ok(k);
    }
    if st.acl().version == 0 {
        return Ok(*UNCLAIMED_KEY.get_or_init(random32));
    }
    let key = keys::params_key();
    let (k, _) = txn_loop!(st.store, None, |t| {
        Ok(match t.get(&key).await? {
            Some(v) if v.len() == 32 => v.try_into().expect("32"),
            _ => {
                let k = random32();
                t.set(&key, &k);
                k
            }
        })
    })?;
    *st.params_key.lock().expect("params key lock") = Some(k);
    Ok(k)
}

static UNCLAIMED_KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

fn target_user(caller: &Caller, user: Option<&[u8]>) -> ApiResult<Fp> {
    match user {
        None => Ok(caller.user),
        Some(u) => {
            let u: Fp = u
                .try_into()
                .map_err(|_| bad_request("user must be 32 bytes"))?;
            if u != caller.user {
                caller.require_admin()?;
            }
            Ok(u)
        }
    }
}

/// `POST /v1/auth/credentials/list`: the caller's own, or as an admin any
/// member's. Metadata only.
pub async fn list_endpoint(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CredentialsList>,
) -> ApiResult<Cbor<Credentials>> {
    caller.require_interactive()?;
    let user = target_user(&caller, req.user.as_deref())?;
    let (creds, _) = txn_loop!(st.store, None, |t| { list(&mut t, &user).await })?;
    Ok(Cbor(Credentials {
        credentials: creds.iter().map(|(id, r)| r.summary(id)).collect(),
    }))
}

/// `POST /v1/auth/credentials/remove`: the caller's own, or as an admin
/// any member's. Ends the sessions it created.
pub async fn remove_endpoint(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CredentialId>,
) -> ApiResult<Cbor<Empty>> {
    caller.require_interactive()?;
    let id: Fp = req
        .id
        .as_slice()
        .try_into()
        .map_err(|_| bad_request("id must be 32 bytes"))?;
    let admin = caller.is_admin();
    let (user, _) = txn_loop!(st.store, None, |t| {
        let none = || not_found("no such credential");
        let user = owner(&mut t, &id).await?.ok_or_else(none)?;
        // Another member's credential is reported as missing, not
        // forbidden: ids don't reveal whose they are.
        if user != caller.user && !admin {
            return Err(none());
        }
        let rec = get(&mut t, &user, &id).await?.ok_or_else(none)?;
        delete(&mut t, &user, &id, &rec).await?;
        Ok(user)
    })?;
    evict(&st, &user, &id);
    tracing::info!(id = %hex(&id), "credential removed");
    Ok(Cbor(Empty {}))
}

/// Lowercase hex, for logs.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
