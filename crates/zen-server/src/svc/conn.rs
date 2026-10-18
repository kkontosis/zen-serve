//! A relayed connection's task, which owns its socket (svc.md §3): it
//! accepts the connection in storage, appends the input in batches, and
//! reaps the connection when the socket closes.

use super::listen::reset;
use super::reap::{reap, reap_unclaimed};
use super::{
    Acks, ClaimRec, Cmd, ConnRec, Id, RH_EOF, Service, decode_head, encode_head, enqueue, ring,
    unix_ms,
};
use crate::error::ApiResult;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zen_proto::{ByteBuf, SvcPeer};

/// The most input one batch carries.
pub const BATCH: usize = 32 * 1024;
/// How long a batch gathers input after its first bytes.
const GATHER: Duration = Duration::from_millis(5);
/// How often a blocked reader re-checks the window, the quota and storage.
const RECHECK: Duration = Duration::from_secs(1);

/// Accept `sock` as a new connection of `svc` and run it.
pub fn spawn(st: &Shared, svc: Arc<Service>, sock: TcpStream, peer: SvcPeer) {
    let mut conn = [0u8; 16];
    getrandom::fill(&mut conn).expect("OS RNG");
    let st2 = st.clone();
    st.svc.spawn_conn(conn, move |rx| async move {
        let _guard = Guard {
            st: st2.clone(),
            svc: svc.clone(),
            conn,
        };
        let c = Conn {
            fs: svc.cfg.fs,
            id: svc.cfg.id.clone(),
            st: st2,
            svc,
            conn,
        };
        c.run(sock, peer, rx).await;
    });
}

/// Unregisters the task and frees its `max_conns` slot, however it ends.
struct Guard {
    st: Shared,
    svc: Arc<Service>,
    conn: Id,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.st.svc.unregister(&self.conn);
        self.svc.close();
    }
}

struct Conn {
    st: Shared,
    svc: Arc<Service>,
    fs: u32,
    id: Vec<u8>,
    conn: Id,
}

/// What a batch's transaction found.
enum Outcome {
    /// Appended; whether a handler has ever claimed the connection.
    Done(bool),
    /// The fs is over its byte quota: hold the batch.
    Over,
    /// Reaped elsewhere, or an abort is requested.
    Gone,
}

