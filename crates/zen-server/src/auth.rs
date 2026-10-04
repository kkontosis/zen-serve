//! Sessions and device sign-in (api.md §3, auth.md §3, §6) and the
//! [`Caller`] extractor.

use crate::acl::{AclState, Fp, R_READ};
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::{random32, unix_now};
use crate::keys;
use crate::origin::SignedOrigin;
use crate::state::{CachedSession, SessionInfo, Shared};
use crate::txn::txn_loop;
use axum::extract::{FromRequestParts, State};
use axum::http::{HeaderMap, header, request::Parts};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_core::labels;
use zen_core::sig::{PublicIdentity, verify_device_cert};
use zen_proto::{
    AuthInfo, AuthMethod, Challenge, Empty, OriginInfo, Session, SessionRequest, session_message,
};

const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// The sign-in methods this server implements. A method also needs its
/// `[auth]` flag ([`crate::state::AppState::method_on`]).
pub const IMPLEMENTED: &[AuthMethod] = &[
    AuthMethod::DeviceKey,
    AuthMethod::Passkey,
    AuthMethod::Opaque,
    AuthMethod::ApiToken,
    AuthMethod::Mtls,
    AuthMethod::PasswordKey,
];

/// The order in which a client offers sign-in methods: the first one that
/// is on is `/v1/info`'s `auth.default`. API tokens are for services, never
/// the default.
const PREFERENCE: &[AuthMethod] = &[
    AuthMethod::PasswordKey,
    AuthMethod::Passkey,
    AuthMethod::DeviceKey,
    AuthMethod::Opaque,
    AuthMethod::Mtls,
];

/// Whether the server offers `m` (auth.md §2): it is on, and for mTLS
/// not dormant, that is a client certificate can reach the server at all
/// (auth.md §10).
pub fn offered(st: &Shared, m: AuthMethod) -> bool {
    st.method_on(m) && (m != AuthMethod::Mtls || crate::mtls::configured(&st.cfg))
}

/// `/v1/info` `auth` (auth.md §2).
pub async fn info(st: &Shared) -> AuthInfo {
    let own = crate::origin::own_origins(st)
        .await
        .inspect_err(|e| tracing::warn!(error = %e.message, "reading the pinned origins failed"))
        .ok();
    let passkey = st
        .method_on(AuthMethod::Passkey)
        .then(|| crate::passkey::info(st, own.as_ref()));
    let origins = own.map(|o| OriginInfo {
        origins: o.origins,
        pinning: o.pinning,
        host_fallback: o.host_fallback,
    });
    AuthInfo {
        passkey,
        origins,
        methods: AuthMethod::ALL
            .into_iter()
            .filter(|m| offered(st, *m))
            .map(|m| m.name().to_string())
            .collect(),
        default: PREFERENCE
            .iter()
            .find(|m| offered(st, **m))
            .map(|m| m.name().to_string()),
        password_params: st
            .method_on(AuthMethod::PasswordKey)
            .then(|| st.cfg.auth.password_params()),
    }
}

/// An authenticated request: the user is a member of the current ACL and
/// the credential it signed in with is still valid.
pub struct Caller {
    /// User fingerprint.
    pub user: Fp,
    /// "The device": the device fingerprint for `device_key` sessions, the
    /// stable credential id for every other method (auth.md §3). Fencing
    /// tokens, idempotency records, the filesystem op chain and ephemeral
    /// rate limits are keyed by it.
    pub device: Fp,
    /// How the caller signed in.
    pub method: AuthMethod,
    /// Hash of the session token.
    pub session: [u8; 32],
    /// The ACL the request was authorized against.
    pub acl: Arc<AclState>,
}

impl Caller {
    /// Whether the user is an admin.
    pub fn is_admin(&self) -> bool {
        self.acl.admins.contains(&self.user)
    }

    /// 403 unless the caller signed in interactively: API tokens can't
    /// manage credentials or the origin policy (auth.md §9).
    pub fn require_interactive(&self) -> ApiResult<()> {
        if self.method == AuthMethod::ApiToken {
            Err(forbidden("API tokens cannot manage sign-in"))
        } else {
            Ok(())
        }
    }

    /// 403 unless the caller is an admin, signed in interactively.
    pub fn require_admin(&self) -> ApiResult<()> {
        self.require_interactive()?;
        if self.is_admin() {
            Ok(())
        } else {
            Err(forbidden("admins only"))
        }
    }

