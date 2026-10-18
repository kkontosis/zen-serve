//! `engine_state` (svc.md §7.2): a CBOR map, checked strictly on restore.

use super::auth::{self, Ask, Auth};
use super::channel::{self, Channel, Request};
use super::cipher::{Cipher, Dir, Keys};
use super::kex::{self, Chosen, Kex};
use super::{
    Config, Conn, ENDED, Error, HELD_BYTES, HELD_MSGS, KexState, PEER_ENDED, PublicKey, QUEUE_MAX,
    QUEUE_PIECES, UNREAD_MAX,
};
use crate::cbor::{Fields, encode, text, zeroize_value};
use ciborium::Value;

const VERSION: u64 = 1;

fn uint(n: impl Into<u64>) -> Value {
    Value::Integer(n.into().into())
}

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec())
}

fn keys_value(k: &Keys) -> Value {
    let mut m = vec![
        (text("c"), uint(k.cipher.code())),
        (text("key"), bytes(&k.key)),
    ];
    if !k.iv.is_empty() {
        m.push((text("iv"), bytes(&k.iv)));
    }
    Value::Map(m)
}

fn dir_value(d: &Dir) -> Value {
    let mut m = Vec::new();
    if let Some(k) = &d.keys {
        m.push((text("keys"), keys_value(k)));
    }
    m.extend([
        (text("seq"), uint(d.seq)),
        (text("bytes"), uint(d.bytes)),
        (text("packets"), uint(d.packets)),
    ]);
    Value::Map(m)
}

fn kex_value(k: &KexState) -> Value {
    let mut m = Vec::new();
    if let Some(o) = &k.ours {
        m.push((text("ours"), bytes(o)));
    }
    if let Some(t) = &k.theirs {
        m.push((text("theirs"), bytes(t)));
    }
    if let Some(c) = &k.chosen {
        m.push((text("alg"), uint(c.kex.code())));
        m.push((text("c2s"), uint(c.c2s.code())));
        m.push((text("s2c"), uint(c.s2c.code())));
    }
    m.push((text("replied"), Value::Bool(k.replied)));
    m.push((text("skip"), Value::Bool(k.skip)));
    if let Some((a, b)) = k.ahead {
        m.push((text("ahead"), Value::Array(vec![uint(a), uint(b)])));
    }
    Value::Map(m)
}

fn chan_value(c: &Channel) -> Value {
    let mut m = vec![
        (text("id"), uint(c.id)),
        (text("peer"), uint(c.peer)),
        (text("peer_window"), uint(c.peer_window)),
        (text("peer_max"), uint(c.peer_max)),
        (text("window"), uint(c.window)),
        (text("unread"), bytes(&c.unread)),
        (
            text("queue"),
            Value::Array(
                c.queue
                    .iter()
                    .map(|(k, b)| Value::Array(vec![uint(*k), bytes(b)]))
                    .collect(),
            ),
        ),
    ];
    if let Some(r) = c.req {
        m.push((text("req"), uint(r.code())));
    }
    if let Some(e) = c.exit {
        m.push((text("exit"), uint(e)));
    }
    m.push((text("flags"), uint(c.flags)));
    Value::Map(m)
}

