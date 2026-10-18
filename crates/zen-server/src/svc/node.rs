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
pub async fn renew(st: &Shared, first: bool) -> ApiResult<bool> {
    let key = keys::node_lease(&st.svc.node);
    let (held, _) = txn_loop!(st.store, None, |t| {
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
