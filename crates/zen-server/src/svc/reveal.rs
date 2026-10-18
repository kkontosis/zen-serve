//! `POST /v1/svc/reveal` (api.md §14.7, svc.md §5): release committed
//! output records by their keys.

use super::api::service;
use super::reap::abort_request;
use super::{ClaimRec, Cmd, ConnRec, Id, RZ_BAD_RECORD, decode_u64, encode_work, parse_id, ring};
use crate::acl::R_CONSUME;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use zen_proto::{SvcRevealRequest, SvcRevealResult};

/// Most reveals per request.
pub const MAX_REVEALS: usize = 1_000;

/// Revealed: the key is stored and the owning node woken.
pub const REVEALED: u8 = 0;
/// Already revealed or written, or a plain record.
pub const ALREADY: u8 = 1;
/// No committed record there, or the connection is reaped.
pub const NOT_COMMITTED: u8 = 2;
/// The key doesn't open the record.
pub const BAD_KEY: u8 = 3;

/// `POST /v1/svc/reveal`.
pub async fn reveal(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<SvcRevealRequest>,
) -> ApiResult<Cbor<SvcRevealResult>> {
    service(&st, req.fs, &req.svc)?;
    caller.require_topic(req.fs, &req.svc, R_CONSUME)?;
    if req.reveals.len() > MAX_REVEALS {
        return Err(too_large("too many reveals"));
    }
    let mut todo = Vec::with_capacity(req.reveals.len());
    for r in &req.reveals {
        let conn = parse_id(&r.conn, "conn")?;
        let key: [u8; 32] = r
            .key
            .as_slice()
            .try_into()
            .map_err(|_| bad_request("key must be 32 bytes"))?;
        todo.push((conn, r.off, key));
    }
    let mut results = Vec::with_capacity(todo.len());
    for (conn, off, key) in &todo {
        results.push(one(&st, &caller, req.fs, &req.svc, conn, *off, key).await?);
    }
    Ok(Cbor(SvcRevealResult { results }))
}

/// One reveal, in its own transaction.
async fn one(
    st: &Shared,
    caller: &Caller,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    off: u64,
    key: &[u8; 32],
) -> ApiResult<u8> {
    let k = |tag| keys::conn(tag, fs, svc, conn);
    let at = |tag| keys::conn_key(tag, fs, svc, conn).int(off as i64).finish();
    let ((code, node), _) = txn_loop!(st.store, None, |t| {
        let Some(rc) = t.get(&k("rc")).await? else {
            return Ok((NOT_COMMITTED, None));
        };
        let rc = ConnRec::decode(&rc)?;
        if off < decode_u64(t.snapshot_get(&k("rw")).await?.as_deref())
            || t.get(&at("rv")).await?.is_some()
        {
            return Ok((ALREADY, None));
        }
        let Some(rec) = t.get(&at("ro")).await? else {
            return Ok((NOT_COMMITTED, None));
        };
        if zen_core::svc::parse_record(&rec).is_ok_and(|r| r.is_plain()) {
            return Ok((ALREADY, None));
        }
        match zen_core::svc::reveal(fs, svc, conn, off, &rec, key) {
            Ok(_) => {}
            Err(zen_core::Error::Decrypt) => {
                // The key is the record's, but its body doesn't open: the
                // record can never be written. Only the holder's word
                // counts, since anyone with `consume` could send a key.
                let holder = t
                    .get(&k("rm"))
                    .await?
                    .as_deref()
                    .and_then(ClaimRec::decode)
                    .is_some_and(|c| c.is_claimed() && c.holder == caller.device);
                if holder && t.get(&k("rz")).await?.is_none() {
                    let mut uv = 0;
                    abort_request(&mut t, fs, svc, conn, RZ_BAD_RECORD, &mut uv).await?;
                }
                return Ok((BAD_KEY, None));
            }
            Err(_) => return Ok((BAD_KEY, None)),
        }
        t.set(&at("rv"), key);
        let node = rc.node();
        if let Some(node) = &node {
            let (p, s) = keys::node_work(node).vs_incomplete(0).finish_incomplete();
            t.set_versionstamped_key(&p, &s, &encode_work(fs, svc, conn));
            ring(&mut t, &keys::node_bell(node));
        }
        Ok((REVEALED, node))
    })?;
    // The fast path: a connection of this node is woken at once.
    if node == Some(st.svc.node) {
        st.svc.send(conn, Cmd::Wake);
    }
    Ok(code)
}
