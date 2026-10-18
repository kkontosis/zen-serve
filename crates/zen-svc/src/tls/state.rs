//! `engine_state` (svc.md §7.1): a CBOR map, checked strictly on restore.

use super::session::{CLOSED, Dir, PEER_CLOSED, PLAIN_HELD, SENT_CLOSE, Session, Suite};
use super::{Config, Error};
use crate::cbor::{Fields, text};
use ciborium::Value;
use rustls::ProtocolVersion;
use zen_tls_rustcrypto::record::MAX_PLAIN;
use zeroize::Zeroizing;

const VERSION: u64 = 1;

fn dir_value(d: &Dir, tls13: bool) -> Value {
    let mut m = vec![(text("seq"), Value::Integer(d.seq.into()))];
    if tls13 {
        m.push((text("secret"), Value::Bytes(d.secret.to_vec())));
    } else {
        m.push((text("key"), Value::Bytes(d.key_bytes.to_vec())));
        m.push((text("iv"), Value::Bytes(d.iv.to_vec())));
    }
    Value::Map(m)
}

impl Session {
    /// The state to carry to the next step or handler.
    pub fn state(&self) -> Vec<u8> {
        let tls13 = self.suite.tls13;
        let ver: u16 = self.suite.version().into();
        let mut m = vec![
            (text("v"), Value::Integer(VERSION.into())),
            (text("ver"), Value::Integer(ver.into())),
            (text("suite"), Value::Integer(self.suite.id.into())),
            (text("tx"), dir_value(&self.tx, tls13)),
            (text("rx"), dir_value(&self.rx, tls13)),
            (text("plain"), Value::Bytes(self.plain.clone())),
        ];
        if let Some(sni) = &self.sni {
            m.push((text("sni"), text(sni)));
        }
        if let Some(alpn) = &self.alpn {
            m.push((text("alpn"), Value::Bytes(alpn.clone())));
        }
        m.push((text("flags"), Value::Integer(self.flags.into())));
        // The map holds copies of the keys; `encode` zeroizes it.
        crate::cbor::encode(Value::Map(m))
    }

    /// Rebuilds a session from [`Session::state`]. Anything unexpected is
    /// refused: an unknown `v`, version or suite, a missing or unknown
    /// field, a wrong length.
    pub fn restore(config: &Config, state: &[u8]) -> Result<Session, Error> {
        let v: Value = ciborium::from_reader(state).map_err(|_| Error::BadState("not CBOR"))?;
        let mut m = Fields::new(v)?;
        if m.uint("v")? != VERSION {
            return Err(Error::BadState("unknown v"));
        }
        let ver = match m.uint("ver")? {
            0x0304 => ProtocolVersion::TLSv1_3,
            0x0303 => ProtocolVersion::TLSv1_2,
            _ => return Err(Error::BadState("unknown version")),
        };
        let id = u16::try_from(m.uint("suite")?).map_err(|_| Error::BadState("suite"))?;
        let suite = Suite::from_id(id, ver).ok_or(Error::BadState("unknown suite"))?;
        let tx = dir(suite, m.take("tx")?)?;
        let rx = dir(suite, m.take("rx")?)?;
        let plain = m.bytes("plain")?;
        if plain.len() > PLAIN_HELD + MAX_PLAIN {
            return Err(Error::BadState("too much plaintext"));
        }
        let sni = match m.opt("sni") {
            Some(Value::Text(s)) => Some(s),
            Some(_) => return Err(Error::BadState("sni")),
            None => None,
        };
        let alpn = match m.opt("alpn") {
            Some(Value::Bytes(b)) if !b.is_empty() && b.len() < 256 => Some(b),
            Some(_) => return Err(Error::BadState("alpn")),
            None => None,
        };
        let flags = u8::try_from(m.uint("flags")?).map_err(|_| Error::BadState("flags"))?;
        if flags & !(PEER_CLOSED | SENT_CLOSE | CLOSED) != 0 {
            return Err(Error::BadState("flags"));
        }
        m.done()?;
        let mut s = Session::new(suite, tx, rx, plain, sni, alpn, config.key_limit);
        s.flags = flags;
        Ok(s)
    }
}

fn dir(suite: Suite, v: Value) -> Result<Dir, Error> {
    let mut m = Fields::new(v)?;
    let seq = m.uint("seq")?;
    let d = if suite.tls13 {
        let secret = Zeroizing::new(m.bytes("secret")?);
        Dir::new13(suite, secret, seq)?
    } else {
        let key = Zeroizing::new(m.bytes("key")?);
        let iv = Zeroizing::new(m.bytes("iv")?);
        Dir::new12(suite, &key, &iv, seq)?
    };
    m.done()?;
    Ok(d)
}
