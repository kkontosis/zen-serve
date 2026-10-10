//! Server-merged CRDT rows (api.md §13, spec/zendb.md §19):
//! * [`RowEngine`] applies `row`, `lww`, `ctr`, `add` and `rem` inside the
//!   `/v1/commit` transaction.
//! * Read endpoints `/v1/crdt/get` and `/v1/crdt/range`.
//! * [`sweep`]: purges dead and register-less objects past the horizon.
//!
//! Storage is keyspace.md §3.8. Values are opaque sealed bytes; the server
//! merges by timestamps, sequence numbers and dots only.

use crate::acl::R_READ;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::tree::{Ts, check_clock, id16, unix_ms};
use crate::txn::txn_loop;
use axum::extract::State;
use std::collections::{BTreeSet, HashMap};
use zen_proto::{
    ByteBuf, CrdtGet, CrdtOp, CrdtRange, CtrEntry, LwwReg, ObjState, ObjStates, RowReg, SetDot,
};
use zen_store::tuple::{Elem, unpack_prefix};
use zen_store::{STAMP_LEN, Txn, VERSIONS_PER_SEC, Version, key_after, stamp_of};

/// Whether `op` is a CRDT-row operation (not a filesystem one).
pub fn is_row_op(op: &CrdtOp) -> bool {
    op.tree().is_none()
}

/// Validate an object id: 16·n bytes, at most `max_key_bytes`.
pub fn check_object(st: &Shared, object: &[u8]) -> ApiResult<()> {
    if object.is_empty() || !object.len().is_multiple_of(16) {
        return Err(bad_request(
            "object must be a non-empty multiple of 16 bytes",
        ));
    }
    if object.len() > st.cfg.limits.max_key_bytes as usize {
        return Err(bad_request("object too long"));
    }
    Ok(())
}

/// Stateless checks of one row operation; returns its byte cost.
pub fn validate(st: &Shared, op: &CrdtOp) -> ApiResult<usize> {
    let max = st.cfg.limits.max_value_bytes as usize;
    let value_ok = |v: &[u8]| -> ApiResult<usize> {
        if v.len() > max {
            Err(bad_request("value too large"))
        } else {
            Ok(v.len())
        }
    };
    let id_ok = |b: &[u8], what: &str| id16(b, what).map(|_| ());
    Ok(match op {
        CrdtOp::Row { object, value, .. } => {
            check_object(st, object)?;
            object.len() + value_ok(value)?
        }
        CrdtOp::Lww {
            object,
            field,
            value,
            ..
        } => {
            check_object(st, object)?;
            id_ok(field, "field")?;
            object.len() + 16 + value_ok(value.as_deref().unwrap_or(&[]))?
        }
        CrdtOp::Ctr {
            object,
            field,
            value,
            ..
        } => {
            check_object(st, object)?;
            id_ok(field, "field")?;
            object.len() + 16 + value_ok(value)?
        }
        CrdtOp::Add {
            object,
            field,
            elem,
            value,
            ..
        } => {
            check_object(st, object)?;
            id_ok(field, "field")?;
            id_ok(elem, "elem")?;
            object.len() + 32 + value_ok(value)?
        }
        CrdtOp::Rem {
            object,
            field,
            elem,
            dots,
            ..
        } => {
            check_object(st, object)?;
            id_ok(field, "field")?;
            id_ok(elem, "elem")?;
            if dots.iter().any(|d| d.len() != 12) {
                return Err(bad_request("dots are 12 bytes"));
            }
            object.len() + 32 + 12 * dots.len()
        }
        _ => 0,
    })
}

fn ts_of(v: &[u8]) -> ApiResult<Ts> {
    if v.len() < 40 {
        return Err(internal("bad CRDT register"));
    }
    Ok(Ts {
        hlc: u64::from_be_bytes(v[..8].try_into().expect("8")),
        dev: v[8..40].try_into().expect("32"),
    })
}

fn register(hlc: u64, dev: &[u8; 32], flag: bool, value: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(41 + value.len());
    r.extend_from_slice(&hlc.to_be_bytes());
    r.extend_from_slice(dev);
    r.push(flag as u8);
    r.extend_from_slice(value);
    r
}

