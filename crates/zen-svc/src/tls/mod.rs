//! The TLS engine (svc.md §7.1): a sans-IO TLS server whose state, once
//! the handshake is done, serializes at every record boundary, so another
//! handler can restore it and continue the live connection.
//!
//! * The handshake runs through rustls's unbuffered server on the
//!   RustCrypto provider (`zen_tls_rustcrypto::provider_tls12`): TLS 1.3
//!   and 1.2 (ECDHE-ECDSA AEAD suites, extended master secret required),
//!   no tickets, no resumption, no early data, no client certificates.
//! * Then the engine's own record layer ([`Session`]) takes over, with the
//!   keys and sequence numbers rustls hands over. For 1.3 the application
//!   traffic secrets come from rustls's key log; the keys derived from them
//!   are checked against rustls's before the switch.
//!
//! A [`Conn`] is one connection. A runtime feeds it input
//! ([`Conn::input`]), takes the plaintext ([`Conn::plaintext`],
//! [`Conn::consume`]), encrypts its output ([`Conn::write`]) and saves
//! [`Conn::state`] with each step; [`Conn::restore`] continues from it.

mod handshake;
mod session;
mod state;

pub use rustls::AlertDescription;
pub use session::{PLAIN_HELD, Session};

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::NoServerSessionStorage;
use rustls::sign::CertifiedKey;
use std::fmt;
use std::sync::Arc;

/// What [`Config::new`] may change.
#[derive(Clone, Debug)]
pub struct Options {
    /// ALPN protocols, preferred first. Empty: no ALPN.
    pub alpn: Vec<Vec<u8>>,
    /// Offer TLS 1.3 (default true).
    pub tls13: bool,
    /// Offer TLS 1.2 (default true).
    pub tls12: bool,
    /// Tests only: records per key before the confidentiality limit's
    /// action (svc.md §7.1), instead of the AEAD's.
    #[doc(hidden)]
    pub key_limit: Option<u64>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            alpn: Vec::new(),
            tls13: true,
            tls12: true,
            key_limit: None,
        }
    }
}

/// A server certificate and the TLS settings, shared by connections.
pub struct Config {
    base: Arc<ServerConfig>,
    key: Arc<CertifiedKey>,
    key_limit: Option<u64>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config").finish_non_exhaustive()
    }
}

impl Config {
    /// A certificate chain (leaf first) and its key: ECDSA P-256 or P-384,
    /// or Ed25519. RSA keys are refused (`TD-TLS-RSA-SERVER-KEY`), and so
    /// is a key that isn't the leaf's.
    pub fn new(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        opts: Options,
    ) -> Result<Config, Error> {
        let provider = Arc::new(zen_tls_rustcrypto::provider_tls12());
        if chain.is_empty() {
            return Err(Error::Config("no certificate".into()));
        }
        let signing = provider
            .key_provider
            .load_private_key(key)
            .map_err(|e| Error::Config(e.to_string()))?;
        let certified = Arc::new(CertifiedKey::new(chain, signing));
        certified
            .keys_match()
            .map_err(|e| Error::Config(e.to_string()))?;
        let mut versions = Vec::new();
        if opts.tls13 {
            versions.push(&rustls::version::TLS13);
        }
        if opts.tls12 {
            versions.push(&rustls::version::TLS12);
        }
        if versions.is_empty() {
            return Err(Error::Config("no TLS version".into()));
        }
        if opts.key_limit == Some(0) {
            return Err(Error::Config("a key limit of 0".into()));
        }
        let capture = Arc::new(handshake::Capture::default());
        let mut c = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&versions)
            .map_err(|e| Error::Config(e.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(handshake::Resolver {
                key: certified.clone(),
                capture,
            }));
        c.alpn_protocols = opts.alpn;
        c.require_ems = true;
        c.enable_secret_extraction = true;
        c.send_tls13_tickets = 0;
        c.session_storage = Arc::new(NoServerSessionStorage {});
        c.max_early_data_size = 0;
        c.send_half_rtt_data = false;
        Ok(Config {
            base: Arc::new(c),
            key: certified,
            key_limit: opts.key_limit,
        })
    }
}

/// What one [`Conn::input`] did.
#[derive(Debug, Default)]
pub struct Progress {
    /// Input bytes processed: the step's ack. After the handshake it always
    /// ends at a record boundary.
    pub consumed: usize,
    /// Bytes to send to the peer: handshake flights, alerts, KeyUpdates.
    pub out: Vec<u8>,
    /// This call finished the handshake: the connection is now resumable.
    pub established: bool,
    /// The peer sent close_notify.
    pub peer_closed: bool,
    /// The input ended (`eof`) without close_notify.
    pub truncated: bool,
    /// The connection failed; any alert for the peer is in `out`. The
    /// runtime closes the connection after sending `out`.
    pub error: Option<Error>,
}

