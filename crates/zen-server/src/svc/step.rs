//! The `svc` commit step (svc.md §4, api.md §14.5): acks input, stores
//! output records and the state, and wakes the owning node's writer.

use super::api::service;
use super::{
    Acks, ConnRec, Id, RA_ABORT, RA_CLOSE, RA_FIN, RA_RESUMABLE, RA_UNSEAL, check_claim,
    decode_head, decode_u64, encode_work, parse_id, ring,
};
use crate::acl::R_CONSUME;
use crate::auth::Caller;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use std::collections::{HashMap, HashSet};
use zen_core::svc::{ABORT, CLOSE, FIN, Record, parse_record};
use zen_proto::SvcStep;
use zen_store::Txn;

/// The bytes a sealed record adds to its raw bytes, at most (formats.md
/// §13.1); a record's raw bytes are at most `max_value_bytes` minus this.
const RECORD_ROOM: usize = 130;

/// The flags that end a connection's output.
const ENDS: u8 = FIN | CLOSE | ABORT;

/// The stateless checks of a step (svc.md §4, check 1). Returns the bytes
/// it counts toward `max_commit_bytes`.
pub(crate) fn validate(
    st: &Shared,
    caller: &Caller,
    s: &SvcStep,
    seen: &mut HashSet<(u32, Vec<u8>, Id)>,
) -> ApiResult<usize> {
    let svc = service(st, s.fs, &s.svc)?;
    caller.require_topic(s.fs, &s.svc, R_CONSUME)?;
    let conn = parse_id(&s.conn, "conn")?;
    if s.in_from > s.in_to {
        return Err(bad_request("in_from is after in_to"));
    }
    if !seen.insert((s.fs, s.svc.clone(), conn)) {
        return Err(bad_request("one step per connection per commit"));
    }
    let max = st.cfg.limits.max_value_bytes as usize;
    let state = s.state.as_ref().map_or(0, Vec::len);
    if state > max {
        return Err(bad_request("state too large"));
    }
    if s.resumable == Some(true) && !svc.cfg.resume {
        return Err(bad_request("a resume: false connection is never resumable"));
    }
    records(&s.out, svc.cfg.resume, max)?;
    Ok(state + s.out.iter().map(|r| r.len()).sum::<usize>())
}

/// Parse a step's records and check their shape. Returns them with their
/// total raw bytes.
fn records(
    out: &[zen_proto::ByteBuf],
    resume: bool,
    max: usize,
) -> ApiResult<(Vec<Record<'_>>, u64)> {
    let mut recs = Vec::with_capacity(out.len());
    let mut raw = 0u64;
    for r in out {
        let rec = parse_record(r).map_err(|_| bad_request("malformed output record"))?;
        if recs.last().is_some_and(|p: &Record| p.flags & ENDS != 0) {
            return Err(bad_request("a record follows a FIN, close or abort"));
        }
        if rec.raw_len() > max.saturating_sub(RECORD_ROOM) {
            return Err(bad_request("output record too large"));
        }
        if rec.is_plain() == resume {
            return Err(bad_request(if resume {
                "records are sealed on a resume service"
            } else {
                "records are plain on a resume: false service"
            }));
        }
        raw += rec.raw_len() as u64;
        recs.push(rec);
    }
    Ok((recs, raw))
}