/// Applies one commit's CRDT-row operations.
pub struct RowEngine<'a> {
    st: &'a Shared,
    device: [u8; 32],
    now_ms: u64,
    /// Registers and counter entries read or written in this attempt.
    cache: HashMap<Vec<u8>, Option<Vec<u8>>>,
    changed: BTreeSet<(u32, Vec<u8>)>,
    /// `add`s so far (the next set dot's index).
    pub adds: u16,
    /// Quota deltas per fs: (bytes, keys).
    pub delta: HashMap<u32, (i64, i64)>,
}

impl<'a> RowEngine<'a> {
    /// A fresh engine for one transaction attempt.
    pub fn new(st: &'a Shared, device: [u8; 32]) -> Self {
        RowEngine {
            st,
            device,
            now_ms: unix_ms(),
            cache: HashMap::new(),
            changed: BTreeSet::new(),
            adds: 0,
            delta: HashMap::new(),
        }
    }

    async fn get(&mut self, t: &mut Box<dyn Txn>, key: &[u8]) -> ApiResult<Option<Vec<u8>>> {
        if let Some(v) = self.cache.get(key) {
            return Ok(v.clone());
        }
        let v = t.get(key).await?;
        self.cache.insert(key.to_vec(), v.clone());
        Ok(v)
    }

    /// Replace `key`'s value, counting the quota change.
    fn put(
        &mut self,
        t: &mut Box<dyn Txn>,
        fs: u32,
        key: Vec<u8>,
        old: Option<usize>,
        new: Vec<u8>,
    ) {
        let d = self.delta.entry(fs).or_default();
        d.0 += new.len() as i64 - old.unwrap_or(0) as i64;
        d.1 += i64::from(old.is_none());
        t.set(&key, &new);
        self.cache.insert(key, Some(new));
    }

    /// Last-writer-wins on a register at `key`: write if `(hlc, device)` is
    /// greater than the stored timestamp.
    async fn lww(
        &mut self,
        t: &mut Box<dyn Txn>,
        (fs, object): (u32, &[u8]),
        key: Vec<u8>,
        hlc: u64,
        (flag, value): (bool, &[u8]),
    ) -> ApiResult<()> {
        check_clock(self.st, self.now_ms, hlc)?;
        let cur = self.get(t, &key).await?;
        let ts = Ts {
            hlc,
            dev: self.device,
        };
        if let Some(c) = &cur
            && ts <= ts_of(c)?
        {
            return Ok(()); // older or equal: the stored one wins
        }
        let rec = register(hlc, &self.device, flag, value);
        self.put(t, fs, key, cur.map(|c| c.len()), rec);
        self.changed.insert((fs, object.to_vec()));
        Ok(())
    }

    /// Apply one CRDT-row operation (api.md §13.2).
    pub async fn apply(&mut self, t: &mut Box<dyn Txn>, op: &CrdtOp) -> ApiResult<()> {
        match op {
            CrdtOp::Row {
                fs,
                object,
                hlc,
                alive,
                value,
            } => {
                let key = keys::crdt_row(*fs, object);
                self.lww(t, (*fs, object), key, *hlc, (*alive, value)).await
            }
            CrdtOp::Lww {
                fs,
                object,
                field,
                hlc,
                value,
            } => {
                let key = keys::crdt_lww(*fs, object).bytes(field).finish();
                let v = value.as_deref().unwrap_or(&[]);
                self.lww(t, (*fs, object), key, *hlc, (value.is_some(), v))
                    .await
            }
            CrdtOp::Ctr {
                fs,
                object,
                field,
                seq,
                value,
            } => {
                let key = keys::crdt_ctr(*fs, object)
                    .bytes(field)
                    .bytes(&self.device)
                    .finish();
                let cur = self.get(t, &key).await?;
                if let Some(c) = &cur {
                    if c.len() < 8 {
                        return Err(internal("bad counter entry"));
                    }
                    if *seq <= u64::from_be_bytes(c[..8].try_into().expect("8")) {
                        return Err(stale_op("counter seq is not newer; re-read and reissue"));
                    }
                }
                let mut rec = seq.to_be_bytes().to_vec();
                rec.extend_from_slice(value);
                self.put(t, *fs, key, cur.map(|c| c.len()), rec);
                self.changed.insert((*fs, object.clone()));
                Ok(())
            }
            CrdtOp::Add {
                fs,
                object,
                field,
                elem,
                value,
            } => {
                let (p, s) = keys::crdt_set(*fs, object)
                    .bytes(field)
                    .bytes(elem)
                    .vs_incomplete(self.adds)
                    .finish_incomplete();
                let mut rec = self.device.to_vec();
                rec.extend_from_slice(value);
                let d = self.delta.entry(*fs).or_default();
                d.0 += rec.len() as i64;
                d.1 += 1;
                t.set_versionstamped_key(&p, &s, &rec);
                self.adds = self
                    .adds
                    .checked_add(1)
                    .ok_or_else(|| too_large("a commit may add at most 65535 set elements"))?;
                self.changed.insert((*fs, object.clone()));
                Ok(())
            }
            CrdtOp::Rem {
                fs,
                object,
                field,
                elem,
                dots,
            } => {
                for dot in dots {
                    let dot: [u8; 12] = dot[..].try_into().expect("validated");
                    let key = keys::crdt_set(*fs, object)
                        .bytes(field)
                        .bytes(elem)
                        .vs(&dot)
                        .finish();
                    if let Some(v) = t.get(&key).await? {
                        t.clear(&key);
                        let d = self.delta.entry(*fs).or_default();
                        d.0 -= v.len() as i64;
                        d.1 -= 1;
                        self.changed.insert((*fs, object.clone()));
                    }
                }
                Ok(())
            }
            _ => Err(internal("not a CRDT-row operation")),
        }
    }

