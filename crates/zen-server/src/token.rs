//! Sign-in method 4, API tokens (auth.md §9): admin-issued bearer secrets
//! for services and bots, used directly as `Authorization: Bearer
//! zen_at_…` without a session.

use crate::acl::Fp;
use crate::auth::{Caller, SESSION_CACHE};
use crate::cbor::Cbor;
use crate::cred::{self, CredRecord};
use crate::error::*;
use crate::ids::{random32, unix_now};
use crate::state::{CachedSession, SessionInfo, Shared};
use crate::txn::txn_loop;
use axum::extract::State;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::time::Instant;
use zen_proto::{API_TOKEN_PREFIX, ApiToken, ApiTokenCreate, AuthMethod};

const METHOD: AuthMethod = AuthMethod::ApiToken;

/// Length of a token's text: the prefix and 43 base64url characters.
pub const TOKEN_LEN: usize = API_TOKEN_PREFIX.len() + 43;

/// Max label length.
const MAX_LABEL: usize = 128;

/// The credential id of a token secret. Only this hash is stored.
pub fn token_id(secret: &[u8; 32]) -> Fp {
    blake3::derive_key("zen-serve 2026 api token", secret)
}

/// Whether bearer bytes are an API token rather than a session token.
pub fn is_api_token(bearer: &[u8]) -> bool {
    bearer.len() == TOKEN_LEN && bearer.starts_with(API_TOKEN_PREFIX.as_bytes())
}

/// `POST /v1/auth/tokens/create`: an admin issues a token for a member.
pub async fn create(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<ApiTokenCreate>,
) -> ApiResult<Cbor<ApiToken>> {
    st.require_method(METHOD)?;
    caller.require_admin()?;
    let user: Fp = req
        .user
        .as_slice()
        .try_into()
        .map_err(|_| bad_request("user must be 32 bytes"))?;
    if !caller.acl.members.contains_key(&user) {
        return Err(bad_request("the user is not a member"));
    }
    let now = unix_now();
    if req.expires_unix.is_some_and(|e| e <= now) {
        return Err(bad_request("expires_unix is in the past"));
    }
    if req.label.as_ref().is_some_and(|l| l.len() > MAX_LABEL) {
        return Err(bad_request(format!("a label is at most {MAX_LABEL} bytes")));
    }
    let secret = random32();
    let id = token_id(&secret);
    let rec = CredRecord {
        method: METHOD.id(),
        created_unix: now,
        expires_unix: req.expires_unix,
        label: req.label.clone(),
        issued_by: Some(caller.user.to_vec().into()),
        ..Default::default()
    };
    txn_loop!(st.store, None, |t| {
        if cred::list(&mut t, &user).await?.len() >= cred::MAX_PER_USER {
            return Err(quota("too many credentials"));
        }
        cred::put(&mut t, &user, &id, &rec);
        Ok(())
    })?;
    tracing::info!(id = %cred::hex(&id), "API token created");
    Ok(Cbor(ApiToken {
        token: format!("{API_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(secret)),
        id: id.to_vec(),
        expires_unix: req.expires_unix,
    }))
}

/// Authenticate an API token (`is_api_token`). Like a session, the result
/// is cached for up to [`SESSION_CACHE`]; revocation reaches other nodes
/// within that time.
pub async fn resolve(st: &Shared, bearer: &[u8]) -> ApiResult<Caller> {
    let bad = || unauthorized("unknown API token");
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&bearer[API_TOKEN_PREFIX.len()..])
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(bad)?;
    if !st.method_on(METHOD) {
        return Err(unauthorized("the sign-in method api_token is disabled"));
    }
    let id = token_id(&secret);
    let cached = {
        let sessions = st.sessions.lock().expect("session lock");
        sessions
            .get(&id)
            .filter(|c| c.at.elapsed() < SESSION_CACHE)
            .map(|c| c.info.clone())
    };
    let info = match cached {
        Some(i) => i,
        None => {
            let (found, _) = txn_loop!(st.store, None, |t| {
                let Some(user) = cred::owner(&mut t, &id).await? else {
                    return Ok(None);
                };
                Ok(cred::get(&mut t, &user, &id)
                    .await?
                    .filter(|r| r.method() == Some(METHOD))
                    .map(|r| (user, r)))
            })?;
            let (user, rec) = found.ok_or_else(bad)?;
            let info = SessionInfo {
                user,
                cred: id,
                method: METHOD,
                expires_unix: rec.expires_unix.unwrap_or(u64::MAX),
            };
            st.sessions.lock().expect("session lock").insert(
                id,
                CachedSession {
                    info: info.clone(),
                    at: Instant::now(),
                },
            );
            info
        }
    };
    if info.expires_unix <= unix_now() {
        return Err(unauthorized("API token expired"));
    }
    let acl = st.acl();
    if !acl.members.contains_key(&info.user) {
        return Err(unauthorized("no longer a member"));
    }
    Ok(Caller {
        user: info.user,
        device: id,
        method: METHOD,
        session: id,
        acl,
    })
}

/// Delete expired API tokens (sweeper). Returns the number removed.
pub async fn sweep(st: &Shared) -> ApiResult<usize> {
    let now = unix_now();
    let prefix = crate::keys::creds_prefix();
    let end = crate::keys::end_of(&prefix);
    let mut from = prefix.clone();
    let mut removed = 0;
    loop {
        let ((n, next), _) = txn_loop!(st.store, None, |t| {
            let got = t.snapshot_get_range(&from, &end, 1000, false).await?;
            let mut n = 0;
            for (k, v) in &got {
                let Ok(rec) = zen_proto::from_cbor::<CredRecord>(v) else {
                    continue;
                };
                if rec.method() != Some(METHOD) || rec.expires_unix.is_none_or(|e| e > now) {
                    continue;
                }
                if let Some((user, id)) = parse_cred_key(&prefix, k) {
                    // Re-read in the transaction, so a concurrent change
                    // conflicts instead of being undone.
                    if let Some(rec) = cred::get(&mut t, &user, &id).await? {
                        cred::delete(&mut t, &user, &id, &rec).await?;
                        n += 1;
                    }
                }
            }
            let next = (got.len() == 1000).then(|| zen_store::key_after(&got[999].0));
            Ok((n, next))
        })?;
        removed += n;
        match next {
            Some(k) => from = k,
            None => return Ok(removed),
        }
    }
}

fn parse_cred_key(prefix: &[u8], k: &[u8]) -> Option<(Fp, Fp)> {
    use zen_store::tuple::{Elem, unpack_prefix};
    let (e, _) = unpack_prefix(k.get(prefix.len()..)?, 2).ok()?;
    match e.as_slice() {
        [Elem::Bytes(u), Elem::Bytes(i)] => {
            Some((u.as_slice().try_into().ok()?, i.as_slice().try_into().ok()?))
        }
        _ => None,
    }
}
