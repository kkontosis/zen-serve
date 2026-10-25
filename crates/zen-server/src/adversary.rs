//! The test adversary: a server that lies to readers, so that clients'
//! detection of rollbacks and withheld data (formats.md §14.4) is tested
//! against an attack, never only on honest runs. Built with `test-utils`
//! and switched on by the config key `adversary = true`.
//!
//! [`Adversary`] wraps the store, and the read endpoints read through it
//! (`AppState::view`): kv `get`/`range`, `log/read` and stream
//! subscriptions, `fs/header/get`, the tree reads, checkpoint `heads`/`get`
//! and `cksub`. Commits and the server's own bookkeeping use the store
//! itself, so its state stays honest and lifting the lies restores honest
//! answers.
//!
//! Each lie freezes something as it is when the lie is told: later writes
//! happen, but readers keep seeing the frozen state. The admin-only
//! `POST /v1/test/adversary {lies: [Lie], clear?: bool}` tells them (and
//! `clear` first drops every rule):
//!
//! | Lie | Readers see |
//! |---|---|
//! | `{kind: "kv", fs, key}` | the key's value (or absence) and version as it is now |
//! | `{kind: "header", fs}` | the fs header as it is now |
//! | `{kind: "topic", fs, topic}` | the topic up to its current head: later events are withheld |
//! | `{kind: "tree", fs, tree}` | the tree header (ops, chain) as it is now; later changes are withheld |
//! | `{kind: "checkpoints", fs, device}` | the device's checkpoints up to its current head |
//!
//! Forks need no server code: two servers on copies of one data directory.

use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use axum::extract::State;
use serde::Deserialize;
use std::sync::{Arc, RwLock};
use zen_proto::{ByteBuf, Empty};
use zen_store::tuple::{Elem, unpack_prefix};
use zen_store::{KeyValue, Result, Stamp, Storage, Txn, Version, Watch, key_after};

type Pred = Box<dyn Fn(&[u8], &[u8]) -> bool + Send + Sync>;

/// A storage-level lie.
enum Rule {
    /// Reads of `key` return `value`.
    Freeze {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    /// Keys under `prefix` for which `pred(key, value)` holds are absent.
    Hide { prefix: Vec<u8>, pred: Pred },
}

type Rules = Arc<Vec<Arc<Rule>>>;

/// The store, seen through the current lies.
pub struct Adversary {
    inner: Arc<dyn Storage>,
    rules: RwLock<Rules>,
}

impl Adversary {
    /// Wrap `inner`, telling no lies yet.
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        Adversary {
            inner,
            rules: RwLock::new(Arc::new(Vec::new())),
        }
    }

    fn rules(&self) -> Rules {
        self.rules.read().expect("adversary lock").clone()
    }

    /// Add `more` (after dropping every rule, with `clear`). Transactions
    /// already begun keep the rules they started with.
    fn add(&self, more: Vec<Rule>, clear: bool) {
        let mut cur = self.rules.write().expect("adversary lock");
        let mut v: Vec<Arc<Rule>> = if clear { Vec::new() } else { cur.to_vec() };
        v.extend(more.into_iter().map(Arc::new));
        *cur = Arc::new(v);
    }
}

fn hidden(rules: &[Arc<Rule>], k: &[u8], v: &[u8]) -> bool {
    rules.iter().any(|r| match &**r {
        Rule::Hide { prefix, pred } => k.starts_with(prefix) && pred(k, v),
        Rule::Freeze { .. } => false,
    })
}

fn frozen<'a>(rules: &'a [Arc<Rule>], k: &[u8]) -> Option<&'a Option<Vec<u8>>> {
    rules.iter().rev().find_map(|r| match &**r {
        Rule::Freeze { key, value } if key == k => Some(value),
        _ => None,
    })
}

#[async_trait::async_trait]
impl Storage for Adversary {
    async fn begin(&self, read_version: Option<Version>) -> Result<Box<dyn Txn>> {
        Ok(Box::new(LyingTxn {
            inner: self.inner.begin(read_version).await?,
            rules: self.rules(),
        }))
    }

    async fn watch(&self, key: &[u8]) -> Result<Watch> {
        self.inner.watch(key).await
    }

    async fn now_version(&self) -> Result<Version> {
        self.inner.now_version().await
    }

    async fn advance_version(&self, at_least: Version) -> Result<()> {
        self.inner.advance_version(at_least).await
    }
}

struct LyingTxn {
    inner: Box<dyn Txn>,
    rules: Rules,
}

