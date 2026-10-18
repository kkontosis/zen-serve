//! The record layer after the handshake (svc.md §7.1): TLS 1.3 and 1.2
//! records, KeyUpdate, alerts and the key limits.

use super::{Error, Progress};
use rustls::{AlertDescription, CipherSuite, ProtocolVersion};
use zen_tls_rustcrypto::record::{
    self, ALERT, APPLICATION_DATA, Aead, CHANGE_CIPHER_SPEC, HANDSHAKE, HEADER_LEN, Hash,
    MAX_CIPHER_12, MAX_CIPHER_13, MAX_PLAIN, RecordError, RecordKey,
};
use zeroize::{Zeroize, Zeroizing};

/// The peer sent close_notify.
pub(crate) const PEER_CLOSED: u8 = 1;
/// We sent close_notify.
pub(crate) const SENT_CLOSE: u8 = 2;
/// A fatal alert was sent or received.
pub(crate) const CLOSED: u8 = 4;

/// The engine stops decrypting once it holds this much unconsumed
/// plaintext.
pub const PLAIN_HELD: usize = 16_384;

const KEY_UPDATE: u8 = 24;

/// A negotiated suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Suite {
    pub(crate) id: u16,
    pub(crate) tls13: bool,
    pub(crate) aead: Aead,
    pub(crate) hash: Hash,
}

impl Suite {
    /// The suites the engine runs: the provider's (`provider_tls12`).
    pub(crate) fn from_id(id: u16, ver: ProtocolVersion) -> Option<Suite> {
        let (tls13, aead, hash) = match id {
            0x1301 => (true, Aead::Aes128Gcm, Hash::Sha256),
            0x1302 => (true, Aead::Aes256Gcm, Hash::Sha384),
            0x1303 => (true, Aead::Chacha20Poly1305, Hash::Sha256),
            0xc02b => (false, Aead::Aes128Gcm, Hash::Sha256),
            0xc02c => (false, Aead::Aes256Gcm, Hash::Sha384),
            0xcca9 => (false, Aead::Chacha20Poly1305, Hash::Sha256),
            _ => return None,
        };
        let want = if tls13 {
            ProtocolVersion::TLSv1_3
        } else {
            ProtocolVersion::TLSv1_2
        };
        (ver == want).then_some(Suite {
            id,
            tls13,
            aead,
            hash,
        })
    }

    pub(crate) fn version(&self) -> ProtocolVersion {
        if self.tls13 {
            ProtocolVersion::TLSv1_3
        } else {
            ProtocolVersion::TLSv1_2
        }
    }
}

/// One direction: its sequence number and keys.
#[derive(Clone)]
pub(crate) struct Dir {
    pub(crate) seq: u64,
    /// 1.3: the current traffic secret. Empty for 1.2.
    pub(crate) secret: Zeroizing<Vec<u8>>,
    /// The record key's bytes (1.3: derived from the secret).
    pub(crate) key_bytes: Zeroizing<Vec<u8>>,
    /// The IV (1.3: derived; 1.2: as the key block gives it).
    pub(crate) iv: [u8; 12],
    pub(crate) key: RecordKey,
}

impl Drop for Dir {
    fn drop(&mut self) {
        self.iv.zeroize();
    }
}

impl Dir {
    /// A 1.3 direction from its traffic secret.
    pub(crate) fn new13(suite: Suite, secret: Zeroizing<Vec<u8>>, seq: u64) -> Result<Dir, Error> {
        if secret.len() != suite.hash.output_len() {
            return Err(Error::BadState("traffic secret length"));
        }
        let key_bytes = Zeroizing::new(record::expand_label(
            suite.hash,
            &secret,
            b"key",
            b"",
            suite.aead.key_len(),
        ));
        let iv_bytes = Zeroizing::new(record::expand_label(suite.hash, &secret, b"iv", b"", 12));
        let iv: [u8; 12] = iv_bytes[..].try_into().expect("12 bytes");
        let key =
            RecordKey::new(suite.aead, &key_bytes, &iv).map_err(|_| Error::BadState("key"))?;
        Ok(Dir {
            seq,
            secret,
            key_bytes,
            iv,
            key,
        })
    }

