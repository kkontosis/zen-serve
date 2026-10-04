//! Consumer groups (api.md §8, DESIGN-4 §1): definitions, leases, per-key
//! claims, delivery, the consume step, nack and the DLQ.

use crate::acl::R_CONSUME;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::{Offset, check_group, check_key_token, check_topic, decode_entry, parse_offset};
use crate::keys::{self, Sub};
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zen_proto::{
    ByteBuf, Commit, CommitResult, Consume, Cursor, Deliveries, Delivery, DlqItem, DlqItems,
    DlqList, DlqOp, Empty, GroupCreated, GroupDef, GroupRef, Lease, LeaseRelease, LeaseRequest,
    Mode, Nack, NackResult, NextRequest, OnPoison, Start, ZERO_OFFSET, from_cbor, to_cbor,
};
use zen_store::tuple::{Elem, unpack_prefix};
use zen_store::{Txn, VERSIONS_PER_SEC};

const DEFAULT_LEASE_MS: u32 = 10_000;
const MAX_WAIT_MS: u32 = 30_000;
const MAX_BACKFILL: usize = 100_000;

/// A stored group: the normalized definition and its start offset.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredGroup {
    /// Definition, defaults filled in.
    pub def: GroupDef,
    /// Cursor of a partition or key that never committed.
    pub start: ByteBuf,
}

impl StoredGroup {
    fn start(&self) -> Offset {
        self.start.as_slice().try_into().unwrap_or(ZERO_OFFSET)
    }

    fn max_attempts(&self) -> u32 {
        self.def.max_attempts.unwrap_or(5)
    }
}

fn ms_to_versions(ms: u32) -> u64 {
    u64::from(ms) * (VERSIONS_PER_SEC / 1000)
}

/// Offset in the trailing versionstamp element of a log-like key.
fn tail_offset(key: &[u8]) -> ApiResult<Offset> {
    key.len()
        .checked_sub(12)
        .map(|at| key[at..].try_into().expect("12 bytes"))
        .ok_or_else(|| internal("short key"))
}

/// Partition of an event.
pub fn partition_of(key_token: Option<&[u8]>, n: u32) -> u32 {
    match key_token {
        Some(k) if n > 1 => {
            let v = u128::from_be_bytes(k.try_into().unwrap_or([0; 16]));
            (v % u128::from(n)) as u32
        }
        _ => 0,
    }
}

/// Normalize and validate a definition.
fn normalize(mut d: GroupDef) -> ApiResult<GroupDef> {
    check_group(&d.group)?;
    check_topic(&d.topic)?;
    check_key_token(d.key_token.as_deref())?;
    match d.mode {
        Mode::Partitioned => match d.partitions {
            Some(1..=256) => {}
            _ => return Err(bad_request("partitioned groups need 1..=256 partitions")),
        },
        _ if d.partitions.is_some() => {
            return Err(bad_request("partitions apply to partitioned groups only"));
        }
        _ => {}
    }
    match d.mode {
        Mode::SingleKey if d.key_token.is_none() => {
            return Err(bad_request("single_key groups need key_token"));
        }
        Mode::SingleKey => {}
        _ if d.key_token.is_some() => {
            return Err(bad_request("key_token applies to single_key groups only"));
        }
        _ => {}
    }
    d.max_inflight = Some(d.max_inflight.unwrap_or(1).clamp(1, 1000));
    d.max_attempts = Some(d.max_attempts.unwrap_or(5));
    d.on_poison = Some(d.on_poison.unwrap_or(OnPoison::Dlq));
    d.start = Some(d.start.unwrap_or(Start::Earliest));
    Ok(d)
}

/// Load a group (conflict-tracked).
pub async fn load_group(t: &mut Box<dyn Txn>, fs: u32, group: &[u8]) -> ApiResult<StoredGroup> {
    let v = t
        .get(&keys::group(fs, group))
        .await?
        .ok_or_else(|| not_found("unknown consumer group"))?;
    from_cbor(&v).map_err(internal)
}

