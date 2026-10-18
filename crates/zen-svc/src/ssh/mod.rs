//! The SSH engine (svc.md §7.2): a sans-IO SSH-2 server whose whole state
//! serializes at every packet boundary, a key exchange in progress
//! included, so another handler can restore it and continue the live
//! session at any point.
//!
//! * Key exchange `mlkem768x25519-sha256` and `curve25519-sha256`, host
//!   key `ssh-ed25519`, ciphers `chacha20-poly1305@openssh.com` and
//!   `aes256-gcm@openssh.com`; strict kex and EXT_INFO.
//! * `publickey` authentication, decided by the application
//!   ([`Event::Authorize`], [`Conn::authorize`]).
//! * `session` channels with `exec`, `shell` and `subsystem`.
//!
//! A [`Conn`] is one connection. A runtime feeds it input
//! ([`Conn::input`]), hands its events to the application, which reads
//! ([`Conn::channel_data`], [`Conn::consume`]) and writes
//! ([`Conn::write`]), and saves [`Conn::state`] with each step;
//! [`Conn::restore`] continues from it.

mod auth;
mod channel;
mod cipher;
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub mod client;
mod kex;
mod keys;
mod state;
mod wire;

pub use auth::Authorize;
pub use channel::{ChannelInfo, Request};
pub use cipher::Cipher;
pub use kex::Kex;
pub use keys::{HostKey, PublicKey};

use cipher::{Dir, Keys, PacketError};
use kex::{Chosen, HashInput};
use std::fmt;
use std::sync::Arc;
use wire::{Reader, Writer};

/// Disconnect reasons (RFC 4253 §11.1).
pub mod reason {
    pub const PROTOCOL_ERROR: u32 = 2;
    pub const KEY_EXCHANGE_FAILED: u32 = 3;
    pub const MAC_ERROR: u32 = 5;
    pub const SERVICE_NOT_AVAILABLE: u32 = 7;
    pub const PROTOCOL_VERSION_NOT_SUPPORTED: u32 = 8;
    pub const BY_APPLICATION: u32 = 11;
    pub const NO_MORE_AUTH_METHODS_AVAILABLE: u32 = 14;
}

const MSG_DISCONNECT: u8 = 1;
const MSG_IGNORE: u8 = 2;
const MSG_UNIMPLEMENTED: u8 = 3;
const MSG_DEBUG: u8 = 4;
const MSG_SERVICE_REQUEST: u8 = 5;
const MSG_SERVICE_ACCEPT: u8 = 6;
const MSG_EXT_INFO: u8 = 7;
const MSG_GLOBAL_REQUEST: u8 = 80;
const MSG_REQUEST_SUCCESS: u8 = 81;
const MSG_REQUEST_FAILURE: u8 = 82;

/// Unread channel data held across channels (svc.md §7.2).
pub const UNREAD_MAX: usize = 32_768;
/// Output queued across channels.
pub const QUEUE_MAX: usize = 16_384;
/// Pieces of queued output.
pub(crate) const QUEUE_PIECES: usize = 64;
/// Replies held during the server's own key exchange.
pub(crate) const HELD_MSGS: usize = 32;
pub(crate) const HELD_BYTES: usize = 4096;
/// Lines accepted before the client's version line.
const MAX_LINES: u32 = 20;
/// Packets under one key before a rekey.
const REKEY_PACKETS: u64 = 1 << 31;

/// What [`Config::new`] may change.
#[derive(Clone, Debug)]
pub struct Options {
    /// Key-exchange methods, preferred first.
    pub kex: Vec<Kex>,
    /// Ciphers, preferred first.
    pub ciphers: Vec<Cipher>,
    /// The software name in `SSH-2.0-<software>` (printable ASCII, no
    /// spaces or `-`).
    pub software: String,
    /// The window each channel is offered (svc.md §7.2).
    pub window: u32,
    /// Bytes in either direction under one key before the server rekeys.
    pub rekey_bytes: u64,
    /// Seconds since the last key exchange before the server rekeys.
    pub rekey_secs: u64,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            kex: vec![Kex::MlKem768X25519, Kex::Curve25519],
            ciphers: vec![Cipher::Chacha20Poly1305, Cipher::Aes256Gcm],
            software: "zen".into(),
            window: 98_304,
            rekey_bytes: 1 << 30,
            rekey_secs: 3600,
        }
    }
}

