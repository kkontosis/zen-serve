//! Sign-in method 2, passkeys (auth.md §7): WebAuthn registration and
//! sign-in, verified by [`crate::webauthn`].
//!
//! * The relying-party id is `[auth] passkey_rp_id`, or else the host of
//!   the canonical origin (auth.md §5.5). Without either, passkeys can't be
//!   registered or used yet.
//! * The user handle is the user fingerprint.
//! * A passkey's store id is `BLAKE3.derive_key("zen-serve 2026 passkey",
//!   credential id)`; the credential's owner index (`credx`) finds the
//!   user from the id the authenticator returns.
//! * The clientDataJSON origin goes through the origin policy in
//!   `issue_session`, like a signed device or password origin.

use crate::acl::Fp;
use crate::auth::{Caller, Signed, check_challenge, issue_session, new_challenge, spend_challenge};
use crate::cbor::Cbor;
use crate::cred::{self, CredRecord};
use crate::error::*;
use crate::ids::unix_now;
use crate::origin::{self, OwnOrigins, SignedOrigin, Verdict};
use crate::state::Shared;
use crate::txn::txn_loop;
use crate::webauthn::{self, CoseKey, Expected, Stored};
use axum::extract::State;
use axum::http::HeaderMap;
use zen_proto::{
    AuthMethod, ByteBuf, CredentialId, Empty, PasskeyBegin, PasskeyCreation, PasskeyInfo,
    PasskeyRegister, PasskeyRequest, PasskeySession, Session, valid_origin,
};

const METHOD: AuthMethod = AuthMethod::Passkey;

/// Max label length.
const MAX_LABEL: usize = 128;

/// The store id of a WebAuthn credential id.
pub fn credential_id(webauthn_id: &[u8]) -> Fp {
    blake3::derive_key("zen-serve 2026 passkey", webauthn_id)
}

/// The relying-party id, given the server's own origins.
fn rp_id_of(st: &Shared, own: Option<&OwnOrigins>) -> Option<String> {
    st.cfg
        .auth
        .passkey_rp_id
        .clone()
        .or_else(|| own.and_then(OwnOrigins::rp_id).map(str::to_owned))
}

/// The relying-party id, or 400 while the server knows no origin of its
/// own (auth.md §7.1).
async fn rp_id(st: &Shared) -> ApiResult<String> {
    let own = origin::own_origins(st).await?;
    rp_id_of(st, Some(&own)).ok_or_else(|| {
        bad_request(
            "passkeys need the server's origin, and none is known yet: set public_origins, \
             claim with `origin`, or sign in once with another method (spec/auth.md §7.1)",
        )
    })
}

/// `/v1/info` `auth.passkey`.
pub fn info(st: &Shared, own: Option<&OwnOrigins>) -> PasskeyInfo {
    PasskeyInfo {
        rp_id: rp_id_of(st, own),
        user_verification: st.cfg.auth.passkey_user_verification().into(),
        algorithms: webauthn::ALGORITHMS.to_vec(),
    }
}

/// The WebAuthn ids of `user`'s passkeys registered under `rp_id`.
async fn webauthn_ids(st: &Shared, user: &Fp, rp_id: &str) -> ApiResult<Vec<ByteBuf>> {
    let (creds, _) = txn_loop!(st.store, None, |t| { cred::list(&mut t, user).await })?;
    Ok(creds
        .into_iter()
        .filter(|(_, r)| r.method() == Some(METHOD) && r.rp_id.as_deref() == Some(rp_id))
        .filter_map(|(_, r)| r.webauthn_id)
        .collect())
}

fn refused(e: webauthn::Error) -> ApiError {
    unauthorized(e.to_string())
}

/// `POST /v1/auth/passkey/register/begin`: the options for
/// `navigator.credentials.create`.
pub async fn register_begin(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<PasskeyCreation>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    let rp_id = rp_id(&st).await?;
    Ok(Cbor(PasskeyCreation {
        challenge: new_challenge(&st).to_vec(),
        exclude: webauthn_ids(&st, &caller.user, &rp_id).await?,
        rp_id,
        user_handle: caller.user.to_vec(),
        algorithms: webauthn::ALGORITHMS.to_vec(),
        user_verification: st.cfg.auth.passkey_user_verification().into(),
    }))
}

/// `POST /v1/auth/passkey/register/finish`: store the caller's new
/// passkey. The attestation statement is not verified (auth.md §7.2).
pub async fn register_finish(
    State(st): State<Shared>,
    caller: Caller,
    headers: HeaderMap,
    Cbor(req): Cbor<PasskeyRegister>,
) -> ApiResult<Cbor<CredentialId>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    if req.label.as_ref().is_some_and(|l| l.len() > MAX_LABEL) {
        return Err(bad_request(format!("a label is at most {MAX_LABEL} bytes")));
    }
    let rp_id = rp_id(&st).await?;
    let cd = webauthn::parse_client_data(&req.client_data_json).map_err(refused)?;
    let challenge = check_challenge(&st, &cd.challenge)?;
    let origin = SignedOrigin::new(&headers, &cd.origin)?;
    let reg = webauthn::verify_registration(
        &req.attestation_object,
        &req.client_data_json,
        &Expected {
            challenge: &challenge,
            rp_id: &rp_id,
            require_uv: st.cfg.auth.passkey_require_uv,
            // The policy itself runs in the transaction below.
            origin_ok: &valid_origin,
        },
    )
    .map_err(|e| bad_request(e.to_string()))?;
    let c = reg.credential();
    let id = credential_id(&c.credential_id);
    let now = unix_now();
    let rec = CredRecord {
        method: METHOD.id(),
        created_unix: now,
        label: req.label.clone(),
        webauthn_id: Some(ByteBuf::from(c.credential_id.clone())),
        cose_key: Some(ByteBuf::from(c.public_key_cose.clone())),
        alg: Some(c.public_key.alg()),
        sign_count: Some(reg.auth_data.sign_count),
        rp_id: Some(rp_id.clone()),
        ..Default::default()
    };
    let user = caller.user;
    txn_loop!(st.store, None, |t| {
        spend_challenge(&mut t, &challenge).await?;
        origin::check_in_txn(&st, &mut t, &origin).await?;
        if cred::owner(&mut t, &id).await?.is_some() {
            return Err(bad_request("this passkey is already registered"));
        }
        if cred::list(&mut t, &user).await?.len() >= cred::MAX_PER_USER {
            return Err(quota("too many credentials"));
        }
        cred::put(&mut t, &user, &id, &rec);
        Ok(())
    })?;
    tracing::info!(id = %cred::hex(&id), fmt = %reg.fmt, alg = c.public_key.alg(), "passkey registered");
    Ok(Cbor(CredentialId { id: id.to_vec() }))
}