    /// A 1.2 direction from its key and IV.
    pub(crate) fn new12(suite: Suite, key: &[u8], iv: &[u8], seq: u64) -> Result<Dir, Error> {
        let iv: [u8; 12] = iv.try_into().map_err(|_| Error::BadState("IV length"))?;
        let record_key =
            RecordKey::new(suite.aead, key, &iv).map_err(|_| Error::BadState("key length"))?;
        Ok(Dir {
            seq,
            secret: Zeroizing::new(Vec::new()),
            key_bytes: Zeroizing::new(key.to_vec()),
            iv,
            key: record_key,
        })
    }

    /// The next 1.3 traffic secret (RFC 8446 §7.2), from sequence number 0.
    fn update13(&mut self, suite: Suite) -> Result<(), Error> {
        let next = Zeroizing::new(record::expand_label(
            suite.hash,
            &self.secret,
            b"traffic upd",
            b"",
            suite.hash.output_len(),
        ));
        *self = Dir::new13(suite, next, 0)?;
        Ok(())
    }
}

/// An established connection: the record layer and its state. `Clone`, so
/// a runtime can run a step on a copy and keep the original if the step's
/// commit fails.
#[derive(Clone)]
pub struct Session {
    pub(crate) suite: Suite,
    pub(crate) tx: Dir,
    pub(crate) rx: Dir,
    pub(crate) plain: Vec<u8>,
    pub(crate) sni: Option<String>,
    pub(crate) alpn: Option<Vec<u8>>,
    pub(crate) flags: u8,
    /// Records per key before the confidentiality limit's action.
    pub(crate) limit: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("version", &self.suite.version())
            .field("suite", &self.suite.id)
            .field("tx_seq", &self.tx.seq)
            .field("rx_seq", &self.rx.seq)
            .field("plain", &self.plain.len())
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

fn alert_error(e: RecordError) -> AlertDescription {
    match e {
        RecordError::BadRecordMac => AlertDescription::BadRecordMac,
        RecordError::RecordOverflow => AlertDescription::RecordOverflow,
        RecordError::NoContentType => AlertDescription::UnexpectedMessage,
        RecordError::Malformed => AlertDescription::DecodeError,
    }
}

/// What one record asked for.
enum Step {
    Continue,
    /// The peer closed (close_notify): ignore what follows.
    PeerClosed,
    /// Send this fatal alert and stop.
    Fatal(AlertDescription),
    /// The peer sent a fatal alert.
    PeerFatal(AlertDescription),
}

impl Session {
    pub(crate) fn new(
        suite: Suite,
        tx: Dir,
        rx: Dir,
        plain: Vec<u8>,
        sni: Option<String>,
        alpn: Option<Vec<u8>>,
        limit: Option<u64>,
    ) -> Session {
        Session {
            suite,
            tx,
            rx,
            plain,
            sni,
            alpn,
            flags: 0,
            limit: limit.unwrap_or(suite.aead.confidentiality_limit()),
        }
    }

    /// The protocol version.
    pub fn version(&self) -> ProtocolVersion {
        self.suite.version()
    }

    /// The cipher suite.
    pub fn suite(&self) -> CipherSuite {
        CipherSuite::from(self.suite.id)
    }

    /// The client's server name, if it sent one.
    pub fn sni(&self) -> Option<&str> {
        self.sni.as_deref()
    }

    /// The negotiated application protocol, if any.
    pub fn alpn(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    /// Whether the peer sent close_notify.
    pub fn peer_closed(&self) -> bool {
        self.flags & PEER_CLOSED != 0
    }

    /// Whether this side can't send application data any more (close_notify
    /// sent, or a fatal alert).
    pub fn write_closed(&self) -> bool {
        self.flags & (SENT_CLOSE | CLOSED) != 0
    }

    /// Whether a fatal alert ended the connection.
    pub fn is_closed(&self) -> bool {
        self.flags & CLOSED != 0
    }

    /// Decrypted application data not consumed yet.
    pub fn plaintext(&self) -> &[u8] {
        &self.plain
    }

    /// Consumes the first `n` bytes of [`Session::plaintext`].
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.plain.len());
        self.plain.drain(..n);
    }

    /// The next record sequence numbers `(tx, rx)`.
    pub fn seqs(&self) -> (u64, u64) {
        (self.tx.seq, self.rx.seq)
    }

