//! A relayed connection's task, which owns its socket (svc.md §3): it
//! accepts the connection in storage, appends the input in batches, writes
//! released output, keeps the timers, and reaps the connection when the
//! socket closes. The socket is a TCP stream (`tcp`) or a WebSocket from
//! `/v1/svc/connect` (`tcp-auth`, api.md §14.9).

use super::listen::reset;
use super::reap::{reap, reap_unclaimed};
use super::{
    Acks, ClaimRec, Cmd, ConnRec, Id, Inbox, RA_UNSEAL, RH_EOF, Service, Slot, decode_head,
    decode_u64, encode_head, enqueue, log_input, ring, tail_int, unix_ms,
};
use crate::error::{ApiError, ApiResult, internal};
use crate::keys::{self, end_of};
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use zen_core::rng::OsRng;
use zen_core::svc::{ABORT, CLOSE, FIN, InboxKey, inbox_encaps, parse_record};
use zen_proto::{ByteBuf, Frame, SvcPeer, to_cbor};

/// The most input one batch carries.
pub const BATCH: usize = 32 * 1024;
/// How long a batch gathers input after its first bytes.
const GATHER: Duration = Duration::from_millis(5);
/// How often a blocked reader re-checks the window, the quota and storage.
const RECHECK: Duration = Duration::from_secs(1);
/// The most output one write carries.
const OUT_BATCH: usize = 256 * 1024;
/// Records read per output pass.
const OUT_RECORDS: usize = 64;
/// How long a close record's connection drains input.
const DRAIN: Duration = Duration::from_secs(1);
/// The largest WebSocket frame, both ways (api.md §14.9).
pub const WS_FRAME: usize = 64 * 1024;
/// The close code of a connection that ended normally.
pub const WS_NORMAL: u16 = 1000;
/// The close code of a reset (api.md §14.9).
pub const WS_RESET: u16 = 4000;

/// A connection's transport.
pub enum Sock {
    /// A `tcp` service's socket.
    Tcp(TcpStream),
    /// A `tcp-auth` connection's WebSocket, authenticated already.
    Ws(Box<WebSocket>),
}

/// The read half of a [`Sock`].
enum Rd {
    Tcp(OwnedReadHalf),
    /// `rest` is what is left of the last binary frame: nothing more is
    /// read until it is consumed, so the input window holds.
    Ws {
        rx: SplitStream<WebSocket>,
        rest: Bytes,
    },
}

/// The write half of a [`Sock`].
enum Wr {
    Tcp(OwnedWriteHalf),
    Ws(SplitSink<WebSocket, Message>),
}

impl Sock {
    fn split(self) -> (Rd, Wr) {
        match self {
            Sock::Tcp(s) => {
                let (r, w) = s.into_split();
                (Rd::Tcp(r), Wr::Tcp(w))
            }
            Sock::Ws(ws) => {
                let (tx, rx) = (*ws).split();
                (
                    Rd::Ws {
                        rx,
                        rest: Bytes::new(),
                    },
                    Wr::Ws(tx),
                )
            }
        }
    }

    /// A WebSocket learns its connection id once the accept has
    /// committed (api.md §14.9); false if it's gone.
    async fn accepted(&mut self, conn: &Id) -> bool {
        match self {
            Sock::Tcp(_) => true,
            Sock::Ws(ws) => {
                let ok = Frame::Ok {
                    id: None,
                    conn: Some(conn.to_vec()),
                };
                ws.send(Message::Binary(to_cbor(&ok).into())).await.is_ok()
            }
        }
    }

    /// The accept failed: a WebSocket gets the error first.
    async fn refused(self, e: &ApiError) {
        match self {
            Sock::Tcp(s) => reset(s),
            Sock::Ws(ws) => {
                let mut ws = *ws;
                let f = Frame::Err {
                    id: None,
                    code: e.code.into(),
                    message: e.message.clone(),
                };
                let _ = ws.send(Message::Binary(to_cbor(&f).into())).await;
                ws_close(ws, WS_RESET).await;
            }
        }
    }

    /// Reset: `SO_LINGER` 0, or close 4000.
    async fn reset(self) {
        match self {
            Sock::Tcp(s) => reset(s),
            Sock::Ws(ws) => ws_close(*ws, WS_RESET).await,
        }
    }

