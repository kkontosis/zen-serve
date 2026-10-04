//! End-to-end API tests against a real server (spec/api.md).

mod common;

use common::*;
use std::time::Duration;
use zen_core::kdf::RangeHasher;
use zen_proto::*;

type R<T> = Result<T, ApiErr>;

fn code<T: std::fmt::Debug>(r: R<T>) -> (u16, String) {
    let (s, e) = r.expect_err("expected an error");
    (s, e.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn claim_acl_chain_and_cas() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let bob = User::new(2);
    let mallory = User::new(3);

    let info: Info = h.get("/v1/info").await;
    assert!(!info.claimed);

    let (v1, doc1) = signed_acl(
        &admin,
        1,
        None,
        &[&admin],
        &[&admin],
        vec![fs_grant(&admin, 1, &["read"])],
        vec![],
    );
    assert_eq!(
        code(h.put_acl(v1.clone(), None).await),
        (403, "forbidden".into())
    );
    assert_eq!(
        code(h.put_acl(v1.clone(), Some("wrong".into())).await),
        (403, "forbidden".into())
    );
    assert_eq!(
        h.put_acl(v1.clone(), h.server.claim_token.clone())
            .await
            .unwrap()
            .version,
        1
    );
    // Replaying version 1 fails the CAS.
    assert_eq!(
        code(h.put_acl(v1, h.server.claim_token.clone()).await).1,
        "version_mismatch"
    );

    // Version 2 signed by a non-admin is refused, even if it names them admin.
    let members = [&admin, &bob, &mallory];
    let (bad, _) = signed_acl(
        &mallory,
        2,
        Some(&doc1),
        &[&mallory],
        &members,
        vec![],
        vec![],
    );
    assert_eq!(code(h.put_acl(bad, None).await), (403, "forbidden".into()));
    // Wrong prev_hash.
    let (bad, _) = signed_acl(&admin, 2, None, &[&admin], &members, vec![], vec![]);
    assert_eq!(code(h.put_acl(bad, None).await).1, "version_mismatch");
    // A proper successor.
    let (v2, _) = signed_acl(&admin, 2, Some(&doc1), &[&admin], &members, vec![], vec![]);
    assert_eq!(h.put_acl(v2, None).await.unwrap().version, 2);

    let tok = h.sign_in(&bob).await.unwrap();
    let chain: AclEntries = h
        .call("/v1/acl/get", Some(&tok), &AclGet { from: Some(1) })
        .await
        .unwrap();
    assert_eq!(chain.head, 2);
    assert_eq!(chain.entries.len(), 2);
    let info: Info = h.get("/v1/info").await;
    assert!(info.claimed);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_and_permissions() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let bob = User::new(2);
    let stranger = User::new(9);
    let members = [&admin, &bob];
    let grants = vec![
        fs_grant(&admin, 1, &["read", "write"]),
        fs_grant(&bob, 1, &["read"]),
    ];
    let (v1, doc1) = signed_acl(&admin, 1, None, &[&admin], &members, grants, vec![]);
    h.put_acl(v1, h.server.claim_token.clone()).await.unwrap();

    assert_eq!(
        code(h.sign_in(&stranger).await),
        (401, "unauthorized".into())
    );
    let no_auth: R<ReadVersion> = h.call("/v1/grv", None, &Empty {}).await;
    assert_eq!(code(no_auth).0, 401);

    let bob_tok = h.sign_in(&bob).await.unwrap();
    let get = KvGet {
        fs: 1,
        keys: vec![ByteBuf::from(vec![1; 16])],
        read_version: None,
    };
    let r: KvItems = h.call("/v1/kv/get", Some(&bob_tok), &get).await.unwrap();
    assert_eq!(r.items[0].value, None);
    // Bob cannot write, cannot read fs 2, and cannot append.
    let c = Commit {
        commit_id: cid(1),
        writes: vec![Write {
            fs: 1,
            key: vec![1; 16],
            value: Some(vec![0; 64]),
        }],
        ..Default::default()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&bob_tok), &c)
                .await
        ),
        (403, "forbidden".into())
    );
    let get2 = KvGet {
        fs: 2,
        ..get.clone()
    };
    assert_eq!(
        code(
            h.call::<_, KvItems>("/v1/kv/get", Some(&bob_tok), &get2)
                .await
        )
        .0,
        403
    );
    let ap = LogAppend {
        commit_id: cid(2),
        append: vec![append(&topic(1), None, b"x")],
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/log/append", Some(&bob_tok), &ap)
                .await
        )
        .0,
        403
    );
    // Unknown fs is 404.
    let get9 = KvGet {
        fs: 9,
        ..get.clone()
    };
    assert_eq!(
        code(
            h.call::<_, KvItems>("/v1/kv/get", Some(&bob_tok), &get9)
                .await
        )
        .0,
        404
    );

    // Removing Bob's device ends his session immediately.
    let (v2, _) = signed_acl(&admin, 2, Some(&doc1), &[&admin], &[&admin], vec![], vec![]);
    h.put_acl(v2, None).await.unwrap();
    assert_eq!(
        code(
            h.call::<_, KvItems>("/v1/kv/get", Some(&bob_tok), &get)
                .await
        )
        .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_origin_is_bound() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let c: Challenge = h.call("/v1/auth/challenge", None, &Empty {}).await.unwrap();
    let origin = "https://evil.example".to_string();
    let sig = admin
        .device
        .signing()
        .sign(
            zen_core::labels::SIG_SESSION,
            &session_message(&c.challenge, &origin),
        )
        .unwrap();
    let r: R<Session> = h
        .call(
            "/v1/auth/session",
            None,
            &SessionRequest {
                challenge: c.challenge,
                origin,
                user: admin.id.public().encode(),
                cert: admin.cert.clone(),
                sig,
            },
        )
        .await;
    assert_eq!(code(r).0, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_commit_and_idempotent_replay() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let id = cid(1);
    let c = Commit {
        commit_id: id.clone(),
        writes: vec![
            Write {
                fs: 1,
                key: b"k1".to_vec(),
                value: Some(b"v1".to_vec()),
            },
            Write {
                fs: 1,
                key: b"k2".to_vec(),
                value: Some(b"v2".to_vec()),
            },
        ],
        append: vec![append(&topic(1), None, b"e1")],
        ..Default::default()
    };
    let r1: CommitResult = h.call("/v1/commit", Some(&tok), &c).await.unwrap();
    let r2: CommitResult = h.call("/v1/commit", Some(&tok), &c).await.unwrap();
    assert_eq!(r1, r2, "resend returns the original result");
    assert_eq!(r1.appended.len(), 1);
    // Applied once: one event in the log.
    let log: LogEvents = h
        .call(
            "/v1/log/read",
            Some(&tok),
            &LogRead {
                fs: 1,
                topic: topic(1),
                after: None,
                key_token: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(log.events.len(), 1);
    assert_eq!(log.events[0].offset, r1.appended[0].to_vec());

    let r: KvItems = h
        .call(
            "/v1/kv/range",
            Some(&tok),
            &KvRange {
                fs: 1,
                begin: b"k".to_vec(),
                end: Some(b"l".to_vec()),
                limit: None,
                reverse: None,
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.items[0].value.as_deref(), Some(&b"v1"[..]));
    assert_eq!(r.items[0].version.as_deref(), Some(&r1.versionstamp[..]));

    // Another device reusing the commit id is refused.
    let bob = User::new(2);
    let (v2, _) = {
        let doc1 = {
            let e: AclEntries = h
                .call("/v1/acl/get", Some(&tok), &AclGet::default())
                .await
                .unwrap();
            let s: zen_proto::acl::SignedAcl = from_cbor(&e.entries[0]).unwrap();
            s.doc
        };
        signed_acl(
            &admin,
            2,
            Some(&doc1),
            &[&admin],
            &[&admin, &bob],
            vec![
                fs_grant(&bob, 1, &["read", "write"]),
                fs_grant(&admin, 1, &["read", "write"]),
            ],
            vec![],
        )
    };
    h.put_acl(v2, None).await.unwrap();
    let bob_tok = h.sign_in(&bob).await.unwrap();
    assert_eq!(
        code(
            h.call::<_, CommitResult>(
                "/v1/commit",
                Some(&bob_tok),
                &Commit {
                    commit_id: id,
                    ..Default::default()
                }
            )
            .await
        )
        .1,
        "commit_id_reused"
    );

    // crdt_ops are not implemented.
    let c = Commit {
        commit_id: cid(3),
        crdt_ops: vec![CborValue::Null],
        ..Default::default()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c)
                .await
        )
        .0,
        501
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn short_mode_conflict_exactly_one_wins() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let rv: ReadVersion = h.call("/v1/grv", Some(&tok), &Empty {}).await.unwrap();
    let mk = |n: u8| Commit {
        commit_id: cid(10 + n),
        read_version: Some(rv.read_version),
        read_conflicts: vec![FsRange {
            fs: 1,
            begin: b"counter".to_vec(),
            end: Some(b"counter\0".to_vec()),
        }],
        writes: vec![Write {
            fs: 1,
            key: b"counter".to_vec(),
            value: Some(vec![n]),
        }],
        ..Default::default()
    };
    let (c1, c2) = (mk(1), mk(2));
    let (a, b) = tokio::join!(
        h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c1),
        h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c2),
    );
    let oks = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(oks, 1, "exactly one commit wins: {a:?} {b:?}");
    let err = a.err().or(b.err()).unwrap();
    assert_eq!((err.0, err.1.code.as_str()), (409, "conflict"));

    // A read version from far in the past is too old.
    let c = Commit {
        read_version: Some(1),
        ..mk(3)
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c)
                .await
        ),
        (409, "too_old".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn long_mode_expect_and_phantoms() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let put = |id: u8, k: &[u8], v: &[u8]| Commit {
        commit_id: cid(id),
        writes: vec![Write {
            fs: 1,
            key: k.to_vec(),
            value: Some(v.to_vec()),
        }],
        ..Default::default()
    };
    let r1: CommitResult = h
        .call("/v1/commit", Some(&tok), &put(1, b"a/1", b"x"))
        .await
        .unwrap();
    // Expect absent → conflict; expect current version → ok.
    let mut c = put(2, b"a/1", b"y");
    c.expect = vec![Expect {
        fs: 1,
        key: b"a/1".to_vec(),
        version: None,
    }];
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c)
                .await
        ),
        (409, "conflict".into())
    );
    c.expect[0].version = Some(r1.versionstamp.clone());
    c.commit_id = cid(3);
    let r2: CommitResult = h.call("/v1/commit", Some(&tok), &c).await.unwrap();
    // The old version is now stale.
    c.commit_id = cid(4);
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c)
                .await
        )
        .1,
        "conflict"
    );

    // expect_ranges: hash the scanned range, then a phantom insert breaks it.
    let scan: KvItems = h
        .call(
            "/v1/kv/range",
            Some(&tok),
            &KvRange {
                fs: 1,
                begin: b"a/".to_vec(),
                end: Some(b"a0".to_vec()),
                limit: None,
                reverse: None,
                read_version: None,
            },
        )
        .await
        .unwrap();
    let mut hasher = RangeHasher::new();
    for it in &scan.items {
        hasher.update(&it.key, it.version.as_deref().unwrap().try_into().unwrap());
    }
    assert_eq!(scan.items[0].version.as_deref(), Some(&r2.versionstamp[..]));
    let guarded = |id: u8| Commit {
        commit_id: cid(id),
        expect_ranges: vec![ExpectRange {
            fs: 1,
            begin: b"a/".to_vec(),
            end: Some(b"a0".to_vec()),
            hash: hasher.finalize().to_vec(),
        }],
        writes: vec![Write {
            fs: 1,
            key: b"summary".to_vec(),
            value: Some(b"1 item".to_vec()),
        }],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&tok), &guarded(5)).await.unwrap();
    let _: CommitResult = h
        .call("/v1/commit", Some(&tok), &put(6, b"a/2", b"phantom"))
        .await
        .unwrap();
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &guarded(7))
                .await
        )
        .1,
        "conflict"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aborted_commit_publishes_nothing_and_quota() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let members = [&admin];
    let grants = vec![
        fs_grant(&admin, 1, &["read", "write"]),
        topic_grant(&admin, 1, &[], &["read", "append"]),
    ];
    let limits = vec![acl::FsLimit {
        fs: 1,
        max_keys: Some(2),
        max_bytes: None,
    }];
    let (v1, _) = signed_acl(&admin, 1, None, &[&admin], &members, grants, limits);
    h.put_acl(v1, h.server.claim_token.clone()).await.unwrap();
    let tok = h.sign_in(&admin).await.unwrap();
    let c = Commit {
        commit_id: cid(1),
        expect: vec![Expect {
            fs: 1,
            key: b"nope".to_vec(),
            version: Some(vec![0; 10]),
        }],
        append: vec![append(&topic(1), None, b"ghost")],
        ..Default::default()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &c)
                .await
        )
        .0,
        409
    );
    let log: LogEvents = h
        .call(
            "/v1/log/read",
            Some(&tok),
            &LogRead {
                fs: 1,
                topic: topic(1),
                after: None,
                key_token: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(
        log.events.is_empty(),
        "events of an aborted commit never exist"
    );

    // Key quota: 2 keys allowed.
    let w = |id: u8, keys: &[&[u8]]| Commit {
        commit_id: cid(id),
        writes: keys
            .iter()
            .map(|k| Write {
                fs: 1,
                key: k.to_vec(),
                value: Some(b"v".to_vec()),
            })
            .collect(),
        ..Default::default()
    };
    let _: CommitResult = h
        .call("/v1/commit", Some(&tok), &w(2, &[b"a", b"b"]))
        .await
        .unwrap();
    let _: CommitResult = h
        .call("/v1/commit", Some(&tok), &w(3, &[b"a"]))
        .await
        .unwrap(); // overwrite is fine
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &w(4, &[b"c"]))
                .await
        ),
        (429, "quota".into())
    );
    let del = Commit {
        commit_id: cid(5),
        clear_ranges: vec![FsRange {
            fs: 1,
            begin: b"a".to_vec(),
            end: Some(b"b".to_vec()),
        }],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&tok), &del).await.unwrap();
    let _: CommitResult = h
        .call("/v1/commit", Some(&tok), &w(6, &[b"c"]))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn log_order_and_key_index() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let mut offsets = Vec::new();
    for i in 0..5u8 {
        let k = key(i % 2);
        let ap = LogAppend {
            commit_id: cid(i),
            append: vec![
                append(&topic(1), Some(&k), &[i]),
                append(&topic(2), None, &[i]),
            ],
        };
        let r: CommitResult = h.call("/v1/log/append", Some(&tok), &ap).await.unwrap();
        offsets.push(r.appended[0].to_vec());
    }
    let read = |after: Option<Vec<u8>>, key_token: Option<Vec<u8>>, limit| LogRead {
        fs: 1,
        topic: topic(1),
        after,
        key_token,
        limit,
    };
    let all: LogEvents = h
        .call("/v1/log/read", Some(&tok), &read(None, None, None))
        .await
        .unwrap();
    assert_eq!(
        all.events.iter().map(|e| e.envelope[0]).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(
        all.events
            .iter()
            .map(|e| e.offset.clone())
            .collect::<Vec<_>>(),
        offsets
    );
    let page: LogEvents = h
        .call(
            "/v1/log/read",
            Some(&tok),
            &read(Some(offsets[1].clone()), None, Some(2)),
        )
        .await
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|e| e.envelope[0])
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(page.more);
    let k1: LogEvents = h
        .call("/v1/log/read", Some(&tok), &read(None, Some(key(1)), None))
        .await
        .unwrap();
    assert_eq!(
        k1.events.iter().map(|e| e.envelope[0]).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(k1.events[0].key_token.as_deref(), Some(&key(1)[..]));
}