/// Resolve the partition or key a request addresses.
pub fn sub_for(g: &StoredGroup, partition: Option<u32>, key: Option<&[u8]>) -> ApiResult<Sub> {
    match g.def.mode {
        Mode::Broadcast => Err(bad_request("broadcast groups keep no server cursor")),
        Mode::Sequential | Mode::SingleKey => match (partition.unwrap_or(0), key) {
            (0, None) => Ok(Sub::Part(0)),
            _ => Err(bad_request("this group has a single cursor")),
        },
        Mode::Partitioned => match (partition, key) {
            (Some(p), None) if p < g.def.partitions.unwrap_or(1) => Ok(Sub::Part(p)),
            _ => Err(bad_request("partitioned groups need a valid partition")),
        },
        Mode::PerKey => match (partition, key) {
            (None, Some(k)) if k.len() == 16 => Ok(Sub::Key(k.to_vec())),
            _ => Err(bad_request("per_key groups need key_token")),
        },
    }
}

struct LeaseRec {
    holder: [u8; 32],
    token: u64,
    expires: u64,
}

fn decode_lease(v: &[u8]) -> Option<LeaseRec> {
    (v.len() >= 48).then(|| LeaseRec {
        holder: v[..32].try_into().expect("32"),
        token: u64::from_be_bytes(v[32..40].try_into().expect("8")),
        expires: u64::from_be_bytes(v[40..48].try_into().expect("8")),
    })
}

fn encode_lease(holder: &[u8; 32], token: u64, expires: u64, offset: Option<&Offset>) -> Vec<u8> {
    let mut v = holder.to_vec();
    v.extend_from_slice(&token.to_be_bytes());
    v.extend_from_slice(&expires.to_be_bytes());
    if let Some(o) = offset {
        v.extend_from_slice(o);
    }
    v
}

/// `(last committed offset, last claim token)` of a key.
async fn key_cursor(
    t: &mut Box<dyn Txn>,
    fs: u32,
    g: &StoredGroup,
    key: &[u8],
) -> ApiResult<(Offset, u64)> {
    Ok(
        match t.get(&keys::key_cursor(fs, &g.def.group, key)).await? {
            Some(v) if v.len() == 20 => (
                v[..12].try_into().expect("12"),
                u64::from_be_bytes(v[12..].try_into().expect("8")),
            ),
            _ => (g.start(), 0),
        },
    )
}

/// The committed cursor of a partition or key.
pub async fn cursor_of(
    t: &mut Box<dyn Txn>,
    fs: u32,
    g: &StoredGroup,
    sub: &Sub,
) -> ApiResult<Offset> {
    match sub {
        Sub::Part(p) => Ok(match t.get(&keys::cursor(fs, &g.def.group, *p)).await? {
            Some(v) => parse_offset(Some(&v))?,
            None => g.start(),
        }),
        Sub::Key(k) => Ok(key_cursor(t, fs, g, k).await?.0),
    }
}

async fn range(
    t: &mut Box<dyn Txn>,
    b: &[u8],
    e: &[u8],
    limit: usize,
    track: bool,
) -> ApiResult<Vec<(Vec<u8>, Vec<u8>)>> {
    Ok(if track {
        t.get_range(b, e, limit, false).await?
    } else {
        t.snapshot_get_range(b, e, limit, false).await?
    })
}

/// The next event of `sub` after `after`: `(offset, event entry)`.
pub async fn next_eligible(
    t: &mut Box<dyn Txn>,
    fs: u32,
    g: &StoredGroup,
    sub: &Sub,
    after: &Offset,
    track: bool,
) -> ApiResult<Option<(Offset, Vec<u8>)>> {
    let topic = &g.def.topic;
    let via_index = |key: &[u8]| keys::lk_prefix(fs, topic, key);
    match (g.def.mode, sub) {
        (Mode::Sequential, _) => {
            let (b, e) = keys::after_offset(keys::log_prefix(fs, topic), after);
            let got = range(t, &b, &e, 1, track).await?;
            got.into_iter()
                .next()
                .map(|(k, v)| Ok((tail_offset(&k)?, v)))
                .transpose()
        }
        (Mode::SingleKey, _) | (Mode::PerKey, Sub::Key(_)) => {
            let key = match sub {
                Sub::Key(k) => k.clone(),
                Sub::Part(_) => g.def.key_token.clone().unwrap_or_default(),
            };
            let (b, e) = keys::after_offset(via_index(&key), after);
            let Some((k, _)) = range(t, &b, &e, 1, track).await?.into_iter().next() else {
                return Ok(None);
            };
            let o = tail_offset(&k)?;
            let entry = t
                .snapshot_get(&keys::log_prefix(fs, topic).vs(&o).finish())
                .await?
                .ok_or_else(|| internal("per-key index points at a missing event"))?;
            Ok(Some((o, entry)))
        }
        (Mode::Partitioned, Sub::Part(p)) => {
            let n = g.def.partitions.unwrap_or(1);
            let mut from = *after;
            loop {
                let (b, e) = keys::after_offset(keys::log_prefix(fs, topic), &from);
                let got = range(t, &b, &e, 256, track).await?;
                let done = got.len() < 256;
                for (k, v) in got {
                    let o = tail_offset(&k)?;
                    let (key, _) = decode_entry(&v);
                    if partition_of(key.as_deref(), n) == *p {
                        return Ok(Some((o, v)));
                    }
                    from = o;
                }
                if done {
                    return Ok(None);
                }
            }
        }
        _ => Err(bad_request("invalid partition or key for this group")),
    }
}