    /// 403 unless the caller has `right` on `fs`.
    pub fn require_fs(&self, fs: u32, right: u8) -> ApiResult<()> {
        if self.acl.fs_rights(&self.user, fs) & right == right {
            Ok(())
        } else {
            Err(forbidden(if right == R_READ {
                "no read right on this fs"
            } else {
                "no write right on this fs"
            }))
        }
    }

    /// 403 unless the caller has `right` on `topic` (or on every topic
    /// under it, for a prefix).
    pub fn require_topic(&self, fs: u32, topic: &[u8], right: u8) -> ApiResult<()> {
        if self.acl.topic_rights(&self.user, fs, topic) & right == right {
            Ok(())
        } else {
            Err(forbidden("no right on this topic"))
        }
    }
}

/// How long a node trusts its cached copy of a session. A logout or expiry
/// reaches other nodes within this time; ACL changes apply immediately.
pub const SESSION_CACHE: Duration = Duration::from_secs(10);

/// The keyspace id of a bearer token: a hash, so a dump holds no tokens.
pub fn token_hash(token: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("zen-serve 2025 session token", token)
}

/// Session record: `user_fp(32) ‖ cred(32) ‖ u64 expires_unix ‖ u8 method`
/// (keyspace.md §3.5).
pub fn encode_session(s: &SessionInfo) -> Vec<u8> {
    let mut v = Vec::with_capacity(73);
    v.extend_from_slice(&s.user);
    v.extend_from_slice(&s.cred);
    v.extend_from_slice(&s.expires_unix.to_be_bytes());
    v.push(s.method.id());
    v
}

/// Records written before sign-in methods existed have no method byte:
/// they are device sessions.
fn decode_session(v: &[u8]) -> Option<SessionInfo> {
    let method = match v.len() {
        72 => AuthMethod::DeviceKey,
        73 => AuthMethod::from_id(v[72])?,
        _ => return None,
    };
    Some(SessionInfo {
        user: v[..32].try_into().expect("32"),
        cred: v[32..64].try_into().expect("32"),
        method,
        expires_unix: u64::from_be_bytes(v[64..72].try_into().expect("8")),
    })
}

async fn lookup_session(st: &Shared, hash: &[u8; 32]) -> ApiResult<Option<SessionInfo>> {
    let cached = {
        let sessions = st.sessions.lock().expect("session lock");
        sessions
            .get(hash)
            .filter(|c| c.at.elapsed() < SESSION_CACHE)
            .map(|c| c.info.clone())
    };
    if let Some(s) = cached {
        return Ok(Some(s));
    }
    let mut t = st.store.begin(None).await?;
    let s = t
        .snapshot_get(&keys::session(hash))
        .await?
        .as_deref()
        .and_then(decode_session);
    // Outside the ACL, a session lives only as long as its credential.
    // (A session whose method is off fails in `resolve` instead.)
    if let Some(info) = &s
        && info.method != AuthMethod::DeviceKey
        && st.method_on(info.method)
        && t.snapshot_get(&keys::cred(&info.user, &info.cred))
            .await?
            .is_none()
    {
        st.sessions.lock().expect("session lock").remove(hash);
        return Err(unauthorized("the session's credential was removed"));
    }
    let mut sessions = st.sessions.lock().expect("session lock");
    match &s {
        Some(info) => {
            sessions.insert(
                *hash,
                CachedSession {
                    info: info.clone(),
                    at: Instant::now(),
                },
            );
        }
        None => {
            sessions.remove(hash);
        }
    }
    Ok(s)
}

/// Resolve a bearer token, a session token or an API token (auth.md §9),
/// against the current ACL.
pub async fn resolve(st: &Shared, token: &[u8]) -> ApiResult<Caller> {
    if crate::token::is_api_token(token) {
        return crate::token::resolve(st, token).await;
    }
    let token: [u8; 32] = token
        .try_into()
        .map_err(|_| unauthorized("bad session token"))?;
    let hash = token_hash(&token);
    let s = lookup_session(st, &hash)
        .await?
        .ok_or_else(|| unauthorized("unknown session"))?;
    if s.expires_unix <= unix_now() {
        return Err(unauthorized("session expired"));
    }
    // A session outlives its method being turned off, but isn't accepted
    // while it is off (auth.md §3).
    if !st.method_on(s.method) {
        return Err(unauthorized(format!(
            "the session's sign-in method {} is disabled",
            s.method.name()
        )));
    }
    let acl = st.acl();
    match s.method {
        AuthMethod::DeviceKey => {
            if !acl.has_device(&s.user, &s.cred) {
                return Err(unauthorized("device no longer in the ACL"));
            }
        }
        _ => {
            if !acl.members.contains_key(&s.user) {
                return Err(unauthorized("no longer a member"));
            }
        }
    }
    Ok(Caller {
        user: s.user,
        device: s.cred,
        method: s.method,
        session: hash,
        acl,
    })
}

