//! The server-merged CRDT filesystem (spec/fs.md): tree semantics, late
//! moves (undo/redo), convergence against a reference model, content
//! registers and chunks, limits, the change feed, the sweeper.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use zen_core::fs as zfs;
use zen_proto::*;

const ROOT: [u8; 16] = zfs::ROOT;
type Id = [u8; 16];
type Dev32 = [u8; 32];
/// `(hlc, device, node, parent)`.
type MoveRec = (u64, Dev32, Id, Id);
/// `(hlc, device, node, meta)`.
type MetaRec = (u64, Dev32, Id, Vec<u8>);
/// Node → (parent, meta).
type TreeView = BTreeMap<Id, (Option<Id>, Option<Vec<u8>>)>;
const TRASH: [u8; 16] = zfs::TRASH;

fn rcid() -> Vec<u8> {
    let mut c = vec![0u8; 16];
    getrandom::fill(&mut c).unwrap();
    c
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn id(n: u8) -> [u8; 16] {
    [n; 16]
}

/// A signed-in device working on one tree of fs 1.
struct Dev {
    tok: Vec<u8>,
    fp: [u8; 32],
}

impl Dev {
    async fn new(h: &Harness, u: &User) -> Self {
        Dev {
            tok: h.sign_in(u).await.unwrap(),
            fp: u.device.public().fingerprint(),
        }
    }
}

const TREE: [u8; 16] = [0x7E; 16];

fn mv(node: [u8; 16], parent: [u8; 16], hlc: u64, meta: Option<&[u8]>) -> CrdtOp {
    CrdtOp::Move {
        fs: 1,
        tree: TREE.to_vec(),
        node: node.to_vec(),
        parent: parent.to_vec(),
        hlc,
        meta: meta.map(<[u8]>::to_vec),
    }
}

fn meta(node: [u8; 16], hlc: u64, m: &[u8]) -> CrdtOp {
    CrdtOp::Meta {
        fs: 1,
        tree: TREE.to_vec(),
        node: node.to_vec(),
        hlc,
        meta: m.to_vec(),
    }
}

fn write(node: [u8; 16], replaces: &[Vec<u8>], chunks: &[[u8; 16]], manifest: &[u8]) -> CrdtOp {
    CrdtOp::Write {
        fs: 1,
        tree: TREE.to_vec(),
        node: node.to_vec(),
        replaces: replaces.iter().map(|d| ByteBuf::from(d.clone())).collect(),
        chunks: chunks.iter().map(|c| ByteBuf::from(c.to_vec())).collect(),
        pages: vec![],
        manifest: manifest.to_vec(),
    }
}

fn write_paged(
    node: [u8; 16],
    replaces: &[Vec<u8>],
    pages: &[[u8; 16]],
    manifest: &[u8],
) -> CrdtOp {
    CrdtOp::Write {
        fs: 1,
        tree: TREE.to_vec(),
        node: node.to_vec(),
        replaces: replaces.iter().map(|d| ByteBuf::from(d.clone())).collect(),
        chunks: vec![],
        pages: pages.iter().map(|c| ByteBuf::from(c.to_vec())).collect(),
        manifest: manifest.to_vec(),
    }
}

async fn ops(h: &Harness, d: &Dev, ops: Vec<CrdtOp>) -> Result<CommitResult, ApiErr> {
    let c = Commit {
        commit_id: rcid(),
        crdt_ops: ops,
        ..Default::default()
    };
    h.call("/v1/commit", Some(&d.tok), &c).await
}

async fn get(h: &Harness, d: &Dev, nodes: &[[u8; 16]]) -> BTreeMap<[u8; 16], NodeState> {
    let r: Nodes = h
        .call(
            "/v1/fs/tree/get",
            Some(&d.tok),
            &TreeGet {
                fs: 1,
                tree: TREE.to_vec(),
                nodes: nodes.iter().map(|n| ByteBuf::from(n.to_vec())).collect(),
                read_version: None,
            },
        )
        .await
        .unwrap();
    r.nodes
        .into_iter()
        .map(|s| (s.node[..].try_into().unwrap(), s))
        .collect()
}

async fn children(h: &Harness, d: &Dev, parent: [u8; 16]) -> Vec<[u8; 16]> {
    let r: Nodes = h
        .call(
            "/v1/fs/tree/children",
            Some(&d.tok),
            &TreeChildren {
                fs: 1,
                tree: TREE.to_vec(),
                parent: parent.to_vec(),
                after: None,
                limit: None,
                read_version: None,
            },
        )
        .await
        .unwrap();
    r.nodes
        .iter()
        .map(|s| s.node[..].try_into().unwrap())
        .collect()
}

fn parent_of(s: &NodeState) -> Option<[u8; 16]> {
    s.parent.as_ref().map(|p| p[..].try_into().unwrap())
}

/// Every node's (parent, meta) through a full change-feed sync.
async fn full_sync(h: &Harness, d: &Dev) -> TreeView {
    let mut out = BTreeMap::new();
    let mut after = None;
    loop {
        let c: Changes = h
            .call(
                "/v1/fs/tree/changes",
                Some(&d.tok),
                &TreeChanges {
                    fs: 1,
                    tree: TREE.to_vec(),
                    after: after.clone(),
                    limit: Some(3),
                    wait_ms: None,
                },
            )
            .await
            .unwrap();
        for ch in &c.changes {
            if let Some(s) = &ch.state {
                out.insert(
                    s.node[..].try_into().unwrap(),
                    (parent_of(s), s.meta.clone()),
                );
            }
        }
        after = c.cursor;
        if !c.more {
            return out;
        }
    }
}

async fn two_devices(f: impl FnOnce(&mut zen_server::config::Config)) -> (Harness, Dev, Dev) {
    let h = Harness::start_with(f).await;
    let (a, b) = (User::new(1), User::new(2));
    h.claim(&a, &[&b]).await;
    let (da, db) = (Dev::new(&h, &a).await, Dev::new(&h, &b).await);
    (h, da, db)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_move_rename_delete_and_cycles() {
    let (h, a, b) = two_devices(|_| {}).await;
    let t = zfs::hlc(now_ms(), 0);
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, Some(b"dirA")),
            mv(id(2), id(1), t + 1, Some(b"fileF")),
            mv(id(3), id(1), t + 2, Some(b"dirB")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(children(&h, &a, ROOT).await, vec![id(1)]);
    let mut kids = children(&h, &a, id(1)).await;
    kids.sort();
    assert_eq!(kids, vec![id(2), id(3)]);

    // Move F to ROOT, rename it, then a cycle: A into its own child B.
    ops(
        &h,
        &a,
        vec![mv(id(2), ROOT, t + 3, None), meta(id(2), t + 4, b"renamed")],
    )
    .await
    .unwrap();
    ops(&h, &a, vec![mv(id(1), id(3), t + 5, None)])
        .await
        .unwrap();
    let s = get(&h, &a, &[id(1), id(2), id(3)]).await;
    assert_eq!(parent_of(&s[&id(1)]), Some(ROOT), "cycle move skipped");
    assert_eq!(parent_of(&s[&id(2)]), Some(ROOT));
    assert_eq!(s[&id(2)].meta.as_deref(), Some(&b"renamed"[..]));
    assert_eq!(s[&id(2)].move_device, a.fp.to_vec());

    // Concurrent rename (b, later ts) and move (a): both survive.
    ops(&h, &b, vec![meta(id(2), t + 7, b"bob-name")])
        .await
        .unwrap();
    ops(&h, &a, vec![mv(id(2), id(3), t + 6, None)])
        .await
        .unwrap();
    let s = get(&h, &a, &[id(2)]).await;
    assert_eq!(parent_of(&s[&id(2)]), Some(id(3)));
    assert_eq!(s[&id(2)].meta.as_deref(), Some(&b"bob-name"[..]));
    // An older meta loses.
    ops(&h, &a, vec![meta(id(2), t + 6, b"older")])
        .await
        .unwrap();
    assert_eq!(
        get(&h, &a, &[id(2)]).await[&id(2)].meta.as_deref(),
        Some(&b"bob-name"[..])
    );

    // Delete = move to trash; restore = move out.
    ops(&h, &a, vec![mv(id(3), TRASH, t + 8, None)])
        .await
        .unwrap();
    assert_eq!(children(&h, &a, TRASH).await, vec![id(3)]);
    ops(&h, &a, vec![mv(id(3), ROOT, t + 9, None)])
        .await
        .unwrap();
    assert!(children(&h, &a, TRASH).await.is_empty());

    // Refusals: unknown parent, unknown node, ROOT as node, reused timestamp.
    for (op, why) in [
        (mv(id(9), id(42), t + 10, None), "unknown parent"),
        (meta(id(42), t + 10, b"x"), "unknown node"),
        (mv(ROOT, id(1), t + 10, None), "ROOT"),
        (mv(id(2), ROOT, t + 9, None), "reused"),
    ] {
        let e = ops(&h, &a, vec![op]).await.unwrap_err();
        assert_eq!(e.0, 400, "{why}: {e:?}");
    }
    // The same hlc from another device is a different timestamp.
    ops(&h, &b, vec![mv(id(2), ROOT, t + 9, None)])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn late_moves_undo_and_redo() {
    let (h, a, b) = two_devices(|_| {}).await;
    let t = zfs::hlc(now_ms() - 60_000, 0);
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, None),
            mv(id(2), ROOT, t + 1, None),
            mv(id(3), ROOT, t + 2, None),
        ],
    )
    .await
    .unwrap();
    // Online: A into B at t+20. Late (offline) and earlier: B into A at t+10.
    ops(&h, &a, vec![mv(id(1), id(2), t + 20, None)])
        .await
        .unwrap();
    ops(&h, &b, vec![mv(id(2), id(1), t + 10, None)])
        .await
        .unwrap();
    // In timestamp order: B under A, then A into B is a cycle and skipped.
    let s = get(&h, &a, &[id(1), id(2)]).await;
    assert_eq!(parent_of(&s[&id(2)]), Some(id(1)));
    assert_eq!(parent_of(&s[&id(1)]), Some(ROOT));
    // An older offline move doesn't override a newer online one.
    ops(&h, &a, vec![mv(id(3), id(1), t + 40, None)])
        .await
        .unwrap();
    ops(&h, &b, vec![mv(id(3), id(2), t + 30, None)])
        .await
        .unwrap();
    assert_eq!(parent_of(&get(&h, &a, &[id(3)]).await[&id(3)]), Some(id(1)));
}

