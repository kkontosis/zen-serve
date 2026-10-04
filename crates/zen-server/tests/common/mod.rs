//! Test harness: a server on 127.0.0.1:0 in a tempdir, fixture identities,
//! signed ACLs and a small CBOR client.
#![allow(dead_code)]

pub mod pki;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use zen_core::kdf::acl_hash;
use zen_core::labels;
use zen_core::sig::{DeviceSecret, SigningIdentity, issue_device_cert};
use zen_proto::acl::{AclDoc, FsLimit, Grant, Member, SignedAcl};
use zen_proto::*;
use zen_server::config::Config;

/// A user with one device.
pub struct User {
    pub id: SigningIdentity,
    pub device: DeviceSecret,
    pub cert: Vec<u8>,
}

impl User {
    pub fn new(seed: u8) -> Self {
        let id = SigningIdentity::from_seed(&[seed; 32]);
        let device = DeviceSecret::from_seed(&[seed.wrapping_add(100); 32]);
        let cert = issue_device_cert(&id, &device.public(), 1_700_000_000).unwrap();
        User { id, device, cert }
    }

    pub fn fp(&self) -> Vec<u8> {
        self.id.public().fingerprint().to_vec()
    }

    pub fn member(&self) -> Member {
        Member {
            devices: vec![ByteBuf::from(self.cert.clone())],
            identity: self.id.public().encode(),
        }
    }
}

pub fn fs_grant(u: &User, fs: u32, rights: &[&str]) -> Grant {
    Grant {
        fs,
        topic: None,
        rights: rights.iter().map(|s| s.to_string()).collect(),
        subject: u.fp(),
    }
}

pub fn topic_grant(u: &User, fs: u32, prefix: &[u8], rights: &[&str]) -> Grant {
    Grant {
        fs,
        topic: Some(prefix.to_vec()),
        rights: rights.iter().map(|s| s.to_string()).collect(),
        subject: u.fp(),
    }
}

/// Build and sign an ACL doc.
pub fn signed_acl(
    signer: &User,
    version: u64,
    prev_doc: Option<&[u8]>,
    admins: &[&User],
    members: &[&User],
    grants: Vec<Grant>,
    limits: Vec<FsLimit>,
) -> (Vec<u8>, Vec<u8>) {
    let doc = acl_doc(version, prev_doc, admins, members, grants, limits);
    sign_doc(signer, &doc)
}

/// An unsigned ACL doc.
pub fn acl_doc(
    version: u64,
    prev_doc: Option<&[u8]>,
    admins: &[&User],
    members: &[&User],
    grants: Vec<Grant>,
    limits: Vec<FsLimit>,
) -> AclDoc {
    AclDoc {
        admins: admins.iter().map(|u| ByteBuf::from(u.fp())).collect(),
        grants,
        limits,
        members: members.iter().map(|u| u.member()).collect(),
        origins: Vec::new(),
        version,
        prev_hash: prev_doc.map_or(vec![0; 32], |d| acl_hash(d).to_vec()),
    }
}

/// Sign an ACL doc. Returns the signed ACL and the doc bytes.
pub fn sign_doc(signer: &User, doc: &AclDoc) -> (Vec<u8>, Vec<u8>) {
    let doc_bytes = to_cbor(doc);
    let sig = signer.id.sign(labels::SIG_ACL, &doc_bytes).unwrap();
    let signed = SignedAcl {
        doc: doc_bytes.clone(),
        sig,
        signer: signer.fp(),
    };
    (to_cbor(&signed), doc_bytes)
}

pub type ApiErr = (u16, ErrorBody);

pub struct Harness {
    pub server: zen_server::Server,
    pub dir: tempfile::TempDir,
    pub http: reqwest::Client,
    pub base: String,
    pub cfg: Config,
    /// `Host` header to send, as behind a load balancer (peers send their
    /// first node's, so all nodes share one origin). `None`: the address.
    pub host: Option<String>,
}

