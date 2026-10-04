//! The signed ACL: parsing, validation (formats.md §9.3), enforcement, and
//! the `/v1/acl/*` and `/v1/fs/*` endpoints.

use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use std::collections::{HashMap, HashSet};
use zen_core::kdf::acl_hash;
use zen_core::labels;
use zen_core::sig::{PublicIdentity, verify_device_cert};
use zen_proto::acl::{AclDoc, FS_RIGHTS, FsLimit, MAX_ACL_ORIGINS, SignedAcl, TOPIC_RIGHTS};
use zen_proto::{
    AclEntries, AclGet, AclPut, AclVersion, ByteBuf, Empty, FsEntry, FsList, Header, HeaderGet,
    HeaderPut, HeaderVersion, from_cbor,
};
use zen_store::Storage;

/// Read KV / read events.
pub const R_READ: u8 = 1;
/// Write KV.
pub const R_WRITE: u8 = 2;
/// Append events.
pub const R_APPEND: u8 = 4;
/// Consume events (lead a consumer group).
pub const R_CONSUME: u8 = 8;

/// A 32-byte fingerprint.
pub type Fp = [u8; 32];

fn right_bit(r: &str) -> u8 {
    match r {
        "read" => R_READ,
        "write" => R_WRITE,
        "append" => R_APPEND,
        "consume" => R_CONSUME,
        _ => 0,
    }
}

/// A member.
pub struct MemberInfo {
    /// The user's public identity.
    pub identity: PublicIdentity,
    /// Fingerprints of the certified devices.
    pub devices: HashSet<Fp>,
}

/// A topic grant.
pub struct TopicGrant {
    user: Fp,
    fs: u32,
    prefix: Vec<u8>,
    rights: u8,
}

/// The current ACL, parsed for enforcement.
#[derive(Default)]
pub struct AclState {
    /// Version (0 = unclaimed).
    pub version: u64,
    /// Encoded doc of this version (for `prev_hash`).
    pub doc_bytes: Vec<u8>,
    /// Admins.
    pub admins: HashSet<Fp>,
    /// Members by user fingerprint.
    pub members: HashMap<Fp, MemberInfo>,
    fs_grants: HashMap<(Fp, u32), u8>,
    topic_grants: Vec<TopicGrant>,
    /// Quotas.
    pub limits: HashMap<u32, FsLimit>,
    /// Origins the admins vouch for (spec/auth.md §5.3).
    pub origins: Vec<String>,
}

impl AclState {
    /// Rights of `user` on `fs`.
    pub fn fs_rights(&self, user: &Fp, fs: u32) -> u8 {
        self.fs_grants.get(&(*user, fs)).copied().unwrap_or(0)
    }

    /// Rights of `user` on a topic (or on every topic under a prefix).
    pub fn topic_rights(&self, user: &Fp, fs: u32, topic: &[u8]) -> u8 {
        self.topic_grants
            .iter()
            .filter(|g| g.user == *user && g.fs == fs && topic.starts_with(&g.prefix))
            .fold(0, |acc, g| acc | g.rights)
    }

    /// Whether `user` has any topic grant in `fs`.
    pub fn has_topic_grants(&self, user: &Fp, fs: u32) -> bool {
        self.topic_grants
            .iter()
            .any(|g| g.user == *user && g.fs == fs)
    }

    /// Whether `device` of `user` is in this ACL.
    pub fn has_device(&self, user: &Fp, device: &Fp) -> bool {
        self.members
            .get(user)
            .is_some_and(|m| m.devices.contains(device))
    }

