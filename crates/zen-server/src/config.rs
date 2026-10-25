//! `zen-serve.toml`.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use zen_proto::AuthMethod;

/// Server configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Listen address.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Directory for the database and the claim token.
    pub data_dir: PathBuf,
    /// 7a: accepted origins for sign-in signatures (auth.md §5.1). Empty:
    /// pin the first contact, or derive from the `Host` header.
    #[serde(default)]
    pub public_origins: Vec<String>,
    /// Configured filesystems.
    #[serde(default)]
    pub fs: Vec<FsConfig>,
    /// Directory served at `/unencrypted`. Absent: the bundled defaults.
    #[serde(default)]
    pub unencrypted_dir: Option<PathBuf>,
    /// Root paths aliased into `/unencrypted`.
    #[serde(default = "default_aliases")]
    pub aliases: Vec<String>,
    /// Serve `index.html` for other `GET`s that accept HTML.
    #[serde(default = "yes")]
    pub spa_fallback: bool,
    /// Content-Security-Policy for static responses.
    #[serde(default = "default_csp")]
    pub csp: String,
    /// Send COOP/COEP/CORP on every response (DESIGN-3 §4.2).
    #[serde(default)]
    pub cross_origin_isolation: bool,
    /// CORS allowlist (G17). Empty: same-origin only.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Serve the test adversary (`adversary.rs`): lies to readers on
    /// request, for testing detection. Only builds with `test-utils`.
    #[serde(default)]
    pub adversary: bool,
    /// Native TLS on `listen` (operations.md §8). Absent: plain HTTP.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Sign-in methods and origin policy (spec/auth.md §2).
    #[serde(default)]
    pub auth: AuthConfig,
    /// Limits.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Storage backend.
    #[serde(default)]
    pub storage: StorageConfig,
    /// FoundationDB processes run by the supervisor (`zen-serve init/join`).
    #[serde(default)]
    pub fdb: FdbConfig,
    /// FoundationDB native backup.
    #[serde(default)]
    pub backup: BackupConfig,
    /// Relayed services (spec/svc.md §2). Every node carries the same
    /// table; only `listen` may differ.
    #[serde(default)]
    pub service: Vec<ServiceConfig>,
}

/// Storage backend kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// redb file `data_dir/zen.redb` (single node).
    #[default]
    Embedded,
    /// FoundationDB (needs the `fdb` build feature).
    Fdb,
}

/// `[storage]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Backend. Absent: `fdb` on a node set up by `zen-serve init/join`
    /// (`data_dir/fdb.cluster` exists), else `embedded`.
    pub backend: Option<Backend>,
    /// FoundationDB cluster file. Absent: `data_dir/fdb.cluster` if it
    /// exists (written by `zen-serve init/join`), else the platform default.
    pub cluster_file: Option<PathBuf>,
    /// Serve the keyspace under this key prefix (UTF-8 bytes), e.g. a clone
    /// restored with `zen-serve restore --add-prefix` (operations.md).
    pub key_prefix: String,
}

/// `[fdb]`: the supervised `fdbserver` processes (operations.md).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FdbConfig {
    /// Directory with `fdbserver`, `fdbcli`, `fdbbackup`, `fdbrestore` and
    /// `backup_agent`. Absent: search the standard install locations.
    pub bin_dir: Option<PathBuf>,
    /// `fdbserver` processes on this node (1 per core is a good start).
    pub processes: u16,
    /// IP the processes listen on.
    pub listen_ip: String,
    /// IP other nodes reach this node at. Absent: `listen_ip`.
    pub public_ip: Option<String>,
    /// First process port; process `i` uses `port + i`.
    pub port: u16,
    /// TLS for FoundationDB traffic (all three, or none).
    pub tls_cert: Option<PathBuf>,
    /// TLS private key.
    pub tls_key: Option<PathBuf>,
    /// TLS CA bundle.
    pub tls_ca: Option<PathBuf>,
    /// Manage redundancy mode and coordinators automatically.
    pub auto_redundancy: bool,
}

impl Default for FdbConfig {
    fn default() -> Self {
        FdbConfig {
            bin_dir: None,
            processes: 1,
            listen_ip: "127.0.0.1".into(),
            public_ip: None,
            port: 4500,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            auto_redundancy: true,
        }
    }
}

