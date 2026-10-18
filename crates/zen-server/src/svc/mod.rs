//! Relayed services (spec/svc.md, api.md §14): zen-serve as a blind TCP
//! relay. This node accepts connections on the services' ports and stores
//! their input in storage; handlers on member clients claim them.
//!
//! * [`listen`]: admission and the accept transaction
//! * [`conn`]: the task that owns a socket and appends its input
//! * [`node`]: the node lease and the node's work queue
//! * [`reap`]: reaping connections, ending leases, the sweeper
//! * [`api`]: the `/v1/svc/*` endpoints
//! * [`step`]: the `svc` commit step
//! * [`reveal`]: releasing output records by their keys

pub mod api;
pub mod conn;
pub mod listen;
pub mod node;
pub mod reap;
pub mod reveal;
pub mod step;

use crate::config::{Config, ServiceConfig};
use crate::error::{ApiResult, claim_lost, conn_closed, internal};
use crate::keys;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use zen_proto::{ByteBuf, SvcClaim, SvcPeer, SvcPending};
use zen_store::Txn;

pub use reap::sweep;

/// A connection, lease or node id.
pub type Id = [u8; 16];

/// `ra` flags (keyspace.md §3.9).
pub const RA_FIN: u8 = 1;
/// Closed by a record.
pub const RA_CLOSE: u8 = 2;
/// Aborted by a record.
pub const RA_ABORT: u8 = 4;
/// A step made the connection resumable.
pub const RA_RESUMABLE: u8 = 8;
/// A step turned input sealing off.
pub const RA_UNSEAL: u8 = 16;
/// `rh` flag: the peer sent FIN.
pub const RH_EOF: u8 = 1;

/// `rz` reasons.
pub const RZ_BAD_RECORD: u8 = 1;
/// A requeue found the connection not resumable.
pub const RZ_NOT_RESUMABLE: u8 = 2;
/// Released with `close`.
pub const RZ_RELEASED: u8 = 3;

/// The node lease's lifetime and renewal period.
pub const NODE_LEASE_SECS: u64 = 10;

/// A command for a local connection task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Something changed: re-check the window and the output.
    Wake,
    /// Reset the socket and reap the connection.
    Abort,
}

struct Local {
    tx: mpsc::Sender<Cmd>,
    task: AbortHandle,
}

/// A configured service on this node.
pub struct Service {
    /// Its configuration.
    pub cfg: ServiceConfig,
    /// Open connections on this node.
    open: AtomicUsize,
    /// Per-address token buckets.
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
    /// The bound address (`tcp`).
    addr: Mutex<Option<SocketAddr>>,
}

impl Service {
    fn new(cfg: ServiceConfig) -> Self {
        Service {
            cfg,
            open: AtomicUsize::new(0),
            buckets: Mutex::new(HashMap::new()),
            addr: Mutex::new(None),
        }
    }