    /// Build from a doc, validating it (rule 3).
    pub fn from_doc(
        doc_bytes: &[u8],
        is_fs: &(dyn Fn(u32) -> bool + Sync),
    ) -> ApiResult<(Self, AclDoc)> {
        let doc: AclDoc = from_cbor(doc_bytes).map_err(|e| bad_request(format!("ACL doc: {e}")))?;
        let fp = |b: &[u8]| -> ApiResult<Fp> {
            b.try_into()
                .map_err(|_| bad_request("fingerprints are 32 bytes"))
        };
        let mut members = HashMap::new();
        for m in &doc.members {
            let identity = PublicIdentity::decode(&m.identity)
                .map_err(|_| bad_request("bad member identity"))?;
            let user = identity.fingerprint();
            let mut devices = HashSet::new();
            for cert in &m.devices {
                let (device, _) = verify_device_cert(&identity, cert)
                    .map_err(|_| bad_request("device certificate does not verify"))?;
                devices.insert(device.fingerprint());
            }
            if members
                .insert(user, MemberInfo { identity, devices })
                .is_some()
            {
                return Err(bad_request("duplicate member"));
            }
        }
        let mut admins = HashSet::new();
        for a in &doc.admins {
            let a = fp(a)?;
            if !members.contains_key(&a) {
                return Err(bad_request("every admin must be a member"));
            }
            admins.insert(a);
        }
        if admins.is_empty() {
            return Err(bad_request("an ACL needs at least one admin"));
        }
        let mut fs_grants = HashMap::new();
        let mut topic_grants = Vec::new();
        for g in &doc.grants {
            let user = fp(&g.subject)?;
            if !members.contains_key(&user) {
                return Err(bad_request("grant subject is not a member"));
            }
            if g.fs == 0 || !is_fs(g.fs) {
                return Err(bad_request(format!("grant for unknown fs {}", g.fs)));
            }
            let allowed = if g.topic.is_some() {
                TOPIC_RIGHTS
            } else {
                FS_RIGHTS
            };
            let mut bits = 0;
            for r in &g.rights {
                if !allowed.contains(&r.as_str()) {
                    return Err(bad_request(format!("invalid right {r:?}")));
                }
                bits |= right_bit(r);
            }
            match &g.topic {
                Some(prefix) => {
                    crate::ids::check_prefix(prefix)?;
                    topic_grants.push(TopicGrant {
                        user,
                        fs: g.fs,
                        prefix: prefix.clone(),
                        rights: bits,
                    });
                }
                None => *fs_grants.entry((user, g.fs)).or_insert(0) |= bits,
            }
        }
        if doc.origins.len() > MAX_ACL_ORIGINS {
            return Err(bad_request(format!(
                "an ACL lists at most {MAX_ACL_ORIGINS} origins"
            )));
        }
        let mut origins = Vec::new();
        for o in &doc.origins {
            if !zen_proto::valid_origin(o) {
                return Err(bad_request(format!("invalid origin {o:?}")));
            }
            if origins.contains(o) {
                return Err(bad_request(format!("duplicate origin {o:?}")));
            }
            origins.push(o.clone());
        }
        let mut limits = HashMap::new();
        for l in &doc.limits {
            if l.fs == 0 || !is_fs(l.fs) {
                return Err(bad_request(format!("limit for unknown fs {}", l.fs)));
            }
            limits.insert(l.fs, l.clone());
        }
        Ok((
            AclState {
                version: doc.version,
                doc_bytes: doc_bytes.to_vec(),
                admins,
                members,
                fs_grants,
                topic_grants,
                limits,
                origins,
            },
            doc,
        ))
    }
}

/// Validate a signed ACL as the successor of `head` (formats.md §9.3).
///
/// The cheap checks come first (chain position, then one signature by an
/// admin of the head); only a doc that passes them has every device
/// certificate verified. `/v1/acl/put` needs no session, so this keeps an
/// unauthenticated caller from making the server verify thousands of
/// certificates per request.
pub fn validate_successor(
    signed: &[u8],
    head: &AclState,
    is_fs: &(dyn Fn(u32) -> bool + Sync),
) -> ApiResult<AclState> {
    let s: SignedAcl = from_cbor(signed).map_err(|e| bad_request(format!("signed ACL: {e}")))?;
    let doc: AclDoc = from_cbor(&s.doc).map_err(|e| bad_request(format!("ACL doc: {e}")))?;
    if doc.version != head.version + 1 {
        return Err(version_mismatch(format!(
            "ACL version must be {}",
            head.version + 1
        )));
    }
    let want_prev = if head.version == 0 {
        [0u8; 32]
    } else {
        acl_hash(&head.doc_bytes)
    };
    if doc.prev_hash != want_prev {
        return Err(version_mismatch("prev_hash does not match the head ACL"));
    }
    let signer: Fp = s
        .signer
        .as_slice()
        .try_into()
        .map_err(|_| bad_request("signer must be 32 bytes"))?;
    let not_admin = || forbidden("ACL signer is not an admin");
    let identity = if head.version == 0 {
        // Version 1 is signed by an admin of itself (plus the claim token).
        if !doc.admins.iter().any(|a| a.as_slice() == signer) {
            return Err(not_admin());
        }
        doc.members
            .iter()
            .filter_map(|m| PublicIdentity::decode(&m.identity).ok())
            .find(|i| i.fingerprint() == signer)
            .ok_or_else(not_admin)?
    } else {
        if !head.admins.contains(&signer) {
            return Err(not_admin());
        }
        head.members[&signer].identity.clone()
    };
    identity
        .verify(labels::SIG_ACL, &s.doc, &s.sig)
        .map_err(|_| forbidden("ACL signature does not verify"))?;
    Ok(AclState::from_doc(&s.doc, is_fs)?.0)
}

