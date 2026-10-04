//! WebSocket subscriptions (gap-free, G9), ephemeral pub/sub, static files.

mod common;

use common::*;
use std::time::Duration;
use zen_proto::*;

async fn publish(h: &Harness, tok: &[u8], n: u8, t: &[u8]) -> Vec<u8> {
    let ap = LogAppend {
        commit_id: cid(n),
        append: vec![append(t, None, &[n])],
    };
    let r: CommitResult = h.call("/v1/log/append", Some(tok), &ap).await.unwrap();
    r.appended[0].to_vec()
}

fn ev(f: Frame) -> (Vec<u8>, Vec<u8>, u8) {
    match f {
        Frame::Ev {
            topic,
            offset,
            envelope,
            ..
        } => (topic, offset, envelope[0]),
        other => panic!("expected an event, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribe_history_then_live_and_resume_without_gaps() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    for i in 0..3 {
        publish(&h, &tok, i, &topic(1)).await;
    }
    let mut ws = connect(&h, &tok).await;
    send(
        &mut ws,
        &Frame::Sub {
            id: 1,
            fs: 1,
            topic: Some(topic(1)),
            prefix: None,
            after: Some(ZERO_OFFSET.to_vec()),
        },
    )
    .await;
    assert_eq!(recv(&mut ws).await, Frame::Ok { id: Some(1) });
    let mut last = Vec::new();
    for i in 0..3 {
        let (_, o, n) = ev(recv(&mut ws).await);
        assert_eq!(n, i);
        last = o;
    }
    publish(&h, &tok, 3, &topic(1)).await;
    let (_, o, n) = ev(recv(&mut ws).await);
    assert_eq!(n, 3);
    last = o.max(last);
    drop(ws);

    // Events published while disconnected arrive after resuming from the
    // last offset, in order, with nothing skipped or repeated.
    for i in 4..7 {
        publish(&h, &tok, i, &topic(1)).await;
    }
    let mut ws = connect(&h, &tok).await;
    send(
        &mut ws,
        &Frame::Sub {
            id: 2,
            fs: 1,
            topic: Some(topic(1)),
            prefix: None,
            after: Some(last),
        },
    )
    .await;
    assert_eq!(recv(&mut ws).await, Frame::Ok { id: Some(2) });
    let writer = {
        let base = h.base.clone();
        let http = h.http.clone();
        let tok = tok.clone();
        tokio::spawn(async move {
            for i in 7..20u8 {
                let ap = LogAppend {
                    commit_id: vec![i; 16],
                    append: vec![append(&topic(1), None, &[i])],
                };
                http.post(format!("{base}/v1/log/append"))
                    .header(
                        "authorization",
                        format!(
                            "Bearer {}",
                            base64::Engine::encode(
                                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                                &tok
                            )
                        ),
                    )
                    .body(to_cbor(&ap))
                    .send()
                    .await
                    .unwrap();
            }
        })
    };
    for i in 4..20u8 {
        let (_, _, n) = ev(recv(&mut ws).await);
        assert_eq!(n, i);
    }
    writer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prefix_subscription_and_live_only() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let chat_a = [vec![5; 16], vec![1; 16]].concat();
    let chat_b = [vec![5; 16], vec![2; 16]].concat();
    let other = vec![6; 16];
    publish(&h, &tok, 0, &chat_a).await;
    let mut ws = connect(&h, &tok).await;
    // No `after`: live events only.
    send(
        &mut ws,
        &Frame::Sub {
            id: 1,
            fs: 1,
            topic: None,
            prefix: Some(vec![5; 16]),
            after: None,
        },
    )
    .await;
    assert_eq!(recv(&mut ws).await, Frame::Ok { id: Some(1) });
    tokio::time::sleep(Duration::from_millis(100)).await;
    publish(&h, &tok, 1, &other).await;
    publish(&h, &tok, 2, &chat_b).await;
    publish(&h, &tok, 3, &chat_a).await;
    let (t, _, n) = ev(recv(&mut ws).await);
    assert_eq!((t, n), (chat_b.clone(), 2));
    let (t, _, n) = ev(recv(&mut ws).await);
    assert_eq!((t, n), (chat_a.clone(), 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_requires_auth_and_rights() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let bob = User::new(2);
    let members = [&admin, &bob];
    let grants = vec![
        topic_grant(&admin, 1, &[], &["read", "append"]),
        topic_grant(&bob, 1, &[7; 16], &["read"]),
    ];
    let (v1, _) = signed_acl(&admin, 1, None, &[&admin], &members, grants, vec![]);
    h.put_acl(v1, h.server.claim_token.clone()).await.unwrap();
    let bob_tok = h.sign_in(&bob).await.unwrap();

    let url = format!("ws://{}/v1/stream", h.server.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &Frame::Sub {
            id: 1,
            fs: 1,
            topic: Some(topic(1)),
            prefix: None,
            after: None,
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, Frame::Err { code, .. } if code == "unauthorized"));

    let mut ws = connect(&h, &bob_tok).await;
    send(
        &mut ws,
        &Frame::Sub {
            id: 1,
            fs: 1,
            topic: Some(topic(1)),
            prefix: None,
            after: None,
        },
    )
    .await;
    assert!(
        matches!(recv(&mut ws).await, Frame::Err { id: Some(1), code, .. } if code == "forbidden")
    );
    send(
        &mut ws,
        &Frame::Sub {
            id: 2,
            fs: 1,
            topic: Some(topic(7)),
            prefix: None,
            after: None,
        },
    )
    .await;
    assert_eq!(recv(&mut ws).await, Frame::Ok { id: Some(2) });
    // Bob may read topic 7 but not publish to it.
    send(
        &mut ws,
        &Frame::Epub {
            fs: 1,
            topic: topic(7),
            data: b"hi".to_vec(),
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, Frame::Err { code, .. } if code == "forbidden"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_fan_out() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let bob = User::new(2);
    h.claim(&admin, &[&bob]).await;
    let a_tok = h.sign_in(&admin).await.unwrap();
    let b_tok = h.sign_in(&bob).await.unwrap();
    let mut a = connect(&h, &a_tok).await;
    let mut b = connect(&h, &b_tok).await;
    send(
        &mut b,
        &Frame::Esub {
            id: 9,
            fs: 1,
            topic: Some(topic(3)),
            prefix: None,
        },
    )
    .await;
    assert_eq!(recv(&mut b).await, Frame::Ok { id: Some(9) });
    send(
        &mut a,
        &Frame::Epub {
            fs: 1,
            topic: topic(4),
            data: b"other".to_vec(),
        },
    )
    .await;
    send(
        &mut a,
        &Frame::Epub {
            fs: 1,
            topic: topic(3),
            data: b"typing".to_vec(),
        },
    )
    .await;
    match recv(&mut b).await {
        Frame::Eph {
            id,
            topic: t,
            data,
            sender,
        } => {
            assert_eq!((id, t, data), (9, topic(3), b"typing".to_vec()));
            assert_eq!(sender, admin.device.public().fingerprint().to_vec());
        }
        other => panic!("{other:?}"),
    }
    // Ephemeral messages are never stored.
    let log: LogEvents = h
        .call(
            "/v1/log/read",
            Some(&a_tok),
            &LogRead {
                fs: 1,
                topic: topic(3),
                after: None,
                key_token: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(log.events.is_empty());
}

async fn fetch(h: &Harness, path: &str, accept: Option<&str>) -> reqwest::Response {
    let mut rb = h.http.get(format!("{}{}", h.base, path));
    if let Some(a) = accept {
        rb = rb.header("accept", a);
    }
    rb.send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn static_files_aliases_fallback_and_safety() {
    let h = Harness::start().await;
    let r = fetch(&h, "/", None).await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("require-trusted-types-for")
    );
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert!(
        r.headers().get("cross-origin-opener-policy").is_none(),
        "isolation is off by default"
    );
    let etag = r.headers()["etag"].clone();
    let r = h
        .http
        .get(format!("{}/", h.base))
        .header("if-none-match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    assert_eq!(fetch(&h, "/favicon.ico", None).await.status(), 200);
    assert_eq!(
        fetch(&h, "/unencrypted/style.css", None).await.status(),
        200
    );
    // SPA fallback only for HTML navigations.
    assert_eq!(
        fetch(&h, "/some/app/route", Some("text/html,*/*"))
            .await
            .status(),
        200
    );
    assert_eq!(
        fetch(&h, "/some/app/route", Some("application/json"))
            .await
            .status(),
        404
    );
    assert_eq!(fetch(&h, "/v1/nope", Some("text/html")).await.status(), 404);

    // A configured directory: traversal and symlink escapes are refused.
    let dir = tempfile::tempdir().unwrap();
    let www = dir.path().join("www");
    std::fs::create_dir(&www).unwrap();
    std::fs::write(www.join("index.html"), "<p>custom</p>").unwrap();
    std::fs::write(www.join("sw.js"), "// sw").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("secret.txt"), www.join("link.txt")).unwrap();
    let h2 = Harness::start_with(|c| {
        c.unencrypted_dir = Some(www.clone());
        c.cross_origin_isolation = true;
    })
    .await;
    let r = fetch(&h2, "/", None).await;
    assert_eq!(r.headers()["cross-origin-opener-policy"], "same-origin");
    assert_eq!(r.headers()["cross-origin-embedder-policy"], "require-corp");
    assert_eq!(r.text().await.unwrap(), "<p>custom</p>");
    let r = fetch(&h2, "/unencrypted/sw.js", None).await;
    assert_eq!(r.headers()["service-worker-allowed"], "/");
    assert_eq!(
        fetch(&h2, "/unencrypted/../secret.txt", None)
            .await
            .status(),
        404
    );
    assert_eq!(
        fetch(&h2, "/unencrypted/%2e%2e/secret.txt", None)
            .await
            .status(),
        404
    );
    #[cfg(unix)]
    assert_eq!(
        fetch(&h2, "/unencrypted/link.txt", None).await.status(),
        404
    );
    // Isolation headers are on API responses too, and reported in /v1/info.
    let r = fetch(&h2, "/v1/info", None).await;
    assert_eq!(r.headers()["cross-origin-resource-policy"], "same-origin");
    let info: Info = from_cbor(&r.bytes().await.unwrap()).unwrap();
    assert!(info.cross_origin_isolation);
}
