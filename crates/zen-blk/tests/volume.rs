//! Block volumes (spec/blk.md) against an in-process server: reads and
//! writes against a model and the reference tree, batching, a crash, one
//! writer at a time, and each attack of the test adversary (or of a forked
//! copy of the server), first shown to work, then detected.

#[path = "../../zen-client/tests/common/mod.rs"]
mod common;

use common::*;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use zen_blk::{BLOCK_SIZE, Volume, VolumeOptions};
use zen_client::{
    AlarmKind, AlarmObject, DeviceKey, Entry, Integrity, IntegrityOptions, Session, UnlockedFs,
};
use zen_core::blk::{self, RootRecord, VolumeKeys};
use zen_proto::{ByteBuf, KvItems, KvRange};

const MIB: u64 = 1 << 20;
const BS: u64 = BLOCK_SIZE as u64;

/// A device with integrity attached, on fs 1.
struct Dev {
    s: Session,
    u: UnlockedFs,
    i: Integrity,
}

async fn dev_with(srv: &Server, key: &DeviceKey, opts: IntegrityOptions) -> Dev {
    let s = srv.session(key).await;
    let fs = s.fs(1);
    let i = fs
        .integrity(IntegrityOptions {
            signer: Some(key.clone()),
            ..opts
        })
        .await
        .unwrap();
    let u = fs.unlock_device(&key.device).await.unwrap();
    Dev { s, u, i }
}

async fn dev(srv: &Server, key: &DeviceKey) -> Dev {
    dev_with(srv, key, IntegrityOptions::default()).await
}

/// Checkpoints only when asked (`close`, `checkpoint`).
fn quiet() -> IntegrityOptions {
    IntegrityOptions {
        sig_every: 1_000_000,
        delay: Duration::from_secs(3600),
        ..Default::default()
    }
}

/// A server claimed by key 1, with fs 1 holding a slot for each key.
async fn world_with(
    keys: &[&DeviceKey],
    f: impl FnOnce(&mut zen_server::config::Config),
) -> (Server, Session) {
    let srv = Server::start_with(f).await;
    let admin = key(1);
    srv.claim(&admin, keys).await;
    let a = srv.session(&admin).await;
    let mut devices = vec![admin.public()];
    devices.extend(keys.iter().map(|k| k.public()));
    a.fs(1).init_for(&devices, false).await.unwrap();
    (srv, a)
}

async fn world(keys: &[&DeviceKey]) -> (Server, Session) {
    world_with(keys, |_| {}).await
}

/// No idle flush: commits only on `flush`, full batches and `close`.
fn opts() -> VolumeOptions {
    VolumeOptions {
        idle_flush: None,
        ..Default::default()
    }
}

fn pinned(at: PathBuf) -> VolumeOptions {
    VolumeOptions {
        pin: Some(at),
        ..opts()
    }
}

fn block_of(tag: &str) -> Vec<u8> {
    tag.bytes().cycle().take(BLOCK_SIZE).collect()
}

async fn read(v: &Volume, offset: u64, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    v.read(offset, &mut b).await.unwrap();
    b
}

async fn read_block(v: &Volume, i: u64) -> zen_client::Result<Vec<u8>> {
    let mut b = vec![0u8; BLOCK_SIZE];
    v.read(i * BS, &mut b).await.map(|_| b)
}

fn no_alarms(devs: &[&Dev]) {
    for d in devs {
        assert!(d.i.alarms().is_empty(), "{:?}", d.i.alarms());
    }
}

fn vol_alarm(e: &zen_client::Error) -> (AlarmKind, AlarmObject) {
    let a = e.alarm().unwrap_or_else(|| panic!("an alarm, not {e}"));
    (a.kind, a.object)
}

/// A small deterministic generator for the model test.
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