/// Load the current ACL from storage (trusted: it was validated on write).
pub async fn load(
    store: &dyn Storage,
    is_fs: &(dyn Fn(u32) -> bool + Sync),
) -> ApiResult<AclState> {
    // Retried: FoundationDB answers `too_old` during recoveries.
    let (signed, _) = txn_loop!(store, None, |t| {
        let Some(head) = t.get(&keys::acl_head()).await? else {
            return Ok(None);
        };
        let v = u64::from_be_bytes(head.try_into().map_err(|_| internal("bad acl_head"))?);
        Ok(Some(
            t.get(&keys::acl(v))
                .await?
                .ok_or_else(|| internal("missing ACL"))?,
        ))
    })?;
    let Some(signed) = signed else {
        return Ok(AclState::default());
    };
    let s: SignedAcl = from_cbor(&signed).map_err(internal)?;
    Ok(AclState::from_doc(&s.doc, is_fs)?.0)
}

/// `POST /v1/acl/put`.
pub async fn put(State(st): State<Shared>, Cbor(req): Cbor<AclPut>) -> ApiResult<Cbor<AclVersion>> {
    let head = st.acl();
    if head.version == 0 {
        // Checked before anything costly: this request needs no session.
        let ok = {
            let claim = st.claim.lock().expect("claim lock");
            match (&*claim, &req.claim) {
                (Some(want), Some(got)) => constant_time_eq(want.as_bytes(), got.as_bytes()),
                _ => false,
            }
        };
        if !ok {
            return Err(forbidden("version 1 requires the claim token"));
        }
    }
    let is_fs = |fs| st.cfg.has_fs(fs);
    let new = validate_successor(&req.acl, &head, &is_fs)?;
    let removed: Vec<Fp> = head
        .members
        .keys()
        .filter(|u| !new.members.contains_key(*u))
        .copied()
        .collect();
    // The claim may name the origin to pin (auth.md §5.2).
    let pin = match &req.origin {
        Some(o) if new.version == 1 => {
            crate::origin::check_origin(o)?;
            Some(o.clone())
        }
        _ => None,
    };
    txn_loop!(st.store, None, idempotent, |t| {
        let current = t
            .get(&keys::acl_head())
            .await?
            .map(|b| u64::from_be_bytes(b.try_into().unwrap_or_default()))
            .unwrap_or(0);
        // A retry after an unknown result finds its own write.
        if current == new.version && t.get(&keys::acl(new.version)).await? == Some(req.acl.clone())
        {
            return Ok(());
        }
        if current != head.version {
            return Err(version_mismatch("the ACL changed concurrently"));
        }
        t.set(&keys::acl(new.version), &req.acl);
        t.set(&keys::acl_head(), &new.version.to_be_bytes());
        if let Some(o) = &pin {
            crate::origin::pin_at_claim(&st.cfg, &mut t, o).await?;
        }
        // A member who leaves takes their stored credentials along
        // (auth.md §4).
        for user in &removed {
            crate::cred::delete_user(&mut t, user).await?;
        }
        Ok(())
    })?;
    let version = new.version;
    // Also spends the claim token at version 1.
    st.set_acl(new);
    tracing::info!(version, "ACL updated");
    Ok(Cbor(AclVersion { version }))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `POST /v1/acl/get`.
pub async fn get(
    State(st): State<Shared>,
    _caller: Caller,
    Cbor(req): Cbor<AclGet>,
) -> ApiResult<Cbor<AclEntries>> {
    let head = st.acl().version;
    let from = req.from.unwrap_or(head).max(1);
    if head > 0 && from > head {
        return Err(not_found("no such ACL version"));
    }
    if head.saturating_sub(from) >= 1000 {
        return Err(too_large("request at most 1000 versions"));
    }
    let mut t = st.store.begin(None).await?;
    let mut entries = Vec::new();
    for v in from..=head {
        let e = t
            .get(&keys::acl(v))
            .await?
            .ok_or_else(|| internal("missing ACL version"))?;
        entries.push(ByteBuf::from(e));
    }
    Ok(Cbor(AclEntries { head, entries }))
}

/// `POST /v1/fs/list`.
pub async fn fs_list(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(_): Cbor<Empty>,
) -> ApiResult<Cbor<FsList>> {
    let acl = &caller.acl;
    let admin = acl.admins.contains(&caller.user);
    let fs = st
        .cfg
        .fs
        .iter()
        .filter_map(|f| {
            let bits = acl.fs_rights(&caller.user, f.id);
            let mut rights = Vec::new();
            if bits & R_READ != 0 {
                rights.push("read".to_string());
            }
            if bits & R_WRITE != 0 {
                rights.push("write".to_string());
            }
            if acl.has_topic_grants(&caller.user, f.id) {
                rights.push("topics".to_string());
            }
            if admin {
                rights.push("admin".to_string());
            }
            (!rights.is_empty()).then_some(FsEntry { id: f.id, rights })
        })
        .collect();
    Ok(Cbor(FsList { fs }))
}

/// `POST /v1/fs/header/get`.
pub async fn header_get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<HeaderGet>,
) -> ApiResult<Cbor<Header>> {
    st.check_fs(req.fs)?;
    if !caller.is_admin() && caller.acl.fs_rights(&caller.user, req.fs) & R_READ == 0 {
        return Err(forbidden("no read right on this fs"));
    }
    let mut t = st.store.begin(None).await?;
    Ok(Cbor(match t.get(&keys::header(req.fs)).await? {
        Some(v) if v.len() >= 10 => Header {
            version: Some(v[..10].to_vec()),
            header: Some(v[10..].to_vec()),
        },
        _ => Header {
            header: None,
            version: None,
        },
    }))
}

