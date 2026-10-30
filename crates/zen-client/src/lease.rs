//! Named, fenced leases (spec/api.md §16): one holder at a time, and a
//! token that grows with every new holder, which commits carry as a fence.

use crate::error::Result;
use crate::fs::Fs;
use zen_proto::{NamedLease, NamedLeasePut};

/// A held lease.
#[derive(Clone)]
pub struct Lease {
    fs: Fs,
    name: Vec<u8>,
    ttl_ms: u32,
    token: u64,
    expires_version: u64,
}

impl Lease {
    /// Acquire lease `name` (1–64 bytes) of `fs` for `ttl_ms`: 412
    /// `not_leader` while another device holds it.
    pub async fn acquire(fs: &Fs, name: &[u8], ttl_ms: u32) -> Result<Lease> {
        let r = put(fs, name, None, ttl_ms, false).await?;
        Ok(Lease {
            fs: fs.clone(),
            name: name.to_vec(),
            ttl_ms,
            token: r.token,
            expires_version: r.expires_version,
        })
    }

    /// Renew it, keeping the token (412 `not_leader` once lost).
    pub async fn renew(&mut self) -> Result<()> {
        let r = put(&self.fs, &self.name, Some(self.token), self.ttl_ms, false).await?;
        self.token = r.token;
        self.expires_version = r.expires_version;
        Ok(())
    }

    /// Release it: the next holder gets the next token at once.
    pub async fn release(self) -> Result<()> {
        put(&self.fs, &self.name, Some(self.token), self.ttl_ms, true).await?;
        Ok(())
    }

    /// The lease name.
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// The fencing token.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// The commit version it expires at.
    pub fn expires_version(&self) -> u64 {
        self.expires_version
    }
}

async fn put(
    fs: &Fs,
    name: &[u8],
    token: Option<u64>,
    ttl_ms: u32,
    release: bool,
) -> Result<NamedLease> {
    fs.session()
        .call(
            "/v1/lease",
            &NamedLeasePut {
                fs: fs.id(),
                name: name.to_vec(),
                token,
                ttl_ms: Some(ttl_ms),
                release,
            },
        )
        .await
}