impl LyingTxn {
    fn point(&self, key: &[u8], got: Option<Vec<u8>>) -> Option<Vec<u8>> {
        if let Some(v) = frozen(&self.rules, key) {
            return v.clone();
        }
        got.filter(|v| !hidden(&self.rules, key, v))
    }

    /// A range read that skips hidden pairs (reading on until `limit`
    /// visible ones or the end) and shows frozen keys as frozen.
    async fn range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
        snapshot: bool,
    ) -> Result<Vec<KeyValue>> {
        if self.rules.is_empty() {
            return if snapshot {
                self.inner
                    .snapshot_get_range(begin, end, limit, reverse)
                    .await
            } else {
                self.inner.get_range(begin, end, limit, reverse).await
            };
        }
        let (mut b, mut e) = (begin.to_vec(), end.to_vec());
        let mut out: Vec<KeyValue> = Vec::new();
        while out.len() < limit {
            let n = limit - out.len();
            let got = if snapshot {
                self.inner.snapshot_get_range(&b, &e, n, reverse).await?
            } else {
                self.inner.get_range(&b, &e, n, reverse).await?
            };
            let done = got.len() < n;
            let last = got.last().map(|kv| kv.0.clone());
            for (k, v) in got {
                if frozen(&self.rules, &k).is_none() && !hidden(&self.rules, &k, &v) {
                    out.push((k, v));
                }
            }
            match last {
                Some(l) if !done => {
                    if reverse {
                        e = l;
                    } else {
                        b = key_after(&l);
                    }
                }
                _ => break,
            }
        }
        // Frozen keys in the range, at their frozen values (the newest
        // freeze of a key wins).
        for r in self.rules.iter() {
            if let Rule::Freeze { key, .. } = &**r
                && key.as_slice() >= begin
                && key.as_slice() < end
                && !out.iter().any(|(k, _)| k == key)
                && let Some(Some(v)) = frozen(&self.rules, key)
            {
                out.push((key.clone(), v.clone()));
            }
        }
        out.sort_by(|x, y| {
            if reverse {
                y.0.cmp(&x.0)
            } else {
                x.0.cmp(&y.0)
            }
        });
        out.truncate(limit);
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Txn for LyingTxn {
    fn read_version(&self) -> Version {
        self.inner.read_version()
    }

    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let got = self.inner.get(key).await?;
        Ok(self.point(key, got))
    }

    async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let got = self.inner.snapshot_get(key).await?;
        Ok(self.point(key, got))
    }

    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        self.range(begin, end, limit, reverse, false).await
    }

    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        self.range(begin, end, limit, reverse, true).await
    }

    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]) {
        self.inner.add_read_conflict_range(begin, end)
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.inner.set(key, value)
    }

    fn clear(&mut self, key: &[u8]) {
        self.inner.clear(key)
    }

    fn clear_range(&mut self, begin: &[u8], end: &[u8]) {
        self.inner.clear_range(begin, end)
    }

    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]) {
        self.inner.set_versionstamped_key(prefix, suffix, value)
    }

    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]) {
        self.inner.set_versionstamped_value(key, prefix, suffix)
    }

    fn atomic_add(&mut self, key: &[u8], delta: i64) {
        self.inner.atomic_add(key, delta)
    }

    async fn commit(self: Box<Self>) -> Result<Stamp> {
        self.inner.commit().await
    }
}

/// A lie, as told to `/v1/test/adversary`.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lie {
    /// Freeze a KV key (a stored key).
    Kv {
        /// fs_id.
        fs: u32,
        /// Stored key.
        key: ByteBuf,
    },
    /// Freeze the fs header.
    Header {
        /// fs_id.
        fs: u32,
    },
    /// Withhold a topic's events after its current head.
    Topic {
        /// fs_id.
        fs: u32,
        /// Topic id.
        topic: ByteBuf,
    },
    /// Freeze a tree's header and withhold its later changes.
    Tree {
        /// fs_id.
        fs: u32,
        /// Tree id.
        tree: ByteBuf,
    },
    /// Withhold a device's checkpoints after its current head.
    Checkpoints {
        /// fs_id.
        fs: u32,
        /// Device fingerprint.
        device: ByteBuf,
    },
}

/// `POST /v1/test/adversary` request.
#[derive(Deserialize)]
pub struct Tell {
    /// Lies to add.
    #[serde(default)]
    pub lies: Vec<Lie>,
    /// Drop every rule first.
    #[serde(default)]
    pub clear: bool,
}

