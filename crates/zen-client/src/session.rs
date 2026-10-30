//! A signed-in session: its credential, the clock, the verified ACL, and
//! the filesystems it reaches.

use crate::acl::{Acl, AclVersion};
use crate::client::Client;
use crate::device::DeviceKey;
use crate::error::Result;
use crate::fs::Fs;
use crate::transport::bearer_of;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::{Arc, Mutex};
use zen_core::sig::SigningIdentity;
use zen_proto::Info;

struct Inner {
    client: Client,
    bearer: Mutex<String>,
    device_fp: Mutex<Vec<u8>>,
    key: Option<DeviceKey>,
    identity: Mutex<Option<Arc<SigningIdentity>>>,
    info: Info,
    clock: Mutex<Clock>,
    pub(crate) acl: Mutex<Option<AclVersion>>,
    resign: tokio::sync::Mutex<()>,
}

/// The session's hybrid logical clock (spec/fs.md §2), its wall time
/// corrected by the server's (`/v1/info` `time_ms`).
struct Clock {
    hlc: zen_core::fs::Clock,
    offset_ms: i64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A session (cheap to clone: clones share it).
#[derive(Clone)]
pub struct Session(Arc<Inner>);

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    pub(crate) async fn new(
        client: Client,
        token: Vec<u8>,
        device_fp: Vec<u8>,
        key: Option<DeviceKey>,
    ) -> Result<Self> {
        let t0 = now_ms();
        let info = client.info().await?;
        let t1 = now_ms();
        let offset_ms = info.time_ms as i64 - (t0 + t1) / 2;
        let mut hlc = zen_core::fs::Clock::default();
        hlc.observe(zen_core::fs::hlc(info.time_ms, 0));
        Ok(Session(Arc::new(Inner {
            client,
            bearer: Mutex::new(bearer_of(&token)),
            device_fp: Mutex::new(device_fp),
            identity: Mutex::new(key.as_ref().map(|k| k.identity.clone())),
            key,
            info,
            clock: Mutex::new(Clock { hlc, offset_ms }),
            acl: Mutex::new(None),
            resign: tokio::sync::Mutex::new(()),
        })))
    }

    /// The server.
    pub fn client(&self) -> &Client {
        &self.0.client
    }

    /// The server's `/v1/info`, as read when the session started.
    pub fn info(&self) -> &Info {
        &self.0.info
    }

    /// The device key it signed in with, if any.
    pub fn device_key(&self) -> Option<&DeviceKey> {
        self.0.key.as_ref()
    }

    /// The device fingerprint the server names this session by (empty for tokens).
    pub fn device_fp(&self) -> Vec<u8> {
        self.0.device_fp.lock().expect("session").clone()
    }

    /// The user identity that signs headers and ACL versions: the device
    /// key's, or the one set with [`Session::use_identity`].
    pub fn identity(&self) -> Option<Arc<SigningIdentity>> {
        self.0.identity.lock().expect("session").clone()
    }

    /// Sign with this identity from now on (an admin's, for signed headers).
    pub fn use_identity(&self, identity: Arc<SigningIdentity>) {
        *self.0.identity.lock().expect("session") = Some(identity);
    }

    /// A new timestamp, later than everything seen.
    pub fn hlc(&self) -> u64 {
        let mut c = self.0.clock.lock().expect("clock");
        let wall = (now_ms() + c.offset_ms).max(0) as u64;
        c.hlc.tick(wall)
    }

    /// The bearer credential, as sent.
    pub fn bearer(&self) -> String {
        self.0.bearer.lock().expect("session").clone()
    }

    /// A CBOR POST with this session's credential. A 401 (the session
    /// expired or was revoked) signs in again once, with the device key,
    /// and resends: commits carry their `commit_id`, and checkpoint puts are
    /// idempotent, so a resend applies at most once.
    pub async fn call<Q: Serialize, R: DeserializeOwned>(&self, path: &str, req: &Q) -> Result<R> {
        let bearer = self.bearer();
        let t = &self.0.client.transport;
        match t.call(path, Some(&bearer), req).await {
            Err(e) if e.status() == 401 && self.0.key.is_some() => {
                self.sign_in_again(&bearer).await?;
                t.call(path, Some(&self.bearer()), req).await
            }
            r => r,
        }
    }

    async fn sign_in_again(&self, stale: &str) -> Result<()> {
        let _one = self.0.resign.lock().await;
        if self.bearer() != stale {
            return Ok(()); // another call did
        }
        let key = self.0.key.as_ref().expect("a device key");
        let s = self.0.client.device_session(key).await?;
        *self.0.bearer.lock().expect("session") = bearer_of(&s.token);
        *self.0.device_fp.lock().expect("session") = s.device_fp;
        Ok(())
    }

    /// The ACL as this session verified it.
    pub fn acl(&self) -> Acl {
        Acl::new(self.clone())
    }

    pub(crate) fn acl_slot(&self) -> &Mutex<Option<AclVersion>> {
        &self.0.acl
    }

    /// Filesystem `id`.
    pub fn fs(&self, id: u32) -> Fs {
        Fs::new(self.clone(), id)
    }
}
