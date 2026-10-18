//! The handler API (api.md §14.1–14.4, 14.6, 14.8).

use super::reap::{End, end_lease, reap_expired_leases, requeue_or_abort};
use super::{
    Acks, ClaimRec, Id, RH_EOF, RZ_RELEASED, Service, check_claim, claim_of, decode_head,
    decode_lease, decode_u64, encode_lease, flags, parse_id, tail_int,
};
use crate::acl::{R_APPEND, R_CONSUME};
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::check_topic;
use crate::keys::{self, end_of};
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_proto::{
    Empty, SvcChunk, SvcClaim, SvcClaims, SvcInfo, SvcInfoRequest, SvcLeaseRequest, SvcLeaseResult,
    SvcNextRequest, SvcReadRequest, SvcReadResult, SvcReleaseRequest, SvcStateRequest,
};
use zen_store::{Txn, VERSIONS_PER_SEC, key_after};

const DEFAULT_LEASE_MS: u32 = 5_000;
const MAX_LEASE_MS: u32 = 60_000;
const MAX_WAIT_MS: u32 = 30_000;
const MAX_CLAIMS: u32 = 64;

/// The configured service `svc` of `fs`, else 404.
pub(crate) fn service(st: &Shared, fs: u32, svc: &[u8]) -> ApiResult<Arc<Service>> {
    st.check_fs(fs)?;
    check_topic(svc)?;
    st.svc
        .service(fs, svc)
        .cloned()
        .ok_or_else(|| not_found("no such service"))
}

fn wait_of(ms: Option<u32>) -> Duration {
    Duration::from_millis(ms.unwrap_or(0).min(MAX_WAIT_MS).into())
}

/// `POST /v1/svc/info`.
pub async fn info(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcInfoRequest>,
) -> ApiResult<Cbor<SvcInfo>> {
    let s = service(&st, req.fs, &req.svc)?;
    if caller.require_topic(req.fs, &req.svc, R_CONSUME).is_err() {
        caller.require_topic(req.fs, &req.svc, R_APPEND)?;
    }
    let c = &s.cfg;
    Ok(Cbor(SvcInfo {
        mode: c.mode.name().into(),
        window: c.window,
        out_window: c.out_window,
        resume: c.resume,
        inbox: None,
        accept_wait_ms: c.accept_wait_ms,
        handshake_secs: c.handshake_secs,
        idle_secs: c.idle_secs,
    }))
}

/// `POST /v1/svc/lease`: acquire, renew or release a handler lease.
pub async fn lease(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcLeaseRequest>,
) -> ApiResult<Cbor<SvcLeaseResult>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    let lease = parse_id(&req.lease, "lease")?;
    let (fs, svc) = (req.fs, &req.svc[..]);
    if req.release == Some(true) {
        end_lease(&st, fs, svc, &lease, End::Release(caller.device)).await?;
        reap_expired_leases(&st, fs, svc).await?;
        return Ok(Cbor(SvcLeaseResult { expires_version: 0 }));
    }
    let ttl = u64::from(
        req.ttl_ms
            .unwrap_or(DEFAULT_LEASE_MS)
            .clamp(100, MAX_LEASE_MS),
    ) * (VERSIONS_PER_SEC / 1000);
    let key = keys::svc_lease(fs, svc, &lease);
    let expires = loop {
        let (r, _) = txn_loop!(st.store, None, |t| {
            let now = t.read_version();
            match t.get(&key).await?.as_deref().and_then(decode_lease) {
                Some((holder, exp)) if exp > now && holder != caller.device => {
                    return Err(not_leader("the lease is held by another device"));
                }
                // Expired: its connections are requeued before it is reused.
                Some((_, exp)) if exp <= now => return Ok(None),
                _ => {}
            }
            t.set(&key, &encode_lease(&caller.device, now + ttl));
            Ok(Some(now + ttl))
        })?;
        match r {
            Some(e) => break e,
            None => end_lease(&st, fs, svc, &lease, End::Expired).await?,
        }
    };
    reap_expired_leases(&st, fs, svc).await?;
    Ok(Cbor(SvcLeaseResult {
        expires_version: expires,
    }))
}

