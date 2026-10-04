//! `POST /v1/commit`: the single write path (api.md §6).

use crate::acl::{R_APPEND, R_READ, R_WRITE};
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::consume::consume_step;
use crate::error::*;
use crate::ids::{check_key_token, check_topic, encode_entry, offset};
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use std::collections::{BTreeMap, HashMap, HashSet};
use zen_core::kdf::RangeHasher;
use zen_proto::{
    ByteBuf, COMMIT_ID_LEN, Commit, CommitResult, CrdtOp, LogAppend, Mode, VERSION_LEN,
};
use zen_store::{STAMP_LEN, Txn, Version, stamp_of, version_of};

enum Outcome {
    Replay(Vec<u8>),
    Applied(u16, u16),
}

fn result(stamp: &[u8], appended: u16, writes: u16) -> CommitResult {
    let stamp: [u8; STAMP_LEN] = stamp[..STAMP_LEN].try_into().expect("10 bytes");
    let offsets = |n: u16| {
        (0..n)
            .map(|i| ByteBuf::from(offset(&stamp, i).to_vec()))
            .collect()
    };
    CommitResult {
        commit_version: version_of(&stamp),
        versionstamp: stamp.to_vec(),
        appended: offsets(appended),
        dots: offsets(writes),
    }
}

/// Decode an idempotency record for `device`: `stamp ‖ u16 appended ‖
/// device(32) ‖ [u16 writes]` (keyspace.md §3.6).
fn replay(rec: &[u8], device: &[u8; 32]) -> ApiResult<CommitResult> {
    let base = STAMP_LEN + 2 + 32;
    if rec.len() != base && rec.len() != base + 2 {
        return Err(internal("bad idempotency record"));
    }
    if &rec[STAMP_LEN + 2..base] != device {
        return Err(commit_id_reused("commit_id was used by another device"));
    }
    let n = u16::from_be_bytes(rec[STAMP_LEN..STAMP_LEN + 2].try_into().expect("2"));
    let w = rec
        .get(base..base + 2)
        .map_or(0, |b| u16::from_be_bytes(b.try_into().expect("2")));
    Ok(result(&rec[..STAMP_LEN], n, w))
}

