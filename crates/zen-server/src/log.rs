//! Log reads (api.md §7.2) and the shared "events after a cursor" reader.

use crate::acl::R_READ;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::ids::{Offset, check_key_token, check_topic, decode_entry, parse_offset};
use crate::keys;
use crate::state::Shared;
use axum::extract::State;
use zen_proto::{Event, LogEvents, LogRead};
use zen_store::Storage;

/// Up to `limit` events of `topic` after `after` (optionally one key's).
/// Returns the events and whether more follow.
pub async fn read_topic(
    store: &dyn Storage,
    fs: u32,
    topic: &[u8],
    key: Option<&[u8]>,
    after: &Offset,
    limit: usize,
    max_bytes: usize,
) -> ApiResult<(Vec<(Offset, Event)>, bool)> {
    let mut t = store.begin(None).await?;
    let prefix = match key {
        Some(k) => keys::lk_prefix(fs, topic, k),
        None => keys::log_prefix(fs, topic),
    };
    let (b, e) = keys::after_offset(prefix, after);
    let (mut got, capped) =
        crate::kv::read_capped(&mut t, &b, &e, limit + 1, false, max_bytes, false).await?;
    let mut more = capped || got.len() > limit;
    got.truncate(limit);
    let mut out = Vec::with_capacity(got.len());
    let mut bytes = 0usize;
    for (k, v) in got {
        let o: Offset = k[k.len() - 12..].try_into().expect("12 bytes");
        let entry = match key {
            Some(_) => t
                .snapshot_get(&keys::log_prefix(fs, topic).vs(&o).finish())
                .await?
                .ok_or_else(|| internal("index points at a missing event"))?,
            None => v,
        };
        // Through the per-key index the bodies are fetched one by one.
        bytes += entry.len();
        if key.is_some() && bytes > max_bytes && !out.is_empty() {
            more = true;
            break;
        }
        let (key_token, envelope) = decode_entry(&entry);
        out.push((
            o,
            Event {
                offset: o.to_vec(),
                key_token,
                envelope,
            },
        ));
    }
    Ok((out, more))
}

/// Up to `limit` events of every topic under `prefix` after `after`, in
/// offset order: `(topic, offset, event)`. Also returns the last offset
/// scanned (which moves even past filtered-out events).
pub async fn read_prefix(
    store: &dyn Storage,
    fs: u32,
    prefix: &[u8],
    after: &Offset,
    limit: usize,
    max_bytes: usize,
) -> ApiResult<(Vec<(Vec<u8>, Event)>, Offset, bool)> {
    let mut t = store.begin(None).await?;
    let (b, e) = keys::after_offset(keys::gl_prefix(fs), after);
    let got = t.snapshot_get_range(&b, &e, limit, false).await?;
    let mut more = got.len() == limit;
    let mut last = *after;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for (k, topic) in got {
        let o: Offset = k[k.len() - 12..].try_into().expect("12 bytes");
        if bytes >= max_bytes {
            more = true;
            break;
        }
        last = o;
        if !topic.starts_with(prefix) {
            continue;
        }
        let Some(entry) = t
            .snapshot_get(&keys::log_prefix(fs, &topic).vs(&o).finish())
            .await?
        else {
            continue;
        };
        bytes += entry.len();
        let (key_token, envelope) = decode_entry(&entry);
        out.push((
            topic,
            Event {
                offset: o.to_vec(),
                key_token,
                envelope,
            },
        ));
    }
    Ok((out, last, more))
}

/// `POST /v1/log/read`.
pub async fn read(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<LogRead>,
) -> ApiResult<Cbor<LogEvents>> {
    st.check_fs(req.fs)?;
    check_topic(&req.topic)?;
    check_key_token(req.key_token.as_deref())?;
    caller.require_topic(req.fs, &req.topic, R_READ)?;
    let after = parse_offset(req.after.as_deref())?;
    let max = st.cfg.limits.max_range_items;
    let limit = req.limit.unwrap_or(max).clamp(1, max) as usize;
    let (events, more) = read_topic(
        st.store.as_ref(),
        req.fs,
        &req.topic,
        req.key_token.as_deref(),
        &after,
        limit,
        st.cfg.limits.max_range_bytes as usize,
    )
    .await?;
    Ok(Cbor(LogEvents {
        events: events.into_iter().map(|(_, e)| e).collect(),
        more,
    }))
}
