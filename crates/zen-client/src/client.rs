//! A server and signing in (spec/auth.md §3: device key; §4: API tokens).

use crate::device::DeviceKey;
use crate::error::{Error, Result};
use crate::session::Session;
use crate::transport::{TlsOptions, Transport};
use std::sync::Arc;
use zen_core::labels;
use zen_proto::{Challenge, Empty, Info, SessionRequest, session_message, valid_origin};

/// Options of [`Client::connect`].
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    /// The origin signed at sign-in (default: the URL's), as the server's
    /// origin policy expects it (spec/auth.md §2).
    pub origin: Option<String>,
    /// How to verify the server's certificate.
    pub tls: TlsOptions,
}

/// A server.
#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) transport: Arc<Transport>,
    origin: String,
}

impl Client {
    /// A client of the server at `url`.
    pub fn connect(url: &str, opts: ClientOptions) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(url).map_err(|e| Error::local("param", format!("{url}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::local("param", format!("{url}: not http(s)")));
        }
        let origin = opts
            .origin
            .unwrap_or_else(|| parsed.origin().ascii_serialization());
        if !valid_origin(&origin) {
            return Err(Error::local("param", format!("{origin}: not an origin")));
        }
        Ok(Client {
            transport: Arc::new(Transport::new(url, &opts.tls)?),
            origin,
        })
    }

    /// The origin this client signs.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `GET /v1/info`.
    pub async fn info(&self) -> Result<Info> {
        self.transport.get("/v1/info").await
    }

    /// Sign in with a device key (method 1): sign the server's challenge and
    /// this origin. The session signs in again on its own when it expires.
    pub async fn sign_in_device(&self, key: &DeviceKey) -> Result<Session> {
        let s = self.device_session(key).await?;
        Session::new(self.clone(), s.token, s.device_fp, Some(key.clone())).await
    }

    pub(crate) async fn device_session(&self, key: &DeviceKey) -> Result<zen_proto::Session> {
        let c: Challenge = self
            .transport
            .call("/v1/auth/challenge", None, &Empty {})
            .await?;
        let sig = key.device.signing().sign(
            labels::SIG_SESSION,
            &session_message(&c.challenge, &self.origin),
        )?;
        self.transport
            .call(
                "/v1/auth/session",
                None,
                &SessionRequest {
                    challenge: c.challenge,
                    origin: self.origin.clone(),
                    user: key.identity.public().encode(),
                    cert: key.cert.clone(),
                    sig,
                },
            )
            .await
    }

    /// A session from a token: a session token from an earlier sign-in, or
    /// an API token (`zen_at_…`, as UTF-8 bytes). It can't sign in again.
    pub async fn session_from_token(&self, token: Vec<u8>) -> Result<Session> {
        Session::new(self.clone(), token, Vec::new(), None).await
    }
}
