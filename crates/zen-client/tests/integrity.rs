//! Device checkpoints and pins (spec/formats.md §14) against an in-process
//! server: honest runs report nothing, and each attack of the test
//! adversary (or of a forked copy of the server) is first shown to work,
//! then detected.

mod common;

use common::*;
use std::path::PathBuf;
use std::time::Duration;
use zen_client::{
    AlarmKind, AlarmObject, DeviceKey, Entry, Integrity, IntegrityOptions, Session, UnlockedFs,
};
use zen_core::integrity::{CheckpointBody, Head, Seen, seal_checkpoint};

/// A device with integrity attached, on fs 1.
struct Dev {
    key: DeviceKey,
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
    Dev {
        key: key.clone(),
        s,
        u,
        i,
    }
}

async fn dev(srv: &Server, key: &DeviceKey) -> Dev {
    dev_with(srv, key, IntegrityOptions::default()).await
}

/// A volume root entry.
fn vol(n: u8, seq: u64) -> Entry {
    Entry::Vol {
        vol: vec![n; 16],
        seq,
        hash: [seq as u8; 32],
    }
}

/// A server claimed by key 1, with fs 1 holding a slot for each key.
async fn world(keys: &[&DeviceKey]) -> (Server, Session) {
    world_with(keys, |_| {}).await
}

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

fn no_alarms(devs: &[&Dev]) {
    for d in devs {
        assert!(d.i.alarms().is_empty(), "{:?}", d.i.alarms());
        assert!(d.i.trusted());
    }
}

fn head(d: &Dev, of: &DeviceKey) -> zen_client::integrity::DevicePin {
    d.i.device_head(&of.fp()).expect("a pinned head")
}