/// `POST /v1/svc/next`: claim queued connections.
pub async fn next(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcNextRequest>,
) -> ApiResult<Cbor<SvcClaims>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    let lease = parse_id(&req.lease, "lease")?;
    let (fs, svc) = (req.fs, &req.svc[..]);
    let limit = req.limit.unwrap_or(1).clamp(1, MAX_CLAIMS) as usize;
    let wait = wait_of(req.wait_ms);
    let deadline = Instant::now() + wait;
    let bell = keys::svc_queue_bell(fs, svc);
    loop {
        // Reaping first requeues the connections of dead handlers.
        let earliest = reap_expired_leases(&st, fs, svc).await?;
        let w = if wait.is_zero() {
            None
        } else {
            Some(st.store.watch(&bell).await?)
        };
        let ((conns, rv), _) = txn_loop!(st.store, None, |t| {
            let conns = claim_some(&mut t, fs, svc, &lease, &caller.device, limit).await?;
            Ok((conns, t.read_version()))
        })?;
        let now = Instant::now();
        let Some(w) = w.filter(|_| conns.is_empty() && now < deadline) else {
            return Ok(Cbor(SvcClaims { conns }));
        };
        // Wake no later than the first lease expiry: its connections are
        // then requeued, so a takeover waits about a lease's lifetime.
        let mut slice = deadline - now;
        if let Some(e) = earliest {
            slice =
                slice.min(Duration::from_micros(e.saturating_sub(rv)) + Duration::from_millis(5));
        }
        let _ = tokio::time::timeout(slice, w).await;
    }
}

/// Claim up to `limit` connections from the front of the ready queue.
async fn claim_some(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    lease: &Id,
    device: &[u8; 32],
    limit: usize,
) -> ApiResult<Vec<SvcClaim>> {
    let now = t.read_version();
    match t
        .get(&keys::svc_lease(fs, svc, lease))
        .await?
        .as_deref()
        .and_then(decode_lease)
    {
        Some((holder, exp)) if holder == *device && exp > now => {}
        _ => return Err(not_leader("no live lease with this id")),
    }
    let q = keys::svc_queue(fs, svc).finish();
    let entries = t.snapshot_get_range(&q, &end_of(&q), limit, false).await?;
    let mut out = Vec::new();
    for (k, v) in entries {
        let (Ok(conn), Some(vs)) = (Id::try_from(v.as_slice()), k.len().checked_sub(12)) else {
            continue;
        };
        let vs: [u8; 12] = k[vs..].try_into().expect("12");
        t.add_read_conflict_range(&k, &key_after(&k));
        let rm = keys::conn("rm", fs, svc, &conn);
        let Some(c) = t.get(&rm).await?.as_deref().and_then(ClaimRec::decode) else {
            continue;
        };
        if c.queued != Some(vs) {
            continue;
        }
        let token = c.token + 1;
        t.set(&rm, &ClaimRec::claimed(lease, device, token));
        t.clear(&k);
        t.set(&keys::svc_claims(fs, svc, lease).bytes(&conn).finish(), &[]);
        out.push(claim_of(t, fs, svc, &conn, token).await?);
    }
    Ok(out)
}

/// `POST /v1/svc/state`: the holder re-reads its connection.
pub async fn state(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcStateRequest>,
) -> ApiResult<Cbor<SvcClaim>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    let conn = parse_id(&req.conn, "conn")?;
    let (claim, _) = txn_loop!(st.store, None, |t| {
        check_claim(&mut t, req.fs, &req.svc, &conn, req.token, &caller.device).await?;
        claim_of(&mut t, req.fs, &req.svc, &conn, req.token).await
    })?;
    Ok(Cbor(claim))
}

