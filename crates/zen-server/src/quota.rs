//! fs quotas (formats.md §9): per-fs byte and key counters, checked and
//! moved by every write that stores data.

use crate::error::{ApiResult, quota};
use crate::keys;
use std::collections::HashMap;
use zen_proto::acl::FsLimit;
use zen_store::Txn;

/// `(bytes, keys)` deltas per fs.
pub type Delta = HashMap<u32, (i64, i64)>;

/// Refuse (`quota`) a growth past an fs's limits, then move the counters.
/// The counters are read with snapshot reads (keyspace.md §3.1).
pub async fn apply(
    t: &mut Box<dyn Txn>,
    limits: &HashMap<u32, FsLimit>,
    delta: &Delta,
) -> ApiResult<()> {
    for (fs, (bytes, nkeys)) in delta {
        if let Some(l) = limits.get(fs) {
            for (what, d, max) in [("bytes", *bytes, l.max_bytes), ("keys", *nkeys, l.max_keys)] {
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
    Ok(())
}