    /// Reads records from `input`. `consumed` ends at a record boundary;
    /// an incomplete record, or one that would exceed the plaintext held,
    /// waits. `eof`: the peer's input ends after `input`.
    pub fn input(&mut self, input: &[u8], eof: bool) -> Progress {
        let mut p = Progress::default();
        let mut owed = false;
        let mut at = 0;
        while self.flags & (PEER_CLOSED | CLOSED) == 0 && self.plain.len() < PLAIN_HELD {
            let rest = &input[at..];
            if rest.len() < HEADER_LEN {
                break;
            }
            let len = usize::from(u16::from_be_bytes([rest[3], rest[4]]));
            let max = if self.suite.tls13 {
                MAX_CIPHER_13
            } else {
                MAX_CIPHER_12
            };
            let step = if len > max {
                Step::Fatal(AlertDescription::RecordOverflow)
            } else if rest.len() < HEADER_LEN + len {
                break;
            } else {
                at += HEADER_LEN + len;
                self.record(&rest[..HEADER_LEN + len], &mut owed, &mut p)
            };
            match step {
                Step::Continue => {}
                Step::PeerClosed => self.flags |= PEER_CLOSED,
                Step::Fatal(d) => {
                    self.fatal(d, &mut p.out);
                    p.error = Some(Error::AlertSent(d));
                }
                Step::PeerFatal(d) => {
                    self.flags |= CLOSED;
                    p.error = Some(Error::AlertReceived(d));
                }
            }
        }
        // Input after close_notify or a fatal alert is ignored.
        if self.flags & (PEER_CLOSED | CLOSED) != 0 {
            at = input.len();
        }
        if owed && !self.write_closed() {
            self.send_key_update(false, &mut p.out);
        }
        p.consumed = at;
        p.peer_closed = self.peer_closed();
        // Records held back by the plaintext bound aren't a truncation yet.
        p.truncated =
            eof && self.flags & (PEER_CLOSED | CLOSED) == 0 && self.plain.len() < PLAIN_HELD;
        p
    }

    /// One complete record.
    fn record(&mut self, rec: &[u8], owed: &mut bool, p: &mut Progress) -> Step {
        let outer = rec[0];
        let seq = self.rx.seq;
        let (typ, plain) = if self.suite.tls13 {
            if outer != APPLICATION_DATA {
                return Step::Fatal(AlertDescription::UnexpectedMessage);
            }
            match self.rx.key.open13(seq, rec) {
                Ok(x) => x,
                Err(e) => return Step::Fatal(alert_error(e)),
            }
        } else {
            if !matches!(outer, APPLICATION_DATA | ALERT | HANDSHAKE) {
                return Step::Fatal(AlertDescription::UnexpectedMessage);
            }
            match self.rx.key.open12(seq, rec) {
                Ok(x) => (outer, x),
                Err(e) => return Step::Fatal(alert_error(e)),
            }
        };
        self.rx.seq += 1;
        match typ {
            APPLICATION_DATA => {
                self.plain.extend_from_slice(&plain);
                Step::Continue
            }
            ALERT => {
                let [level, desc] = plain[..] else {
                    return Step::Fatal(AlertDescription::DecodeError);
                };
                let desc = AlertDescription::from(desc);
                match (level, desc) {
                    (_, AlertDescription::CloseNotify) => Step::PeerClosed,
                    (1, AlertDescription::UserCanceled) => Step::Continue,
                    // 1.3: every other alert is fatal (RFC 8446 §6); a
                    // warning that claims otherwise is refused, as rustls
                    // does.
                    (1, _) if self.suite.tls13 => Step::Fatal(AlertDescription::DecodeError),
                    (1, _) => Step::Continue,
                    _ => Step::PeerFatal(desc),
                }
            }
            HANDSHAKE if self.suite.tls13 => self.post_handshake(&plain, owed),
            HANDSHAKE => {
                // 1.2 renegotiation: refused with a warning, as rustls does.
                let warning = [1, u8::from(AlertDescription::NoRenegotiation)];
                self.fresh_prefix();
                self.seal(ALERT, &warning, &mut p.out);
                Step::Continue
            }
            CHANGE_CIPHER_SPEC => Step::Fatal(AlertDescription::UnexpectedMessage),
            _ => Step::Fatal(AlertDescription::UnexpectedMessage),
        }
    }

