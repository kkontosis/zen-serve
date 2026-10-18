//! The handshake: rustls's unbuffered server, fed one record at a time so
//! it never reads past the client's last handshake flight. Once rustls is
//! done, the record keys and sequence numbers move to a [`Session`].

use super::session::{Dir, Session, Suite};
use super::{Config, Error, Progress};
use rustls::server::{ClientHello, ResolvesServerCert, UnbufferedServerConnection};
use rustls::sign::CertifiedKey;
use rustls::unbuffered::{ConnectionState, EncodeError, UnbufferedStatus};
use rustls::{ConnectionTrafficSecrets, KeyLog, ProtocolVersion};
use std::sync::{Arc, Mutex};
use zen_tls_rustcrypto::record::{HEADER_LEN, MAX_CIPHER_12};
use zeroize::Zeroizing;

/// What one connection's handshake told us besides rustls's state: the 1.3
/// application traffic secrets (from the key log) and the client's server
/// name (from the certificate resolver).
#[derive(Default)]
struct Captured {
    client_secret: Option<Zeroizing<Vec<u8>>>,
    server_secret: Option<Zeroizing<Vec<u8>>>,
    sni: Option<String>,
}

#[derive(Default)]
pub(crate) struct Capture(Mutex<Captured>);

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}

impl Capture {
    fn lock(&self) -> std::sync::MutexGuard<'_, Captured> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl KeyLog for Capture {
    fn will_log(&self, label: &str) -> bool {
        matches!(label, "CLIENT_TRAFFIC_SECRET_0" | "SERVER_TRAFFIC_SECRET_0")
    }

    fn log(&self, label: &str, _client_random: &[u8], secret: &[u8]) {
        let secret = Some(Zeroizing::new(secret.to_vec()));
        let mut c = self.lock();
        match label {
            "CLIENT_TRAFFIC_SECRET_0" => c.client_secret = secret,
            "SERVER_TRAFFIC_SECRET_0" => c.server_secret = secret,
            _ => {}
        }
    }
}

/// The one certificate, remembering the client's server name.
#[derive(Debug)]
pub(crate) struct Resolver {
    pub(crate) key: Arc<CertifiedKey>,
    pub(crate) capture: Arc<Capture>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.capture.lock().sni = hello.server_name().map(str::to_owned);
        Some(self.key.clone())
    }
}

pub(crate) struct Handshake {
    conn: Option<UnbufferedServerConnection>,
    /// Records handed to rustls that it hasn't discarded yet (handshake
    /// fragments it is joining).
    buf: Vec<u8>,
    /// The record being collected from the input.
    rec: Vec<u8>,
    /// Application data rustls decrypted (none expected: early data is off).
    plain: Vec<u8>,
    capture: Arc<Capture>,
    limit: Option<u64>,
}

pub(crate) enum Outcome {
    Pending,
    Done(Box<Session>),
    Failed,
}

impl Handshake {
    pub(crate) fn new(config: &Config) -> Result<Handshake, Error> {
        let capture = Arc::new(Capture::default());
        let mut c = (*config.base).clone();
        c.key_log = capture.clone();
        c.cert_resolver = Arc::new(Resolver {
            key: config.key.clone(),
            capture: capture.clone(),
        });
        let conn = UnbufferedServerConnection::new(Arc::new(c)).map_err(Error::Tls)?;
        Ok(Handshake {
            conn: Some(conn),
            buf: Vec::new(),
            rec: Vec::new(),
            plain: Vec::new(),
            capture,
            limit: config.key_limit,
        })
    }

    /// Feeds `input` to rustls a record at a time. Every byte taken is
    /// consumed (held here or by rustls); the handshake isn't resumable.
    pub(crate) fn input(&mut self, input: &[u8], eof: bool, p: &mut Progress) -> Outcome {
        let mut i = 0;
        while i < input.len() {
            if self.rec.len() < HEADER_LEN {
                let n = (HEADER_LEN - self.rec.len()).min(input.len() - i);
                self.rec.extend_from_slice(&input[i..i + n]);
                i += n;
                if self.rec.len() < HEADER_LEN {
                    break;
                }
            }
            let typ = self.rec[0];
            let len = usize::from(u16::from_be_bytes([self.rec[3], self.rec[4]]));
            // A header rustls refuses is handed over alone, so it fails now
            // rather than after waiting for a body.
            let bad = !(20..=23).contains(&typ) || len > MAX_CIPHER_12;
            if !bad {
                let n = (HEADER_LEN + len - self.rec.len()).min(input.len() - i);
                self.rec.extend_from_slice(&input[i..i + n]);
                i += n;
                if self.rec.len() < HEADER_LEN + len {
                    break;
                }
            }
            self.buf.append(&mut self.rec);
            if self.drive(p).is_err() || bad && p.error.is_none() {
                p.consumed = i;
                if p.error.is_none() {
                    p.error = Some(Error::Internal("a malformed record was not refused"));
                }
                return Outcome::Failed;
            }
            if self.conn.as_ref().is_some_and(|c| !c.is_handshaking()) {
                p.consumed = i;
                return self.switch(p);
            }
        }
        p.consumed = i;
        if eof {
            p.truncated = true;
            p.error = Some(Error::Tls(rustls::Error::HandshakeNotComplete));
            return Outcome::Failed;
        }
        Outcome::Pending
    }

