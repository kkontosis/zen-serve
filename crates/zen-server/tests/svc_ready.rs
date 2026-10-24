//! `svc_ready` over `/v1/stream` (api.md §14.10): a connection queued
//! while the push loop arms its queue watch again is still announced. A
//! test binary of its own, because the pause that widens that window
//! (`svc::push::testing`) is process-wide.

mod common;

use common::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use zen_proto::*;
use zen_server::config::{ServiceConfig, SvcMode};
use zen_server::svc::push::testing::QUEUE_REARM_PAUSE_MS;

const SVC: [u8; 16] = [0x5E; 16];
const LEASE: [u8; 16] = [1; 16];
const SUB: u32 = 9;

/// The next frame within `within`, if any.
async fn frame_within(ws: &mut Ws, within: Duration) -> Option<Frame> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return Some(from_cbor(&b).unwrap()),
            Ok(Some(Ok(_))) => continue,
            Ok(_) | Err(_) => return None,
        }
    }
}

/// Claim what the queue holds now (no waiting).
async fn next(h: &Harness, tok: &[u8]) -> usize {
    let c: SvcClaims = h
        .call(
            "/v1/svc/next",
            Some(tok),
            &SvcNextRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: LEASE.to_vec(),
                limit: Some(64),
                wait_ms: Some(0),
            },
        )
        .await
        .unwrap();
    c.conns.len()
}

#[tokio::test]
async fn a_connection_queued_while_the_watch_rearms_is_announced() {
    let h = Harness::start_with(|c| {
        c.service.push(ServiceConfig {
            name: "echo".into(),
            fs: 1,
            id: SVC.to_vec(),
            mode: SvcMode::Tcp,
            listen: Some("127.0.0.1:0".parse().unwrap()),
            max_conns: 1024,
            per_ip_rate: 0,
            per_ip_burst: 50,
            window: 262_144,
            out_window: 1_048_576,
            accept_wait_ms: 60_000,
            handshake_secs: 30,
            idle_secs: 300,
            resume: true,
            inbox: None,
        })
    })
    .await;
    let alice = User::new(1);
    h.claim(&alice, &[]).await;
    let tok = h.sign_in(&alice).await.unwrap();
    let addr = h.server.state.svc.local_addr("echo").unwrap();
    let _: SvcLeaseResult = h
        .call(
            "/v1/svc/lease",
            Some(&tok),
            &SvcLeaseRequest {
                fs: 1,
                svc: SVC.to_vec(),
                lease: LEASE.to_vec(),
                ttl_ms: Some(30_000),
                release: None,
            },
        )
        .await
        .unwrap();
    let mut ws = connect(&h, &tok).await;
    send(
        &mut ws,
        &Frame::SvcSub {
            id: SUB,
            fs: 1,
            svc: SVC.to_vec(),
            lease: LEASE.to_vec(),
        },
    )
    .await;
    assert_eq!(
        recv(&mut ws).await,
        Frame::Ok {
            id: Some(SUB),
            conn: None
        }
    );
    // Every time the queue's watch fires, the push loop now waits 400 ms
    // before it watches again.
    QUEUE_REARM_PAUSE_MS.store(400, Ordering::Relaxed);

    // A handler that claims whenever it is told: every connection must be
    // claimed through svc_ready alone.
    let mut peers = Vec::new();
    let mut claimed = 0;
    for round in 0..3 {
        peers.push(TcpStream::connect(addr).await.unwrap());
        let f = frame_within(&mut ws, Duration::from_secs(5)).await;
        assert_eq!(f, Some(Frame::SvcReady { id: SUB }), "round {round}");
        claimed += next(&h, &tok).await;
        // Queued right after the handler's read, while the watch may be
        // re-arming: still announced.
        peers.push(TcpStream::connect(addr).await.unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while claimed < peers.len() {
            let Some(f) = frame_within(&mut ws, deadline - Instant::now()).await else {
                panic!(
                    "round {round}: {} of {} connections claimed, and no svc_ready for the rest",
                    claimed,
                    peers.len()
                );
            };
            assert_eq!(f, Frame::SvcReady { id: SUB });
            claimed += next(&h, &tok).await;
        }
    }
    QUEUE_REARM_PAUSE_MS.store(0, Ordering::Relaxed);
}
