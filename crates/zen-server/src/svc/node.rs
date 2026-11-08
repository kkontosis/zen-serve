//! The node lease and the node's work queue (svc.md §3 step 12, §10).

use super::{Cmd, NODE_LEASE_SECS, decode_u64, decode_work};
use crate::error::ApiResult;
use crate::keys::{self, end_of};
use crate::state::Shared;
use crate::txn::txn_loop;
use std::time::Duration;
use zen_store::VERSIONS_PER_SEC;

/// Renew this node's lease. Returns false when the lease had lapsed (the
/// sweeper may have reaped this node's connections): the caller aborts its
/// sockets. `first` creates it.
///
/// Retried after an unknown commit (a recovery of the cluster, OPS §3):
/// the body sets the lease to a fresh expiry, so a re-run only sets a later
/// one, and a commit that did apply left the lease in the future, so `held`
/// reads the same.
pub async fn renew(st: &Shared, first: bool) -> ApiResult<bool> {
    let key = keys::node_lease(&st.svc.node);
    let (held, _) = txn_loop!(st.store, None, idempotent, |t| {
        let now = t.read_version();
        let held = first || decode_u64(t.get(&key).await?.as_deref()) > now;
        t.set(
            &key,
            &(now + NODE_LEASE_SECS * VERSIONS_PER_SEC).to_be_bytes(),
        );
        Ok(held)
    })?;
    Ok(held)
}

/// Renew the node lease every second.
pub async fn renewer(st: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match renew(&st, false).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("the node lease lapsed: aborting this node's relayed connections");
                st.svc.abort_conns();
            }
            Err(e) => tracing::warn!(error = %e.message, "node lease renewal failed"),
        }
    }
}

/// The node's work loop: wait on its doorbell, take the work entries and
/// hand each to its local connection task.
pub async fn work(st: Shared) {
    let bell = keys::node_bell(&st.svc.node);
    loop {
        let w = match st.store.watch(&bell).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "node doorbell watch");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        match drain(&st).await {
            Ok(0) => {}
            Ok(_) => continue,
            Err(e) => tracing::warn!(error = %e.message, "node work queue"),
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), w).await;
    }
}

/// Take a page of work entries, clearing exactly the ones read.
async fn drain(st: &Shared) -> ApiResult<usize> {
    let p = keys::node_work(&st.svc.node).finish();
    let (items, _) = txn_loop!(st.store, None, |t| {
        let got = t.snapshot_get_range(&p, &end_of(&p), 1000, false).await?;
        let mut out = Vec::with_capacity(got.len());
        for (k, v) in &got {
            t.clear(k);
            if let Some((fs, svc, conn)) = decode_work(v) {
                let abort = t
                    .snapshot_get(&keys::conn("rz", fs, &svc, &conn))
                    .await?
                    .is_some();
                out.push((conn, abort));
            }
        }
        Ok(out)
    })?;
    for (conn, abort) in &items {
        st.svc
            .send(conn, if *abort { Cmd::Abort } else { Cmd::Wake });
    }
    Ok(items.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::AclState;
    use crate::config::Config;
    use crate::state::AppState;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use zen_store::embedded::{Embedded, Options};
    use zen_store::{Error, KeyValue, Result, Stamp, Storage, Txn, Version, Watch};

    /// A store whose next `unknown` commits apply and then answer
    /// `CommitUnknown`, as FoundationDB does when a recovery interrupts one.
    struct MaybeCommitted {
        inner: Arc<dyn Storage>,
        unknown: Arc<AtomicU32>,
    }

    struct MaybeTxn {
        inner: Box<dyn Txn>,
        unknown: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl Storage for MaybeCommitted {
        async fn begin(&self, rv: Option<Version>) -> Result<Box<dyn Txn>> {
            Ok(Box::new(MaybeTxn {
                inner: self.inner.begin(rv).await?,
                unknown: self.unknown.clone(),
            }))
        }
        async fn watch(&self, key: &[u8]) -> Result<Watch> {
            self.inner.watch(key).await
        }
        async fn now_version(&self) -> Result<Version> {
            self.inner.now_version().await
        }
        async fn advance_version(&self, at_least: Version) -> Result<()> {
            self.inner.advance_version(at_least).await
        }
    }

    #[async_trait::async_trait]
    impl Txn for MaybeTxn {
        fn read_version(&self) -> Version {
            self.inner.read_version()
        }
        async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.inner.get(key).await
        }
        async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.inner.snapshot_get(key).await
        }
        async fn get_range(
            &mut self,
            b: &[u8],
            e: &[u8],
            limit: usize,
            reverse: bool,
        ) -> Result<Vec<KeyValue>> {
            self.inner.get_range(b, e, limit, reverse).await
        }
        async fn snapshot_get_range(
            &mut self,
            b: &[u8],
            e: &[u8],
            limit: usize,
            reverse: bool,
        ) -> Result<Vec<KeyValue>> {
            self.inner.snapshot_get_range(b, e, limit, reverse).await
        }
        fn add_read_conflict_range(&mut self, b: &[u8], e: &[u8]) {
            self.inner.add_read_conflict_range(b, e)
        }
        fn set(&mut self, k: &[u8], v: &[u8]) {
            self.inner.set(k, v)
        }
        fn clear(&mut self, k: &[u8]) {
            self.inner.clear(k)
        }
        fn clear_range(&mut self, b: &[u8], e: &[u8]) {
            self.inner.clear_range(b, e)
        }
        fn set_versionstamped_key(&mut self, p: &[u8], s: &[u8], v: &[u8]) {
            self.inner.set_versionstamped_key(p, s, v)
        }
        fn set_versionstamped_value(&mut self, k: &[u8], p: &[u8], s: &[u8]) {
            self.inner.set_versionstamped_value(k, p, s)
        }
        fn atomic_add(&mut self, k: &[u8], d: i64) {
            self.inner.atomic_add(k, d)
        }
        async fn commit(self: Box<Self>) -> Result<Stamp> {
            let stamp = self.inner.commit().await?;
            let left = self.unknown.load(Ordering::SeqCst);
            if left > 0 {
                self.unknown.store(left - 1, Ordering::SeqCst);
                return Err(Error::CommitUnknown);
            }
            Ok(stamp)
        }
    }

    /// Taking and renewing the lease survive a commit whose outcome is
    /// unknown (it applied): the lease is held, and in the future.
    #[tokio::test]
    async fn the_node_lease_survives_an_unknown_commit() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = Arc::new(AtomicU32::new(1));
        let store = Arc::new(MaybeCommitted {
            inner: Arc::new(
                Embedded::open(dir.path().join("zen.redb"), Options::default()).unwrap(),
            ),
            unknown: unknown.clone(),
        });
        let cfg = Config::with_data_dir(dir.path().to_path_buf());
        let st: Shared = Arc::new(AppState::new(
            cfg,
            store,
            AclState::default(),
            None,
            [0; 32],
        ));
        assert_eq!(renew(&st, true).await.map_err(|e| e.code), Ok(true));
        unknown.store(1, Ordering::SeqCst);
        assert_eq!(renew(&st, false).await.map_err(|e| e.code), Ok(true));
        let mut t = st.store.begin(None).await.unwrap();
        let until = decode_u64(
            t.get(&keys::node_lease(&st.svc.node))
                .await
                .unwrap()
                .as_deref(),
        );
        assert!(until > t.read_version());
    }
}
