//! Ephemeral pub/sub across nodes (api.md §9.3): a short-lived ring in
//! storage, `("eph", fs, vs)`, with head `("eh", fs)`. Each node runs one
//! tailer per fs that has live subscribers: it watches the head, reads new
//! entries and fans them out locally. The sweeper drops entries older than
//! `limits.ephemeral_ttl_secs`.
//!
//! Publishes are rate-limited per device by a token bucket on each node
//! (`limits.ephemeral_bytes_per_sec`, `ephemeral_burst_bytes`): the ring
//! holds no more than the limit's worth of a device's messages per node.

use crate::acl::Fp;
use crate::config::EPH_MSG_OVERHEAD;
use crate::error::{ApiResult, internal, quota};
use crate::ids::offset;
use crate::keys;
use crate::txn::txn_loop;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};
use zen_store::{Storage, Version, key_after, stamp_of};

const BATCH: usize = 1024;

/// One ephemeral message.
#[derive(Debug)]
pub struct EphMsg {
    /// fs_id.
    pub fs: u32,
    /// Topic id.
    pub topic: Vec<u8>,
    /// Opaque data.
    pub data: Vec<u8>,
    /// Sending device.
    pub sender: Fp,
}

fn encode(topic: &[u8], sender: &Fp, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + topic.len() + 32 + data.len());
    v.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    v.extend_from_slice(topic);
    v.extend_from_slice(sender);
    v.extend_from_slice(data);
    v
}

fn decode(fs: u32, v: &[u8]) -> Option<EphMsg> {
    let n = u16::from_be_bytes(v.get(..2)?.try_into().ok()?) as usize;
    let topic = v.get(2..2 + n)?.to_vec();
    let sender = v.get(2 + n..2 + n + 32)?.try_into().ok()?;
    Some(EphMsg {
        fs,
        topic,
        sender,
        data: v[2 + n + 32..].to_vec(),
    })
}

struct Tailer {
    tx: broadcast::Sender<Arc<EphMsg>>,
    ready: watch::Receiver<bool>,
}

struct Inner {
    store: Arc<dyn Storage>,
    tailers: Mutex<HashMap<u32, Arc<Tailer>>>,
    limiter: Mutex<Limiter>,
}

/// Token buckets of publish bytes, one per device, in this node's memory.
struct Limiter {
    /// Refill rate, bytes per second (0: unlimited).
    rate: u64,
    /// Bucket size, bytes.
    burst: u64,
    /// Device → (bytes available, when last refilled).
    buckets: HashMap<Fp, (f64, Instant)>,
}

impl Limiter {
    /// Above this many buckets, full ones are dropped (a full bucket is the
    /// same as none).
    const PRUNE_AT: usize = 4096;

    /// Take `cost` bytes from `device`'s bucket, or refuse.
    fn take(&mut self, device: &Fp, cost: u64, now: Instant) -> bool {
        if self.rate == 0 {
            return true;
        }
        let (rate, burst) = (self.rate as f64, self.burst as f64);
        let refill = |(have, at): (f64, Instant)| {
            (have + now.duration_since(at).as_secs_f64() * rate).min(burst)
        };
        if self.buckets.len() >= Self::PRUNE_AT {
            self.buckets.retain(|_, b| refill(*b) < burst);
        }
        let b = self.buckets.entry(*device).or_insert((burst, now));
        let have = refill(*b);
        let ok = have >= cost as f64;
        *b = (if ok { have - cost as f64 } else { have }, now);
        ok
    }
}

/// This node's ring tailers, one per fs with live subscribers.
#[derive(Clone)]
pub struct EphHub(Arc<Inner>);

impl EphHub {
    /// A hub on `store`, limiting each device to `rate` bytes per second
    /// with bursts of `burst` bytes (`rate` 0: no limit).
    pub fn new(store: Arc<dyn Storage>, rate: u64, burst: u64) -> Self {
        EphHub(Arc::new(Inner {
            store,
            tailers: Mutex::new(HashMap::new()),
            limiter: Mutex::new(Limiter {
                rate,
                burst,
                buckets: HashMap::new(),
            }),
        }))
    }

    /// Subscribe to every message on `fs` published after this returns.
    pub async fn subscribe(&self, fs: u32) -> ApiResult<broadcast::Receiver<Arc<EphMsg>>> {
        let (tailer, rx) = {
            let mut tailers = self.0.tailers.lock().expect("eph lock");
            match tailers.get(&fs) {
                Some(t) => (t.clone(), t.tx.subscribe()),
                None => {
                    let (tx, rx) = broadcast::channel(BATCH);
                    let (ready_tx, ready) = watch::channel(false);
                    let t = Arc::new(Tailer { tx, ready });
                    tailers.insert(fs, t.clone());
                    tokio::spawn(self.clone().tail(fs, t.clone(), ready_tx));
                    (t, rx)
                }
            }
        };
        let mut ready = tailer.ready.clone();
        ready
            .wait_for(|r| *r)
            .await
            .map_err(|_| internal("ephemeral tailer stopped"))?;
        Ok(rx)
    }

