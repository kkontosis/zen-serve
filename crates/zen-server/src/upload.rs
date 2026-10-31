//! Upload sessions (spec/api.md §12.1, keyspace.md §3.6): a session holds
//! one reference to every chunk and page put under it, so a long upload's
//! content outlives `chunk_grace_secs` until the session closes or expires.
//!
//! * `us` is the record, `user ‖ u64 expires_version` (0 once closed).
//!   Commits under the session only read it, so several can be in flight.
//! * `ur` holds the references, one per kind and id.
//! * `ux` indexes sessions by expiry (0 when closed), for the sweeper.
//! * `uu` indexes a user's sessions, for the cap.
//!
//! Every release round rewrites `us`, so a commit or renew that read the
//! record before the expiry and commits after the round conflicts, and its
//! retry (at a read version past the expiry) is refused: nothing new is
//! referenced once the release has started.

use crate::acl::{Fp, R_READ, R_WRITE};
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::{ApiResult, bad_request, internal, quota, upload_gone};
use crate::keys;
use crate::state::Shared;
use crate::tree::{add_quota, release_chunk, release_page, unix_ms};
use crate::txn::txn_loop;
use axum::extract::State;
use zen_proto::{
    Empty, UploadClose, UploadExpiry, UploadList, UploadOpen, UploadRef, UploadRenew,
    UploadSession, UploadSessions,
};
use zen_store::tuple::{Elem, unpack_prefix};
use zen_store::{Txn, VERSIONS_PER_SEC, Version};

/// A session id.
pub type Session = [u8; 16];

/// Open sessions per user and fs (api.md §12.1).
pub const MAX_UPLOAD_SESSIONS: usize = 64;
/// `ur` kind of a chunk reference.
pub const KIND_CHUNK: u8 = 1;
/// `ur` kind of a page reference.
pub const KIND_PAGE: u8 = 2;
/// References released per transaction: three mutations each.
const RELEASE_BUDGET: usize = 5_000;
/// Release transactions per fs and sweeper pass.
const MAX_ROUNDS: usize = 100;

struct Rec {
    user: Fp,
    expires: Version,
}

fn encode(user: &Fp, expires: Version) -> Vec<u8> {
    let mut v = user.to_vec();
    v.extend_from_slice(&expires.to_be_bytes());
    v
}

fn decode(v: &[u8]) -> ApiResult<Rec> {
    if v.len() != 40 {
        return Err(internal("bad upload session record"));
    }
    Ok(Rec {
        user: v[..32].try_into().expect("32"),
        expires: u64::from_be_bytes(v[32..].try_into().expect("8")),
    })
}

fn u64_of(v: &[u8]) -> u64 {
    v.try_into().map(u64::from_be_bytes).unwrap_or(0)
}

fn session_id(b: &[u8]) -> ApiResult<Session> {
    b.try_into()
        .map_err(|_| bad_request("session must be 16 bytes"))
}

/// The lifetime asked for, defaulted and capped, in seconds.
fn ttl_of(st: &Shared, asked: Option<u64>) -> ApiResult<u64> {
    let l = &st.cfg.limits;
    match asked {
        Some(0) => Err(bad_request("ttl_secs must be at least 1")),
        Some(t) => Ok(t.min(l.upload_max_ttl_secs)),
        None => Ok(l.upload_ttl_secs),
    }
}

/// The unix-ms time of `expires`, from the wall clock now and the
/// distance in versions.
fn expires_ms(expires: Version, now: Version) -> u64 {
    unix_ms() + expires.saturating_sub(now) / (VERSIONS_PER_SEC / 1000)
}

/// Write the record and both indexes for an open session.
fn put(t: &mut Box<dyn Txn>, fs: u32, s: &Session, user: &Fp, expires: Version) {
    t.set(&keys::upload_session(fs, s), &encode(user, expires));
    t.set(&keys::upload_expiry_entry(fs, expires, s), &[]);
    t.set(
        &keys::upload_user_entry(fs, user, s),
        &expires.to_be_bytes(),
    );
}