/// Why the engine refused or ended something.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A bad certificate, key or option.
    Config(String),
    /// A state that doesn't restore.
    BadState(&'static str),
    /// Application data before the handshake is done.
    Handshaking,
    /// This side sent close_notify, or the connection failed.
    Closed,
    /// TLS 1.2: the key's record limit is reached; close the connection.
    KeyLimit,
    /// Not available for this connection (KeyUpdate in 1.2).
    Unsupported(&'static str),
    /// The handshake failed in rustls.
    Tls(rustls::Error),
    /// The engine ended the connection with this fatal alert.
    AlertSent(AlertDescription),
    /// The peer ended the connection with this fatal alert.
    AlertReceived(AlertDescription),
    /// An internal check failed (the switch's key cross-check).
    Internal(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => write!(f, "TLS configuration: {e}"),
            Error::BadState(e) => write!(f, "TLS state: {e}"),
            Error::Handshaking => f.write_str("the TLS handshake is not done"),
            Error::Closed => f.write_str("the TLS connection is closed"),
            Error::KeyLimit => f.write_str("the TLS key's record limit is reached"),
            Error::Unsupported(e) => f.write_str(e),
            Error::Tls(e) => write!(f, "TLS handshake: {e}"),
            Error::AlertSent(a) => write!(f, "TLS: sent {a:?}"),
            Error::AlertReceived(a) => write!(f, "TLS: peer sent {a:?}"),
            Error::Internal(e) => write!(f, "TLS engine: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::cbor::Bad> for Error {
    fn from(b: crate::cbor::Bad) -> Error {
        Error::BadState(b.0)
    }
}

enum Inner {
    Handshake(Box<handshake::Handshake>),
    Session(Box<Session>),
    Failed,
}

/// One TLS connection, server side.
pub struct Conn {
    inner: Inner,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Handshake(_) => f.write_str("Conn(handshake)"),
            Inner::Session(s) => write!(f, "Conn({s:?})"),
            Inner::Failed => f.write_str("Conn(failed)"),
        }
    }
}

impl Conn {
    /// A new connection, before the client's first byte.
    pub fn new(config: &Config) -> Result<Conn, Error> {
        Ok(Conn {
            inner: Inner::Handshake(Box::new(handshake::Handshake::new(config)?)),
        })
    }

    /// An established connection from its [`Conn::state`].
    pub fn restore(config: &Config, state: &[u8]) -> Result<Conn, Error> {
        Ok(Conn::from(Session::restore(config, state)?))
    }

    /// Processes input from the peer. `buf` starts at the first byte not
    /// yet consumed; `eof`: the peer's input ends after it.
    pub fn input(&mut self, buf: &[u8], eof: bool) -> Progress {
        match &mut self.inner {
            Inner::Failed => Progress {
                consumed: buf.len(),
                ..Progress::default()
            },
            Inner::Session(s) => s.input(buf, eof),
            Inner::Handshake(h) => {
                let mut p = Progress::default();
                match h.input(buf, eof, &mut p) {
                    handshake::Outcome::Pending => {}
                    handshake::Outcome::Failed => self.inner = Inner::Failed,
                    handshake::Outcome::Done(mut s) => {
                        p.established = true;
                        let q = s.input(&buf[p.consumed..], eof);
                        p.consumed += q.consumed;
                        p.out.extend_from_slice(&q.out);
                        p.peer_closed = q.peer_closed;
                        p.truncated = q.truncated;
                        p.error = q.error;
                        self.inner = Inner::Session(s);
                    }
                }
                p
            }
        }
    }

    /// Whether the handshake is done (and the connection didn't fail
    /// during it).
    pub fn is_established(&self) -> bool {
        matches!(self.inner, Inner::Session(_))
    }

    /// The established session.
    pub fn session(&self) -> Option<&Session> {
        match &self.inner {
            Inner::Session(s) => Some(s),
            _ => None,
        }
    }

    /// The established session, mutably.
    pub fn session_mut(&mut self) -> Option<&mut Session> {
        match &mut self.inner {
            Inner::Session(s) => Some(s),
            _ => None,
        }
    }

    /// Decrypted application data not consumed yet.
    pub fn plaintext(&self) -> &[u8] {
        self.session().map_or(&[], Session::plaintext)
    }

    /// Consumes the first `n` bytes of [`Conn::plaintext`].
    pub fn consume(&mut self, n: usize) {
        if let Some(s) = self.session_mut() {
            s.consume(n);
        }
    }

    /// Encrypts application data ([`Session::write`]).
    pub fn write(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        match &mut self.inner {
            Inner::Session(s) => s.write(data),
            Inner::Handshake(_) => Err(Error::Handshaking),
            Inner::Failed => Err(Error::Closed),
        }
    }

    /// A KeyUpdate ([`Session::key_update`]).
    pub fn key_update(&mut self, request_peer: bool) -> Result<Vec<u8>, Error> {
        match &mut self.inner {
            Inner::Session(s) => s.key_update(request_peer),
            Inner::Handshake(_) => Err(Error::Handshaking),
            Inner::Failed => Err(Error::Closed),
        }
    }

    /// close_notify, once established; nothing during the handshake (the
    /// runtime then just closes the socket).
    pub fn close(&mut self) -> Vec<u8> {
        match &mut self.inner {
            Inner::Session(s) => s.close(),
            _ => Vec::new(),
        }
    }

    /// The state to save with the step: `None` until the handshake is done,
    /// and after it failed.
    pub fn state(&self) -> Option<Vec<u8>> {
        self.session().map(Session::state)
    }
}

impl From<Session> for Conn {
    fn from(s: Session) -> Conn {
        Conn {
            inner: Inner::Session(Box::new(s)),
        }
    }
}
