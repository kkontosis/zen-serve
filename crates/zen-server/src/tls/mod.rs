//! Native TLS on the API listener (operations.md §8): rustls, TLS 1.3
//! only, and optional client certificates for sign-in method 5 (auth.md
//! §10).
//!
//! * The crypto provider ([`provider`]): ring's, with our post-quantum
//!   hybrid key exchange added ([`ring`], the `ring` feature, on by
//!   default), or the pure-Rust one on RustCrypto ([`rustcrypto`], built
//!   with `--no-default-features`).
//! * `[tls]` absent: plain HTTP, as before.
//! * `[tls] cert` and `key`: the listener speaks TLS only.
//! * `[tls] client_ca`, with `[auth] mtls` on: the server asks for a client
//!   certificate and verifies it against that CA, but doesn't require one,
//!   so browsers and the other sign-in methods work on the same port.
//!
//! Handshakes run in their own tasks, with a timeout, so a slow client
//! can't hold up the others. Each connection's verified client
//! certificate reaches the handlers as [`Peer::cert`].

#[cfg(feature = "ring")]
pub mod ring;
pub mod rustcrypto;

use crate::config::TlsConfig;
use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// The crypto provider of this build: [`ring::provider`] with the `ring`
/// feature, otherwise [`rustcrypto::provider`].
pub fn provider() -> rustls::crypto::CryptoProvider {
    #[cfg(feature = "ring")]
    return ring::provider();
    #[cfg(not(feature = "ring"))]
    return rustcrypto::provider();
}

/// The name of [`provider`], for the start-up log.
pub const PROVIDER: &str = if cfg!(feature = "ring") {
    "ring, with the X25519MLKEM768 hybrid on RustCrypto"
} else {
    "RustCrypto (pure Rust)"
};

/// Make [`provider`] the process-wide default of rustls, for code that
/// builds TLS configurations without naming one (HTTP clients in tests and
/// tools). Does nothing if a default is installed already.
pub fn install_default() {
    let _ = provider().install_default();
}

/// How long a TLS handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Handshakes in progress at once; further connections wait in the
/// kernel's accept queue.
const MAX_HANDSHAKES: usize = 1024;

/// What the handlers know about a connection.
#[derive(Clone, Debug)]
pub struct Peer {
    /// The remote address: the client, or a reverse proxy.
    pub addr: SocketAddr,
    /// The client certificate the TLS handshake verified against
    /// `[tls] client_ca` (DER, the end-entity certificate). Always `None`
    /// on plain HTTP.
    pub cert: Option<Arc<CertificateDer<'static>>>,
}

impl Connected<IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Peer {
            addr: *stream.remote_addr(),
            cert: None,
        }
    }
}

impl Connected<IncomingStream<'_, TlsListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        let (_, conn) = stream.io().get_ref();
        Peer {
            addr: *stream.remote_addr(),
            cert: conn
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| Arc::new(c.clone().into_owned())),
        }
    }
}

/// Whether the listener asks for client certificates: native mTLS
/// (auth.md §10.1).
pub fn requests_client_certs(tls: Option<&TlsConfig>, mtls_on: bool) -> bool {
    mtls_on && tls.is_some_and(|t| t.client_ca.is_some())
}

fn certs(path: &std::path::Path, what: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("tls.{what}: {}: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "tls.{what}: {}: no PEM certificate in it",
            path.display()
        ));
    }
    Ok(certs)
}

/// The rustls server configuration for `[tls]`. Client certificates are
/// requested when `client_certs` is set ([`requests_client_certs`]).
pub fn server_config(t: &TlsConfig, client_certs: bool) -> Result<Arc<ServerConfig>, String> {
    let provider = Arc::new(provider());
    let chain = certs(&t.cert, "cert")?;
    let key = PrivateKeyDer::from_pem_file(&t.key)
        .map_err(|e| format!("tls.key: {}: {e}", t.key.display()))?;
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("tls: {e}"))?;
    let builder = match (&t.client_ca, client_certs) {
        (Some(ca), true) => {
            let mut roots = RootCertStore::empty();
            for c in certs(ca, "client_ca")? {
                roots
                    .add(c)
                    .map_err(|e| format!("tls.client_ca: {}: {e}", ca.display()))?;
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .allow_unauthenticated()
                .build()
                .map_err(|e| format!("tls.client_ca: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        _ => builder.with_no_client_auth(),
    };
    let mut cfg = builder
        .with_single_cert(chain, key)
        .map_err(|e| format!("tls.cert / tls.key: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// A TCP listener that completes TLS handshakes in the background and
/// hands out established connections.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TlsListener {
    /// Accept on `tcp` with `cfg`.
    pub fn new(tcp: TcpListener, cfg: Arc<ServerConfig>) -> std::io::Result<Self> {
        let local = tcp.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = TlsAcceptor::from(cfg);
        let slots = Arc::new(Semaphore::new(MAX_HANDSHAKES));
        let task = tokio::spawn(async move {
            loop {
                let Ok(slot) = slots.clone().acquire_owned().await else {
                    return;
                };
                let (tcp_stream, addr) = match tcp.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // As axum does: back off on errors such as EMFILE.
                        tracing::debug!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    let _slot = slot;
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp_stream)).await
                    {
                        Ok(Ok(s)) => {
                            let _ = tx.send((s, addr)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%addr, error = %e, "TLS handshake failed"),
                        Err(_) => tracing::debug!(%addr, "TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(TlsListener { rx, local, task })
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The accept task only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The certificate in `bytes`: DER (a SEQUENCE), or the first
/// certificate of a PEM text.
pub fn parse_cert(bytes: &[u8]) -> Option<CertificateDer<'static>> {
    if bytes.first() == Some(&0x30) {
        Some(CertificateDer::from(bytes.to_vec()))
    } else {
        CertificateDer::from_pem_slice(bytes).ok()
    }
}

/// SHA-256 of a certificate's DER `SubjectPublicKeyInfo`: the key's
/// fingerprint, the same across renewals with the same key (the
/// `pin-sha256` of RFC 7469, unencoded). `None` if `cert` doesn't parse.
pub fn spki_sha256(cert: &CertificateDer<'_>) -> Option<[u8; 32]> {
    use sha2::Digest;
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    Some(sha2::Sha256::digest(ee.subject_public_key_info().as_ref()).into())
}
