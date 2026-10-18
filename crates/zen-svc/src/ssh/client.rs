//! A minimal SSH client on the engine's own parts, for tests
//! (`test-utils`): the version exchange, both key exchanges (and rekeys
//! from either side), both ciphers, and messages built and parsed by hand.
//! It checks the host key's signature; interoperability is tested against
//! OpenSSH.

use super::cipher::{Cipher, Dir, Keys};
use super::kex::{self, ClientKex, HashInput, Kex};
use super::keys::HostKey;
use super::wire::{Reader, Writer};
use super::{MSG_EXT_INFO, PublicKey};

/// How the client behaves.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub kex: Kex,
    pub cipher: Cipher,
    /// Offer strict kex (`kex-strict-c-v00@openssh.com`).
    pub strict: bool,
    /// The host key to expect.
    pub host: Option<PublicKey>,
    /// Guess wrong in the first KEXINIT: list an unknown method first, set
    /// `first_kex_packet_follows`, and send a guessed packet the server
    /// must ignore.
    pub wrong_guess: bool,
}

impl ClientOptions {
    pub fn new(kex: Kex, cipher: Cipher) -> ClientOptions {
        ClientOptions {
            kex,
            cipher,
            strict: true,
            host: None,
            wrong_guess: false,
        }
    }
}

/// A client connection: feed it the server's bytes, take its output.
pub struct Client {
    opts: ClientOptions,
    vc: Vec<u8>,
    vs: Option<Vec<u8>>,
    tx: Dir,
    rx: Dir,
    sid: Option<Vec<u8>>,
    ours: Option<Vec<u8>>,
    theirs: Option<Vec<u8>>,
    kx: Option<(ClientKex, Vec<u8>)>,
    rx_next: Option<Keys>,
    strict: bool,
    buf: Vec<u8>,
    out: Vec<u8>,
    /// Messages held from our KEXINIT to our NEWKEYS (RFC 4253 §7.1).
    held: Vec<Vec<u8>>,
    /// Key exchanges completed.
    pub kexes: u32,
    /// The server's EXT_INFO, if it sent one.
    pub ext_info: Option<Vec<u8>>,
}

impl Client {
    pub fn new(opts: ClientOptions) -> Client {
        let vc = b"SSH-2.0-zentest".to_vec();
        let mut c = Client {
            opts,
            vc,
            vs: None,
            tx: Dir::default(),
            rx: Dir::default(),
            sid: None,
            ours: None,
            theirs: None,
            kx: None,
            rx_next: None,
            strict: false,
            buf: Vec::new(),
            out: Vec::new(),
            held: Vec::new(),
            kexes: 0,
            ext_info: None,
        };
        c.out.extend_from_slice(&c.vc);
        c.out.extend_from_slice(b"\r\n");
        c.kexinit();
        c
    }