    /// Take one accept from `ip`'s bucket (svc.md §3, admission).
    fn take(&self, ip: IpAddr, now: Instant) -> bool {
        let (rate, burst) = (
            f64::from(self.cfg.per_ip_rate),
            f64::from(self.cfg.per_ip_burst),
        );
        if self.cfg.per_ip_rate == 0 {
            return true;
        }
        let mut b = self.buckets.lock().expect("bucket lock");
        if b.len() > 4096 {
            // Forget the full buckets: they behave like new ones.
            b.retain(|_, (tokens, at)| {
                *tokens + rate * now.duration_since(*at).as_secs_f64() < burst
            });
        }
        let (tokens, at) = b.entry(ip).or_insert((burst, now));
        *tokens = (*tokens + rate * now.duration_since(*at).as_secs_f64()).min(burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Count an open connection, unless at `max_conns`.
    fn open(&self) -> bool {
        if self.open.fetch_add(1, Ordering::AcqRel) < self.cfg.max_conns as usize {
            true
        } else {
            self.open.fetch_sub(1, Ordering::AcqRel);
            false
        }
    }

    fn close(&self) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The relay hub of this node.
pub struct Svc {
    /// This node's id, new at each start.
    pub node: Id,
    services: HashMap<(u32, Vec<u8>), Arc<Service>>,
    conns: Mutex<HashMap<Id, Local>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl Svc {
    /// The hub for `cfg`'s services.
    pub fn new(cfg: &Config) -> Self {
        let mut node = [0u8; 16];
        getrandom::fill(&mut node).expect("OS RNG");
        Svc {
            node,
            services: cfg
                .service
                .iter()
                .map(|s| ((s.fs, s.id.clone()), Arc::new(Service::new(s.clone()))))
                .collect(),
            conns: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// A configured service.
    pub fn service(&self, fs: u32, id: &[u8]) -> Option<&Arc<Service>> {
        self.services.get(&(fs, id.to_vec()))
    }

    /// Every configured service.
    pub fn services(&self) -> impl Iterator<Item = &Arc<Service>> {
        self.services.values()
    }

    /// The address a `tcp` service is bound to on this node.
    pub fn local_addr(&self, name: &str) -> Option<SocketAddr> {
        self.services
            .values()
            .find(|s| s.cfg.name == name)
            .and_then(|s| *s.addr.lock().expect("addr lock"))
    }

    /// Open connections of this node.
    pub fn open_conns(&self) -> usize {
        self.conns.lock().expect("conns lock").len()
    }

    /// Keep a background task, so [`Svc::abort`] stops it.
    pub(crate) fn keep(&self, task: AbortHandle) {
        self.tasks.lock().expect("tasks lock").push(task);
    }

    /// Spawn a connection task and register it under `conn`; `f` gets the
    /// receiving end of its commands.
    pub(crate) fn spawn_conn<F>(&self, conn: Id, f: impl FnOnce(mpsc::Receiver<Cmd>) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(8);
        // Registered under the lock, so the task can't unregister first.
        let mut m = self.conns.lock().expect("conns lock");
        let task = tokio::spawn(f(rx)).abort_handle();
        m.insert(conn, Local { tx, task });
    }

    pub(crate) fn unregister(&self, conn: &Id) {
        self.conns.lock().expect("conns lock").remove(conn);
    }

    /// Send `cmd` to a local connection, if it is on this node.
    pub fn send(&self, conn: &Id, cmd: Cmd) {
        if let Some(l) = self.conns.lock().expect("conns lock").get(conn) {
            // A full queue already holds a wake-up; an abort must get through.
            if l.tx.try_send(cmd).is_err() && cmd == Cmd::Abort {
                let tx = l.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Cmd::Abort).await;
                });
            }
        }
    }

    /// Abort every local connection (the node lease was lost).
    pub fn abort_conns(&self) {
        let ids: Vec<Id> = self
            .conns
            .lock()
            .expect("conns lock")
            .keys()
            .copied()
            .collect();
        for c in ids {
            self.send(&c, Cmd::Abort);
        }
    }

    /// Stop every relay task of this node at once (`Server::abort`).
    pub fn abort(&self) {
        for t in self.tasks.lock().expect("tasks lock").drain(..) {
            t.abort();
        }
        for (_, l) in self.conns.lock().expect("conns lock").drain() {
            l.task.abort();
        }
    }
}

// ---- encodings (keyspace.md §3.9)

/// `rm`: a connection's claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRec {
    /// The lease it is claimed under (zeros while unclaimed).
    pub lease: Id,
    /// The holder's device (zeros while unclaimed).
    pub holder: [u8; 32],
    /// The claim token: one more at each claim.
    pub token: u64,
    /// The `rq` versionstamp while queued.
    pub queued: Option<[u8; 12]>,
}

impl ClaimRec {
    /// Decode.
    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() != 56 && v.len() != 68 {
            return None;
        }
        Some(ClaimRec {
            lease: v[..16].try_into().ok()?,
            holder: v[16..48].try_into().ok()?,
            token: u64::from_be_bytes(v[48..56].try_into().ok()?),
            queued: (v.len() == 68).then(|| v[56..].try_into().expect("12")),
        })
    }

    /// Encode a claim (not queued).
    pub fn claimed(lease: &Id, holder: &[u8; 32], token: u64) -> Vec<u8> {
        let mut v = lease.to_vec();
        v.extend_from_slice(holder);
        v.extend_from_slice(&token.to_be_bytes());
        v
    }

    /// Whether a handler holds it.
    pub fn is_claimed(&self) -> bool {
        self.lease != [0; 16]
    }
}

/// Queue a connection (accept, requeue): `rm = zeros ‖ token ‖ vs` and
/// `rq(vs) = conn`, under the commit's versionstamp with user version `uv`.
pub(crate) fn enqueue(t: &mut Box<dyn Txn>, fs: u32, svc: &[u8], conn: &Id, token: u64, uv: u16) {
    let mut prefix = vec![0u8; 48];
    prefix.extend_from_slice(&token.to_be_bytes());
    t.set_versionstamped_value(&keys::conn("rm", fs, svc, conn), &prefix, &uv.to_be_bytes());
    let (p, s) = keys::svc_queue(fs, svc)
        .vs_incomplete(uv)
        .finish_incomplete();
    t.set_versionstamped_key(&p, &s, conn);
    t.set_versionstamped_value(&keys::svc_queue_bell(fs, svc), &[], &[]);
}

/// Ring a doorbell.
pub(crate) fn ring(t: &mut Box<dyn Txn>, bell: &[u8]) {
    t.set_versionstamped_value(bell, &[], &[]);
}

/// `ra`: the step cursors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Acks {
    /// Input acked.
    pub in_acked: u64,
    /// Start of the oldest input chunk kept.
    pub in_cleared: u64,
    /// Output committed.
    pub out_head: u64,
    /// The state's length.
    pub state_len: u32,
    /// `RA_*`.
    pub flags: u8,
}