/// `POST /v1/fs/header/put`.
pub async fn header_put(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<HeaderPut>,
) -> ApiResult<Cbor<HeaderVersion>> {
    st.check_fs(req.fs)?;
    if !caller.is_admin() {
        return Err(forbidden("only admins can change fs headers"));
    }
    if req.header.len() > st.cfg.limits.max_value_bytes as usize {
        return Err(too_large("header too large"));
    }
    let mut t = st.store.begin(None).await?;
    let key = keys::header(req.fs);
    let current = t.get(&key).await?.map(|v| v[..10].to_vec());
    if current != req.expect {
        return Err(version_mismatch("header version mismatch"));
    }
    t.set_versionstamped_value(&key, &[], &req.header);
    let stamp = t.commit().await.map_err(|e| match e {
        zen_store::Error::Conflict => version_mismatch("header changed concurrently"),
        e => e.into(),
    })?;
    Ok(Cbor(HeaderVersion {
        version: stamp.to_vec(),
    }))
}

/// Keep the in-memory ACL in sync with storage (other nodes, M3). A new
/// head also spends this node's claim token when another node was claimed.
pub async fn follow(st: Shared) {
    loop {
        let w = match st.store.watch(&keys::acl_head()).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "ACL watch failed");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let is_fs = |fs| st.cfg.has_fs(fs);
        match load(st.store.as_ref(), &is_fs).await {
            Ok(a) if a.version > st.acl().version => st.set_acl(a),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e.message, "ACL reload failed"),
        }
        w.await;
    }
}
