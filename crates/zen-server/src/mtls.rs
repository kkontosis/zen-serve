//! Sign-in method 5, TLS client certificates (auth.md §10).
//!
//! * **Native** (§10.1): `[tls] client_ca` makes the listener ask for a
//!   client certificate and verify it against that CA ([`crate::tls`]).
//! * **Trusted proxy** (§10.2): a reverse proxy at one of
//!   `[auth] mtls_trusted_proxies` verifies the certificate and forwards
//!   it in `[auth] mtls_proxy_header`. The header is read from those
//!   addresses only; from any other it is refused.
//! * Either way the certificate must also be **registered**: bound to a
//!   member as a credential whose id is the SHA-256 of its public key
//!   (`SubjectPublicKeyInfo`), so a renewal with the same key keeps
//!   working (§10.3).
//! * Signing in, `POST /v1/auth/mtls/session`, turns the connection's
//!   certificate into an ordinary session (§10.4).
//!
//! The method is on by default but **dormant** until one of the two modes
//! is configured: it is then not offered in `/v1/info`, and sign-in gets
//! 401.

use crate::acl::Fp;
use crate::auth::{Caller, issue_session};
use crate::cbor::Cbor;
use crate::config::Config;
use crate::cred::{self, CredRecord};
use crate::error::*;
use crate::ids::unix_now;
use crate::state::Shared;
use crate::tls::{self, Peer};
use crate::txn::txn_loop;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use base64::Engine;
use rustls::pki_types::CertificateDer;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use zen_proto::{AuthMethod, CredentialId, Empty, MtlsRegister, Session};

const METHOD: AuthMethod = AuthMethod::Mtls;

/// Max label length.
const MAX_LABEL: usize = 128;

/// An IP address block from `[auth] mtls_trusted_proxies`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// `"10.0.0.5"`, `"10.0.0.0/8"`, `"::1"` or `"fd00::/8"`. The address
    /// must have no bits set past the prefix.
    pub fn parse(s: &str) -> Option<Cidr> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        // "::ffff:10.0.0.0/104" would be IPv4 with a prefix of 104: refused.
        let addr = a.parse::<IpAddr>().ok()?.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            None => max,
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok()?,
            Some(_) => return None,
        };
        if prefix > max {
            return None;
        }
        let (bits, width) = Self::bits(addr);
        let host = width - u32::from(prefix);
        // No host bits: "10.0.0.1/8" is a typo, not a block.
        let clean = host >= 128 || bits & ((1u128 << host) - 1) == 0;
        clean.then_some(Cidr { addr, prefix })
    }

    /// The address as a number, and its width in bits. An IPv4-mapped
    /// IPv6 address (a dual-stack listener) counts as its IPv4 address.
    fn bits(ip: IpAddr) -> (u128, u32) {
        match ip.to_canonical() {
            IpAddr::V4(a) => (u128::from(u32::from(a)), 32),
            IpAddr::V6(a) => (u128::from(a), 128),
        }
    }

    /// Whether `ip` is in the block. An IPv4-mapped IPv6 address (a
    /// dual-stack listener) counts as its IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ((net, n), (got, m)) = (Self::bits(self.addr), Self::bits(ip));
        if n != m {
            return false;
        }
        let shift = n - u32::from(self.prefix);
        shift >= 128 || net >> shift == got >> shift
    }
}

/// Native mTLS: the listener verifies client certificates (§10.1).
pub fn native(cfg: &Config) -> bool {
    tls::requests_client_certs(cfg.tls.as_ref(), cfg.auth.mtls)
}

/// Proxy mode: some reverse proxy is trusted to forward certificates.
pub fn proxy(cfg: &Config) -> bool {
    !cfg.auth.mtls_trusted_proxies.is_empty()
}

/// Whether a client certificate can reach the server at all: without
/// either mode the method is dormant.
pub fn configured(cfg: &Config) -> bool {
    native(cfg) || proxy(cfg)
}

/// The start-up note about method 5, logged once.
pub fn startup_note(cfg: &Config) -> Option<String> {
    if !cfg.auth.mtls {
        return cfg
            .tls
            .as_ref()
            .and_then(|t| t.client_ca.as_ref())
            .map(|_| {
                "tls.client_ca is set but [auth] mtls is off: client certificates are not \
                 requested"
                    .to_string()
            });
    }
    Some(match (native(cfg), proxy(cfg)) {
        (false, false) => "mTLS sign-in (method 5) is on but dormant: set [tls] client_ca for \
                           native TLS, or [auth] mtls_trusted_proxies for a TLS-terminating \
                           proxy (spec/auth.md §10). /v1/info doesn't offer it until then"
            .to_string(),
        (n, p) => format!(
            "mTLS sign-in (method 5): native {}, trusted proxy {}",
            if n { "on" } else { "off" },
            if p { "on" } else { "off" }
        ),
    })
}

fn trusted_proxy(cfg: &Config, ip: IpAddr) -> bool {
    cfg.auth
        .mtls_trusted_proxies
        .iter()
        .filter_map(|c| Cidr::parse(c))
        .any(|c| c.contains(ip))
}

