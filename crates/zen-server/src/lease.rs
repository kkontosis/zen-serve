//! Fenced leases (api.md §16): a named single-holder lease with a fencing
//! token, and the `fence` check of commits (api.md §6).
//!
//! They work like the consume leases (§8.2) and share their record
//! (keyspace.md §3.10): tokens only grow, expiry is in versions, and a
//! release sets the expiry to 0.

use crate::acl::R_WRITE;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::consume::{decode_lease, encode_lease, ms_to_versions};
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use zen_proto::{Fence, NamedLease, NamedLeasePut};
use zen_store::Txn;

/// Longest lease name.
pub const MAX_NAME: usize = 64;

const DEFAULT_TTL_MS: u32 = 10_000;
const MAX_TTL_MS: u32 = 600_000;

/// 400 unless `name` is 1–64 bytes.
pub fn check_name(name: &[u8]) -> ApiResult<()> {
    if name.is_empty() || name.len() > MAX_NAME {
        Err(bad_request("lease names are 1 to 64 bytes"))
    } else {
        Ok(())
    }
}

/// `POST /v1/lease`.
pub async fn lease(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<NamedLeasePut>,
) -> ApiResult<Cbor<NamedLease>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_WRITE)?;
    check_name(&req.name)?;
    let ttl = ms_to_versions(req.ttl_ms.unwrap_or(DEFAULT_TTL_MS).clamp(100, MAX_TTL_MS));
    let key = keys::named_lease(req.fs, &req.name);
    let (lease, _) = txn_loop!(st.store, None, |t| {
        let now = t.read_version();
        let cur = t.get(&key).await?.as_deref().and_then(decode_lease);
        if req.release {
            return match (&cur, req.token) {
                (Some(l), Some(tok))
                    if l.holder == caller.device && l.token == tok && l.expires > now =>
                {
                    t.set(&key, &encode_lease(&l.holder, tok, 0, None));
                    Ok(NamedLease {
                        token: tok,
                        expires_version: 0,
                    })
                }
                _ => Err(not_leader("not the lease holder")),
            };
        }
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
        Ok(NamedLease {
            token,
            expires_version: expires,
        })
    })?;
    Ok(Cbor(lease))
}

/// The `fence` check of a commit: the caller holds the lease with this
/// token, unexpired at the transaction's read. The lease is read normally,
/// so a new holder's acquire and this commit can't both commit.
pub async fn check_fence(t: &mut Box<dyn Txn>, caller: &Caller, f: &Fence) -> ApiResult<()> {
    let now = t.read_version();
    match t
        .get(&keys::named_lease(f.fs, &f.name))
        .await?
        .as_deref()
        .and_then(decode_lease)
    {
        Some(l) if l.holder == caller.device && l.token == f.token && l.expires > now => Ok(()),
        _ => Err(claim_lost("the fenced lease is not held with this token")),
    }
}
