//! Sign-in method 6, the password-derived key (auth.md §11): parameter
//! lookup, sign-in, registration, and the failed-attempt limiter.
//!
//! The client derives a hybrid key from the password (formats.md §7.5) and
//! signs `challenge ‖ origin`. The server stores the salt, the Argon2id
//! parameters and the public identity in the credential store, and never
//! sees the password.

use crate::acl::Fp;
use crate::auth::{Caller, Signed, check_challenge, issue_session};
use crate::cbor::Cbor;
use crate::cred::{self, CredRecord};
use crate::error::*;
use crate::ids::unix_now;
use crate::keys;
use crate::origin::SignedOrigin;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zen_core::keyslot::Argon2Params as CoreParams;
use zen_core::pwkey;
use zen_core::sig::PublicIdentity;
use zen_proto::{
    Argon2Params, AuthMethod, ByteBuf, CredentialId, PasswordParams, PasswordParamsRequest,
    PasswordSessionRequest, PasswordSet, Session,
};

const METHOD: AuthMethod = AuthMethod::PasswordKey;

fn core(p: Argon2Params) -> CoreParams {
    CoreParams {
        m_cost_kib: p.m_cost_kib,
        t_cost: p.t_cost,
        p_cost: p.p_cost,
    }
}

/// 400 unless `p` meets the registration floors and ceilings
/// (formats.md §7.5).
pub fn check_params(p: Argon2Params) -> Result<(), String> {
    core(p).validate_for_create().map_err(|_| {
        format!(
            "Argon2id parameters need m_cost_kib {}..={}, t_cost 1..={}, p_cost 1..={}",
            CoreParams::MIN_M_COST_KIB,
            CoreParams::MAX_M_COST_KIB,
            CoreParams::MAX_T_COST,
            CoreParams::MAX_P_COST
        )
    })
}

/// The parameters for an unknown login name: the configured ones, and a
/// salt derived from the name with the server's key, so repeated lookups
/// agree and look like a real account's (auth.md §4.2).
async fn fake_params(st: &Shared, name_hash: &[u8; 32]) -> ApiResult<PasswordParams> {
    let mut h = blake3::Hasher::new_keyed(&cred::params_key(st).await?);
    h.update(b"zen-serve fake password salt\0");
    h.update(name_hash);
    let p = st.cfg.auth.password_params();
    Ok(PasswordParams {
        salt: h.finalize().as_bytes().to_vec(),
        m_cost_kib: p.m_cost_kib,
        t_cost: p.t_cost,
        p_cost: p.p_cost,
    })
}

/// The user and password credential behind a login name, if any.
async fn lookup(st: &Shared, name_hash: &[u8; 32]) -> ApiResult<Option<(Fp, Fp, CredRecord)>> {
    let (found, _) = txn_loop!(st.store, None, |t| {
        Ok(match cred::login(&mut t, METHOD, name_hash).await? {
            Some((user, id)) => cred::get(&mut t, &user, &id)
                .await?
                .filter(|r| r.method() == Some(METHOD))
                .map(|r| (user, id, r)),
            None => {
                // The same number of reads as a known name.
                let _ = t.get(&keys::cred(&[0; 32], name_hash)).await?;
                None
            }
        })
    })?;
    Ok(found)
}

/// `POST /v1/auth/password/params`: salt and parameters for a login name.
/// Unknown names get stable fakes, so the answer doesn't tell which
/// accounts exist.
pub async fn params(
    State(st): State<Shared>,
    Cbor(req): Cbor<PasswordParamsRequest>,
) -> ApiResult<Cbor<PasswordParams>> {
    st.require_method(METHOD)?;
    let h = cred::login_hash(&req.name)?;
    Ok(Cbor(match lookup(&st, &h).await? {
        Some((_, _, r)) => match (r.salt, r.params) {
            (Some(salt), Some(p)) => PasswordParams {
                salt: salt.into_vec(),
                m_cost_kib: p.m_cost_kib,
                t_cost: p.t_cost,
                p_cost: p.p_cost,
            },
            _ => return Err(internal("password credential without parameters")),
        },
        None => fake_params(&st, &h).await?,
    }))
}

/// `POST /v1/auth/password/session`: sign in with a password-derived key.
pub async fn session(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<PasswordSessionRequest>,
) -> ApiResult<Cbor<Session>> {
    st.require_method(METHOD)?;
    let h = cred::login_hash(&req.name)?;
    st.pw_limiter.check(METHOD, &h)?;
    let challenge = check_challenge(&st, &req.challenge)?;
    let origin = SignedOrigin::new(&headers, &req.origin)?;
    // Every way of failing from here on looks the same and counts.
    let acl = st.acl();
    let verified = lookup(&st, &h).await?.filter(|(user, _, rec)| {
        rec.identity
            .as_deref()
            .and_then(|i| PublicIdentity::decode(i).ok())
            .is_some_and(|p| pwkey::verify_session(&p, &challenge, &req.origin, &req.sig).is_ok())
            && acl.members.contains_key(user)
    });
    let Some((user, id, _)) = verified else {
        st.pw_limiter.fail(METHOD, &h);
        return Err(unauthorized("unknown login name or wrong password"));
    };
    st.pw_limiter.succeed(METHOD, &h);
    let signed = Signed {
        challenge,
        origin: &origin,
    };
    issue_session(&st, user, id, METHOD, Some(signed)).await
}

