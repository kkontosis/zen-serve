//! Pushed input over `/v1/stream` (api.md §14.10, svc.md §10): the input of
//! the connections held under a lease, found through the lease's input log
//! `rli`, and wake-ups of the service's ready queue.

use super::api::chunks;
use super::reap::last_id;
use super::{Acks, ClaimRec, Id, RH_EOF, decode_head, decode_lease, flags};
use crate::acl::R_CONSUME;
use crate::auth::{Caller, resolve};
use crate::error::{ApiResult, not_leader};
use crate::keys::{self, end_of};
use crate::state::Shared;
use crate::txn::txn_loop;
use futures::FutureExt;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use zen_proto::Frame;
use zen_store::{VERSIONS_PER_SEC, Version, Watch, key_after};

/// `rli` entries and `rx` keys read per transaction.
const PAGE: usize = 1_000;

/// What has been pushed of one held connection.
#[derive(Default)]
struct Held {
    /// The claim token the pushes belong to; 0 before the first read.
    token: u64,
    /// The end of the input pushed.
    next: u64,
    /// The EOF has been pushed.
    eof_sent: bool,
}

/// Check that `caller` may push `lease`: topic `consume`, and the lease is
/// live and held by the caller's device.
pub(crate) async fn check(
    st: &Shared,
    caller: &Caller,
    fs: u32,
    svc: &[u8],
    lease: &Id,
) -> ApiResult<()> {
    caller.require_topic(fs, svc, R_CONSUME)?;
    let key = keys::svc_lease(fs, svc, lease);
    let (live, _) = txn_loop!(st.store, None, |t| {
        let now = t.read_version();
        Ok(t.snapshot_get(&key)
            .await?
            .as_deref()
            .and_then(decode_lease)
            .is_some_and(|(holder, exp)| holder == caller.device && exp > now))
    })?;
    if !live {
        return Err(not_leader("no live lease with this id"));
    }
    Ok(())
}

/// One `svc_sub`, until the lease ends, the rights go, or the socket closes.
/// The caller has checked it and sent `ok`.
pub(crate) async fn run(
    st: Shared,
    token: Vec<u8>,
    id: u32,
    fs: u32,
    svc: Vec<u8>,
    lease: Id,
    out: mpsc::Sender<Frame>,
) {
    if let Err(e) = push(&st, &token, id, fs, &svc, &lease, &out).await {
        let _ = out
            .send(Frame::Err {
                id: Some(id),
                code: e.code.into(),
                message: e.message,
            })
            .await;
    }
}

