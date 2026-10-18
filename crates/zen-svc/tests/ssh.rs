//! The SSH engine against the crate's test client, in process: whole
//! sessions fed at once and one byte at a time, with the engine restored
//! at every step; the strict-kex rules; authentication's limits and a
//! question across a restore; the read and write bounds; rekeys from
//! either side; channel refusals; malformed input; the state's checks.

mod sshcommon;

use sshcommon::*;
use zen_svc::ssh::client::{Client, ClientOptions, Msg, msg, parse};
use zen_svc::ssh::{
    Cipher, Config, Conn, Error, Event, HostKey, Kex, Options, PublicKey, QUEUE_MAX, UNREAD_MAX,
    reason,
};

fn user() -> HostKey {
    HostKey::from_seed(&[9; 32])
}

fn copts(kex: Kex, cipher: Cipher) -> ClientOptions {
    ClientOptions {
        host: Some(host_key().public()),
        ..ClientOptions::new(kex, cipher)
    }
}

/// The client and a driven engine.
struct Pair {
    d: Driver,
    c: Client,
    got: Vec<Msg>,
}

impl Pair {
    fn new(o: ClientOptions, opts: Options, feed: usize) -> Pair {
        let (mut d, out) = Driver::new(config(opts), App::new(vec![user().public()]));
        d.feed = feed;
        let mut c = Client::new(o);
        c.input(&out).unwrap();
        Pair {
            d,
            c,
            got: Vec::new(),
        }
    }

    /// Exchanges bytes until both sides are quiet.
    fn run(&mut self) {
        loop {
            let co = self.c.take_out();
            let fresh = !co.is_empty();
            self.d.buf.extend_from_slice(&co);
            let mut so = Vec::new();
            loop {
                let (o, moved) = self.d.step();
                so.extend(o);
                if self.d.ended || !(moved || self.d.pending()) {
                    break;
                }
            }
            let msgs = self.c.input(&so).unwrap();
            self.got.extend(msgs.iter().map(|m| parse(m)));
            if !fresh && so.is_empty() {
                return;
            }
            if self.d.ended {
                return;
            }
        }
    }

    fn send(&mut self, m: &[u8]) {
        self.c.send(m);
    }

    /// Key exchange and authentication with the user key.
    fn login(&mut self) {
        self.run();
        assert!(self.c.session_id().is_some(), "{:?}", self.d.stats);
        self.send(&msg::service_request("ssh-userauth"));
        self.send(&msg::userauth_none("test"));
        self.run();
        let sid = self.c.session_id().unwrap().to_vec();
        self.send(&msg::userauth_ed25519(&sid, "test", &user(), false));
        self.run();
        self.send(&msg::userauth_ed25519(&sid, "test", &user(), true));
        self.run();
        assert_eq!(
            self.take(),
            [
                Msg::ServiceAccept,
                Msg::AuthFailure,
                Msg::PkOk,
                Msg::AuthSuccess
            ]
        );
    }

    fn take(&mut self) -> Vec<Msg> {
        std::mem::take(&mut self.got)
    }

    fn conn(&self) -> Conn {
        Conn::restore(&self.d.config, &self.d.state).unwrap()
    }

    /// The bytes a channel received, and whether it got exit 0 and CLOSE.
    fn output(&self, ch: u32, got: &[Msg]) -> (Vec<u8>, bool) {
        let mut d = Vec::new();
        let (mut exit, mut close) = (false, false);
        for m in got {
            match m {
                Msg::Data { ch: c, data } if *c == ch => d.extend_from_slice(data),
                Msg::ExitStatus { ch: c, code: 0 } if *c == ch => exit = true,
                Msg::Close { ch: c } if *c == ch => close = true,
                _ => {}
            }
        }
        (d, exit && close)
    }
}

fn session(kex: Kex, cipher: Cipher, feed: usize) {
    let mut p = Pair::new(copts(kex, cipher), Options::default(), feed);
    p.login();
    // The client's channel 5 is the engine's 0.
    p.send(&msg::channel_open(5, 1 << 20, 32_768));
    p.send(&msg::exec(0, b"cat", true));
    p.send(&msg::data(0, b"hello "));
    p.send(&msg::data(0, b"world"));
    p.send(&msg::eof(0));
    p.run();
    let got = p.take();
    assert!(
        matches!(
            got[0],
            Msg::OpenConfirmation {
                recipient: 5,
                sender: 0,
                ..
            }
        ),
        "{got:?}"
    );
    assert_eq!(got[1], Msg::Success { ch: 5 });
    assert_eq!(p.output(5, &got), (b"hello world".to_vec(), true));
    p.send(&msg::close(0));
    p.run();
    assert!(p.conn().channels().is_empty() || p.conn().channels()[0].closed);
    assert_eq!(p.d.stats.ciphers, Some((cipher, cipher)));
}

