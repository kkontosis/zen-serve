//! [`Prefixed`]: a backend seen through a key prefix.
//!
//! Every key is stored as `root ‖ key`, and reads strip `root` again, so the
//! wrapped keyspace looks exactly like an unprefixed one. Uses:
//! * test isolation: many test servers share one FoundationDB cluster
//! * serving a restored clone: `fdbrestore --add-prefix P`, then a server with
//!   `storage.key_prefix = P` (spec/operations.md)

use crate::{KeyValue, Result, Stamp, Storage, Txn, Version, Watch};
use std::sync::Arc;

/// `inner`, with every key under `root`.
#[derive(Clone)]
pub struct Prefixed {
    inner: Arc<dyn Storage>,
    root: Arc<[u8]>,
}

impl Prefixed {
    /// Wrap `inner` under `root`. `root` must not be a prefix of another
    /// keyspace in use (`root` itself is not escaped).
    pub fn new(inner: Arc<dyn Storage>, root: &[u8]) -> Self {
        Prefixed {
            inner,
            root: root.into(),
        }
    }
}

fn join(root: &[u8], key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(root.len() + key.len());
    k.extend_from_slice(root);
    k.extend_from_slice(key);
    k
}

#[async_trait::async_trait]
impl Storage for Prefixed {
    async fn begin(&self, read_version: Option<Version>) -> Result<Box<dyn Txn>> {
        Ok(Box::new(PrefixedTxn {
            inner: self.inner.begin(read_version).await?,
            root: self.root.clone(),
        }))
    }

    async fn watch(&self, key: &[u8]) -> Result<Watch> {
        self.inner.watch(&join(&self.root, key)).await
    }

    async fn now_version(&self) -> Result<Version> {
        self.inner.now_version().await
    }

    async fn advance_version(&self, at_least: Version) -> Result<()> {
        self.inner.advance_version(at_least).await
    }
}

struct PrefixedTxn {
    inner: Box<dyn Txn>,
    root: Arc<[u8]>,
}

impl PrefixedTxn {
    fn k(&self, key: &[u8]) -> Vec<u8> {
        join(&self.root, key)
    }

    fn strip(&self, kvs: Vec<KeyValue>) -> Vec<KeyValue> {
        let n = self.root.len();
        kvs.into_iter()
            .map(|(mut k, v)| {
                k.drain(..n);
                (k, v)
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Txn for PrefixedTxn {
    fn read_version(&self) -> Version {
        self.inner.read_version()
    }

    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let k = self.k(key);
        self.inner.get(&k).await
    }

    async fn snapshot_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let k = self.k(key);
        self.inner.snapshot_get(&k).await
    }

    async fn get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        let (b, e) = (self.k(begin), self.k(end));
        let got = self.inner.get_range(&b, &e, limit, reverse).await?;
        Ok(self.strip(got))
    }

    async fn snapshot_get_range(
        &mut self,
        begin: &[u8],
        end: &[u8],
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<KeyValue>> {
        let (b, e) = (self.k(begin), self.k(end));
        let got = self
            .inner
            .snapshot_get_range(&b, &e, limit, reverse)
            .await?;
        Ok(self.strip(got))
    }

    fn add_read_conflict_range(&mut self, begin: &[u8], end: &[u8]) {
        let (b, e) = (self.k(begin), self.k(end));
        self.inner.add_read_conflict_range(&b, &e);
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        let k = self.k(key);
        self.inner.set(&k, value);
    }

    fn clear(&mut self, key: &[u8]) {
        let k = self.k(key);
        self.inner.clear(&k);
    }

    fn clear_range(&mut self, begin: &[u8], end: &[u8]) {
        let (b, e) = (self.k(begin), self.k(end));
        self.inner.clear_range(&b, &e);
    }

    fn set_versionstamped_key(&mut self, prefix: &[u8], suffix: &[u8], value: &[u8]) {
        let p = self.k(prefix);
        self.inner.set_versionstamped_key(&p, suffix, value);
    }

    fn set_versionstamped_value(&mut self, key: &[u8], prefix: &[u8], suffix: &[u8]) {
        let k = self.k(key);
        self.inner.set_versionstamped_value(&k, prefix, suffix);
    }

    fn atomic_add(&mut self, key: &[u8], delta: i64) {
        let k = self.k(key);
        self.inner.atomic_add(&k, delta);
    }

    async fn commit(self: Box<Self>) -> Result<Stamp> {
        self.inner.commit().await
    }
}
