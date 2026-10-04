//! Sign-in method 3, a password via OPAQUE (auth.md §8, RFC 9807):
//! registration, the two-round sign-in, the server setup and the sealed
//! login state.
//!
//! The client never sends the password or anything derived from it that
//! allows offline guessing without the server's keys. The server stores
//! the registration record in the credential store and keeps one
//! cluster-wide **server setup** (the OPRF seed and the AKE key pair) in
//! the keyspace, exported with the data.
//!
//! The login state between the two rounds is **stateless**: the server
//! seals it under a key derived from the cluster's challenge key and hands
//! it to the client, so the second round may reach any node.

use crate::acl::Fp;
use crate::auth::{Caller, Signed, check_challenge, issue_session, new_challenge};
use crate::cbor::Cbor;
use crate::cred::{self, CredRecord};
use crate::error::*;
use crate::ids::{random32, unix_now};
use crate::keys;
use crate::origin::SignedOrigin;
use crate::password::check_params;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use axum::http::HeaderMap;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use std::sync::{Arc, OnceLock};
use zen_core::opaque::opaque_ke::{
    CredentialFinalization, CredentialRequest, RegistrationRequest, RegistrationUpload,
    ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
};
use zen_core::opaque::{self as op, RandCore, Suite};
use zen_core::rng::OsRng;
use zen_proto::{
    Argon2Params, AuthMethod, ByteBuf, CredentialId, OpaqueLoginFinish, OpaqueLoginResponse,
    OpaqueLoginStart, OpaqueRegisterFinish, OpaqueRegisterStart, OpaqueRegistration, Session,
};

const METHOD: AuthMethod = AuthMethod::Opaque;

/// The answer to every credential failure of a sign-in.
const FAILED: &str = "unknown login name or wrong password";

/// The server setup, read or created on first use once the cluster is
/// claimed (keyspace.md §3.7). Before the claim no account exists, and a
/// per-process setup answers sign-ins with fake records, so that a server
/// that was only started holds no data.
pub async fn setup(st: &Shared) -> ApiResult<Arc<ServerSetup<Suite>>> {
    if let Some(s) = st.opaque_setup.lock().expect("opaque setup lock").clone() {
        return Ok(s);
    }
    if st.acl().version == 0 {
        static UNCLAIMED: OnceLock<Arc<ServerSetup<Suite>>> = OnceLock::new();
        return Ok(UNCLAIMED
            .get_or_init(|| Arc::new(ServerSetup::new(&mut RandCore(&mut OsRng))))
            .clone());
    }
    let key = keys::opaque_setup();
    // Idempotent: a retry after an unknown commit result reads the setup back.
    let (bytes, _) = txn_loop!(st.store, None, idempotent, |t| {
        Ok(match t.get(&key).await? {
            Some(v) => v,
            None => {
                let v = ServerSetup::<Suite>::new(&mut RandCore(&mut OsRng))
                    .serialize()
                    .to_vec();
                t.set(&key, &v);
                tracing::info!("created the OPAQUE server setup");
                v
            }
        })
    })?;
    // Never replaced: every record depends on it (auth.md §8.1).
    let s = Arc::new(ServerSetup::deserialize(&bytes).map_err(|_| {
        tracing::error!("the stored OPAQUE server setup doesn't decode");
        internal("the OPAQUE server setup is damaged")
    })?);
    *st.opaque_setup.lock().expect("opaque setup lock") = Some(s.clone());
    Ok(s)
}

/// The OPAQUE context of a sign-in through `origin` (auth.md §8.4).
fn params(ctx: &[u8]) -> ServerLoginParameters<'_, '_> {
    ServerLoginParameters {
        context: Some(ctx),
        ..Default::default()
    }
}