#[test]
fn whole_sessions() {
    for kex in [Kex::MlKem768X25519, Kex::Curve25519] {
        for cipher in [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm] {
            session(kex, cipher, 0);
        }
    }
}

#[test]
fn sessions_one_byte_at_a_time() {
    // Every byte is a step, and the engine is restored before each: the
    // version exchange, the key exchange and authentication resume at any
    // point.
    for kex in [Kex::MlKem768X25519, Kex::Curve25519] {
        session(kex, Cipher::Chacha20Poly1305, 1);
    }
    session(Kex::Curve25519, Cipher::Aes256Gcm, 3);
}

#[test]
fn ext_info_and_strict_kex() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.run();
    let e = p.c.ext_info.clone().expect("EXT_INFO");
    assert!(e.windows(15).any(|w| w == b"server-sig-algs"));
    // Strict kex: the sequence numbers restarted at NEWKEYS.
    assert_eq!(p.c.tx_seq(), 0);
}

/// A driver and a client whose first bytes carry a plain IGNORE between
/// the version line and the KEXINIT.
fn ignore_first(o: ClientOptions) -> Pair {
    let (d, out) = Driver::new(config(Options::default()), App::new(vec![user().public()]));
    let mut c = Client::new(o);
    c.input(&out).unwrap();
    let mut co = c.take_out();
    let nl = co.iter().position(|&b| b == b'\n').unwrap() + 1;
    co.splice(nl..nl, plain_ignore());
    let mut p = Pair {
        d,
        c,
        got: Vec::new(),
    };
    p.d.buf.extend_from_slice(&co);
    p
}

#[test]
fn an_ignore_before_kexinit_ends_a_strict_kex() {
    let mut p = ignore_first(copts(Kex::Curve25519, Cipher::Aes256Gcm));
    p.d.step();
    assert_eq!(
        p.d.stats.error,
        Some(Error::Protocol {
            reason: reason::PROTOCOL_ERROR,
            detail: "strict kex: KEXINIT was not the first packet",
        })
    );
}

#[test]
fn without_strict_kex_ignores_are_ignored() {
    // Before the KEXINIT and during the key exchange. (The client's
    // sequence numbers don't count the injected packet, so it is one the
    // client sends itself, during the key exchange, that is tested here.)
    let mut o = copts(Kex::Curve25519, Cipher::Aes256Gcm);
    o.strict = false;
    let (d, out) = Driver::new(config(Options::default()), App::new(vec![user().public()]));
    let mut c = Client::new(o);
    c.input(&out).unwrap();
    c.send(&msg::ignore(b"x"));
    let mut p = Pair {
        d,
        c,
        got: Vec::new(),
    };
    p.login();
    p.send(&msg::channel_open(0, 1 << 20, 32_768));
    p.send(&msg::exec(0, b"echo ok", false));
    p.run();
    let got = p.take();
    assert_eq!(p.output(0, &got), (b"ok\n".to_vec(), true));
    // A plain IGNORE before the KEXINIT is read, without strict kex.
    let mut o = copts(Kex::Curve25519, Cipher::Aes256Gcm);
    o.strict = false;
    let mut p = ignore_first(o);
    p.d.step();
    assert_eq!(p.d.stats.error, None);
    assert!(p.d.buf.is_empty());
}

#[test]
fn a_wrong_guess_is_ignored() {
    let mut o = copts(Kex::Curve25519, Cipher::Chacha20Poly1305);
    o.wrong_guess = true;
    let mut p = Pair::new(o, Options::default(), 0);
    p.login();
}