/// The connection a request came on, if the server recorded it.
pub struct Conn(pub Option<Peer>);

impl<S: Send + Sync> FromRequestParts<S> for Conn {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Conn(
            parts
                .extensions
                .get::<ConnectInfo<Peer>>()
                .map(|c| c.0.clone()),
        ))
    }
}

/// Decode `%XX` escapes.
fn percent_decode(s: &[u8]) -> Option<Vec<u8>> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            out.push(hex(*s.get(i + 1)?)? << 4 | hex(*s.get(i + 2)?)?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The certificate in a proxy header (§10.2): URL-escaped PEM, as nginx's
/// `$ssl_client_escaped_cert`, or base64 DER, as Caddy, HAProxy and
/// Traefik can send (also URL-escaped). Of a chain, the first certificate
/// counts: the first PEM block, or the base64 text up to the first comma.
/// Whitespace is ignored. `None` if it doesn't parse.
pub fn parse_header(v: &[u8]) -> Option<CertificateDer<'static>> {
    let v = percent_decode(v)?;
    let text = std::str::from_utf8(&v).ok()?.trim();
    if text.starts_with("-----BEGIN") {
        return tls::parse_cert(text.as_bytes());
    }
    let first = text.split(',').next().unwrap_or_default();
    let b64: String = first.chars().filter(|c| !c.is_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    (der.first() == Some(&0x30)).then(|| CertificateDer::from(der))
}

/// Unix second of the last "header from an untrusted address" warning:
/// at most one a minute, so the log can't be flooded.
static LAST_WARNING: AtomicU64 = AtomicU64::new(0);

/// The client certificate of a request (§10.2):
/// * from a trusted proxy: the header, and nothing else (the proxy's own
///   TLS certificate, if any, is not the user's);
/// * from any other address: the certificate the TLS handshake verified,
///   if any. A request that carries the header is refused (401) while
///   proxy mode is configured, and logged.
pub fn presented(
    st: &Shared,
    headers: &HeaderMap,
    peer: Option<&Peer>,
) -> ApiResult<Option<CertificateDer<'static>>> {
    let Some(peer) = peer else {
        return Ok(None);
    };
    let header = headers.get(st.cfg.auth.mtls_proxy_header.as_str());
    if proxy(&st.cfg) {
        if trusted_proxy(&st.cfg, peer.addr.ip()) {
            return match header {
                // Some proxies send the header empty when there is no
                // certificate.
                None => Ok(None),
                Some(v) if v.as_bytes().trim_ascii().is_empty() => Ok(None),
                Some(v) => parse_header(v.as_bytes()).map(Some).ok_or_else(|| {
                    tracing::warn!(proxy = %peer.addr, "the trusted proxy forwarded a client \
                        certificate that doesn't parse");
                    unauthorized("the proxy's client-certificate header doesn't parse")
                }),
            };
        }
        if header.is_some() {
            let now = unix_now();
            if LAST_WARNING.swap(now, Ordering::Relaxed) + 60 <= now {
                tracing::warn!(peer = %peer.addr, header = %st.cfg.auth.mtls_proxy_header,
                    "refused a client-certificate header from an address that is not in \
                     mtls_trusted_proxies (spec/auth.md §10.2)");
            }
            return Err(unauthorized(
                "a client-certificate header from an address that is not a trusted proxy",
            ));
        }
    }
    Ok(native(&st.cfg)
        .then(|| peer.cert.as_deref().cloned())
        .flatten())
}

/// The credential id of a certificate: SHA-256 of its
/// `SubjectPublicKeyInfo` (§10.3).
pub fn cert_id(cert: &CertificateDer<'_>) -> Option<Fp> {
    tls::spki_sha256(cert)
}