/// `POST /v1/auth/opaque/register/start`: evaluate the OPRF for the
/// caller's new password under the login name's key. Stateless: the
/// response depends only on the server setup and the name.
pub async fn register_start(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<OpaqueRegisterStart>,
) -> ApiResult<Cbor<OpaqueRegistration>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    let h = cred::login_hash(&req.name)?;
    let request = op::exact(&req.request, op::REGISTRATION_REQUEST_LEN)
        .ok()
        .and_then(|r| RegistrationRequest::<Suite>::deserialize(r).ok())
        .ok_or_else(|| bad_request("request must be an OPAQUE RegistrationRequest (32 bytes)"))?;
    let (holder, _) = txn_loop!(st.store, None, |t| { cred::name_holder(&mut t, &h).await })?;
    if holder.is_some_and(|u| u != caller.user) {
        return Err(name_taken("that login name is taken"));
    }
    let s = setup(&st).await?;
    let r = ServerRegistration::<Suite>::start(&s, request, &h)
        .map_err(|_| bad_request("request must be an OPAQUE RegistrationRequest (32 bytes)"))?;
    let p = st.cfg.auth.password_params();
    Ok(Cbor(OpaqueRegistration {
        response: r.message.serialize().to_vec(),
        m_cost_kib: p.m_cost_kib,
        t_cost: p.t_cost,
        p_cost: p.p_cost,
    }))
}

/// `POST /v1/auth/opaque/register/finish`: store the caller's OPAQUE
/// record, or replace it (a password or login-name change). The old
/// credential and its sessions end; the new one gets a fresh id.
pub async fn register_finish(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<OpaqueRegisterFinish>,
) -> ApiResult<Cbor<CredentialId>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    let h = cred::login_hash(&req.name)?;
    op::exact(&req.upload, op::REGISTRATION_UPLOAD_LEN)
        .ok()
        .and_then(|u| RegistrationUpload::<Suite>::deserialize(u).ok())
        .ok_or_else(|| bad_request("upload must be an OPAQUE RegistrationUpload (192 bytes)"))?;
    let ksf = Argon2Params {
        m_cost_kib: req.m_cost_kib,
        t_cost: req.t_cost,
        p_cost: req.p_cost,
    };
    check_params(ksf).map_err(bad_request)?;
    let user = caller.user;
    let id = cred::new_id();
    let rec = CredRecord {
        method: METHOD.id(),
        created_unix: unix_now(),
        name_hash: Some(ByteBuf::from(h.to_vec())),
        opaque_record: Some(ByteBuf::from(req.upload.clone())),
        opaque_ksf: Some(ksf),
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
        // One OPAQUE credential per user: this one replaces it.
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
    tracing::info!(id = %cred::hex(&id), replaced = old.len(), "OPAQUE password set");
    Ok(Cbor(CredentialId { id: id.to_vec() }))
}

/// A stored OPAQUE credential behind a login name.
struct Found {
    user: Fp,
    id: Fp,
    record: Option<ServerRegistration<Suite>>,
    ksf: Argon2Params,
    last_used_unix: Option<u64>,
}

/// The OPAQUE credential behind a login name, if any.
async fn lookup(st: &Shared, name_hash: &[u8; 32]) -> ApiResult<Option<Found>> {
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
    Ok(found.map(|(user, id, r)| {
        let record = r
            .opaque_record
            .as_deref()
            .and_then(|b| op::exact(b, op::REGISTRATION_UPLOAD_LEN).ok())
            .and_then(|b| ServerRegistration::deserialize(b).ok())
            .filter(|_| r.opaque_ksf.is_some());
        if record.is_none() {
            // Answered like an unknown name, so the damage isn't visible.
            tracing::error!(id = %cred::hex(&id), "an OPAQUE credential without a usable record");
        }
        Found {
            user,
            id,
            record,
            ksf: r.opaque_ksf.unwrap_or_default(),
            last_used_unix: r.last_used_unix,
        }
    }))
}

