//! Ending connections and leases: reaping (svc.md §3, step 10), requeueing
//! on takeover (§6), handler leases ending (§10) and the sweeper.

use super::{Acks, ClaimRec, ConnRec, Id, RA_RESUMABLE, RZ_NOT_RESUMABLE, decode_head};
use super::{decode_lease, decode_u64, encode_work, enqueue, ring};
use crate::error::{ApiResult, not_leader};
use crate::keys::{self, end_of};
use crate::state::Shared;
use crate::txn::txn_loop;
use zen_store::tuple::{Elem, unpack};
use zen_store::{Txn, Version};

/// Leases, `rx` entries and node connections handled per transaction.
const PAGE: usize = 100;

/// Reap a connection: delete every key of it, give its bytes back to the
/// quota and ring its holder's doorbell. A reaped connection is a no-op.
pub async fn reap(st: &Shared, fs: u32, svc: &[u8], conn: &Id) -> ApiResult<()> {
    txn_loop!(st.store, None, |t| {
        reap_in(&mut t, fs, svc, conn, false).await
    })?;
    Ok(())
}

/// Reap a connection no handler ever claimed (`accept_wait_ms`). Returns
/// whether it is gone; false once it has been claimed.
pub async fn reap_unclaimed(st: &Shared, fs: u32, svc: &[u8], conn: &Id) -> ApiResult<bool> {
    let (gone, _) = txn_loop!(st.store, None, |t| {
        reap_in(&mut t, fs, svc, conn, true).await
    })?;
    Ok(gone)
}

async fn reap_in(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    only_unclaimed: bool,
) -> ApiResult<bool> {
    let k = |tag| keys::conn(tag, fs, svc, conn);
    let rm = t.get(&k("rm")).await?;
    let rc = t.get(&k("rc")).await?;
    if rm.is_none() && rc.is_none() {
        return Ok(true);
    }
    let claim = rm.as_deref().and_then(ClaimRec::decode);
    if only_unclaimed && claim.as_ref().is_some_and(|c| c.token > 0) {
        return Ok(false);
    }
    let (in_head, _) = decode_head(t.get(&k("rh")).await?.as_deref());
    let ra = Acks::decode(t.get(&k("ra")).await?.as_deref());
    let written_to = decode_u64(t.get(&k("rw")).await?.as_deref());
    for tag in ["rc", "rh", "rw", "ra", "rm", "rz", "rs"] {
        t.clear(&k(tag));
    }
    for tag in ["ri", "ro", "rv"] {
        let p = keys::conn_key(tag, fs, svc, conn).finish();
        t.clear_range(&p, &end_of(&p));
    }
    if let Some(node) = rc
        .as_deref()
        .and_then(|v| ConnRec::decode(v).ok())
        .and_then(|r| r.node())
    {
        t.clear(
            &keys::node_conns(&node)
                .int(fs.into())
                .bytes(svc)
                .bytes(conn)
                .finish(),
        );
    }
    if let Some(c) = claim {
        if let Some(vs) = c.queued {
            t.clear(&keys::svc_queue(fs, svc).vs(&vs).finish());
        }
        if c.is_claimed() {
            t.clear(&keys::svc_claims(fs, svc, &c.lease).bytes(conn).finish());
            // Not for an ended lease: its doorbell is gone with it.
            if t.snapshot_get(&keys::svc_lease(fs, svc, &c.lease))
                .await?
                .is_some()
            {
                ring(t, &keys::svc_lease_bell(fs, svc, &c.lease));
            }
        }
    }
    let held = in_head.saturating_sub(ra.in_cleared)
        + ra.out_head.saturating_sub(written_to)
        + u64::from(ra.state_len);
    if held > 0 {
        t.atomic_add(&keys::quota(fs, "bytes"), -(held as i64));
    }
    Ok(true)
}

/// Ask the owning node to reset and reap a connection (svc.md §3, step 11):
/// `rz`, a work entry for the node and its doorbell. `uv` numbers the
/// versionstamped keys of one transaction.
pub async fn abort_request(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    reason: u8,
    uv: &mut u16,
) -> ApiResult<()> {
    t.set(&keys::conn("rz", fs, svc, conn), &[reason]);
    let node = t
        .get(&keys::conn("rc", fs, svc, conn))
        .await?
        .and_then(|v| ConnRec::decode(&v).ok())
        .and_then(|r| r.node());
    if let Some(node) = node {
        let (p, s) = keys::node_work(&node)
            .vs_incomplete(*uv)
            .finish_incomplete();
        *uv += 1;
        t.set_versionstamped_key(&p, &s, &encode_work(fs, svc, conn));
        ring(t, &keys::node_bell(&node));
    }
    Ok(())
}

/// A handler gave up a connection, or its lease ended (svc.md §6): requeue
/// it when it is fresh or resumable, else ask its node to abort it.
pub async fn requeue_or_abort(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    uv: &mut u16,
) -> ApiResult<()> {
    let rm = keys::conn("rm", fs, svc, conn);
    let Some(c) = t.get(&rm).await?.as_deref().and_then(ClaimRec::decode) else {
        return Ok(());
    };
    if c.is_claimed() {
        t.clear(&keys::svc_claims(fs, svc, &c.lease).bytes(conn).finish());
    }
    let ra = Acks::decode(t.get(&keys::conn("ra", fs, svc, conn)).await?.as_deref());
    let aborting = t.get(&keys::conn("rz", fs, svc, conn)).await?.is_some();
    let fresh = ra.in_acked == 0 && ra.out_head == 0;
    if !aborting && (fresh || ra.flags & RA_RESUMABLE != 0) {
        enqueue(t, fs, svc, conn, c.token, *uv);
        *uv += 1;
    } else {
        t.set(&rm, &ClaimRec::claimed(&[0; 16], &[0; 32], c.token));
        if !aborting {
            abort_request(t, fs, svc, conn, RZ_NOT_RESUMABLE, uv).await?;
        }
    }
    Ok(())
}

