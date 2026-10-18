//! A harness that drives the TLS engine the way a handler does: after
//! every input it saves the state, drops the connection and restores it,
//! so every record boundary is a takeover point.
#![allow(dead_code)]

use rustls::crypto::SupportedKxGroup;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, KeyLog, SupportedCipherSuite};
use std::io::Read;
use std::sync::{Arc, Mutex};
use zen_svc::tls::{Config, Conn, Error, Options};
use zen_tls_rustcrypto::testing::{KeyKind, TestCert, server_cert};

#[allow(unused_imports)]
pub use zen_tls_rustcrypto::{
    SECP256R1, TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS13_AES_128_GCM_SHA256,
    TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256, X25519, X25519MLKEM768,
};

pub const SUITES13: [SupportedCipherSuite; 3] = [
    TLS13_AES_128_GCM_SHA256,
    TLS13_AES_256_GCM_SHA384,
    TLS13_CHACHA20_POLY1305_SHA256,
];

pub const SUITES12: [SupportedCipherSuite; 3] = [
    TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
];

/// An engine configuration and the CA its certificate chains to.
pub fn server(kind: KeyKind, opts: Options) -> (Arc<Config>, TestCert) {
    let cert = server_cert(kind);
    let config = Config::new(cert.chain.clone(), cert.key.clone_key(), opts).unwrap();
    (Arc::new(config), cert)
}

/// The client's view of the traffic secrets (its key log).
#[derive(Debug, Default)]
pub struct Secrets(pub Mutex<Vec<(String, Vec<u8>)>>);

impl Secrets {
    pub fn get(&self, label: &str) -> Vec<u8> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| panic!("no {label}"))
    }
}

impl KeyLog for Secrets {
    fn log(&self, label: &str, _client_random: &[u8], secret: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push((label.to_string(), secret.to_vec()));
    }
}

/// A rustls client on the same provider, limited to one suite and group
/// when given.
pub fn client(
    ca: &CertificateDer<'static>,
    suite: Option<SupportedCipherSuite>,
    group: Option<&'static dyn SupportedKxGroup>,
) -> (ClientConfig, Arc<Secrets>) {
    let mut p = zen_tls_rustcrypto::provider_tls12();
    if let Some(s) = suite {
        p.cipher_suites = vec![s];
    }
    if let Some(g) = group {
        p.kx_groups = vec![g];
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let mut c = ClientConfig::builder_with_provider(Arc::new(p))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let secrets = Arc::new(Secrets::default());
    c.key_log = secrets.clone();
    (c, secrets)
}

/// How the client's bytes reach the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// What the client wrote, at once.
    Whole,
    /// One record per input call.
    Records,
    /// One byte per input call.
    Bytes,
}