/// `POST /v1/auth/password/set`: register the caller's password-derived
/// key, or replace it (a password or login-name change). The old
/// credential and its sessions end; the new one gets a fresh id.
pub async fn set(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<PasswordSet>,
) -> ApiResult<Cbor<CredentialId>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    let h = cred::login_hash(&req.name)?;
    if req.salt.len() != pwkey::SALT_LEN {
        return Err(bad_request("salt must be 32 bytes"));
    }
    let params = Argon2Params {
        m_cost_kib: req.m_cost_kib,
        t_cost: req.t_cost,
        p_cost: req.p_cost,
    };
    check_params(params).map_err(bad_request)?;
    PublicIdentity::decode(&req.identity).map_err(|_| bad_request("bad public identity"))?;
    let user = caller.user;
    let id = cred::new_id();
    let rec = CredRecord {
        method: METHOD.id(),
        created_unix: unix_now(),
        name_hash: Some(ByteBuf::from(h.to_vec())),
        salt: Some(ByteBuf::from(req.salt.clone())),
        params: Some(params),
        identity: Some(ByteBuf::from(req.identity.clone())),
        ..Default::default()
    };
    let (old, _) = txn_loop!(st.store, None, |t| {
        if let Some(holder) = cred::name_holder(&mut t, &h).await?
            && holder != user
        {
            return Err(name_taken("that login name is taken"));
        }
        let creds = cred::list(&mut t, &user).await?;
        if creds.len() >= cred::MAX_PER_USER {
            return Err(quota("too many credentials"));
        }
        // One password credential per user: this one replaces it.
        let mut old = Vec::new();
        for (oid, r) in &creds {
            if r.method() == Some(METHOD) {
                cred::delete(&mut t, &user, oid, r).await?;
                old.push(*oid);
            }
        }
        cred::put(&mut t, &user, &id, &rec);
        Ok(old)
    })?;
    for oid in &old {
        cred::evict(&st, &user, oid);
    }
    st.pw_limiter.succeed(METHOD, &h);
    tracing::info!(id = %cred::hex(&id), replaced = old.len(), "password key set");
    Ok(Cbor(CredentialId { id: id.to_vec() }))
}

/// Failed sign-ins per method and login name, in this node's memory
/// (auth.md §11.3, §8.5). Each method counts a name on its own, with the
/// full cap: after `max` failures of one method, the name is locked for
/// that method for `lockout` from the last one; a success with that
/// method clears its count.
pub struct Limiter {
    max: u32,
    lockout: Duration,
    by_name: Mutex<HashMap<(AuthMethod, [u8; 32]), Failures>>,
}

/// The failures of one name: how many, and when the last one was.
struct Failures {
    n: u32,
    last: Instant,
    last_unix: u64,
}

/// Names tracked before stale entries are dropped.
const LIMITER_CAP: usize = 100_000;

impl Limiter {
    /// A limiter allowing `max` failures per `lockout`.
    pub fn new(max: u32, lockout: Duration) -> Self {
        Limiter {
            max,
            lockout,
            by_name: Mutex::new(HashMap::new()),
        }
    }

    /// 429 `quota` while `name` is locked for `method`.
    pub fn check(&self, method: AuthMethod, name: &[u8; 32]) -> ApiResult<()> {
        let m = self.by_name.lock().expect("limiter lock");
        match m.get(&(method, *name)) {
            Some(f) if f.n >= self.max && f.last.elapsed() < self.lockout => Err(quota(format!(
                "too many failed sign-ins for this login name; retry in {} s",
                (self.lockout - f.last.elapsed()).as_secs() + 1
            ))),
            _ => Ok(()),
        }
    }

    /// Count a failure of `method` for `name`.
    pub fn fail(&self, method: AuthMethod, name: &[u8; 32]) {
        let mut m = self.by_name.lock().expect("limiter lock");
        if m.len() >= LIMITER_CAP {
            let lockout = self.lockout;
            m.retain(|_, f| f.last.elapsed() < lockout);
        }
        let e = m.entry((method, *name)).or_insert(Failures {
            n: 0,
            last: Instant::now(),
            last_unix: 0,
        });
        if e.last.elapsed() >= self.lockout {
            e.n = 0;
        }
        e.n += 1;
        e.last = Instant::now();
        e.last_unix = unix_now();
    }

    /// Clear `method`'s count of a name after a success with it.
    pub fn succeed(&self, method: AuthMethod, name: &[u8; 32]) {
        self.by_name
            .lock()
            .expect("limiter lock")
            .remove(&(method, *name));
    }

    /// Clear `method`'s count of a name whose last failure here is no newer
    /// than a success with that method that another node recorded at
    /// `success_unix` (auth.md §8.5).
    pub fn succeeded_at(&self, method: AuthMethod, name: &[u8; 32], success_unix: u64) {
        let mut m = self.by_name.lock().expect("limiter lock");
        let key = (method, *name);
        if m.get(&key).is_some_and(|f| f.last_unix <= success_unix) {
            m.remove(&key);
        }
    }
}