/// `POST /v1/auth/opaque/login/start`: the credential response (KE2) for
/// a login name, real or fake, and the sealed login state.
pub async fn login_start(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<OpaqueLoginStart>,
) -> ApiResult<Cbor<OpaqueLoginResponse>> {
    st.require_method(METHOD)?;
    let h = cred::login_hash(&req.name)?;
    // A malformed origin is refused here; the policy is applied at
    // `finish`, with the session (auth.md §8.4).
    SignedOrigin::new(&headers, &req.origin)?;
    let request = op::exact(&req.request, op::CREDENTIAL_REQUEST_LEN)
        .ok()
        .and_then(|r| CredentialRequest::<Suite>::deserialize(r).ok())
        .ok_or_else(|| bad_request("request must be an OPAQUE CredentialRequest (96 bytes)"))?;
    let found = lookup(&st, &h).await?;
    // A success on another node clears the failures counted here before it.
    if let Some(t) = found.as_ref().and_then(|f| f.last_used_unix) {
        st.pw_limiter.succeeded_at(METHOD, &h, t);
    }
    st.pw_limiter.check(METHOD, &h)?;
    // A credential response lets the client test one password guess
    // offline, so every start counts as a failure until a finish succeeds.
    st.pw_limiter.fail(METHOD, &h);
    let s = setup(&st).await?;
    let ctx = op::context(&req.origin);
    let (record, ksf, who) = match found {
        Some(f) if f.record.is_some() => (f.record, f.ksf, Some((f.user, f.id))),
        _ => (None, st.cfg.auth.password_params(), None),
    };
    // With no record, OPAQUE answers from a dummy record: the response
    // looks like a real one (RFC 9807, client enumeration).
    let started = ServerLogin::start(
        &mut RandCore(&mut OsRng),
        &s,
        record,
        request,
        &h,
        params(&ctx),
    )
    .map_err(|_| bad_request("request must be an OPAQUE CredentialRequest (96 bytes)"))?;
    let state = LoginState {
        name_hash: h,
        who,
        origin: req.origin,
        server_login: started.state.serialize().to_vec(),
    };
    Ok(Cbor(OpaqueLoginResponse {
        response: started.message.serialize().to_vec(),
        state: state.seal(&st),
        m_cost_kib: ksf.m_cost_kib,
        t_cost: ksf.t_cost,
        p_cost: ksf.p_cost,
    }))
}

/// `POST /v1/auth/opaque/login/finish`: check the client's finalization
/// (KE3) against the sealed state, and issue a session.
pub async fn login_finish(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<OpaqueLoginFinish>,
) -> ApiResult<Cbor<Session>> {
    st.require_method(METHOD)?;
    let (challenge, state) = LoginState::open(&st, &req.state)?;
    let origin = SignedOrigin::new(&headers, &state.origin)?;
    // Every way of failing from here on looks the same.
    let ctx = op::context(&state.origin);
    let verified = op::exact(&req.finalization, op::CREDENTIAL_FINALIZATION_LEN)
        .ok()
        .and_then(|f| CredentialFinalization::<Suite>::deserialize(f).ok())
        .zip(ServerLogin::<Suite>::deserialize(&state.server_login).ok())
        .is_some_and(|(fin, login)| login.finish(fin, params(&ctx)).is_ok());
    let acl = st.acl();
    let Some((user, id)) = state
        .who
        .filter(|(user, _)| verified && acl.members.contains_key(user))
    else {
        return Err(unauthorized(FAILED));
    };
    // The credential may have been replaced or removed since `start`.
    let (current, _) = txn_loop!(st.store, None, |t| {
        Ok(cred::login(&mut t, METHOD, &state.name_hash).await? == Some((user, id)))
    })?;
    if !current {
        return Err(unauthorized(FAILED));
    }
    let signed = Signed {
        challenge,
        origin: &origin,
    };
    let session = issue_session(&st, user, id, METHOD, Some(signed)).await?;
    st.pw_limiter.succeed(METHOD, &state.name_hash);
    // The sign-in time, which also tells other nodes' limiters about the
    // success (auth.md §8.5). Only an existing record is updated.
    let now = unix_now();
    let touched = txn_loop!(st.store, None, |t| {
        if let Some(mut rec) = cred::get(&mut t, &user, &id).await? {
            rec.last_used_unix = Some(now);
            cred::put(&mut t, &user, &id, &rec);
        }
        Ok(())
    });
    if let Err(e) = touched {
        tracing::warn!(error = %e.message, "recording an OPAQUE sign-in time failed");
    }
    Ok(session)
}

