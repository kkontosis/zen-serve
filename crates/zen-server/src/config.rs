//! `zen-serve.toml`.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Server configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Listen address.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Directory for the database and the claim token.
    pub data_dir: PathBuf,
    /// Accepted origins for session signatures (api.md §3.3). Empty: derive
    /// from the `Host` header.
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
    /// Limits.
    #[serde(default)]
    pub limits: LimitsConfig,
}

/// One filesystem.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsConfig {
    /// fs_id (non-zero).
    pub id: u32,
}

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
    /// Idempotency record lifetime.
    pub idempotency_ttl_secs: u64,
    /// Session lifetime.
    pub session_ttl_secs: u64,
    /// `per_key` claim lifetime.
    pub claim_ttl_ms: u32,
    /// Sweeper period.
    pub sweep_interval_secs: u64,
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
            idempotency_ttl_secs: 86_400,
            session_ttl_secs: 86_400,
            claim_ttl_ms: 30_000,
            sweep_interval_secs: 60,
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
            limits: LimitsConfig::default(),
        }
    }

    /// Check invariants.
    pub fn validate(&self) -> Result<(), String> {
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