/// `POST /v1/auth/passkey/session/begin`: the options for
/// `navigator.credentials.get`. No session.
pub async fn session_begin(
    State(st): State<Shared>,
    Cbor(req): Cbor<PasskeyBegin>,
) -> ApiResult<Cbor<PasskeyRequest>> {
    st.require_method(METHOD)?;
    let rp_id = rp_id(&st).await?;
    let allow = match req.user.as_deref() {
        None => Vec::new(),
        Some(u) => {
            let u: Fp = u
                .try_into()
                .map_err(|_| bad_request("user must be 32 bytes"))?;
            // A non-member gets the same empty list as a member without
            // passkeys.
            if st.acl().members.contains_key(&u) {
                webauthn_ids(&st, &u, &rp_id).await?
            } else {
                Vec::new()
            }
        }
    };
    Ok(Cbor(PasskeyRequest {
        challenge: new_challenge(&st).to_vec(),
        rp_id,
        allow,
        user_verification: st.cfg.auth.passkey_user_verification().into(),
    }))
}

/// `POST /v1/auth/passkey/session`: sign in with a passkey assertion.
pub async fn session(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<PasskeySession>,
) -> ApiResult<Cbor<Session>> {
    st.require_method(METHOD)?;
    let cd = webauthn::parse_client_data(&req.client_data_json).map_err(refused)?;
    let challenge = check_challenge(&st, &cd.challenge)?;
    let origin = SignedOrigin::new(&headers, &cd.origin)?;
    if req.credential_id.is_empty() || req.credential_id.len() > webauthn::MAX_CREDENTIAL_ID {
        return Err(unauthorized("unknown passkey"));
    }
    let id = credential_id(&req.credential_id);
    let acl = st.acl();
    let require_uv = st.cfg.auth.passkey_require_uv;
    let now = unix_now();
    let (user, _) = txn_loop!(st.store, None, |t| {
        let unknown = || unauthorized("unknown passkey");
        let user = cred::owner(&mut t, &id).await?.ok_or_else(unknown)?;
        let mut rec = cred::get(&mut t, &user, &id)
            .await?
            .filter(|r| r.method() == Some(METHOD))
            .ok_or_else(unknown)?;
        if !acl.members.contains_key(&user) {
            return Err(unauthorized("not a member"));
        }
        if req.user_handle.as_deref().is_some_and(|h| h != user) {
            return Err(unauthorized("the user handle doesn't match the passkey"));
        }
        // Refuse a replay or a refused origin before the counter check,
        // which would call a replay a clone. `issue_session` checks both
        // again, and pins the origin, with the session.
        if t.get(&crate::keys::challenge(&challenge)).await?.is_some() {
            return Err(unauthorized("challenge already used"));
        }
        let pins = origin::read_pins(&mut t).await?;
        if origin::decide(&st.cfg, &acl.origins, &pins, &origin) == Verdict::Reject {
            return Err(unauthorized("origin not accepted"));
        }
        let (Some(key), Some(rp_id)) = (rec.cose_key.as_deref(), rec.rp_id.as_deref()) else {
            return Err(internal("passkey without a key"));
        };
        let key = CoseKey::decode(key).map_err(|_| internal("stored passkey key"))?;
        let stored = Stored {
            key: &key,
            rp_id,
            sign_count: rec.sign_count.unwrap_or(0),
        };
        let want = Expected {
            challenge: &challenge,
            rp_id,
            require_uv,
            origin_ok: &|_| true,
        };
        let ad = match webauthn::verify_assertion(
            &stored,
            &req.authenticator_data,
            &req.client_data_json,
            &req.signature,
            &want,
        ) {
            Ok(ad) => ad,
            Err(e @ webauthn::Error::Counter { .. }) => {
                tracing::warn!(id = %cred::hex(&id), user = %cred::hex(&user), error = %e,
                    "passkey sign-in refused: possible cloned authenticator");
                return Err(refused(e));
            }
            Err(e) => return Err(refused(e)),
        };
        rec.sign_count = Some(ad.sign_count);
        rec.last_used_unix = Some(now);
        cred::put(&mut t, &user, &id, &rec);
        Ok(user)
    })?;
    let signed = Signed {
        challenge,
        origin: &origin,
    };
    issue_session(&st, user, id, METHOD, Some(signed)).await
}