/// The server's state between the two sign-in rounds, sealed for the
/// client to carry (auth.md §8.3):
///
/// ```text
/// state = challenge(32) ‖ nonce(24) ‖ XChaCha20-Poly1305(K, plaintext, aad = challenge)
/// K     = BLAKE3.derive_key("zen-serve 2026 opaque login state", challenge key)
/// plaintext = name_hash(32) ‖ u8 known ‖ user(32) ‖ cred(32)
///             ‖ u16 len ‖ origin ‖ ServerLogin
/// ```
///
/// The challenge is an ordinary one (api.md §3.1): it bounds the state's
/// life to 60 s, and `issue_session` spends it, so a state finishes at
/// most once.
struct LoginState {
    name_hash: [u8; 32],
    who: Option<(Fp, Fp)>,
    origin: String,
    server_login: Vec<u8>,
}

fn state_key(st: &Shared) -> [u8; 32] {
    blake3::derive_key("zen-serve 2026 opaque login state", &st.challenge_key)
}

impl LoginState {
    fn seal(&self, st: &Shared) -> Vec<u8> {
        let challenge = new_challenge(st);
        let mut pt = Vec::with_capacity(99 + self.origin.len() + self.server_login.len());
        pt.extend_from_slice(&self.name_hash);
        let (user, id) = self.who.unwrap_or(([0; 32], [0; 32]));
        pt.push(self.who.is_some() as u8);
        pt.extend_from_slice(&user);
        pt.extend_from_slice(&id);
        pt.extend_from_slice(&(self.origin.len() as u16).to_be_bytes());
        pt.extend_from_slice(self.origin.as_bytes());
        pt.extend_from_slice(&self.server_login);
        let nonce: [u8; 24] = random32()[..24].try_into().expect("24");
        let ct = XChaCha20Poly1305::new(&Key::from(state_key(st)))
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &pt,
                    aad: &challenge,
                },
            )
            .expect("sealing a login state");
        let mut out = challenge.to_vec();
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// The live challenge and the state, or 401.
    fn open(st: &Shared, sealed: &[u8]) -> ApiResult<([u8; 32], Self)> {
        let bad = || unauthorized("bad or expired login state");
        if sealed.len() < 56 {
            return Err(bad());
        }
        let challenge = check_challenge(st, &sealed[..32])
            .map_err(|_| unauthorized("expired login state: start again"))?;
        let nonce: [u8; 24] = sealed[32..56].try_into().expect("24");
        let pt = XChaCha20Poly1305::new(&Key::from(state_key(st)))
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &sealed[56..],
                    aad: &challenge,
                },
            )
            .map_err(|_| bad())?;
        if pt.len() < 99 {
            return Err(bad());
        }
        let name_hash = pt[..32].try_into().expect("32");
        let who = (pt[32] == 1).then(|| {
            (
                pt[33..65].try_into().expect("32"),
                pt[65..97].try_into().expect("32"),
            )
        });
        let len = u16::from_be_bytes([pt[97], pt[98]]) as usize;
        let origin = pt
            .get(99..99 + len)
            .and_then(|o| String::from_utf8(o.to_vec()).ok())
            .ok_or_else(bad)?;
        Ok((
            challenge,
            LoginState {
                name_hash,
                who,
                origin,
                server_login: pt[99 + len..].to_vec(),
            },
        ))
    }
}