    /// Stamp every changed object and keep its GC-candidate entry.
    pub async fn flush(&mut self, t: &mut Box<dyn Txn>) -> ApiResult<()> {
        for (fs, object) in std::mem::take(&mut self.changed) {
            let cv = keys::crdt_objects(fs).bytes(&object).finish();
            // A snapshot read: concurrent commits on one object don't
            // conflict here. The last one's stamp and entry win; the
            // sweeper drops entries whose stamp is not the object's.
            match t.snapshot_get(&cv).await? {
                Some(old) if old.len() == STAMP_LEN => {
                    let mut vs = [0u8; 12];
                    vs[..STAMP_LEN].copy_from_slice(&old);
                    t.clear(&keys::crdt_gc(fs).vs(&vs).bytes(&object).finish());
                }
                Some(_) => return Err(internal("bad CRDT object stamp")),
                None => self.delta.entry(fs).or_default().1 += 1,
            }
            // Conflict-tracked: whether the object is a candidate serializes
            // with concurrent row operations.
            let reg = self.get(t, &keys::crdt_row(fs, &object)).await?;
            let candidate = match &reg {
                None => true,
                Some(r) => r.get(40) != Some(&1),
            };
            t.set_versionstamped_value(&cv, &[], &[]);
            if candidate {
                let (p, s) = keys::crdt_gc(fs)
                    .vs_incomplete(0)
                    .bytes(&object)
                    .finish_incomplete();
                t.set_versionstamped_key(&p, &s, &[]);
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ reads

fn last_elems(k: &[u8], pfx_len: usize, n: usize) -> ApiResult<Vec<Elem>> {
    Ok(unpack_prefix(&k[pfx_len..], n)
        .map_err(|_| internal("bad CRDT key"))?
        .0)
}

fn bytes_elem(e: &Elem) -> ApiResult<Vec<u8>> {
    match e {
        Elem::Bytes(b) => Ok(b.clone()),
        _ => Err(internal("bad CRDT key")),
    }
}

/// Read a prefix whole, or 413 past the caps.
async fn read_all(
    st: &Shared,
    t: &mut Box<dyn Txn>,
    prefix: &[u8],
) -> ApiResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let max = st.cfg.limits.max_range_items as usize;
    let got = t
        .snapshot_get_range(prefix, &keys::end_of(prefix), max + 1, false)
        .await?;
    if got.len() > max {
        return Err(too_large("the object has too many entries"));
    }
    Ok(got)
}

/// The state of one object, and its size in bytes; `None` if it doesn't exist.
async fn load(
    st: &Shared,
    t: &mut Box<dyn Txn>,
    fs: u32,
    object: &[u8],
) -> ApiResult<Option<(ObjState, usize)>> {
    let Some(version) = t
        .snapshot_get(&keys::crdt_objects(fs).bytes(object).finish())
        .await?
    else {
        return Ok(None);
    };
    let mut size = object.len() + version.len();
    let row = match t.snapshot_get(&keys::crdt_row(fs, object)).await? {
        Some(r) => {
            let ts = ts_of(&r)?;
            size += r.len();
            Some(RowReg {
                hlc: ts.hlc,
                device: ts.dev.to_vec(),
                alive: r.get(40) == Some(&1),
                value: r.get(41..).unwrap_or_default().to_vec(),
            })
        }
        None => None,
    };
    let mut lww = Vec::new();
    let pfx = keys::crdt_lww(fs, object).finish();
    for (k, v) in read_all(st, t, &pfx).await? {
        let field = bytes_elem(&last_elems(&k, pfx.len(), 1)?[0])?;
        let ts = ts_of(&v)?;
        size += k.len() + v.len();
        lww.push(LwwReg {
            field,
            hlc: ts.hlc,
            device: ts.dev.to_vec(),
            value: (v.get(40) == Some(&1)).then(|| v[41..].to_vec()),
        });
    }
    let mut ctr = Vec::new();
    let pfx = keys::crdt_ctr(fs, object).finish();
    for (k, v) in read_all(st, t, &pfx).await? {
        let e = last_elems(&k, pfx.len(), 2)?;
        if v.len() < 8 || e.len() != 2 {
            return Err(internal("bad counter entry"));
        }
        size += k.len() + v.len();
        ctr.push(CtrEntry {
            field: bytes_elem(&e[0])?,
            device: bytes_elem(&e[1])?,
            seq: u64::from_be_bytes(v[..8].try_into().expect("8")),
            value: v[8..].to_vec(),
        });
    }
    let mut set = Vec::new();
    let pfx = keys::crdt_set(fs, object).finish();
    for (k, v) in read_all(st, t, &pfx).await? {
        let e = last_elems(&k, pfx.len(), 3)?;
        let [Elem::Bytes(field), Elem::Bytes(elem), Elem::Vs(dot)] = e.as_slice() else {
            return Err(internal("bad set key"));
        };
        if v.len() < 32 {
            return Err(internal("bad set entry"));
        }
        size += k.len() + v.len();
        set.push(SetDot {
            field: field.clone(),
            elem: elem.clone(),
            dot: dot.to_vec(),
            device: v[..32].to_vec(),
            value: v[32..].to_vec(),
        });
    }
    if size > st.cfg.limits.max_range_bytes as usize {
        return Err(too_large("the object's state exceeds max_range_bytes"));
    }
    Ok(Some((
        ObjState {
            object: object.to_vec(),
            row,
            lww,
            ctr,
            set,
            version,
        },
        size,
    )))
}

fn check_read(st: &Shared, caller: &Caller, fs: u32) -> ApiResult<()> {
    st.check_fs(fs)?;
    caller.require_fs(fs, R_READ)
}

/// `POST /v1/crdt/get`.
pub async fn get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CrdtGet>,
) -> ApiResult<Cbor<ObjStates>> {
    check_read(&st, &caller, req.fs)?;
    if req.objects.len() > 1000 {
        return Err(too_large("at most 1000 objects"));
    }
    for o in &req.objects {
        check_object(&st, o)?;
    }
    let mut t = st.store.begin(req.read_version).await?;
    let mut objects = Vec::new();
    let mut bytes = 0usize;
    for o in &req.objects {
        if let Some((s, n)) = load(&st, &mut t, req.fs, o).await? {
            bytes += n;
            if bytes > st.cfg.limits.max_range_bytes as usize {
                return Err(too_large(
                    "the objects exceed max_range_bytes; ask for fewer",
                ));
            }
            objects.push(s);
        }
    }
    Ok(Cbor(ObjStates {
        read_version: t.read_version(),
        objects,
        more: false,
    }))
}

/// `POST /v1/crdt/range`.
pub async fn range(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CrdtRange>,
) -> ApiResult<Cbor<ObjStates>> {
    check_read(&st, &caller, req.fs)?;
    let limit = req
        .limit
        .unwrap_or(st.cfg.limits.max_range_items)
        .clamp(1, st.cfg.limits.max_range_items) as usize;
    let pfx = keys::crdt_objects(req.fs).finish();
    let begin = keys::crdt_objects(req.fs).bytes(&req.begin).finish();
    let end = match &req.end {
        Some(e) => keys::crdt_objects(req.fs).bytes(e).finish(),
        None => keys::end_of(&pfx),
    };
    let mut t = st.store.begin(req.read_version).await?;
    let got = t.snapshot_get_range(&begin, &end, limit + 1, false).await?;
    let mut more = got.len() > limit;
    let max_bytes = st.cfg.limits.max_range_bytes as usize;
    let mut bytes = 0usize;
    let mut objects = Vec::new();
    for (k, _) in got.into_iter().take(limit) {
        if bytes >= max_bytes {
            more = true;
            break;
        }
        let object = bytes_elem(&last_elems(&k, pfx.len(), 1)?[0])?;
        if let Some((s, n)) = load(&st, &mut t, req.fs, &object).await? {
            bytes += n;
            objects.push(s);
        }
    }
    Ok(Cbor(ObjStates {
        read_version: t.read_version(),
        objects,
        more,
    }))
}

// ------------------------------------------------------------------ sweep

/// Purge one object if it is still a candidate as of `stamp`. Returns
/// whether its candidate entry was settled.
async fn purge(st: &Shared, fs: u32, gc_key: &[u8], stamp: &[u8], object: &[u8]) -> ApiResult<()> {
    let (_, _) = txn_loop!(st.store, None, |t| {
        let cv = keys::crdt_objects(fs).bytes(object).finish();
        let cur = t.get(&cv).await?;
        let reg = t.get(&keys::crdt_row(fs, object)).await?;
        t.clear(gc_key);
        let is_candidate = reg.as_ref().is_none_or(|r| r.get(40) != Some(&1));
        if cur.as_deref() != Some(stamp) || !is_candidate {
            return Ok(()); // a newer change owns the object: a stale entry
        }
        let (mut bytes, mut nkeys) = (0i64, 1i64);
        if let Some(r) = &reg {
            bytes += r.len() as i64;
            nkeys += 1;
        }
        t.clear(&keys::crdt_row(fs, object));
        t.clear(&cv);
        for p in [
            keys::crdt_lww(fs, object).finish(),
            keys::crdt_ctr(fs, object).finish(),
            keys::crdt_set(fs, object).finish(),
        ] {
            let end = keys::end_of(&p);
            let mut begin = p.clone();
            loop {
                let page = t.get_range(&begin, &end, 1000, false).await?;
                for (_, v) in &page {
                    bytes += v.len() as i64;
                    nkeys += 1;
                }
                match page.last() {
                    Some((k, _)) if page.len() == 1000 => begin = key_after(k),
                    _ => break,
                }
            }
            t.clear_range(&p, &end);
        }
        t.atomic_add(&keys::quota(fs, "bytes"), -bytes);
        t.atomic_add(&keys::quota(fs, "keys"), -nkeys);
        Ok(())
    })?;
    Ok(())
}

/// Purge objects whose row register is `alive: false`, or that have none,
/// and whose last change is older than the horizon (api.md §13.4).
pub async fn sweep(st: &Shared, now: Version) -> ApiResult<usize> {
    let l = &st.cfg.limits;
    // Keep `crdt_max_skew_ms` of margin, as for trees.
    let horizon = l.crdt_horizon_secs * VERSIONS_PER_SEC + l.crdt_max_skew_ms * 1000;
    let mut cut = [0u8; 12];
    cut[..STAMP_LEN].copy_from_slice(&stamp_of(now.saturating_sub(horizon)));
    let mut purged = 0;
    for f in &st.cfg.fs {
        let fs = f.id;
        let pfx = keys::crdt_gc(fs).finish();
        let end = keys::crdt_gc(fs).vs(&cut).finish();
        for _ in 0..100 {
            let page = {
                let mut t = st.store.begin(None).await?;
                t.snapshot_get_range(&pfx, &end, 1000, false).await?
            };
            for (k, _) in &page {
                let e = last_elems(k, pfx.len(), 2)?;
                let [Elem::Vs(vs), Elem::Bytes(object)] = e.as_slice() else {
                    return Err(internal("bad CRDT GC key"));
                };
                purge(st, fs, k, &vs[..STAMP_LEN], object).await?;
                purged += 1;
            }
            if page.len() < 1000 {
                break;
            }
        }
    }
    Ok(purged)
}

/// Encode the set dots of a commit.
pub fn set_dots(stamp: &[u8; STAMP_LEN], n: u16) -> Vec<ByteBuf> {
    (0..n)
        .map(|i| ByteBuf::from(crate::ids::offset(stamp, i).to_vec()))
        .collect()
}