/// Reference: apply every move in timestamp order (Kleppmann et al.).
fn reference(moves: &[MoveRec], metas: &[MetaRec]) -> TreeView {
    let mut sorted = moves.to_vec();
    sorted.sort_by_key(|x| (x.0, x.1));
    let mut parent: BTreeMap<[u8; 16], [u8; 16]> = BTreeMap::new();
    for (_, _, node, p) in sorted {
        // Skip the move if `node` is `p` or one of its ancestors.
        let mut cur = Some(p);
        let mut cycle = false;
        while let Some(c) = cur {
            if c == node {
                cycle = true;
                break;
            }
            cur = parent.get(&c).copied();
        }
        if !cycle {
            parent.insert(node, p);
        }
    }
    let mut m: BTreeMap<Id, ((u64, Dev32), Vec<u8>)> = BTreeMap::new();
    for (hlc, dev, node, v) in metas {
        let ts = (*hlc, *dev);
        if m.get(node).is_none_or(|(old, _)| ts > *old) {
            m.insert(*node, (ts, v.clone()));
        }
    }
    parent
        .iter()
        .map(|(n, p)| (*n, (Some(*p), m.get(n).map(|x| x.1.clone()))))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random moves and metas from three devices, submitted in different
/// arrival orders, always converge to the reference state (G23).
#[tokio::test(flavor = "multi_thread")]
async fn convergence_matches_reference_in_any_arrival_order() {
    const NODES: u8 = 8;
    const OPS: usize = 60;
    for seed in [7u64, 99, 12345] {
        let h = Harness::start().await;
        let users = [User::new(1), User::new(2), User::new(3)];
        h.claim(&users[0], &[&users[1], &users[2]]).await;
        let mut devs = Vec::new();
        for u in &users {
            devs.push(Dev::new(&h, u).await);
        }
        let mut rng = Rng(seed);
        let base = now_ms() - 3_600_000;
        // Every node is created first, under ROOT, by device 0.
        let mut moves = Vec::new();
        let mut metas = Vec::new();
        let mut creates = Vec::new();
        for n in 1..=NODES {
            let hlc = zfs::hlc(base, n as u16);
            creates.push(mv(id(n), ROOT, hlc, None));
            moves.push((hlc, devs[0].fp, id(n), ROOT));
        }
        ops(&h, &devs[0], creates).await.unwrap();
        // Random later ops with distinct timestamps per device.
        let mut pending = Vec::new();
        for i in 0..OPS {
            let d = rng.below(3) as usize;
            let hlc = zfs::hlc(base + 10_000 + rng.below(1_000_000), i as u16);
            let node = id(1 + rng.below(NODES as u64) as u8);
            if rng.below(4) == 0 {
                let v = vec![i as u8; 8];
                metas.push((hlc, devs[d].fp, node, v.clone()));
                pending.push((d, meta(node, hlc, &v)));
            } else {
                let p = match rng.below(NODES as u64 + 2) {
                    0 => ROOT,
                    1 => TRASH,
                    k if id(k as u8 - 1) == node => ROOT,
                    k => id(k as u8 - 1),
                };
                moves.push((hlc, devs[d].fp, node, p));
                pending.push((d, mv(node, p, hlc, None)));
            }
        }
        // Shuffle the arrival order.
        for i in (1..pending.len()).rev() {
            pending.swap(i, rng.below(i as u64 + 1) as usize);
        }
        for (d, op) in pending {
            ops(&h, &devs[d], vec![op]).await.unwrap();
        }
        let got = full_sync(&h, &devs[0]).await;
        assert_eq!(got, reference(&moves, &metas), "seed {seed}");
        // The children index agrees with the parents.
        for (n, (p, _)) in &got {
            assert!(children(&h, &devs[0], p.unwrap()).await.contains(n));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn content_versions_siblings_and_chunks() {
    let (h, a, b) = two_devices(|_| {}).await;
    let t = zfs::hlc(now_ms(), 0);
    ops(&h, &a, vec![mv(id(1), ROOT, t, Some(b"f"))])
        .await
        .unwrap();
    // Chunks first, in their own commit; then the write.
    let (c1, c2) = (id(0xC1), id(0xC2));
    let up = Commit {
        commit_id: rcid(),
        chunks: vec![
            ChunkPut {
                fs: 1,
                id: c1.to_vec(),
                data: vec![1; 100],
            },
            ChunkPut {
                fs: 1,
                id: c2.to_vec(),
                data: vec![2; 100],
            },
        ],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&a.tok), &up).await.unwrap();
    let r = ops(&h, &a, vec![write(id(1), &[], &[c1, c2], b"manifest-1")])
        .await
        .unwrap();
    assert_eq!(r.dots.len(), 1);
    let d1 = r.dots[0].to_vec();
    assert_eq!(&d1[..10], &r.versionstamp[..]);

    // Concurrent writes that both saw d1 leave two siblings.
    let ra = ops(
        &h,
        &a,
        vec![write(id(1), std::slice::from_ref(&d1), &[c1], b"by-a")],
    )
    .await
    .unwrap();
    let rb = ops(
        &h,
        &b,
        vec![write(id(1), std::slice::from_ref(&d1), &[c2], b"by-b")],
    )
    .await
    .unwrap();
    let v: Versions = h
        .call(
            "/v1/fs/file/get",
            Some(&a.tok),
            &FileGet {
                fs: 1,
                tree: TREE.to_vec(),
                node: id(1).to_vec(),
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(v.versions.len(), 2);
    assert_eq!(get(&h, &a, &[id(1)]).await[&id(1)].versions, 2);
    let mut dots: Vec<_> = v.versions.iter().map(|x| x.dot.clone()).collect();
    dots.sort();
    let mut want = vec![ra.dots[0].to_vec(), rb.dots[0].to_vec()];
    want.sort();
    assert_eq!(dots, want);
    assert!(
        v.versions
            .iter()
            .any(|x| x.manifest == b"by-b" && x.device == b.fp.to_vec())
    );
    // A write that saw both resolves the conflict.
    ops(&h, &a, vec![write(id(1), &dots, &[c1, c2], b"merged")])
        .await
        .unwrap();
    let v: Versions = h
        .call(
            "/v1/fs/file/get",
            Some(&a.tok),
            &FileGet {
                fs: 1,
                tree: TREE.to_vec(),
                node: id(1).to_vec(),
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(v.versions.len(), 1);
    assert_eq!(v.versions[0].manifest, b"merged");
    assert_eq!(v.versions[0].chunks.len(), 2);

    // Chunks read back; an unknown chunk is refused in a write; an id holds
    // one chunk forever, but re-uploading the same bytes is fine.
    let ch: Chunks = h
        .call(
            "/v1/fs/chunks/get",
            Some(&b.tok),
            &ChunksGet {
                fs: 1,
                ids: vec![ByteBuf::from(c1.to_vec()), ByteBuf::from(id(0xEE).to_vec())],
            },
        )
        .await
        .unwrap();
    assert_eq!(ch.chunks[0].data.as_deref(), Some(&[1u8; 100][..]));
    assert_eq!(ch.chunks[1].data, None);
    let e = ops(&h, &a, vec![write(id(1), &[], &[id(0xEE)], b"m")])
        .await
        .unwrap_err();
    assert_eq!(e.0, 400);
    let _: CommitResult = h.call("/v1/commit", Some(&a.tok), &up).await.unwrap();
    let mut clash = up.clone();
    clash.commit_id = rcid();
    clash.chunks[0].data = vec![9; 100];
    assert_eq!(
        h.call::<_, CommitResult>("/v1/commit", Some(&a.tok), &clash)
            .await
            .unwrap_err()
            .0,
        400
    );

    // Upload and write in one commit, replayed by commit_id: same dots.
    let c = Commit {
        commit_id: rcid(),
        chunks: vec![ChunkPut {
            fs: 1,
            id: id(0xC3).to_vec(),
            data: vec![3; 10],
        }],
        crdt_ops: vec![write(id(1), &[], &[id(0xC3)], b"again")],
        ..Default::default()
    };
    let r1: CommitResult = h.call("/v1/commit", Some(&a.tok), &c).await.unwrap();
    let r2: CommitResult = h.call("/v1/commit", Some(&a.tok), &c).await.unwrap();
    assert_eq!(r1.dots, r2.dots);
    // Two writes to one node in one commit are refused.
    let e = ops(
        &h,
        &a,
        vec![write(id(1), &[], &[], b"x"), write(id(1), &[], &[], b"y")],
    )
    .await
    .unwrap_err();
    assert_eq!(e.0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn clock_limits_and_rebase() {
    let (h, a, _) = two_devices(|c| c.limits.crdt_max_redo = 2).await;
    let now = now_ms();
    let e = ops(
        &h,
        &a,
        vec![mv(id(1), ROOT, zfs::hlc(now + 600_000, 0), None)],
    )
    .await
    .unwrap_err();
    assert_eq!(e.1.code, "clock_skew");
    let e = ops(
        &h,
        &a,
        vec![mv(id(1), ROOT, zfs::hlc(now - 8 * 86_400_000, 0), None)],
    )
    .await
    .unwrap_err();
    assert_eq!(e.1.code, "stale_op");
    // Three later moves; a late move needing to undo all three is refused,
    // and goes through once rebased with a fresh hlc.
    let t = zfs::hlc(now - 10_000, 0);
    ops(
        &h,
        &a,
        vec![mv(id(1), ROOT, t, None), mv(id(2), ROOT, t + 1, None)],
    )
    .await
    .unwrap();
    for i in 0..3 {
        ops(&h, &a, vec![mv(id(1), ROOT, t + 10 + i, None)])
            .await
            .unwrap();
    }
    let e = ops(&h, &a, vec![mv(id(2), id(1), t + 5, None)])
        .await
        .unwrap_err();
    assert_eq!(e.1.code, "stale_op");
    ops(&h, &a, vec![mv(id(2), id(1), zfs::hlc(now_ms(), 0), None)])
        .await
        .unwrap();
    assert_eq!(parent_of(&get(&h, &a, &[id(2)]).await[&id(2)]), Some(id(1)));
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_and_op_chain() {
    let h = Harness::start().await;
    let (admin, carol) = (User::new(1), User::new(3));
    let grants = vec![
        fs_grant(&admin, 1, &["read", "write"]),
        fs_grant(&carol, 1, &["read"]),
    ];
    let (acl, _) = signed_acl(
        &admin,
        1,
        None,
        &[&admin],
        &[&admin, &carol],
        grants,
        vec![],
    );
    h.put_acl(acl, h.server.claim_token.clone()).await.unwrap();
    let a = Dev::new(&h, &admin).await;
    let c = Dev::new(&h, &carol).await;
    let t = zfs::hlc(now_ms(), 0);
    let first = vec![mv(id(1), ROOT, t, Some(b"m1")), meta(id(1), t + 1, b"m2")];
    ops(&h, &a, first.clone()).await.unwrap();
    // Read-only carol can read but not write; fs 2 is not hers.
    assert_eq!(
        ops(&h, &c, vec![mv(id(2), ROOT, t + 2, None)])
            .await
            .unwrap_err()
            .0,
        403
    );
    assert_eq!(children(&h, &c, ROOT).await, vec![id(1)]);
    let e = h
        .call::<_, Trees>("/v1/fs/tree/list", Some(&c.tok), &TreeList { fs: 2 })
        .await
        .unwrap_err();
    assert_eq!(e.0, 403);
    let trees: Trees = h
        .call("/v1/fs/tree/list", Some(&c.tok), &TreeList { fs: 1 })
        .await
        .unwrap();
    assert_eq!(
        trees.trees,
        vec![TreeEntry {
            tree: TREE.to_vec(),
            ops: 2
        }]
    );

    // The op chain matches a client-side recomputation (formats.md §11.5).
    let chain: TreeChain = h
        .call(
            "/v1/fs/tree/chain",
            Some(&c.tok),
            &TreeRef {
                fs: 1,
                tree: TREE.to_vec(),
            },
        )
        .await
        .unwrap();
    let mut want = [0u8; 32];
    want = zfs::chain_next(
        &want,
        &zfs::move_bytes(1, &TREE, &id(1), &ROOT, t, b"m1"),
        &a.fp,
    );
    want = zfs::chain_next(
        &want,
        &zfs::meta_bytes(1, &TREE, &id(1), t + 1, b"m2"),
        &a.fp,
    );
    assert_eq!((chain.ops, chain.chain), (2, want.to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn change_feed_cursor_and_long_poll() {
    let (h, a, _) = two_devices(|_| {}).await;
    let t = zfs::hlc(now_ms(), 0);
    ops(
        &h,
        &a,
        vec![mv(id(1), ROOT, t, None), mv(id(2), ROOT, t + 1, None)],
    )
    .await
    .unwrap();
    let all: Changes = h
        .call(
            "/v1/fs/tree/changes",
            Some(&a.tok),
            &TreeChanges {
                fs: 1,
                tree: TREE.to_vec(),
                after: None,
                limit: None,
                wait_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(all.changes.len(), 2);
    let cursor = all.cursor.clone();
    // Nothing new: an immediate empty answer keeps the cursor.
    let none: Changes = h
        .call(
            "/v1/fs/tree/changes",
            Some(&a.tok),
            &TreeChanges {
                fs: 1,
                tree: TREE.to_vec(),
                after: cursor.clone(),
                limit: None,
                wait_ms: None,
            },
        )
        .await
        .unwrap();
    assert!(none.changes.is_empty());
    assert_eq!(none.cursor, cursor);
    // A long-poll wakes on the next change, which is the only one returned.
    let (http, url, tok) = (
        h.http.clone(),
        format!("{}/v1/fs/tree/changes", h.base),
        a.tok.clone(),
    );
    let req = TreeChanges {
        fs: 1,
        tree: TREE.to_vec(),
        after: cursor,
        limit: None,
        wait_ms: Some(10_000),
    };
    let waiter = tokio::spawn(async move {
        let started = Instant::now();
        let resp = http
            .post(url)
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &tok)
                ),
            )
            .body(to_cbor(&req))
            .send()
            .await
            .unwrap();
        (
            started.elapsed(),
            from_cbor::<Changes>(&resp.bytes().await.unwrap()).unwrap(),
        )
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    ops(&h, &a, vec![mv(id(2), id(1), t + 2, None)])
        .await
        .unwrap();
    let (took, c) = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(took < Duration::from_secs(5));
    assert_eq!(c.changes.len(), 1);
    assert_eq!(c.changes[0].node, id(2).to_vec());
    assert_eq!(c.changes[0].state.as_ref().and_then(parent_of), Some(id(1)));

    // Content changes are changes, including an overwrite that leaves the
    // node with one version as before.
    let changes_after = |after: Option<Vec<u8>>| {
        let req = TreeChanges {
            fs: 1,
            tree: TREE.to_vec(),
            after,
            limit: None,
            wait_ms: None,
        };
        let h = &h;
        let tok = a.tok.clone();
        async move {
            h.call::<_, Changes>("/v1/fs/tree/changes", Some(&tok), &req)
                .await
                .unwrap()
        }
    };
    let r = ops(&h, &a, vec![write(id(2), &[], &[], b"v1")])
        .await
        .unwrap();
    let c1 = changes_after(c.cursor.clone()).await;
    assert_eq!(c1.changes.len(), 1);
    assert_eq!(c1.changes[0].node, id(2).to_vec());
    ops(
        &h,
        &a,
        vec![write(id(2), &[r.dots[0].to_vec()], &[], b"v2")],
    )
    .await
    .unwrap();
    let c2 = changes_after(c1.cursor.clone()).await;
    assert_eq!(c2.changes.len(), 1, "an overwrite is a change");
    assert_eq!(c2.changes[0].node, id(2).to_vec());
    assert!(c2.changes[0].offset > c1.changes[0].offset);
    assert_eq!(c2.changes[0].state.as_ref().unwrap().versions, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn sweeper_purges_trash_and_collects_chunks() {
    let (h, a, _) = two_devices(|c| {
        c.limits.crdt_horizon_secs = 2;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.chunk_grace_secs = 0;
        c.limits.sweep_interval_secs = 1;
    })
    .await;
    let t = zfs::hlc(now_ms(), 0);
    let c = Commit {
        commit_id: rcid(),
        chunks: vec![
            ChunkPut {
                fs: 1,
                id: id(0xC1).to_vec(),
                data: vec![1; 50],
            },
            ChunkPut {
                fs: 1,
                id: id(0xC9).to_vec(),
                data: vec![9; 50],
            },
        ],
        crdt_ops: vec![
            mv(id(1), ROOT, t, Some(b"dir")),
            mv(id(2), id(1), t + 1, Some(b"file")),
            write(id(2), &[], &[id(0xC1)], b"m"),
            mv(id(3), ROOT, t + 2, Some(b"keep")),
            write(id(3), &[], &[id(0xC9)], b"m"),
        ],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&a.tok), &c).await.unwrap();
    ops(&h, &a, vec![mv(id(1), TRASH, t + 3, None)])
        .await
        .unwrap();
    let before: Changes = h
        .call(
            "/v1/fs/tree/changes",
            Some(&a.tok),
            &TreeChanges {
                fs: 1,
                tree: TREE.to_vec(),
                after: None,
                limit: None,
                wait_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(before.ops, Some(6));
    let chain0 = tree_chain(&h, &a).await;
    assert_eq!(
        (chain0.ops, Some(chain0.chain.clone())),
        (6, before.chain.clone())
    );
    let chunk = |n: u8| ChunksGet {
        fs: 1,
        ids: vec![ByteBuf::from(id(n).to_vec())],
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = get(&h, &a, &[id(1), id(2), id(3)]).await;
        let gone: Chunks = h
            .call("/v1/fs/chunks/get", Some(&a.tok), &chunk(0xC1))
            .await
            .unwrap();
        if !s.contains_key(&id(1)) && !s.contains_key(&id(2)) && gone.chunks[0].data.is_none() {
            assert!(s.contains_key(&id(3)), "live nodes stay");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "trash purged and chunk collected: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let kept: Chunks = h
        .call("/v1/fs/chunks/get", Some(&a.tok), &chunk(0xC9))
        .await
        .unwrap();
    assert!(kept.chunks[0].data.is_some(), "referenced chunks stay");
    // The purge round extended the op chain by one record (formats.md
    // §11.5): the subtree, leaves first, by the server.
    let chain1 = tree_chain(&h, &a).await;
    let want = zfs::chain_next(
        &chain0.chain.clone().try_into().unwrap(),
        &zfs::purge_bytes(1, &TREE, &[id(2), id(1)]),
        &zfs::PURGE_DEVICE,
    );
    assert_eq!((chain1.ops, chain1.chain.clone()), (7, want.to_vec()));
    let mut seen_head = false;
    // The old cursor eventually needs a full resync (tombstones dropped);
    // until then it sees the tombstones.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = h
            .call::<_, Changes>(
                "/v1/fs/tree/changes",
                Some(&a.tok),
                &TreeChanges {
                    fs: 1,
                    tree: TREE.to_vec(),
                    after: before.cursor.clone(),
                    limit: None,
                    wait_ms: None,
                },
            )
            .await;
        match r {
            Ok(c) => {
                assert!(
                    c.changes.iter().all(|c| c.state.is_none()),
                    "only tombstones: {c:?}"
                );
                // A caught-up response names the new head.
                assert_eq!(c.ops, Some(7));
                assert_eq!(c.chain.as_deref(), Some(&want[..]));
                seen_head = true;
            }
            Err(e) => {
                assert_eq!(e.1.code, "resync");
                break;
            }
        }
        assert!(Instant::now() < deadline, "tombstones dropped");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen_head, "the tombstones were seen before the resync");
    let s = full_sync(&h, &a).await;
    assert_eq!(s.keys().copied().collect::<Vec<_>>(), vec![id(3)]);
}

async fn tree_chain(h: &Harness, u: &Dev) -> TreeChain {
    h.call(
        "/v1/fs/tree/chain",
        Some(&u.tok),
        &TreeRef {
            fs: 1,
            tree: TREE.to_vec(),
        },
    )
    .await
    .unwrap()
}

/// A skipped (cycle) move can name a node that is purged later; a late move
/// that undoes and redoes past it must still apply.
#[tokio::test(flavor = "multi_thread")]
async fn late_move_past_a_purged_node() {
    const HORIZON_MS: u64 = 8_000;
    let (h, a, _) = two_devices(|c| {
        c.limits.crdt_horizon_secs = HORIZON_MS / 1000;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.sweep_interval_secs = 1;
    })
    .await;
    let t = zfs::hlc(now_ms(), 0);
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, Some(b"x")),
            mv(id(2), id(1), t + 1, Some(b"d")),
            mv(id(5), ROOT, t + 2, Some(b"z")),
            mv(id(1), TRASH, t + 3, None),
        ],
    )
    .await
    .unwrap();
    // Halfway through the horizon, a stale replica moves X under its own
    // child D: skipped, but logged with a recent timestamp.
    tokio::time::sleep(Duration::from_millis(HORIZON_MS / 2)).await;
    let skipped = zfs::hlc(now_ms(), 0);
    ops(&h, &a, vec![mv(id(1), id(2), skipped, None)])
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while get(&h, &a, &[id(1), id(2)]).await.contains_key(&id(1)) {
        assert!(Instant::now() < deadline, "trash purged");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A late move before the skipped one undoes and redoes it.
    ops(&h, &a, vec![mv(id(5), ROOT, skipped - 1, Some(b"z2"))])
        .await
        .expect("a late move past a purged node applies");
    let s = full_sync(&h, &a).await;
    assert_eq!(s.keys().copied().collect::<Vec<_>>(), vec![id(5)]);
    assert_eq!(s[&id(5)].1.as_deref(), Some(&b"z2"[..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_across_nodes() {
    if !on_fdb() {
        return;
    }
    let (h, a, _) = two_devices(|_| {}).await;
    let peer = h.peer().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while peer
        .call::<_, Trees>("/v1/fs/tree/list", Some(&a.tok), &TreeList { fs: 1 })
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let req = TreeChanges {
        fs: 1,
        tree: TREE.to_vec(),
        after: None,
        limit: None,
        wait_ms: Some(10_000),
    };
    let (http, url, tok) = (
        peer.http.clone(),
        format!("{}/v1/fs/tree/changes", peer.base),
        a.tok.clone(),
    );
    let waiter = tokio::spawn(async move {
        let resp = http
            .post(url)
            .header(
                "authorization",
                format!(
                    "Bearer {}",
                    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &tok)
                ),
            )
            .body(to_cbor(&req))
            .send()
            .await
            .unwrap();
        from_cbor::<Changes>(&resp.bytes().await.unwrap()).unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    ops(&h, &a, vec![mv(id(1), ROOT, zfs::hlc(now_ms(), 0), None)])
        .await
        .unwrap();
    let c = tokio::time::timeout(Duration::from_secs(8), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.changes.len(), 1);
}

async fn chunk_exists(h: &Harness, d: &Dev, c: [u8; 16]) -> bool {
    let r: Chunks = h
        .call(
            "/v1/fs/chunks/get",
            Some(&d.tok),
            &ChunksGet {
                fs: 1,
                ids: vec![ByteBuf::from(c.to_vec())],
            },
        )
        .await
        .unwrap();
    r.chunks[0].data.is_some()
}

async fn put_chunk(h: &Harness, d: &Dev, c: [u8; 16]) {
    let commit = Commit {
        commit_id: rcid(),
        chunks: vec![ChunkPut {
            fs: 1,
            id: c.to_vec(),
            data: vec![c[0]; 40],
        }],
        ..Default::default()
    };
    let _: CommitResult = h.call("/v1/commit", Some(&d.tok), &commit).await.unwrap();
}

/// The grace period of an unreferenced chunk restarts on re-upload and on
/// every release: the sweeper only acts on a chunk's newest GC candidate,
/// so an older candidate past the grace period does not delete it.
#[tokio::test(flavor = "multi_thread")]
async fn chunk_grace_restarts_on_reupload_and_release() {
    let (h, a, _) = two_devices(|c| {
        c.limits.chunk_grace_secs = 3;
        c.limits.sweep_interval_secs = 3600; // swept by hand below
    })
    .await;
    let st = &h.server.state;
    // Re-upload.
    put_chunk(&h, &a, id(0xA1)).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    put_chunk(&h, &a, id(0xA1)).await; // restarts the grace period
    tokio::time::sleep(Duration::from_millis(2100)).await;
    // The first candidate is older than the grace period, the second is not.
    zen_server::sweep_once(st).await.unwrap();
    assert!(
        chunk_exists(&h, &a, id(0xA1)).await,
        "re-upload restarted grace"
    );
    // Release, re-reference, release.
    let t = zfs::hlc(now_ms(), 0);
    let r = ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, Some(b"f")),
            write(id(1), &[], &[id(0xA1)], b"m1"),
        ],
    )
    .await
    .unwrap();
    let d1 = r.dots[0].to_vec();
    let r = ops(&h, &a, vec![write(id(1), &[d1], &[], b"m2")])
        .await
        .unwrap(); // released
    let d2 = r.dots[0].to_vec();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let r = ops(&h, &a, vec![write(id(1), &[d2], &[id(0xA1)], b"m3")])
        .await
        .unwrap();
    let d3 = r.dots[0].to_vec();
    let _ = ops(&h, &a, vec![write(id(1), &[d3], &[], b"m4")])
        .await
        .unwrap(); // released again
    tokio::time::sleep(Duration::from_millis(2100)).await;
    zen_server::sweep_once(st).await.unwrap();
    assert!(
        chunk_exists(&h, &a, id(0xA1)).await,
        "release restarted grace"
    );
    // Once the newest candidate is past the grace period, the chunk goes.
    // The grace is measured in commit versions, which on an idle
    // FoundationDB cluster can trail the wall clock by a couple of seconds.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        zen_server::sweep_once(st).await.unwrap();
        if !chunk_exists(&h, &a, id(0xA1)).await {
            break;
        }
        assert!(Instant::now() < deadline, "collected after grace");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// While a purged node's tombstone is kept, operations naming it (as the
/// node or as a parent) are refused with `stale_op` instead of resurrecting
/// an empty node. Once the tombstone is dropped, the id is new again.
#[tokio::test(flavor = "multi_thread")]
async fn operations_on_purged_nodes_are_stale() {
    let (h, a, _) = two_devices(|c| {
        c.limits.crdt_horizon_secs = 2;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.sweep_interval_secs = 3600; // swept by hand below
    })
    .await;
    let st = &h.server.state;
    let t = zfs::hlc(now_ms(), 0);
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, Some(b"dir")),
            mv(id(2), id(1), t + 1, Some(b"file")),
            mv(id(1), TRASH, t + 2, None),
        ],
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2800)).await;
    zen_server::sweep_once(st).await.unwrap();
    assert!(get(&h, &a, &[id(1), id(2)]).await.is_empty(), "purged");
    let fresh = || zfs::hlc(now_ms(), 0);
    let stale = |r: Result<CommitResult, ApiErr>| {
        let e = r.expect_err("refused");
        assert_eq!((e.0, e.1.code.as_str()), (409, "stale_op"), "{e:?}");
    };
    stale(ops(&h, &a, vec![mv(id(2), ROOT, fresh(), None)]).await);
    stale(ops(&h, &a, vec![mv(id(1), ROOT, fresh(), Some(b"x"))]).await);
    stale(ops(&h, &a, vec![mv(id(9), id(2), fresh(), Some(b"new"))]).await);
    stale(ops(&h, &a, vec![meta(id(2), fresh(), b"m")]).await);
    stale(ops(&h, &a, vec![write(id(2), &[], &[], b"m")]).await);
    assert!(
        get(&h, &a, &[id(1), id(2), id(9)]).await.is_empty(),
        "nothing resurrected"
    );
    // An unknown parent that was never purged is still a plain 400.
    let e = ops(&h, &a, vec![mv(id(9), id(8), fresh(), None)])
        .await
        .expect_err("unknown parent");
    assert_eq!(e.0, 400);
    // Tombstones older than the horizon are dropped; the id is then new.
    // Tombstone age is measured in commit versions, which on an idle
    // FoundationDB cluster can trail the wall clock by a couple of seconds.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        zen_server::sweep_once(st).await.unwrap();
        match ops(&h, &a, vec![mv(id(2), ROOT, fresh(), Some(b"again"))]).await {
            Ok(_) => break,
            Err(e) => assert_eq!(e.1.code, "stale_op", "{e:?}"),
        }
        assert!(Instant::now() < deadline, "tombstone dropped");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(parent_of(&get(&h, &a, &[id(2)]).await[&id(2)]), Some(ROOT));
}

/// A move whose parent has more than `crdt_max_depth` ancestors is refused.
#[tokio::test(flavor = "multi_thread")]
async fn depth_is_bounded() {
    let (h, a, _) = two_devices(|c| c.limits.crdt_max_depth = 3).await;
    let t = zfs::hlc(now_ms(), 0);
    // ROOT → 1 → 2 → 3 → 4: node 4's parent has three ancestors below ROOT.
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, None),
            mv(id(2), id(1), t + 1, None),
            mv(id(3), id(2), t + 2, None),
            mv(id(4), id(3), t + 3, None),
        ],
    )
    .await
    .unwrap();
    let e = ops(&h, &a, vec![mv(id(5), id(4), t + 4, None)])
        .await
        .expect_err("too deep");
    assert_eq!(e.0, 400);
    assert!(get(&h, &a, &[id(5)]).await.is_empty());
    let info: Info = h.get("/v1/info").await;
    assert_eq!(info.limits.crdt_max_depth, 3);
}

/// The sweeper visits only the trees in its index: a move lists the tree,
/// and the sweeper drops it once its move log, trash and tombstones are
/// gone. Data from before the index is listed once by a backfill.
#[tokio::test(flavor = "multi_thread")]
async fn sweeper_visits_only_indexed_trees() {
    use zen_server::keys;
    let (h, a, _) = two_devices(|c| {
        c.limits.crdt_horizon_secs = 2;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.sweep_interval_secs = 3600; // swept by hand below
    })
    .await;
    let st = &h.server.state;
    let store = st.store.clone();
    let has = |k: Vec<u8>| {
        let store = store.clone();
        async move {
            let mut t = store.begin(None).await.unwrap();
            t.get(&k).await.unwrap().is_some()
        }
    };
    let clear = |k: Vec<u8>| {
        let store = store.clone();
        async move {
            let mut t = store.begin(None).await.unwrap();
            t.clear(&k);
            t.commit().await.unwrap();
        }
    };
    let (entry, ready) = (keys::sweep_needed(1, &TREE), keys::sweep_index_ready(1));
    let t = zfs::hlc(now_ms(), 0);
    ops(
        &h,
        &a,
        vec![
            mv(id(1), ROOT, t, Some(b"x")),
            mv(id(1), TRASH, t + 1, None),
        ],
    )
    .await
    .unwrap();
    assert!(has(entry.clone()).await, "a move lists the tree");
    zen_server::sweep_once(st).await.unwrap();
    assert!(
        has(ready.clone()).await,
        "the first sweep completes the index"
    );
    assert!(has(entry.clone()).await, "work left: still listed");

    // A tree missing from a complete index is not visited.
    clear(entry.clone()).await;
    tokio::time::sleep(Duration::from_millis(2800)).await;
    zen_server::sweep_once(st).await.unwrap();
    assert!(!get(&h, &a, &[id(1)]).await.is_empty(), "not visited");

    // Without the marker (data from before the index), a sweep lists every
    // tree again; then the trash is purged and, once its tombstone is
    // dropped (an age in versions, polled), the tree leaves the index.
    clear(ready.clone()).await;
    zen_server::sweep_once(st).await.unwrap();
    assert!(has(ready.clone()).await);
    assert!(get(&h, &a, &[id(1)]).await.is_empty(), "purged");
    let deadline = Instant::now() + Duration::from_secs(30);
    while has(entry.clone()).await {
        assert!(Instant::now() < deadline, "the tree leaves the index");
        tokio::time::sleep(Duration::from_millis(250)).await;
        zen_server::sweep_once(st).await.unwrap();
    }
    assert!(!has(keys::trash_cursor(1, &TREE)).await);

    // New work lists it again.
    ops(&h, &a, vec![mv(id(2), ROOT, zfs::hlc(now_ms(), 0), None)])
        .await
        .unwrap();
    assert!(has(entry.clone()).await);
}

// ------------------------------------------------------------ paged versions

fn chunk_put(c: [u8; 16]) -> ChunkPut {
    ChunkPut {
        fs: 1,
        id: c.to_vec(),
        data: vec![c[0]; 40],
    }
}

fn page_put(p: [u8; 16], chunks: &[[u8; 16]]) -> PagePut {
    PagePut {
        fs: 1,
        id: p.to_vec(),
        data: zfs::encode_page(chunks),
    }
}

async fn upload(
    h: &Harness,
    d: &Dev,
    chunks: Vec<ChunkPut>,
    pages: Vec<PagePut>,
    ops: Vec<CrdtOp>,
) -> Result<CommitResult, ApiErr> {
    let c = Commit {
        commit_id: rcid(),
        chunks,
        pages,
        crdt_ops: ops,
        ..Default::default()
    };
    h.call("/v1/commit", Some(&d.tok), &c).await
}

async fn page_data(h: &Harness, d: &Dev, p: [u8; 16]) -> Option<Vec<u8>> {
    let r: Pages = h
        .call(
            "/v1/fs/pages/get",
            Some(&d.tok),
            &PagesGet {
                fs: 1,
                ids: vec![ByteBuf::from(p.to_vec())],
            },
        )
        .await
        .unwrap();
    r.pages.into_iter().next().unwrap().data
}

async fn file(h: &Harness, d: &Dev, node: [u8; 16]) -> Versions {
    h.call(
        "/v1/fs/file/get",
        Some(&d.tok),
        &FileGet {
            fs: 1,
            tree: TREE.to_vec(),
            node: node.to_vec(),
            read_version: None,
        },
    )
    .await
    .unwrap()
}

async fn quota_bytes(h: &Harness) -> i64 {
    let mut t = h.server.state.store.begin(None).await.unwrap();
    t.get(&zen_server::keys::quota(1, "bytes"))
        .await
        .unwrap()
        .map_or(0, |v| i64::from_le_bytes(v.try_into().unwrap()))
}

fn ids(v: &[ByteBuf]) -> Vec<[u8; 16]> {
    v.iter().map(|b| b[..].try_into().unwrap()).collect()
}

/// A paged version round-trips through `file/get` and `pages/get`, its op
/// is chained as `write_paged`, and malformed uploads and writes are refused.
#[tokio::test(flavor = "multi_thread")]
async fn paged_versions_round_trip_and_refusals() {
    let (h, a, _) = two_devices(|c| c.limits.sweep_interval_secs = 3600).await;
    let (p1, p2, p3) = (id(0xB1), id(0xB2), id(0xB3));
    let c: Vec<[u8; 16]> = (0xC1..=0xC5).map(id).collect();
    upload(
        &h,
        &a,
        c[..4].iter().map(|&c| chunk_put(c)).collect(),
        vec![page_put(p1, &c[..2]), page_put(p2, &c[2..4])],
        vec![],
    )
    .await
    .unwrap();
    // The last chunk, its page and the write in one commit.
    let t = zfs::hlc(now_ms(), 0);
    upload(
        &h,
        &a,
        vec![chunk_put(c[4])],
        vec![page_put(p3, &c[4..])],
        vec![
            mv(id(1), ROOT, t, Some(b"big")),
            write_paged(id(1), &[], &[p1, p2, p3], b"manifest"),
        ],
    )
    .await
    .unwrap();
    let v = file(&h, &a, id(1)).await;
    assert_eq!(v.versions.len(), 1);
    assert!(v.versions[0].chunks.is_empty());
    assert_eq!(ids(&v.versions[0].pages), [p1, p2, p3]);
    assert_eq!(v.versions[0].manifest, b"manifest");
    assert_eq!(
        page_data(&h, &a, p2).await,
        Some(zfs::encode_page(&c[2..4]))
    );
    assert_eq!(page_data(&h, &a, id(0xBF)).await, None);
    // The op chain holds the paged write as `write_paged` (formats.md §11.5).
    let chain: TreeChain = h
        .call(
            "/v1/fs/tree/chain",
            Some(&a.tok),
            &TreeRef {
                fs: 1,
                tree: TREE.to_vec(),
            },
        )
        .await
        .unwrap();
    let mut want = zfs::chain_next(
        &[0; 32],
        &zfs::move_bytes(1, &TREE, &id(1), &ROOT, t, b"big"),
        &a.fp,
    );
    want = zfs::chain_next(
        &want,
        &zfs::write_paged_bytes(1, &TREE, &id(1), &[], &[p1, p2, p3], b"manifest"),
        &a.fp,
    );
    assert_eq!(chain.chain, want.to_vec());

    let err = |r: Result<CommitResult, ApiErr>| {
        let e = r.unwrap_err();
        (e.0, e.1.message)
    };
    // A write naming an unknown page.
    let (status, msg) = err(upload(
        &h,
        &a,
        vec![],
        vec![],
        vec![write_paged(id(1), &[], &[id(0xBE)], b"m")],
    )
    .await);
    assert_eq!((status, msg.as_str()), (400, "unknown page"));
    // A page listing an unknown chunk.
    let (status, msg) = err(upload(
        &h,
        &a,
        vec![],
        vec![page_put(id(0xB4), &[id(0xCE)])],
        vec![],
    )
    .await);
    assert_eq!((status, msg.as_str()), (400, "unknown chunk in page"));
    // Malformed, or other bytes under an existing id.
    let bad = PagePut {
        fs: 1,
        id: id(0xB5).to_vec(),
        data: vec![0, 0, 0, 2, 1],
    };
    assert_eq!(err(upload(&h, &a, vec![], vec![bad], vec![]).await).0, 400);
    let (status, msg) = err(upload(&h, &a, vec![], vec![page_put(p1, &c[..1])], vec![]).await);
    assert_eq!(
        (status, msg.as_str()),
        (400, "page id already holds another page")
    );
    // Identical bytes again are fine.
    upload(&h, &a, vec![], vec![page_put(p1, &c[..2])], vec![])
        .await
        .unwrap();
    // A page over max_value_bytes (validated before any chunk is looked up).
    let many: Vec<[u8; 16]> = (0..5_625u32).map(|_| id(0xC1)).collect();
    assert_eq!(
        err(upload(&h, &a, vec![], vec![page_put(id(0xB6), &many)], vec![]).await).0,
        413
    );
    // A write with both chunks and pages.
    let both = CrdtOp::Write {
        fs: 1,
        tree: TREE.to_vec(),
        node: id(1).to_vec(),
        replaces: vec![],
        chunks: vec![ByteBuf::from(c[0].to_vec())],
        pages: vec![ByteBuf::from(p1.to_vec())],
        manifest: b"m".to_vec(),
    };
    assert_eq!(err(upload(&h, &a, vec![], vec![], vec![both]).await).0, 400);
    // `/v1/info` advertises the feature.
    let info: Info = h.get("/v1/info").await;
    assert!(info.features.iter().any(|f| f == "fs_pages"));
}

/// Sweep until exactly `want_pages` of `pages` and `want_chunks` of
/// `chunks` exist, checking after every sweep that no page outlives a chunk
/// it lists (pages are collected first). With a grace of 0 this can still
/// take a few sweeps: on FoundationDB, commit versions can trail the clock
/// the grace is measured with.
async fn sweep_until(
    h: &Harness,
    a: &Dev,
    pages: &[([u8; 16], Vec<[u8; 16]>)],
    chunks: &[[u8; 16]],
    want_pages: &[[u8; 16]],
    want_chunks: &[[u8; 16]],
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        zen_server::sweep_once(&h.server.state).await.unwrap();
        let mut done = true;
        for (p, list) in pages {
            let present = page_data(h, a, *p).await.is_some();
            if present {
                for c in list {
                    assert!(
                        chunk_exists(h, a, *c).await,
                        "page {p:02x?} outlived chunk {c:02x?}"
                    );
                }
            }
            done &= present == want_pages.contains(p);
        }
        for c in chunks {
            done &= chunk_exists(h, a, *c).await == want_chunks.contains(c);
        }
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "swept to pages {want_pages:02x?}, chunks {want_chunks:02x?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Pages shared between versions keep their chunks; replaced pages are
/// collected first and their chunks one grace period later; a re-upload
/// doesn't count a page's chunks twice; quota follows.
#[tokio::test(flavor = "multi_thread")]
async fn shared_pages_and_gc_order() {
    let (h, a, _) = two_devices(|c| {
        c.limits.chunk_grace_secs = 0;
        c.limits.sweep_interval_secs = 3600; // swept by hand below
    })
    .await;
    let n = id(1);
    ops(&h, &a, vec![mv(n, ROOT, zfs::hlc(now_ms(), 0), Some(b"f"))])
        .await
        .unwrap();
    let q0 = quota_bytes(&h).await;
    let c: Vec<[u8; 16]> = (0xC1..=0xC6).map(id).collect();
    let (a1, a2, b2, u) = (id(0xA1), id(0xA2), id(0xB2), id(0xEE));
    // Version A: [c1 c2][c3].
    let r = upload(
        &h,
        &a,
        c.iter().map(|&c| chunk_put(c)).collect(),
        vec![page_put(a1, &c[..2]), page_put(a2, &c[2..3])],
        vec![write_paged(n, &[], &[a1, a2], b"vA")],
    )
    .await
    .unwrap();
    let da = r.dots[0].to_vec();
    // Version B appends: [c1 c2][c3 c4], reusing a1 and replacing A. A page
    // nothing ever references, and c6, referenced by nothing at all.
    let r = upload(
        &h,
        &a,
        vec![],
        vec![page_put(b2, &c[2..4]), page_put(u, &c[4..5])],
        vec![write_paged(n, &[da], &[a1, b2], b"vB")],
    )
    .await
    .unwrap();
    let db = r.dots[0].to_vec();
    let pages = [
        (a1, c[..2].to_vec()),
        (a2, c[2..3].to_vec()),
        (b2, c[2..4].to_vec()),
        (u, c[4..5].to_vec()),
    ];
    // a2 and u go, with c5 (only u listed it) and c6 (nothing did); a1
    // (shared) and b2 stay, and B reads fully.
    sweep_until(&h, &a, &pages, &c, &[a1, b2], &c[..4]).await;
    // Re-uploading a1 restarts its grace without counting its chunks again:
    // once B is replaced, every page and chunk goes.
    upload(&h, &a, vec![], vec![page_put(a1, &c[..2])], vec![])
        .await
        .unwrap();
    upload(&h, &a, vec![], vec![], vec![write(n, &[db], &[], b"vC")])
        .await
        .unwrap();
    sweep_until(&h, &a, &pages, &c, &[], &[]).await;
    assert_eq!(quota_bytes(&h).await, q0 + b"vC".len() as i64);
}

/// Purging a node releases the pages of its versions, then their chunks.
#[tokio::test(flavor = "multi_thread")]
async fn purge_releases_pages() {
    let (h, a, _) = two_devices(|c| {
        c.limits.crdt_horizon_secs = 2;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.chunk_grace_secs = 0;
        c.limits.sweep_interval_secs = 3600;
    })
    .await;
    let t = zfs::hlc(now_ms(), 0);
    let (p, c) = (id(0xA1), id(0xC1));
    upload(
        &h,
        &a,
        vec![chunk_put(c)],
        vec![page_put(p, &[c])],
        vec![
            mv(id(1), ROOT, t, Some(b"f")),
            write_paged(id(1), &[], &[p], b"m"),
        ],
    )
    .await
    .unwrap();
    ops(&h, &a, vec![mv(id(1), TRASH, t + 1, None)])
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        zen_server::sweep_once(&h.server.state).await.unwrap();
        if page_data(&h, &a, p).await.is_none() && !chunk_exists(&h, &a, c).await {
            break;
        }
        assert!(Instant::now() < deadline, "page and chunk collected");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A live version in the reference model: its dot, whether it is paged,
/// and its chunk or page ids.
type LiveVersion = (Vec<u8>, bool, Vec<[u8; 16]>);

/// Random uploads and inline, paged and sibling writes: after the sweeps,
/// exactly the pages of live versions and the chunks of live inline
/// versions and live pages remain (three seeds).
#[tokio::test(flavor = "multi_thread")]
async fn paged_gc_matches_reference() {
    use std::collections::{BTreeSet, HashMap};
    let (h, a, _) = two_devices(|c| {
        c.limits.chunk_grace_secs = 0;
        c.limits.sweep_interval_secs = 3600;
    })
    .await;
    for seed in [1u64, 2, 3] {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let tagged = |tag: u8, i: usize| {
            let mut b = [tag; 16];
            b[0] = seed as u8;
            b[1..3].copy_from_slice(&(i as u16).to_be_bytes());
            b
        };
        let nodes: Vec<[u8; 16]> = (0..3).map(|i| tagged(0x10, i)).collect();
        let t = zfs::hlc(now_ms(), 0);
        ops(
            &h,
            &a,
            nodes
                .iter()
                .enumerate()
                .map(|(i, &n)| mv(n, ROOT, t + i as u64, None))
                .collect(),
        )
        .await
        .unwrap();
        let (mut chunks, mut pages) = (Vec::new(), Vec::<([u8; 16], Vec<[u8; 16]>)>::new());
        // Live versions per node: dot → (inline chunks, pages).
        let mut live: HashMap<[u8; 16], Vec<LiveVersion>> = HashMap::new();
        for _ in 0..60 {
            match rnd(5) {
                0 => {
                    let c = tagged(0xC0, chunks.len());
                    upload(&h, &a, vec![chunk_put(c)], vec![], vec![])
                        .await
                        .unwrap();
                    chunks.push(c);
                }
                1 if !chunks.is_empty() => {
                    let list: Vec<[u8; 16]> = (0..1 + rnd(3))
                        .map(|_| chunks[rnd(chunks.len() as u64) as usize])
                        .collect();
                    let p = tagged(0xA0, pages.len());
                    upload(&h, &a, vec![], vec![page_put(p, &list)], vec![])
                        .await
                        .unwrap();
                    pages.push((p, list));
                }
                k @ (2..=4) => {
                    let n = nodes[rnd(3) as usize];
                    let vs = live.entry(n).or_default();
                    // 2: replace every version; 3, 4: also add a sibling.
                    let replaces: Vec<Vec<u8>> = if k == 2 {
                        vs.iter().map(|v| v.0.clone()).collect()
                    } else {
                        Vec::new()
                    };
                    let paged = rnd(2) == 0 && !pages.is_empty();
                    let list: Vec<[u8; 16]> = if paged {
                        (0..1 + rnd(3))
                            .map(|_| pages[rnd(pages.len() as u64) as usize].0)
                            .collect()
                    } else if chunks.is_empty() {
                        Vec::new()
                    } else {
                        (0..rnd(3))
                            .map(|_| chunks[rnd(chunks.len() as u64) as usize])
                            .collect()
                    };
                    let op = if paged {
                        write_paged(n, &replaces, &list, b"m")
                    } else {
                        write(n, &replaces, &list, b"m")
                    };
                    let r = ops(&h, &a, vec![op]).await.unwrap();
                    if k == 2 {
                        vs.clear();
                    }
                    vs.push((r.dots[0].to_vec(), paged, list));
                }
                _ => {}
            }
        }
        let mut want_pages = BTreeSet::new();
        let mut want_chunks = BTreeSet::new();
        for vs in live.values() {
            for (_, paged, list) in vs {
                for &x in list {
                    if *paged {
                        want_pages.insert(x);
                    } else {
                        want_chunks.insert(x);
                    }
                }
            }
        }
        for (p, list) in &pages {
            if want_pages.contains(p) {
                want_chunks.extend(list.iter().copied());
            }
        }
        let want_pages: Vec<[u8; 16]> = want_pages.into_iter().collect();
        let want_chunks: Vec<[u8; 16]> = want_chunks.into_iter().collect();
        sweep_until(&h, &a, &pages, &chunks, &want_pages, &want_chunks).await;
        // Every live version still reads back as written.
        for (n, vs) in &live {
            let got = file(&h, &a, *n).await;
            assert_eq!(got.versions.len(), vs.len(), "seed {seed}");
        }
    }
}