#[test]
fn authentication_limits() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.run();
    p.send(&msg::service_request("ssh-userauth"));
    for _ in 0..20 {
        p.send(&msg::userauth_none("test"));
    }
    p.run();
    let got = p.take();
    assert_eq!(got.len(), 21);
    assert!(got[1..].iter().all(|m| *m == Msg::AuthFailure));
    p.send(&msg::userauth_none("test"));
    p.run();
    assert_eq!(
        p.take(),
        [Msg::Disconnect(reason::NO_MORE_AUTH_METHODS_AVAILABLE)]
    );

    // A connection message before authentication ends the connection.
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.run();
    p.send(&msg::channel_open(0, 1 << 20, 32_768));
    p.run();
    assert_eq!(p.take(), [Msg::Disconnect(reason::PROTOCOL_ERROR)]);
}

#[test]
fn a_question_survives_a_restore() {
    let cfg = config(Options::default());
    let (mut conn, out) = Conn::new(&cfg, now());
    let mut c = Client::new(copts(Kex::Curve25519, Cipher::Chacha20Poly1305));
    c.input(&out).unwrap();
    let mut buf = Vec::new();
    let step = |conn: &mut Conn, c: &mut Client, buf: &mut Vec<u8>| -> (Vec<Event>, bool) {
        buf.extend(c.take_out());
        let p = conn.input(buf, false, now());
        buf.drain(..p.consumed);
        c.input(&p.out).unwrap();
        (p.events, p.held)
    };
    for _ in 0..4 {
        step(&mut conn, &mut c, &mut buf);
    }
    let sid = c.session_id().unwrap().to_vec();
    c.send(&msg::service_request("ssh-userauth"));
    c.send(&msg::userauth_ed25519(&sid, "test", &user(), true));
    // Pipelined behind the question: not read until it is answered.
    c.send(&msg::channel_open(0, 1 << 20, 32_768));
    let (ev, held) = step(&mut conn, &mut c, &mut buf);
    assert!(held);
    let q = match &ev[..] {
        [Event::Authorize(a)] => a.clone(),
        e => panic!("{e:?}"),
    };
    assert!(q.signed);
    assert_eq!(q.key, user().public());
    assert!(!buf.is_empty(), "the channel open waits");
    // Another handler takes over: it asks its own application.
    let mut conn = Conn::restore(&cfg, &conn.state()).unwrap();
    assert_eq!(conn.pending_authorize(), Some(q));
    let o = conn.authorize(true).unwrap();
    let got: Vec<Msg> = c.input(&o).unwrap().iter().map(|m| parse(m)).collect();
    assert_eq!(got, [Msg::AuthSuccess]);
    assert_eq!(conn.authorize(true), Err(Error::NotAsked));
    let (ev, held) = step(&mut conn, &mut c, &mut buf);
    assert_eq!(ev, [Event::ChannelOpen { ch: 0 }]);
    assert!(!held);
    assert_eq!(conn.user(), Some("test"));
}

#[test]
fn the_read_bound_holds_back_input() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Chacha20Poly1305),
        Options::default(),
        0,
    );
    p.login();
    p.send(&msg::channel_open(0, 1 << 20, 32_768));
    p.send(&msg::request(0, "shell", false));
    p.run();
    let mut c = p.conn();
    let chunk = vec![b'a'; 10_000];
    for _ in 0..8 {
        p.c.send(&msg::data(0, &chunk));
    }
    // Hand the input to a bare engine: no application consumes it.
    let input = p.c.take_out();
    let r = c.input(&input, false, now());
    assert!(r.consumed < input.len());
    assert!(r.held);
    assert!(c.channel_data(0).len() <= UNREAD_MAX);
    assert_eq!(c.channel_data(0).len(), 30_000);
    // Nothing consumed: input again holds back as before.
    let again = c.input(&input[r.consumed..], false, now());
    assert_eq!((again.consumed, again.held), (0, true));
    // Consuming lets reading go on.
    let mut rest = input[r.consumed..].to_vec();
    let mut total = 30_000;
    while !rest.is_empty() {
        c.consume(0, c.channel_data(0).len()).unwrap();
        let r = c.input(&rest, false, now());
        assert!(r.consumed > 0);
        assert_eq!(r.held, r.consumed < rest.len());
        rest.drain(..r.consumed);
        total += c.channel_data(0).len();
        assert!(c.channel_data(0).len() <= UNREAD_MAX);
    }
    assert_eq!(total, 80_000);
}

