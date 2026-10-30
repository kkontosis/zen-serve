//! HTTP transport: CBOR POSTs over rustls with the pure-Rust provider
//! (zen-tls-rustcrypto, with the hybrid X25519MLKEM768 key exchange), TLS
//! 1.3 only, retried on 503 and on 429 with `Retry-After` (spec/api.md §1).

use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use zen_proto::{CBOR, ErrorBody, from_cbor, to_cbor};

/// How to verify the server.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Trust only these CA certificates (PEM) instead of the platform's roots.
    pub ca: Option<Vec<u8>>,
}

/// A connection to one server.
#[derive(Debug, Clone)]
pub struct Transport {
    http: reqwest::Client,
    base: String,
    attempts: u32,
}

/// A credential to send: a session token (sent base64url) or an API token
/// (`zen_at_…`, sent as is).
pub(crate) fn bearer_of(token: &[u8]) -> String {
    match std::str::from_utf8(token) {
        Ok(t) if t.starts_with(zen_proto::API_TOKEN_PREFIX) => t.to_string(),
        _ => URL_SAFE_NO_PAD.encode(token),
    }
}

fn tls_config(opts: &TlsOptions) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(zen_tls_rustcrypto::provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::local("tls", e.to_string()))?;
    Ok(match &opts.ca {
        Some(pem) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(pem) {
                let cert = cert.map_err(|e| Error::local("tls", format!("CA file: {e}")))?;
                roots
                    .add(cert)
                    .map_err(|e| Error::local("tls", format!("CA file: {e}")))?;
            }
            if roots.is_empty() {
                return Err(Error::local("tls", "the CA file holds no certificate"));
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => {
            let verifier = rustls_platform_verifier::Verifier::new(provider)
                .map_err(|e| Error::local("tls", e.to_string()))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
    })
}

impl Transport {
    /// A transport to `url` (`http://` or `https://`, no path).
    pub fn new(url: &str, tls: &TlsOptions) -> Result<Self> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config(tls)?)
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .map_err(|e| Error::local("tls", e.to_string()))?;
        Ok(Transport {
            http,
            base: url.trim_end_matches('/').to_string(),
            attempts: 3,
        })
    }

    /// The base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET path`, CBOR answer.
    pub async fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
        self.send(path, || self.http.get(format!("{}{path}", self.base)))
            .await
    }

    /// A CBOR POST of `req` to `path`, with `bearer` as the credential.
    pub async fn call<Q: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        bearer: Option<&str>,
        req: &Q,
    ) -> Result<R> {
        let body = to_cbor(req);
        self.send(path, || {
            let mut rb = self
                .http
                .post(format!("{}{path}", self.base))
                .header("content-type", CBOR)
                .header("accept", CBOR)
                .body(body.clone());
            if let Some(b) = bearer {
                rb = rb.header("authorization", format!("Bearer {b}"));
            }
            rb
        })
        .await
    }

    async fn send<R: DeserializeOwned>(
        &self,
        path: &str,
        make: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<R> {
        for i in 1.. {
            let resp = make()
                .send()
                .await
                .map_err(|e| Error::Network(format!("{path}: {e}")))?;
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = resp
                .bytes()
                .await
                .map_err(|e| Error::Network(format!("{path}: {e}")))?;
            if status == 200 {
                return from_cbor(&body).map_err(|e| Error::format(format!("{path}: {e}")));
            }
            let again =
                i < self.attempts && (status == 503 || (status == 429 && retry_after.is_some()));
            if !again {
                let (code, message) = match from_cbor::<ErrorBody>(&body) {
                    Ok(e) => (e.code, e.message),
                    Err(_) => (format!("http_{status}"), String::new()),
                };
                return Err(Error::Api {
                    status,
                    code,
                    message,
                    retry_after,
                });
            }
            let wait = retry_after.unwrap_or(Duration::from_millis(100 << i));
            tokio::time::sleep(wait.min(Duration::from_secs(10))).await;
        }
        unreachable!()
    }
}