    /// Publish to every subscriber on every node. 429 `quota` when `sender`
    /// is over its rate limit on this node.
    pub async fn publish(&self, fs: u32, topic: &[u8], sender: &Fp, data: &[u8]) -> ApiResult<()> {
        let cost = data.len() as u64 + EPH_MSG_OVERHEAD;
        let ok =
            self.0
                .limiter
                .lock()
                .expect("eph limiter lock")
                .take(sender, cost, Instant::now());
        if !ok {
            return Err(quota("ephemeral publish rate limit; slow down"));
        }
        let (p, s) = keys::eph_prefix(fs).vs_incomplete(0).finish_incomplete();
        let v = encode(topic, sender, data);
        let head = keys::eph_head(fs);
        txn_loop!(self.0.store, None, |t| {
            t.set_versionstamped_key(&p, &s, &v);
            t.set_versionstamped_value(&head, &[], &[]);
            Ok(())
        })?;
        Ok(())
    }

    /// Drop entries committed before `cutoff`.
    pub async fn sweep(&self, fs: u32, cutoff: Version) -> ApiResult<()> {
        let b = keys::eph_prefix(fs).finish();
        let e = keys::eph_prefix(fs)
            .vs(&offset(&stamp_of(cutoff), 0))
            .finish();
        txn_loop!(self.0.store, None, |t| {
            t.clear_range(&b, &e);
            Ok(())
        })?;
        Ok(())
    }

    async fn tail(self, fs: u32, me: Arc<Tailer>, ready: watch::Sender<bool>) {
        let prefix = keys::eph_prefix(fs).finish();
        let end = keys::end_of(&prefix);
        let head = keys::eph_head(fs);
        let store = self.0.store.clone();
        // Start after the newest entry: new messages only.
        let mut from = loop {
            match last_key(store.as_ref(), &prefix, &end).await {
                Ok(Some(k)) => break key_after(&k),
                Ok(None) => break prefix.clone(),
                Err(e) => {
                    tracing::warn!(error = %e, fs, "ephemeral tailer");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let mut w = None;
        loop {
            if w.is_none() {
                match store.watch(&head).await {
                    Ok(x) => w = Some(x),
                    Err(e) => {
                        tracing::warn!(error = %e, fs, "ephemeral tailer watch");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
            let _ = ready.send(true);
            let got = match read(store.as_ref(), &from, &end).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(error = %e, fs, "ephemeral tailer read");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if let Some((k, _)) = got.last() {
                from = key_after(k);
            }
            for (_, v) in &got {
                if let Some(m) = decode(fs, v) {
                    let _ = me.tx.send(Arc::new(m));
                }
            }
            if got.len() == BATCH {
                continue;
            }
            let armed = w.take().expect("armed");
            let _ = tokio::time::timeout(Duration::from_secs(30), armed).await;
            let mut tailers = self.0.tailers.lock().expect("eph lock");
            if me.tx.receiver_count() == 0 {
                tailers.remove(&fs);
                return;
            }
        }
    }
}

async fn last_key(store: &dyn Storage, b: &[u8], e: &[u8]) -> zen_store::Result<Option<Vec<u8>>> {
    let mut t = store.begin(None).await?;
    Ok(t.snapshot_get_range(b, e, 1, true)
        .await?
        .pop()
        .map(|(k, _)| k))
}

async fn read(
    store: &dyn Storage,
    b: &[u8],
    e: &[u8],
) -> zen_store::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut t = store.begin(None).await?;
    t.snapshot_get_range(b, e, BATCH, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let mut l = Limiter {
            rate: 1000,
            burst: 3000,
            buckets: HashMap::new(),
        };
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let t0 = Instant::now();
        // A full burst, then nothing until it refills.
        assert!(l.take(&a, 2000, t0));
        assert!(l.take(&a, 1000, t0));
        assert!(!l.take(&a, 1, t0));
        // Devices are independent.
        assert!(l.take(&b, 3000, t0));
        // 0.5 s refills 500 bytes; a refused take costs nothing.
        let t1 = t0 + Duration::from_millis(500);
        assert!(!l.take(&a, 600, t1));
        assert!(l.take(&a, 500, t1));
        // Refills stop at the burst.
        let t2 = t1 + Duration::from_secs(60);
        assert!(!l.take(&a, 3001, t2));
        assert!(l.take(&a, 3000, t2));
        // Rate 0 is unlimited.
        l.rate = 0;
        assert!(l.take(&a, u64::MAX, t2));
    }

    #[test]
    fn full_buckets_are_pruned() {
        let mut l = Limiter {
            rate: 1000,
            burst: 1000,
            buckets: HashMap::new(),
        };
        let t0 = Instant::now();
        for i in 0..Limiter::PRUNE_AT as u32 {
            let mut d = [0u8; 32];
            d[..4].copy_from_slice(&i.to_be_bytes());
            assert!(l.take(&d, 1000, t0));
        }
        // Two seconds later every bucket is full again and is dropped.
        assert!(l.take(&[0xFF; 32], 10, t0 + Duration::from_secs(2)));
        assert_eq!(l.buckets.len(), 1);
    }
}