    /// Bytes to send.
    pub fn take_out(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Sends a message under the current keys; during the client's own key
    /// exchange, messages above the transport layer wait for its NEWKEYS.
    pub fn send(&mut self, payload: &[u8]) {
        if self.ours.is_some() && payload[0] >= 50 {
            self.held.push(payload.to_vec());
        } else {
            self.tx.seal(payload, &mut self.out);
        }
    }

    /// Sends bytes as they are.
    pub fn send_raw(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    pub fn session_id(&self) -> Option<&[u8]> {
        self.sid.as_deref()
    }

    /// The client's sequence number for its next packet.
    pub fn tx_seq(&self) -> u32 {
        self.tx.seq
    }

    /// Starts a key exchange (a client rekey).
    pub fn rekey(&mut self) {
        if self.ours.is_none() {
            self.kexinit();
        }
    }

    /// A key exchange is in progress.
    pub fn in_kex(&self) -> bool {
        self.ours.is_some() || self.rx_next.is_some()
    }

    fn kexinit(&mut self) {
        let p = kex::kexinit(
            &[self.opts.kex],
            &[self.opts.cipher],
            false,
            self.sid.is_none(),
        );
        let first = self.sid.is_none();
        let guess = first && self.opts.wrong_guess;
        let p = if (self.opts.strict || !first) && !guess {
            p
        } else {
            let mut r = Reader::new(&p);
            r.raw(17).expect("cookie");
            let mut names: Vec<&str> = r.names().expect("kex");
            if !self.opts.strict {
                names.retain(|n| *n != kex::STRICT_C);
            }
            if guess {
                names.insert(0, "sntrup761x25519-sha512");
            }
            let mut rest = r.rest().to_vec();
            if guess {
                // first_kex_packet_follows
                let at = rest.len() - 5;
                rest[at] = 1;
            }
            let mut w = Writer::default();
            w.raw(&p[..17]).names(&names).raw(&rest);
            w.finish()
        };
        self.send(&p);
        if guess {
            self.send(
                &Writer::new(kex::MSG_KEX_INIT)
                    .string(b"a wrong guess")
                    .finish(),
            );
        }
        self.ours = Some(p);
    }

    /// Takes the server's bytes; the messages the key exchange didn't
    /// consume, in order.
    pub fn input(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.buf.extend_from_slice(data);
        let mut msgs = Vec::new();
        if self.vs.is_none() {
            let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else {
                return Ok(msgs);
            };
            let line = self.buf[..nl]
                .strip_suffix(b"\r")
                .unwrap_or(&self.buf[..nl])
                .to_vec();
            self.buf.drain(..=nl);
            self.vs = Some(line);
        }
        loop {
            let (p, n) = match self.rx.open(&self.buf) {
                Ok(Some(x)) => x,
                Ok(None) => return Ok(msgs),
                Err(e) => return Err(format!("client: a bad packet: {e:?}")),
            };
            self.buf.drain(..n);
            match p[0] {
                kex::MSG_KEXINIT => {
                    if self.ours.is_none() {
                        self.kexinit();
                    }
                    if self.sid.is_none() {
                        let prop = kex::parse_kexinit(&p).map_err(|_| "client: a bad KEXINIT")?;
                        self.strict = self.opts.strict && prop.kex.contains(&kex::STRICT_S);
                    }
                    self.theirs = Some(p);
                    let (kx, q_c) = ClientKex::start(self.opts.kex);
                    self.send(&Writer::new(kex::MSG_KEX_INIT).string(&q_c).finish());
                    self.kx = Some((kx, q_c));
                }
                kex::MSG_KEX_REPLY => self.on_reply(&p)?,
                kex::MSG_NEWKEYS => {
                    let k = self.rx_next.take().ok_or("client: NEWKEYS out of order")?;
                    self.rx.set_keys(Some(k), self.strict);
                    self.kexes += 1;
                }
                MSG_EXT_INFO => self.ext_info = Some(p),
                _ => msgs.push(p),
            }
        }
    }

    fn on_reply(&mut self, p: &[u8]) -> Result<(), String> {
        let mut r = Reader::new(p);
        let mut parse = || -> Option<(&[u8], &[u8], &[u8])> {
            r.u8().ok()?;
            let k_s = r.string().ok()?;
            let q_s = r.string().ok()?;
            let sig = r.string().ok()?;
            r.done().ok()?;
            Some((k_s, q_s, sig))
        };
        let (k_s, q_s, sig) = parse().ok_or("client: a bad reply")?;
        let (kx, q_c) = self.kx.take().ok_or("client: a reply out of order")?;
        let k = kx.finish(q_s).map_err(|_| "client: a bad server value")?;
        let (ours, theirs) = (
            self.ours.take().ok_or("no KEXINIT")?,
            self.theirs.take().ok_or("no KEXINIT")?,
        );
        let h = kex::exchange_hash(&HashInput {
            v_c: &self.vc,
            v_s: self.vs.as_deref().ok_or("no version")?,
            i_c: &ours,
            i_s: &theirs,
            k_s,
            q_c: &q_c,
            q_s,
            k: &k,
        });
        let host = PublicKey::from_blob(k_s).map_err(|e| format!("client: host key: {e}"))?;
        if let Some(want) = &self.opts.host
            && *want != host
        {
            return Err("client: an unexpected host key".into());
        }
        if !host.verify("ssh-ed25519", &h, sig) {
            return Err("client: a bad host key signature".into());
        }
        let sid = self.sid.get_or_insert_with(|| h.to_vec()).clone();
        self.send(&[kex::MSG_NEWKEYS]);
        self.tx.set_keys(
            Some(kex::keys(&k, &h, &sid, self.opts.cipher, true)),
            self.strict,
        );
        self.rx_next = Some(kex::keys(&k, &h, &sid, self.opts.cipher, false));
        for m in std::mem::take(&mut self.held) {
            self.send(&m);
        }
        Ok(())
    }
}

/// Messages a client sends.
pub mod msg {
    use super::super::auth::signed_data;
    use super::*;

    pub fn ignore(data: &[u8]) -> Vec<u8> {
        Writer::new(2).string(data).finish()
    }

    pub fn service_request(name: &str) -> Vec<u8> {
        Writer::new(5).string(name.as_bytes()).finish()
    }

    pub fn userauth_none(user: &str) -> Vec<u8> {
        Writer::new(50)
            .string(user.as_bytes())
            .string(b"ssh-connection")
            .string(b"none")
            .finish()
    }

    /// A `publickey` request with an Ed25519 user key, signed or not.
    pub fn userauth_ed25519(sid: &[u8], user: &str, key: &HostKey, signed: bool) -> Vec<u8> {
        let blob = key.blob();
        let mut w = Writer::new(50);
        w.string(user.as_bytes())
            .string(b"ssh-connection")
            .string(b"publickey")
            .bool(signed)
            .string(b"ssh-ed25519")
            .string(&blob);
        if signed {
            w.string(&key.sign(&signed_data(sid, user, "ssh-ed25519", &blob)));
        }
        w.finish()
    }

    pub fn channel_open(id: u32, window: u32, max: u32) -> Vec<u8> {
        Writer::new(90)
            .string(b"session")
            .u32(id)
            .u32(window)
            .u32(max)
            .finish()
    }

    pub fn exec(ch: u32, command: &[u8], want: bool) -> Vec<u8> {
        Writer::new(98)
            .u32(ch)
            .string(b"exec")
            .bool(want)
            .string(command)
            .finish()
    }

    pub fn request(ch: u32, name: &str, want: bool) -> Vec<u8> {
        Writer::new(98)
            .u32(ch)
            .string(name.as_bytes())
            .bool(want)
            .finish()
    }

    pub fn data(ch: u32, d: &[u8]) -> Vec<u8> {
        Writer::new(94).u32(ch).string(d).finish()
    }

    pub fn eof(ch: u32) -> Vec<u8> {
        Writer::new(96).u32(ch).finish()
    }

    pub fn close(ch: u32) -> Vec<u8> {
        Writer::new(97).u32(ch).finish()
    }

    pub fn adjust(ch: u32, n: u32) -> Vec<u8> {
        Writer::new(93).u32(ch).u32(n).finish()
    }

    pub fn global_request(name: &str, want: bool) -> Vec<u8> {
        Writer::new(80).string(name.as_bytes()).bool(want).finish()
    }

    pub fn disconnect(reason: u32) -> Vec<u8> {
        Writer::new(1)
            .u32(reason)
            .string(b"bye")
            .string(b"")
            .finish()
    }
}

/// A server message, parsed for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Disconnect(u32),
    Unimplemented(u32),
    ServiceAccept,
    AuthFailure,
    AuthSuccess,
    PkOk,
    RequestFailure,
    OpenConfirmation {
        recipient: u32,
        sender: u32,
        window: u32,
        max: u32,
    },
    OpenFailure {
        recipient: u32,
        reason: u32,
    },
    Adjust {
        ch: u32,
        n: u32,
    },
    Data {
        ch: u32,
        data: Vec<u8>,
    },
    Stderr {
        ch: u32,
        data: Vec<u8>,
    },
    Eof {
        ch: u32,
    },
    Close {
        ch: u32,
    },
    ExitStatus {
        ch: u32,
        code: u32,
    },
    Success {
        ch: u32,
    },
    Failure {
        ch: u32,
    },
    Other(u8),
}

pub fn parse(p: &[u8]) -> Msg {
    let mut r = Reader::new(p);
    let typ = r.u8().unwrap_or(0);
    let mut u = || r.u32().unwrap_or(u32::MAX);
    match typ {
        1 => Msg::Disconnect(u()),
        3 => Msg::Unimplemented(u()),
        6 => Msg::ServiceAccept,
        51 => Msg::AuthFailure,
        52 => Msg::AuthSuccess,
        60 => Msg::PkOk,
        82 => Msg::RequestFailure,
        91 => Msg::OpenConfirmation {
            recipient: u(),
            sender: u(),
            window: u(),
            max: u(),
        },
        92 => Msg::OpenFailure {
            recipient: u(),
            reason: u(),
        },
        93 => Msg::Adjust { ch: u(), n: u() },
        94 => {
            let ch = r.u32().unwrap_or(u32::MAX);
            Msg::Data {
                ch,
                data: r.string().unwrap_or_default().to_vec(),
            }
        }
        95 => {
            let ch = r.u32().unwrap_or(u32::MAX);
            r.u32().ok();
            Msg::Stderr {
                ch,
                data: r.string().unwrap_or_default().to_vec(),
            }
        }
        96 => Msg::Eof { ch: u() },
        97 => Msg::Close { ch: u() },
        98 => {
            let ch = r.u32().unwrap_or(u32::MAX);
            let name = r.string().unwrap_or_default().to_vec();
            r.bool().ok();
            if name == b"exit-status" {
                Msg::ExitStatus {
                    ch,
                    code: r.u32().unwrap_or(u32::MAX),
                }
            } else {
                Msg::Other(98)
            }
        }
        99 => Msg::Success { ch: u() },
        100 => Msg::Failure { ch: u() },
        t => Msg::Other(t),
    }
}
