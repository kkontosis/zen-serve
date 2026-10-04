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
    /// PEM private key: ECDSA P-256 or P-384 (PKCS#8 or SEC1), or Ed25519
    /// (PKCS#8).
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
            tls: None,
            auth: AuthConfig::default(),
            limits: LimitsConfig::default(),
            storage: StorageConfig::default(),
            fdb: FdbConfig::default(),
            backup: BackupConfig::default(),
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
        if self.limits.crdt_max_depth == 0 || self.limits.max_range_items == 0 {
            return Err("limits.crdt_max_depth and max_range_items must be at least 1".into());
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
        Ok(())
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
