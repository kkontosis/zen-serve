//! WebSocket `/v1/stream` (api.md §9): gap-free subscriptions (G9) and
//! ephemeral pub/sub.

use crate::acl::{R_APPEND, R_READ};
use crate::auth::{Caller, resolve};
use crate::eph::EphMsg;
use crate::error::{ApiError, ApiResult, bad_request};
use crate::ids::{Offset, check_prefix, check_topic, parse_offset};
use crate::keys;
use crate::log::{read_prefix, read_topic};
use crate::state::Shared;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use zen_proto::{Frame, ZERO_OFFSET, from_cbor, to_cbor};

const BATCH: usize = 256;
const MAX_SUBS: usize = 256;

/// `GET /v1/stream`.
pub async fn ws(State(st): State<Shared>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |sock| run(st, sock))
}

fn err_frame(id: Option<u32>, e: &ApiError) -> Frame {
    Frame::Err {
        id,
        code: e.code.into(),
        message: e.message.clone(),
    }
}

#[derive(Clone)]
enum Target {
    Topic(Vec<u8>),
    Prefix(Vec<u8>),
}

impl Target {
    fn from(topic: Option<Vec<u8>>, prefix: Option<Vec<u8>>) -> ApiResult<Self> {
        match (topic, prefix) {
            (Some(t), None) => {
                check_topic(&t)?;
                Ok(Target::Topic(t))
            }
            (None, Some(p)) => {
                check_prefix(&p)?;
                Ok(Target::Prefix(p))
            }
            _ => Err(bad_request("give exactly one of topic and prefix")),
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Target::Topic(t) | Target::Prefix(t) => t,
        }
    }

    fn matches(&self, topic: &[u8]) -> bool {
        match self {
            Target::Topic(t) => t == topic,
            Target::Prefix(p) => topic.starts_with(p),
        }
    }
}