async fn push(
    st: &Shared,
    token: &[u8],
    id: u32,
    fs: u32,
    svc: &[u8],
    lease: &Id,
    out: &mpsc::Sender<Frame>,
) -> ApiResult<()> {
    let lease_bell = keys::svc_lease_bell(fs, svc, lease);
    let queue_bell = keys::svc_queue_bell(fs, svc);
    let log = keys::svc_lease_input(fs, svc, lease).finish();
    // Log entries older than this may be swept before they are read.
    let stale = st.cfg.limits.ephemeral_ttl_secs * VERSIONS_PER_SEC / 2;
    let mut held: HashMap<Id, Held> = HashMap::new();
    // The last log key read, and the version up to which the log was read.
    let mut cursor: Option<Vec<u8>> = None;
    let mut cursor_at: Option<Version> = None;
    // Connections with input left over from the last turn, and whether
    // the log has more entries to read.
    let mut again: HashSet<Id> = HashSet::new();
    let mut log_left;
    let (mut w_lease, mut w_queue): (Option<Watch>, Option<Watch>) = (None, None);
    let mut first = true;
    loop {
        let caller = resolve(st, token).await?;
        check(st, &caller, fs, svc, lease).await?;
        // Watch first, then read: a change between the two still wakes us.
        if w_lease.is_none() {
            w_lease = Some(st.store.watch(&lease_bell).await?);
        }
        if w_queue.is_none() {
            w_queue = Some(st.store.watch(&queue_bell).await?);
        }
        if first && queued(st, fs, svc).await? && !send(out, Frame::SvcReady { id }).await {
            return Ok(());
        }

        // The connections to read this turn.
        let mut changed: Vec<Id> = again.drain().collect();
        let (now, newest) = log_tail(st, &log).await?;
        if cursor_at.is_none_or(|at| now.saturating_sub(at) > stale) {
            // A rescan: everything claimed under the lease, then the log
            // from its newest entry on.
            let conns = claimed(st, fs, svc, lease).await?;
            held.retain(|c, _| conns.contains(c));
            for c in conns {
                held.entry(c).or_default();
                changed.push(c);
            }
            cursor = newest;
            cursor_at = Some(now);
            log_left = false;
        } else {
            let begin = cursor.as_deref().map_or_else(|| log.clone(), key_after);
            let ((entries, rv), _) = txn_loop!(st.store, None, |t| {
                Ok((
                    t.snapshot_get_range(&begin, &end_of(&log), PAGE, false)
                        .await?,
                    t.read_version(),
                ))
            })?;
            let full = entries.len() == PAGE;
            log_left = full;
            for (k, v) in &entries {
                if let Ok(c) = Id::try_from(v.as_slice()) {
                    held.entry(c).or_default();
                    changed.push(c);
                }
                cursor = Some(k.clone());
            }
            cursor_at = Some(match (&cursor, full) {
                (Some(k), true) => version_of(k).unwrap_or(rv),
                _ => rv,
            });
        }
        first = false;
        let mut seen = HashSet::new();
        changed.retain(|c| seen.insert(*c));

        for conn in changed {
            let Some(h) = held.get_mut(&conn) else {
                continue;
            };
            match conn_turn(st, &caller, fs, svc, lease, &conn, h, id).await? {
                Turn::Gone => {
                    held.remove(&conn);
                }
                Turn::Frames(frames, left) => {
                    for f in frames {
                        if !send(out, f).await {
                            return Ok(());
                        }
                    }
                    if left {
                        again.insert(conn);
                    }
                }
            }
        }

        let (wl, wq) = (
            w_lease.as_mut().expect("armed"),
            w_queue.as_mut().expect("armed"),
        );
        if log_left || !again.is_empty() {
            // More to push: another turn without waiting, but a queue
            // change is still reported.
            let (lease_rang, queue_rang) = (wl.now_or_never(), wq.now_or_never());
            if lease_rang.is_some() {
                w_lease = None;
            }
            if queue_rang.is_some() {
                w_queue = None;
                if !send(out, Frame::SvcReady { id }).await {
                    return Ok(());
                }
            }
            continue;
        }
        tokio::select! {
            _ = wl => w_lease = None,
            _ = wq => {
                w_queue = None;
                if !send(out, Frame::SvcReady { id }).await {
                    return Ok(());
                }
            }
        }
    }
}

/// What one connection's turn found.
enum Turn {
    /// No longer held under the lease by the caller.
    Gone,
    /// Frames to push, and whether input is left for another turn.
    Frames(Vec<Frame>, bool),
}