impl FromRequestParts<Shared> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, ApiError> {
        let h = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| unauthorized("missing bearer token"))?
            .trim();
        if crate::token::is_api_token(h.as_bytes()) {
            return resolve(st, h.as_bytes()).await;
        }
        let token = URL_SAFE_NO_PAD
            .decode(h)
            .map_err(|_| unauthorized("bad bearer token"))?;
        resolve(st, &token).await
    }
}

/// `POST /v1/auth/challenge`. Challenges are stateless, so any node can
/// check them: `nonce(12) ‖ u32 expires_unix ‖ MAC(16)` under the cluster's
/// challenge key. Single use is enforced when a session is created.
pub async fn challenge(State(st): State<Shared>, Cbor(_): Cbor<Empty>) -> Cbor<Challenge> {
    Cbor(Challenge {
        challenge: new_challenge(&st).to_vec(),
    })
}

/// A fresh challenge (`/v1/auth/challenge`; passkeys hand one out with
/// their ceremony options).
pub fn new_challenge(st: &Shared) -> [u8; 32] {
    let mut c = [0u8; 32];
    c[..12].copy_from_slice(&random32()[..12]);
    let exp = (unix_now() + CHALLENGE_TTL.as_secs()) as u32;
    c[12..16].copy_from_slice(&exp.to_be_bytes());
    let mac = challenge_mac(&st.challenge_key, &c[..16]);
    c[16..].copy_from_slice(&mac);
    c
}

/// Spend challenge `c` in `t`: 401 if it was used already. The record is
/// kept until the challenge would have expired (the sweeper).
pub async fn spend_challenge(t: &mut Box<dyn zen_store::Txn>, c: &[u8; 32]) -> ApiResult<()> {
    let used = keys::challenge(c);
    if t.get(&used).await?.is_some() {
        return Err(unauthorized("challenge already used"));
    }
    let exp = u32::from_be_bytes(c[12..16].try_into().expect("4")) as u64;
    t.set(&used, &exp.to_be_bytes());
    Ok(())
}

fn challenge_mac(key: &[u8; 32], body: &[u8]) -> [u8; 16] {
    blake3::keyed_hash(key, body).as_bytes()[..16]
        .try_into()
        .expect("16")
}

/// Whether `c` is a live challenge this cluster issued.
fn challenge_ok(st: &Shared, c: &[u8; 32]) -> bool {
    let mac = challenge_mac(&st.challenge_key, &c[..16]);
    let fresh = u32::from_be_bytes(c[12..16].try_into().expect("4")) as u64 > unix_now();
    let mut diff = 0u8;
    for (a, b) in mac.iter().zip(&c[16..]) {
        diff |= a ^ b;
    }
    diff == 0 && fresh
}

/// Load or create the cluster-wide challenge key.
pub async fn challenge_key(store: &dyn zen_store::Storage) -> ApiResult<[u8; 32]> {
    let key = keys::meta("challenge_key");
    let (k, _) = txn_loop!(store, None, |t| {
        Ok(match t.get(&key).await? {
            Some(v) if v.len() == 32 => v.try_into().expect("32"),
            _ => {
                let k = random32();
                t.set(&key, &k);
                k
            }
        })
    })?;
    Ok(k)
}

/// A live challenge from a request field, or 401.
pub fn check_challenge(st: &Shared, c: &[u8]) -> ApiResult<[u8; 32]> {
    let challenge: [u8; 32] = c.try_into().map_err(|_| unauthorized("bad challenge"))?;
    if !challenge_ok(st, &challenge) {
        return Err(unauthorized("unknown or expired challenge"));
    }
    Ok(challenge)
}

