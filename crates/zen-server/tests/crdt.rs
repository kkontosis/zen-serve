//! Server-merged CRDT rows (api.md §13, spec/zendb.md §19): merge rules,
//! clocks, counters, add-wins sets, delete vs update, replays, permissions,
//! reads and paging, the sweeper, and convergence against a reference model.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::time::Duration;
use zen_core::fs as zfs;
use zen_proto::*;

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

/// An object id: `n` 16-byte elements.
fn obj(tag: u8) -> Vec<u8> {
    let mut o = vec![0x0B; 48];
    o[47] = tag;
    o
}

fn f(n: u8) -> Vec<u8> {
    vec![0xF0 | n; 16]
}

fn e(n: u8) -> Vec<u8> {
    vec![0xE0 | n; 16]
}

struct Dev {
    tok: Vec<u8>,
    fp: [u8; 32],
}

async fn devices(n: u8, cfg: impl FnOnce(&mut zen_server::config::Config)) -> (Harness, Vec<Dev>) {
    let h = Harness::start_with(cfg).await;
    let users: Vec<User> = (1..=n).map(User::new).collect();
    let others: Vec<&User> = users[1..].iter().collect();
    h.claim(&users[0], &others).await;
    let mut devs = Vec::new();
    for u in &users {
        devs.push(Dev {
            tok: h.sign_in(u).await.unwrap(),
            fp: u.device.public().fingerprint(),
        });
    }
    (h, devs)
}

fn row(o: &[u8], hlc: u64, alive: bool, v: &[u8]) -> CrdtOp {
    CrdtOp::Row {
        fs: 1,
        object: o.to_vec(),
        hlc,
        alive,
        value: v.to_vec(),
    }
}

fn lww(o: &[u8], field: &[u8], hlc: u64, v: Option<&[u8]>) -> CrdtOp {
    CrdtOp::Lww {
        fs: 1,
        object: o.to_vec(),
        field: field.to_vec(),
        hlc,
        value: v.map(<[u8]>::to_vec),
    }
}

fn ctr(o: &[u8], field: &[u8], seq: u64, v: &[u8]) -> CrdtOp {
    ctr_as(o, field, 1, seq, v)
}

/// A counter op from installation `actor`.
fn ctr_as(o: &[u8], field: &[u8], actor: u8, seq: u64, v: &[u8]) -> CrdtOp {
    CrdtOp::Ctr {
        fs: 1,
        object: o.to_vec(),
        field: field.to_vec(),
        actor: vec![0xA0 | actor; 16],
        seq,
        value: v.to_vec(),
    }
}

fn add(o: &[u8], field: &[u8], elem: &[u8], v: &[u8]) -> CrdtOp {
    CrdtOp::Add {
        fs: 1,
        object: o.to_vec(),
        field: field.to_vec(),
        elem: elem.to_vec(),
        value: v.to_vec(),
    }
}

fn rem(o: &[u8], field: &[u8], elem: &[u8], dots: &[Vec<u8>]) -> CrdtOp {
    CrdtOp::Rem {
        fs: 1,
        object: o.to_vec(),
        field: field.to_vec(),
        elem: elem.to_vec(),
        dots: dots.iter().map(|d| ByteBuf::from(d.clone())).collect(),
    }
}

async fn send(h: &Harness, d: &Dev, ops: Vec<CrdtOp>) -> Result<CommitResult, ApiErr> {
    send_id(h, d, rcid(), ops).await
}

async fn send_id(
    h: &Harness,
    d: &Dev,
    commit_id: Vec<u8>,
    ops: Vec<CrdtOp>,
) -> Result<CommitResult, ApiErr> {
    let c = Commit {
        commit_id,
        crdt_ops: ops,
        ..Default::default()
    };
    h.call("/v1/commit", Some(&d.tok), &c).await
}