/// `POST /v1/auth/mtls/session`: sign in with the connection's client
/// certificate. No session needed.
pub async fn session(
    State(st): State<Shared>,
    Conn(peer): Conn,
    headers: HeaderMap,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<Session>> {
    st.require_method(METHOD)?;
    if !configured(&st.cfg) {
        return Err(unauthorized(
            "mTLS sign-in is not set up on this server: no [tls] client_ca and no trusted \
             proxy (spec/auth.md §10)",
        ));
    }
    let cert = presented(&st, &headers, peer.as_ref())?
        .ok_or_else(|| unauthorized("no client certificate"))?;
    let id = cert_id(&cert).ok_or_else(|| unauthorized("the client certificate doesn't parse"))?;
    let acl = st.acl();
    let now = unix_now();
    let (user, _) = txn_loop!(st.store, None, |t| {
        let unknown = || unauthorized("unregistered client certificate");
        let user = cred::owner(&mut t, &id).await?.ok_or_else(unknown)?;
        let mut rec = cred::get(&mut t, &user, &id)
            .await?
            .filter(|r| r.method() == Some(METHOD))
            .ok_or_else(unknown)?;
        if !acl.members.contains_key(&user) {
            return Err(unauthorized("not a member"));
        }
        rec.last_used_unix = Some(now);
        cred::put(&mut t, &user, &id, &rec);
        Ok(user)
    })?;
    // Bound to the TLS handshake, not to a signed origin (auth.md §10.4).
    issue_session(&st, user, id, METHOD, None).await
}

/// `POST /v1/auth/mtls/register`: bind a client certificate to a member.
/// A member binds the certificate their own connection presents (proof of
/// possession); an admin may also bind an uploaded one, to any member.
pub async fn register(
    State(st): State<Shared>,
    caller: Caller,
    Conn(peer): Conn,
    headers: HeaderMap,
    Cbor(req): Cbor<MtlsRegister>,
) -> ApiResult<Cbor<CredentialId>> {
    st.require_method(METHOD)?;
    caller.require_interactive()?;
    if req.label.as_ref().is_some_and(|l| l.len() > MAX_LABEL) {
        return Err(bad_request(format!("a label is at most {MAX_LABEL} bytes")));
    }
    let user: Fp = match req.user.as_deref() {
        None => caller.user,
        Some(u) => u
            .try_into()
            .map_err(|_| bad_request("user must be 32 bytes"))?,
    };
    if user != caller.user {
        caller.require_admin()?;
    }
    if !caller.acl.members.contains_key(&user) {
        return Err(bad_request("the user is not a member"));
    }
    let cert = match req.cert.as_deref() {
        Some(bytes) => {
            caller.require_admin()?;
            tls::parse_cert(bytes)
                .ok_or_else(|| bad_request("cert is not a DER or PEM certificate"))?
        }
        None => presented(&st, &headers, peer.as_ref())?.ok_or_else(|| {
            bad_request(
                "no client certificate on this connection: connect with the certificate, or \
                 have an admin upload it",
            )
        })?,
    };
    let id = cert_id(&cert).ok_or_else(|| bad_request("cert is not an X.509 certificate"))?;
    let rec = CredRecord {
        method: METHOD.id(),
        created_unix: unix_now(),
        label: req.label.clone(),
        issued_by: (req.cert.is_some() || user != caller.user).then(|| caller.user.to_vec().into()),
        ..Default::default()
    };
    txn_loop!(st.store, None, |t| {
        if cred::owner(&mut t, &id).await?.is_some() {
            return Err(bad_request("this certificate's key is already registered"));
        }
        if cred::list(&mut t, &user).await?.len() >= cred::MAX_PER_USER {
            return Err(quota("too many credentials"));
        }
        cred::put(&mut t, &user, &id, &rec);
        Ok(())
    })?;
    tracing::info!(id = %cred::hex(&id), user = %cred::hex(&user), "client certificate registered");
    Ok(Cbor(CredentialId { id: id.to_vec() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidrs() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(c.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(!c.contains("::1".parse().unwrap()));
        let one = Cidr::parse("127.0.0.1").unwrap();
        assert!(one.contains("127.0.0.1".parse().unwrap()));
        assert!(!one.contains("127.0.0.2".parse().unwrap()));
        assert!(
            Cidr::parse("::1/128")
                .unwrap()
                .contains("::1".parse().unwrap())
        );
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("1.2.3.4".parse().unwrap())
        );
        assert!(
            Cidr::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        for bad in [
            "10.0.0.1/8",
            "::ffff:10.0.0.0/104",
            "10.0.0.0/33",
            "::/129",
            "x",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "",
        ] {
            assert_eq!(Cidr::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_startup_note_says_whether_it_is_dormant() {
        let mut cfg = Config::with_data_dir("/tmp/x".into());
        let note = startup_note(&cfg).unwrap();
        assert!(note.contains("dormant"), "{note}");
        assert!(!configured(&cfg));
        cfg.auth.mtls_trusted_proxies = vec!["10.0.0.1".into()];
        let note = startup_note(&cfg).unwrap();
        assert!(note.contains("native off, trusted proxy on"), "{note}");
        cfg.tls = Some(crate::config::TlsConfig {
            cert: "c".into(),
            key: "k".into(),
            client_ca: Some("ca".into()),
        });
        assert!(
            startup_note(&cfg)
                .unwrap()
                .contains("native on, trusted proxy on")
        );
        cfg.auth.mtls = false;
        assert!(startup_note(&cfg).unwrap().contains("mtls is off"));
        cfg.tls = None;
        assert_eq!(startup_note(&cfg), None);
    }

    #[test]
    fn proxy_headers() {
        assert_eq!(parse_header(b""), None);
        assert_eq!(parse_header(b"%%"), None);
        assert_eq!(parse_header(b"aGVsbG8="), None, "base64, but not DER");
        assert_eq!(
            parse_header(b"MAA=").map(|c| c.to_vec()),
            Some(vec![0x30, 0])
        );
        assert_eq!(
            parse_header(b"MAA%3D,AAAA").map(|c| c.to_vec()),
            Some(vec![0x30, 0])
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode(b"a%2Bb%0a").unwrap(), b"a+b\n");
        assert_eq!(percent_decode(b"%zz"), None);
        assert_eq!(percent_decode(b"%2"), None);
    }
}