impl Acks {
    /// Encoded length.
    pub const LEN: usize = 29;

    /// Decode (missing or short: zeros).
    pub fn decode(v: Option<&[u8]>) -> Self {
        match v {
            Some(v) if v.len() == Self::LEN => Acks {
                in_acked: u64::from_be_bytes(v[..8].try_into().expect("8")),
                in_cleared: u64::from_be_bytes(v[8..16].try_into().expect("8")),
                out_head: u64::from_be_bytes(v[16..24].try_into().expect("8")),
                state_len: u32::from_be_bytes(v[24..28].try_into().expect("4")),
                flags: v[28],
            },
            _ => Acks::default(),
        }
    }

    /// Encode.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::LEN);
        v.extend_from_slice(&self.in_acked.to_be_bytes());
        v.extend_from_slice(&self.in_cleared.to_be_bytes());
        v.extend_from_slice(&self.out_head.to_be_bytes());
        v.extend_from_slice(&self.state_len.to_be_bytes());
        v.push(self.flags);
        v
    }
}

/// `rh`: `(in_head, flags)`.
pub fn decode_head(v: Option<&[u8]>) -> (u64, u8) {
    match v {
        Some(v) if v.len() == 9 => (u64::from_be_bytes(v[..8].try_into().expect("8")), v[8]),
        _ => (0, 0),
    }
}

/// Encode `rh`.
pub fn encode_head(in_head: u64, flags: u8) -> Vec<u8> {
    let mut v = in_head.to_be_bytes().to_vec();
    v.push(flags);
    v
}

/// A `u64` value (`rw`, `rk`); missing or short: 0.
pub fn decode_u64(v: Option<&[u8]>) -> u64 {
    v.and_then(|v| v.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0)
}

/// `rl`: `(holder, expires_version)`.
pub fn decode_lease(v: &[u8]) -> Option<([u8; 32], u64)> {
    (v.len() == 40).then(|| {
        (
            v[..32].try_into().expect("32"),
            u64::from_be_bytes(v[32..].try_into().expect("8")),
        )
    })
}

/// Encode `rl`.
pub fn encode_lease(holder: &[u8; 32], expires: u64) -> Vec<u8> {
    let mut v = holder.to_vec();
    v.extend_from_slice(&expires.to_be_bytes());
    v
}

/// `rnq` value: `u32 fs ‖ lp(svc) ‖ conn`.
pub fn encode_work(fs: u32, svc: &[u8], conn: &Id) -> Vec<u8> {
    let mut v = fs.to_be_bytes().to_vec();
    v.extend_from_slice(&(svc.len() as u32).to_be_bytes());
    v.extend_from_slice(svc);
    v.extend_from_slice(conn);
    v
}

/// Decode an `rnq` value.
pub fn decode_work(v: &[u8]) -> Option<(u32, Vec<u8>, Id)> {
    let fs = u32::from_be_bytes(v.get(..4)?.try_into().ok()?);
    let n = u32::from_be_bytes(v.get(4..8)?.try_into().ok()?) as usize;
    let svc = v.get(8..8 + n)?.to_vec();
    let conn = v.get(8 + n..)?.try_into().ok()?;
    Some((fs, svc, conn))
}

/// `rc`: the connection record, CBOR.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnRec {
    /// The owning node.
    pub node: ByteBuf,
    /// The remote end.
    pub peer: SvcPeer,
    /// Accept time, unix ms.
    pub accepted_ms: u64,
    /// `"tcp"` or `"tcp-auth"`.
    pub mode: String,
    /// Unacked input bytes.
    pub window: u32,
    /// Unwritten output bytes.
    pub out_window: u32,
    /// Release by reveal.
    pub resume: bool,
    /// The inbox ciphertext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<Inbox>,
}

/// `rc.inbox`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    /// Version (1).
    pub v: u8,
    /// The X-Wing ciphertext.
    pub ct: ByteBuf,
}