/// Check the fencing token: the lease (lease modes) or the claim (`per_key`).
async fn check_token(
    t: &mut Box<dyn Txn>,
    fs: u32,
    g: &StoredGroup,
    sub: &Sub,
    token: u64,
) -> ApiResult<()> {
    match sub {
        Sub::Part(p) => match t
            .get(&keys::lease(fs, &g.def.group, *p))
            .await?
            .as_deref()
            .and_then(decode_lease)
        {
            Some(l) if l.token == token => Ok(()),
            _ => Err(not_leader("lease token is not current")),
        },
        Sub::Key(k) => match t
            .get(&keys::claim(fs, &g.def.group, k))
            .await?
            .as_deref()
            .and_then(decode_lease)
        {
            Some(c) if c.token == token => Ok(()),
            _ => Err(claim_lost("claim token is not current")),
        },
    }
}

/// Move a cursor to `to` (consume step / dead-lettering).
async fn advance(
    t: &mut Box<dyn Txn>,
    fs: u32,
    g: &StoredGroup,
    sub: &Sub,
    to: &Offset,
) -> ApiResult<()> {
    let group = &g.def.group;
    t.clear(&keys::attempts(fs, group, sub));
    match sub {
        Sub::Part(p) => t.set(&keys::cursor(fs, group, *p), to),
        Sub::Key(k) => {
            let (_, last_token) = key_cursor(t, fs, g, k).await?;
            let mut kc = to.to_vec();
            kc.extend_from_slice(&last_token.to_be_bytes());
            t.set(&keys::key_cursor(fs, group, k), &kc);
            let ptr = keys::ready_ptr(fs, group, k);
            if let Some(o) = t.get(&ptr).await? {
                let o = parse_offset(Some(&o))?;
                t.clear(&keys::ready_prefix(fs, group).vs(&o).bytes(k).finish());
            }
            // Conflict-tracked, so a concurrent append to this key retries.
            match next_eligible(t, fs, g, sub, to, true).await? {
                Some((next, _)) => {
                    t.set(
                        &keys::ready_prefix(fs, group).vs(&next).bytes(k).finish(),
                        &[],
                    );
                    t.set(&ptr, &next);
                }
                None => t.clear(&ptr),
            }
            t.clear(&keys::claim(fs, group, k));
        }
    }
    Ok(())
}

/// The consume step inside a commit (api.md §8.3).
pub async fn consume_step(t: &mut Box<dyn Txn>, caller: &Caller, c: &Consume) -> ApiResult<()> {
    let g = load_group(t, c.fs, &c.group).await?;
    caller.require_topic(c.fs, &g.def.topic, R_CONSUME)?;
    let sub = sub_for(&g, c.partition, c.key_token.as_deref())?;
    let from = parse_offset(Some(&c.from))?;
    let to = parse_offset(Some(&c.to))?;
    check_token(t, c.fs, &g, &sub, c.token).await?;
    if cursor_of(t, c.fs, &g, &sub).await? != from {
        return Err(cursor_moved("cursor is not at `from`"));
    }
    match next_eligible(t, c.fs, &g, &sub, &from, false).await? {
        Some((o, _)) if o == to => {}
        _ => return Err(cursor_moved("`to` is not the next event after `from`")),
    }
    advance(t, c.fs, &g, &sub, &to).await
}

