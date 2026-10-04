//! Shared server state.

use crate::acl::{AclState, Fp};
use crate::config::Config;
use crate::error::{ApiResult, not_found};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::broadcast;
use zen_store::Storage;

/// A signed-in device.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    /// User fingerprint.
    pub user: Fp,
    /// Device fingerprint.
    pub device: Fp,
    /// Expiry.
    pub expires: Instant,
}

/// An ephemeral message (never stored).
#[derive(Debug)]
pub struct EphMsg {
    /// fs_id.
    pub fs: u32,
    /// Topic id.
    pub topic: Vec<u8>,
    /// Opaque data.
    pub data: Vec<u8>,
    /// Sending device.
    pub sender: Fp,
}

/// Server state.
pub struct AppState {
    /// Configuration.
    pub cfg: Config,
    /// Storage backend.
    pub store: Arc<dyn Storage>,
    acl: RwLock<Arc<AclState>>,
    /// Sessions by token.
    pub sessions: Mutex<HashMap<[u8; 32], SessionInfo>>,
    /// Outstanding challenges and their expiry.
    pub challenges: Mutex<HashMap<[u8; 32], Instant>>,
    /// Claim token while unclaimed.
    pub claim: Mutex<Option<String>>,
    /// Ephemeral pub/sub fan-out.
    pub eph: broadcast::Sender<Arc<EphMsg>>,
}

/// Shared handle.
pub type Shared = Arc<AppState>;

impl AppState {
    /// Build the state.
    pub fn new(cfg: Config, store: Arc<dyn Storage>, acl: AclState, claim: Option<String>) -> Self {
        AppState {
            cfg,
            store,
            acl: RwLock::new(Arc::new(acl)),
            sessions: Mutex::new(HashMap::new()),
            challenges: Mutex::new(HashMap::new()),
            claim: Mutex::new(claim),
            eph: broadcast::channel(1024).0,
        }
    }

    /// The current ACL.
    pub fn acl(&self) -> Arc<AclState> {
        self.acl.read().expect("acl lock").clone()
    }

    /// Replace the ACL (only moves forward).
    pub fn set_acl(&self, a: AclState) {
        let mut cur = self.acl.write().expect("acl lock");
        if a.version > cur.version {
            *cur = Arc::new(a);
        }
    }

    /// Forget the claim token once version 1 exists.
    pub fn consume_claim_token(&self) {
        *self.claim.lock().expect("claim lock") = None;
        let _ = std::fs::remove_file(self.cfg.data_dir.join("claim-token"));
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