    /// Close: the connection ended normally.
    async fn close(self) {
        match self {
            Sock::Tcp(s) => drop(s),
            Sock::Ws(ws) => ws_close(*ws, WS_NORMAL).await,
        }
    }

    /// A close record (svc.md §3 step 9): read and discard input for up to
    /// [`DRAIN`] after shutting down the write side, so the kernel doesn't
    /// reset the last bytes; then close.
    async fn drain(self) {
        match self {
            Sock::Tcp(mut sock) => {
                let _ = sock.shutdown().await;
                let mut buf = [0u8; 4096];
                let until = Instant::now() + DRAIN;
                while let Ok(Ok(n)) = tokio::time::timeout_at(until, sock.read(&mut buf)).await {
                    if n == 0 {
                        break;
                    }
                }
            }
            Sock::Ws(ws) => ws_close(*ws, WS_NORMAL).await,
        }
    }
}

/// Send a close with `code`, then read and discard until the client's
/// close, for up to [`DRAIN`].
async fn ws_close(mut ws: WebSocket, code: u16) {
    let until = Instant::now() + DRAIN;
    let close = Message::Close(Some(CloseFrame {
        code,
        reason: "".into(),
    }));
    if !matches!(
        tokio::time::timeout_at(until, ws.send(close)).await,
        Ok(Ok(()))
    ) {
        return;
    }
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(until, ws.next()).await {
        if matches!(m, Message::Close(_)) {
            break;
        }
    }
}

impl Rd {
    /// Read input into `buf` (not empty); 0 is the peer's FIN (a `"fin"`
    /// frame on a WebSocket). Cancel-safe.
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Rd::Tcp(r) => r.read(buf).await,
            Rd::Ws { rx, rest } => loop {
                if !rest.is_empty() {
                    let n = rest.len().min(buf.len());
                    buf[..n].copy_from_slice(&rest.split_to(n));
                    return Ok(n);
                }
                match rx.next().await {
                    Some(Ok(Message::Binary(b))) => *rest = b,
                    Some(Ok(Message::Text(t))) if t.as_str() == "fin" => return Ok(0),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Text(_))) => {
                        return Err(io::Error::other("a text frame other than fin"));
                    }
                    // The client's close, or a frame over the limit: a reset.
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(io::ErrorKind::ConnectionReset.into());
                    }
                    Some(Err(e)) => return Err(io::Error::other(e)),
                }
            },
        }
    }

    fn reunite(self, wr: Wr) -> Sock {
        match (self, wr) {
            (Rd::Tcp(r), Wr::Tcp(w)) => Sock::Tcp(r.reunite(w).expect("the halves of one socket")),
            (Rd::Ws { rx, .. }, Wr::Ws(tx)) => Sock::Ws(Box::new(
                rx.reunite(tx).expect("the halves of one WebSocket"),
            )),
            _ => unreachable!("the halves of one socket"),
        }
    }
}

impl Wr {
    /// Write all of `data`: in frames of at most [`WS_FRAME`] on a
    /// WebSocket.
    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Wr::Tcp(w) => w.write_all(data).await,
            Wr::Ws(tx) => {
                for c in data.chunks(WS_FRAME) {
                    tx.feed(Message::Binary(Bytes::copy_from_slice(c)))
                        .await
                        .map_err(io::Error::other)?;
                }
                tx.flush().await.map_err(io::Error::other)
            }
        }
    }

    /// Half-close: shut down the write side, or send `"fin"`.
    async fn fin(&mut self) {
        match self {
            Wr::Tcp(w) => {
                let _ = w.shutdown().await;
            }
            Wr::Ws(tx) => {
                let _ = tx.send(Message::Text("fin".into())).await;
            }
        }
    }
}

/// Accept `sock` as a new connection of the slot's service and run it.
pub fn spawn(st: &Shared, slot: Slot, sock: Sock, peer: SvcPeer) {
    let mut conn = [0u8; 16];
    getrandom::fill(&mut conn).expect("OS RNG");
    let st2 = st.clone();
    let svc = slot.service().clone();
    st.svc.spawn_conn(conn, move |rx| async move {
        let _guard = Guard {
            st: st2.clone(),
            _slot: slot,
            conn,
        };
        let c = Conn {
            fs: svc.cfg.fs,
            id: svc.cfg.id.clone(),
            st: st2,
            svc,
            conn,
            inbox: OnceLock::new(),
            sealing: AtomicBool::new(true),
        };
        c.run(sock, peer, rx).await;
    });
}