/// The caller's open, unexpired session, read with a conflict (a close,
/// renew or release that commits meanwhile makes this transaction retry);
/// anything else is 412 `upload_gone`. Another user's session reads as
/// unknown.
async fn live(t: &mut Box<dyn Txn>, fs: u32, s: &Session, user: &Fp) -> ApiResult<Rec> {
    let now = t.read_version();
    if let Some(v) = t.get(&keys::upload_session(fs, s)).await? {
        let rec = decode(&v)?;
        if rec.user == *user && rec.expires > now {
            return Ok(rec);
        }
    }
    Err(upload_gone("upload session closed, expired or unknown"))
}

/// The upload-session check of a commit (api.md §6): the session must be
/// the caller's, open and unexpired at the commit's read.
pub async fn check(t: &mut Box<dyn Txn>, caller: &Caller, r: &UploadRef) -> ApiResult<Session> {
    let s = session_id(&r.session)?;
    live(t, r.fs, &s, &caller.user).await?;
    Ok(s)
}

/// Reference `id` from session `s`, unless it does already: adds one to
/// its `cr` or `pr`. The `ur` key is read with a conflict, so two commits
/// in flight that put the same id can't both count it. Returns whether a
/// reference (one quota key) was added.
pub async fn reference(
    t: &mut Box<dyn Txn>,
    fs: u32,
    s: &Session,
    kind: u8,
    id: &[u8; 16],
) -> ApiResult<bool> {
    let k = keys::upload_ref(fs, s, kind, id);
    if t.get(&k).await?.is_some() {
        return Ok(false);
    }
    t.set(&k, &[]);
    let refs = if kind == KIND_CHUNK {
        keys::chunk_refs(fs, id)
    } else {
        keys::page_refs(fs, id)
    };
    t.atomic_add(&refs, 1);
    Ok(true)
}

/// `POST /v1/fs/upload/open`.
pub async fn open(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<UploadOpen>,
) -> ApiResult<Cbor<UploadSession>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_WRITE)?;
    let ttl = ttl_of(&st, req.ttl_secs)?;
    let mut s: Session = [0; 16];
    getrandom::fill(&mut s).expect("OS RNG");
    let (out, _) = txn_loop!(st.store, None, |t| {
        let now = t.read_version();
        let pfx = keys::upload_user(req.fs, &caller.user).finish();
        let mine = t
            .get_range(&pfx, &keys::end_of(&pfx), 10_000, false)
            .await?;
        if mine.iter().filter(|(_, v)| u64_of(v) > now).count() >= MAX_UPLOAD_SESSIONS {
            return Err(quota(format!(
                "at most {MAX_UPLOAD_SESSIONS} open upload sessions per user and fs"
            )));
        }
        let expires = now + ttl * VERSIONS_PER_SEC;
        put(&mut t, req.fs, &s, &caller.user, expires);
        Ok(UploadSession {
            session: s.to_vec(),
            expires_ms: expires_ms(expires, now),
        })
    })?;
    Ok(Cbor(out))
}

/// `POST /v1/fs/upload/renew`.
pub async fn renew(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<UploadRenew>,
) -> ApiResult<Cbor<UploadExpiry>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_WRITE)?;
    let s = session_id(&req.session)?;
    let ttl = ttl_of(&st, req.ttl_secs)?;
    let (out, _) = txn_loop!(st.store, None, |t| {
        let rec = live(&mut t, req.fs, &s, &caller.user).await?;
        let now = t.read_version();
        let expires = now + ttl * VERSIONS_PER_SEC;
        t.clear(&keys::upload_expiry_entry(req.fs, rec.expires, &s));
        put(&mut t, req.fs, &s, &caller.user, expires);
        Ok(UploadExpiry {
            expires_ms: expires_ms(expires, now),
        })
    })?;
    Ok(Cbor(out))
}