/// `POST /v1/consume/groups`.
pub async fn create_group(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<GroupDef>,
) -> ApiResult<Cbor<GroupCreated>> {
    st.check_fs(req.fs)?;
    let def = normalize(req)?;
    caller.require_topic(def.fs, &def.topic, R_CONSUME)?;
    let fs = def.fs;
    let (created, _) = txn_loop!(st.store, None, |t| {
        let key = keys::group(fs, &def.group);
        if let Some(v) = t.get(&key).await? {
            let existing: StoredGroup = from_cbor(&v).map_err(internal)?;
            return if existing.def == def {
                Ok(false)
            } else {
                Err(group_exists("a different group with this name exists"))
            };
        }
        let log = keys::log_prefix(fs, &def.topic).finish();
        let log_end = keys::end_of(&log);
        let start = match def.start {
            Some(Start::Latest) => match t.get_range(&log, &log_end, 1, true).await?.pop() {
                Some((k, _)) => tail_offset(&k)?,
                None => ZERO_OFFSET,
            },
            _ => ZERO_OFFSET,
        };
        if def.mode == Mode::PerKey && start == ZERO_OFFSET {
            // Backfill: each key's first event goes on the ready list.
            let all = t.get_range(&log, &log_end, MAX_BACKFILL + 1, false).await?;
            if all.len() > MAX_BACKFILL {
                return Err(too_large("topic too long to backfill; use start = latest"));
            }
            let mut seen = std::collections::HashSet::new();
            for (k, v) in all {
                if let (Some(key), _) = decode_entry(&v)
                    && seen.insert(key.clone())
                {
                    let o = tail_offset(&k)?;
                    t.set(
                        &keys::ready_prefix(fs, &def.group)
                            .vs(&o)
                            .bytes(&key)
                            .finish(),
                        &[],
                    );
                    t.set(&keys::ready_ptr(fs, &def.group, &key), &o);
                }
            }
        }
        let stored = StoredGroup {
            def: def.clone(),
            start: ByteBuf::from(start.to_vec()),
        };
        t.set(&key, &to_cbor(&stored));
        t.set(
            &keys::topic_groups(fs, &def.topic)
                .bytes(&def.group)
                .finish(),
            &[def.mode.byte()],
        );
        Ok(true)
    })?;
    Ok(Cbor(GroupCreated { created }))
}

/// `POST /v1/consume/lease`.
pub async fn lease(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<LeaseRequest>,
) -> ApiResult<Cbor<Lease>> {
    st.check_fs(req.fs)?;
    let ttl = ms_to_versions(req.ttl_ms.unwrap_or(DEFAULT_LEASE_MS).clamp(100, 600_000));
    let (lease, _) = txn_loop!(st.store, None, |t| {
        let g = load_group(&mut t, req.fs, &req.group).await?;
        caller.require_topic(req.fs, &g.def.topic, R_CONSUME)?;
        let sub = sub_for(&g, req.partition, None)?;
        if g.def.mode == Mode::PerKey {
            return Err(bad_request("per_key groups use claims, not leases"));
        }
        let Sub::Part(p) = sub else {
            return Err(bad_request("lease needs a partition"));
        };
        let key = keys::lease(req.fs, &g.def.group, p);
        let now = st.store.now_version();
        let cur = t.get(&key).await?.as_deref().and_then(decode_lease);
        let token = match (&cur, req.token) {
            (Some(l), Some(tok))
                if l.holder == caller.device && l.token == tok && l.expires > now =>
            {
                tok
            }
            (Some(l), _) if l.expires > now => return Err(not_leader("the lease is held")),
            (Some(l), _) => l.token + 1,
            (None, _) => 1,
        };
        let expires = now + ttl;
        t.set(&key, &encode_lease(&caller.device, token, expires, None));
        let cursor = cursor_of(&mut t, req.fs, &g, &sub).await?;
        Ok(Lease {
            token,
            expires_version: expires,
            cursor: cursor.to_vec(),
        })
    })?;
    Ok(Cbor(lease))
}

/// `POST /v1/consume/release`.
pub async fn release(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<LeaseRelease>,
) -> ApiResult<Cbor<Empty>> {
    st.check_fs(req.fs)?;
    txn_loop!(st.store, None, |t| {
        let g = load_group(&mut t, req.fs, &req.group).await?;
        caller.require_topic(req.fs, &g.def.topic, R_CONSUME)?;
        let Sub::Part(p) = sub_for(&g, req.partition, None)? else {
            return Err(bad_request("release needs a partition"));
        };
        let key = keys::lease(req.fs, &g.def.group, p);
        match t.get(&key).await?.as_deref().and_then(decode_lease) {
            Some(l) if l.holder == caller.device && l.token == req.token => {
                t.set(&key, &encode_lease(&l.holder, l.token, 0, None));
                Ok(())
            }
            _ => Err(not_leader("not the lease holder")),
        }
    })?;
    Ok(Cbor(Empty {}))
}

