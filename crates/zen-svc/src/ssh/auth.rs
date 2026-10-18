//! The `ssh-userauth` service (RFC 4252): `publickey` only, decided by the
//! application.

use super::wire::{Reader, Writer};
use super::{Conn, Event, MSG_SERVICE_ACCEPT, PublicKey, Stop, fail, reason};

const MSG_USERAUTH_REQUEST: u8 = 50;
const MSG_USERAUTH_FAILURE: u8 = 51;
const MSG_USERAUTH_SUCCESS: u8 = 52;
const MSG_USERAUTH_PK_OK: u8 = 60;

/// Requests before the engine gives up.
pub(crate) const MAX_TRIES: u32 = 20;
/// Longest user name accepted.
const MAX_USER: usize = 256;

pub(crate) const BEFORE_SERVICE: u8 = 0;
pub(crate) const ACCEPTED: u8 = 1;
pub(crate) const DONE: u8 = 2;

/// A question for the application: may `key` log in as `user`?
#[derive(Clone, Debug, PartialEq)]
pub struct Authorize {
    pub user: String,
    pub key: PublicKey,
    /// The client proved it holds the key: a yes logs it in. Without a
    /// signature, a yes only lets it go on to sign.
    pub signed: bool,
}

/// The question as the state keeps it.
#[derive(Clone, Debug)]
pub(crate) struct Ask {
    pub(crate) user: String,
    pub(crate) alg: String,
    pub(crate) key: PublicKey,
    pub(crate) signed: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Auth {
    pub(crate) state: u8,
    pub(crate) tries: u32,
    pub(crate) user: Option<String>,
    pub(crate) ask: Option<Ask>,
}

impl Conn {
    pub(super) fn on_service_request(&mut self, p: &[u8]) -> Result<(), Stop> {
        let mut r = Reader::new(p);
        r.u8()?;
        let name = r.string()?;
        r.done()?;
        if name != b"ssh-userauth" {
            return fail(reason::SERVICE_NOT_AVAILABLE, "only ssh-userauth");
        }
        if self.auth.state == BEFORE_SERVICE {
            self.auth.state = ACCEPTED;
        }
        self.send(Writer::new(MSG_SERVICE_ACCEPT).string(name).finish());
        Ok(())
    }

    pub(super) fn on_userauth(&mut self, p: &[u8]) -> Result<(), Stop> {
        if p[0] != MSG_USERAUTH_REQUEST {
            return fail(
                reason::PROTOCOL_ERROR,
                "an unexpected authentication message",
            );
        }
        match self.auth.state {
            BEFORE_SERVICE => {
                return fail(
                    reason::PROTOCOL_ERROR,
                    "authentication before the service request",
                );
            }
            DONE => return Ok(()),
            _ => {}
        }
        self.auth.tries += 1;
        if self.auth.tries > MAX_TRIES {
            return fail(
                reason::NO_MORE_AUTH_METHODS_AVAILABLE,
                "too many authentication attempts",
            );
        }
        let mut r = Reader::new(p);
        r.u8()?;
        let user = r.string()?;
        let service = r.string()?;
        let method = r.string()?;
        if service != b"ssh-connection" {
            return fail(reason::SERVICE_NOT_AVAILABLE, "only ssh-connection");
        }
        if method != b"publickey" {
            self.auth_failure();
            return Ok(());
        }
        let signed = r.bool()?;
        let alg = r.text()?;
        let blob = r.string()?;
        let sig = if signed { Some(r.string()?) } else { None };
        r.done()?;
        let user = match std::str::from_utf8(user) {
            Ok(u) if u.len() <= MAX_USER => u,
            _ => {
                self.auth_failure();
                return Ok(());
            }
        };
        let key = match PublicKey::from_blob(blob) {
            Ok(k) if k.allows(alg) => k,
            _ => {
                self.auth_failure();
                return Ok(());
            }
        };
        if let Some(sig) = sig {
            // RFC 4252 §7: what the client signed.
            let sid = self.sid.as_deref().expect("after the key exchange");
            let mut w = Writer::default();
            w.string(sid)
                .u8(MSG_USERAUTH_REQUEST)
                .string(user.as_bytes())
                .string(service)
                .string(b"publickey")
                .bool(true)
                .string(alg.as_bytes())
                .string(blob);
            if !key.verify(alg, &w.0, sig) {
                self.auth_failure();
                return Ok(());
            }
        }
        let ask = Ask {
            user: user.to_owned(),
            alg: alg.to_owned(),
            key,
            signed,
        };
        self.events.push(Event::Authorize(ask.question()));
        self.auth.ask = Some(ask);
        Ok(())
    }

    fn auth_failure(&mut self) {
        let m = Writer::new(MSG_USERAUTH_FAILURE)
            .names(&["publickey"])
            .bool(false)
            .finish();
        self.send(m);
    }

    /// The question waiting for [`Conn::authorize`], if any (after a
    /// restore, the application asks again).
    pub fn pending_authorize(&self) -> Option<Authorize> {
        self.auth.ask.as_ref().map(Ask::question)
    }

    /// Answers [`Event::Authorize`]: the reply to send. Reading resumes
    /// with the next [`Conn::input`].
    pub fn authorize(&mut self, accept: bool) -> Result<Vec<u8>, super::Error> {
        if self.ended() {
            return Err(super::Error::Closed);
        }
        let ask = self.auth.ask.take().ok_or(super::Error::NotAsked)?;
        if !accept {
            self.auth_failure();
        } else if ask.signed {
            self.auth.state = DONE;
            self.auth.user = Some(ask.user);
            self.send(vec![MSG_USERAUTH_SUCCESS]);
        } else {
            let m = Writer::new(MSG_USERAUTH_PK_OK)
                .string(ask.alg.as_bytes())
                .string(ask.key.blob())
                .finish();
            self.send(m);
        }
        Ok(std::mem::take(&mut self.out))
    }
}

impl Ask {
    fn question(&self) -> Authorize {
        Authorize {
            user: self.user.clone(),
            key: self.key.clone(),
            signed: self.signed,
        }
    }
}

/// What a client signs to authenticate (tests).
#[cfg(feature = "test-utils")]
pub(crate) fn signed_data(sid: &[u8], user: &str, alg: &str, blob: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    w.string(sid)
        .u8(MSG_USERAUTH_REQUEST)
        .string(user.as_bytes())
        .string(b"ssh-connection")
        .string(b"publickey")
        .bool(true)
        .string(alg.as_bytes())
        .string(blob);
    w.finish()
}