/// `[tls]`: native TLS on the API listener (operations.md §8).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain, the server's certificate first.
    pub cert: PathBuf,
    /// PEM private key: ECDSA P-256 or P-384 (PKCS#8 or SEC1), Ed25519
    /// (PKCS#8), or with the `ring` feature RSA (PKCS#1 or PKCS#8, 2048 to
    /// 4096 bits). The pure-Rust build refuses RSA (`TD-TLS-RSA-SERVER-KEY`).
    pub key: PathBuf,
    /// PEM CA certificates that client certificates must chain to. Set,
    /// with `[auth] mtls` on, the server asks for client certificates
    /// (optional at the TLS layer): native mTLS sign-in (auth.md §10).
    #[serde(default)]
    pub client_ca: Option<PathBuf>,
}

/// `[backup]`: FoundationDB native continuous backup (operations.md).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    /// Run `backup_agent` processes under the supervisor.
    pub agents: bool,
}

/// `[auth]`: which sign-in methods are on (spec/auth.md §2). A method that
/// is off refuses its endpoints with 403 `method_disabled`, and the
/// sessions it created stop working until it is turned on again.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Method 1: per-device keys certified in the signed ACL.
    pub device_keys: bool,
    /// Method 2: WebAuthn passkeys.
    pub passkeys: bool,
    /// Method 3: OPAQUE passwords.
    pub opaque: bool,
    /// Method 4: admin-issued API tokens.
    pub api_tokens: bool,
    /// Method 5: TLS client certificates.
    pub mtls: bool,
    /// Method 6: password-derived keys, the default method.
    pub password_keys: bool,
    /// 7b: pin the first origin that signs in after the claim, and accept
    /// only pinned origins after that. In force only while
    /// `public_origins` is empty, unless `origin_pinning_always`.
    pub origin_pinning: bool,
    /// Keep 7b in force even when `public_origins` is set: the listed
    /// origins and the pinned first contact are both accepted. Risky: the
    /// first contact still trusts the `Host` header.
    pub origin_pinning_always: bool,
    /// 7c: also accept the `origins` listed in the head ACL.
    pub acl_origins: bool,
    /// Method 6: Argon2id memory (KiB) that `/v1/info` recommends for new
    /// registrations, and that unknown login names are answered with.
    pub password_m_cost_kib: u32,
    /// Method 6: Argon2id passes, likewise.
    pub password_t_cost: u32,
    /// Method 6: Argon2id lanes, likewise.
    pub password_p_cost: u32,
    /// Method 6: failed sign-ins per login name before it is locked.
    pub password_max_failures: u32,
    /// Method 6: how long a locked login name stays locked after its last
    /// failure.
    pub password_lockout_secs: u64,
    /// Method 2: the WebAuthn relying-party id. Absent: the host of the
    /// canonical origin (auth.md §5.5). Set it to a registrable suffix of
    /// that host to share passkeys across subdomains.
    pub passkey_rp_id: Option<String>,
    /// Method 2: refuse passkey sign-ins and registrations in which the
    /// authenticator didn't verify the user (PIN or biometric).
    pub passkey_require_uv: bool,
    /// Method 5: reverse proxies (addresses or CIDR blocks) trusted to
    /// verify client certificates and forward them in
    /// `mtls_proxy_header` (auth.md §10.2). Empty: no proxy mode.
    pub mtls_trusted_proxies: Vec<String>,
    /// Method 5: the request header a trusted proxy forwards the verified
    /// client certificate in: URL-escaped PEM (nginx
    /// `$ssl_client_escaped_cert`) or base64 DER.
    pub mtls_proxy_header: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            device_keys: true,
            passkeys: true,
            opaque: false,
            api_tokens: false,
            mtls: true,
            password_keys: true,
            origin_pinning: true,
            origin_pinning_always: false,
            acl_origins: false,
            // The browser recommendation of formats.md §6: the slowest
            // client must be able to sign in.
            password_m_cost_kib: 256 * 1024,
            password_t_cost: 3,
            password_p_cost: 1,
            password_max_failures: 10,
            password_lockout_secs: 300,
            passkey_rp_id: None,
            passkey_require_uv: true,
            mtls_trusted_proxies: Vec::new(),
            mtls_proxy_header: "x-client-cert".into(),
        }
    }
}

impl AuthConfig {
    /// The configured Argon2id parameters of method 6.
    pub fn password_params(&self) -> zen_proto::Argon2Params {
        zen_proto::Argon2Params {
            m_cost_kib: self.password_m_cost_kib,
            t_cost: self.password_t_cost,
            p_cost: self.password_p_cost,
        }
    }