struct Inner {
    host: HostKey,
    opts: Options,
}

/// A host key and the SSH settings, shared by connections.
#[derive(Clone)]
pub struct Config(Arc<Inner>);

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.0.host)
            .field("opts", &self.0.opts)
            .finish()
    }
}

impl Config {
    pub fn new(host_key: HostKey, opts: Options) -> Result<Config, Error> {
        let bad = |e: &str| Err(Error::Config(e.into()));
        if opts.kex.is_empty() {
            return bad("no key-exchange method");
        }
        if opts.ciphers.is_empty() {
            return bad("no cipher");
        }
        let s = &opts.software;
        if s.is_empty() || s.len() > 64 || !s.bytes().all(|b| b.is_ascii_graphic() && b != b'-') {
            return bad("a software name must be 1 to 64 printable ASCII characters, without '-'");
        }
        if !(1024..=1 << 30).contains(&opts.window) {
            return bad("a channel window must be 1 KiB to 1 GiB");
        }
        if opts.rekey_bytes == 0 || opts.rekey_secs == 0 {
            return bad("rekey limits must be positive");
        }
        Ok(Config(Arc::new(Inner {
            host: host_key,
            opts,
        })))
    }

    /// The host key.
    pub fn host_key(&self) -> &HostKey {
        &self.0.host
    }

    fn opts(&self) -> &Options {
        &self.0.opts
    }
}

