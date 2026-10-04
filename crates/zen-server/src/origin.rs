//! The origin policy (auth.md §5): which origins a sign-in signature may
//! name, the first-contact pin, and the `/v1/admin/origins/*` endpoints.
//!
//! Three sources make up the accepted set, as a union:
//! * 7a `public_origins` in the config;
//! * 7b the origin pinned at first contact (`[auth] origin_pinning`), only
//!   while 7a is empty unless `origin_pinning_always` is set;
//! * 7c the `origins` of the head ACL (`[auth] acl_origins`).
//!
//! An origin outside the set is accepted from the `Host` header only while
//! nothing pins it down: before the first pin (which it then becomes), or
//! when every source is empty and pinning is off.

use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::config::Config;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use zen_proto::{Empty, OriginPins, OriginState, from_cbor, to_cbor, valid_origin};
use zen_store::Txn;

/// Max pinned origins.
pub const MAX_PINS: usize = 16;

/// Whether first-contact pinning (7b) is in force: it is on, and either
/// `public_origins` is empty or `origin_pinning_always` keeps it alongside.
pub fn pinning_active(cfg: &Config) -> bool {
    cfg.auth.origin_pinning && (cfg.public_origins.is_empty() || cfg.auth.origin_pinning_always)
}

/// The origin a client signed and what the request's `Host` header says.
pub struct SignedOrigin {
    /// The origin in the signed message.
    pub origin: String,
    /// It is `http://<Host>` or `https://<Host>`.
    pub host_match: bool,
}

impl SignedOrigin {
    /// 401 unless `origin` is a well-formed origin.
    pub fn new(headers: &HeaderMap, origin: &str) -> ApiResult<Self> {
        if !valid_origin(origin) {
            return Err(unauthorized("malformed origin"));
        }
        let host_match = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|host| {
                origin == format!("http://{host}") || origin == format!("https://{host}")
            });
        Ok(SignedOrigin {
            origin: origin.to_owned(),
            host_match,
        })
    }
}

/// What to do with a sign-in's origin.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Accept it.
    Accept,
    /// Accept it, and pin it: nothing is pinned yet.
    AcceptAndPin,
    /// Refuse the sign-in.
    Reject,
}

/// The explicit origins: 7a, then the pins when 7b is active, then 7c.
pub fn explicit(cfg: &Config, acl_origins: &[String], pins: &[String]) -> Vec<String> {
    let mut out: Vec<String> = cfg.public_origins.clone();
    let pinned = if pinning_active(cfg) { pins } else { &[] };
    let acl = if cfg.auth.acl_origins {
        acl_origins
    } else {
        &[]
    };
    for o in pinned.iter().chain(acl) {
        if !out.contains(o) {
            out.push(o.clone());
        }
    }
    out
}

/// Whether an origin outside the explicit set is accepted from the `Host`
/// header right now: the relay protection of api.md §3.3 is then absent.
pub fn host_fallback(cfg: &Config, acl_origins: &[String], pins: &[String]) -> bool {
    if pinning_active(cfg) {
        pins.is_empty()
    } else {
        explicit(cfg, acl_origins, pins).is_empty()
    }
}

/// The policy (auth.md §5.4), given the head ACL's origins and the pins.
pub fn decide(cfg: &Config, acl_origins: &[String], pins: &[String], o: &SignedOrigin) -> Verdict {
    let pin_now = pinning_active(cfg) && pins.is_empty();
    let ok = explicit(cfg, acl_origins, pins).contains(&o.origin)
        || (o.host_match && host_fallback(cfg, acl_origins, pins));
    match (ok, pin_now) {
        (false, _) => Verdict::Reject,
        (true, true) => Verdict::AcceptAndPin,
        (true, false) => Verdict::Accept,
    }
}

/// Read the pinned origins inside a transaction (absent: none).
pub async fn read_pins(t: &mut Box<dyn Txn>) -> ApiResult<Vec<String>> {
    Ok(match t.get(&keys::origin_pins()).await? {
        Some(v) => from_cbor(&v).map_err(internal)?,
        None => Vec::new(),
    })
}

fn write_pins(t: &mut Box<dyn Txn>, pins: &[String]) {
    if pins.is_empty() {
        t.clear(&keys::origin_pins());
    } else {
        t.set(&keys::origin_pins(), &to_cbor(&pins));
    }
}

/// Check a sign-in's origin inside its session transaction, pinning it if
/// it is the first. Running in the transaction makes two first contacts
/// through different origins conflict: the retry sees the other's pin.
pub async fn check_in_txn(st: &Shared, t: &mut Box<dyn Txn>, o: &SignedOrigin) -> ApiResult<()> {
    let pins = read_pins(t).await?;
    match decide(&st.cfg, &st.acl().origins, &pins, o) {
        Verdict::Reject => Err(unauthorized("origin not accepted")),
        Verdict::Accept => Ok(()),
        Verdict::AcceptAndPin => {
            write_pins(t, std::slice::from_ref(&o.origin));
            tracing::info!(origin = %o.origin, "pinned the first sign-in origin");
            Ok(())
        }
    }
}