/// `POST /v1/fs/upload/close`: ends the caller's open session; anything
/// else (ended, unknown, another user's) is a no-op. The sweeper releases
/// its references.
pub async fn close(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<UploadClose>,
) -> ApiResult<Cbor<Empty>> {
    st.check_fs(req.fs)?;
    let s = session_id(&req.session)?;
    txn_loop!(st.store, None, |t| {
        let Ok(rec) = live(&mut t, req.fs, &s, &caller.user).await else {
            return Ok(());
        };
        t.set(&keys::upload_session(req.fs, &s), &encode(&rec.user, 0));
        t.clear(&keys::upload_expiry_entry(req.fs, rec.expires, &s));
        t.set(&keys::upload_expiry_entry(req.fs, 0, &s), &[]);
        t.clear(&keys::upload_user_entry(req.fs, &rec.user, &s));
        Ok(())
    })?;
    Ok(Cbor(Empty {}))
}

/// `POST /v1/fs/upload/list`: the caller's open, unexpired sessions.
pub async fn list(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<UploadList>,
) -> ApiResult<Cbor<UploadSessions>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_READ)?;
    let mut t = st.store.begin(None).await?;
    let now = t.read_version();
    let pfx = keys::upload_user(req.fs, &caller.user).finish();
    let mut sessions = Vec::new();
    for (k, v) in t
        .snapshot_get_range(&pfx, &keys::end_of(&pfx), 10_000, false)
        .await?
    {
        let expires = u64_of(&v);
        if expires > now {
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 1).map_err(|_| internal("bad uu key"))?;
            let [Elem::Bytes(s)] = elems.as_slice() else {
                return Err(internal("bad uu key"));
            };
            sessions.push(UploadSession {
                session: s.clone(),
                expires_ms: expires_ms(expires, now),
            });
        }
    }
    Ok(Cbor(UploadSessions { sessions }))
}

/// One release round of session `s`, listed in `ux` at `at`: releases up
/// to `budget` references. Returns the references released and whether
/// the session is done (its record and indexes cleared, or nothing to do).
async fn release_round(
    st: &Shared,
    fs: u32,
    at: Version,
    s: &Session,
    budget: usize,
) -> ApiResult<(usize, bool)> {
    let (out, _) = txn_loop!(st.store, None, |t| {
        let entry = keys::upload_expiry_entry(fs, at, s);
        let key = keys::upload_session(fs, s);
        let rec = match t.get(&key).await? {
            Some(v) => decode(&v)?,
            None => {
                t.clear(&entry);
                return Ok((0, true));
            }
        };
        if rec.expires != at {
            // A stale entry: renew or close moved it.
            t.clear(&entry);
            return Ok((0, true));
        }
        if rec.expires > t.read_version() {
            return Ok((0, true)); // not due yet
        }
        let pfx = keys::upload_refs(fs, s).finish();
        let got = t
            .get_range(&pfx, &keys::end_of(&pfx), budget, false)
            .await?;
        for (k, _) in &got {
            t.clear(k);
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad ur key"))?;
            let [Elem::Int(kind), Elem::Bytes(id)] = elems.as_slice() else {
                return Err(internal("bad ur key"));
            };
            let id: [u8; 16] = id[..].try_into().map_err(|_| internal("bad ur key"))?;
            match *kind {
                k if k == i64::from(KIND_CHUNK) => release_chunk(&mut t, fs, &id),
                k if k == i64::from(KIND_PAGE) => release_page(&mut t, fs, &id),
                _ => return Err(internal("bad ur kind")),
            }
        }
        add_quota(&mut t, fs, 0, -(got.len() as i64));
        // Fewer than asked: the read covered the whole range, with a
        // conflict, so no reference can be left behind.
        if got.len() < budget {
            t.clear(&key);
            t.clear(&entry);
            t.clear(&keys::upload_user_entry(fs, &rec.user, s));
            Ok((got.len(), true))
        } else {
            // Rewritten so that commits and renews that read it before
            // the expiry conflict (module docs).
            t.set(&key, &encode(&rec.user, rec.expires));
            Ok((got.len(), false))
        }
    })?;
    Ok(out)
}

