//! `tcp-auth` connections (api.md §14.9, svc.md §3): a member's WebSocket
//! on `/v1/svc/connect` becomes a relayed connection, whose peer is the
//! member's user and device. Rights are checked once, here.

use super::conn::{self, Sock, WS_FRAME, WS_RESET};
use super::{Service, Slot};
use crate::acl::R_APPEND;
use crate::auth::{Caller, bearer, resolve, resolve_bearer};
use crate::config::{SvcMode, parse_hex};
use crate::error::{ApiError, ApiResult, bad_request, not_found, quota, unauthorized};
use crate::state::Shared;
use crate::tls;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_proto::{Frame, SvcPeer, from_cbor, to_cbor};

/// How long a WebSocket without a Bearer header has to authenticate.
const AUTH_WAIT: Duration = Duration::from_secs(10);

/// The query of `/v1/svc/connect`.
#[derive(Deserialize)]
pub struct ConnectQuery {
    fs: u32,
    svc: String,
}

/// `GET /v1/svc/connect?fs=<fs>&svc=<hex>`.
pub async fn connect(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<tls::Peer>,
    query: Result<Query<ConnectQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let (fs, svc) = match query {
        Ok(Query(q)) => match parse_hex(&q.svc) {
            Some(svc) => (q.fs, svc),
            None => return bad_request("svc is not hex").into_response(),
        },
        Err(e) => return bad_request(e.body_text()).into_response(),
    };
    let ip = peer.addr.ip();
    let upgrade = upgrade.max_message_size(WS_FRAME).max_frame_size(WS_FRAME);
    match bearer(&headers) {
        // Everything is checked before the upgrade: failures are HTTP errors.
        Some(h) => {
            let admitted = match resolve_bearer(&st, h).await {
                Ok(c) => admit(&st, &c, fs, &svc, ip).map(|s| (s, peer_of(&c))),
                Err(e) => Err(e),
            };
            match admitted {
                Ok((slot, peer)) => upgrade.on_upgrade(move |ws| async move {
                    conn::spawn(&st, slot, Sock::Ws(Box::new(ws)), peer)
                }),
                Err(e) => e.into_response(),
            }
        }
        None => upgrade.on_upgrade(move |mut ws| async move {
            let admitted = match first_frame(&st, &mut ws).await {
                Ok(c) => admit(&st, &c, fs, &svc, ip).map(|s| (s, peer_of(&c))),
                Err(e) => Err(e),
            };
            match admitted {
                Ok((slot, peer)) => conn::spawn(&st, slot, Sock::Ws(Box::new(ws)), peer),
                Err(e) => refuse(ws, &e).await,
            }
        }),
    }
}

/// The checks after the token (api.md §14.9): the service and its mode,
/// topic `append`, then admission (svc.md §3 step 1).
fn admit(st: &Shared, c: &Caller, fs: u32, svc: &[u8], ip: IpAddr) -> ApiResult<Slot> {
    let s: Arc<Service> = super::api::service(st, fs, svc)?;
    if s.cfg.mode != SvcMode::TcpAuth {
        return Err(not_found("not a tcp-auth service"));
    }
    c.require_topic(fs, svc, R_APPEND)?;
    if !s.take(ip, Instant::now()) {
        return Err(quota("too many connections from this address"));
    }
    s.open().ok_or_else(|| quota("the service is at max_conns"))
}

fn peer_of(c: &Caller) -> SvcPeer {
    SvcPeer {
        addr: None,
        user: Some(c.user.to_vec()),
        device: Some(c.device.to_vec()),
    }
}

/// Without a Bearer header: the first frame is `{op: "auth", token}`,
/// within [`AUTH_WAIT`].
async fn first_frame(st: &Shared, ws: &mut WebSocket) -> ApiResult<Caller> {
    let until = tokio::time::Instant::now() + AUTH_WAIT;
    loop {
        let msg = match tokio::time::timeout_at(until, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(_) => return Err(unauthorized("closed before authenticating")),
            Err(_) => return Err(unauthorized("no auth frame within 10 s")),
        };
        match msg {
            Message::Binary(b) => {
                return match from_cbor::<Frame>(&b) {
                    Ok(Frame::Auth { token }) => resolve(st, &token).await,
                    Ok(_) => Err(unauthorized("authenticate first")),
                    Err(e) => Err(bad_request(e)),
                };
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => return Err(unauthorized("authenticate first")),
        }
    }
}

/// An `err` frame, then a close.
async fn refuse(mut ws: WebSocket, e: &ApiError) {
    let f = Frame::Err {
        id: None,
        code: e.code.into(),
        message: e.message.clone(),
    };
    let _ = ws.send(Message::Binary(to_cbor(&f).into())).await;
    let _ = ws
        .send(Message::Close(Some(CloseFrame {
            code: WS_RESET,
            reason: "".into(),
        })))
        .await;
}