async fn attempts_of(
    t: &mut Box<dyn Txn>,
    fs: u32,
    group: &[u8],
    sub: &Sub,
    offset: &Offset,
) -> ApiResult<u32> {
    Ok(
        match t.snapshot_get(&keys::attempts(fs, group, sub)).await? {
            Some(v) if v.len() == 16 && v[..12] == offset[..] => {
                u32::from_be_bytes(v[12..].try_into().expect("4"))
            }
            _ => 0,
        },
    )
}

/// One delivery attempt; empty if nothing is ready.
async fn deliver(
    st: &Shared,
    caller: &Caller,
    req: &NextRequest,
) -> ApiResult<(Vec<Delivery>, Vec<u8>)> {
    let (out, _) = txn_loop!(st.store, None, |t| {
        let fs = req.fs;
        let g = load_group(&mut t, fs, &req.group).await?;
        caller.require_topic(fs, &g.def.topic, R_CONSUME)?;
        let head = keys::topic_head(fs, &g.def.topic);
        let limit = req.limit.unwrap_or(1).clamp(1, 1000);
        let now = st.store.now_version();
        let mut out = Vec::new();
        if g.def.mode == Mode::PerKey {
            if req.partition.is_some() {
                return Err(bad_request("per_key groups have no partitions"));
            }
            let ttl = ms_to_versions(st.cfg.limits.claim_ttl_ms);
            let prefix = keys::ready_prefix(fs, &g.def.group).finish();
            let end = keys::end_of(&prefix);
            let ready = t
                .snapshot_get_range(&prefix, &end, (limit as usize) * 4 + 16, false)
                .await?;
            for (k, _) in ready {
                let (elems, _) =
                    unpack_prefix(&k[prefix.len()..], 2).map_err(|_| internal("bad ready key"))?;
                let [Elem::Vs(o), Elem::Bytes(key)] = elems.as_slice() else {
                    return Err(internal("bad ready key"));
                };
                let claim_key = keys::claim(fs, &g.def.group, key);
                let claim = t.get(&claim_key).await?;
                if claim
                    .as_deref()
                    .and_then(decode_lease)
                    .is_some_and(|c| c.expires > now)
                {
                    continue;
                }
                let (from, last_token) = key_cursor(&mut t, fs, &g, key).await?;
                let token = last_token + 1;
                let mut kc = from.to_vec();
                kc.extend_from_slice(&token.to_be_bytes());
                t.set(&keys::key_cursor(fs, &g.def.group, key), &kc);
                t.set(
                    &claim_key,
                    &encode_lease(&caller.device, token, now + ttl, Some(o)),
                );
                let entry = t
                    .snapshot_get(&keys::log_prefix(fs, &g.def.topic).vs(o).finish())
                    .await?
                    .ok_or_else(|| internal("ready list points at a missing event"))?;
                let (key_token, envelope) = decode_entry(&entry);
                let sub = Sub::Key(key.clone());
                out.push(Delivery {
                    offset: o.to_vec(),
                    key_token,
                    envelope,
                    from: from.to_vec(),
                    token,
                    attempts: attempts_of(&mut t, fs, &g.def.group, &sub, o).await?,
                });
                if out.len() as u32 >= limit {
                    break;
                }
            }
        } else {
            let sub = sub_for(&g, req.partition, None)?;
            let Sub::Part(p) = sub else {
                return Err(bad_request("next needs a partition"));
            };
            let tok = req
                .token
                .ok_or_else(|| bad_request("lease token required"))?;
            match t
                .snapshot_get(&keys::lease(fs, &g.def.group, p))
                .await?
                .as_deref()
                .and_then(decode_lease)
            {
                Some(l) if l.token == tok && l.holder == caller.device && l.expires > now => {}
                _ => return Err(not_leader("lease token is not current")),
            }
            let n = limit.min(g.def.max_inflight.unwrap_or(1));
            let mut from = cursor_of(&mut t, fs, &g, &sub).await?;
            for _ in 0..n {
                let Some((o, entry)) = next_eligible(&mut t, fs, &g, &sub, &from, false).await?
                else {
                    break;
                };
                let (key_token, envelope) = decode_entry(&entry);
                out.push(Delivery {
                    offset: o.to_vec(),
                    key_token,
                    envelope,
                    from: from.to_vec(),
                    token: tok,
                    attempts: attempts_of(&mut t, fs, &g.def.group, &sub, &o).await?,
                });
                from = o;
            }
        }
        Ok((out, head))
    })?;
    Ok(out)
}