    /// The `userVerification` passkey ceremonies request.
    pub fn passkey_user_verification(&self) -> &'static str {
        if self.passkey_require_uv {
            "required"
        } else {
            "preferred"
        }
    }

    /// Whether `m` is turned on (implemented or not).
    pub fn enabled(&self, m: AuthMethod) -> bool {
        match m {
            AuthMethod::DeviceKey => self.device_keys,
            AuthMethod::Passkey => self.passkeys,
            AuthMethod::Opaque => self.opaque,
            AuthMethod::ApiToken => self.api_tokens,
            AuthMethod::Mtls => self.mtls,
            AuthMethod::PasswordKey => self.password_keys,
        }
    }
}

/// Whether `s` can be a WebAuthn relying-party id: a lowercase domain
/// name of dot-separated labels of `a-z`, `0-9` and inner `-`.
pub fn valid_rp_id(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// One filesystem.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsConfig {
    /// fs_id (non-zero).
    pub id: u32,
}

/// How a relayed service's connections arrive (spec/svc.md §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SvcMode {
    /// A public port.
    Tcp,
    /// Members connect over `/v1/svc/connect`.
    TcpAuth,
}

impl SvcMode {
    /// The name in the config and the API.
    pub fn name(self) -> &'static str {
        match self {
            SvcMode::Tcp => "tcp",
            SvcMode::TcpAuth => "tcp-auth",
        }
    }
}

/// One relayed service: `[[service]]` (spec/svc.md §2).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Unique name, for logs.
    pub name: String,
    /// The fs the service belongs to.
    pub fs: u32,
    /// The service id: a topic id (16·n bytes), in hex.
    #[serde(deserialize_with = "de_hex")]
    pub id: Vec<u8>,
    /// How connections arrive.
    pub mode: SvcMode,
    /// The address to accept on (`tcp` only).
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// Open connections per node.
    #[serde(default = "default_max_conns")]
    pub max_conns: u32,
    /// Accepts per second per peer address and node (0: no limit).
    #[serde(default = "default_per_ip_rate")]
    pub per_ip_rate: u32,
    /// Burst of accepts per peer address and node.
    #[serde(default = "default_per_ip_burst")]
    pub per_ip_burst: u32,
    /// Unacked input bytes per connection.
    #[serde(default = "default_window")]
    pub window: u32,
    /// Committed, unwritten output bytes per connection.
    #[serde(default = "default_out_window")]
    pub out_window: u32,
    /// A connection not claimed within this time is aborted.
    #[serde(default = "default_accept_wait_ms")]
    pub accept_wait_ms: u32,
    /// A connection without a committed step this long is aborted.
    #[serde(default = "default_handshake_secs")]
    pub handshake_secs: u32,
    /// A connection without input or output this long is aborted (0: off).
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u32,
    /// Output is released by key reveal, so handlers can take over.
    #[serde(default = "yes")]
    pub resume: bool,
    /// The inbox's X-Wing public key, in hex: input is sealed at rest.
    #[serde(default, deserialize_with = "de_hex_opt")]
    pub inbox: Option<Vec<u8>>,
}

/// The smallest `window` and `out_window`.
pub const MIN_SVC_WINDOW: u32 = 65_536;

fn default_max_conns() -> u32 {
    1024
}
fn default_per_ip_rate() -> u32 {
    10
}
fn default_per_ip_burst() -> u32 {
    50
}
fn default_window() -> u32 {
    262_144
}
fn default_out_window() -> u32 {
    1_048_576
}
fn default_accept_wait_ms() -> u32 {
    10_000
}
fn default_handshake_secs() -> u32 {
    30
}
fn default_idle_secs() -> u32 {
    300
}

fn de_hex<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    parse_hex(&s).ok_or_else(|| serde::de::Error::custom(format!("{s:?} is not hex")))
}

fn de_hex_opt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
    de_hex(d).map(Some)
}

/// Decode lowercase or uppercase hex.
pub fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What one ephemeral message costs against its device's rate limit on top
/// of its data: the topic id (at most 256 bytes), the sender and the ring
/// entry's key.
pub const EPH_MSG_OVERHEAD: u64 = 256;