/// `POST /v1/svc/read`: input from `from`, long-polling on the lease's
/// doorbell.
pub async fn read(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcReadRequest>,
) -> ApiResult<Cbor<SvcReadResult>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    let conn = parse_id(&req.conn, "conn")?;
    let (fs, svc) = (req.fs, &req.svc[..]);
    let cap = st.cfg.limits.max_range_bytes;
    let max = req.max_bytes.map_or(cap, |m| u64::from(m).min(cap)).max(1);
    let items = st.cfg.limits.max_range_items as usize;
    let wait = wait_of(req.wait_ms);
    let deadline = Instant::now() + wait;
    let mut bell: Option<Vec<u8>> = None;
    loop {
        // The watch is armed before the read, so no input is missed.
        let w = match &bell {
            Some(b) => Some(st.store.watch(b).await?),
            None => None,
        };
        let ((res, lease), _) = txn_loop!(st.store, None, |t| {
            let rm = check_claim(&mut t, fs, svc, &conn, req.token, &caller.device).await?;
            let k = |tag| keys::conn(tag, fs, svc, &conn);
            let ra = Acks::decode(t.snapshot_get(&k("ra")).await?.as_deref());
            if req.from < ra.in_acked {
                return Err(cursor_moved("from is below in_acked"));
            }
            let (in_head, rh) = decode_head(t.snapshot_get(&k("rh")).await?.as_deref());
            let written_to = decode_u64(t.snapshot_get(&k("rw")).await?.as_deref());
            let chunks = chunks(&mut t, fs, svc, &conn, req.from, max, items).await?;
            Ok((
                SvcReadResult {
                    chunks,
                    in_head,
                    written_to,
                    flags: flags(rh, ra.flags),
                },
                rm.lease,
            ))
        })?;
        let now = Instant::now();
        if !res.chunks.is_empty() || res.flags & RH_EOF != 0 || now >= deadline {
            return Ok(Cbor(res));
        }
        match w {
            Some(w) => {
                let _ = tokio::time::timeout(deadline - now, w).await;
            }
            None => bell = Some(keys::svc_lease_bell(fs, svc, &lease)),
        }
    }
}

/// Input chunks from the one containing `from`, up to `max` bytes (at
/// least one chunk). Chunks are immutable, so snapshot reads are exact.
async fn chunks(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    from: u64,
    max: u64,
    items: usize,
) -> ApiResult<Vec<SvcChunk>> {
    let ri = keys::conn_key("ri", fs, svc, conn);
    let p = ri.clone().finish();
    let at = ri.int(from as i64).finish();
    let mut out: Vec<SvcChunk> = Vec::new();
    let mut bytes = 0u64;
    let mut start = at.clone();
    if let Some((k, v)) = t
        .snapshot_get_range(&p, &key_after(&at), 1, true)
        .await?
        .into_iter()
        .next()
    {
        let c = chunk(&k, v)?;
        if c.off + c.data.len() as u64 > from {
            start = key_after(&k);
            bytes += c.data.len() as u64;
            out.push(c);
        }
    }
    if bytes < max {
        for (k, v) in t
            .snapshot_get_range(&start, &end_of(&p), items, false)
            .await?
        {
            let c = chunk(&k, v)?;
            if !out.is_empty() && bytes + c.data.len() as u64 > max {
                break;
            }
            bytes += c.data.len() as u64;
            out.push(c);
        }
    }
    Ok(out)
}

/// An `ri` entry: `u8 sealed ‖ data`.
fn chunk(k: &[u8], v: Vec<u8>) -> ApiResult<SvcChunk> {
    let off = tail_int(k).ok_or_else(|| internal("bad input key"))?;
    let (sealed, data) = v
        .split_first()
        .ok_or_else(|| internal("empty input chunk"))?;
    Ok(SvcChunk {
        off,
        data: data.to_vec(),
        sealed: *sealed != 0,
    })
}

/// `POST /v1/svc/release`: the holder gives up one connection.
pub async fn release(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcReleaseRequest>,
) -> ApiResult<Cbor<Empty>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    let conn = parse_id(&req.conn, "conn")?;
    let lease = parse_id(&req.lease, "lease")?;
    let (fs, svc) = (req.fs, &req.svc[..]);
    txn_loop!(st.store, None, |t| {
        let rm = check_claim(&mut t, fs, svc, &conn, req.token, &caller.device).await?;
        if rm.lease != lease {
            return Err(claim_lost("the connection is claimed under another lease"));
        }
        let mut uv = 0u16;
        if req.close == Some(true) {
            super::reap::abort_request(&mut t, fs, svc, &conn, RZ_RELEASED, &mut uv).await?;
        } else {
            requeue_or_abort(&mut t, fs, svc, &conn, &mut uv).await?;
        }
        Ok(())
    })?;
    Ok(Cbor(Empty {}))
}