/// How a lease ends.
#[derive(Clone, Copy, Debug)]
pub enum End {
    /// Expired: ended only if still expired.
    Expired,
    /// Released by this device.
    Release([u8; 32]),
}

/// End a handler lease: requeue or abort each of its connections, a page
/// per transaction, and delete the lease last (svc.md §10).
pub async fn end_lease(st: &Shared, fs: u32, svc: &[u8], lease: &Id, how: End) -> ApiResult<()> {
    let key = keys::svc_lease(fs, svc, lease);
    let rx = keys::svc_claims(fs, svc, lease).finish();
    loop {
        let (done, _) = txn_loop!(st.store, None, |t| {
            let Some((holder, expires)) = t.get(&key).await?.as_deref().and_then(decode_lease)
            else {
                return Ok(true);
            };
            let live = expires > t.read_version();
            match how {
                End::Expired if live => return Ok(true),
                End::Release(d) if live && holder != d => {
                    return Err(not_leader("the lease is held by another device"));
                }
                _ => {}
            }
            let page = t.get_range(&rx, &end_of(&rx), PAGE, false).await?;
            let mut uv = 0u16;
            for (k, _) in &page {
                if let Some(conn) = last_id(k) {
                    requeue_or_abort(&mut t, fs, svc, &conn, &mut uv).await?;
                }
                t.clear(k);
            }
            if page.len() < PAGE {
                // Deleting the doorbell wakes its watchers, like a ring.
                t.clear(&key);
                t.clear(&keys::svc_lease_bell(fs, svc, lease));
                return Ok(true);
            }
            Ok(false)
        })?;
        if done {
            return Ok(());
        }
    }
}

/// End the service's expired leases (a lease call, `next`, the sweeper).
/// Returns the earliest expiry among the live ones.
pub async fn reap_expired_leases(st: &Shared, fs: u32, svc: &[u8]) -> ApiResult<Option<Version>> {
    let p = keys::svc_key("rl", fs, svc).finish();
    let ((leases, now), _) = txn_loop!(st.store, None, |t| {
        Ok((
            t.snapshot_get_range(&p, &end_of(&p), 10_000, false).await?,
            t.read_version(),
        ))
    })?;
    let mut earliest: Option<Version> = None;
    for (k, v) in leases {
        let (Some((_, expires)), Some(lease)) = (decode_lease(&v), last_id(&k)) else {
            continue;
        };
        if expires <= now {
            end_lease(st, fs, svc, &lease, End::Expired).await?;
        } else {
            earliest = Some(earliest.map_or(expires, |e| e.min(expires)));
        }
    }
    Ok(earliest)
}

/// The sweeper's relay pass: reap the connections of expired node leases,
/// then the expired handler leases of every service.
pub async fn sweep(st: &Shared, now: Version) -> ApiResult<()> {
    let p = keys::node_leases().finish();
    let (nodes, _) = txn_loop!(st.store, None, |t| {
        Ok(t.snapshot_get_range(&p, &end_of(&p), 10_000, false).await?)
    })?;
    for (k, v) in nodes {
        if decode_u64(Some(&v)) < now
            && let Some(node) = last_id(&k)
        {
            tracing::info!(node = %crate::cred::hex(&node), "reaping a dead node's connections");
            reap_node(st, &node).await?;
        }
    }
    for s in st.svc.services() {
        reap_expired_leases(st, s.cfg.fs, &s.cfg.id).await?;
    }
    Ok(())
}

/// Reap every connection of an expired node, then its lease. Stops if the
/// node renews its lease meanwhile: it then aborts its sockets itself.
pub async fn reap_node(st: &Shared, node: &Id) -> ApiResult<()> {
    let p = keys::node_conns(node).finish();
    let lease = keys::node_lease(node);
    loop {
        let ((page, live), _) = txn_loop!(st.store, None, |t| {
            let live = decode_u64(t.snapshot_get(&lease).await?.as_deref()) >= t.read_version();
            Ok((
                t.snapshot_get_range(&p, &end_of(&p), PAGE, false).await?,
                live,
            ))
        })?;
        if live {
            return Ok(());
        }
        for (k, _) in &page {
            let e = unpack(k).unwrap_or_default();
            match e.as_slice() {
                [.., Elem::Int(fs), Elem::Bytes(svc), Elem::Bytes(conn)]
                    if u32::try_from(*fs).is_ok() && conn.len() == 16 =>
                {
                    let conn: Id = conn.as_slice().try_into().expect("16");
                    reap(st, *fs as u32, svc, &conn).await?;
                }
                _ => {}
            }
            // Reaping cleared it; a malformed or orphaned entry is cleared here.
            txn_loop!(st.store, None, |t| {
                t.clear(k);
                Ok(())
            })?;
        }
        if page.len() < PAGE {
            let (done, _) = txn_loop!(st.store, None, |t| {
                if !t.get_range(&p, &end_of(&p), 1, false).await?.is_empty() {
                    return Ok(false);
                }
                if decode_u64(t.get(&lease).await?.as_deref()) < t.read_version() {
                    t.clear(&lease);
                }
                Ok(true)
            })?;
            if done {
                return Ok(());
            }
        }
    }
}

/// The trailing 16-byte element of a key (a lease, node or connection id).
pub fn last_id(k: &[u8]) -> Option<Id> {
    match unpack(k).ok()?.pop()? {
        Elem::Bytes(b) => b.try_into().ok(),
        _ => None,
    }
}