/// The trailing 12-byte offset of a log-like key.
fn tail_offset(k: &[u8]) -> &[u8] {
    &k[k.len().saturating_sub(12)..]
}

/// The first element after `pfx` in `k`.
fn elem_after(pfx: &[u8], k: &[u8]) -> Option<Elem> {
    let rest = k.get(pfx.len()..)?;
    unpack_prefix(rest, 1).ok()?.0.into_iter().next()
}

async fn rules_for(t: &mut Box<dyn Txn>, lie: &Lie) -> ApiResult<Vec<Rule>> {
    Ok(match lie {
        Lie::Kv { fs, key } => {
            let key = keys::kv(*fs, key);
            let value = t.snapshot_get(&key).await?;
            vec![Rule::Freeze { key, value }]
        }
        Lie::Header { fs } => {
            let key = keys::header(*fs);
            let value = t.snapshot_get(&key).await?;
            vec![Rule::Freeze { key, value }]
        }
        Lie::Topic { fs, topic } => {
            let head_key = keys::topic_head(*fs, topic);
            let head = t.snapshot_get(&head_key).await?;
            let h: Vec<u8> = head.clone().unwrap_or_else(|| vec![0; 12]);
            let after = move |k: &[u8]| tail_offset(k) > h.as_slice();
            let (a1, a2, a3) = (after.clone(), after.clone(), after);
            let topic = topic.to_vec();
            vec![
                Rule::Freeze {
                    key: head_key,
                    value: head,
                },
                Rule::Hide {
                    prefix: keys::log_prefix(*fs, &topic).finish(),
                    pred: Box::new(move |k, _| a1(k)),
                },
                Rule::Hide {
                    prefix: keys::lk_topic(*fs, &topic).finish(),
                    pred: Box::new(move |k, _| a2(k)),
                },
                Rule::Hide {
                    prefix: keys::gl_prefix(*fs).finish(),
                    pred: Box::new(move |k, v| v == topic.as_slice() && a3(k)),
                },
            ]
        }
        Lie::Tree { fs, tree } => {
            let hkey = keys::tree_header(*fs, tree);
            let header = t.snapshot_get(&hkey).await?;
            let pfx = keys::changes(*fs, tree).finish();
            let last = t
                .snapshot_get_range(&pfx, &keys::end_of(&pfx), 1, true)
                .await?;
            let max = match last.first().and_then(|(k, _)| elem_after(&pfx, k)) {
                Some(Elem::Vs(o)) => o,
                _ => [0; 12],
            };
            let p = pfx.clone();
            vec![
                Rule::Freeze {
                    key: hkey,
                    value: header,
                },
                Rule::Hide {
                    prefix: pfx,
                    pred: Box::new(
                        move |k, _| matches!(elem_after(&p, k), Some(Elem::Vs(o)) if o > max),
                    ),
                },
            ]
        }
        Lie::Checkpoints { fs, device } => {
            let hkey = keys::ckpt_head(*fs, device);
            let head = t.snapshot_get(&hkey).await?;
            let s = head
                .as_deref()
                .and_then(|v| <[u8; 8]>::try_from(v).ok())
                .map_or(0, u64::from_be_bytes) as i64;
            let pfx = keys::ckpts(*fs, device).finish();
            let p = pfx.clone();
            vec![
                Rule::Freeze {
                    key: hkey,
                    value: head,
                },
                Rule::Hide {
                    prefix: pfx,
                    pred: Box::new(
                        move |k, _| matches!(elem_after(&p, k), Some(Elem::Int(q)) if q > s),
                    ),
                },
            ]
        }
    })
}

/// `POST /v1/test/adversary` (admins; only with `adversary = true`).
pub async fn tell(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<Tell>,
) -> ApiResult<Cbor<Empty>> {
    let Some(adv) = st.adversary.clone() else {
        return Err(not_found("no adversary"));
    };
    if !caller.is_admin() {
        return Err(forbidden("admins only"));
    }
    let mut t = st.store.begin(None).await?;
    let mut rules = Vec::new();
    for lie in &req.lies {
        st.check_fs(match lie {
            Lie::Kv { fs, .. }
            | Lie::Header { fs }
            | Lie::Topic { fs, .. }
            | Lie::Tree { fs, .. }
            | Lie::Checkpoints { fs, .. } => *fs,
        })?;
        rules.extend(rules_for(&mut t, lie).await?);
    }
    adv.add(rules, req.clear);
    Ok(Cbor(Empty {}))
}