impl Conn {
    /// The state to carry to the next step or handler.
    pub fn state(&self) -> Vec<u8> {
        let mut m = vec![(text("v"), uint(VERSION)), (text("vs"), bytes(&self.vs))];
        if let Some(vc) = &self.vc {
            m.push((text("vc"), bytes(vc)));
        }
        m.push((text("lines"), uint(self.lines)));
        if let Some(sid) = &self.sid {
            m.push((text("sid"), bytes(sid)));
        }
        m.push((text("strict"), Value::Bool(self.strict)));
        m.push((text("tx"), dir_value(&self.tx)));
        m.push((text("rx"), dir_value(&self.rx)));
        if let Some(k) = &self.rx_next {
            m.push((text("rx_next"), keys_value(k)));
        }
        if let Some(k) = &self.kex {
            m.push((text("kex"), kex_value(k)));
        }
        m.push((text("kex_at"), uint(self.kex_at)));
        m.push((text("kexes"), uint(self.kexes)));
        let mut a = vec![
            (text("state"), uint(self.auth.state)),
            (text("tries"), uint(self.auth.tries)),
        ];
        if let Some(u) = &self.auth.user {
            a.push((text("user"), text(u)));
        }
        if let Some(q) = &self.auth.ask {
            a.push((
                text("ask"),
                Value::Map(vec![
                    (text("user"), text(&q.user)),
                    (text("alg"), text(&q.alg)),
                    (text("key"), bytes(q.key.blob())),
                    (text("signed"), Value::Bool(q.signed)),
                ]),
            ));
        }
        m.push((text("auth"), Value::Map(a)));
        m.push((
            text("ch"),
            Value::Array(self.chans.iter().map(chan_value).collect()),
        ));
        m.push((
            text("held"),
            Value::Array(self.held.iter().map(|h| bytes(h)).collect()),
        ));
        m.push((text("flags"), uint(self.flags)));
        encode(Value::Map(m))
    }

