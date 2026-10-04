//! Device sign-in (api.md §3) and the [`Caller`] extractor.

use crate::acl::{AclState, Fp, R_READ};
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::{random32, unix_now};
use crate::state::{SessionInfo, Shared};
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

/// Resolve a session token against the current ACL.
pub fn resolve(st: &Shared, token: &[u8]) -> ApiResult<Caller> {
    let token: [u8; 32] = token
        .try_into()
        .map_err(|_| unauthorized("bad session token"))?;
    let s = {
        let sessions = st.sessions.lock().expect("session lock");
        sessions.get(&token).cloned()
    }
    .ok_or_else(|| unauthorized("unknown session"))?;
    if s.expires <= Instant::now() {
        return Err(unauthorized("session expired"));
    }
    let acl = st.acl();
    if !acl.has_device(&s.user, &s.device) {
        return Err(unauthorized("device no longer in the ACL"));
    }
    Ok(Caller {
        user: s.user,
        device: s.device,
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
        resolve(st, &token)
    }
}

/// `POST /v1/auth/challenge`.
pub async fn challenge(State(st): State<Shared>, Cbor(_): Cbor<Empty>) -> Cbor<Challenge> {
    let c = random32();
    let mut ch = st.challenges.lock().expect("challenge lock");
    let now = Instant::now();
    ch.retain(|_, exp| *exp > now);
    if ch.len() < 100_000 {
        ch.insert(c, now + CHALLENGE_TTL);
    }
    Cbor(Challenge {
        challenge: c.to_vec(),
    })
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
    let live = st
        .challenges
        .lock()
        .expect("challenge lock")
        .remove(&challenge)
        .is_some_and(|exp| exp > Instant::now());
    if !live {
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
    st.sessions.lock().expect("session lock").insert(
        token,
        SessionInfo {
            user: user_fp,
            device: device_fp,
            expires: Instant::now() + Duration::from_secs(ttl),
        },
    );
    Ok(Cbor(Session {
        token: token.to_vec(),
        expires_unix: unix_now() + ttl,
        user_fp: user_fp.to_vec(),
        device_fp: device_fp.to_vec(),
    }))
}
