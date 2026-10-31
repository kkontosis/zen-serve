//! The adapter both frontends call (`Disk`), against an in-process server:
//! requests and their errnos, failing closed after a lost lease or an
//! alarm, retries across an outage, and a commit whose answer was lost.
//! No root or kernel devices needed (those are tests/devices.rs).

#[path = "../../zen-client/tests/common/mod.rs"]
mod common;

use common::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use zen_blk::{BLOCK_SIZE, Volume, VolumeOptions};
use zen_client::{Client, ClientOptions, DeviceKey, Integrity, IntegrityOptions, UnlockedFs};
use zen_core::blk::VolumeKeys;
use zen_proto::ByteBuf;
use zen_ublk::disk::Disk;

const MIB: u64 = 1 << 20;
const BS: u64 = BLOCK_SIZE as u64;

struct Dev {
    u: UnlockedFs,
    i: Integrity,
}

async fn dev_at(url: &str, origin: &str, key: &DeviceKey) -> Dev {
    let c = Client::connect(
        url,
        ClientOptions {
            origin: Some(origin.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let s = c.sign_in_device(key).await.unwrap();
    let fs = s.fs(1);
    let i = fs
        .integrity(IntegrityOptions {
            signer: Some(key.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let u = fs.unlock_device(&key.device).await.unwrap();
    Dev { u, i }
}

async fn dev(srv: &Server, key: &DeviceKey) -> Dev {
    dev_at(&srv.url, &srv.url, key).await
}

/// A server claimed by key 1, with fs 1 holding a slot for each key.
async fn world(keys: &[&DeviceKey]) -> (Server, zen_client::Session) {
    let srv = Server::start().await;
    let admin = key(1);
    srv.claim(&admin, keys).await;
    let a = srv.session(&admin).await;
    let mut devices = vec![admin.public()];
    devices.extend(keys.iter().map(|k| k.public()));
    a.fs(1).init_for(&devices, false).await.unwrap();
    (srv, a)
}

fn opts() -> VolumeOptions {
    VolumeOptions {
        idle_flush: None,
        ..Default::default()
    }
}

fn block_of(tag: &str) -> Vec<u8> {
    tag.bytes().cycle().take(BLOCK_SIZE).collect()
}

const T: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn requests_and_their_errnos() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let w = dev(&srv, &kw).await;
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    let d = Disk::new(v.clone(), T);
    assert_eq!(d.size(), 64 * MIB);
    assert!(!d.read_only());

    // Writes, unaligned too, read back before and after a flush.
    assert_eq!(d.read(0, 8192).await.unwrap(), vec![0; 8192]);
    d.write(100, b"hello", false).await.unwrap();
    d.write(BS - 2, b"edge", false).await.unwrap();
    let b = d.read(0, 2 * BLOCK_SIZE).await.unwrap();
    assert_eq!(&b[100..105], b"hello");
    assert_eq!(&b[BLOCK_SIZE - 2..BLOCK_SIZE + 2], b"edge");
    let seq = v.root().0;
    d.flush().await.unwrap();
    assert_eq!(v.root().0, seq + 1);

    // FUA: committed before the write returns.
    d.write(5 * BS, &block_of("fua"), true).await.unwrap();
    assert_eq!(v.root().0, seq + 2);

    // A discard reads as zeros; its edges are zeroed.
    d.discard(5 * BS + 10, BS).await.unwrap();
    let b = d.read(5 * BS, 2 * BLOCK_SIZE).await.unwrap();
    assert_eq!(&b[..10], &block_of("fua")[..10]);
    assert!(b[10..BLOCK_SIZE + 10].iter().all(|x| *x == 0));

    // Past the end: EINVAL.
    assert_eq!(d.read(64 * MIB - 10, 20).await.unwrap_err(), libc::EINVAL);
    assert_eq!(
        d.write(64 * MIB, b"x", false).await.unwrap_err(),
        libc::EINVAL
    );
    assert!(d.failure().is_none(), "EINVAL doesn't fail the disk");
    d.close().await.unwrap();

    // Read-only: EROFS for writes and discards, reads work, a flush is a no-op.
    let r = Disk::new(
        Volume::open_read_only(&w.u, b"disk", opts()).await.unwrap(),
        T,
    );
    assert!(r.read_only());
    assert_eq!(&r.read(100, 5).await.unwrap(), b"hello");
    assert_eq!(r.write(0, b"x", false).await.unwrap_err(), libc::EROFS);
    assert_eq!(r.discard(0, BS).await.unwrap_err(), libc::EROFS);
    r.flush().await.unwrap();
    r.close().await.unwrap();
    assert!(w.i.alarms().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_lease_fails_the_disk_closed() {
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
    let da = Disk::new(va.clone(), T);
    da.write(0, &block_of("a"), true).await.unwrap();
    // A's renewals stop; the lease lapses and B takes the volume over.
    va.pause_renewals(true);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let vb = Volume::open(&b.u, b"disk", o).await.unwrap();
    vb.write(0, &block_of("b")).await.unwrap();
    vb.flush().await.unwrap();

    da.write(BS, &block_of("late"), false).await.unwrap();
    assert_eq!(da.flush().await.unwrap_err(), libc::EIO);
    let f = da.failure().expect("failed");
    assert_eq!(f.code, 1);
    assert!(f.message.contains("lease_lost"), "{}", f.message);
    // Every request fails from now on, reads too.
    assert_eq!(da.read(0, 10).await.unwrap_err(), libc::EIO);
    assert_eq!(da.write(0, b"x", false).await.unwrap_err(), libc::EIO);
    assert_eq!(da.close().await.unwrap_err().code, 1);
    vb.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_alarm_fails_the_disk_closed() {
    let (kw, kr) = (key(2), key(3));
    let (srv, admin) = world(&[&kw, &kr]).await;
    let (w, r) = (dev(&srv, &kw).await, dev(&srv, &kr).await);
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();
    v.write(0, &block_of("old")).await.unwrap();
    v.flush().await.unwrap();
    // Block 0 frozen at its old value, then written again.
    let keys = VolumeKeys::new(&w.u.keys(), b"disk");
    lie(
        &admin,
        vec![Lie::Kv {
            fs: 1,
            key: ByteBuf::from(keys.block(0)),
        }],
    )
    .await;
    v.write(0, &block_of("new")).await.unwrap();
    v.flush().await.unwrap();

    let d = Disk::new(
        Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap(),
        T,
    );
    assert_eq!(d.read(0, 10).await.unwrap_err(), libc::EIO);
    let f = d.failure().expect("failed");
    assert_eq!(f.code, 3, "an alarm: exit code 3");
    assert!(f.message.contains("rollback"), "{}", f.message);
    // Other blocks would verify, but the disk stays failed.
    assert_eq!(d.read(BS, 10).await.unwrap_err(), libc::EIO);
    assert_eq!(r.i.alarms().len(), 1);
    v.close().await.unwrap();
}

// ------------------------------------------------------------- the network

/// What the proxy does.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Up,
    /// Refuse connections and drop the open ones.
    Down,
    /// Forward the next commit, swallow its answer, then go down.
    LoseNextCommit,
}

/// A TCP proxy in front of the server, to take it away.
struct Proxy {
    url: String,
    mode: Arc<Mutex<Mode>>,
    down: watch::Sender<bool>,
}

impl Proxy {
    async fn start(to: &str) -> Proxy {
        let upstream = to.trim_start_matches("http://").to_string();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let mode = Arc::new(Mutex::new(Mode::Up));
        let (down, _) = watch::channel(false);
        let (m, d) = (mode.clone(), down.clone());
        tokio::spawn(async move {
            loop {
                let Ok((c, _)) = l.accept().await else { return };
                if *m.lock().unwrap() == Mode::Down {
                    drop(c);
                    continue;
                }
                let Ok(s) = TcpStream::connect(&upstream).await else {
                    continue;
                };
                tokio::spawn(pipe(c, s, m.clone(), d.subscribe(), d.clone()));
            }
        });
        Proxy { url, mode, down }
    }

    fn set(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
        self.down.send_replace(m == Mode::Down);
    }

    fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }
}

async fn pipe(
    mut c: TcpStream,
    mut s: TcpStream,
    mode: Arc<Mutex<Mode>>,
    mut down: watch::Receiver<bool>,
    tell: watch::Sender<bool>,
) {
    let (mut cr, mut cw) = c.split();
    let (mut sr, mut sw) = s.split();
    let mut swallow = false;
    let (mut a, mut b) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        tokio::select! {
            n = cr.read(&mut a) => {
                let Ok(n @ 1..) = n else { return };
                {
                    let mut m = mode.lock().unwrap();
                    if *m == Mode::LoseNextCommit
                        && a[..n].windows(15).any(|w| w == b"POST /v1/commit")
                    {
                        swallow = true;
                        *m = Mode::Down;
                    }
                }
                if sw.write_all(&a[..n]).await.is_err() { return }
            }
            n = sr.read(&mut b) => {
                let Ok(n @ 1..) = n else { return };
                if swallow {
                    // The server answered (so it applied): the client never hears.
                    tell.send_replace(true);
                    return;
                }
                if cw.write_all(&b[..n]).await.is_err() { return }
            }
            _ = async { let _ = down.wait_for(|d| *d).await; } => return,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outage_is_retried_within_the_io_timeout() {
    let kw = key(2);
    let (srv, _) = world(&[&kw]).await;
    let p = Proxy::start(&srv.url).await;
    let w = dev_at(&p.url, &srv.url, &kw).await;
    let v = Volume::create(&w.u, b"disk", 64 * MIB, opts())
        .await
        .unwrap();

    // The server goes away for a second: a flush waits it out.
    let d = Disk::new(v.clone(), T);
    d.write(0, &block_of("one"), false).await.unwrap();
    p.set(Mode::Down);
    let flush = tokio::spawn({
        let d = d.clone();
        async move { d.flush().await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!flush.is_finished());
    p.set(Mode::Up);
    flush.await.unwrap().unwrap();
    assert_eq!(v.root().0, 2);

    // With a short timeout it gives up: EIO, the disk not failed, and the
    // blocks still buffered, so the next flush commits them.
    let short = Disk::new(v.clone(), Duration::from_millis(300));
    short.write(BS, &block_of("two"), false).await.unwrap();
    p.set(Mode::Down);
    assert_eq!(short.flush().await.unwrap_err(), libc::EIO);
    assert!(short.failure().is_none());
    p.set(Mode::Up);
    short.flush().await.unwrap();
    assert_eq!(v.root().0, 3);
    v.close().await.unwrap();

    let r = dev(&srv, &kw).await;
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    let mut b = vec![0u8; 2 * BLOCK_SIZE];
    rv.read(0, &mut b).await.unwrap();
    assert_eq!(&b[..BLOCK_SIZE], &block_of("one")[..]);
    assert_eq!(&b[BLOCK_SIZE..], &block_of("two")[..]);
    assert!(w.i.alarms().is_empty() && r.i.alarms().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_whose_answer_was_lost_is_settled_from_the_root() {
    let (kw, kr) = (key(2), key(3));
    let (srv, _) = world(&[&kw, &kr]).await;
    let p = Proxy::start(&srv.url).await;
    let w = dev_at(&p.url, &srv.url, &kw).await;
    let pin = srv.dir.path().join("disk.pin");
    let v = Volume::create(
        &w.u,
        b"disk",
        64 * MIB,
        VolumeOptions {
            pin: Some(pin.clone()),
            ..opts()
        },
    )
    .await
    .unwrap();
    let d = Disk::new(v.clone(), T);
    d.write(0, &block_of("lost"), false).await.unwrap();

    // The commit applies, its answer is lost, and the server stays out of
    // reach while the writer asks again and reads the root: the outcome
    // stays unknown, until the server is back.
    p.set(Mode::LoseNextCommit);
    let flush = tokio::spawn({
        let d = d.clone();
        async move { d.flush().await }
    });
    until(|| p.mode() == Mode::Down).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!flush.is_finished());
    p.set(Mode::Up);
    flush.await.unwrap().unwrap();

    // The lost commit was seq 2; settling saw it applied, so the buffered
    // block went again as seq 3: one root record per seq, a chain.
    assert_eq!(v.root().0, 3);
    let r = dev(&srv, &kr).await;
    r.i.sync().await.unwrap();
    let rv = Volume::open_read_only(&r.u, b"disk", opts()).await.unwrap();
    assert_eq!(rv.root(), v.root());
    let mut b = vec![0u8; BLOCK_SIZE];
    rv.read(0, &mut b).await.unwrap();
    assert_eq!(b, block_of("lost"));
    v.close().await.unwrap();
    // A reopen with the pin agrees too.
    let v2 = Volume::open(
        &w.u,
        b"disk",
        VolumeOptions {
            pin: Some(pin),
            ..opts()
        },
    )
    .await
    .unwrap();
    assert_eq!(v2.root().0, 3);
    v2.close().await.unwrap();
    assert!(w.i.alarms().is_empty() && r.i.alarms().is_empty());
}
