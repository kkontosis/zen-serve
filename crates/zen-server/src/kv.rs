//! KV reads (api.md §5).

use crate::acl::R_READ;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use axum::extract::State;
use zen_proto::{Empty, KvGet, KvItem, KvItems, KvRange, ReadVersion};
use zen_store::{KeyValue, Txn, key_after};

/// Items per storage round trip of a capped read.
const PAGE: usize = 256;

/// Read `[begin, end)` in pages: at most `limit` pairs, stopping once the
/// pairs read hold `max_bytes` or more (keys and values). Returns the pairs
/// and whether the byte cap stopped the read before `limit`. A snapshot
/// read, unless `track`.
pub async fn read_capped(
    t: &mut Box<dyn Txn>,
    begin: &[u8],
    end: &[u8],
    limit: usize,
    reverse: bool,
    max_bytes: usize,
    track: bool,
) -> ApiResult<(Vec<KeyValue>, bool)> {
    let (mut b, mut e) = (begin.to_vec(), end.to_vec());
    let mut out: Vec<KeyValue> = Vec::new();
    let mut bytes = 0usize;
    while out.len() < limit {
        let n = (limit - out.len()).min(PAGE);
        let got = if track {
            t.get_range(&b, &e, n, reverse).await?
        } else {
            t.snapshot_get_range(&b, &e, n, reverse).await?
        };
        let done = got.len() < n;
        for kv in got {
            bytes += kv.0.len() + kv.1.len();
            out.push(kv);
            if bytes >= max_bytes {
                let capped = out.len() < limit;
                return Ok((out, capped));
            }
        }
        if done {
            break;
        }
        let last = &out.last().expect("non-empty page").0;
        if reverse {
            e = last.clone();
        } else {
            b = key_after(last);
        }
    }
    Ok((out, false))
}

/// Split a stored value into `(sealed value, version)`.
pub fn split_value(v: Vec<u8>) -> ApiResult<(Vec<u8>, Vec<u8>)> {
    if v.len() < 10 {
        return Err(internal("corrupt KV value"));
    }
    let mut v = v;
    let sealed = v.split_off(10);
    Ok((sealed, v))
}

/// `POST /v1/grv`.
pub async fn grv(
    State(st): State<Shared>,
    _caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<ReadVersion>> {
    let t = st.store.begin(None).await?;
    Ok(Cbor(ReadVersion {
        read_version: t.read_version(),
    }))
}

/// `POST /v1/kv/get`.
pub async fn get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<KvGet>,
) -> ApiResult<Cbor<KvItems>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_READ)?;
    if req.keys.len() > st.cfg.limits.max_range_items as usize {
        return Err(too_large("too many keys"));
    }
    let mut t = st.store.begin(req.read_version).await?;
    let mut items = Vec::with_capacity(req.keys.len());
    for k in req.keys {
        let k = k.into_vec();
        let item = match t.snapshot_get(&keys::kv(req.fs, &k)).await? {
            Some(v) => {
                let (value, version) = split_value(v)?;
                KvItem {
                    key: k,
                    value: Some(value),
                    version: Some(version),
                }
            }
            None => KvItem {
                key: k,
                value: None,
                version: None,
            },
        };
        items.push(item);
    }
    Ok(Cbor(KvItems {
        read_version: t.read_version(),
        items,
        more: false,
    }))
}

/// `POST /v1/kv/range`.
pub async fn range(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<KvRange>,
) -> ApiResult<Cbor<KvItems>> {
    st.check_fs(req.fs)?;
    caller.require_fs(req.fs, R_READ)?;
    let max = st.cfg.limits.max_range_items;
    let limit = req.limit.unwrap_or(max).clamp(1, max) as usize;
    let (b, e) = keys::kv_range(req.fs, &req.begin, req.end.as_deref());
    let mut t = st.store.begin(req.read_version).await?;
    let (mut got, capped) = read_capped(
        &mut t,
        &b,
        &e,
        limit + 1,
        req.reverse.unwrap_or(false),
        st.cfg.limits.max_range_bytes as usize,
        false,
    )
    .await?;
    let more = capped || got.len() > limit;
    got.truncate(limit);
    let prefix_len = keys::kv_prefix(req.fs).len();
    let items = got
        .into_iter()
        .map(|(k, v)| {
            let (value, version) = split_value(v)?;
            Ok(KvItem {
                key: k[prefix_len..].to_vec(),
                value: Some(value),
                version: Some(version),
            })
        })
        .collect::<ApiResult<_>>()?;
    Ok(Cbor(KvItems {
        read_version: t.read_version(),
        items,
        more,
    }))
}