/// Pin the origin a claim names (`/v1/acl/put` version 1), when pinning
/// is active and nothing is pinned yet; otherwise leave the pins alone.
pub async fn pin_at_claim(cfg: &Config, t: &mut Box<dyn Txn>, origin: &str) -> ApiResult<()> {
    if !pinning_active(cfg) {
        return Ok(());
    }
    if read_pins(t).await?.is_empty() {
        write_pins(t, &[origin.to_owned()]);
    }
    Ok(())
}

/// The origins this server considers its own (auth.md §5.5), and whether
/// the `Host` fallback is open. `origins[0]`, if any, is the canonical
/// origin: the first of 7a, else the pin, else the first ACL origin.
/// Passkeys derive their relying-party id from it.
pub struct OwnOrigins {
    /// Explicit origins, canonical first.
    pub origins: Vec<String>,
    /// Origins outside the list are accepted from the `Host` header.
    pub host_fallback: bool,
    /// First-contact pinning is in force.
    pub pinning: bool,
}

impl OwnOrigins {
    /// The canonical origin.
    pub fn primary(&self) -> Option<&str> {
        self.origins.first().map(String::as_str)
    }

    /// The host of the canonical origin, without scheme or port: the
    /// WebAuthn relying-party id.
    pub fn rp_id(&self) -> Option<&str> {
        let rest = self.primary()?.split_once("://")?.1;
        Some(match rest.strip_prefix('[') {
            Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
            None => rest.split_once(':').map_or(rest, |(h, _)| h),
        })
    }
}

/// The server's own origins, read from storage.
pub async fn own_origins(st: &Shared) -> ApiResult<OwnOrigins> {
    let pins = stored_pins(st).await?;
    let acl = st.acl();
    Ok(OwnOrigins {
        origins: explicit(&st.cfg, &acl.origins, &pins),
        host_fallback: host_fallback(&st.cfg, &acl.origins, &pins),
        pinning: pinning_active(&st.cfg),
    })
}

async fn stored_pins(st: &Shared) -> ApiResult<Vec<String>> {
    let (pins, _) = txn_loop!(st.store, None, |t| { read_pins(&mut t).await })?;
    Ok(pins)
}

fn check_list(origins: &[String], max: usize) -> ApiResult<()> {
    if origins.len() > max {
        return Err(bad_request(format!("at most {max} origins")));
    }
    for (i, o) in origins.iter().enumerate() {
        if !valid_origin(o) {
            return Err(bad_request(format!("invalid origin {o:?}")));
        }
        if origins[..i].contains(o) {
            return Err(bad_request(format!("duplicate origin {o:?}")));
        }
    }
    Ok(())
}

/// 400 unless `o` is a valid origin (the claim's `origin`).
pub fn check_origin(o: &str) -> ApiResult<()> {
    check_list(std::slice::from_ref(&o.to_owned()), 1)
}

/// The multi-line start-up warning when `public_origins` is empty
/// (auth.md §5.1), or `None`.
pub fn startup_warning(cfg: &Config) -> Option<String> {
    if !cfg.public_origins.is_empty() {
        return None;
    }
    let rule = "=".repeat(78);
    let pinning = if pinning_active(cfg) {
        "  First-contact pinning is on: the first origin that signs in after the\n  \
         claim is pinned, and only pinned origins are accepted after that. Until\n  \
         then the Host header is trusted, and if that first sign-in came through a\n  \
         relay, the relay's origin is what gets pinned. Check the pinned origin\n  \
         with POST /v1/admin/origins/get.\n"
    } else {
        "  First-contact pinning is OFF: every sign-in trusts the Host header, so\n  \
         there is NO protection against relayed sign-ins.\n"
    };
    Some(format!(
        "\n{rule}\n  \
         WARNING: public_origins is empty. Sign-in relay protection is weak.\n\
         {rule}\n  \
         Sign-in signatures bind the origin the client sees (scheme://host[:port]),\n  \
         so that a malicious server can't relay a sign-in to this one. Without\n  \
         public_origins this server takes its origin from the request's Host\n  \
         header, which a relaying server chooses itself.\n\n\
         {pinning}\n  \
         Fix: list the origins clients use, in the config file:\n\n      \
         public_origins = [\"https://zen.example.org\"]\n\n  \
         See spec/auth.md §5 and spec/api.md §3.3.\n\
         {rule}\n"
    ))
}

/// `POST /v1/admin/origins/get`.
pub async fn admin_get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<OriginState>> {
    caller.require_admin()?;
    let pins = stored_pins(&st).await?;
    let acl = st.acl();
    Ok(Cbor(OriginState {
        public_origins: st.cfg.public_origins.clone(),
        pinned: pins.clone(),
        acl_origins: acl.origins.clone(),
        pinning: pinning_active(&st.cfg),
        pinning_always: st.cfg.auth.origin_pinning_always,
        acl: st.cfg.auth.acl_origins,
        accepted: explicit(&st.cfg, &acl.origins, &pins),
        host_fallback: host_fallback(&st.cfg, &acl.origins, &pins),
    }))
}