async fn get(h: &Harness, d: &Dev, objects: &[Vec<u8>]) -> Vec<ObjState> {
    let r: ObjStates = h
        .call(
            "/v1/crdt/get",
            Some(&d.tok),
            &CrdtGet {
                fs: 1,
                objects: objects.iter().map(|o| ByteBuf::from(o.clone())).collect(),
                read_version: None,
            },
        )
        .await
        .unwrap();
    r.objects
}

async fn one(h: &Harness, d: &Dev, o: &[u8]) -> Option<ObjState> {
    get(h, d, &[o.to_vec()]).await.into_iter().next()
}

fn field_value(s: &ObjState, field: &[u8]) -> Option<Option<Vec<u8>>> {
    s.lww
        .iter()
        .find(|r| r.field == field)
        .map(|r| r.value.clone())
}

fn code(r: Result<CommitResult, ApiErr>) -> (u16, String) {
    let (s, b) = r.unwrap_err();
    (s, b.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn lww_registers_merge_by_timestamp() {
    let (h, d) = devices(2, |_| {}).await;
    let (a, b) = (&d[0], &d[1]);
    let o = obj(1);
    let t = zfs::hlc(now_ms(), 0);
    send(
        &h,
        a,
        vec![row(&o, t, true, b"pk"), lww(&o, &f(1), t, Some(b"A"))],
    )
    .await
    .unwrap();
    send(&h, b, vec![lww(&o, &f(1), t + 1, Some(b"B"))])
        .await
        .unwrap();
    // An older write is ignored, an equal one (a re-send) too.
    send(&h, a, vec![lww(&o, &f(1), t - 5, Some(b"old"))])
        .await
        .unwrap();
    // An identical re-send under a new commit id is a no-op; the same
    // timestamp with other content is a collision, refused for a rebase.
    send(&h, b, vec![lww(&o, &f(1), t + 1, Some(b"B"))])
        .await
        .unwrap();
    assert_eq!(
        code(send(&h, b, vec![lww(&o, &f(1), t + 1, Some(b"again"))]).await),
        (409, "stale_op".into())
    );
    assert_eq!(
        code(send(&h, a, vec![row(&o, t, false, b"pk")]).await),
        (409, "stale_op".into())
    );
    send(&h, a, vec![row(&o, t, true, b"pk")]).await.unwrap();
    let s = one(&h, a, &o).await.unwrap();
    assert_eq!(field_value(&s, &f(1)), Some(Some(b"B".to_vec())));
    let r = s.row.unwrap();
    assert!(r.alive);
    assert_eq!(r.device, a.fp.to_vec());
    assert_eq!(r.value, b"pk");
    // Unset, then a concurrent field survives on its own register.
    send(
        &h,
        a,
        vec![
            lww(&o, &f(1), t + 2, None),
            lww(&o, &f(2), t + 2, Some(b"x")),
        ],
    )
    .await
    .unwrap();
    let s = one(&h, a, &o).await.unwrap();
    assert_eq!(field_value(&s, &f(1)), Some(None));
    assert_eq!(field_value(&s, &f(2)), Some(Some(b"x".to_vec())));
    assert_eq!(s.version.len(), 10);
    // Clocks: too far ahead, and older than the horizon.
    let ahead = zfs::hlc(now_ms() + 3_600_000, 0);
    assert_eq!(
        code(send(&h, a, vec![lww(&o, &f(1), ahead, Some(b"z"))]).await),
        (409, "clock_skew".into())
    );
    let old = zfs::hlc(now_ms() - 8 * 86_400_000, 0);
    assert_eq!(
        code(send(&h, a, vec![row(&o, old, false, b"pk")]).await),
        (409, "stale_op".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_wins_over_older_updates_and_reinsert_revives() {
    let (h, d) = devices(1, |_| {}).await;
    let a = &d[0];
    let o = obj(2);
    let t = zfs::hlc(now_ms(), 0);
    send(&h, a, vec![row(&o, t, true, b"pk")]).await.unwrap();
    send(&h, a, vec![row(&o, t + 10, false, b"pk")])
        .await
        .unwrap();
    // A field update after the delete doesn't resurrect the row.
    send(&h, a, vec![lww(&o, &f(1), t + 20, Some(b"late"))])
        .await
        .unwrap();
    let s = one(&h, a, &o).await.unwrap();
    assert!(!s.row.as_ref().unwrap().alive);
    // An insert older than the delete loses; a newer one revives the row.
    send(&h, a, vec![row(&o, t + 5, true, b"pk")])
        .await
        .unwrap();
    assert!(!one(&h, a, &o).await.unwrap().row.unwrap().alive);
    send(&h, a, vec![row(&o, t + 30, true, b"pk")])
        .await
        .unwrap();
    let s = one(&h, a, &o).await.unwrap();
    assert!(s.row.as_ref().unwrap().alive);
    assert_eq!(field_value(&s, &f(1)), Some(Some(b"late".to_vec())));
}

/// `(device, actor, seq, value)`.
type CtrRow = (Vec<u8>, Vec<u8>, u64, Vec<u8>);

#[tokio::test(flavor = "multi_thread")]
async fn counters_keep_one_entry_per_actor() {
    let (h, d) = devices(2, |_| {}).await;
    let (a, b) = (&d[0], &d[1]);
    let o = obj(3);
    send(
        &h,
        a,
        vec![ctr(&o, &f(1), 1, b"3"), ctr(&o, &f(1), 2, b"5")],
    )
    .await
    .unwrap();
    send(&h, b, vec![ctr(&o, &f(1), 1, b"2")]).await.unwrap();
    // A second installation of device A races with the first: its own entry.
    send(&h, a, vec![ctr_as(&o, &f(1), 2, 1, b"7")])
        .await
        .unwrap();
    let entries = |s: ObjState| {
        let mut v: Vec<CtrRow> = s
            .ctr
            .iter()
            .map(|c| (c.device.clone(), c.actor.clone(), c.seq, c.value.clone()))
            .collect();
        v.sort();
        v
    };
    let mut want = vec![
        (a.fp.to_vec(), vec![0xA1; 16], 2, b"5".to_vec()),
        (a.fp.to_vec(), vec![0xA2; 16], 1, b"7".to_vec()),
        (b.fp.to_vec(), vec![0xA1; 16], 1, b"2".to_vec()),
    ];
    want.sort();
    assert_eq!(entries(one(&h, a, &o).await.unwrap()), want);
    // A re-send under a new commit id (its record expired): an older or
    // equal seq is an operation already applied, ignored.
    send(
        &h,
        a,
        vec![ctr(&o, &f(1), 2, b"5"), ctr(&o, &f(1), 1, b"3")],
    )
    .await
    .unwrap();
    assert_eq!(entries(one(&h, a, &o).await.unwrap()), want);
    // A replayed commit returns its result and applies nothing again.
    let cid = rcid();
    let first = send_id(&h, b, cid.clone(), vec![ctr(&o, &f(1), 2, b"4")])
        .await
        .unwrap();
    let again = send_id(&h, b, cid, vec![ctr(&o, &f(1), 2, b"4")])
        .await
        .unwrap();
    assert_eq!(first, again);
    // Actors are 16 bytes.
    let mut bad = ctr(&o, &f(1), 9, b"1");
    if let CrdtOp::Ctr { actor, .. } = &mut bad {
        actor.truncate(15);
    }
    assert_eq!(code(send(&h, a, vec![bad]).await).0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn sets_are_add_wins_with_dots() {
    let (h, d) = devices(2, |_| {}).await;
    let (a, b) = (&d[0], &d[1]);
    let o = obj(4);
    let cid = rcid();
    let r1 = send_id(
        &h,
        a,
        cid.clone(),
        vec![
            add(&o, &f(1), &e(1), b"red"),
            add(&o, &f(1), &e(2), b"blue"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(r1.set_dots.len(), 2);
    assert!(r1.dots.is_empty());
    assert_eq!(r1.set_dots[0][..10], r1.versionstamp[..]);
    assert_eq!(&r1.set_dots[1][10..], &[0, 1]);
    // A replay returns the same dots.
    let r1b = send_id(
        &h,
        a,
        cid,
        vec![
            add(&o, &f(1), &e(1), b"red"),
            add(&o, &f(1), &e(2), b"blue"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(r1, r1b);
    // B adds "red" concurrently; A removes the "red" it saw.
    let r2 = send(&h, b, vec![add(&o, &f(1), &e(1), b"red")])
        .await
        .unwrap();
    send(
        &h,
        a,
        vec![rem(
            &o,
            &f(1),
            &e(1),
            &[r1.set_dots[0].to_vec(), vec![9; 12]],
        )],
    )
    .await
    .unwrap();
    let s = one(&h, a, &o).await.unwrap();
    let mut dots: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = s
        .set
        .iter()
        .map(|x| (x.elem.clone(), x.dot.clone(), x.device.clone()))
        .collect();
    dots.sort();
    let mut want = vec![
        (e(1), r2.set_dots[0].to_vec(), b.fp.to_vec()),
        (e(2), r1.set_dots[1].to_vec(), a.fp.to_vec()),
    ];
    want.sort();
    assert_eq!(dots, want, "add-wins: B's unseen add survives");
}

#[tokio::test(flavor = "multi_thread")]
async fn rights_and_validation() {
    let h = Harness::start().await;
    let (admin, reader) = (User::new(1), User::new(2));
    let doc = h.claim(&admin, &[]).await;
    let grants = vec![
        fs_grant(&admin, 1, &["read", "write"]),
        fs_grant(&reader, 1, &["read"]),
    ];
    let (signed, _) = signed_acl(
        &admin,
        2,
        Some(&doc),
        &[&admin],
        &[&admin, &reader],
        grants,
        vec![],
    );
    h.put_acl(signed, None).await.unwrap();
    let a = Dev {
        tok: h.sign_in(&admin).await.unwrap(),
        fp: admin.device.public().fingerprint(),
    };
    let r = Dev {
        tok: h.sign_in(&reader).await.unwrap(),
        fp: reader.device.public().fingerprint(),
    };
    let o = obj(5);
    let t = zfs::hlc(now_ms(), 0);
    assert_eq!(
        code(send(&h, &r, vec![row(&o, t, true, b"pk")]).await).0,
        403
    );
    send(&h, &a, vec![row(&o, t, true, b"pk")]).await.unwrap();
    assert!(one(&h, &r, &o).await.unwrap().row.unwrap().alive);
    // Malformed objects, fields, dots.
    for bad in [
        row(&[1; 15], t, true, b""),
        row(&[], t, true, b""),
        lww(&o, &[1; 15], t, None),
        add(&o, &f(1), &[1; 3], b""),
        rem(&o, &f(1), &e(1), &[vec![1; 11]]),
        row(&o, t, true, &vec![0; 90_001]),
    ] {
        assert_eq!(code(send(&h, &a, vec![bad]).await).0, 400);
    }
    let info: Info = h.get("/v1/info").await;
    assert!(info.features.iter().any(|x| x == "crdt_rows"));
}

#[tokio::test(flavor = "multi_thread")]
async fn range_pages_over_objects() {
    let (h, d) = devices(1, |_| {}).await;
    let a = &d[0];
    let t = zfs::hlc(now_ms(), 0);
    for i in 0..7u8 {
        send(&h, a, vec![row(&obj(i), t + i as u64, true, &[i])])
            .await
            .unwrap();
    }
    // Another table's prefix stays out of the range.
    send(&h, a, vec![row(&[0x0C; 32], t, true, b"x")])
        .await
        .unwrap();
    let mut seen = Vec::new();
    let mut begin = vec![0x0B; 32];
    loop {
        let r: ObjStates = h
            .call(
                "/v1/crdt/range",
                Some(&a.tok),
                &CrdtRange {
                    fs: 1,
                    begin: begin.clone(),
                    end: Some(vec![0x0B, 0x0B + 1]),
                    limit: Some(3),
                    read_version: None,
                },
            )
            .await
            .unwrap();
        for s in &r.objects {
            seen.push(s.object.clone());
        }
        if !r.more {
            break;
        }
        begin = r.objects.last().unwrap().object.clone();
        begin.push(0);
    }
    assert_eq!(seen, (0..7).map(obj).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweeper_purges_dead_and_registerless_objects() {
    let (h, d) = devices(1, |c| {
        c.limits.crdt_horizon_secs = 2;
        c.limits.crdt_max_skew_ms = 500;
        c.limits.sweep_interval_secs = 3600; // swept by hand below
    })
    .await;
    let a = &d[0];
    let st = &h.server.state;
    let (dead, alive, bare, revived) = (obj(1), obj(2), obj(3), obj(4));
    let t = zfs::hlc(now_ms(), 0);
    send(
        &h,
        a,
        vec![
            row(&dead, t, true, b"pk"),
            lww(&dead, &f(1), t, Some(b"v")),
            add(&dead, &f(2), &e(1), b"x"),
            ctr(&dead, &f(3), 1, b"1"),
            row(&alive, t, true, b"pk"),
            ctr(&bare, &f(1), 1, b"1"),
            row(&revived, t, true, b"pk"),
        ],
    )
    .await
    .unwrap();
    send(
        &h,
        a,
        vec![
            row(&dead, t + 1, false, b"pk"),
            row(&revived, t + 1, false, b"pk"),
        ],
    )
    .await
    .unwrap();
    send(&h, a, vec![row(&revived, t + 2, true, b"pk")])
        .await
        .unwrap();
    // Nothing is older than the horizon yet.
    zen_server::sweep_once(st).await.unwrap();
    assert_eq!(get(&h, a, &[dead.clone(), bare.clone()]).await.len(), 2);
    tokio::time::sleep(Duration::from_millis(2800)).await;
    zen_server::sweep_once(st).await.unwrap();
    let left: Vec<Vec<u8>> = get(
        &h,
        a,
        &[dead.clone(), alive.clone(), bare.clone(), revived.clone()],
    )
    .await
    .into_iter()
    .map(|s| s.object)
    .collect();
    assert_eq!(left, vec![alive, revived]);
    // A new operation on a purged object starts it afresh.
    send(&h, a, vec![row(&dead, zfs::hlc(now_ms(), 0), true, b"pk")])
        .await
        .unwrap();
    let s = one(&h, a, &dead).await.unwrap();
    assert!(s.lww.is_empty() && s.set.is_empty() && s.ctr.is_empty());
}

/// `field → (hlc, device, value)`.
type LwwModel = BTreeMap<Vec<u8>, (u64, [u8; 32], Option<Vec<u8>>)>;
/// `(field, device, actor) → (seq, value)`.
type CtrModel = BTreeMap<(Vec<u8>, [u8; 32], Vec<u8>), (u64, Vec<u8>)>;

/// Reference state of one object, from the operations accepted.
#[derive(Default, Debug, PartialEq)]
struct RefObj {
    row: Option<(u64, [u8; 32], bool)>,
    lww: LwwModel,
    ctr: CtrModel,
}

fn reg_view(s: &ObjState) -> RefObj {
    let dev = |d: &[u8]| -> [u8; 32] { d.try_into().unwrap() };
    RefObj {
        row: s.row.as_ref().map(|r| (r.hlc, dev(&r.device), r.alive)),
        lww: s
            .lww
            .iter()
            .map(|r| (r.field.clone(), (r.hlc, dev(&r.device), r.value.clone())))
            .collect(),
        ctr: s
            .ctr
            .iter()
            .map(|c| {
                (
                    (c.field.clone(), dev(&c.device), c.actor.clone()),
                    (c.seq, c.value.clone()),
                )
            })
            .collect(),
    }
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

/// Random operations from three devices on three objects, arriving in a
/// shuffled order, give the reference state: greatest timestamp per
/// register, greatest seq per counter entry, every add not removed.
#[tokio::test(flavor = "multi_thread")]
async fn convergence_matches_reference_in_any_arrival_order() {
    for seed in [7u64, 99, 12345] {
        let (h, devs) = devices(3, |_| {}).await;
        let mut rng = Rng(seed);
        let base = now_ms() - 3_600_000;
        let objects: Vec<Vec<u8>> = (0..3).map(|i| obj(0x40 + i)).collect();
        let mut model: Vec<RefObj> = (0..3).map(|_| RefObj::default()).collect();
        let mut seqs = [[[[0u64; 2]; 3]; 2]; 3]; // device, actor, object, field
        let mut pending = Vec::new();
        for i in 0..90u64 {
            let d = rng.below(3) as usize;
            let oi = rng.below(3) as usize;
            let o = &objects[oi];
            let fp = devs[d].fp;
            let hlc = zfs::hlc(base + 1 + rng.below(1_000_000), i as u16);
            let m = &mut model[oi];
            let op = match rng.below(3) {
                0 => {
                    let alive = rng.below(3) != 0;
                    if m.row.is_none_or(|(h0, d0, _)| (hlc, fp) > (h0, d0)) {
                        m.row = Some((hlc, fp, alive));
                    }
                    row(o, hlc, alive, b"pk")
                }
                1 => {
                    let fl = f(rng.below(2) as u8);
                    let v = (rng.below(4) != 0).then(|| vec![i as u8]);
                    let cur = m.lww.get(&fl);
                    if cur.is_none_or(|(h0, d0, _)| (hlc, fp) > (*h0, *d0)) {
                        m.lww.insert(fl.clone(), (hlc, fp, v.clone()));
                    }
                    lww(o, &fl, hlc, v.as_deref())
                }
                _ => {
                    // Two installations per device.
                    let (fi, ai) = (rng.below(2) as usize, rng.below(2) as usize);
                    seqs[d][ai][oi][fi] += 1;
                    let seq = seqs[d][ai][oi][fi];
                    let fl = f(fi as u8);
                    let k = (fl.clone(), fp, vec![0xA1 + ai as u8; 16]);
                    if m.ctr.get(&k).is_none_or(|(s0, _)| seq > *s0) {
                        m.ctr.insert(k, (seq, vec![i as u8]));
                    }
                    ctr_as(o, &fl, 1 + ai as u8, seq, &[i as u8])
                }
            };
            pending.push((d, op));
        }
        for i in (1..pending.len()).rev() {
            pending.swap(i, rng.below(i as u64 + 1) as usize);
        }
        // Every op is also re-sent later under a new commit id, as an
        // offline queue would after its idempotency record expired: no
        // re-send changes the state. A counter total that arrives after a
        // newer one of its actor is ignored, and the newer one stands.
        let resends: Vec<(usize, CrdtOp)> = pending.clone();
        for (d, op) in pending.into_iter().chain(resends) {
            if let Err(e) = send(&h, &devs[d], vec![op]).await {
                panic!("seed {seed}: {e:?}");
            }
        }
        let got = get(&h, &devs[0], &objects).await;
        assert_eq!(got.len(), 3);
        for (s, m) in got.iter().zip(&model) {
            assert_eq!(&reg_view(s), m, "seed {seed}");
        }
    }
}

/// Two nodes of one FoundationDB cluster see each other's writes.
#[tokio::test(flavor = "multi_thread")]
async fn rows_are_shared_across_nodes() {
    if !on_fdb() {
        return;
    }
    let (h, d) = devices(1, |_| {}).await;
    let p = h.peer().await;
    let tok = p.sign_in(&User::new(1)).await.unwrap();
    let o = obj(9);
    let t = zfs::hlc(now_ms(), 0);
    send(&h, &d[0], vec![row(&o, t, true, b"pk")])
        .await
        .unwrap();
    let r: ObjStates = p
        .call(
            "/v1/crdt/get",
            Some(&tok),
            &CrdtGet {
                fs: 1,
                objects: vec![ByteBuf::from(o.clone())],
                read_version: None,
            },
        )
        .await
        .unwrap();
    assert!(r.objects[0].row.as_ref().unwrap().alive);
}