async fn run(st: Shared, sock: WebSocket) {
    let (mut ws_tx, mut ws_rx) = sock.split();
    let (out, mut out_rx) = mpsc::channel::<Frame>(BATCH);
    let writer = tokio::spawn(async move {
        while let Some(f) = out_rx.recv().await {
            if ws_tx
                .send(Message::Binary(to_cbor(&f).into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut token: Option<Vec<u8>> = None;
    let mut tasks: HashMap<u32, JoinHandle<()>> = HashMap::new();
    while let Some(Ok(msg)) = ws_rx.next().await {
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        let frame: Frame = match from_cbor(&bytes) {
            Ok(f) => f,
            Err(e) => {
                let _ = out.send(err_frame(None, &bad_request(e))).await;
                continue;
            }
        };
        let caller = match (&frame, &token) {
            (Frame::Auth { token: t }, _) => match resolve(&st, t).await {
                Ok(_) => {
                    token = Some(t.clone());
                    let _ = out.send(Frame::Ok { id: None }).await;
                    continue;
                }
                Err(e) => {
                    let _ = out.send(err_frame(None, &e)).await;
                    break;
                }
            },
            (_, None) => {
                let _ = out
                    .send(err_frame(
                        None,
                        &crate::error::unauthorized("authenticate first"),
                    ))
                    .await;
                break;
            }
            (_, Some(t)) => match resolve(&st, t).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = out.send(err_frame(None, &e)).await;
                    break;
                }
            },
        };
        let tok = token.clone().expect("authenticated");
        tasks.retain(|_, h| !h.is_finished());
        match frame {
            Frame::Sub {
                id,
                fs,
                topic,
                prefix,
                after,
            } => {
                let started = (|| {
                    st.check_fs(fs)?;
                    let target = Target::from(topic, prefix)?;
                    caller.require_topic(fs, target.bytes(), R_READ)?;
                    let after = after.map(|a| parse_offset(Some(&a))).transpose()?;
                    if tasks.len() >= MAX_SUBS || tasks.contains_key(&id) {
                        return Err(bad_request(
                            "subscription id in use or too many subscriptions",
                        ));
                    }
                    Ok((target, after))
                })();
                match started {
                    Ok((target, after)) => {
                        let _ = out.send(Frame::Ok { id: Some(id) }).await;
                        let h = tokio::spawn(subscription(
                            st.clone(),
                            tok,
                            id,
                            fs,
                            target,
                            after,
                            out.clone(),
                        ));
                        tasks.insert(id, h);
                    }
                    Err(e) => {
                        let _ = out.send(err_frame(Some(id), &e)).await;
                    }
                }
            }
            Frame::Esub {
                id,
                fs,
                topic,
                prefix,
            } => {
                let started = (|| {
                    st.check_fs(fs)?;
                    let target = Target::from(topic, prefix)?;
                    caller.require_topic(fs, target.bytes(), R_READ)?;
                    if tasks.len() >= MAX_SUBS || tasks.contains_key(&id) {
                        return Err(bad_request(
                            "subscription id in use or too many subscriptions",
                        ));
                    }
                    Ok(target)
                })();
                let started = match started {
                    Ok(target) => st.eph.subscribe(fs).await.map(|rx| (target, rx)),
                    Err(e) => Err(e),
                };
                match started {
                    Ok((target, rx)) => {
                        let _ = out.send(Frame::Ok { id: Some(id) }).await;
                        let h = tokio::spawn(ephemeral(
                            st.clone(),
                            tok,
                            id,
                            fs,
                            target,
                            rx,
                            out.clone(),
                        ));
                        tasks.insert(id, h);
                    }
                    Err(e) => {
                        let _ = out.send(err_frame(Some(id), &e)).await;
                    }
                }
            }
            Frame::Unsub { id } => {
                if let Some(h) = tasks.remove(&id) {
                    h.abort();
                }
                let _ = out.send(Frame::Ok { id: Some(id) }).await;
            }
            Frame::Epub { fs, topic, data } => {
                let r = (|| {
                    st.check_fs(fs)?;
                    check_topic(&topic)?;
                    caller.require_topic(fs, &topic, R_APPEND)?;
                    if data.len() > st.cfg.limits.max_envelope_bytes as usize {
                        return Err(crate::error::too_large("ephemeral message too large"));
                    }
                    Ok(())
                })();
                let r = match r {
                    Ok(()) => st.eph.publish(fs, &topic, &caller.device, &data).await,
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => {}
                    Err(e) => {
                        let _ = out.send(err_frame(None, &e)).await;
                    }
                }
            }
            _ => {
                let _ = out
                    .send(err_frame(None, &bad_request("not a client frame")))
                    .await;
            }
        }
    }
    for (_, h) in tasks {
        h.abort();
    }
    drop(out);
    let _ = writer.await;
}

/// Re-authorize a long-lived task against the current ACL.
async fn recheck(st: &Shared, token: &[u8], fs: u32, target: &Target) -> ApiResult<Caller> {
    let caller = resolve(st, token).await?;
    caller.require_topic(fs, target.bytes(), R_READ)?;
    Ok(caller)
}

async fn current_head(st: &Shared, fs: u32, target: &Target) -> ApiResult<Offset> {
    let prefix = match target {
        Target::Topic(t) => keys::log_prefix(fs, t).finish(),
        Target::Prefix(_) => keys::gl_prefix(fs).finish(),
    };
    let mut t = st.store.begin(None).await?;
    Ok(
        match t
            .snapshot_get_range(&prefix, &keys::end_of(&prefix), 1, true)
            .await?
            .pop()
        {
            Some((k, _)) => k[k.len() - 12..].try_into().expect("12 bytes"),
            None => ZERO_OFFSET,
        },
    )
}

async fn subscription(
    st: Shared,
    token: Vec<u8>,
    id: u32,
    fs: u32,
    target: Target,
    after: Option<Offset>,
    out: mpsc::Sender<Frame>,
) {
    let r: ApiResult<()> = async {
        let mut cursor = match after {
            Some(a) => a,
            None => current_head(&st, fs, &target).await?,
        };
        let head_key = match &target {
            Target::Topic(t) => keys::topic_head(fs, t),
            Target::Prefix(_) => keys::fs_head(fs),
        };
        loop {
            recheck(&st, &token, fs, &target).await?;
            // Watch first, then read: an append between the two still wakes us.
            let w = st.store.watch(&head_key).await?;
            let more = match &target {
                Target::Topic(t) => {
                    let (events, more) =
                        read_topic(st.store.as_ref(), fs, t, None, &cursor, BATCH).await?;
                    for (o, e) in events {
                        cursor = o;
                        let f = Frame::Ev {
                            id,
                            topic: t.clone(),
                            offset: e.offset,
                            key_token: e.key_token,
                            envelope: e.envelope,
                        };
                        if out.send(f).await.is_err() {
                            return Ok(());
                        }
                    }
                    more
                }
                Target::Prefix(p) => {
                    let (events, last, more) =
                        read_prefix(st.store.as_ref(), fs, p, &cursor, BATCH).await?;
                    let caller = resolve(&st, &token).await?;
                    for (topic, e) in events {
                        if caller.require_topic(fs, &topic, R_READ).is_err() {
                            continue;
                        }
                        let f = Frame::Ev {
                            id,
                            topic,
                            offset: e.offset,
                            key_token: e.key_token,
                            envelope: e.envelope,
                        };
                        if out.send(f).await.is_err() {
                            return Ok(());
                        }
                    }
                    cursor = last;
                    more
                }
            };
            if !more {
                w.await;
            }
        }
    }
    .await;
    if let Err(e) = r {
        let _ = out.send(err_frame(Some(id), &e)).await;
    }
}

async fn ephemeral(
    st: Shared,
    token: Vec<u8>,
    id: u32,
    fs: u32,
    target: Target,
    mut rx: broadcast::Receiver<Arc<EphMsg>>,
    out: mpsc::Sender<Frame>,
) {
    loop {
        let m = match rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if m.fs != fs || !target.matches(&m.topic) {
            continue;
        }
        if let Err(e) = recheck(&st, &token, fs, &target).await {
            let _ = out.send(err_frame(Some(id), &e)).await;
            return;
        }
        let f = Frame::Eph {
            id,
            topic: m.topic.clone(),
            data: m.data.clone(),
            sender: m.sender.to_vec(),
        };
        if out.send(f).await.is_err() {
            return;
        }
    }
}