/// Whether the tests run on FoundationDB (`ZEN_TEST_BACKEND=fdb`, with
/// `ZEN_TEST_CLUSTER_FILE`). Each server then gets a random key prefix, so
/// tests run in parallel on one cluster.
pub fn on_fdb() -> bool {
    std::env::var("ZEN_TEST_BACKEND").as_deref() == Ok("fdb")
}

fn use_test_backend(cfg: &mut Config) {
    if !on_fdb() {
        return;
    }
    cfg.storage.backend = Some(zen_server::config::Backend::Fdb);
    cfg.storage.cluster_file = Some(
        std::env::var("ZEN_TEST_CLUSTER_FILE")
            .expect("ZEN_TEST_CLUSTER_FILE")
            .into(),
    );
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).unwrap();
    let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
    cfg.storage.key_prefix = format!("test/{hex}/");
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(f: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::with_data_dir(dir.path().join("data"));
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        use_test_backend(&mut cfg);
        f(&mut cfg);
        Self::launch(cfg, dir).await
    }

    async fn launch(cfg: Config, dir: tempfile::TempDir) -> Self {
        let server = zen_server::start(cfg.clone()).await.unwrap();
        let base = format!("http://{}", server.addr);
        // The server's pure-Rust rustls provider (reqwest has none of its own).
        zen_server::tls::provider::install_default();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        Harness {
            server,
            dir,
            http,
            base,
            cfg,
            host: None,
        }
    }

    /// Another node serving the same storage (FoundationDB only).
    pub async fn peer(&self) -> Self {
        self.peer_with(|_| {}).await
    }

    /// A peer whose data directory `f` prepares before it starts.
    pub async fn peer_with(&self, f: impl FnOnce(&std::path::Path)) -> Self {
        assert!(on_fdb(), "peers need a shared backend");
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = self.cfg.clone();
        cfg.data_dir = dir.path().join("data");
        std::fs::create_dir_all(&cfg.data_dir).unwrap();
        f(&cfg.data_dir);
        let mut peer = Self::launch(cfg, dir).await;
        peer.host = Some(self.host_header());
        peer
    }

    /// The `Host` header this harness sends.
    pub fn host_header(&self) -> String {
        self.host
            .clone()
            .unwrap_or_else(|| self.server.addr.to_string())
    }

    /// The origin clients of this harness sign.
    pub fn origin(&self) -> String {
        let scheme = if self.base.starts_with("https:") {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://{}", self.host_header())
    }

    /// Talk HTTPS to a server with `[tls]`, through `http`.
    pub fn use_https(&mut self, http: reqwest::Client) {
        self.base = format!("https://{}", self.server.addr);
        self.http = http;
    }

    pub async fn call<Q: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&[u8]>,
        req: &Q,
    ) -> Result<R, ApiErr> {
        self.call_host(path, token, req, &self.host_header()).await
    }

    /// `call` with an explicit `Host` header.
    pub async fn call_host<Q: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&[u8]>,
        req: &Q,
        host: &str,
    ) -> Result<R, ApiErr> {
        let rb = self
            .http
            .post(format!("{}{}", self.base, path))
            .header("host", host);
        self.send(rb, path, token, req).await
    }

    /// `call` through another HTTP client, adding `headers`.
    pub async fn call_via<Q: Serialize, R: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        headers: &[(&str, &str)],
        path: &str,
        token: Option<&[u8]>,
        req: &Q,
    ) -> Result<R, ApiErr> {
        let mut rb = http
            .post(format!("{}{}", self.base, path))
            .header("host", self.host_header());
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        self.send(rb, path, token, req).await
    }

    async fn send<Q: Serialize, R: DeserializeOwned>(
        &self,
        rb: reqwest::RequestBuilder,
        path: &str,
        token: Option<&[u8]>,
        req: &Q,
    ) -> Result<R, ApiErr> {
        let mut rb = rb.header("content-type", CBOR).body(to_cbor(req));
        if let Some(t) = token {
            rb = rb.header(
                "authorization",
                format!("Bearer {}", URL_SAFE_NO_PAD.encode(t)),
            );
        }
        let resp = rb.send().await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap();
        if status == 200 {
            Ok(from_cbor(&body).unwrap_or_else(|e| panic!("{path}: decode: {e}")))
        } else {
            Err((
                status,
                from_cbor(&body).unwrap_or_else(|e| panic!("{path} {status}: {e}")),
            ))
        }
    }

    pub async fn get<R: DeserializeOwned>(&self, path: &str) -> R {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path}");
        from_cbor(&resp.bytes().await.unwrap()).unwrap()
    }

    pub async fn sign_in(&self, u: &User) -> Result<Vec<u8>, ApiErr> {
        self.sign_in_at(u, &self.origin(), &self.host_header())
            .await
    }

    /// Device sign-in signing `origin`, sending `host` as the `Host` header.
    pub async fn sign_in_at(&self, u: &User, origin: &str, host: &str) -> Result<Vec<u8>, ApiErr> {
        let c: Challenge = self.call("/v1/auth/challenge", None, &Empty {}).await?;
        let origin = origin.to_string();
        let sig = u
            .device
            .signing()
            .sign(labels::SIG_SESSION, &session_message(&c.challenge, &origin))
            .unwrap();
        let s: Session = self
            .call_host(
                "/v1/auth/session",
                None,
                &SessionRequest {
                    challenge: c.challenge,
                    origin,
                    user: u.id.public().encode(),
                    cert: u.cert.clone(),
                    sig,
                },
                host,
            )
            .await?;
        Ok(s.token)
    }

    pub async fn put_acl(
        &self,
        signed: Vec<u8>,
        claim: Option<String>,
    ) -> Result<AclVersion, ApiErr> {
        self.call(
            "/v1/acl/put",
            None,
            &AclPut {
                acl: signed,
                claim,
                origin: None,
            },
        )
        .await
    }

    /// Claim the server with `admin`, granting each user `rights` on fs 1
    /// and all topic rights on all topics of fs 1. Returns the doc bytes.
    pub async fn claim(&self, admin: &User, others: &[&User]) -> Vec<u8> {
        let mut members = vec![admin];
        members.extend_from_slice(others);
        let mut grants = Vec::new();
        for u in &members {
            grants.push(fs_grant(u, 1, &["read", "write"]));
            grants.push(topic_grant(u, 1, &[], &["read", "append", "consume"]));
        }
        let (signed, doc) = signed_acl(admin, 1, None, &[admin], &members, grants, vec![]);
        self.put_acl(signed, self.server.claim_token.clone())
            .await
            .unwrap();
        doc
    }
}