    /// Rebuilds a connection from [`Conn::state`]. Anything unexpected is
    /// refused: an unknown `v` or field, a wrong type or length, or a
    /// bound exceeded.
    pub fn restore(config: &Config, state: &[u8]) -> Result<Conn, Error> {
        let v: Value = ciborium::from_reader(state).map_err(|_| Error::BadState("not CBOR"))?;
        let mut m = Fields::new(v)?;
        if m.uint("v")? != VERSION {
            return Err(Error::BadState("unknown v"));
        }
        let vs = m.bytes("vs")?;
        let vc = opt_bytes(&mut m, "vc")?;
        if vs.len() > 253 || vc.as_ref().is_some_and(|v| v.len() > 253) {
            return Err(Error::BadState("a version line too long"));
        }
        let lines = u32_of(m.uint("lines")?)?;
        if lines > super::MAX_LINES {
            return Err(Error::BadState("lines"));
        }
        let sid = opt_bytes(&mut m, "sid")?;
        if sid.as_ref().is_some_and(|s| s.len() != 32) {
            return Err(Error::BadState("a session id that isn't 32 bytes"));
        }
        let strict = boolean(&mut m, "strict")?;
        let tx = dir(m.take("tx")?)?;
        let rx = dir(m.take("rx")?)?;
        let rx_next = match m.opt("rx_next") {
            Some(v) => Some(keys(v)?),
            None => None,
        };
        let kex = match m.opt("kex") {
            Some(v) => Some(kex_state(v)?),
            None => None,
        };
        let kex_at = m.uint("kex_at")?;
        let kexes = m.uint("kexes")?;
        let auth = auth_state(m.take("auth")?)?;
        let chans = match m.take("ch")? {
            Value::Array(a) => a.into_iter().map(chan).collect::<Result<Vec<_>, _>>()?,
            v => {
                zeroize_value(v);
                return Err(Error::BadState("ch"));
            }
        };
        let held = match m.take("held")? {
            Value::Array(a) => a
                .into_iter()
                .map(|v| match v {
                    Value::Bytes(b) if !b.is_empty() => Ok(b),
                    _ => Err(Error::BadState("held")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(Error::BadState("held")),
        };
        let flags = u8::try_from(m.uint("flags")?).map_err(|_| Error::BadState("flags"))?;
        m.done()?;
        if flags & !(PEER_ENDED | ENDED) != 0 {
            return Err(Error::BadState("flags"));
        }
        // The bounds of svc.md §7.2.
        let unread: usize = chans.iter().map(|c| c.unread.len()).sum();
        let queued: usize = chans.iter().map(Channel::queued).sum();
        let pieces: usize = chans.iter().map(|c| c.queue.len()).sum();
        let held_bytes: usize = held.iter().map(Vec::len).sum();
        if chans.len() > channel::MAX_CHANNELS
            || unread > UNREAD_MAX
            || queued > QUEUE_MAX
            || pieces > QUEUE_PIECES
            || held.len() > HELD_MSGS + 1
            || held_bytes > HELD_BYTES + 1024
        {
            return Err(Error::BadState("over a bound"));
        }
        let mut ids: Vec<u32> = chans.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != chans.len() {
            return Err(Error::BadState("a repeated channel id"));
        }
        if (auth.state == auth::DONE) != auth.user.is_some()
            || (auth.user.is_some() && sid.is_none())
        {
            return Err(Error::BadState("auth"));
        }
        if (vc.is_none() && sid.is_some()) || (sid.is_none() && kex.is_none()) {
            return Err(Error::BadState("a key exchange out of order"));
        }
        Ok(Conn {
            cfg: config.clone(),
            vs,
            vc,
            lines,
            sid,
            strict,
            tx,
            rx,
            rx_next,
            kex,
            kex_at,
            kexes,
            auth,
            chans,
            held,
            flags,
            out: Vec::new(),
            events: Vec::new(),
        })
    }
}

fn u32_of(n: u64) -> Result<u32, Error> {
    u32::try_from(n).map_err(|_| Error::BadState("not a u32"))
}

fn boolean(m: &mut Fields, k: &str) -> Result<bool, Error> {
    match m.take(k)? {
        Value::Bool(b) => Ok(b),
        _ => Err(Error::BadState("not a bool")),
    }
}

fn opt_bytes(m: &mut Fields, k: &str) -> Result<Option<Vec<u8>>, Error> {
    match m.opt(k) {
        Some(Value::Bytes(b)) => Ok(Some(b)),
        Some(v) => {
            zeroize_value(v);
            Err(Error::BadState("not bytes"))
        }
        None => Ok(None),
    }
}

fn keys(v: Value) -> Result<Keys, Error> {
    let mut m = Fields::new(v)?;
    let c = Cipher::from_code(m.uint("c")?).ok_or(Error::BadState("unknown cipher"))?;
    let key = zeroize::Zeroizing::new(m.bytes("key")?);
    let iv = zeroize::Zeroizing::new(opt_bytes(&mut m, "iv")?.unwrap_or_default());
    m.done()?;
    Keys::new(c, &key, &iv).ok_or(Error::BadState("a key or IV of the wrong length"))
}

fn dir(v: Value) -> Result<Dir, Error> {
    let mut m = Fields::new(v)?;
    let keys = match m.opt("keys") {
        Some(v) => Some(keys(v)?),
        None => None,
    };
    let d = Dir {
        keys,
        seq: u32_of(m.uint("seq")?)?,
        bytes: m.uint("bytes")?,
        packets: m.uint("packets")?,
    };
    m.done()?;
    Ok(d)
}

fn kex_state(v: Value) -> Result<KexState, Error> {
    let mut m = Fields::new(v)?;
    let ours = opt_bytes(&mut m, "ours")?;
    let theirs = opt_bytes(&mut m, "theirs")?;
    if theirs.as_ref().is_some_and(|t| t.len() > kex::MAX_KEXINIT)
        || ours.as_ref().is_some_and(|o| o.len() > 4096)
    {
        return Err(Error::BadState("a KEXINIT too long"));
    }
    let alg = m.opt("alg");
    let chosen = match alg {
        Some(Value::Integer(a)) => {
            let a = u64::try_from(a).map_err(|_| Error::BadState("alg"))?;
            Some(Chosen {
                kex: Kex::from_code(a).ok_or(Error::BadState("unknown kex"))?,
                c2s: Cipher::from_code(m.uint("c2s")?).ok_or(Error::BadState("unknown cipher"))?,
                s2c: Cipher::from_code(m.uint("s2c")?).ok_or(Error::BadState("unknown cipher"))?,
                wrong_guess: false,
            })
        }
        Some(_) => return Err(Error::BadState("alg")),
        None => None,
    };
    let replied = boolean(&mut m, "replied")?;
    let skip = boolean(&mut m, "skip")?;
    let ahead = match m.opt("ahead") {
        Some(Value::Array(a)) if a.len() == 2 => {
            let n = |v: &Value| match v {
                Value::Integer(i) => u32::try_from(*i).map_err(|_| Error::BadState("ahead")),
                _ => Err(Error::BadState("ahead")),
            };
            Some((n(&a[0])?, n(&a[1])?))
        }
        Some(_) => return Err(Error::BadState("ahead")),
        None => None,
    };
    m.done()?;
    // Until the reply both KEXINITs are kept (ours always: it was sent
    // first); after it, neither.
    let consistent = if replied {
        ours.is_none() && theirs.is_none() && chosen.is_some()
    } else {
        ours.is_some() && theirs.is_some() == chosen.is_some() && ahead.is_none()
    };
    if !consistent {
        return Err(Error::BadState("a key exchange out of order"));
    }
    Ok(KexState {
        ours,
        theirs,
        chosen,
        replied,
        skip,
        ahead,
    })
}

fn auth_state(v: Value) -> Result<Auth, Error> {
    let mut m = Fields::new(v)?;
    let state = u8::try_from(m.uint("state")?).map_err(|_| Error::BadState("auth state"))?;
    if state > auth::DONE {
        return Err(Error::BadState("auth state"));
    }
    let tries = u32_of(m.uint("tries")?)?;
    let user = match m.opt("user") {
        Some(Value::Text(t)) => Some(t),
        Some(_) => return Err(Error::BadState("user")),
        None => None,
    };
    let ask = match m.opt("ask") {
        Some(v) => {
            let mut a = Fields::new(v)?;
            let text_of = |a: &mut Fields, k: &str| match a.take(k) {
                Ok(Value::Text(t)) => Ok(t),
                _ => Err(Error::BadState("ask")),
            };
            let user = text_of(&mut a, "user")?;
            let alg = text_of(&mut a, "alg")?;
            let key =
                PublicKey::from_blob(&a.bytes("key")?).map_err(|_| Error::BadState("ask key"))?;
            let signed = boolean(&mut a, "signed")?;
            a.done()?;
            if !key.allows(&alg) || user.len() > 256 {
                return Err(Error::BadState("ask"));
            }
            Some(Ask {
                user,
                alg,
                key,
                signed,
            })
        }
        None => None,
    };
    m.done()?;
    if ask.is_some() && state != auth::ACCEPTED {
        return Err(Error::BadState("ask"));
    }
    Ok(Auth {
        state,
        tries,
        user,
        ask,
    })
}

fn chan(v: Value) -> Result<Channel, Error> {
    let mut m = Fields::new(v)?;
    let id = u32_of(m.uint("id")?)?;
    let peer = u32_of(m.uint("peer")?)?;
    let peer_window = u32_of(m.uint("peer_window")?)?;
    let peer_max = u32_of(m.uint("peer_max")?)?;
    let window = u32_of(m.uint("window")?)?;
    let unread = m.bytes("unread")?;
    let queue = match m.take("queue")? {
        Value::Array(a) => a
            .into_iter()
            .map(|p| match p {
                Value::Array(p) if p.len() == 2 => match (&p[0], &p[1]) {
                    (Value::Integer(k), Value::Bytes(b)) if !b.is_empty() => {
                        let k = u8::try_from(*k).map_err(|_| Error::BadState("queue"))?;
                        if k > 1 {
                            return Err(Error::BadState("queue"));
                        }
                        Ok((k, b.clone()))
                    }
                    _ => Err(Error::BadState("queue")),
                },
                _ => Err(Error::BadState("queue")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(Error::BadState("queue")),
    };
    let req = match m.opt("req") {
        Some(Value::Integer(r)) => Some(
            u64::try_from(r)
                .ok()
                .and_then(Request::from_code)
                .ok_or(Error::BadState("req"))?,
        ),
        Some(_) => return Err(Error::BadState("req")),
        None => None,
    };
    let exit = match m.opt("exit") {
        Some(Value::Integer(e)) => Some(u32::try_from(e).map_err(|_| Error::BadState("exit"))?),
        Some(_) => return Err(Error::BadState("exit")),
        None => None,
    };
    let flags = u8::try_from(m.uint("flags")?).map_err(|_| Error::BadState("flags"))?;
    m.done()?;
    if flags & !channel::FLAGS != 0 || peer_max == 0 {
        return Err(Error::BadState("channel"));
    }
    Ok(Channel {
        id,
        peer,
        peer_window,
        peer_max,
        window,
        unread,
        queue,
        req,
        exit,
        flags,
    })
}