/// Release closed and expired sessions (fs.md §6), in bounded rounds.
/// Returns the references released.
pub async fn sweep(st: &Shared, now: Version) -> ApiResult<usize> {
    sweep_with(st, now, RELEASE_BUDGET, MAX_ROUNDS).await
}

async fn sweep_with(st: &Shared, now: Version, budget: usize, rounds: usize) -> ApiResult<usize> {
    let mut released = 0;
    for f in &st.cfg.fs {
        let fs = f.id;
        let pfx = keys::upload_expiry(fs).finish();
        let end = keys::upload_expiry(fs).int((now + 1) as i64).finish();
        let mut left = rounds;
        'fs: loop {
            let due = {
                let mut t = st.store.begin(None).await?;
                t.snapshot_get_range(&pfx, &end, 100, false).await?
            };
            for (k, _) in &due {
                let (elems, _) =
                    unpack_prefix(&k[pfx.len()..], 2).map_err(|_| internal("bad ux key"))?;
                let [Elem::Int(at), Elem::Bytes(s)] = elems.as_slice() else {
                    return Err(internal("bad ux key"));
                };
                let s = session_id(s).map_err(|_| internal("bad ux key"))?;
                loop {
                    if left == 0 {
                        break 'fs;
                    }
                    left -= 1;
                    let (n, done) = release_round(st, fs, *at as Version, &s, budget).await?;
                    released += n;
                    if done {
                        break;
                    }
                }
            }
            if due.len() < 100 {
                break;
            }
        }
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::AclState;
    use crate::config::Config;
    use crate::state::AppState;
    use std::sync::Arc;
    use zen_store::embedded::{Embedded, Options};

    const FS: u32 = 1;

    async fn count(st: &Shared, pfx: Vec<u8>) -> usize {
        let mut t = st.store.begin(None).await.unwrap();
        t.snapshot_get_range(&pfx, &keys::end_of(&pfx), 1_000_000, false)
            .await
            .unwrap()
            .len()
    }

    /// A closed session with 30 references releases at most `budget` per
    /// round, and only the round that finds the range short clears the
    /// record and its indexes.
    #[tokio::test]
    async fn release_is_bounded_and_clears_last() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Embedded::open(dir.path().join("zen.redb"), Options::default()).unwrap());
        let cfg = Config::with_data_dir(dir.path().to_path_buf());
        let st: Shared = Arc::new(AppState::new(
            cfg,
            store,
            AclState::default(),
            None,
            [0; 32],
        ));
        let s: Session = [7; 16];
        let user: Fp = [9; 32];
        let mut t = st.store.begin(None).await.unwrap();
        put(&mut t, FS, &s, &user, 5);
        for i in 0..30u8 {
            assert!(
                reference(&mut t, FS, &s, KIND_CHUNK, &[i; 16])
                    .await
                    .unwrap()
            );
        }
        t.commit().await.unwrap();
        let now = st.store.now_version().await.unwrap();
        let refs = || keys::upload_refs(FS, &s).finish();
        for left in [23, 16, 9, 2] {
            assert_eq!(sweep_with(&st, now, 7, 1).await.unwrap(), 7);
            assert_eq!(count(&st, refs()).await, left);
            assert_eq!(count(&st, keys::upload_session(FS, &s)).await, 1);
        }
        assert_eq!(sweep_with(&st, now, 7, 1).await.unwrap(), 2);
        assert_eq!(count(&st, refs()).await, 0);
        assert_eq!(count(&st, keys::upload_session(FS, &s)).await, 0);
        assert_eq!(count(&st, keys::upload_expiry(FS).finish()).await, 0);
        assert_eq!(count(&st, keys::upload_user(FS, &user).finish()).await, 0);
        // Each release made a GC candidate and dropped the count to 0.
        let mut t = st.store.begin(None).await.unwrap();
        let cr = t
            .get(&keys::chunk_refs(FS, &[3; 16]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(i64::from_le_bytes(cr.try_into().unwrap()), 0);
        assert_eq!(count(&st, keys::chunk_gc(FS).finish()).await, 30);
    }
}