pub fn cid(n: u8) -> Vec<u8> {
    let mut c = vec![0xC0; 16];
    c[15] = n;
    c[14] = rand_byte();
    c
}

fn rand_byte() -> u8 {
    use std::sync::atomic::{AtomicU8, Ordering};
    static N: AtomicU8 = AtomicU8::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

pub fn topic(n: u8) -> Vec<u8> {
    vec![n; 16]
}

pub fn key(n: u8) -> Vec<u8> {
    vec![0x70 | (n & 0x0f); 16]
}

pub fn append(t: &[u8], k: Option<&[u8]>, body: &[u8]) -> Append {
    Append {
        fs: 1,
        topic: t.to_vec(),
        key_token: k.map(<[u8]>::to_vec),
        envelope: body.to_vec(),
    }
}

// ---- WebSocket client

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn connect(h: &Harness, token: &[u8]) -> Ws {
    let url = format!("ws://{}/v1/stream", h.server.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send(
        &mut ws,
        &Frame::Auth {
            token: token.to_vec(),
        },
    )
    .await;
    assert_eq!(recv(&mut ws).await, Frame::Ok { id: None });
    ws
}

pub async fn send(ws: &mut Ws, f: &Frame) {
    ws.send(Message::Binary(to_cbor(f).into())).await.unwrap();
}

pub async fn recv(ws: &mut Ws) -> Frame {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame within 5 s")
            .unwrap()
            .unwrap();
        if let Message::Binary(b) = m {
            return from_cbor(&b).unwrap();
        }
    }
}