/// `POST /v1/consume/next`.
pub async fn next(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<NextRequest>,
) -> ApiResult<Cbor<Deliveries>> {
    st.check_fs(req.fs)?;
    let wait = Duration::from_millis(req.wait_ms.unwrap_or(0).min(MAX_WAIT_MS).into());
    let deadline = Instant::now() + wait;
    let mut watch_key: Option<Vec<u8>> = None;
    loop {
        // Register the watch before reading, so no append is missed.
        let w = watch_key.as_deref().map(|k| st.store.watch(k));
        let (events, head) = deliver(&st, &caller, &req).await?;
        let now = Instant::now();
        if !events.is_empty() || now >= deadline {
            return Ok(Cbor(Deliveries { events }));
        }
        match w {
            Some(w) => {
                let _ = tokio::time::timeout(deadline - now, w).await;
            }
            None => watch_key = Some(head),
        }
    }
}

/// `POST /v1/consume/nack`.
pub async fn nack(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<Nack>,
) -> ApiResult<Cbor<NackResult>> {
    st.check_fs(req.fs)?;
    check_key_token(req.key_token.as_deref())?;
    let offset = parse_offset(Some(&req.offset))?;
    let (res, _) = txn_loop!(st.store, None, |t| {
        let fs = req.fs;
        let g = load_group(&mut t, fs, &req.group).await?;
        caller.require_topic(fs, &g.def.topic, R_CONSUME)?;
        let sub = sub_for(&g, req.partition, req.key_token.as_deref())?;
        check_token(&mut t, fs, &g, &sub, req.token).await?;
        let cur = cursor_of(&mut t, fs, &g, &sub).await?;
        let Some((o, entry)) = next_eligible(&mut t, fs, &g, &sub, &cur, false).await? else {
            return Err(cursor_moved("no pending event"));
        };
        if o != offset {
            return Err(cursor_moved("offset is not the pending event"));
        }
        let ca = keys::attempts(fs, &g.def.group, &sub);
        let count = match t.get(&ca).await? {
            Some(v) if v.len() == 16 && v[..12] == offset[..] => {
                u32::from_be_bytes(v[12..].try_into().expect("4")) + 1
            }
            _ => 1,
        };
        let max = g.max_attempts();
        let dead = max > 0 && count >= max && g.def.on_poison != Some(OnPoison::Block);
        if dead {
            let (p, s) = keys::dlq_prefix(fs, &g.def.group)
                .vs_incomplete(0)
                .finish_incomplete();
            let mut v = offset.to_vec();
            v.extend_from_slice(&(g.def.topic.len() as u32).to_be_bytes());
            v.extend_from_slice(&g.def.topic);
            v.extend_from_slice(&entry);
            t.set_versionstamped_key(&p, &s, &v);
            advance(&mut t, fs, &g, &sub, &offset).await?;
        } else {
            let mut v = offset.to_vec();
            v.extend_from_slice(&count.to_be_bytes());
            t.set(&ca, &v);
            if let Sub::Key(k) = &sub {
                t.clear(&keys::claim(fs, &g.def.group, k));
            }
        }
        Ok(NackResult {
            attempts: count,
            dead_lettered: dead,
        })
    })?;
    Ok(Cbor(res))
}