/// Apply a step inside the commit's transaction (svc.md §4, checks 2–7 and
/// the effects). `i` numbers the step in the commit, for its work entry;
/// the quota change is added to `delta`.
pub(crate) async fn apply(
    t: &mut Box<dyn Txn>,
    st: &Shared,
    caller: &Caller,
    s: &SvcStep,
    i: u16,
    delta: &mut HashMap<u32, (i64, i64)>,
) -> ApiResult<()> {
    let (fs, svc) = (s.fs, &s.svc[..]);
    let conn = parse_id(&s.conn, "conn")?;
    let k = |tag| keys::conn(tag, fs, svc, &conn);
    check_claim(t, fs, svc, &conn, s.token, &caller.device).await?;
    let rc = t
        .get(&k("rc"))
        .await?
        .ok_or_else(|| conn_closed("the connection is closed"))?;
    let rc = ConnRec::decode(&rc)?;
    let (recs, raw) = records(&s.out, rc.resume, st.cfg.limits.max_value_bytes as usize)?;
    if s.resumable == Some(true) && !rc.resume {
        return Err(bad_request("a resume: false connection is never resumable"));
    }
    let ra = Acks::decode(t.get(&k("ra")).await?.as_deref());
    if s.in_from != ra.in_acked || s.out_from != ra.out_head {
        return Err(cursor_moved("the step's cursors are not current"));
    }
    let (in_head, _) = decode_head(t.snapshot_get(&k("rh")).await?.as_deref());
    if s.in_to > in_head {
        return Err(cursor_moved("in_to is beyond the input received"));
    }
    if ra.flags & (RA_CLOSE | RA_ABORT) != 0 || (ra.flags & RA_FIN != 0 && !recs.is_empty()) {
        return Err(conn_closed("the connection's output is closed"));
    }
    if raw > 0 {
        let written_to = decode_u64(t.snapshot_get(&k("rw")).await?.as_deref());
        if ra.out_head + raw - written_to.min(ra.out_head) > u64::from(rc.out_window) {
            return Err(window("the output window is full"));
        }
    }
    let moves = s.in_to > s.in_from || raw > 0;
    if ra.state_len > 0 && moves && s.state.is_none() {
        return Err(bad_request(
            "a step that moves a cursor must carry the new state",
        ));
    }

    // Input: clear the chunks wholly below in_to.
    let mut in_cleared = ra.in_cleared;
    if s.in_to > ra.in_cleared {
        let ri = keys::conn_key("ri", fs, svc, &conn);
        let begin = ri.clone().finish();
        let end = ri.clone().int(s.in_to as i64).finish();
        in_cleared = s.in_to;
        if let Some((key, v)) = t.snapshot_get_range(&begin, &end, 1, true).await?.pop() {
            let off = super::tail_int(&key).ok_or_else(|| internal("bad chunk key"))?;
            if off + chunk_len(&v) > s.in_to {
                in_cleared = off;
            }
        }
        in_cleared = in_cleared.max(ra.in_cleared);
        if in_cleared > ra.in_cleared {
            t.clear_range(&begin, &ri.int(in_cleared as i64).finish());
        }
    }
    // Records, each at its offset.
    let mut off = ra.out_head;
    let mut flags = ra.flags;
    for (rec, bytes) in recs.iter().zip(&s.out) {
        t.set(
            &keys::conn_key("ro", fs, svc, &conn)
                .int(off as i64)
                .finish(),
            bytes,
        );
        off += rec.raw_len() as u64;
        for (f, ra_f) in [(FIN, RA_FIN), (CLOSE, RA_CLOSE), (ABORT, RA_ABORT)] {
            if rec.flags & f != 0 {
                flags |= ra_f;
            }
        }
    }
    if s.resumable == Some(true) {
        flags |= RA_RESUMABLE;
    }
    if s.unseal == Some(true) {
        flags |= RA_UNSEAL;
    }
    let state_len = s.state.as_ref().map_or(ra.state_len, |v| v.len() as u32);
    if let Some(state) = &s.state {
        t.set(&k("rs"), state);
    }
    let new = Acks {
        in_acked: s.in_to,
        in_cleared,
        out_head: off,
        state_len,
        flags,
    };
    t.set(&k("ra"), &new.encode());
    let d = delta.entry(fs).or_default();
    d.0 += raw as i64 - (in_cleared - ra.in_cleared) as i64 + i64::from(state_len)
        - i64::from(ra.state_len);
    // Wake the owning node: records to write, or a window to reopen.
    if (moves || !recs.is_empty() || flags != ra.flags)
        && let Some(node) = rc.node()
    {
        let (p, sfx) = keys::node_work(&node).vs_incomplete(i).finish_incomplete();
        t.set_versionstamped_key(&p, &sfx, &encode_work(fs, svc, &conn));
        ring(t, &keys::node_bell(&node));
    }
    Ok(())
}

/// The input bytes an `ri` value holds: `u8 sealed ‖ data`, where a sealed
/// chunk carries a 16-byte tag (formats.md §13.3).
pub(crate) fn chunk_len(v: &[u8]) -> u64 {
    match v.first() {
        Some(1) => v.len().saturating_sub(17) as u64,
        _ => v.len().saturating_sub(1) as u64,
    }
}