/// Read one connection's claim and new input, by snapshot.
#[allow(clippy::too_many_arguments)]
async fn conn_turn(
    st: &Shared,
    caller: &Caller,
    fs: u32,
    svc: &[u8],
    lease: &Id,
    conn: &Id,
    h: &mut Held,
    id: u32,
) -> ApiResult<Turn> {
    let max = st.cfg.limits.max_range_bytes;
    let items = st.cfg.limits.max_range_items as usize;
    let k = |tag| keys::conn(tag, fs, svc, conn);
    let (from_held, next_held) = (h.token, h.next);
    let (r, _) = txn_loop!(st.store, None, |t| {
        let Some(rm) = t
            .snapshot_get(&k("rm"))
            .await?
            .as_deref()
            .and_then(ClaimRec::decode)
        else {
            return Ok(None);
        };
        if !rm.is_claimed() || rm.lease != *lease || rm.holder != caller.device {
            return Ok(None);
        }
        let ra = Acks::decode(t.snapshot_get(&k("ra")).await?.as_deref());
        let (in_head, rh) = decode_head(t.snapshot_get(&k("rh")).await?.as_deref());
        // A new claim starts over at in_acked.
        let next = if rm.token == from_held {
            next_held
        } else {
            ra.in_acked
        };
        let from = next.max(ra.in_acked);
        let chunks = if from < in_head {
            chunks(&mut t, fs, svc, conn, from, max, items).await?
        } else {
            Vec::new()
        };
        Ok(Some((rm.token, ra, in_head, rh, chunks)))
    })?;
    let Some((token, ra, in_head, rh, chunks)) = r else {
        return Ok(Turn::Gone);
    };
    if token != h.token {
        *h = Held {
            token,
            next: ra.in_acked,
            eof_sent: false,
        };
    }
    let fl = flags(rh, ra.flags);
    let mut frames = Vec::with_capacity(chunks.len() + 1);
    for c in chunks {
        h.next = h.next.max(c.off + c.data.len() as u64);
        frames.push(Frame::SvcIn {
            id,
            conn: conn.to_vec(),
            off: c.off,
            data: c.data,
            sealed: c.sealed,
            flags: fl,
        });
    }
    let left = !frames.is_empty() && h.next < in_head;
    if rh & RH_EOF != 0 && h.next >= in_head && !h.eof_sent {
        h.eof_sent = true;
        frames.push(Frame::SvcIn {
            id,
            conn: conn.to_vec(),
            off: in_head,
            data: Vec::new(),
            sealed: false,
            flags: fl,
        });
    }
    Ok(Turn::Frames(frames, left))
}

/// Send a frame; false once the socket is gone.
async fn send(out: &mpsc::Sender<Frame>, f: Frame) -> bool {
    out.send(f).await.is_ok()
}

/// Whether the service's ready queue holds a connection.
async fn queued(st: &Shared, fs: u32, svc: &[u8]) -> ApiResult<bool> {
    let q = keys::svc_queue(fs, svc).finish();
    let (any, _) = txn_loop!(st.store, None, |t| {
        Ok(!t
            .snapshot_get_range(&q, &end_of(&q), 1, false)
            .await?
            .is_empty())
    })?;
    Ok(any)
}

/// The read version, and the log's newest key.
async fn log_tail(st: &Shared, log: &[u8]) -> ApiResult<(Version, Option<Vec<u8>>)> {
    let (r, _) = txn_loop!(st.store, None, |t| {
        let newest = t
            .snapshot_get_range(log, &end_of(log), 1, true)
            .await?
            .pop()
            .map(|(k, _)| k);
        Ok((t.read_version(), newest))
    })?;
    Ok(r)
}

/// Every connection claimed under the lease (`rx`).
async fn claimed(st: &Shared, fs: u32, svc: &[u8], lease: &Id) -> ApiResult<HashSet<Id>> {
    let p = keys::svc_claims(fs, svc, lease).finish();
    let mut begin = p.clone();
    let mut conns = HashSet::new();
    loop {
        let (page, _) = txn_loop!(st.store, None, |t| {
            Ok(t.snapshot_get_range(&begin, &end_of(&p), PAGE, false)
                .await?)
        })?;
        for (k, _) in &page {
            if let Some(c) = last_id(k) {
                conns.insert(c);
            }
        }
        match page.last() {
            Some((k, _)) if page.len() == PAGE => begin = key_after(k),
            _ => return Ok(conns),
        }
    }
}

/// The commit version of a log key's trailing versionstamp.
fn version_of(k: &[u8]) -> Option<Version> {
    let vs = k.get(k.len().checked_sub(12)?..)?;
    Some(u64::from_be_bytes(vs[..8].try_into().ok()?))
}