    /// A 1.3 post-handshake handshake record: only whole KeyUpdate
    /// messages, the last one ending the record.
    fn post_handshake(&mut self, mut m: &[u8], owed: &mut bool) -> Step {
        if m.is_empty() {
            return Step::Fatal(AlertDescription::UnexpectedMessage);
        }
        while !m.is_empty() {
            if m.len() < 4 {
                return Step::Fatal(AlertDescription::UnexpectedMessage);
            }
            let len = usize::from(m[1]) << 16 | usize::from(m[2]) << 8 | usize::from(m[3]);
            if m[0] != KEY_UPDATE || m.len() < 4 + len {
                return Step::Fatal(AlertDescription::UnexpectedMessage);
            }
            if len != 1 {
                return Step::Fatal(AlertDescription::DecodeError);
            }
            let request = m[4];
            if request > 1 {
                return Step::Fatal(AlertDescription::IllegalParameter);
            }
            m = &m[5..];
            // A key change ends the record (RFC 8446 §5.1).
            if !m.is_empty() {
                return Step::Fatal(AlertDescription::UnexpectedMessage);
            }
            if self.rx.update13(self.suite).is_err() {
                return Step::Fatal(AlertDescription::InternalError);
            }
            if request == 1 {
                *owed = true;
            }
        }
        Step::Continue
    }

    /// Seals one record under the current send key.
    fn seal(&mut self, typ: u8, plain: &[u8], out: &mut Vec<u8>) {
        let r = if self.suite.tls13 {
            self.tx.key.seal13(self.tx.seq, typ, plain)
        } else {
            self.tx.key.seal12(self.tx.seq, typ, plain)
        };
        self.tx.seq += 1;
        out.extend_from_slice(&r);
    }

    /// 1.2 AES-GCM: a fresh random explicit-nonce prefix for the records
    /// about to be sent (svc.md §7.1).
    fn fresh_prefix(&mut self) {
        if self.suite.tls13 || self.suite.aead == Aead::Chacha20Poly1305 {
            return;
        }
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&self.tx.iv[..4]);
        // Without randomness the prefix stays zero: still unique within the
        // connection, only the rollback margin is lost.
        let _ = getrandom::fill(&mut iv[4..8]);
        self.tx.key.set_iv(iv);
    }

    fn send_key_update(&mut self, request: bool, out: &mut Vec<u8>) {
        self.seal(HANDSHAKE, &[KEY_UPDATE, 0, 0, 1, u8::from(request)], out);
        // A secret of the right length always derives.
        let _ = self.tx.update13(self.suite);
    }

    /// Sends a fatal alert and closes.
    fn fatal(&mut self, desc: AlertDescription, out: &mut Vec<u8>) {
        if self.flags & CLOSED == 0 {
            self.fresh_prefix();
            self.seal(ALERT, &[2, u8::from(desc)], out);
        }
        self.flags |= CLOSED;
    }

    /// Encrypts `data` into records of at most 16 KiB. In 1.3, a key that
    /// reaches its limit is updated first; in 1.2 a write that would pass
    /// the limit fails with [`Error::KeyLimit`] and sends nothing.
    pub fn write(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        if self.write_closed() {
            return Err(Error::Closed);
        }
        let n = data.len().div_ceil(MAX_PLAIN) as u64;
        // 1.2 keeps one record for close_notify.
        if !self.suite.tls13 && self.tx.seq.saturating_add(n).saturating_add(1) > self.limit {
            return Err(Error::KeyLimit);
        }
        self.fresh_prefix();
        let mut out = Vec::with_capacity(data.len() + n as usize * 22);
        for chunk in data.chunks(MAX_PLAIN) {
            if self.suite.tls13 && self.tx.seq.saturating_add(1) >= self.limit {
                self.send_key_update(false, &mut out);
            }
            self.seal(APPLICATION_DATA, chunk, &mut out);
        }
        Ok(out)
    }

    /// A KeyUpdate (1.3 only), asking the peer to update too if
    /// `request_peer`.
    pub fn key_update(&mut self, request_peer: bool) -> Result<Vec<u8>, Error> {
        if !self.suite.tls13 {
            return Err(Error::Unsupported("KeyUpdate is TLS 1.3 only"));
        }
        if self.write_closed() {
            return Err(Error::Closed);
        }
        let mut out = Vec::new();
        self.send_key_update(request_peer, &mut out);
        Ok(out)
    }

    /// close_notify, once; later writes fail with [`Error::Closed`].
    pub fn close(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.write_closed() {
            self.fresh_prefix();
            self.seal(
                ALERT,
                &[1, u8::from(AlertDescription::CloseNotify)],
                &mut out,
            );
            self.flags |= SENT_CLOSE;
        }
        out
    }
}