/// Every stored value under a stored-key prefix, raw (sealed).
async fn raw_prefix(s: &Session, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let end = zen_client::kv::prefix_end(prefix);
    let mut lo = prefix.to_vec();
    let mut out = Vec::new();
    loop {
        let r: KvItems = s
            .call(
                "/v1/kv/range",
                &KvRange {
                    fs: 1,
                    begin: lo.clone(),
                    end: end.clone(),
                    limit: None,
                    reverse: None,
                    read_version: None,
                },
            )
            .await
            .unwrap();
        for it in &r.items {
            out.push((it.key.clone(), it.value.clone().unwrap()));
        }
        match r.items.last() {
            Some(last) if r.more => {
                lo = last.key.clone();
                lo.push(0);
            }
            _ => return out,
        }
    }
}

/// The stored root record, and a check that the stored blocks and nodes
/// are exactly the reference tree's (spec/blk.md §2).
async fn check_against_reference(d: &Dev, name: &[u8], blocks: u64, index: &HashMap<Vec<u8>, u64>) {
    let keys = VolumeKeys::new(&d.u.keys(), name);
    let rk = keys.root();
    let rec = RootRecord::decode(&d.u.get(&rk).await.unwrap().unwrap().value).unwrap();
    let mut leaves = BTreeMap::new();
    for (k, sealed) in raw_prefix(&d.s, &keys.block(0)[..48]).await {
        let i = index[&k];
        leaves.insert(i, blk::leaf(i, &sealed));
    }
    let t = blk::tree(blocks, &leaves);
    assert_eq!(rec.root, t.root, "the stored root is the reference tree's");
    let stored = raw_prefix(&d.s, &keys.node(1, 0)[..48]).await;
    assert_eq!(
        stored.len(),
        t.nodes.len(),
        "exactly the non-empty nodes are stored"
    );
    let by_key: HashMap<Vec<u8>, Vec<u8>> = stored.into_iter().collect();
    for ((l, j), n) in &t.nodes {
        let k = keys.node(*l, *j);
        let plain = d.u.open(&k, &by_key[&k]).unwrap();
        assert_eq!(blk::node_from_bytes(&plain).unwrap(), *n);
    }
}

// ------------------------------------------------------------------ basics

#[tokio::test(flavor = "multi_thread")]
async fn create_open_read_write() {
    let (kw, kr) = (key(2), key(3));
    let (srv, _) = world(&[&kw, &kr]).await;
    let w = dev(&srv, &kw).await;

    // Integrity is needed, and a writer needs a signer.
    let bare = srv
        .session(&kw)
        .await
        .fs(1)
        .unlock_device(&kw.device)
        .await
        .unwrap();
    let e = Volume::create(&bare, b"disk", 64 * MIB, opts())
        .await
        .err()
        .unwrap();
    assert!(e.is("integrity_needed"), "{e}");
    let ro = srv.session(&kr).await.fs(1);
    ro.integrity(IntegrityOptions::default()).await.unwrap();
    let ro = ro.unlock_device(&kr.device).await.unwrap();
    let e = Volume::create(&ro, b"disk", 64 * MIB, opts())
        .await
        .err()
        .unwrap();
    assert!(e.is("integrity_needed"), "{e}");

    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    assert_eq!(v.size(), 64 * MIB);
    assert_eq!(v.root().0, 1);
    assert_eq!(read(&v, 0, 8192).await, vec![0; 8192]);

    // Aligned and unaligned writes read back before and after the flush.
    let a = block_of("aligned ");
    v.write(BS, &a).await.unwrap();
    let u: Vec<u8> = (0..5000u32).map(|x| (x % 251) as u8).collect();
    v.write(10_000, &u).await.unwrap();
    assert_eq!(v.root().0, 1, "nothing committed before the flush");
    assert_eq!(read(&v, BS, BLOCK_SIZE).await, a);
    assert_eq!(read(&v, 10_000, 5000).await, u);
    v.flush().await.unwrap();
    assert_eq!(v.root().0, 2);
    assert_eq!(read(&v, BS, BLOCK_SIZE).await, a);
    assert_eq!(read(&v, 10_000, 5000).await, u);
    assert_eq!(read(&v, 9_990, 10).await, vec![0; 10]);
    v.write(BS + 100, b"tail").await.unwrap();
    v.flush().await.unwrap();
    assert_eq!(v.root().0, 3);
    v.flush().await.unwrap();
    assert_eq!(v.root().0, 3, "an empty flush commits nothing");

    // Past the end.
    let mut b = [0u8; 2];
    assert!(v.read(64 * MIB - 1, &mut b).await.unwrap_err().is("range"));
    assert!(v.write(64 * MIB, b"x").await.unwrap_err().is("range"));
    v.close().await.unwrap();

    // Another device reads the same.
    let r = dev(&srv, &kr).await;
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    assert_eq!(rv.root().0, 3);
    let mut want = a.clone();
    want[100..104].copy_from_slice(b"tail");
    assert_eq!(read(&rv, BS, BLOCK_SIZE).await, want);
    assert_eq!(read(&rv, 10_000, 5000).await, u);
    assert!(rv.write(0, b"x").await.unwrap_err().is("read_only"));
    assert!(
        Volume::open_read_only(&r.u, b"nope", opts())
            .await
            .err()
            .unwrap()
            .is("not_found")
    );

    // Again: `exists`; and the writer reopens.
    let e = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .err()
        .unwrap();
    assert!(e.is("exists"), "{e}");
    let v = Volume::open(&w.u, b"disk", opts()).await.unwrap();
    v.write(0, &block_of("zero ")).await.unwrap();
    v.close().await.unwrap();
    no_alarms(&[&w, &r]);
}