    /// Runs rustls until it needs more input or has nothing left to do.
    /// Its handshake flights and alerts go to `p.out`.
    fn drive(&mut self, p: &mut Progress) -> Result<(), ()> {
        let Some(conn) = self.conn.as_mut() else {
            return Err(());
        };
        let mut failed: Option<rustls::Error> = None;
        // After an error rustls still hands out the alert it queued, then
        // repeats the error.
        for _ in 0..64 {
            let UnbufferedStatus { discard, state } = conn.process_tls_records(&mut self.buf);
            let mut stop = false;
            match state {
                Ok(ConnectionState::EncodeTlsData(mut e)) => {
                    let mut out = vec![0u8; 2048];
                    loop {
                        match e.encode(&mut out) {
                            Ok(n) => {
                                p.out.extend_from_slice(&out[..n]);
                                break;
                            }
                            Err(EncodeError::InsufficientSize(s)) => out.resize(s.required_size, 0),
                            Err(EncodeError::AlreadyEncoded) => break,
                        }
                    }
                }
                Ok(ConnectionState::TransmitTlsData(t)) => t.done(),
                Ok(ConnectionState::ReadTraffic(mut r)) => {
                    while let Some(rec) = r.next_record() {
                        match rec {
                            Ok(rec) => self.plain.extend_from_slice(rec.payload),
                            Err(e) => {
                                failed = Some(e);
                                break;
                            }
                        }
                    }
                }
                Ok(ConnectionState::ReadEarlyData(_)) => {
                    failed = Some(rustls::Error::General("early data".into()));
                    stop = true;
                }
                Ok(ConnectionState::PeerClosed | ConnectionState::Closed) => {
                    p.peer_closed = true;
                    failed = Some(rustls::Error::HandshakeNotComplete);
                    stop = true;
                }
                Ok(ConnectionState::BlockedHandshake | ConnectionState::WriteTraffic(_)) => {
                    stop = true
                }
                Ok(_) => stop = true,
                Err(e) => {
                    if failed.is_some() {
                        stop = true;
                    } else {
                        failed = Some(e);
                    }
                }
            }
            // After an error the connection is dead: only the alert rustls
            // queued is wanted. Its input is dropped, so the next call
            // doesn't parse the bad bytes again (and alert twice).
            if failed.is_some() {
                self.buf.clear();
            } else {
                self.buf.drain(..discard);
            }
            if stop {
                break;
            }
        }
        match failed {
            None => Ok(()),
            Some(e) => {
                p.error = Some(match e {
                    rustls::Error::AlertReceived(a) => Error::AlertReceived(a),
                    e => Error::Tls(e),
                });
                self.conn = None;
                Err(())
            }
        }
    }

    /// The handshake is done: the record keys move to a [`Session`].
    fn switch(&mut self, p: &mut Progress) -> Outcome {
        match self.try_switch() {
            Ok(s) => Outcome::Done(Box::new(s)),
            Err(e) => {
                p.error = Some(e);
                Outcome::Failed
            }
        }
    }

    fn try_switch(&mut self) -> Result<Session, Error> {
        let conn = self.conn.take().ok_or(Error::Internal("no connection"))?;
        if !self.buf.is_empty() {
            return Err(Error::Internal("handshake bytes left after the handshake"));
        }
        let ver = conn
            .protocol_version()
            .ok_or(Error::Internal("no version"))?;
        let suite = conn
            .negotiated_cipher_suite()
            .ok_or(Error::Internal("no suite"))?;
        let suite = Suite::from_id(u16::from(suite.suite()), ver)
            .ok_or(Error::Internal("an unexpected suite"))?;
        let alpn = conn.alpn_protocol().map(<[u8]>::to_vec);
        let (secrets, _kernel) = conn
            .dangerous_into_kernel_connection()
            .map_err(Error::Tls)?;
        let (tx_seq, tx) = secrets.tx;
        let (rx_seq, rx) = secrets.rx;
        let mut captured = self.capture.lock();
        let (tx, rx) = if ver == ProtocolVersion::TLSv1_3 {
            let ss = captured
                .server_secret
                .take()
                .ok_or(Error::Internal("no server traffic secret"))?;
            let cs = captured
                .client_secret
                .take()
                .ok_or(Error::Internal("no client traffic secret"))?;
            let tx_dir = Dir::new13(suite, ss, tx_seq)?;
            let rx_dir = Dir::new13(suite, cs, rx_seq)?;
            // The keys derived from the logged secrets must be rustls's.
            if !tx_dir.matches(&tx) || !rx_dir.matches(&rx) {
                return Err(Error::Internal(
                    "the traffic secrets don't give rustls's keys",
                ));
            }
            (tx_dir, rx_dir)
        } else {
            let (tk, ti) = key_iv(&tx)?;
            let (rk, ri) = key_iv(&rx)?;
            (
                Dir::new12(suite, tk, ti, tx_seq)?,
                Dir::new12(suite, rk, ri, rx_seq)?,
            )
        };
        Ok(Session::new(
            suite,
            tx,
            rx,
            std::mem::take(&mut self.plain),
            captured.sni.take(),
            alpn,
            self.limit,
        ))
    }
}

fn key_iv(s: &ConnectionTrafficSecrets) -> Result<(&[u8], &[u8]), Error> {
    match s {
        ConnectionTrafficSecrets::Aes128Gcm { key, iv }
        | ConnectionTrafficSecrets::Aes256Gcm { key, iv }
        | ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => Ok((key.as_ref(), iv.as_ref())),
        _ => Err(Error::Internal("unexpected traffic secrets")),
    }
}

impl Dir {
    /// Whether this direction's key and IV are those rustls extracted.
    pub(crate) fn matches(&self, s: &ConnectionTrafficSecrets) -> bool {
        key_iv(s).is_ok_and(|(k, iv)| k == &self.key_bytes[..] && iv == &self.iv[..])
    }
}