/// Stateless checks: sizes, ids, permissions.
fn validate(st: &Shared, caller: &Caller, req: &Commit) -> ApiResult<()> {
    let l = &st.cfg.limits;
    // Besides its payload, every operation costs keys, index entries,
    // versionstamp operands and conflict ranges in the storage transaction;
    // this keeps a full commit under FoundationDB's 10 MB.
    const OP_OVERHEAD: usize = 512;
    if req.commit_id.len() != COMMIT_ID_LEN {
        return Err(bad_request("commit_id must be 16 bytes"));
    }
    let ops = req.chunks.len()
        + req.crdt_ops.len()
        + req.read_conflicts.len()
        + req.expect.len()
        + req.expect_ranges.len()
        + req.writes.len()
        + req.clear_ranges.len()
        + req.append.len()
        + req.consume.len();
    if ops > l.max_commit_ops as usize {
        return Err(too_large("too many operations"));
    }
    let mut bytes = ops * OP_OVERHEAD;
    let key_ok = |k: &[u8]| -> ApiResult<()> {
        if k.len() > l.max_key_bytes as usize {
            Err(too_large("key too long"))
        } else {
            Ok(())
        }
    };
    let fs_ok = |fs: u32, right: u8| -> ApiResult<()> {
        st.check_fs(fs)?;
        caller.require_fs(fs, right)
    };
    for r in &req.read_conflicts {
        fs_ok(r.fs, R_READ)?;
    }
    for e in &req.expect {
        fs_ok(e.fs, R_READ)?;
        key_ok(&e.key)?;
        if e.version.as_ref().is_some_and(|v| v.len() != VERSION_LEN) {
            return Err(bad_request("versions are 10 bytes"));
        }
    }
    for e in &req.expect_ranges {
        fs_ok(e.fs, R_READ)?;
        if e.hash.len() != 32 {
            return Err(bad_request("range hash must be 32 bytes"));
        }
    }
    for w in &req.writes {
        fs_ok(w.fs, R_WRITE)?;
        key_ok(&w.key)?;
        let n = w.value.as_ref().map_or(0, Vec::len);
        if n > l.max_value_bytes as usize {
            return Err(too_large("value too large"));
        }
        bytes += w.key.len() + n;
    }
    for c in &req.clear_ranges {
        fs_ok(c.fs, R_WRITE)?;
    }
    for a in &req.append {
        st.check_fs(a.fs)?;
        check_topic(&a.topic)?;
        check_key_token(a.key_token.as_deref())?;
        caller.require_topic(a.fs, &a.topic, R_APPEND)?;
        if a.envelope.len() > l.max_envelope_bytes as usize {
            return Err(too_large("envelope too large"));
        }
        bytes += a.envelope.len() + a.topic.len();
    }
    for c in &req.consume {
        st.check_fs(c.fs)?;
        check_key_token(c.key_token.as_deref())?;
        if c.from.len() != 12 || c.to.len() != 12 {
            return Err(bad_request("offsets are 12 bytes"));
        }
    }
    let value_ok = |n: usize| -> ApiResult<()> {
        if n > l.max_value_bytes as usize {
            Err(too_large("value too large"))
        } else {
            Ok(())
        }
    };
    for c in &req.chunks {
        fs_ok(c.fs, R_WRITE)?;
        value_ok(c.data.len())?;
        bytes += c.data.len() + c.id.len();
    }
    let mut written = HashSet::new();
    for op in &req.crdt_ops {
        let (fs, _) = op.target();
        fs_ok(fs, R_WRITE)?;
        match op {
            CrdtOp::Move { meta, .. } => {
                if meta.as_ref().is_some_and(Vec::is_empty) {
                    return Err(bad_request("meta must not be empty"));
                }
                value_ok(meta.as_ref().map_or(0, Vec::len))?;
                bytes += 64 + meta.as_ref().map_or(0, Vec::len);
            }
            CrdtOp::Meta { meta, .. } => {
                if meta.is_empty() {
                    return Err(bad_request("meta must not be empty"));
                }
                value_ok(meta.len())?;
                bytes += 48 + meta.len();
            }
            CrdtOp::Write {
                fs,
                tree,
                node,
                replaces,
                chunks,
                manifest,
            } => {
                value_ok(manifest.len() + 16 * chunks.len())?;
                // A second write to one node in a commit could not read
                // the first one's pending (versionstamped) version.
                if !written.insert((*fs, tree.clone(), node.clone())) {
                    return Err(bad_request("one write per node per commit"));
                }
                bytes += 48 + manifest.len() + 16 * chunks.len() + 12 * replaces.len();
            }
        }
    }
    if bytes > l.max_commit_bytes as usize {
        return Err(too_large("commit too large"));
    }
    Ok(())
}

/// A group on a topic that indexes appends: `per_key` (ready list), or
/// `partitioned` with its partition count (partition index).
#[derive(Clone)]
enum Indexing {
    PerKey(Vec<u8>),
    Partitioned(Vec<u8>, u32),
}