/// Unregisters the task and frees its `max_conns` slot, however it ends.
struct Guard {
    st: Shared,
    _slot: Slot,
    conn: Id,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.st.svc.unregister(&self.conn);
    }
}

struct Conn {
    st: Shared,
    svc: Arc<Service>,
    fs: u32,
    id: Vec<u8>,
    conn: Id,
    /// The key sealing input chunks, on a service with an inbox (svc.md §9).
    inbox: OnceLock<InboxKey>,
    /// Input is sealed until a step sets `unseal`.
    sealing: AtomicBool,
}

/// How a connection's task ends.
enum End {
    /// Reset the socket (`SO_LINGER` 0), then reap.
    Reset,
    /// Close the socket, then reap.
    Close,
    /// Shut down the write side, read and discard input for up to
    /// [`DRAIN`], close, then reap (a close record, svc.md §3 step 9).
    Drain,
    /// Reset the socket; the connection is reaped already.
    Reaped,
}

/// Why the writer stopped.
enum Written {
    /// It wrote a FIN record and shut down the write side.
    Fin,
    /// It wrote a close record.
    Close,
    /// It reached an abort record, or a record that can't be opened.
    Abort,
    /// Writing to the socket failed.
    Failed,
    /// The connection was reaped elsewhere.
    Gone,
}

/// Released output, as [`Conn::output`] finds it.
struct Output {
    /// `written_to`.
    from: u64,
    /// The raw bytes to write next.
    data: Vec<u8>,
    /// The flags of the record that ends the output, if it is reached.
    end: u8,
    /// A record that can't be parsed or opened.
    bad: bool,
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
    async fn run(&self, mut sock: Sock, peer: SvcPeer, mut rx: mpsc::Receiver<Cmd>) {
        if let Err(e) = self.accept(peer).await {
            tracing::warn!(service = %self.svc.cfg.name, error = %e.message, "accept failed");
            sock.refused(&e).await;
            self.reap().await;
            return;
        }
        if !sock.accepted(&self.conn).await {
            sock.reset().await;
            self.reap().await;
            return;
        }
        let (mut rd, mut wr) = sock.split();
        let wake = Notify::new();
        wake.notify_one();
        let t0 = Instant::now();
        let last_out = AtomicU64::new(0);
        let end = {
            let writer = self.writer(&mut wr, &wake, t0, &last_out);
            tokio::pin!(writer);
            self.serve(&mut rd, &mut rx, &wake, writer.as_mut(), t0, &last_out)
                .await
        };
        let sock = rd.reunite(wr);
        match end {
            End::Reset => {
                sock.reset().await;
                self.reap().await;
            }
            End::Reaped => sock.reset().await,
            End::Close => {
                sock.close().await;
                self.reap().await;
            }
            End::Drain => {
                sock.drain().await;
                self.reap().await;
            }
        }
    }

