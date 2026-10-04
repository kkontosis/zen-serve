//! Shared server state.

use crate::acl::{AclState, Fp};
use crate::config::Config;
use crate::eph::EphHub;
use crate::error::{ApiResult, not_found};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use zen_proto::AuthMethod;
use zen_store::Storage;

/// A session (spec/auth.md §3).
#[derive(Clone, Debug)]
pub struct SessionInfo {
    /// User fingerprint.
    pub user: Fp,
    /// The device fingerprint (`device_key`) or the credential id (other
    /// methods): what the rest of the server treats as "the device".
    pub cred: Fp,
    /// How the session was created.
    pub method: AuthMethod,
    /// Expiry, unix seconds.
    pub expires_unix: u64,
}

/// A session as cached by this node.
#[derive(Clone, Debug)]
pub struct CachedSession {
    /// The session.
    pub info: SessionInfo,
    /// When it was read from storage.
    pub at: Instant,
}

/// Server state.
pub struct AppState {
    /// Configuration.
    pub cfg: Config,
    /// Storage backend.
    pub store: Arc<dyn Storage>,
    acl: RwLock<Arc<AclState>>,
    /// Session cache by token hash (the sessions live in storage).
    pub sessions: Mutex<HashMap<[u8; 32], CachedSession>>,
    /// Cluster-wide key for stateless challenges.
    pub challenge_key: [u8; 32],
    /// Claim token while unclaimed.
    pub claim: Mutex<Option<String>>,
    /// Ephemeral pub/sub: this node's ring tailers.
    pub eph: EphHub,
    /// Cluster-wide key for the fake parameters of unknown login names,
    /// once read or created (`cred::params_key`).
    pub params_key: Mutex<Option<[u8; 32]>>,
    /// Failed password sign-ins per login name, on this node.
    pub pw_limiter: crate::password::Limiter,
}

/// Shared handle.
pub type Shared = Arc<AppState>;

impl AppState {
    /// Build the state.
    pub fn new(
        cfg: Config,
        store: Arc<dyn Storage>,
        acl: AclState,
        claim: Option<String>,
        challenge_key: [u8; 32],
    ) -> Self {
        AppState {
            pw_limiter: crate::password::Limiter::new(
                cfg.auth.password_max_failures,
                std::time::Duration::from_secs(cfg.auth.password_lockout_secs),
            ),
            params_key: Mutex::new(None),
            eph: EphHub::new(
                store.clone(),
                cfg.limits.ephemeral_bytes_per_sec,
                cfg.limits.ephemeral_burst_bytes,
            ),
            cfg,
            store,
            acl: RwLock::new(Arc::new(acl)),
            sessions: Mutex::new(HashMap::new()),
            challenge_key,
            claim: Mutex::new(claim),
        }
    }

    /// The current ACL.
    pub fn acl(&self) -> Arc<AclState> {
        self.acl.read().expect("acl lock").clone()
    }

    /// Replace the ACL (only moves forward). Once any version exists, this
    /// node's claim token is spent, whichever node accepted version 1.
    pub fn set_acl(&self, a: AclState) {
        let claimed = {
            let mut cur = self.acl.write().expect("acl lock");
            if a.version > cur.version {
                *cur = Arc::new(a);
            }
            cur.version >= 1
        };
        if claimed && self.claim.lock().expect("claim lock").is_some() {
            self.consume_claim_token();
        }
    }

    /// Forget the claim token once version 1 exists.
    pub fn consume_claim_token(&self) {
        *self.claim.lock().expect("claim lock") = None;
        remove_claim_token(&self.cfg.data_dir);
    }

    /// Whether sign-in method `m` is implemented and turned on.
    pub fn method_on(&self, m: AuthMethod) -> bool {
        self.cfg.auth.enabled(m) && crate::auth::IMPLEMENTED.contains(&m)
    }

    /// 403 `method_disabled` unless `m` is on.
    pub fn require_method(&self, m: AuthMethod) -> ApiResult<()> {
        if self.method_on(m) {
            Ok(())
        } else {
            Err(crate::error::method_disabled(format!(
                "the sign-in method {} is disabled on this server",
                m.name()
            )))
        }
    }

    /// 404 unless `fs` is configured.
    pub fn check_fs(&self, fs: u32) -> ApiResult<()> {
        if self.cfg.has_fs(fs) {
            Ok(())
        } else {
            Err(not_found(format!("unknown fs {fs}")))
        }
    }
}

/// Delete `<data_dir>/claim-token` (api.md §4.1), if it exists.
pub fn remove_claim_token(data_dir: &std::path::Path) {
    match std::fs::remove_file(data_dir.join("claim-token")) {
        Ok(()) => tracing::info!("claimed: deleted the claim-token file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "could not delete the claim-token file"),
    }
}