impl Conn {
    async fn run(&self, mut sock: TcpStream, peer: SvcPeer, mut rx: mpsc::Receiver<Cmd>) {
        if let Err(e) = self.accept(peer).await {
            tracing::warn!(service = %self.svc.cfg.name, error = %e.message, "accept failed");
            reset(sock);
            self.reap().await;
            return;
        }
        let cfg = &self.svc.cfg;
        let window = u64::from(cfg.window);
        let accept_by = Instant::now() + Duration::from_millis(cfg.accept_wait_ms.into());
        let (mut in_head, mut in_acked) = (0u64, 0u64);
        let (mut eof, mut claimed) = (false, false);
        // Input read but not stored yet (the quota was full, or storage
        // failed): no more is read until it is.
        let mut held: Option<Vec<u8>> = None;
        let mut buf = vec![0u8; BATCH];
        loop {
            let room = window.saturating_sub(in_head - in_acked).min(BATCH as u64) as usize;
            let can_read = !eof && held.is_none() && room > 0;
            let blocked = !eof && !can_read;
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Wake) => {
                        if blocked {
                            in_acked = self.in_acked().await.unwrap_or(in_acked);
                        }
                    }
                    Some(Cmd::Abort) | None => {
                        reset(sock);
                        self.reap().await;
                        return;
                    }
                },
                r = sock.read(&mut buf[..room]), if can_read => match r {
                    Ok(0) => {
                        eof = true;
                        held = Some(Vec::new());
                    }
                    Ok(n) => {
                        // Gather more for a moment; not cancellable, so no
                        // input read is ever dropped.
                        let mut len = n;
                        let until = Instant::now() + GATHER;
                        while len < room {
                            match tokio::time::timeout_at(until, sock.read(&mut buf[len..room])).await {
                                Ok(Ok(0)) => {
                                    eof = true;
                                    break;
                                }
                                Ok(Ok(m)) => len += m,
                                Ok(Err(_)) => {
                                    // Store what came, then end on the next read.
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        held = Some(buf[..len].to_vec());
                    }
                    Err(_) => {
                        // A reset or a failed read: the connection is over.
                        self.reap().await;
                        return;
                    }
                },
                _ = tokio::time::sleep_until(accept_by), if !claimed => {
                    match reap_unclaimed(&self.st, self.fs, &self.id, &self.conn).await {
                        Ok(true) => {
                            tracing::debug!(service = %cfg.name, "no handler claimed the connection");
                            reset(sock);
                            return;
                        }
                        Ok(false) => claimed = true,
                        Err(e) => tracing::warn!(error = %e.message, "reap"),
                    }
                },
                _ = tokio::time::sleep(RECHECK), if blocked => {
                    in_acked = self.in_acked().await.unwrap_or(in_acked);
                },
            }
            // Store held input (or the EOF) as one batch.
            if let Some(data) = held.take() {
                match self.append(in_head, &data, eof).await {
                    Ok(Outcome::Done(c)) => {
                        in_head += data.len() as u64;
                        claimed |= c;
                    }
                    Ok(Outcome::Over) => held = Some(data),
                    Ok(Outcome::Gone) => {
                        reset(sock);
                        self.reap().await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e.message, "storing relayed input");
                        held = Some(data);
                    }
                }
                if eof && held.is_some() {
                    // Not stored: read again once the batch is, and the
                    // socket reports the EOF again.
                    eof = false;
                }
            }
        }
    }

    /// The accept transaction (svc.md §3, step 2).
    async fn accept(&self, peer: SvcPeer) -> ApiResult<()> {
        let cfg = &self.svc.cfg;
        let rc = zen_proto::to_cbor(&ConnRec {
            node: ByteBuf::from(self.st.svc.node.to_vec()),
            peer,
            accepted_ms: unix_ms(),
            mode: cfg.mode.name().into(),
            window: cfg.window,
            out_window: cfg.out_window,
            resume: cfg.resume,
            inbox: None,
        });
        let (fs, svc, conn) = (self.fs, &self.id[..], &self.conn);
        txn_loop!(self.st.store, None, |t| {
            t.set(&keys::conn("rc", fs, svc, conn), &rc);
            t.set(&keys::conn("rh", fs, svc, conn), &encode_head(0, 0));
            t.set(&keys::conn("ra", fs, svc, conn), &Acks::default().encode());
            t.set(&keys::conn("rw", fs, svc, conn), &0u64.to_be_bytes());
            enqueue(&mut t, fs, svc, conn, 0, 0);
            t.set(
                &keys::node_conns(&self.st.svc.node)
                    .int(fs.into())
                    .bytes(svc)
                    .bytes(conn)
                    .finish(),
                &[],
            );
            Ok(())
        })?;
        Ok(())
    }

    /// Append a batch at `in_head` (svc.md §3, step 3). After an unknown
    /// commit result, `rh` tells whether it applied, so a batch is never
    /// appended twice.
    async fn append(&self, in_head: u64, data: &[u8], eof: bool) -> ApiResult<Outcome> {
        let (fs, svc, conn) = (self.fs, &self.id[..], &self.conn);
        let max_bytes = self.st.acl().limits.get(&fs).and_then(|l| l.max_bytes);
        let len = data.len() as u64;
        loop {
            let r = txn_loop!(self.st.store, None, |t| {
                if let (Some(max), true) = (max_bytes, len > 0) {
                    let cur = t
                        .snapshot_get(&keys::quota(fs, "bytes"))
                        .await?
                        .map(|v| i64::from_le_bytes(v.try_into().unwrap_or_default()))
                        .unwrap_or(0);
                    if cur.saturating_add(len as i64) > max as i64 {
                        return Ok(Outcome::Over);
                    }
                }
                // Read normally: a reap or a claim conflicts with the batch.
                let Some(rm) = t
                    .get(&keys::conn("rm", fs, svc, conn))
                    .await?
                    .as_deref()
                    .and_then(ClaimRec::decode)
                else {
                    return Ok(Outcome::Gone);
                };
                if t.snapshot_get(&keys::conn("rz", fs, svc, conn))
                    .await?
                    .is_some()
                {
                    return Ok(Outcome::Gone);
                }
                if len > 0 {
                    let mut v = Vec::with_capacity(data.len() + 1);
                    v.push(0);
                    v.extend_from_slice(data);
                    t.set(
                        &keys::conn_key("ri", fs, svc, conn)
                            .int(in_head as i64)
                            .finish(),
                        &v,
                    );
                    t.atomic_add(&keys::quota(fs, "bytes"), len as i64);
                }
                t.set(
                    &keys::conn("rh", fs, svc, conn),
                    &encode_head(in_head + len, if eof { RH_EOF } else { 0 }),
                );
                if rm.is_claimed() {
                    ring(&mut t, &keys::svc_lease_bell(fs, svc, &rm.lease));
                }
                Ok(Outcome::Done(rm.token > 0))
            });
            match r {
                Ok((o, _)) => return Ok(o),
                Err(e) if e.code == "commit_unknown" => {
                    let (head, flags) = txn_loop!(self.st.store, None, |t| {
                        Ok(decode_head(
                            t.get(&keys::conn("rh", fs, svc, conn)).await?.as_deref(),
                        ))
                    })?
                    .0;
                    let want = if eof { RH_EOF } else { 0 };
                    if head == in_head + len && (len > 0 || flags & RH_EOF == want) {
                        return Ok(Outcome::Done(false));
                    }
                    if head != in_head {
                        return Ok(Outcome::Gone);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `in_acked`, by snapshot (the steps own `ra`).
    async fn in_acked(&self) -> ApiResult<u64> {
        let k = keys::conn("ra", self.fs, &self.id, &self.conn);
        let (ra, _) = txn_loop!(self.st.store, None, |t| {
            Ok(Acks::decode(t.snapshot_get(&k).await?.as_deref()))
        })?;
        Ok(ra.in_acked)
    }

    /// Reap, retrying a few times: until it succeeds, only the node's own
    /// death would clean the connection up.
    async fn reap(&self) {
        for attempt in 0..5 {
            match reap(&self.st, self.fs, &self.id, &self.conn).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(error = %e.message, attempt, "reaping a relayed connection");
                    tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
                }
            }
        }
    }
}