/// Indexing groups on a topic, read once per commit (conflict-tracked, so
/// a concurrent group creation and append serialize).
async fn topic_groups(
    t: &mut Box<dyn Txn>,
    cache: &mut HashMap<(u32, Vec<u8>), Vec<Indexing>>,
    fs: u32,
    topic: &[u8],
) -> ApiResult<Vec<Indexing>> {
    if let Some(g) = cache.get(&(fs, topic.to_vec())) {
        return Ok(g.clone());
    }
    let prefix = keys::topic_groups(fs, topic).finish();
    let groups: Vec<Indexing> =
        t.get_range(&prefix, &keys::end_of(&prefix), 10_000, false)
            .await?
            .into_iter()
            .filter_map(|(k, v)| {
                let (elems, _) = zen_store::tuple::unpack_prefix(&k[prefix.len()..], 1).ok()?;
                let Some(zen_store::tuple::Elem::Bytes(g)) = elems.into_iter().next() else {
                    return None;
                };
                match (v.first().copied(), v.get(1..5)) {
                    (Some(m), _) if m == Mode::PerKey.byte() => Some(Indexing::PerKey(g)),
                    // A `ct` entry without a partition count predates the index.
                    (Some(m), Some(n)) if m == Mode::Partitioned.byte() => Some(
                        Indexing::Partitioned(g, u32::from_be_bytes(n.try_into().ok()?)),
                    ),
                    _ => None,
                }
            })
            .collect();
    cache.insert((fs, topic.to_vec()), groups.clone());
    Ok(groups)
}