fn group(name: &[u8], mode: Mode) -> GroupDef {
    GroupDef {
        fs: 1,
        group: name.to_vec(),
        topic: topic(1),
        mode,
        partitions: None,
        key_token: None,
        max_inflight: None,
        max_attempts: None,
        on_poison: None,
        start: None,
    }
}

fn consume(
    name: &[u8],
    d: &Delivery,
    partition: Option<u32>,
    key_token: Option<Vec<u8>>,
) -> Consume {
    Consume {
        fs: 1,
        group: name.to_vec(),
        partition,
        key_token,
        from: d.from.clone(),
        to: d.offset.clone(),
        token: d.token,
    }
}

async fn publish(h: &Harness, tok: &[u8], n: u8, k: Option<Vec<u8>>) -> Vec<u8> {
    let ap = LogAppend {
        commit_id: cid(100 + n),
        append: vec![append(&topic(1), k.as_deref(), &[n])],
    };
    let r: CommitResult = h.call("/v1/log/append", Some(tok), &ap).await.unwrap();
    r.appended[0].to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_gate_and_fencing() {
    let h = Harness::start().await;
    let admin = User::new(1);
    let bob = User::new(2);
    h.claim(&admin, &[&bob]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let bob_tok = h.sign_in(&bob).await.unwrap();
    let g = group(b"audit", Mode::Sequential);
    let c: GroupCreated = h.call("/v1/consume/groups", Some(&tok), &g).await.unwrap();
    assert!(c.created);
    let c: GroupCreated = h.call("/v1/consume/groups", Some(&tok), &g).await.unwrap();
    assert!(!c.created);
    let mut g2 = g.clone();
    g2.mode = Mode::PerKey;
    assert_eq!(
        code(
            h.call::<_, GroupCreated>("/v1/consume/groups", Some(&tok), &g2)
                .await
        )
        .1,
        "group_exists"
    );

    for i in 0..3 {
        publish(&h, &tok, i, None).await;
    }
    let lease: Lease = h
        .call(
            "/v1/consume/lease",
            Some(&tok),
            &LeaseRequest {
                fs: 1,
                group: b"audit".to_vec(),
                partition: None,
                token: None,
                ttl_ms: Some(300),
            },
        )
        .await
        .unwrap();
    // Bob cannot take a live lease.
    let r: R<Lease> = h
        .call(
            "/v1/consume/lease",
            Some(&bob_tok),
            &LeaseRequest {
                fs: 1,
                group: b"audit".to_vec(),
                partition: None,
                token: None,
                ttl_ms: None,
            },
        )
        .await;
    assert_eq!(code(r).1, "not_leader");
    let next = NextRequest {
        fs: 1,
        group: b"audit".to_vec(),
        partition: None,
        token: Some(lease.token),
        limit: Some(5),
        wait_ms: None,
    };
    let d: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(d.events.len(), 1, "delivery gate: max_inflight 1");
    assert_eq!(d.events[0].envelope, vec![0]);
    // Skipping ahead is refused.
    let d2: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(
        d2.events[0].offset, d.events[0].offset,
        "e+1 is withheld until e commits"
    );
    let ack = Commit {
        commit_id: cid(50),
        consume: vec![consume(b"audit", &d.events[0], None, None)],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&tok), &ack).await.unwrap();
    // Same consume again: cursor moved.
    let again = Commit {
        commit_id: cid(51),
        ..ack.clone()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &again)
                .await
        )
        .1,
        "cursor_moved"
    );
    let d: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(d.events[0].envelope, vec![1]);

    // The lease expires; Bob takes over with a higher token, and the stale
    // leader's commit is fenced off.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let bob_lease: Lease = h
        .call(
            "/v1/consume/lease",
            Some(&bob_tok),
            &LeaseRequest {
                fs: 1,
                group: b"audit".to_vec(),
                partition: None,
                token: None,
                ttl_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(bob_lease.token, lease.token + 1);
    assert_eq!(bob_lease.cursor, d.events[0].from);
    let late = Commit {
        commit_id: cid(52),
        writes: vec![Write {
            fs: 1,
            key: b"side-effect".to_vec(),
            value: Some(b"x".to_vec()),
        }],
        consume: vec![consume(b"audit", &d.events[0], None, None)],
        ..Default::default()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &late)
                .await
        ),
        (412, "not_leader".into())
    );
    let side: KvItems = h
        .call(
            "/v1/kv/get",
            Some(&tok),
            &KvGet {
                fs: 1,
                keys: vec![ByteBuf::from(b"side-effect".to_vec())],
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(side.items[0].value, None, "fenced commit applied nothing");
    let mut bob_ev = d.events[0].clone();
    bob_ev.token = bob_lease.token;
    let ok = Commit {
        commit_id: cid(53),
        consume: vec![consume(b"audit", &bob_ev, None, None)],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&bob_tok), &ok).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn per_key_parallel_across_keys_serial_within() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    // Events before the group exists are backfilled.
    publish(&h, &tok, 0, Some(key(1))).await; // A1
    let _: GroupCreated = h
        .call(
            "/v1/consume/groups",
            Some(&tok),
            &group(b"billing", Mode::PerKey),
        )
        .await
        .unwrap();
    publish(&h, &tok, 1, Some(key(1))).await; // A2
    publish(&h, &tok, 2, Some(key(2))).await; // B1
    let next = NextRequest {
        fs: 1,
        group: b"billing".to_vec(),
        partition: None,
        token: None,
        limit: Some(10),
        wait_ms: None,
    };
    let d: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    let got: Vec<u8> = d.events.iter().map(|e| e.envelope[0]).collect();
    assert_eq!(got, vec![0, 2], "one event per key, oldest first");
    // Claimed keys are not handed out again.
    let none: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert!(none.events.is_empty());
    let cur: Cursor = h
        .call(
            "/v1/consume/cursor",
            Some(&tok),
            &GroupRef {
                fs: 1,
                group: b"billing".to_vec(),
                partition: None,
                key_token: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(cur.low_watermark.as_deref(), Some(&d.events[0].offset[..]));
    // Commit A1 → A2 becomes ready.
    let a1 = &d.events[0];
    let ack = Commit {
        commit_id: cid(60),
        consume: vec![consume(b"billing", a1, None, Some(key(1)))],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&tok), &ack).await.unwrap();
    let d2: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(
        d2.events.iter().map(|e| e.envelope[0]).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(d2.events[0].from, a1.offset);
    // A stale claim token is refused.
    let mut stale = d2.events[0].clone();
    stale.token -= 1;
    let bad = Commit {
        commit_id: cid(61),
        consume: vec![consume(b"billing", &stale, None, Some(key(1)))],
        ..Default::default()
    };
    assert_eq!(
        code(
            h.call::<_, CommitResult>("/v1/commit", Some(&tok), &bad)
                .await
        )
        .1,
        "claim_lost"
    );
    // Long-poll wakes on a new append.
    let b1 = d.events[1].clone();
    let ack_b = Commit {
        commit_id: cid(62),
        consume: vec![consume(b"billing", &b1, None, Some(key(2)))],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&tok), &ack_b).await.unwrap();
    let waiter = {
        let next = NextRequest {
            wait_ms: Some(5_000),
            ..next.clone()
        };
        let http = h.http.clone();
        let url = format!("{}/v1/consume/next", h.base);
        let auth = format!(
            "Bearer {}",
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &tok)
        );
        tokio::spawn(async move {
            let resp = http
                .post(url)
                .header("authorization", auth)
                .body(to_cbor(&next))
                .send()
                .await
                .unwrap();
            from_cbor::<Deliveries>(&resp.bytes().await.unwrap()).unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish(&h, &tok, 3, Some(key(3))).await;
    let woke = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        woke.events
            .iter()
            .map(|e| e.envelope[0])
            .collect::<Vec<_>>(),
        vec![3]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn partitioned_and_single_key() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let mut g = group(b"parts", Mode::Partitioned);
    g.partitions = Some(2);
    let _: GroupCreated = h.call("/v1/consume/groups", Some(&tok), &g).await.unwrap();
    let mut s = group(b"one", Mode::SingleKey);
    s.key_token = Some(key(2));
    let _: GroupCreated = h.call("/v1/consume/groups", Some(&tok), &s).await.unwrap();
    for i in 0..6u8 {
        publish(&h, &tok, i, Some(key(i % 3))).await;
    }
    let p = |k: u8| {
        let v = u128::from_be_bytes(key(k).try_into().unwrap());
        (v % 2) as u32
    };
    for part in 0..2u32 {
        let lease: Lease = h
            .call(
                "/v1/consume/lease",
                Some(&tok),
                &LeaseRequest {
                    fs: 1,
                    group: b"parts".to_vec(),
                    partition: Some(part),
                    token: None,
                    ttl_ms: None,
                },
            )
            .await
            .unwrap();
        let mut seen = Vec::new();
        loop {
            let d: Deliveries = h
                .call(
                    "/v1/consume/next",
                    Some(&tok),
                    &NextRequest {
                        fs: 1,
                        group: b"parts".to_vec(),
                        partition: Some(part),
                        token: Some(lease.token),
                        limit: None,
                        wait_ms: None,
                    },
                )
                .await
                .unwrap();
            let Some(e) = d.events.first() else { break };
            seen.push(e.envelope[0]);
            let c = Commit {
                commit_id: cid(200 + seen.len() as u8 + 10 * part as u8),
                consume: vec![consume(b"parts", e, Some(part), None)],
                ..Default::default()
            };
            let _: CommitResult = h.call("/v1/commit", Some(&tok), &c).await.unwrap();
        }
        let want: Vec<u8> = (0..6u8).filter(|i| p(i % 3) == part).collect();
        assert_eq!(seen, want, "partition {part}");
    }
    let lease: Lease = h
        .call(
            "/v1/consume/lease",
            Some(&tok),
            &LeaseRequest {
                fs: 1,
                group: b"one".to_vec(),
                partition: None,
                token: None,
                ttl_ms: None,
            },
        )
        .await
        .unwrap();
    let d: Deliveries = h
        .call(
            "/v1/consume/next",
            Some(&tok),
            &NextRequest {
                fs: 1,
                group: b"one".to_vec(),
                partition: None,
                token: Some(lease.token),
                limit: None,
                wait_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(d.events[0].envelope, vec![2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn poison_events_go_to_the_dlq() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let mut g = group(b"jobs", Mode::Sequential);
    g.max_attempts = Some(2);
    let _: GroupCreated = h.call("/v1/consume/groups", Some(&tok), &g).await.unwrap();
    let bad = publish(&h, &tok, 0, None).await;
    publish(&h, &tok, 1, None).await;
    let lease: Lease = h
        .call(
            "/v1/consume/lease",
            Some(&tok),
            &LeaseRequest {
                fs: 1,
                group: b"jobs".to_vec(),
                partition: None,
                token: None,
                ttl_ms: None,
            },
        )
        .await
        .unwrap();
    let next = NextRequest {
        fs: 1,
        group: b"jobs".to_vec(),
        partition: None,
        token: Some(lease.token),
        limit: None,
        wait_ms: None,
    };
    let nack = Nack {
        fs: 1,
        group: b"jobs".to_vec(),
        partition: None,
        key_token: None,
        offset: bad.clone(),
        token: lease.token,
    };
    let r: NackResult = h.call("/v1/consume/nack", Some(&tok), &nack).await.unwrap();
    assert_eq!((r.attempts, r.dead_lettered), (1, false));
    let d: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(
        (d.events[0].offset.clone(), d.events[0].attempts),
        (bad.clone(), 1)
    );
    let r: NackResult = h.call("/v1/consume/nack", Some(&tok), &nack).await.unwrap();
    assert_eq!((r.attempts, r.dead_lettered), (2, true));
    // The cursor moved past the poison event.
    let d: Deliveries = h.call("/v1/consume/next", Some(&tok), &next).await.unwrap();
    assert_eq!(d.events[0].envelope, vec![1]);
    let dlq: DlqItems = h
        .call(
            "/v1/consume/dlq/list",
            Some(&tok),
            &DlqList {
                fs: 1,
                group: b"jobs".to_vec(),
                after: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(dlq.items.len(), 1);
    assert_eq!(
        (dlq.items[0].offset.clone(), dlq.items[0].envelope.clone()),
        (bad, vec![0])
    );
    // Retry re-appends it; the DLQ entry is gone.
    let retry = DlqOp {
        fs: 1,
        group: b"jobs".to_vec(),
        id: dlq.items[0].id.clone(),
        commit_id: Some(cid(90)),
    };
    let r: CommitResult = h
        .call("/v1/consume/dlq/retry", Some(&tok), &retry)
        .await
        .unwrap();
    let again: CommitResult = h
        .call("/v1/consume/dlq/retry", Some(&tok), &retry)
        .await
        .unwrap();
    assert_eq!(r, again);
    let dlq: DlqItems = h
        .call(
            "/v1/consume/dlq/list",
            Some(&tok),
            &DlqList {
                fs: 1,
                group: b"jobs".to_vec(),
                after: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(dlq.items.is_empty());
    let log: LogEvents = h
        .call(
            "/v1/log/read",
            Some(&tok),
            &LogRead {
                fs: 1,
                topic: topic(1),
                after: None,
                key_token: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        log.events.iter().map(|e| e.envelope[0]).collect::<Vec<_>>(),
        vec![0, 1, 0]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_headers_cas() {
    let h = Harness::start().await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let empty: Header = h
        .call("/v1/fs/header/get", Some(&tok), &HeaderGet { fs: 1 })
        .await
        .unwrap();
    assert_eq!(empty.header, None);
    let v1: HeaderVersion = h
        .call(
            "/v1/fs/header/put",
            Some(&tok),
            &HeaderPut {
                fs: 1,
                header: b"slots".to_vec(),
                expect: None,
            },
        )
        .await
        .unwrap();
    let r: R<HeaderVersion> = h
        .call(
            "/v1/fs/header/put",
            Some(&tok),
            &HeaderPut {
                fs: 1,
                header: b"x".to_vec(),
                expect: None,
            },
        )
        .await;
    assert_eq!(code(r).1, "version_mismatch");
    let _: HeaderVersion = h
        .call(
            "/v1/fs/header/put",
            Some(&tok),
            &HeaderPut {
                fs: 1,
                header: b"slots2".to_vec(),
                expect: Some(v1.version),
            },
        )
        .await
        .unwrap();
    let got: Header = h
        .call("/v1/fs/header/get", Some(&tok), &HeaderGet { fs: 1 })
        .await
        .unwrap();
    assert_eq!(got.header.as_deref(), Some(&b"slots2"[..]));
    let list: FsList = h.call("/v1/fs/list", Some(&tok), &Empty {}).await.unwrap();
    // Admins see every fs (to manage headers); data rights only on fs 1.
    assert_eq!(list.fs.len(), 2);
    assert_eq!(list.fs[0].rights, vec!["read", "write", "topics", "admin"]);
    assert_eq!(list.fs[1].rights, vec!["admin"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sweeper_expires_idempotency_records() {
    let h = Harness::start_with(|c| {
        c.limits.idempotency_ttl_secs = 0;
        c.limits.sweep_interval_secs = 1;
    })
    .await;
    let admin = User::new(1);
    h.claim(&admin, &[]).await;
    let tok = h.sign_in(&admin).await.unwrap();
    let c = LogAppend {
        commit_id: cid(1),
        append: vec![append(&topic(1), None, b"x")],
    };
    let r1: CommitResult = h.call("/v1/log/append", Some(&tok), &c).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // The record is gone, so the same commit_id is a new commit.
    let r2: CommitResult = h.call("/v1/log/append", Some(&tok), &c).await.unwrap();
    assert_ne!(r1.versionstamp, r2.versionstamp);
}