/// `POST /v1/admin/origins/set`: replace the pinned set. An empty set
/// unpins: the next sign-in pins again.
pub async fn admin_set(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<OriginPins>,
) -> ApiResult<Cbor<Empty>> {
    caller.require_admin()?;
    check_list(&req.pinned, MAX_PINS)?;
    txn_loop!(st.store, None, |t| {
        write_pins(&mut t, &req.pinned);
        Ok(())
    })?;
    tracing::info!(pinned = ?req.pinned, "pinned origins replaced");
    Ok(Cbor(Empty {}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "https://a.example";
    const P: &str = "https://pinned.example";
    const L: &str = "https://acl.example";
    const H: &str = "http://host.example";

    fn cfg(public: &[&str], pinning: bool, always: bool, acl: bool) -> Config {
        let mut c = Config::with_data_dir("/tmp/unused".into());
        c.public_origins = public.iter().map(|s| s.to_string()).collect();
        c.auth.origin_pinning = pinning;
        c.auth.origin_pinning_always = always;
        c.auth.acl_origins = acl;
        c
    }

    fn v(c: &Config, acl: &[&str], pins: &[&str], origin: &str, host_match: bool) -> Verdict {
        let acl: Vec<String> = acl.iter().map(|s| s.to_string()).collect();
        let pins: Vec<String> = pins.iter().map(|s| s.to_string()).collect();
        let o = SignedOrigin {
            origin: origin.into(),
            host_match,
        };
        decide(c, &acl, &pins, &o)
    }

    #[test]
    fn policy_table() {
        use Verdict::*;
        // No 7a, pinning on (the default): Host once, then pins only.
        let c = cfg(&[], true, false, false);
        assert_eq!(v(&c, &[], &[], H, true), AcceptAndPin);
        assert_eq!(v(&c, &[], &[], H, false), Reject);
        assert_eq!(v(&c, &[], &[P], P, false), Accept);
        assert_eq!(v(&c, &[], &[P], H, true), Reject);
        // Pinning off, nothing explicit: the development Host fallback.
        let c = cfg(&[], false, false, false);
        assert_eq!(v(&c, &[], &[P], H, true), Accept);
        assert_eq!(v(&c, &[], &[P], P, false), Reject);
        // 7a turns 7b off: pins are ignored, Host never counts.
        let c = cfg(&[A], true, false, false);
        assert_eq!(v(&c, &[], &[], A, false), Accept);
        assert_eq!(v(&c, &[], &[], H, true), Reject);
        assert_eq!(v(&c, &[], &[P], P, false), Reject);
        // 7a with "always": the union, and the first contact pins.
        let c = cfg(&[A], true, true, false);
        assert_eq!(v(&c, &[], &[], H, true), AcceptAndPin);
        assert_eq!(v(&c, &[], &[], A, false), AcceptAndPin);
        assert_eq!(v(&c, &[], &[P], A, false), Accept);
        assert_eq!(v(&c, &[], &[P], P, false), Accept);
        assert_eq!(v(&c, &[], &[P], H, true), Reject);
        // 7c: ignored while off; while on, part of the union and it closes
        // the Host fallback when pinning is off.
        let c = cfg(&[], false, false, false);
        assert_eq!(v(&c, &[L], &[], L, false), Reject);
        let c = cfg(&[], false, false, true);
        assert_eq!(v(&c, &[L], &[], L, false), Accept);
        assert_eq!(v(&c, &[L], &[], H, true), Reject);
        assert_eq!(v(&c, &[], &[], H, true), Accept);
        let c = cfg(&[A], true, false, true);
        assert_eq!(v(&c, &[L], &[], L, false), Accept);
        assert_eq!(v(&c, &[L], &[], A, false), Accept);
        let c = cfg(&[], true, false, true);
        assert_eq!(v(&c, &[L], &[P], L, false), Accept);
        assert_eq!(v(&c, &[L], &[P], P, false), Accept);
        assert_eq!(v(&c, &[L], &[], L, false), AcceptAndPin);
    }

    #[test]
    fn canonical_origin_and_rp_id() {
        let own = |o: &[&str]| OwnOrigins {
            origins: o.iter().map(|s| s.to_string()).collect(),
            host_fallback: false,
            pinning: false,
        };
        assert_eq!(own(&[]).rp_id(), None);
        let o = own(&["https://zen.example.org:8443", A]);
        assert_eq!(o.primary(), Some("https://zen.example.org:8443"));
        assert_eq!(o.rp_id(), Some("zen.example.org"));
        assert_eq!(own(&["http://[::1]:80"]).rp_id(), Some("::1"));
        assert_eq!(own(&["https://a.example"]).rp_id(), Some("a.example"));
        // Precedence: 7a, then the pins, then the ACL.
        let c = cfg(&[A], true, true, true);
        assert_eq!(
            explicit(&c, &[L.into()], &[P.into()]),
            vec![A.to_string(), P.into(), L.into()]
        );
    }
}