/// Run a commit. `must_clear` is an extra key that must exist and is
/// removed atomically (DLQ retry).
pub async fn execute(
    st: &Shared,
    caller: &Caller,
    req: Commit,
    must_clear: Option<Vec<u8>>,
) -> ApiResult<CommitResult> {
    validate(st, caller, &req)?;
    let cid_key = keys::commit_record(&req.commit_id);
    let limits = caller.acl.limits.clone();
    let max_range = st.cfg.limits.max_range_items as usize;
    let max_bytes = st.cfg.limits.max_range_bytes as usize;
    // Last write per key wins; clears apply first (api.md §6).
    let mut writes: BTreeMap<(u32, Vec<u8>), Option<Vec<u8>>> = BTreeMap::new();
    for w in &req.writes {
        writes.insert((w.fs, w.key.clone()), w.value.clone());
    }
    let res = txn_loop!(st.store, req.read_version, idempotent, |t| {
        if let Some(rec) = t.get(&cid_key).await? {
            return Ok(Outcome::Replay(rec));
        }
        if let Some(k) = &must_clear {
            if t.get(k).await?.is_none() {
                return Err(not_found("no such entry"));
            }
            t.clear(k);
        }
        for r in &req.read_conflicts {
            let (b, e) = keys::kv_range(r.fs, &r.begin, r.end.as_deref());
            t.add_read_conflict_range(&b, &e);
        }
        for e in &req.expect {
            let cur = t.get(&keys::kv(e.fs, &e.key)).await?;
            let cur_version = cur.as_ref().map(|v| &v[..VERSION_LEN]);
            if cur_version != e.version.as_deref() {
                return Err(conflict("expected version does not match"));
            }
        }
        for e in &req.expect_ranges {
            let (b, end) = keys::kv_range(e.fs, &e.begin, e.end.as_deref());
            let (got, capped) =
                crate::kv::read_capped(&mut t, &b, &end, max_range + 1, false, max_bytes, true)
                    .await?;
            if capped || got.len() > max_range {
                return Err(too_large("expect_ranges range too long"));
            }
            let prefix_len = keys::kv_prefix(e.fs).len();
            let mut h = RangeHasher::new();
            for (k, v) in &got {
                let version: &[u8; VERSION_LEN] = v[..VERSION_LEN].try_into().expect("10");
                h.update(&k[prefix_len..], version);
            }
            if h.finalize()[..] != e.hash[..] {
                return Err(conflict("range changed"));
            }
        }
        // (bytes, keys) deltas per fs.
        let mut delta: HashMap<u32, (i64, i64)> = HashMap::new();
        for c in &req.clear_ranges {
            let (b, e) = keys::kv_range(c.fs, &c.begin, c.end.as_deref());
            let (got, capped) =
                crate::kv::read_capped(&mut t, &b, &e, max_range + 1, false, max_bytes, false)
                    .await?;
            if capped || got.len() > max_range {
                return Err(too_large("clear range too long; clear in parts"));
            }
            let prefix_len = keys::kv_prefix(c.fs).len();
            let d = delta.entry(c.fs).or_default();
            for (k, v) in &got {
                d.0 -= (k.len() - prefix_len + v.len() - VERSION_LEN) as i64;
                d.1 -= 1;
            }
            t.clear_range(&b, &e);
        }
        for ((fs, key), value) in &writes {
            let sk = keys::kv(*fs, key);
            let d = delta.entry(*fs).or_default();
            if let Some(old) = t.snapshot_get(&sk).await? {
                d.0 -= (key.len() + old.len() - VERSION_LEN) as i64;
                d.1 -= 1;
            }
            match value {
                Some(v) => {
                    d.0 += (key.len() + v.len()) as i64;
                    d.1 += 1;
                    t.set_versionstamped_value(&sk, &[], v);
                }
                None => t.clear(&sk),
            }
        }
        for c in &req.consume {
            consume_step(&mut t, caller, c).await?;
        }
        let mut groups = HashMap::new();
        let mut readied: HashSet<(u32, Vec<u8>, Vec<u8>)> = HashSet::new();
        for (i, a) in req.append.iter().enumerate() {
            let i = i as u16;
            let ix = i.to_be_bytes();
            let (fs, topic) = (a.fs, &a.topic);
            let key = a.key_token.as_deref();
            let (p, s) = keys::log_prefix(fs, topic)
                .vs_incomplete(i)
                .finish_incomplete();
            t.set_versionstamped_key(&p, &s, &encode_entry(key, &a.envelope));
            if let Some(k) = key {
                let (p, s) = keys::lk_prefix(fs, topic, k)
                    .vs_incomplete(i)
                    .finish_incomplete();
                t.set_versionstamped_key(&p, &s, &[]);
            }
            let (p, s) = keys::gl_prefix(fs).vs_incomplete(i).finish_incomplete();
            t.set_versionstamped_key(&p, &s, topic);
            t.set_versionstamped_value(&keys::topic_head(fs, topic), &[], &ix);
            t.set_versionstamped_value(&keys::fs_head(fs), &[], &ix);
            delta.entry(fs).or_default().0 += (a.envelope.len() + topic.len()) as i64;
            for g in topic_groups(&mut t, &mut groups, fs, topic).await? {
                match (g, key) {
                    (Indexing::PerKey(g), Some(k)) => {
                        if !readied.insert((fs, g.clone(), k.to_vec())) {
                            continue;
                        }
                        let ptr = keys::ready_ptr(fs, &g, k);
                        if t.get(&ptr).await?.is_none() {
                            let (p, s) = keys::ready_prefix(fs, &g)
                                .vs_incomplete(i)
                                .bytes(k)
                                .finish_incomplete();
                            t.set_versionstamped_key(&p, &s, &[]);
                            t.set_versionstamped_value(&ptr, &[], &ix);
                        }
                    }
                    (Indexing::PerKey(_), None) => {}
                    (Indexing::Partitioned(g, n), key) => {
                        let part = crate::consume::partition_of(key, n);
                        let (p, s) = keys::partition_index(fs, &g, part)
                            .vs_incomplete(i)
                            .finish_incomplete();
                        t.set_versionstamped_key(&p, &s, &[]);
                    }
                }
            }
        }
        let mut eng = crate::tree::Engine::new(st, caller.device);
        for c in &req.chunks {
            eng.put_chunk(&mut t, c).await?;
        }
        for op in &req.crdt_ops {
            eng.apply(&mut t, op).await?;
        }
        eng.flush(&mut t)?;
        for (fs, (b, k)) in &eng.delta {
            let d = delta.entry(*fs).or_default();
            d.0 += b;
            d.1 += k;
        }
        let writes = eng.writes;
        for (fs, (bytes, nkeys)) in &delta {
            if let Some(l) = limits.get(fs) {
                for (what, d, max) in [("bytes", *bytes, l.max_bytes), ("keys", *nkeys, l.max_keys)]
                {
                    if let (true, Some(max)) = (d > 0, max) {
                        let cur = t
                            .snapshot_get(&keys::quota(*fs, what))
                            .await?
                            .map(|v| i64::from_le_bytes(v.try_into().unwrap_or_default()))
                            .unwrap_or(0);
                        if cur.saturating_add(d) > max as i64 {
                            return Err(quota(format!("fs {fs} {what} quota exceeded")));
                        }
                    }
                }
            }
            if *bytes != 0 {
                t.atomic_add(&keys::quota(*fs, "bytes"), *bytes);
            }
            if *nkeys != 0 {
                t.atomic_add(&keys::quota(*fs, "keys"), *nkeys);
            }
        }
        let n = req.append.len() as u16;
        let mut rec = n.to_be_bytes().to_vec();
        rec.extend_from_slice(&caller.device);
        rec.extend_from_slice(&writes.to_be_bytes());
        t.set_versionstamped_value(&cid_key, &[], &rec);
        let (p, s) = keys::commit_index()
            .vs_incomplete(0)
            .bytes(&req.commit_id)
            .finish_incomplete();
        t.set_versionstamped_key(&p, &s, &[]);
        Ok(Outcome::Applied(n, writes))
    });
    match res {
        Ok((Outcome::Replay(rec), _)) => replay(&rec, &caller.device),
        Ok((Outcome::Applied(n, w), stamp)) => Ok(result(&stamp, n, w)),
        Err(e) if matches!(e.code, "conflict" | "too_old" | "commit_unknown") => {
            // The commit may have landed under an earlier attempt (resend
            // after an unknown result): answer from the record if so.
            match lookup(st, &cid_key).await? {
                Some(rec) => replay(&rec, &caller.device),
                None => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

async fn lookup(st: &Shared, cid_key: &[u8]) -> ApiResult<Option<Vec<u8>>> {
    let mut t = st.store.begin(None).await?;
    Ok(t.get(cid_key).await?)
}

/// `POST /v1/commit`.
pub async fn commit(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<Commit>,
) -> ApiResult<Cbor<CommitResult>> {
    execute(&st, &caller, req, None).await.map(Cbor)
}

/// `POST /v1/log/append`.
pub async fn append(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<LogAppend>,
) -> ApiResult<Cbor<CommitResult>> {
    let c = Commit {
        commit_id: req.commit_id,
        append: req.append,
        ..Default::default()
    };
    execute(&st, &caller, c, None).await.map(Cbor)
}

/// Expire idempotency records older than the TTL (G13). Returns the number
/// removed.
pub async fn sweep(st: &Shared, now: Version) -> ApiResult<usize> {
    let ttl = st.cfg.limits.idempotency_ttl_secs * zen_store::VERSIONS_PER_SEC;
    let cutoff = now.saturating_sub(ttl);
    let prefix = keys::commit_index().finish();
    let end = keys::commit_index()
        .vs(&crate::ids::offset(&stamp_of(cutoff), 0))
        .finish();
    let mut removed = 0;
    // Pages of 1,000, up to 100 per pass: a busy server expires records
    // faster than one page a minute.
    for _ in 0..100 {
        let (n, _) = txn_loop!(st.store, None, |t| {
            let old = t.snapshot_get_range(&prefix, &end, 1000, false).await?;
            for (k, _) in &old {
                let (elems, _) = zen_store::tuple::unpack_prefix(&k[prefix.len()..], 2)
                    .map_err(|_| internal("bad commit index key"))?;
                if let Some(zen_store::tuple::Elem::Bytes(cid)) = elems.get(1) {
                    t.clear(&keys::commit_record(cid));
                }
                t.clear(k);
            }
            Ok(old.len())
        })?;
        removed += n;
        if n < 1000 {
            break;
        }
    }
    Ok(removed)
}