/// `POST /v1/consume/cursor`.
pub async fn cursor(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<GroupRef>,
) -> ApiResult<Cbor<Cursor>> {
    st.check_fs(req.fs)?;
    let mut t = st.store.begin(None).await?;
    let g = load_group(&mut t, req.fs, &req.group).await?;
    caller.require_topic(req.fs, &g.def.topic, R_CONSUME)?;
    let mut low_watermark = None;
    let cursor = if g.def.mode == Mode::PerKey && req.key_token.is_none() {
        g.start()
    } else {
        let sub = sub_for(&g, req.partition, req.key_token.as_deref())?;
        cursor_of(&mut t, req.fs, &g, &sub).await?
    };
    if g.def.mode == Mode::PerKey {
        let prefix = keys::ready_prefix(req.fs, &g.def.group).finish();
        if let Some((k, _)) = t
            .snapshot_get_range(&prefix, &keys::end_of(&prefix), 1, false)
            .await?
            .pop()
        {
            let (elems, _) =
                unpack_prefix(&k[prefix.len()..], 1).map_err(|_| internal("bad ready key"))?;
            if let Some(Elem::Vs(o)) = elems.first() {
                low_watermark = Some(o.to_vec());
            }
        }
    }
    Ok(Cbor(Cursor {
        cursor: cursor.to_vec(),
        low_watermark,
    }))
}

fn decode_dlq(id: Offset, v: &[u8]) -> ApiResult<DlqItem> {
    let bad = || internal("bad DLQ entry");
    let offset = v.get(..12).ok_or_else(bad)?.to_vec();
    let n = u32::from_be_bytes(v.get(12..16).ok_or_else(bad)?.try_into().expect("4")) as usize;
    let topic = v.get(16..16 + n).ok_or_else(bad)?.to_vec();
    let (key_token, envelope) = decode_entry(v.get(16 + n..).ok_or_else(bad)?);
    Ok(DlqItem {
        id: id.to_vec(),
        offset,
        topic,
        key_token,
        envelope,
    })
}

async fn dlq_group(st: &Shared, caller: &Caller, fs: u32, group: &[u8]) -> ApiResult<StoredGroup> {
    st.check_fs(fs)?;
    let mut t = st.store.begin(None).await?;
    let g = load_group(&mut t, fs, group).await?;
    caller.require_topic(fs, &g.def.topic, R_CONSUME)?;
    Ok(g)
}

/// `POST /v1/consume/dlq/list`.
pub async fn dlq_list(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<DlqList>,
) -> ApiResult<Cbor<DlqItems>> {
    dlq_group(&st, &caller, req.fs, &req.group).await?;
    let after = parse_offset(req.after.as_deref())?;
    let limit = req
        .limit
        .unwrap_or(100)
        .clamp(1, st.cfg.limits.max_range_items) as usize;
    let (b, e) = keys::after_offset(keys::dlq_prefix(req.fs, &req.group), &after);
    let mut t = st.store.begin(None).await?;
    let items = t
        .snapshot_get_range(&b, &e, limit, false)
        .await?
        .into_iter()
        .map(|(k, v)| decode_dlq(tail_offset(&k)?, &v))
        .collect::<ApiResult<_>>()?;
    Ok(Cbor(DlqItems { items }))
}

/// `POST /v1/consume/dlq/retry`: re-append the envelope to its topic and
/// remove the entry, atomically.
pub async fn dlq_retry(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<DlqOp>,
) -> ApiResult<Cbor<CommitResult>> {
    dlq_group(&st, &caller, req.fs, &req.group).await?;
    let id = parse_offset(Some(&req.id))?;
    let key = keys::dlq_prefix(req.fs, &req.group).vs(&id).finish();
    let commit_id = req
        .commit_id
        .clone()
        .ok_or_else(|| bad_request("commit_id required"))?;
    let entry = {
        let mut t = st.store.begin(None).await?;
        t.get(&key).await?
    };
    let commit = match entry {
        Some(v) => {
            let item = decode_dlq(id, &v)?;
            Commit {
                commit_id,
                append: vec![zen_proto::Append {
                    fs: req.fs,
                    topic: item.topic,
                    key_token: item.key_token,
                    envelope: item.envelope,
                }],
                ..Default::default()
            }
        }
        // Already retried: let the idempotency record answer.
        None => Commit {
            commit_id,
            ..Default::default()
        },
    };
    crate::commit::execute(&st, &caller, commit, Some(key))
        .await
        .map(Cbor)
}

/// `POST /v1/consume/dlq/drop`.
pub async fn dlq_drop(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<DlqOp>,
) -> ApiResult<Cbor<Empty>> {
    dlq_group(&st, &caller, req.fs, &req.group).await?;
    let id = parse_offset(Some(&req.id))?;
    let key = keys::dlq_prefix(req.fs, &req.group).vs(&id).finish();
    txn_loop!(st.store, None, |t| {
        t.clear(&key);
        Ok(())
    })?;
    Ok(Cbor(Empty {}))
}