impl ConnRec {
    /// Decode.
    pub fn decode(v: &[u8]) -> ApiResult<Self> {
        zen_proto::from_cbor(v).map_err(|_| internal("bad connection record"))
    }

    /// The owning node.
    pub fn node(&self) -> Option<Id> {
        self.node.as_slice().try_into().ok()
    }
}

/// A 16-byte id from a request.
pub fn parse_id(b: &[u8], what: &str) -> ApiResult<Id> {
    b.try_into()
        .map_err(|_| crate::error::bad_request(format!("{what} must be 16 bytes")))
}

/// Check that the caller holds `conn` with `token` (api.md §14.4): `rm` is
/// read normally, so a concurrent claim or reap conflicts.
pub async fn check_claim(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    token: u64,
    device: &[u8; 32],
) -> ApiResult<ClaimRec> {
    let rm = t
        .get(&keys::conn("rm", fs, svc, conn))
        .await?
        .as_deref()
        .and_then(ClaimRec::decode)
        .ok_or_else(|| conn_closed("the connection is closed"))?;
    if t.get(&keys::conn("rz", fs, svc, conn)).await?.is_some() {
        return Err(conn_closed("the connection is being aborted"));
    }
    if rm.token != token || rm.holder != *device || !rm.is_claimed() {
        return Err(claim_lost("claim token is not current"));
    }
    Ok(rm)
}

/// Build the `Claim` of a connection the caller holds (api.md §14.3): `rc`
/// and `ra` read normally, `rh` and `rw` by snapshot.
pub async fn claim_of(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    token: u64,
) -> ApiResult<SvcClaim> {
    let rc = t
        .get(&keys::conn("rc", fs, svc, conn))
        .await?
        .ok_or_else(|| conn_closed("the connection is closed"))?;
    let rc = ConnRec::decode(&rc)?;
    let ra = Acks::decode(t.get(&keys::conn("ra", fs, svc, conn)).await?.as_deref());
    let (in_head, eof) = decode_head(
        t.snapshot_get(&keys::conn("rh", fs, svc, conn))
            .await?
            .as_deref(),
    );
    let written_to = decode_u64(
        t.snapshot_get(&keys::conn("rw", fs, svc, conn))
            .await?
            .as_deref(),
    );
    let state = t.get(&keys::conn("rs", fs, svc, conn)).await?;
    let pending = pending(t, fs, svc, conn, written_to).await?;
    Ok(SvcClaim {
        conn: conn.to_vec(),
        token,
        peer: rc.peer,
        accepted_ms: rc.accepted_ms,
        in_acked: ra.in_acked,
        in_head,
        out_head: ra.out_head,
        written_to,
        flags: flags(eof, ra.flags),
        state,
        inbox_ct: rc.inbox.map(|i| i.ct.into_vec()),
        pending,
    })
}

/// `Claim.flags`: `rh` EOF | `ra` flags << 1.
pub fn flags(rh_flags: u8, ra_flags: u8) -> u8 {
    (rh_flags & RH_EOF) | (ra_flags << 1)
}

/// The escrows of committed records from `written_to` whose key is not
/// revealed, in offset order. An empty record that ends the output sits at
/// `out_head` itself.
async fn pending(
    t: &mut Box<dyn Txn>,
    fs: u32,
    svc: &[u8],
    conn: &Id,
    written_to: u64,
) -> ApiResult<Vec<SvcPending>> {
    let ro = keys::conn_key("ro", fs, svc, conn);
    let begin = ro.clone().int(written_to as i64).finish();
    let end = keys::end_of(&ro.finish());
    let recs = t.get_range(&begin, &end, 100_000, false).await?;
    let mut out = Vec::new();
    for (k, v) in recs {
        let off = tail_int(&k).ok_or_else(|| internal("bad record key"))?;
        if t.get(&keys::conn_key("rv", fs, svc, conn).int(off as i64).finish())
            .await?
            .is_some()
        {
            continue;
        }
        if let Ok(r) = zen_core::svc::parse_record(&v)
            && let Some(e) = r.escrow
        {
            out.push(SvcPending {
                off,
                escrow: e.to_vec(),
            });
        }
    }
    Ok(out)
}

/// The trailing integer element of a key (an `ri`/`ro`/`rv` offset).
pub fn tail_int(k: &[u8]) -> Option<u64> {
    use zen_store::tuple::{Elem, unpack};
    match unpack(k).ok()?.pop()? {
        Elem::Int(i) => u64::try_from(i).ok(),
        _ => None,
    }
}

/// Unix milliseconds now.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