#[test]
fn the_write_bound_and_the_queue() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.login();
    p.send(&msg::channel_open(0, 1000, 32_768));
    p.send(&msg::request(0, "shell", false));
    p.run();
    let mut c = p.conn();
    let data = vec![7u8; 50_000];
    let (taken, out) = c.write(0, &data).unwrap();
    assert_eq!(taken, 1000 + QUEUE_MAX);
    let got: Vec<Msg> = p.c.input(&out).unwrap().iter().map(|m| parse(m)).collect();
    assert_eq!(
        got,
        [Msg::Data {
            ch: 0,
            data: vec![7; 1000]
        }]
    );
    // Full: nothing more is taken, on any channel or stream.
    assert_eq!(c.write(0, b"x").unwrap().0, 0);
    assert_eq!(c.write_stderr(0, b"x").unwrap().0, 0);
    // EOF, exit status and CLOSE wait behind the queue.
    assert!(c.exit_status(0, 0).unwrap().is_empty());
    assert!(c.eof(0).unwrap().is_empty());
    assert!(c.close(0).unwrap().is_empty());
    assert_eq!(c.write(0, b"x"), Err(Error::ChannelEnded));
    // The state carries the queue to another handler.
    let mut c = Conn::restore(&p.d.config, &c.state()).unwrap();
    p.c.send(&msg::adjust(0, 100_000));
    let r = c.input(&p.c.take_out(), false, now());
    let got: Vec<Msg> =
        p.c.input(&r.out)
            .unwrap()
            .iter()
            .map(|m| parse(m))
            .collect();
    let (d, done) = p.output(0, &got);
    assert_eq!(d.len(), QUEUE_MAX);
    assert!(done);
    assert_eq!(
        got[got.len() - 3..],
        [
            Msg::Eof { ch: 0 },
            Msg::ExitStatus { ch: 0, code: 0 },
            Msg::Close { ch: 0 }
        ]
    );
}

#[test]
fn rekeys_from_either_side_mid_transfer() {
    for feed in [0, 5] {
        let mut p = Pair::new(
            copts(Kex::MlKem768X25519, Cipher::Chacha20Poly1305),
            Options::default(),
            feed,
        );
        p.login();
        p.send(&msg::channel_open(0, 1 << 20, 32_768));
        p.send(&msg::exec(0, b"cat", false));
        p.run();
        p.take();
        let mut sent = Vec::new();
        for i in 0..6u8 {
            let d = vec![i; 3000];
            p.send(&msg::data(0, &d));
            sent.extend_from_slice(&d);
            if i == 1 {
                // The client rekeys with data in flight around it.
                p.c.rekey();
            }
            if i == 3 {
                // The server rekeys: its KEXINIT goes out at once.
                let mut c = p.conn();
                let o = c.rekey();
                assert!(!o.is_empty());
                p.d.state = c.state();
                p.c.input(&o).unwrap();
            }
            p.send(&msg::data(0, b"."));
            sent.push(b'.');
            p.run();
        }
        p.send(&msg::eof(0));
        p.run();
        let got = p.take();
        assert_eq!(p.output(0, &got), (sent, true), "feed {feed}");
        assert_eq!(p.d.stats.kexes, 3);
        assert_eq!(p.c.kexes, 3);
    }
}

#[test]
fn channel_refusals() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.login();
    p.send(&msg::global_request("tcpip-forward", true));
    p.send(&msg::global_request("no-more-sessions@openssh.com", false));
    for i in 0..11 {
        p.send(&msg::channel_open(100 + i, 1 << 20, 32_768));
    }
    p.send(&msg::request(0, "pty-req", true));
    p.send(&msg::request(0, "shell", true));
    p.send(&msg::request(0, "shell", true));
    p.send(&msg::exec(0, b"cat", true));
    p.run();
    let got = p.take();
    assert_eq!(got[0], Msg::RequestFailure);
    assert_eq!(got.len(), 1 + 11 + 4, "{got:?}");
    assert!(matches!(
        got[11],
        Msg::OpenFailure {
            recipient: 110,
            reason: 4
        }
    ));
    assert_eq!(
        got[12..],
        [
            Msg::Failure { ch: 100 },
            Msg::Success { ch: 100 },
            Msg::Failure { ch: 100 },
            Msg::Failure { ch: 100 }
        ]
    );
    // An unknown channel ends the connection.
    p.send(&msg::data(77, b"x"));
    p.run();
    assert_eq!(p.take(), [Msg::Disconnect(reason::PROTOCOL_ERROR)]);
}

