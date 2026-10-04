//! Device sign-in (api.md §3) and the [`Caller`] extractor.

use crate::acl::{AclState, Fp, R_READ};
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::{random32, unix_now};
use crate::keys;
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
use zen_proto::{Challenge, Empty, Session, SessionRequest, session_message};

const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// An authenticated request: the device is in the current ACL.
pub struct Caller {
    /// User fingerprint.
    pub user: Fp,
    /// Device fingerprint.
    pub device: Fp,
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

fn encode_session(s: &SessionInfo) -> Vec<u8> {
    let mut v = Vec::with_capacity(72);
    v.extend_from_slice(&s.user);
    v.extend_from_slice(&s.device);
    v.extend_from_slice(&s.expires_unix.to_be_bytes());
    v
}

fn decode_session(v: &[u8]) -> Option<SessionInfo> {
    (v.len() == 72).then(|| SessionInfo {
        user: v[..32].try_into().expect("32"),
        device: v[32..64].try_into().expect("32"),
        expires_unix: u64::from_be_bytes(v[64..].try_into().expect("8")),
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

/// Resolve a session token against the current ACL.
pub async fn resolve(st: &Shared, token: &[u8]) -> ApiResult<Caller> {
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
    let acl = st.acl();
    if !acl.has_device(&s.user, &s.device) {
        return Err(unauthorized("device no longer in the ACL"));
    }
    Ok(Caller {
        user: s.user,
        device: s.device,
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
            .ok_or_else(|| unauthorized("missing bearer token"))?;
        let token = URL_SAFE_NO_PAD
            .decode(h.trim())
            .map_err(|_| unauthorized("bad bearer token"))?;
        resolve(st, &token).await
    }
}

/// `POST /v1/auth/challenge`. Challenges are stateless, so any node can
/// check them: `nonce(12) ‖ u32 expires_unix ‖ MAC(16)` under the cluster's
/// challenge key. Single use is enforced when a session is created.
pub async fn challenge(State(st): State<Shared>, Cbor(_): Cbor<Empty>) -> Cbor<Challenge> {
    let mut c = [0u8; 32];
    c[..12].copy_from_slice(&random32()[..12]);
    let exp = (unix_now() + CHALLENGE_TTL.as_secs()) as u32;
    c[12..16].copy_from_slice(&exp.to_be_bytes());
    let mac = challenge_mac(&st.challenge_key, &c[..16]);
    c[16..].copy_from_slice(&mac);
    Cbor(Challenge {
        challenge: c.to_vec(),
    })
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

fn origin_ok(st: &Shared, headers: &HeaderMap, origin: &str) -> bool {
    if !st.cfg.public_origins.is_empty() {
        return st.cfg.public_origins.iter().any(|o| o == origin);
    }
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    origin == format!("http://{host}") || origin == format!("https://{host}")
}

/// `POST /v1/auth/session`.
pub async fn session(
    State(st): State<Shared>,
    headers: HeaderMap,
    Cbor(req): Cbor<SessionRequest>,
) -> ApiResult<Cbor<Session>> {
    let challenge: [u8; 32] = req
        .challenge
        .as_slice()
        .try_into()
        .map_err(|_| unauthorized("bad challenge"))?;
    if !challenge_ok(&st, &challenge) {
        return Err(unauthorized("unknown or expired challenge"));
    }
    if !origin_ok(&st, &headers, &req.origin) {
        return Err(unauthorized("origin not accepted"));
    }
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
    let token = random32();
    let ttl = st.cfg.limits.session_ttl_secs;
    let info = SessionInfo {
        user: user_fp,
        device: device_fp,
        expires_unix: unix_now() + ttl,
    };
    let hash = token_hash(&token);
    let used = keys::challenge(&challenge);
    let chal_exp = u32::from_be_bytes(challenge[12..16].try_into().expect("4")) as u64;
    txn_loop!(st.store, None, |t| {
        if t.get(&used).await?.is_some() {
            return Err(unauthorized("challenge already used"));
        }
        t.set(&used, &chal_exp.to_be_bytes());
        t.set(&keys::session(&hash), &encode_session(&info));
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
        user_fp: user_fp.to_vec(),
        device_fp: device_fp.to_vec(),
    }))
}

/// `POST /v1/auth/logout`: end the caller's session on every node.
pub async fn logout(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<Empty>> {
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
