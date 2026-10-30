//! Test harness: zen-server in-process on 127.0.0.1:0 (embedded backend,
//! the test adversary on), fixture device keys, claiming, forks and lies.
#![allow(dead_code)]

use serde::Serialize;
use std::time::Duration;
use zen_client::acl::sign_doc;
use zen_client::{Client, ClientOptions, DeviceKey, Session};
use zen_proto::acl::{AclDoc, Grant, Member};
use zen_proto::{AclPut, ByteBuf, Empty};
use zen_server::config::Config;

/// A device key of user `n` (device `n` of its own user).
pub fn key(n: u8) -> DeviceKey {
    DeviceKey::new([n; 32], [n.wrapping_add(100); 32]).unwrap()
}

/// A second device of user `n`.
pub fn second_key(n: u8) -> DeviceKey {
    DeviceKey::new([n; 32], [n.wrapping_add(200); 32]).unwrap()
}

/// A running server.
pub struct Server {
    pub server: zen_server::Server,
    pub dir: tempfile::TempDir,
    pub cfg: Config,
    pub url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Server {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(f: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::with_data_dir(dir.path().join("data"));
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.adversary = true;
        f(&mut cfg);
        Self::launch(cfg, dir).await
    }

    async fn launch(cfg: Config, dir: tempfile::TempDir) -> Self {
        let server = zen_server::start(cfg.clone()).await.unwrap();
        let scheme = if cfg.tls.is_some() { "https" } else { "http" };
        let url = format!("{scheme}://{}", server.addr);
        Server {
            server,
            dir,
            cfg,
            url,
        }
    }

    /// A second server on a copy of this one's store: one history up to
    /// now, two from here on (a fork, or a whole-store rollback).
    pub async fn fork(&self) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = self.cfg.clone();
        cfg.data_dir = dir.path().join("data");
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        {
            let target = zen_server::open_store(&cfg).unwrap();
            zen_server::dump::copy(self.server.state.store.as_ref(), target.as_ref(), false)
                .await
                .unwrap();
        }
        Self::launch(cfg, dir).await
    }

    pub fn client(&self) -> Client {
        Client::connect(&self.url, ClientOptions::default()).unwrap()
    }

    pub async fn session(&self, k: &DeviceKey) -> Session {
        self.client().sign_in_device(k).await.unwrap()
    }

    /// Claim the server with `admin`: every key is a member (its user, with
    /// each of that user's devices) with read/write on fs 1 and 2 and every
    /// topic right.
    pub async fn claim(&self, admin: &DeviceKey, others: &[&DeviceKey]) {
        let mut keys = vec![admin];
        keys.extend_from_slice(others);
        let mut members: Vec<Member> = Vec::new();
        let mut grants = Vec::new();
        for k in keys {
            let id = k.identity.public().encode();
            if let Some(m) = members.iter_mut().find(|m| m.identity == id) {
                m.devices.push(ByteBuf::from(k.cert.clone()));
                continue;
            }
            members.push(k.member());
            for fs in [1, 2] {
                grants.push(Grant {
                    fs,
                    topic: None,
                    rights: vec!["read".into(), "write".into()],
                    subject: k.user_fp().to_vec(),
                });
                grants.push(Grant {
                    fs,
                    topic: Some(Vec::new()),
                    rights: vec!["read".into(), "append".into(), "consume".into()],
                    subject: k.user_fp().to_vec(),
                });
            }
        }
        let doc = AclDoc {
            admins: vec![ByteBuf::from(admin.user_fp().to_vec())],
            grants,
            limits: vec![],
            members,
            origins: vec![],
            version: 1,
            prev_hash: vec![0; 32],
        };
        let put = AclPut {
            acl: sign_doc(&doc, &admin.identity).unwrap(),
            claim: self.server.claim_token.clone(),
            origin: None,
        };
        let t = zen_client::transport::Transport::new(&self.url, &Default::default()).unwrap();
        let _: zen_proto::AclVersion = t.call("/v1/acl/put", None, &put).await.unwrap();
    }
}

/// A lie for the test adversary (zen-server `adversary.rs`).
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lie {
    Kv { fs: u32, key: ByteBuf },
    Header { fs: u32 },
    Checkpoints { fs: u32, device: ByteBuf },
}

#[derive(Serialize)]
struct Tell {
    lies: Vec<Lie>,
    clear: bool,
}

/// Tell the adversary `lies` (an admin's session).
pub async fn lie(admin: &Session, lies: Vec<Lie>) {
    let _: Empty = admin
        .call("/v1/test/adversary", &Tell { lies, clear: false })
        .await
        .unwrap();
}

/// Lift every lie.
pub async fn truth(admin: &Session) {
    let _: Empty = admin
        .call(
            "/v1/test/adversary",
            &Tell {
                lies: vec![],
                clear: true,
            },
        )
        .await
        .unwrap();
}

/// Wait until `f` holds (polling), at most 10 s.
pub async fn until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not met within 10 s");
}