/// `POST /v1/auth/session`: device sign-in (auth.md §6).
pub async fn session(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<SessionRequest>,
) -> ApiResult<Cbor<Session>> {
    st.require_method(AuthMethod::DeviceKey)?;
    let challenge = check_challenge(&st, &req.challenge)?;
    let origin = SignedOrigin::new(&headers, &req.origin)?;
    let user = PublicIdentity::decode(&req.user).map_err(|_| unauthorized("bad identity"))?;
    let user_fp = user.fingerprint();
    let acl = st.acl();
    let member = acl
        .members
        .get(&user_fp)
        .ok_or_else(|| unauthorized("not a member"))?;
    let (device, _) =
        verify_device_cert(&member.identity, &req.cert).map_err(|_| unauthorized("bad cert"))?;
    let device_fp = device.fingerprint();
    if !member.devices.contains(&device_fp) {
        return Err(unauthorized("device not in the ACL"));
    }
    device
        .signing()
        .verify(
            labels::SIG_SESSION,
            &session_message(&challenge, &req.origin),
            &req.sig,
        )
        .map_err(|_| unauthorized("bad session signature"))?;
    let signed = Signed {
        challenge,
        origin: &origin,
    };
    issue_session(&st, user_fp, device_fp, AuthMethod::DeviceKey, Some(signed)).await
}

/// What a sign-in signed: the challenge it spends and the origin it names.
pub struct Signed<'a> {
    /// The challenge.
    pub challenge: [u8; 32],
    /// The origin, checked against the policy (auth.md §5).
    pub origin: &'a SignedOrigin,
}

/// Create a session for `user`, signed in with credential `cred` by
/// `method`. A method that signs a challenge and an origin passes them in
/// `signed`: the challenge is spent and the origin checked, and pinned
/// when it is the first (auth.md §5.2), in the session's transaction.
/// Every sign-in method ends here (auth.md §3).
pub async fn issue_session(
    st: &Shared,
    user: Fp,
    cred: Fp,
    method: AuthMethod,
    signed: Option<Signed<'_>>,
) -> ApiResult<Cbor<Session>> {
    let token = random32();
    let ttl = st.cfg.limits.session_ttl_secs;
    let info = SessionInfo {
        user,
        cred,
        method,
        expires_unix: unix_now() + ttl,
    };
    let hash = token_hash(&token);
    // Idempotent: a retry after an unknown result finds its own session.
    let sess_key = keys::session(&hash);
    txn_loop!(st.store, None, idempotent, |t| {
        if t.get(&sess_key).await?.is_some() {
            return Ok(());
        }
        if let Some(s) = &signed {
            spend_challenge(&mut t, &s.challenge).await?;
            crate::origin::check_in_txn(st, &mut t, s.origin).await?;
        }
        t.set(&sess_key, &encode_session(&info));
        Ok(())
    })?;
    st.sessions.lock().expect("session lock").insert(
        hash,
        CachedSession {
            info: info.clone(),
            at: Instant::now(),
        },
    );
    Ok(Cbor(Session {
        token: token.to_vec(),
        expires_unix: info.expires_unix,
        user_fp: user.to_vec(),
        device_fp: cred.to_vec(),
        method: Some(method.name().into()),
    }))
}

/// `POST /v1/auth/logout`: end the caller's session on every node.
pub async fn logout(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<Empty>> {
    if caller.method == AuthMethod::ApiToken {
        return Err(bad_request(
            "an API token is not a session: an admin revokes it (/v1/auth/credentials/remove)",
        ));
    }
    let key = keys::session(&caller.session);
    txn_loop!(st.store, None, |t| {
        t.clear(&key);
        Ok(())
    })?;
    st.sessions
        .lock()
        .expect("session lock")
        .remove(&caller.session);
    Ok(Cbor(Empty {}))
}

/// Remove expired sessions and consumed challenges (sweeper). Returns the
/// number removed.
pub async fn sweep(st: &Shared) -> ApiResult<usize> {
    let now = unix_now();
    let mut removed = 0;
    for (prefix, exp_at) in [(keys::session_prefix(), 64), (keys::challenge_prefix(), 0)] {
        let end = keys::end_of(&prefix);
        let mut from = prefix.clone();
        loop {
            let ((n, next), _) = txn_loop!(st.store, None, |t| {
                let got = t.snapshot_get_range(&from, &end, 1000, false).await?;
                let mut n = 0;
                for (k, v) in &got {
                    let exp = v
                        .get(exp_at..exp_at + 8)
                        .map(|b| u64::from_be_bytes(b.try_into().expect("8")))
                        .unwrap_or(0);
                    if exp <= now {
                        t.clear(k);
                        n += 1;
                    }
                }
                let next = (got.len() == 1000).then(|| zen_store::key_after(&got[999].0));
                Ok((n, next))
            })?;
            removed += n;
            match next {
                Some(k) => from = k,
                None => break,
            }
        }
    }
    st.sessions
        .lock()
        .expect("session lock")
        .retain(|_, c| c.at.elapsed() < SESSION_CACHE);
    Ok(removed)
}