#[tokio::test(flavor = "multi_thread")]
async fn honest_checkpoints_link_each_other() {
    let (ka, kb) = (key(2), key(3));
    let (srv, _) = world(&[&ka, &kb]).await;
    let a = dev(&srv, &ka).await;
    let b = dev(&srv, &kb).await;

    a.i.note(vol(1, 1), true).unwrap();
    a.i.note(vol(1, 2), true).unwrap();
    assert_eq!(a.i.checkpoint().await.unwrap(), Some(1));
    // Nothing new: nothing to write.
    assert_eq!(a.i.checkpoint().await.unwrap(), None);

    b.i.sync().await.unwrap();
    assert_eq!(b.i.floor(&vol(1, 0)), Some(vol(1, 2)));
    assert_eq!(b.i.checkpoint_always().await.unwrap(), Some(1));
    let hb = head(&b, &kb);
    let ha = head(&a, &ka);
    assert_eq!(
        hb.body.seen,
        vec![Seen {
            device: ka.fp(),
            seq: 1,
            hash: ha.hash
        }]
    );
    // A checks B's link to it.
    a.i.sync().await.unwrap();

    // Bodies are cumulative and chained.
    a.i.note(vol(2, 1), true).unwrap();
    assert_eq!(a.i.checkpoint().await.unwrap(), Some(2));
    let h2 = head(&a, &ka);
    assert_eq!(h2.body.prev, ha.hash);
    assert_eq!(h2.body.objects, vec![vol(1, 2), vol(2, 1)]);
    assert_eq!(h2.body.seen.len(), 1);
    b.i.sync().await.unwrap();
    assert_eq!(head(&b, &ka).seq, 2);
    no_alarms(&[&a, &b]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cadence_writes_checkpoints() {
    let (ka, kb) = (key(2), key(3));
    let (srv, _) = world(&[&ka, &kb]).await;
    // After sig_every changes.
    let a = dev_with(
        &srv,
        &ka,
        IntegrityOptions {
            sig_every: 3,
            delay: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .await;
    for s in 1..=3 {
        a.i.note(vol(1, s), true).unwrap();
    }
    until(|| a.i.device_head(&ka.fp()).is_some_and(|p| p.seq == 1)).await;
    // A change that doesn't count waits for one that does.
    a.i.note(vol(2, 1), false).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(head(&a, &ka).seq, 1);

    // After the delay.
    let b = dev_with(
        &srv,
        &kb,
        IntegrityOptions {
            delay: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .await;
    b.i.note(vol(3, 1), true).unwrap();
    until(|| b.i.device_head(&kb.fp()).is_some_and(|p| p.seq == 1)).await;
    // On close.
    b.i.note(vol(3, 2), true).unwrap();
    b.i.close().await.unwrap();
    assert_eq!(head(&b, &kb).seq, 2);
    no_alarms(&[&a, &b]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_body_leaves_out_the_oldest_entries() {
    let ka = key(2);
    let (srv, _) = world_with(&[&ka], |c| c.limits.max_value_bytes = 8192).await;
    let a = dev(&srv, &ka).await;
    for n in 0..120u8 {
        a.i.note(vol(n, 1), false).unwrap();
    }
    assert_eq!(a.i.checkpoint().await.unwrap(), Some(1));
    let body = head(&a, &ka).body;
    assert!(
        body.objects.len() < 120 && body.objects.len() > 40,
        "{}",
        body.objects.len()
    );
    assert!(body.objects.contains(&vol(119, 1)));
    assert!(!body.objects.contains(&vol(0, 1)));
    assert!(body.encode().unwrap().len() <= 8192 - 3507);
    no_alarms(&[&a]);
}

#[tokio::test(flavor = "multi_thread")]
async fn pins_survive_in_a_file() {
    let (ka, kb) = (key(2), key(3));
    let (srv, _) = world(&[&ka, &kb]).await;
    let a = dev(&srv, &ka).await;
    a.i.note(vol(1, 5), true).unwrap();
    a.i.checkpoint().await.unwrap();

    let path: PathBuf = srv.dir.path().join("b.pins");
    let opts = || IntegrityOptions {
        pins: Some(path.clone()),
        ..Default::default()
    };
    let b = dev_with(&srv, &kb, opts()).await;
    b.i.sync().await.unwrap();
    assert!(path.exists());
    drop(b);

    // A new process: the pins and floors are back before any request.
    let b2 = dev_with(&srv, &kb, opts()).await;
    assert_eq!(head(&b2, &ka).seq, 1);
    assert_eq!(b2.i.floor(&vol(1, 0)), Some(vol(1, 5)));
    assert!(b2.i.acl_pin().is_some());
    b2.i.sync().await.unwrap();
    no_alarms(&[&a, &b2]);

    // A pins file that doesn't decode is refused, not ignored.
    std::fs::write(&path, b"garbage").unwrap();
    let s = srv.session(&kb).await;
    let e = s.fs(1).integrity(opts()).await.err().unwrap();
    assert!(e.is("format"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_processes_of_one_device_merge() {
    let ka = key(2);
    let (srv, _) = world(&[&ka]).await;
    let p1 = dev(&srv, &ka).await;
    let p2 = dev(&srv, &ka).await;
    p1.i.note(vol(1, 1), true).unwrap();
    assert_eq!(p1.i.checkpoint().await.unwrap(), Some(1));
    // p2 doesn't know seq 1 was taken: 409, adopt, merge, seq 2.
    p2.i.note(vol(2, 1), true).unwrap();
    assert_eq!(p2.i.checkpoint().await.unwrap(), Some(2));
    let h = head(&p2, &ka);
    assert_eq!(h.body.objects, vec![vol(1, 1), vol(2, 1)]);
    p1.i.sync().await.unwrap();
    assert_eq!(head(&p1, &ka).seq, 2);
    p1.i.note(vol(1, 2), true).unwrap();
    assert_eq!(p1.i.checkpoint().await.unwrap(), Some(3));
    assert_eq!(head(&p1, &ka).body.objects, vec![vol(1, 2), vol(2, 1)]);
    no_alarms(&[&p1, &p2]);
}

// ---------------------------------------------------------------- attacks

#[tokio::test(flavor = "multi_thread")]
async fn withheld_checkpoints_are_detected() {
    let (ka, kb) = (key(2), key(3));
    let (srv, admin) = world(&[&ka, &kb]).await;
    let a = dev(&srv, &ka).await;
    for s in 1..=2 {
        a.i.note(vol(1, s), true).unwrap();
        a.i.checkpoint().await.unwrap();
    }
    lie(
        &admin,
        vec![Lie::Checkpoints {
            fs: 1,
            device: zen_proto::ByteBuf::from(ka.fp().to_vec()),
        }],
    )
    .await;
    for s in 3..=4 {
        a.i.note(vol(1, s), true).unwrap();
        assert_eq!(a.i.checkpoint().await.unwrap(), Some(s));
    }

    // The attack works: a fresh reader sees A at 2, with no alarm.
    let b = dev(&srv, &kb).await;
    b.i.sync().await.unwrap();
    assert_eq!(head(&b, &ka).seq, 2);
    assert!(b.i.trusted());

    // A sees its own head below its pin.
    let e = a.i.sync().await.unwrap_err();
    let al = e.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Rollback, AlarmObject::Checkpoint)
    );
    assert!(!a.i.trusted());

    // B, comparing pins with A out of band, sees the fork.
    let alarms = b.i.compare_pins(&a.i.export_pins().unwrap()).await.unwrap();
    assert!(
        alarms
            .iter()
            .any(|x| x.kind == AlarmKind::Fork && x.object == AlarmObject::Checkpoint),
        "{alarms:?}"
    );
}

/// An app's root record (as zen-blk's volume root will be): `seq ‖ hash`.
fn root_record(seq: u64) -> Vec<u8> {
    [seq.to_be_bytes().as_slice(), &[seq as u8; 32]].concat()
}

fn read_root(v: &[u8]) -> Entry {
    let seq = u64::from_be_bytes(v[..8].try_into().unwrap());
    Entry::Vol {
        vol: vec![9; 16],
        seq,
        hash: v[8..].try_into().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_root_is_detected_after_a_sync() {
    let (ka, kb) = (key(2), key(3));
    let (srv, admin) = world(&[&ka, &kb]).await;
    let a = dev(&srv, &ka).await;
    let r = a.u.key(&["vol", "root"]);
    for seq in 1..=3 {
        a.u.set(&r, &root_record(seq)).await.unwrap();
        a.i.note(read_root(&root_record(seq)), true).unwrap();
        if seq == 1 {
            lie(
                &admin,
                vec![Lie::Kv {
                    fs: 1,
                    key: zen_proto::ByteBuf::from(r.clone()),
                }],
            )
            .await;
        }
    }
    a.i.checkpoint().await.unwrap();

    // Without a sync, B accepts the frozen root: the window (formats.md §14.3).
    let b = dev(&srv, &kb).await;
    let served = b.u.get(&r).await.unwrap().unwrap().value;
    assert_eq!(
        read_root(&served),
        Entry::Vol {
            vol: vec![9; 16],
            seq: 1,
            hash: [1; 32]
        }
    );
    b.i.observe(read_root(&served), false).unwrap();
    // After one, it is a rollback.
    b.i.sync().await.unwrap();
    let served = b.u.get(&r).await.unwrap().unwrap().value;
    let e = b.i.observe(read_root(&served), false).unwrap_err();
    let al = e.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Rollback, AlarmObject::Vol)
    );
    assert_eq!(al.id.as_deref(), Some([9u8; 16].as_slice()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_header_is_detected() {
    let (kadmin, kb, kc) = (key(1), key(3), key(4));
    let srv = Server::start().await;
    srv.claim(&kadmin, &[&kb, &kc]).await;
    let s = srv.session(&kadmin).await;
    let fs = s.fs(1);
    let ai = fs
        .integrity(IntegrityOptions {
            signer: Some(kadmin.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let au = fs
        .init_for(&[kadmin.public(), kb.public()], true)
        .await
        .unwrap();
    assert_eq!(ai.header_pin().unwrap().version, 1);
    let b = dev(&srv, &kb).await;
    assert_eq!(b.i.header_pin().unwrap().version, 1);

    lie(&s, vec![Lie::Header { fs: 1 }]).await;
    au.add_device_slot(&kc.public()).await.unwrap();
    assert_eq!(ai.header_pin().unwrap().version, 2);
    ai.checkpoint_always().await.unwrap();
    // The attack works: the server serves version 1, signed and valid.
    let e = fs.header().await.unwrap_err();
    let al = e.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Rollback, AlarmObject::Header)
    );
    // B learns version 2 from the admin's checkpoint, and alarms too.
    let e = b.i.sync().await.unwrap_err();
    let al = e.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Rollback, AlarmObject::Header)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forked_server_is_detected() {
    let (kd, kc, ke) = (key(2), key(3), key(4));
    let (s1, _) = world(&[&kd, &kc, &ke]).await;
    let d = dev(&s1, &kd).await;
    d.i.note(vol(1, 1), true).unwrap();
    d.i.checkpoint().await.unwrap();
    let pins: PathBuf = s1.dir.path().join("c.pins");
    let copts = || IntegrityOptions {
        pins: Some(pins.clone()),
        ..Default::default()
    };
    let c = dev_with(&s1, &kc, copts()).await;
    c.i.sync().await.unwrap();

    // Two histories from here.
    let s2 = s1.fork().await;
    d.i.note(vol(1, 2), true).unwrap();
    assert_eq!(d.i.checkpoint().await.unwrap(), Some(2));
    let d2 = dev(&s2, &kd).await;
    d2.i.note(vol(1, 7), true).unwrap();
    assert_eq!(d2.i.checkpoint().await.unwrap(), Some(2));
    // Each side alone is consistent.
    c.i.sync().await.unwrap();
    assert_eq!(head(&c, &kd).seq, 2);
    let e2 = dev(&s2, &ke).await;
    e2.i.sync().await.unwrap();
    no_alarms(&[&c, &e2, &d, &d2]);

    // Pins compared across the sides: a fork.
    let alarms =
        e2.i.compare_pins(&c.i.export_pins().unwrap())
            .await
            .unwrap();
    assert!(
        alarms.iter().any(|x| x.kind == AlarmKind::Fork),
        "{alarms:?}"
    );
    // C, moved to the other side with its pins: a fork.
    drop(c);
    let c2 = dev_with(&s2, &kc, copts()).await;
    let e = c2.i.sync().await.unwrap_err();
    let al = e.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Fork, AlarmObject::Checkpoint)
    );
    assert!(al.detail.contains("another checkpoint 2"), "{}", al.detail);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_signed_by_another_device_is_invalid() {
    // User 2 has two devices: D and E. E puts a checkpoint under D's name.
    let (kd, ke, kc) = (key(2), second_key(2), key(3));
    let (srv, _) = world(&[&kd, &ke, &kc]).await;
    let e = dev(&srv, &ke).await;
    let body = CheckpointBody {
        fs: 1,
        device: kd.fp(),
        seq: 1,
        prev: [0; 32],
        hlc: 1,
        acl: Head::default(),
        header: Head::default(),
        objects: vec![vol(1, 1)],
        seen: vec![],
    };
    // sign_checkpoint refuses another device's body: sign it by hand (§14.2).
    let bytes = body.encode().unwrap();
    let sig = ke
        .device
        .signing()
        .sign(zen_core::labels::SIG_CHECKPOINT, &bytes)
        .unwrap();
    let signed = [&(bytes.len() as u32).to_be_bytes(), bytes.as_slice(), &sig].concat();
    let sealed = seal_checkpoint(
        &e.u.seal_keys().unwrap(),
        &kd.fp(),
        1,
        &signed,
        &mut zen_core::rng::OsRng,
    )
    .unwrap();
    let _: zen_proto::CheckpointVersion =
        e.s.call(
            "/v1/checkpoint/put",
            &zen_proto::CheckpointPut {
                fs: 1,
                device: kd.fp().to_vec(),
                seq: 1,
                checkpoint: sealed,
            },
        )
        .await
        .unwrap();
    let c = dev(&srv, &kc).await;
    let err = c.i.sync().await.unwrap_err();
    let al = err.alarm().unwrap();
    assert_eq!(
        (al.kind, al.object),
        (AlarmKind::Invalid, AlarmObject::Checkpoint)
    );
    assert_eq!(al.id.as_deref(), Some(kd.fp().as_slice()));
    let _ = (&e.key, &e.i);
}