/// Server limits.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Max stored-key length.
    pub max_key_bytes: u32,
    /// Max sealed value length.
    pub max_value_bytes: u32,
    /// Max event envelope length.
    pub max_envelope_bytes: u32,
    /// Max payload of one commit.
    pub max_commit_bytes: u32,
    /// Max operations in one commit.
    pub max_commit_ops: u32,
    /// Max items per range read.
    pub max_range_items: u32,
    /// Max bytes (keys + values) one range read returns; a read stops there
    /// with `more`, a range that must be read whole returns 413.
    pub max_range_bytes: u64,
    /// Idempotency record lifetime.
    pub idempotency_ttl_secs: u64,
    /// Session lifetime.
    pub session_ttl_secs: u64,
    /// `per_key` claim lifetime.
    pub claim_ttl_ms: u32,
    /// Sweeper period.
    pub sweep_interval_secs: u64,
    /// How long ephemeral messages stay in the cross-node ring.
    pub ephemeral_ttl_secs: u64,
    /// Ephemeral publishes per device and node: sustained bytes per second
    /// (a message costs its data plus [`EPH_MSG_OVERHEAD`]); 0 = no limit.
    pub ephemeral_bytes_per_sec: u64,
    /// Ephemeral publishes per device and node: burst, in bytes.
    pub ephemeral_burst_bytes: u64,
    /// How far a filesystem `hlc` may be ahead of the server clock.
    pub crdt_max_skew_ms: u64,
    /// How far back a late filesystem operation may reach (spec/fs.md §3.4).
    pub crdt_horizon_secs: u64,
    /// Max logged moves one late move may undo and redo.
    pub crdt_max_redo: u32,
    /// How long an unreferenced chunk is kept.
    pub chunk_grace_secs: u64,
    /// Max consumer groups per topic.
    pub max_groups_per_topic: u32,
    /// Max depth of a move's new parent (ancestors walked per move).
    pub crdt_max_depth: u32,
    /// Device checkpoints kept per device and fs (api.md §15.1).
    pub checkpoint_keep: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            max_key_bytes: 2048,
            max_value_bytes: 90_000,
            max_envelope_bytes: 90_000,
            max_commit_bytes: 8_000_000,
            max_commit_ops: 10_000,
            max_range_items: 10_000,
            max_range_bytes: 8_000_000,
            idempotency_ttl_secs: 86_400,
            session_ttl_secs: 86_400,
            claim_ttl_ms: 30_000,
            sweep_interval_secs: 60,
            ephemeral_ttl_secs: 60,
            ephemeral_bytes_per_sec: 65_536,
            ephemeral_burst_bytes: 1_048_576,
            crdt_max_skew_ms: 60_000,
            crdt_horizon_secs: 7 * 86_400,
            crdt_max_redo: 1000,
            chunk_grace_secs: 86_400,
            max_groups_per_topic: 64,
            crdt_max_depth: 1000,
            checkpoint_keep: 1024,
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8080".parse().expect("valid address")
}

fn default_aliases() -> Vec<String> {
    ["favicon.ico", "robots.txt", "manifest.webmanifest"]
        .map(String::from)
        .to_vec()
}

fn yes() -> bool {
    true
}

/// Default CSP: same-origin scripts (WASM allowed), Trusted Types (G15).
pub fn default_csp() -> String {
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; object-src 'none'; \
     base-uri 'none'; frame-ancestors 'none'; require-trusted-types-for 'script'"
        .into()
}

impl Config {
    /// Parse TOML and validate.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }

    /// A minimal config for `data_dir` (tests, defaults).
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Config {
            listen: default_listen(),
            data_dir,
            public_origins: Vec::new(),
            fs: vec![FsConfig { id: 1 }, FsConfig { id: 2 }],
            unencrypted_dir: None,
            aliases: default_aliases(),
            spa_fallback: true,
            csp: default_csp(),
            cross_origin_isolation: false,
            cors_origins: Vec::new(),
            adversary: false,
            tls: None,
            auth: AuthConfig::default(),
            limits: LimitsConfig::default(),
            storage: StorageConfig::default(),
            fdb: FdbConfig::default(),
            backup: BackupConfig::default(),
            service: Vec::new(),
        }
    }

    /// The backend in effect.
    pub fn backend(&self) -> Backend {
        self.storage.backend.unwrap_or_else(|| {
            if self.data_dir.join("fdb.cluster").exists() {
                Backend::Fdb
            } else {
                Backend::Embedded
            }
        })
    }

    /// The cluster file to connect with, if any.
    pub fn cluster_file(&self) -> Option<PathBuf> {
        self.storage.cluster_file.clone().or_else(|| {
            let p = self.data_dir.join("fdb.cluster");
            p.exists().then_some(p)
        })
    }

    /// Check invariants.
    pub fn validate(&self) -> Result<(), String> {
        let tls = [&self.fdb.tls_cert, &self.fdb.tls_key, &self.fdb.tls_ca];
        if tls.iter().any(|t| t.is_some()) && !tls.iter().all(|t| t.is_some()) {
            return Err("fdb.tls_cert, tls_key and tls_ca go together".into());
        }
        if self.fdb.processes == 0 {
            return Err("fdb.processes must be at least 1".into());
        }
        // Event offsets and dots index a commit's appends and writes with a
        // u16 (keyspace.md §2).
        if self.limits.max_commit_ops > u16::MAX as u32 {
            return Err("limits.max_commit_ops must be at most 65535".into());
        }
        if self.limits.crdt_max_depth == 0
            || self.limits.max_range_items == 0
            || self.limits.checkpoint_keep == 0
        {
            return Err(
                "limits.crdt_max_depth, max_range_items and checkpoint_keep must be at least 1"
                    .into(),
            );
        }
        if self.adversary && !cfg!(feature = "test-utils") {
            return Err("adversary: this zen-serve is built without test-utils".into());
        }
        let l = &self.limits;
        if l.ephemeral_bytes_per_sec > 0
            && l.ephemeral_burst_bytes < l.max_envelope_bytes as u64 + EPH_MSG_OVERHEAD
        {
            return Err(format!(
                "limits.ephemeral_burst_bytes must be at least max_envelope_bytes + {EPH_MSG_OVERHEAD}"
            ));
        }
        for o in &self.public_origins {
            if !zen_proto::valid_origin(o) {
                return Err(format!(
                    "public_origins: {o:?} is not an origin (scheme://host[:port], lowercase, \
                     no trailing slash)"
                ));
            }
        }
        crate::password::check_params(self.auth.password_params())
            .map_err(|e| format!("auth.password_*: {e}"))?;
        if let Some(rp) = &self.auth.passkey_rp_id
            && !valid_rp_id(rp)
        {
            return Err(format!(
                "auth.passkey_rp_id: {rp:?} is not a domain name (lowercase, no scheme or port)"
            ));
        }
        for p in &self.auth.mtls_trusted_proxies {
            crate::mtls::Cidr::parse(p).ok_or_else(|| {
                format!(
                    "auth.mtls_trusted_proxies: {p:?} is not an IP address or CIDR block \
                     (e.g. \"10.0.0.5\", \"10.0.0.0/8\", \"::1/128\")"
                )
            })?;
        }
        let h = &self.auth.mtls_proxy_header;
        if axum::http::HeaderName::from_bytes(h.as_bytes()).is_err() || h.to_lowercase() != *h {
            return Err(format!(
                "auth.mtls_proxy_header: {h:?} is not a lowercase HTTP header name"
            ));
        }
        if self.auth.password_max_failures == 0 {
            return Err("auth.password_max_failures must be at least 1".into());
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.fs {
            if f.id == 0 {
                return Err("fs id 0 is reserved for /unencrypted".into());
            }
            if !seen.insert(f.id) {
                return Err(format!("duplicate fs id {}", f.id));
            }
        }
        self.validate_services()
    }

    fn validate_services(&self) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut listens = std::collections::HashSet::new();
        for s in &self.service {
            let at = format!("service {:?}", s.name);
            if s.name.is_empty() || !names.insert(&s.name) {
                return Err(format!("{at}: the name must be unique and non-empty"));
            }
            if !self.has_fs(s.fs) {
                return Err(format!("{at}: fs {} is not configured", s.fs));
            }
            if s.id.is_empty() || !s.id.len().is_multiple_of(16) || s.id.len() > 256 {
                return Err(format!("{at}: id must be 16·n bytes, 1 ≤ n ≤ 16"));
            }
            if !ids.insert((s.fs, &s.id)) {
                return Err(format!("{at}: another service has this fs and id"));
            }
            match (s.mode, s.listen) {
                (SvcMode::Tcp, None) => return Err(format!("{at}: mode tcp needs listen")),
                (SvcMode::TcpAuth, Some(_)) => {
                    return Err(format!("{at}: mode tcp-auth takes no listen"));
                }
                (SvcMode::Tcp, Some(l)) => {
                    if l.port() != 0 && (l == self.listen || !listens.insert(l)) {
                        return Err(format!("{at}: listen {l} is already in use"));
                    }
                }
                (SvcMode::TcpAuth, None) => {}
            }
            if s.window < MIN_SVC_WINDOW || s.out_window < MIN_SVC_WINDOW {
                return Err(format!(
                    "{at}: window and out_window must be at least {MIN_SVC_WINDOW}"
                ));
            }
            if s.max_conns == 0 {
                return Err(format!("{at}: max_conns must be at least 1"));
            }
            if let Some(k) = &s.inbox
                && k.len() != zen_core::svc::INBOX_PUBLIC_LEN
            {
                return Err(format!(
                    "{at}: inbox must be an X-Wing public key ({} bytes)",
                    zen_core::svc::INBOX_PUBLIC_LEN
                ));
            }
        }
        Ok(())
    }

    /// The service `id` of `fs`, if configured.
    pub fn service(&self, fs: u32, id: &[u8]) -> Option<&ServiceConfig> {
        self.service.iter().find(|s| s.fs == fs && s.id == id)
    }

    /// Whether `fs` is configured.
    pub fn has_fs(&self, fs: u32) -> bool {
        self.fs.iter().any(|f| f.id == fs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_configs_parse() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for name in ["zen-serve.toml", "zen-serve-fdb.toml"] {
            let text = std::fs::read_to_string(dir.join(name)).unwrap();
            Config::from_toml(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            // The commented `[[service]]` examples parse once uncommented.
            let at = text.find("# [[service]]").expect("a service example");
            let svc: String = text[at..]
                .lines()
                .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
                .collect();
            let c = Config::from_toml(&format!("{}{svc}", &text[..at]))
                .unwrap_or_else(|e| panic!("{name} with its service: {e}"));
            let modes: Vec<_> = c.service.iter().map(|s| s.mode).collect();
            assert_eq!(modes, [SvcMode::Tcp, SvcMode::TcpAuth]);
        }
    }

    #[test]
    fn public_origins_must_be_origins() {
        for bad in ["https://a.example/", "a.example", "https://A.example"] {
            let t = format!("data_dir = \"/tmp/x\"\npublic_origins = [\"{bad}\"]");
            assert!(Config::from_toml(&t).is_err(), "{bad}");
        }
        let t = "data_dir = \"/tmp/x\"\npublic_origins = [\"https://a.example:8443\"]";
        assert!(Config::from_toml(t).is_ok());
    }

    #[test]
    fn passkey_rp_ids_are_domains() {
        for ok in ["zen.example.org", "localhost", "a-b.c1"] {
            assert!(valid_rp_id(ok), "{ok}");
        }
        for bad in [
            "",
            "Zen.example",
            "https://a.example",
            "a.example:443",
            "a..b",
            "-a.b",
            "a.",
            "127.0.0.1:1",
        ] {
            assert!(!valid_rp_id(bad), "{bad}");
        }
        let t = "data_dir = \"/tmp/x\"\n[auth]\npasskey_rp_id = \"Example.org\"";
        assert!(Config::from_toml(t).is_err());
        let t = "data_dir = \"/tmp/x\"\n[auth]\npasskey_rp_id = \"example.org\"";
        assert!(Config::from_toml(t).is_ok());
    }

    const SVC: &str = "data_dir = \"/tmp/x\"\n[[fs]]\nid = 1\n";

    fn svc(extra: &str) -> Result<Config, String> {
        Config::from_toml(&format!(
            "{SVC}[[service]]\nname = \"a\"\nfs = 1\nid = \"{}\"\n{extra}",
            "ab".repeat(16)
        ))
    }

    #[test]
    fn services_parse_with_defaults() {
        let c = svc("mode = \"tcp\"\nlisten = \"127.0.0.1:2222\"").unwrap();
        let s = &c.service[0];
        assert_eq!(s.id, vec![0xab; 16]);
        assert_eq!(s.mode, SvcMode::Tcp);
        assert_eq!(
            (
                s.max_conns,
                s.per_ip_rate,
                s.per_ip_burst,
                s.window,
                s.out_window
            ),
            (1024, 10, 50, 262_144, 1_048_576)
        );
        assert_eq!(
            (s.accept_wait_ms, s.handshake_secs, s.idle_secs),
            (10_000, 30, 300)
        );
        assert!(s.resume);
        assert_eq!(s.inbox, None);
        assert!(c.service(1, &[0xab; 16]).is_some());
        assert!(c.service(2, &[0xab; 16]).is_none());
        assert!(svc("mode = \"tcp-auth\"").is_ok());
        let pk = zen_core::svc::inbox_public(&[7; 32]);
        let hex: String = pk.iter().map(|b| format!("{b:02x}")).collect();
        let c = svc(&format!("mode = \"tcp-auth\"\ninbox = \"{hex}\"")).unwrap();
        assert_eq!(c.service[0].inbox.as_deref(), Some(&pk[..]));
    }

    #[test]
    fn services_are_validated() {
        let tcp = "mode = \"tcp\"\nlisten = \"127.0.0.1:2222\"\n";
        for (extra, why) in [
            ("mode = \"tcp\"", "tcp without listen"),
            (
                "mode = \"tcp-auth\"\nlisten = \"127.0.0.1:2222\"",
                "tcp-auth with listen",
            ),
            (&format!("{tcp}window = 1000"), "small window"),
            (&format!("{tcp}out_window = 1000"), "small out_window"),
            (&format!("{tcp}max_conns = 0"), "no connections"),
            ("mode = \"udp\"", "unknown mode"),
            (&format!("{tcp}inbox = \"00\""), "inbox too short"),
            (&format!("{tcp}inbox = \"zz\""), "inbox not hex"),
            (&format!("{tcp}color = \"red\""), "unknown field"),
            (
                "mode = \"tcp\"\nlisten = \"127.0.0.1:8080\"",
                "the API's address",
            ),
        ] {
            assert!(svc(extra).is_err(), "{why}");
        }
        let base = format!("{SVC}[[service]]\nname = \"a\"\nmode = \"tcp-auth\"\n");
        for (rest, why) in [
            (
                format!("fs = 2\nid = \"{}\"", "ab".repeat(16)),
                "unknown fs",
            ),
            (
                format!("fs = 1\nid = \"{}\"", "ab".repeat(15)),
                "id not 16·n",
            ),
            ("fs = 1\nid = \"zz\"".to_string(), "id not hex"),
            (
                format!("fs = 1\nid = \"{}\"", "ab".repeat(272)),
                "id too long",
            ),
        ] {
            assert!(
                Config::from_toml(&format!("{base}{rest}")).is_err(),
                "{why}"
            );
        }
        let two = |a: &str, b: &str| {
            Config::from_toml(&format!(
                "{SVC}[[service]]\n{a}\nfs = 1\nmode = \"tcp\"\nlisten = \"127.0.0.1:2222\"\n\
                 [[service]]\n{b}\nfs = 1\nmode = \"tcp\"\nlisten = \"127.0.0.1:2223\"\n"
            ))
        };
        let (x, y) = ("ab".repeat(16), "cd".repeat(16));
        assert!(
            two(
                &format!("name = \"a\"\nid = \"{x}\""),
                &format!("name = \"b\"\nid = \"{y}\"")
            )
            .is_ok()
        );
        assert!(
            two(
                &format!("name = \"a\"\nid = \"{x}\""),
                &format!("name = \"a\"\nid = \"{y}\"")
            )
            .is_err()
        );
        assert!(
            two(
                &format!("name = \"a\"\nid = \"{x}\""),
                &format!("name = \"b\"\nid = \"{x}\"")
            )
            .is_err()
        );
        let same_port = format!(
            "{SVC}[[service]]\nname = \"a\"\nfs = 1\nid = \"{x}\"\nmode = \"tcp\"\nlisten = \"127.0.0.1:2222\"\n\
             [[service]]\nname = \"b\"\nfs = 1\nid = \"{y}\"\nmode = \"tcp\"\nlisten = \"127.0.0.1:2222\"\n"
        );
        assert!(Config::from_toml(&same_port).is_err());
    }

    #[test]
    fn auth_defaults() {
        let c = Config::from_toml("data_dir = \"/tmp/x\"").unwrap();
        let on: Vec<_> = AuthMethod::ALL
            .into_iter()
            .filter(|m| c.auth.enabled(*m))
            .collect();
        assert_eq!(
            on,
            [
                AuthMethod::DeviceKey,
                AuthMethod::Passkey,
                AuthMethod::Mtls,
                AuthMethod::PasswordKey
            ]
        );
    }
}