#[test]
fn the_client_disconnects() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.login();
    p.send(&msg::disconnect(11));
    p.run();
    assert!(p.d.ended);
    assert_eq!(
        p.d.stats.error,
        Some(Error::Disconnected {
            reason: 11,
            message: "bye".into()
        })
    );
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Malformed messages, encrypted properly, after login: each one ends in
/// progress or a DISCONNECT, never a panic, and the state restores.
#[test]
fn malformed_messages() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let valid = [
        msg::channel_open(0, 1 << 20, 32_768),
        msg::exec(0, b"cat", true),
        msg::data(0, b"data"),
        msg::adjust(0, 1000),
        msg::eof(0),
        msg::close(0),
        msg::global_request("x", true),
        msg::service_request("ssh-userauth"),
        msg::userauth_none("test"),
        msg::ignore(b"i"),
        vec![20, 1, 2, 3],
        vec![21],
        vec![30, 0, 0, 0, 32],
        vec![200],
    ];
    let mut ended = 0;
    for round in 0..300 {
        let mut p = Pair::new(
            copts(Kex::Curve25519, Cipher::Chacha20Poly1305),
            Options::default(),
            0,
        );
        p.login();
        p.send(&msg::channel_open(0, 1 << 20, 32_768));
        p.send(&msg::request(0, "shell", false));
        p.run();
        let mut m = valid[rng.below(valid.len())].clone();
        match rng.below(4) {
            0 => m.truncate(rng.below(m.len()).max(1)),
            1 => m.extend((0..rng.below(8) + 1).map(|_| rng.next() as u8)),
            2 => {
                let i = rng.below(m.len());
                m[i] ^= 1 << rng.below(8);
            }
            _ => {
                // A length field made huge.
                if m.len() >= 5 {
                    let i = 1 + rng.below(m.len() - 4);
                    m[i..i + 4].copy_from_slice(&u32::MAX.to_be_bytes());
                }
            }
        }
        if m.is_empty() {
            continue;
        }
        p.send(&m);
        p.send(&msg::data(0, b"after\n"));
        p.run();
        let state = p.d.state.clone();
        Conn::restore(&p.d.config, &state).unwrap_or_else(|e| panic!("round {round}: {e}"));
        if p.d.ended {
            ended += 1;
            assert!(
                matches!(p.d.stats.error, Some(Error::Protocol { .. })),
                "round {round}: {:?}",
                p.d.stats
            );
        }
    }
    assert!(ended > 50, "{ended}");
}

/// Bit flips in the encrypted stream don't authenticate.
#[test]
fn flipped_bits() {
    let mut rng = Rng(7);
    for cipher in [Cipher::Chacha20Poly1305, Cipher::Aes256Gcm] {
        for _ in 0..40 {
            let mut p = Pair::new(copts(Kex::Curve25519, cipher), Options::default(), 0);
            p.login();
            p.send(&msg::channel_open(0, 1 << 20, 32_768));
            let mut b = p.c.take_out();
            let i = rng.below(b.len());
            b[i] ^= 1 << rng.below(8);
            p.d.buf.extend_from_slice(&b);
            let before = p.d.buf.len();
            p.d.step();
            let e = p.d.stats.error.clone();
            // A flipped length may also just ask for more input.
            assert!(
                matches!(
                    e,
                    Some(Error::Protocol {
                        reason: reason::MAC_ERROR | reason::PROTOCOL_ERROR,
                        ..
                    })
                ) || (e.is_none() && p.d.buf.len() == before),
                "{e:?}"
            );
        }
    }
}

#[test]
fn garbage_before_the_version() {
    for (input, why) in [
        (
            b"SSH-1.5-old\r\n".to_vec(),
            reason::PROTOCOL_VERSION_NOT_SUPPORTED,
        ),
        (vec![b'x'; 300], reason::PROTOCOL_ERROR),
        (b"a\n".repeat(21), reason::PROTOCOL_ERROR),
    ] {
        let cfg = config(Options::default());
        let (mut c, _) = Conn::new(&cfg, now());
        let p = c.input(&input, false, now());
        assert!(
            matches!(p.error, Some(Error::Protocol { reason, .. }) if reason == why),
            "{:?}",
            p.error
        );
    }
    // Twenty lines are fine, and a bare LF.
    let cfg = config(Options::default());
    let (mut c, _) = Conn::new(&cfg, now());
    let mut input = b"a\r\n".repeat(20);
    input.extend_from_slice(b"SSH-2.0-x\n");
    let p = c.input(&input, false, now());
    assert_eq!((p.consumed, p.error), (input.len(), None));
}