/// Splits TLS bytes at record boundaries (a partial last record stays
/// whole).
pub fn records(mut buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while buf.len() >= 5 {
        let n = 5 + usize::from(u16::from_be_bytes([buf[3], buf[4]]));
        if n > buf.len() {
            break;
        }
        out.push(&buf[..n]);
        buf = &buf[n..];
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// The engine's side: its connection, the input not yet consumed (which a
/// relay keeps), and what it decrypted.
pub struct Server {
    pub config: Arc<Config>,
    pub conn: Conn,
    pub pending: Vec<u8>,
    pub received: Vec<u8>,
    /// Echo plaintext back as it arrives.
    pub echo: bool,
    /// Consume plaintext as it arrives.
    pub consume: bool,
    pub restores: usize,
    pub established: usize,
    pub errors: Vec<Error>,
    pub peer_closed: bool,
    pub truncated: bool,
}

impl Server {
    pub fn new(config: Arc<Config>) -> Server {
        let conn = Conn::new(&config).unwrap();
        Server {
            config,
            conn,
            pending: Vec::new(),
            received: Vec::new(),
            echo: true,
            consume: true,
            restores: 0,
            established: 0,
            errors: Vec::new(),
            peer_closed: false,
            truncated: false,
        }
    }

    /// Delivers bytes from the client; returns what to send back. Input
    /// runs again after the plaintext is consumed, since the engine stops
    /// decrypting at [`zen_svc::tls::PLAIN_HELD`].
    pub fn feed(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let p = self.conn.input(&self.pending, eof);
            assert!(p.consumed <= self.pending.len());
            self.pending.drain(..p.consumed);
            out.extend(p.out);
            if p.established {
                self.established += 1;
            }
            self.peer_closed |= p.peer_closed;
            self.truncated = p.truncated;
            let failed = p.error.is_some();
            if let Some(e) = p.error {
                self.errors.push(e);
            }
            let mut took = 0;
            if self.consume {
                let plain = self.conn.plaintext().to_vec();
                took = plain.len();
                self.conn.consume(plain.len());
                let open = self.conn.session().is_some_and(|s| !s.write_closed());
                if self.echo && open && !plain.is_empty() {
                    out.extend(self.conn.write(&plain).unwrap());
                }
                self.received.extend(plain);
            }
            self.takeover();
            if failed || p.consumed == 0 && took == 0 || self.pending.is_empty() {
                break;
            }
        }
        // All that is left unconsumed is less than a record.
        if self.consume
            && self
                .conn
                .session()
                .is_some_and(|s| !s.is_closed() && !s.peer_closed())
        {
            assert!(self.pending.len() < 5 + (1 << 14) + 2048);
        }
        out
    }

    /// Saves the state, drops the connection and restores it (once the
    /// handshake is done).
    pub fn takeover(&mut self) {
        if let Some(st) = self.conn.state() {
            self.conn = Conn::restore(&self.config, &st).unwrap();
            self.restores += 1;
        }
    }
}

/// A rustls client talking to a [`Server`] in memory.
pub struct Pair {
    pub client: ClientConnection,
    pub server: Server,
    pub received: Vec<u8>,
    pub feed: Feed,
}

impl Pair {
    pub fn new(client: ClientConfig, server: Arc<Config>, feed: Feed) -> Pair {
        let conn =
            ClientConnection::new(Arc::new(client), ServerName::try_from("localhost").unwrap())
                .unwrap();
        Pair {
            client: conn,
            server: Server::new(server),
            received: Vec::new(),
            feed,
        }
    }

    /// What the client wants to send.
    pub fn client_out(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        while self.client.wants_write() {
            self.client.write_tls(&mut b).unwrap();
        }
        b
    }

    /// Gives the client the engine's bytes.
    pub fn client_in(&mut self, bytes: &[u8]) -> Result<(), rustls::Error> {
        let mut r = bytes;
        while !r.is_empty() {
            self.client.read_tls(&mut r).unwrap();
            self.client.process_new_packets()?;
            let mut buf = [0u8; 16384];
            while let Ok(n) = self.client.reader().read(&mut buf) {
                if n == 0 {
                    break;
                }
                self.received.extend_from_slice(&buf[..n]);
            }
        }
        Ok(())
    }

    /// Delivers `bytes` to the engine the configured way.
    pub fn deliver(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        match self.feed {
            Feed::Whole => out.extend(self.server.feed(bytes, false)),
            Feed::Records => {
                for r in records(bytes) {
                    out.extend(self.server.feed(r, false));
                }
            }
            Feed::Bytes => {
                for b in bytes {
                    out.extend(self.server.feed(&[*b], false));
                }
            }
        }
        out
    }

    /// Moves bytes both ways until neither side has more to say.
    pub fn pump(&mut self) -> Result<(), rustls::Error> {
        for _ in 0..1000 {
            let c = self.client_out();
            let s = self.deliver(&c);
            if c.is_empty() && s.is_empty() {
                return Ok(());
            }
            self.client_in(&s)?;
        }
        panic!("no quiet after 1000 rounds");
    }

    pub fn handshake(&mut self) {
        self.pump().unwrap();
        assert!(!self.client.is_handshaking(), "client still handshaking");
        assert!(
            self.server.conn.is_established(),
            "{:?}",
            self.server.errors
        );
        assert_eq!(self.server.established, 1);
    }

    /// The client sends `data`; the engine echoes it; the client must get
    /// it back exactly.
    pub fn echo(&mut self, data: &[u8]) {
        use std::io::Write;
        let start = self.received.len();
        let got_before = self.server.received.len();
        for chunk in data.chunks(10_000) {
            self.client.writer().write_all(chunk).unwrap();
            self.pump().unwrap();
        }
        assert_eq!(&self.server.received[got_before..], data);
        assert_eq!(&self.received[start..], data);
    }
}

/// A deterministic generator (xorshift64*).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}