/// Why the engine refused or ended something.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A bad option.
    Config(String),
    /// A key that doesn't parse or isn't supported.
    Key(&'static str),
    /// A state that doesn't restore.
    BadState(&'static str),
    /// The connection has ended.
    Closed,
    /// No such open channel.
    NoChannel,
    /// This side already ended the channel (EOF or CLOSE), or the client
    /// closed it.
    ChannelEnded,
    /// [`Conn::authorize`] without a question.
    NotAsked,
    /// The engine ended the connection: it sent DISCONNECT with this
    /// reason (RFC 4253 §11.1).
    Protocol { reason: u32, detail: &'static str },
    /// The client sent DISCONNECT.
    Disconnected { reason: u32, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => write!(f, "SSH configuration: {e}"),
            Error::Key(e) => write!(f, "SSH key: {e}"),
            Error::BadState(e) => write!(f, "SSH state: {e}"),
            Error::Closed => f.write_str("the SSH connection has ended"),
            Error::NoChannel => f.write_str("no such SSH channel"),
            Error::ChannelEnded => f.write_str("the SSH channel has ended"),
            Error::NotAsked => f.write_str("no authorization is pending"),
            Error::Protocol { reason, detail } => {
                write!(f, "SSH: disconnected ({reason}): {detail}")
            }
            Error::Disconnected { reason, message } => {
                write!(f, "SSH: the client disconnected ({reason}): {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::cbor::Bad> for Error {
    fn from(b: crate::cbor::Bad) -> Error {
        Error::BadState(b.0)
    }
}

/// What the application is told about.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Answer with [`Conn::authorize`]; no further input is read until then.
    Authorize(Authorize),
    /// A session channel was opened.
    ChannelOpen { ch: u32 },
    /// The channel asks to run `command`.
    Exec { ch: u32, command: Vec<u8> },
    /// The channel asks for a shell.
    Shell { ch: u32 },
    /// The channel asks for the subsystem `name`.
    Subsystem { ch: u32, name: String },
    /// New data in [`Conn::channel_data`].
    Data { ch: u32 },
    /// The client sent EOF on the channel.
    Eof { ch: u32 },
    /// The client closed the channel; it is gone after this step.
    Close { ch: u32 },
}

/// What one [`Conn::input`] did.
#[derive(Debug, Default)]
pub struct Progress {
    /// Input bytes processed: the step's ack, at a packet (or line)
    /// boundary.
    pub consumed: usize,
    /// Bytes to send to the client.
    pub out: Vec<u8>,
    pub events: Vec<Event>,
    /// The client sent DISCONNECT, or its input ended.
    pub peer_closed: bool,
    /// Reading stopped with complete input left: a question waits for
    /// [`Conn::authorize`], or unread data or held replies reached their
    /// bound. Input again once the application has answered or consumed.
    pub held: bool,
    /// The engine ended the connection (its DISCONNECT is in `out`), or
    /// the client disconnected. The runtime closes the connection after
    /// sending `out`.
    pub error: Option<Error>,
}

/// A protocol violation: the engine sends DISCONNECT and ends.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fail(pub(crate) u32, pub(crate) &'static str);

impl From<wire::Malformed> for Fail {
    fn from(_: wire::Malformed) -> Fail {
        Fail(reason::PROTOCOL_ERROR, "a malformed message")
    }
}

/// The key exchange in progress.
#[derive(Clone, Debug, Default)]
pub(crate) struct KexState {
    /// Our KEXINIT and the client's, until the reply.
    pub(crate) ours: Option<Vec<u8>>,
    pub(crate) theirs: Option<Vec<u8>>,
    /// Negotiated once the client's KEXINIT is in.
    pub(crate) chosen: Option<Chosen>,
    /// The reply and our NEWKEYS are sent; the client's NEWKEYS is next.
    pub(crate) replied: bool,
    /// Ignore the client's next packet (a wrong guess).
    pub(crate) skip: bool,
    /// The sequence numbers of the client's KEXINIT and init, handled
    /// ahead of order (equal when only the init was).
    pub(crate) ahead: Option<(u32, u32)>,
}

const PEER_ENDED: u8 = 1;
const ENDED: u8 = 2;

/// One SSH connection, server side.
pub struct Conn {
    cfg: Config,
    vs: Vec<u8>,
    vc: Option<Vec<u8>>,
    lines: u32,
    sid: Option<Vec<u8>>,
    strict: bool,
    tx: Dir,
    rx: Dir,
    rx_next: Option<Keys>,
    kex: Option<KexState>,
    kex_at: u64,
    kexes: u64,
    auth: auth::Auth,
    chans: Vec<channel::Channel>,
    held: Vec<Vec<u8>>,
    flags: u8,
    /// This call's output and events (not part of the state).
    out: Vec<u8>,
    events: Vec<Event>,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("authenticated", &self.auth.user)
            .field("channels", &self.chans.len())
            .finish_non_exhaustive()
    }
}

fn before(seq: u32, mark: u32) -> bool {
    (seq.wrapping_sub(mark) as i32) < 0
}

impl Conn {
    /// A new connection at Unix time `now`, and what to send at once: the
    /// version line and the first KEXINIT (the server speaks first).
    pub fn new(config: &Config, now: u64) -> (Conn, Vec<u8>) {
        let vs = format!("SSH-2.0-{}", config.opts().software).into_bytes();
        let mut c = Conn {
            cfg: config.clone(),
            vs,
            vc: None,
            lines: 0,
            sid: None,
            strict: false,
            tx: Dir::default(),
            rx: Dir::default(),
            rx_next: None,
            kex: None,
            kex_at: now,
            kexes: 0,
            auth: auth::Auth::default(),
            chans: Vec::new(),
            held: Vec::new(),
            flags: 0,
            out: Vec::new(),
            events: Vec::new(),
        };
        c.out.extend_from_slice(&c.vs);
        c.out.extend_from_slice(b"\r\n");
        c.send_kexinit();
        let out = std::mem::take(&mut c.out);
        (c, out)
    }

    /// Processes input from the last ack on (`eof`: nothing follows it),
    /// at Unix time `now`.
    pub fn input(&mut self, buf: &[u8], eof: bool, now: u64) -> Progress {
        let mut p = Progress::default();
        if self.flags & ENDED != 0 {
            p.error = Some(Error::Closed);
            return p;
        }
        self.chans.retain(|c| !c.gone());
        let (at, held, r) = self.read(buf, now);
        p.consumed = at;
        p.held = held && r.is_ok() && !eof;
        match r {
            Err(Stop::Fail(f)) => p.error = Some(self.fail(f)),
            Err(Stop::Peer(reason, message)) => {
                p.peer_closed = true;
                p.error = Some(Error::Disconnected { reason, message });
                self.flags |= PEER_ENDED | ENDED;
            }
            Ok(()) if eof => {
                p.peer_closed = true;
                self.flags |= PEER_ENDED | ENDED;
            }
            Ok(()) => {
                if self.rekey_due(now) {
                    self.send_kexinit();
                }
            }
        }
        p.out = std::mem::take(&mut self.out);
        p.events = std::mem::take(&mut self.events);
        p
    }

    /// Reads lines and packets until the input runs out or something
    /// holds reading back: the bytes consumed, and whether reading was
    /// held back with input left.
    fn read(&mut self, buf: &[u8], now: u64) -> (usize, bool, Result<(), Stop>) {
        let mut at = 0;
        if self.vc.is_none() {
            match self.version(buf) {
                Ok(n) => at = n,
                Err(f) => return (at, false, Err(f)),
            }
            if self.vc.is_none() {
                return (at, false, Ok(()));
            }
        }
        let mut held_back = false;
        loop {
            if self.auth.ask.is_some() {
                return (at, at < buf.len(), Ok(()));
            }
            if self.held.len() >= HELD_MSGS || self.held_bytes() >= HELD_BYTES {
                held_back = true;
                break;
            }
            let rest = &buf[at..];
            let total = match self.rx.next_len(rest) {
                Ok(Some(n)) if n <= rest.len() => n,
                Ok(_) => break,
                Err(e) => return (at, false, Err(packet_error(e))),
            };
            let unread = self.unread();
            if unread > 0 && unread + total > UNREAD_MAX {
                held_back = true;
                break;
            }
            let seq = self.rx.seq;
            let payload = match self.rx.open(rest) {
                Ok(Some((p, _))) => zeroize::Zeroizing::new(p),
                Ok(None) => break,
                Err(e) => return (at, false, Err(packet_error(e))),
            };
            at += total;
            if let Err(e) = self.packet(seq, &payload, now) {
                return (at, false, Err(e));
            }
        }
        if held_back && let Err(e) = self.scan_ahead(&buf[at..]) {
            return (at, false, Err(e));
        }
        (at, held_back && at < buf.len(), Ok(()))
    }

    /// The client's version line, after up to [`MAX_LINES`] other lines.
    fn version(&mut self, buf: &[u8]) -> Result<usize, Stop> {
        let mut at = 0;
        loop {
            let rest = &buf[at..];
            let Some(nl) = rest.iter().take(255).position(|&b| b == b'\n') else {
                if rest.len() >= 255 {
                    return fail(
                        reason::PROTOCOL_ERROR,
                        "a line over 255 bytes before the version",
                    );
                }
                return Ok(at);
            };
            let mut line = &rest[..nl];
            if let [l @ .., b'\r'] = line {
                line = l;
            }
            at += nl + 1;
            if line.starts_with(b"SSH-") {
                if !line.starts_with(b"SSH-2.0-") && !line.starts_with(b"SSH-1.99-") {
                    return fail(reason::PROTOCOL_VERSION_NOT_SUPPORTED, "not SSH 2.0");
                }
                if !line.iter().all(|&b| (0x20..0x7f).contains(&b)) {
                    return fail(
                        reason::PROTOCOL_ERROR,
                        "a version line that isn't printable ASCII",
                    );
                }
                self.vc = Some(line.to_vec());
                return Ok(at);
            }
            self.lines += 1;
            if self.lines > MAX_LINES {
                return fail(reason::PROTOCOL_ERROR, "too many lines before the version");
            }
        }
    }

    /// The client's KEXINIT is in and its NEWKEYS isn't, at packet `seq`.
    fn client_in_kex(&self, seq: u32) -> bool {
        self.kex.as_ref().is_some_and(|k| {
            k.chosen.is_some() && !k.ahead.is_some_and(|(a, b)| a != b && before(seq, a))
        })
    }

    /// The first key exchange is done both ways.
    fn transport_ready(&self) -> bool {
        self.sid.is_some() && self.rx.keys.is_some()
    }

    fn packet(&mut self, seq: u32, p: &[u8], now: u64) -> Result<(), Stop> {
        let typ = p[0];
        if let Some(k) = &mut self.kex {
            if k.skip {
                k.skip = false;
                return Ok(());
            }
            if let Some((a, b)) = k.ahead {
                if seq == b {
                    return if typ == kex::MSG_KEX_INIT {
                        Ok(())
                    } else {
                        fail(
                            reason::PROTOCOL_ERROR,
                            "a packet that changed after it was read ahead",
                        )
                    };
                }
                if seq == a {
                    return if typ == kex::MSG_KEXINIT {
                        Ok(())
                    } else {
                        fail(
                            reason::PROTOCOL_ERROR,
                            "a packet that changed after it was read ahead",
                        )
                    };
                }
            }
        }
        if typ == MSG_DISCONNECT {
            let mut r = Reader::new(p);
            r.u8()?;
            let code = r.u32()?;
            let msg = String::from_utf8_lossy(r.string()?).into_owned();
            return Err(Stop::Peer(code, msg));
        }
        let in_kex = self.client_in_kex(seq);
        let kex_msg = matches!(typ, kex::MSG_KEXINIT | kex::MSG_NEWKEYS | kex::MSG_KEX_INIT);
        if in_kex && self.sid.is_none() && self.strict && !kex_msg {
            return fail(
                reason::PROTOCOL_ERROR,
                "strict kex: an unexpected message during the key exchange",
            );
        }
        match typ {
            kex::MSG_KEXINIT => self.on_kexinit(seq, p),
            kex::MSG_KEX_INIT => self.on_kex_init(p),
            kex::MSG_NEWKEYS => self.on_newkeys(now),
            MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED => Ok(()),
            _ if in_kex || !self.transport_ready() => {
                self.unimplemented(seq);
                Ok(())
            }
            MSG_SERVICE_REQUEST => self.on_service_request(p),
            MSG_EXT_INFO => Ok(()),
            50..=79 => self.on_userauth(p),
            _ if self.auth.user.is_none() => fail(
                reason::PROTOCOL_ERROR,
                "a connection message before authentication",
            ),
            MSG_GLOBAL_REQUEST => {
                let mut r = Reader::new(p);
                r.u8()?;
                r.string()?;
                if r.bool()? {
                    self.send(vec![MSG_REQUEST_FAILURE]);
                }
                Ok(())
            }
            MSG_REQUEST_SUCCESS | MSG_REQUEST_FAILURE => Ok(()),
            90..=100 => self.on_channel(p),
            _ => {
                self.unimplemented(seq);
                Ok(())
            }
        }
    }

    fn unimplemented(&mut self, seq: u32) {
        self.send(Writer::new(MSG_UNIMPLEMENTED).u32(seq).finish());
    }

    fn send_kexinit(&mut self) {
        let o = self.cfg.opts();
        let p = kex::kexinit(&o.kex, &o.ciphers, true, self.sid.is_none());
        self.send_now(&p);
        self.kex = Some(KexState {
            ours: Some(p),
            ..KexState::default()
        });
    }

    fn on_kexinit(&mut self, seq: u32, p: &[u8]) -> Result<(), Stop> {
        if p.len() > kex::MAX_KEXINIT {
            return fail(reason::KEY_EXCHANGE_FAILED, "a KEXINIT over 8192 bytes");
        }
        let prop = kex::parse_kexinit(p)?;
        if self.kex.as_ref().is_some_and(|k| k.chosen.is_some()) {
            return fail(reason::PROTOCOL_ERROR, "a second KEXINIT");
        }
        if self.sid.is_none() {
            self.strict = prop.kex.contains(&kex::STRICT_C);
            if self.strict && seq != 0 {
                return fail(
                    reason::PROTOCOL_ERROR,
                    "strict kex: KEXINIT was not the first packet",
                );
            }
        }
        let o = self.cfg.opts();
        let Some(chosen) = kex::negotiate(&prop, &o.kex, &o.ciphers) else {
            return fail(reason::KEY_EXCHANGE_FAILED, "no algorithm in common");
        };
        if self.kex.is_none() {
            self.send_kexinit();
        }
        let k = self.kex.as_mut().expect("a key exchange");
        k.theirs = Some(p.to_vec());
        k.chosen = Some(chosen);
        k.skip = chosen.wrong_guess;
        Ok(())
    }

    fn on_kex_init(&mut self, p: &[u8]) -> Result<(), Stop> {
        let Some(k) = self
            .kex
            .as_ref()
            .filter(|k| k.chosen.is_some() && !k.replied)
        else {
            return fail(reason::PROTOCOL_ERROR, "an unexpected key-exchange message");
        };
        let chosen = k.chosen.expect("negotiated");
        let (Some(ours), Some(theirs)) = (&k.ours, &k.theirs) else {
            return fail(reason::PROTOCOL_ERROR, "an unexpected key-exchange message");
        };
        let mut r = Reader::new(p);
        r.u8()?;
        let q_c = r.string()?;
        r.done()?;
        let Ok((q_s, k_enc)) = kex::server_reply(chosen.kex, q_c) else {
            return fail(reason::KEY_EXCHANGE_FAILED, "a bad key-exchange value");
        };
        let k_s = self.cfg.host_key().blob();
        let h = kex::exchange_hash(&HashInput {
            v_c: self.vc.as_deref().expect("a version"),
            v_s: &self.vs,
            i_c: theirs,
            i_s: ours,
            k_s: &k_s,
            q_c,
            q_s: &q_s,
            k: &k_enc,
        });
        let first = self.sid.is_none();
        let ext_info = first && kex::parse_kexinit(theirs)?.kex.contains(&kex::EXT_INFO_C);
        let sid = self.sid.get_or_insert_with(|| h.to_vec()).clone();
        let sig = self.cfg.host_key().sign(&h);
        let reply = Writer::new(kex::MSG_KEX_REPLY)
            .string(&k_s)
            .string(&q_s)
            .string(&sig)
            .finish();
        self.send_now(&reply);
        self.send_now(&[kex::MSG_NEWKEYS]);
        self.tx.set_keys(
            Some(kex::keys(&k_enc, &h, &sid, chosen.s2c, false)),
            self.strict,
        );
        self.rx_next = Some(kex::keys(&k_enc, &h, &sid, chosen.c2s, true));
        let k = self.kex.as_mut().expect("a key exchange");
        k.replied = true;
        k.ours = None;
        k.theirs = None;
        if ext_info {
            let e = Writer::new(MSG_EXT_INFO)
                .u32(1)
                .string(b"server-sig-algs")
                .string(b"ssh-ed25519,rsa-sha2-256,rsa-sha2-512")
                .finish();
            self.send_now(&e);
        }
        for m in std::mem::take(&mut self.held) {
            self.send_now(&m);
        }
        self.flush_all();
        Ok(())
    }

    fn on_newkeys(&mut self, now: u64) -> Result<(), Stop> {
        if !self.kex.as_ref().is_some_and(|k| k.replied) {
            return fail(reason::PROTOCOL_ERROR, "an unexpected NEWKEYS");
        }
        self.rx.set_keys(self.rx_next.take(), self.strict);
        self.kex = None;
        self.kex_at = now;
        self.kexes += 1;
        Ok(())
    }

    /// The input is held back while the server's key exchange waits for
    /// the client's messages: read ahead for them (svc.md §7.2), without
    /// consuming anything.
    fn scan_ahead(&mut self, buf: &[u8]) -> Result<(), Stop> {
        let Some(k) = &self.kex else {
            return Ok(());
        };
        if k.replied || k.ahead.is_some() || self.sid.is_none() {
            return Ok(());
        }
        // A wrong guess's packet, still to come in order, is skipped here
        // too.
        let mut skip = k.skip;
        let mut rx = self.rx.clone();
        let mut at = 0;
        let mut a = None;
        loop {
            let seq = rx.seq;
            let Ok(Some((p, n))) = rx.open(&buf[at..]) else {
                return Ok(());
            };
            let p = zeroize::Zeroizing::new(p);
            at += n;
            if std::mem::take(&mut skip) {
                continue;
            }
            match p[0] {
                kex::MSG_KEXINIT
                    if a.is_none() && self.kex.as_ref().is_some_and(|k| k.chosen.is_none()) =>
                {
                    // A wrong guess would make in-order reading skip a
                    // packet; leave that to in-order reading.
                    let o = self.cfg.opts();
                    let ok = kex::parse_kexinit(&p)
                        .ok()
                        .and_then(|prop| kex::negotiate(&prop, &o.kex, &o.ciphers))
                        .is_some_and(|c| !c.wrong_guess);
                    if !ok {
                        return Ok(());
                    }
                    self.on_kexinit(seq, &p)?;
                    a = Some(seq);
                }
                kex::MSG_KEX_INIT if self.kex.as_ref().is_some_and(|k| k.chosen.is_some()) => {
                    self.on_kex_init(&p)?;
                    let k = self.kex.as_mut().expect("a key exchange");
                    k.ahead = Some((a.unwrap_or(seq), seq));
                    return Ok(());
                }
                kex::MSG_KEXINIT | kex::MSG_KEX_INIT | kex::MSG_NEWKEYS | MSG_DISCONNECT => {
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn rekey_due(&self, now: u64) -> bool {
        let o = self.cfg.opts();
        self.kex.is_none()
            && self.transport_ready()
            && (self.tx.bytes >= o.rekey_bytes
                || self.rx.bytes >= o.rekey_bytes
                || self.tx.packets >= REKEY_PACKETS
                || self.rx.packets >= REKEY_PACKETS
                || now >= self.kex_at.saturating_add(o.rekey_secs))
    }

    /// Our output waits: our KEXINIT is out and our NEWKEYS isn't.
    fn out_blocked(&self) -> bool {
        self.kex.as_ref().is_some_and(|k| !k.replied)
    }

    /// Sends a transport or key-exchange message.
    fn send_now(&mut self, payload: &[u8]) {
        self.tx.seal(payload, &mut self.out);
    }

    /// Sends a message, or holds it until our NEWKEYS.
    fn send(&mut self, payload: Vec<u8>) {
        if self.out_blocked() {
            self.held.push(payload);
        } else {
            self.send_now(&payload);
        }
    }

    fn held_bytes(&self) -> usize {
        self.held.iter().map(Vec::len).sum()
    }

    fn unread(&self) -> usize {
        self.chans.iter().map(|c| c.unread.len()).sum()
    }

    /// Sends DISCONNECT and ends the connection.
    fn fail(&mut self, f: Fail) -> Error {
        let p = Writer::new(MSG_DISCONNECT)
            .u32(f.0)
            .string(f.1.as_bytes())
            .string(b"")
            .finish();
        self.send_now(&p);
        self.flags |= ENDED;
        Error::Protocol {
            reason: f.0,
            detail: f.1,
        }
    }

    /// Starts a key exchange now, if none is in progress.
    pub fn rekey(&mut self) -> Vec<u8> {
        if self.flags & ENDED == 0 && self.kex.is_none() && self.transport_ready() {
            self.send_kexinit();
        }
        std::mem::take(&mut self.out)
    }

    /// Ends the connection with DISCONNECT.
    pub fn disconnect(&mut self, reason: u32, message: &str) -> Vec<u8> {
        if self.flags & ENDED == 0 {
            let p = Writer::new(MSG_DISCONNECT)
                .u32(reason)
                .string(message.as_bytes())
                .string(b"")
                .finish();
            self.send_now(&p);
            self.flags |= ENDED;
        }
        std::mem::take(&mut self.out)
    }

    /// The connection has ended (either side).
    pub fn ended(&self) -> bool {
        self.flags & ENDED != 0
    }

    /// The authenticated user.
    pub fn user(&self) -> Option<&str> {
        self.auth.user.as_deref()
    }

    /// The session id, after the first key exchange.
    pub fn session_id(&self) -> Option<&[u8]> {
        self.sid.as_deref()
    }

    /// A key exchange is in progress.
    pub fn in_kex(&self) -> bool {
        self.kex.is_some()
    }

    /// Key exchanges completed.
    pub fn kexes(&self) -> u64 {
        self.kexes
    }

    /// The negotiated ciphers (client to server, server to client), once
    /// the first key exchange is done.
    pub fn ciphers(&self) -> Option<(Cipher, Cipher)> {
        Some((self.rx.keys.as_ref()?.cipher, self.tx.keys.as_ref()?.cipher))
    }
}

/// Why reading stopped for good.
pub(crate) enum Stop {
    Fail(Fail),
    /// The client sent DISCONNECT.
    Peer(u32, String),
}

impl From<Fail> for Stop {
    fn from(f: Fail) -> Stop {
        Stop::Fail(f)
    }
}

impl From<wire::Malformed> for Stop {
    fn from(m: wire::Malformed) -> Stop {
        Stop::Fail(m.into())
    }
}

pub(crate) fn fail<T>(reason: u32, detail: &'static str) -> Result<T, Stop> {
    Err(Stop::Fail(Fail(reason, detail)))
}

fn packet_error(e: PacketError) -> Stop {
    match e {
        PacketError::Malformed => Stop::Fail(Fail(reason::PROTOCOL_ERROR, "a malformed packet")),
        PacketError::Mac => Stop::Fail(Fail(
            reason::MAC_ERROR,
            "a packet that doesn't authenticate",
        )),
    }
}