#[test]
fn state_checks() {
    let mut p = Pair::new(
        copts(Kex::Curve25519, Cipher::Aes256Gcm),
        Options::default(),
        0,
    );
    p.login();
    p.send(&msg::channel_open(0, 10, 32_768));
    p.send(&msg::request(0, "shell", false));
    p.run();
    let cfg = p.d.config.clone();
    let mut c = p.conn();
    // Full buffers: unread data, a full queue in many pieces.
    p.c.send(&msg::data(0, &vec![1u8; 30_000]));
    c.input(&p.c.take_out(), false, now());
    for i in 0..200 {
        let d = vec![i as u8; 200];
        if i % 2 == 0 {
            c.write(0, &d).unwrap();
        } else {
            c.write_stderr(0, &d).unwrap();
        }
    }
    let s = c.state();
    assert!(s.len() < 65_536, "{}", s.len());
    assert!(s.len() > 30_000 + QUEUE_MAX / 2, "{}", s.len());
    // A round trip is exact.
    let c2 = Conn::restore(&cfg, &s).unwrap();
    assert_eq!(c2.state(), s);
    // Corruptions are refused, not misread.
    use ciborium::Value;
    let v: Value = ciborium::from_reader(&s[..]).unwrap();
    type Map = Vec<(Value, Value)>;
    let edit = |f: &dyn Fn(&mut Map)| {
        let mut v = v.clone();
        let Value::Map(m) = &mut v else {
            unreachable!()
        };
        f(m);
        let mut out = Vec::new();
        ciborium::into_writer(&v, &mut out).unwrap();
        Conn::restore(&cfg, &out)
    };
    let set = |k: &'static str, nv: Value| {
        move |m: &mut Map| {
            for (kk, vv) in m.iter_mut() {
                if *kk == Value::Text(k.into()) {
                    *vv = nv.clone();
                }
            }
        }
    };
    assert!(matches!(
        edit(&set("v", Value::Integer(2.into()))),
        Err(Error::BadState(_))
    ));
    assert!(matches!(
        edit(&set("sid", Value::Bytes(vec![0; 31]))),
        Err(Error::BadState(_))
    ));
    assert!(matches!(
        edit(&set("flags", Value::Integer(9.into()))),
        Err(Error::BadState(_))
    ));
    assert!(matches!(
        edit(&|m| m.push((Value::Text("x".into()), Value::Null))),
        Err(Error::BadState(_))
    ));
    assert!(matches!(
        edit(&|m| m.retain(|(k, _)| *k != Value::Text("tx".into()))),
        Err(Error::BadState(_))
    ));
    assert!(matches!(
        Conn::restore(&cfg, b"\xa0"),
        Err(Error::BadState(_))
    ));
    // A key of the wrong length.
    let tx = |m: &mut Map| {
        for (k, v) in m.iter_mut() {
            if *k == Value::Text("tx".into()) {
                let Value::Map(d) = v else { unreachable!() };
                for (kk, vv) in d.iter_mut() {
                    if *kk == Value::Text("keys".into()) {
                        let Value::Map(keys) = vv else { unreachable!() };
                        for (k3, v3) in keys.iter_mut() {
                            if *k3 == Value::Text("key".into()) {
                                *v3 = Value::Bytes(vec![0; 31]);
                            }
                        }
                    }
                }
            }
        }
    };
    assert!(matches!(edit(&tx), Err(Error::BadState(_))));
}

#[test]
fn keys_and_config() {
    let k = HostKey::from_seed(&[1; 32]);
    let line = k.public_line();
    assert_eq!(PublicKey::from_line(&line).unwrap(), k.public());
    assert!(PublicKey::from_line("ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=").is_err());
    assert!(
        Config::new(
            k.clone(),
            Options {
                software: "a b".into(),
                ..Options::default()
            }
        )
        .is_err()
    );
    assert!(
        Config::new(
            k.clone(),
            Options {
                kex: vec![],
                ..Options::default()
            }
        )
        .is_err()
    );
    assert!(
        Config::new(
            k,
            Options {
                window: 10,
                ..Options::default()
            }
        )
        .is_err()
    );
}