#[tokio::test(flavor = "multi_thread")]
async fn random_operations_match_a_model_and_the_reference_tree() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let w = dev(&srv, &kw).await;
    let size = 100 * MIB;
    let blocks = size / BS;
    let o = VolumeOptions {
        batch_blocks: 16,
        ..opts()
    };
    let mut v = Volume::create(&w.u, b"model", size, o.clone())
        .await
        .unwrap();
    assert_eq!(blk::depth(blocks), 3);
    let keys = VolumeKeys::new(&w.u.keys(), b"model");
    let index: HashMap<Vec<u8>, u64> = (0..blocks).map(|i| (keys.block(i), i)).collect();

    let mut model = vec![0u8; size as usize];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut flushes = 0;
    for step in 0..2000u32 {
        // Writes cluster in a few regions, so nodes fill and empty.
        let region = [0, 16_384, 25_000][rng.below(3) as usize] * BS;
        let span = 2_000 * BS;
        let offset = (region + rng.below(span)).min(size - 1);
        let aligned = rng.below(2) == 0;
        let offset = if aligned { offset / BS * BS } else { offset };
        let len = (1 + rng.below(40)) * BS - if aligned { 0 } else { rng.below(BS) };
        let len = len.min(size - offset);
        match rng.below(10) {
            0..=4 => {
                let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                v.write(offset, &data).await.unwrap();
                model[offset as usize..(offset + len) as usize].copy_from_slice(&data);
            }
            5 => {
                v.discard(offset, len).await.unwrap();
                model[offset as usize..(offset + len) as usize].fill(0);
            }
            6..=8 => {
                let got = read(&v, offset, len as usize).await;
                assert!(
                    got == model[offset as usize..(offset + len) as usize],
                    "step {step}: read {len} at {offset} differs from the model"
                );
            }
            _ => {
                v.flush().await.unwrap();
                flushes += 1;
                if flushes % 4 == 0 {
                    check_against_reference(&w, b"model", blocks, &index).await;
                }
            }
        }
        if step % 400 == 399 {
            v.close().await.unwrap();
            v = Volume::open(&w.u, b"model", o.clone()).await.unwrap();
        }
    }
    v.flush().await.unwrap();
    check_against_reference(&w, b"model", blocks, &index).await;
    // A fresh reader agrees with the model everywhere.
    v.close().await.unwrap();
    let r = Volume::open_read_only(&w.u, b"model", opts())
        .await
        .unwrap();
    for chunk in 0..(size / (4 * MIB)) {
        let at = chunk * 4 * MIB;
        assert!(
            read(&r, at, 4 * MIB as usize).await == model[at as usize..(at + 4 * MIB) as usize]
        );
    }
    no_alarms(&[&w]);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_commit_in_batches() {
    let kw = key(2);
    // A small commit limit: a flush splits into commits within it.
    let (srv, _) = world_with(&[&kw], |c| c.limits.max_commit_bytes = 2_000_000).await;
    let w = dev(&srv, &kw).await;
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();

    // 3 × batch_blocks dirty blocks commit on their own.
    let data: Vec<u8> = (0..3 * 256 * BS).map(|x| (x / BS) as u8).collect();
    v.write(0, &data).await.unwrap();
    assert!(v.root().0 > 1, "a full batch committed without a flush");
    assert_eq!(read(&v, 0, data.len()).await, data);

    // A batch bigger than one commit may carry: several commits.
    let v2 = Volume::create(
        &w.u,
        b"big",
        64 * MIB,
        VolumeOptions {
            batch_blocks: 1000,
            ..opts()
        },
    )
    .await
    .unwrap();
    let data: Vec<u8> = (0..1000 * BS).map(|x| (x / BS * 7) as u8).collect();
    v2.write(0, &data).await.unwrap();
    let commits = v2.root().0 - 1;
    assert!(commits >= 2, "{commits} commits");
    v2.close().await.unwrap();
    let r = Volume::open_read_only(&w.u, b"big", opts()).await.unwrap();
    assert_eq!(read(&r, 0, data.len()).await, data);
    v.close().await.unwrap();
    no_alarms(&[&w]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_volume_flushes() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let w = dev(&srv, &kw).await;
    let o = VolumeOptions {
        idle_flush: Some(Duration::from_millis(200)),
        ..Default::default()
    };
    let v = Volume::create(&w.u, b"disk", 64 * MIB, o).await.unwrap();
    v.write(0, b"idle").await.unwrap();
    let t = Instant::now();
    until(|| v.root().0 == 2).await;
    assert!(t.elapsed() >= Duration::from_millis(150));
    v.close().await.unwrap();
}

// ----------------------------------------------------------------- writers

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_loses_only_unflushed_writes() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let w = dev(&srv, &kw).await;
    let pin = srv.dir.path().join("disk.pin");
    let o = VolumeOptions {
        lease_ttl: Duration::from_millis(1500),
        ..pinned(pin.clone())
    };
    let v = Volume::create(&w.u, b"disk", 64 * MIB, o.clone())
        .await
        .unwrap();
    v.write(0, &block_of("A")).await.unwrap();
    v.flush().await.unwrap();
    let older = srv.dir.path().join("older.pin");
    std::fs::copy(&pin, &older).unwrap();
    v.write(BS, &block_of("B")).await.unwrap();
    v.flush().await.unwrap();
    v.write(2 * BS, &block_of("C")).await.unwrap();
    // The process dies: no flush, no close, renewals stop.
    drop(v);
    drop(w);

    // While the lease lasts, nobody else writes.
    let w2 = dev(&srv, &kw).await;
    assert!(
        Volume::open(&w2.u, b"disk", o.clone())
            .await
            .err()
            .unwrap()
            .is("busy")
    );
    tokio::time::sleep(Duration::from_millis(2000)).await;
    // The crash came after a commit and before its pin: the pin is one behind.
    std::fs::copy(&older, &pin).unwrap();
    let v = Volume::open(&w2.u, b"disk", o).await.unwrap();
    assert_eq!(read_block(&v, 0).await.unwrap(), block_of("A"));
    assert_eq!(read_block(&v, 1).await.unwrap(), block_of("B"));
    assert_eq!(read_block(&v, 2).await.unwrap(), vec![0; BLOCK_SIZE]);
    v.close().await.unwrap();
    no_alarms(&[&w2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_writer_is_refused() {
    let (ka, kb) = (key(2), key(3));
    let (srv, _) = world(&[&ka, &kb]).await;
    let (a, b) = (dev(&srv, &ka).await, dev(&srv, &kb).await);
    let va = Volume::create(&a.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    va.write(0, &block_of("a")).await.unwrap();
    va.flush().await.unwrap();
    assert!(
        Volume::open(&b.u, b"disk", opts())
            .await
            .err()
            .unwrap()
            .is("busy")
    );
    let rb = Volume::open_read_only(&b.u, b"disk", opts()).await.unwrap();
    assert_eq!(read_block(&rb, 0).await.unwrap(), block_of("a"));
    va.close().await.unwrap();
    let vb = Volume::open(&b.u, b"disk", opts()).await.unwrap();
    vb.write(BS, &block_of("b")).await.unwrap();
    vb.close().await.unwrap();
    no_alarms(&[&a, &b]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_fence_writes_nothing() {
    let (ka, kb) = (key(2), key(3));
    let (srv, _) = world(&[&ka, &kb]).await;
    let (a, b) = (dev(&srv, &ka).await, dev(&srv, &kb).await);
    let o = VolumeOptions {
        lease_ttl: Duration::from_millis(1500),
        ..opts()
    };
    let va = Volume::create(&a.u, b"disk", 64 * MIB, o.clone())
        .await
        .unwrap();
    va.write(0, &block_of("a0")).await.unwrap();
    va.flush().await.unwrap();
    // A stops renewing (a paused process) and its lease lapses.
    va.pause_renewals(true);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let vb = Volume::open(&b.u, b"disk", o).await.unwrap();
    vb.write(1000 * BS, &block_of("b")).await.unwrap();
    vb.flush().await.unwrap();

    // A doesn't know: its next commit (on paths it holds) is refused.
    va.write(0, &block_of("a1")).await.unwrap();
    let e = va.flush().await.unwrap_err();
    assert!(e.is("lease_lost"), "{e}");
    assert!(va.write(BS, b"x").await.unwrap_err().is("lease_lost"));
    assert_eq!(read_block(&vb, 1000).await.unwrap(), block_of("b"));
    assert_eq!(read_block(&vb, 0).await.unwrap(), block_of("a0"));
    vb.close().await.unwrap();
    no_alarms(&[&a, &b]);
}

// ----------------------------------------------------------------- attacks

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_sees_the_volume_change() {
    let (kw, kr) = (key(2), key(3));
    let (srv, _) = world(&[&kw, &kr]).await;
    let (w, r) = (dev(&srv, &kw).await, dev(&srv, &kr).await);
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    v.write(5 * BS, &block_of("one")).await.unwrap();
    v.flush().await.unwrap();
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    assert_eq!(read_block(&rv, 5).await.unwrap(), block_of("one"));
    v.write(5 * BS, &block_of("two")).await.unwrap();
    v.flush().await.unwrap();
    let e = read_block(&rv, 5).await.unwrap_err();
    assert!(e.is("volume_changed"), "{e}");
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    assert_eq!(read_block(&rv, 5).await.unwrap(), block_of("two"));
    v.close().await.unwrap();
    no_alarms(&[&w, &r]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_root_is_detected() {
    let (kw, kr, kt) = (key(2), key(3), key(4));
    let (srv, admin) = world(&[&kw, &kr, &kt]).await;
    let w = dev_with(&srv, &kw, quiet()).await;
    let pin = srv.dir.path().join("disk.pin");
    let v = Volume::create(&w.u, b"disk", 64 * MIB, pinned(pin.clone()))
        .await
        .unwrap();
    v.write(0, &block_of("x")).await.unwrap();
    v.flush().await.unwrap();
    // A consistent snapshot: the root, its nodes and the block, frozen.
    let keys = VolumeKeys::new(&w.u.keys(), b"disk");
    let frozen = [keys.root(), keys.node(2, 0), keys.node(1, 0), keys.block(0)];
    lie(
        &admin,
        frozen
            .into_iter()
            .map(|k| Lie::Kv {
                fs: 1,
                key: ByteBuf::from(k),
            })
            .collect(),
    )
    .await;
    for t in ["y", "z"] {
        v.write(0, &block_of(t)).await.unwrap();
        v.flush().await.unwrap();
    }
    assert_eq!(v.root().0, 4);

    // The writer, as a fresh process with its pin and no checkpoint yet:
    // the server's root (seq 2) is below the pin.
    let w2 = dev_with(&srv, &kw, quiet()).await;
    let e = Volume::open_read_only(&w2.u, b"disk", pinned(pin.clone()))
        .await
        .err()
        .unwrap();
    assert_eq!(vol_alarm(&e), (AlarmKind::Rollback, AlarmObject::Vol));

    // Another device before the writer's checkpoint: the window.
    let r = dev(&srv, &kr).await;
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    assert_eq!(rv.root().0, 2, "the attack works within the window");
    assert_eq!(read_block(&rv, 0).await.unwrap(), block_of("x"));
    no_alarms(&[&r]);

    // Once the writer checkpoints, another device's sync catches it.
    v.close().await.unwrap();
    let t = dev(&srv, &kt).await;
    let e = Volume::open_read_only(&t.u, b"disk", opts())
        .await
        .err()
        .unwrap();
    assert_eq!(vol_alarm(&e), (AlarmKind::Rollback, AlarmObject::Vol));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_block_or_node_is_detected() {
    let (kw, kr, kt) = (key(2), key(3), key(4));
    let (srv, admin) = world(&[&kw, &kr, &kt]).await;
    let w = dev(&srv, &kw).await;
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    let keys = VolumeKeys::new(&w.u.keys(), b"disk");

    // A block: its older value.
    v.write(5 * BS, &block_of("old")).await.unwrap();
    v.flush().await.unwrap();
    lie(
        &admin,
        vec![Lie::Kv {
            fs: 1,
            key: ByteBuf::from(keys.block(5)),
        }],
    )
    .await;
    v.write(5 * BS, &block_of("new")).await.unwrap();
    v.flush().await.unwrap();
    let r = dev(&srv, &kr).await;
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    let e = read_block(&rv, 5).await.unwrap_err();
    assert_eq!(vol_alarm(&e), (AlarmKind::Rollback, AlarmObject::Vol));
    assert!(e.alarm().unwrap().detail.contains("leaf"), "{e}");
    truth(&admin).await;

    // A level-1 node: its older value, with the block it lacks withheld,
    // under a current parent. The old node and block agree with each other.
    v.write(7 * BS, &block_of("seven")).await.unwrap();
    v.flush().await.unwrap();
    lie(
        &admin,
        [keys.node(1, 0), keys.block(8)]
            .into_iter()
            .map(|k| Lie::Kv {
                fs: 1,
                key: ByteBuf::from(k),
            })
            .collect(),
    )
    .await;
    v.write(8 * BS, &block_of("eight")).await.unwrap();
    v.flush().await.unwrap();
    let t = dev(&srv, &kt).await;
    let tv = Volume::open_read_only(&t.u, b"disk", opts()).await.unwrap();
    let e = read_block(&tv, 8).await.unwrap_err();
    assert_eq!(vol_alarm(&e), (AlarmKind::Rollback, AlarmObject::Vol));
    assert!(e.alarm().unwrap().detail.contains("node (1, 0)"), "{e}");

    // The writer too, once its cache is gone.
    v.close().await.unwrap();
    let v = Volume::open(&w.u, b"disk", opts()).await.unwrap();
    let e = read_block(&v, 8).await.unwrap_err();
    assert_eq!(vol_alarm(&e), (AlarmKind::Rollback, AlarmObject::Vol));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forked_volume_is_detected() {
    let (kd, kr) = (key(2), key(3));
    let (s1, _) = world(&[&kd, &kr]).await;
    let d = dev_with(&s1, &kd, quiet()).await;
    let v = Volume::create(&d.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    v.write(0, &block_of("common")).await.unwrap();
    v.close().await.unwrap();

    // Two histories from here: each writes another seq 3.
    let s2 = s1.fork().await;
    let v = Volume::open(&d.u, b"disk", opts()).await.unwrap();
    v.write(0, &block_of("one side")).await.unwrap();
    v.close().await.unwrap();
    let d2 = dev_with(&s2, &kd, quiet()).await;
    let v2 = Volume::open(&d2.u, b"disk", opts()).await.unwrap();
    v2.write(0, &block_of("other side")).await.unwrap();
    v2.flush().await.unwrap();
    assert_eq!(v2.root().0, 3);

    // A reader that pinned side 1's root, moved to side 2.
    let pin = s1.dir.path().join("reader.pin");
    let r = dev(&s1, &kr).await;
    let rv = Volume::open_read_only(&r.u, b"disk", pinned(pin.clone()))
        .await
        .unwrap();
    assert_eq!(rv.root().0, 3);
    let r2 = dev(&s2, &kr).await;
    let e = Volume::open_read_only(&r2.u, b"disk", pinned(pin))
        .await
        .err()
        .unwrap();
    assert_eq!(vol_alarm(&e), (AlarmKind::Fork, AlarmObject::Vol));
    no_alarms(&[&d, &d2, &r]);
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoints_carry_the_volume_root() {
    let (kw, kr) = (key(2), key(3));
    let (srv, _) = world(&[&kw, &kr]).await;
    let (w, r) = (dev(&srv, &kw).await, dev(&srv, &kr).await);
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    v.write(0, b"carried").await.unwrap();
    v.flush().await.unwrap();
    let (seq, rh) = v.root();
    let vol = v.vol_id().to_vec();
    assert_eq!(vol.len(), 32);
    v.close().await.unwrap();
    let mine = w.i.device_head(&kw.fp()).unwrap();
    assert!(mine.body.objects.contains(&Entry::Vol {
        vol: vol.clone(),
        seq,
        hash: rh
    }));
    r.i.sync().await.unwrap();
    let probe = Entry::Vol {
        vol: vol.clone(),
        seq: 0,
        hash: [0; 32],
    };
    assert_eq!(r.i.floor(&probe), Some(Entry::Vol { vol, seq, hash: rh }));
    no_alarms(&[&w, &r]);
}

/// Throughput and latency (release: `cargo test --release -p zen-blk
/// --test volume bench -- --ignored --nocapture`).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let w = dev(&srv, &kw).await;
    let v = Volume::create(&w.u, b"bench", 256 * MIB, opts())
        .await
        .unwrap();
    let data: Vec<u8> = (0..4 * MIB).map(|x| x as u8).collect();
    let t = Instant::now();
    for k in 0..16 {
        v.write(k * 4 * MIB, &data).await.unwrap();
        v.flush().await.unwrap();
    }
    let s = t.elapsed().as_secs_f64();
    println!(
        "sequential write 64 MiB, flush every 4 MiB: {s:.2} s, {:.1} MiB/s",
        64.0 / s
    );

    let mut rng = Rng(7);
    let mut buf = vec![0u8; BLOCK_SIZE];
    for (label, vol) in [
        (
            "cold",
            Volume::open_read_only(&w.u, b"bench", opts())
                .await
                .unwrap(),
        ),
        ("warm", v.clone()),
    ] {
        let t = Instant::now();
        for _ in 0..500 {
            let i = rng.below(16_384);
            vol.read(i * BS, &mut buf).await.unwrap();
        }
        println!(
            "random 4 KiB read ({label}): {:.3} ms",
            t.elapsed().as_secs_f64() * 1000.0 / 500.0
        );
    }
    let t = Instant::now();
    for k in 0..100u64 {
        v.write(rng.below(65_536) * BS, &k.to_be_bytes())
            .await
            .unwrap();
        v.flush().await.unwrap();
    }
    println!(
        "flush of one block (fsync): {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0 / 100.0
    );
    v.close().await.unwrap();
}