    /// The connection's loop: read input, store it, follow the writer and
    /// the timers, until the connection ends.
    async fn serve<W: Future<Output = Written>>(
        &self,
        rd: &mut Rd,
        rx: &mut mpsc::Receiver<Cmd>,
        wake: &Notify,
        mut writer: Pin<&mut W>,
        t0: Instant,
        last_out: &AtomicU64,
    ) -> End {
        let cfg = &self.svc.cfg;
        let window = u64::from(cfg.window);
        let secs = |s: u32| Duration::from_secs(s.into());
        let far = t0 + Duration::from_secs(365 * 86_400);
        let accept_by = t0 + Duration::from_millis(cfg.accept_wait_ms.into());
        let mut handshake_by = (cfg.handshake_secs > 0).then(|| t0 + secs(cfg.handshake_secs));
        let idle = (cfg.idle_secs > 0).then(|| secs(cfg.idle_secs));
        // The last input read, or the last idle check that found bytes
        // waiting.
        let mut idle_from = t0;
        let (mut in_head, mut in_acked) = (0u64, 0u64);
        let (mut eof, mut claimed) = (false, false);
        // The writer has shut down the write side (a FIN record).
        let (mut writing, mut fin) = (true, false);
        // Input read but not stored yet (the quota was full, or storage
        // failed): no more is read until it is.
        let mut held: Option<Vec<u8>> = None;
        let mut buf = vec![0u8; BATCH];
        loop {
            // Half-closed both ways, and every byte acked (svc.md §3, step 8).
            if fin && eof && held.is_none() && in_acked >= in_head {
                return End::Close;
            }
            let room = window.saturating_sub(in_head - in_acked).min(BATCH as u64) as usize;
            let can_read = !eof && held.is_none() && room > 0;
            let blocked = !eof && !can_read;
            let out_at = t0 + Duration::from_millis(last_out.load(Ordering::Relaxed));
            let idle_at = idle.map(|d| idle_from.max(out_at) + d);
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Wake) => {
                        wake.notify_one();
                        if blocked || fin {
                            in_acked = self.in_acked().await.unwrap_or(in_acked);
                        }
                    }
                    Some(Cmd::Abort) | None => return End::Reset,
                },
                w = &mut writer, if writing => {
                    writing = false;
                    match w {
                        Written::Fin => {
                            fin = true;
                            in_acked = self.in_acked().await.unwrap_or(in_acked);
                        }
                        Written::Close => return End::Drain,
                        Written::Abort => return End::Reset,
                        Written::Failed => return End::Close,
                        Written::Gone => return End::Reaped,
                    }
                },
                r = rd.read(&mut buf[..room]), if can_read => match r {
                    Ok(0) => {
                        eof = true;
                        held = Some(Vec::new());
                    }
                    Ok(n) => {
                        idle_from = Instant::now();
                        // Gather more for a moment; not cancellable, so no
                        // input read is ever dropped.
                        let mut len = n;
                        let until = Instant::now() + GATHER;
                        while len < room {
                            match tokio::time::timeout_at(until, rd.read(&mut buf[len..room])).await {
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
                    // A reset or a failed read: the connection is over. On a
                    // WebSocket, the client's close is a reset (api.md §14.9).
                    Err(_) => return match rd {
                        Rd::Tcp(_) => End::Close,
                        Rd::Ws { .. } => End::Reset,
                    },
                },
                _ = tokio::time::sleep_until(accept_by), if !claimed => {
                    match reap_unclaimed(&self.st, self.fs, &self.id, &self.conn).await {
                        Ok(true) => {
                            tracing::debug!(service = %cfg.name, "no handler claimed the connection");
                            return End::Reaped;
                        }
                        Ok(false) => claimed = true,
                        Err(e) => tracing::warn!(error = %e.message, "reap"),
                    }
                },
                _ = tokio::time::sleep_until(handshake_by.unwrap_or(far)), if handshake_by.is_some() => {
                    match self.acks().await {
                        Ok(ra) if ra == Acks::default() => {
                            tracing::debug!(service = %cfg.name, "no step within handshake_secs");
                            return End::Reset;
                        }
                        Ok(_) => handshake_by = None,
                        Err(_) => handshake_by = Some(Instant::now() + RECHECK),
                    }
                },
                _ = tokio::time::sleep_until(idle_at.unwrap_or(far)), if idle_at.is_some() => {
                    match self.waiting().await {
                        Ok(false) => {
                            tracing::debug!(service = %cfg.name, "idle");
                            return End::Reset;
                        }
                        Ok(true) | Err(_) => idle_from = Instant::now(),
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
                        if fin && eof {
                            in_acked = self.in_acked().await.unwrap_or(in_acked);
                        }
                    }
                    Ok(Outcome::Over) => held = Some(data),
                    Ok(Outcome::Gone) => return End::Reset,
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

    /// The writer (svc.md §3, steps 7–9): writes released records in order
    /// from `written_to`, and returns when a record ends the output, the
    /// socket fails or the connection is gone. `wake` rings when steps or
    /// reveals may have released more.
    async fn writer(
        &self,
        wr: &mut Wr,
        wake: &Notify,
        t0: Instant,
        last_out: &AtomicU64,
    ) -> Written {
        let mut written: Option<u64> = None;
        loop {
            let out = match self.output(written).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(error = %e.message, "reading relayed output");
                    tokio::time::sleep(RECHECK).await;
                    continue;
                }
            };
            if out.bad {
                return Written::Abort;
            }
            if out.data.is_empty() && out.end == 0 {
                written = Some(out.from);
                wake.notified().await;
                continue;
            }
            if !out.data.is_empty() {
                if wr.write_all(&out.data).await.is_err() {
                    return Written::Failed;
                }
                last_out.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                let to = out.from + out.data.len() as u64;
                if !self.advance(out.from, to).await {
                    return Written::Gone;
                }
                written = Some(to);
            }
            if out.end & FIN != 0 {
                wr.fin().await;
                return Written::Fin;
            }
            if out.end & CLOSE != 0 {
                return Written::Close;
            }
            if out.end & ABORT != 0 {
                return Written::Abort;
            }
        }
    }

    /// The released output from `written` (else `rw`): plain records, and
    /// sealed ones whose key is revealed, up to the first that isn't, a
    /// record that ends the output, or [`OUT_BATCH`] bytes. Records are
    /// immutable and keys only appear, so snapshot reads are exact.
    async fn output(&self, written: Option<u64>) -> ApiResult<Output> {
        let (fs, svc, conn) = (self.fs, &self.id[..], &self.conn);
        let (out, _) = txn_loop!(self.st.store, None, |t| {
            let from = match written {
                Some(w) => w,
                None => decode_u64(
                    t.snapshot_get(&keys::conn("rw", fs, svc, conn))
                        .await?
                        .as_deref(),
                ),
            };
            let mut out = Output {
                from,
                data: Vec::new(),
                end: 0,
                bad: false,
            };
            let range = |tag| {
                let p = keys::conn_key(tag, fs, svc, conn);
                (p.clone().int(from as i64).finish(), end_of(&p.finish()))
            };
            let (b, e) = range("ro");
            let recs = t.snapshot_get_range(&b, &e, OUT_RECORDS, false).await?;
            if recs.is_empty() {
                return Ok(out);
            }
            let (b, e) = range("rv");
            let keys: HashMap<u64, Vec<u8>> = t
                .snapshot_get_range(&b, &e, OUT_RECORDS, false)
                .await?
                .into_iter()
                .filter_map(|(k, v)| Some((tail_int(&k)?, v)))
                .collect();
            for (k, v) in &recs {
                let Some(off) = tail_int(k) else { break };
                if off != from + out.data.len() as u64 {
                    break;
                }
                let Ok(rec) = parse_record(v) else {
                    out.bad = true;
                    break;
                };
                if rec.is_plain() {
                    out.data.extend_from_slice(rec.body);
                } else if let Some(k_r) = keys
                    .get(&off)
                    .and_then(|k| <&[u8; 32]>::try_from(&k[..]).ok())
                {
                    match zen_core::svc::reveal(fs, svc, conn, off, v, k_r) {
                        Ok(raw) => out.data.extend_from_slice(&raw),
                        Err(_) => {
                            out.bad = true;
                            break;
                        }
                    }
                } else {
                    break;
                }
                if rec.flags & (FIN | CLOSE | ABORT) != 0 {
                    out.end = rec.flags;
                    break;
                }
                if out.data.len() >= OUT_BATCH {
                    break;
                }
            }
            Ok(out)
        })?;
        Ok(out)
    }

    /// Record output `[from, to)` as written: clear its records and keys,
    /// advance `rw`, give the bytes back to the quota and wake the holder.
    /// Retries until it applies; false once the connection is gone.
    async fn advance(&self, from: u64, to: u64) -> bool {
        let (fs, svc, conn) = (self.fs, &self.id[..], &self.conn);
        let rw = keys::conn("rw", fs, svc, conn);
        let mut attempt = 0u32;
        loop {
            let r = txn_loop!(self.st.store, None, |t| {
                // Read normally: a reap conflicts with the advance.
                let Some(rm) = t
                    .get(&keys::conn("rm", fs, svc, conn))
                    .await?
                    .as_deref()
                    .and_then(ClaimRec::decode)
                else {
                    return Ok(false);
                };
                for tag in ["ro", "rv"] {
                    let p = keys::conn_key(tag, fs, svc, conn);
                    t.clear_range(
                        &p.clone().int(from as i64).finish(),
                        &p.int(to as i64).finish(),
                    );
                }
                t.set(&rw, &to.to_be_bytes());
                t.atomic_add(&keys::quota(fs, "bytes"), -((to - from) as i64));
                if rm.is_claimed()
                    && t.snapshot_get(&keys::svc_lease(fs, svc, &rm.lease))
                        .await?
                        .is_some()
                {
                    ring(&mut t, &keys::svc_lease_bell(fs, svc, &rm.lease));
                }
                Ok(true)
            });
            let e = match r {
                Ok((applied, _)) => return applied,
                Err(e) => e,
            };
            if e.code == "commit_unknown" {
                // Applied already? `rw` tells, so the quota isn't given back twice.
                if let Ok((w, _)) = txn_loop!(self.st.store, None, |t| {
                    Ok(decode_u64(t.get(&rw).await?.as_deref()))
                }) && w >= to
                {
                    return true;
                }
            } else {
                tracing::warn!(error = %e.message, attempt, "advancing written_to");
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(100 << attempt.min(5))).await;
        }
    }

    /// The accept transaction (svc.md §3, step 2).
    async fn accept(&self, peer: SvcPeer) -> ApiResult<()> {
        let cfg = &self.svc.cfg;
        let inbox = match &cfg.inbox {
            Some(pk) => {
                let (ct, key) = inbox_encaps(pk, self.fs, &self.id, &self.conn, &mut OsRng)
                    .map_err(|e| internal(format!("inbox encapsulation: {e}")))?;
                let _ = self.inbox.set(key);
                Some(Inbox {
                    v: 1,
                    ct: ByteBuf::from(ct),
                })
            }
            None => None,
        };
        let rc = zen_proto::to_cbor(&ConnRec {
            node: ByteBuf::from(self.st.svc.node.to_vec()),
            peer,
            accepted_ms: unix_ms(),
            mode: cfg.mode.name().into(),
            window: cfg.window,
            out_window: cfg.out_window,
            resume: cfg.resume,
            inbox,
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
    /// appended twice. On a service with an inbox, the batch is sealed
    /// until the connection's `ra` has `unseal` (svc.md §9).
    async fn append(&self, in_head: u64, data: &[u8], eof: bool) -> ApiResult<Outcome> {
        let (fs, svc, conn) = (self.fs, &self.id[..], &self.conn);
        let max_bytes = self.st.acl().limits.get(&fs).and_then(|l| l.max_bytes);
        let len = data.len() as u64;
        let plain = || {
            let mut v = Vec::with_capacity(data.len() + 1);
            v.push(0);
            v.extend_from_slice(data);
            v
        };
        let sealed = match self.inbox.get() {
            Some(key) if len > 0 && self.sealing.load(Ordering::Relaxed) => {
                let c = key
                    .seal_chunk(in_head, data)
                    .map_err(|e| internal(format!("sealing input: {e}")))?;
                let mut v = Vec::with_capacity(c.len() + 1);
                v.push(1);
                v.extend_from_slice(&c);
                Some(v)
            }
            _ => None,
        };
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
                    let mut v = sealed.as_ref();
                    if v.is_some() {
                        // By snapshot: steps own `ra`, and `unseal` is sticky.
                        let k = keys::conn("ra", fs, svc, conn);
                        let ra = Acks::decode(t.snapshot_get(&k).await?.as_deref());
                        if ra.flags & RA_UNSEAL != 0 {
                            self.sealing.store(false, Ordering::Relaxed);
                            v = None;
                        }
                    }
                    let v = v.cloned().unwrap_or_else(plain);
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
                    log_input(&mut t, fs, svc, &rm.lease, conn, 0);
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
        Ok(self.acks().await?.in_acked)
    }

    /// `ra`, by snapshot.
    async fn acks(&self) -> ApiResult<Acks> {
        let k = keys::conn("ra", self.fs, &self.id, &self.conn);
        let (ra, _) = txn_loop!(self.st.store, None, |t| {
            Ok(Acks::decode(t.snapshot_get(&k).await?.as_deref()))
        })?;
        Ok(ra)
    }

    /// Whether bytes wait: unacked input or unwritten output (svc.md §2,
    /// `idle_secs`).
    async fn waiting(&self) -> ApiResult<bool> {
        let k = |tag| keys::conn(tag, self.fs, &self.id, &self.conn);
        let (w, _) = txn_loop!(self.st.store, None, |t| {
            let (in_head, _) = decode_head(t.snapshot_get(&k("rh")).await?.as_deref());
            let ra = Acks::decode(t.snapshot_get(&k("ra")).await?.as_deref());
            let written_to = decode_u64(t.snapshot_get(&k("rw")).await?.as_deref());
            Ok(ra.in_acked < in_head || written_to < ra.out_head)
        })?;
        Ok(w)
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
