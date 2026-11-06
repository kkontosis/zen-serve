//! `zen-ublk import` and `export` (docs/BLK.md §2): the CLI against an
//! in-process server, with images in a temp dir. No root needed.

#[path = "../../zen-client/tests/common/mod.rs"]
mod common;

use common::*;
use std::collections::BTreeSet;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use zen_blk::{BLOCK_SIZE, Volume, VolumeOptions};
use zen_client::{Client, ClientOptions, DeviceKey, Integrity, IntegrityOptions, UnlockedFs};
use zen_core::blk::VolumeKeys;
use zen_proto::{ByteBuf, KvGet, KvItems};

const BIN: &str = env!("CARGO_BIN_EXE_zen-ublk");
const BS: u64 = BLOCK_SIZE as u64;
const MIB: u64 = 1 << 20;
/// The images' size: 64 MiB, 16,384 blocks (64 windows of 256 blocks).
const SIZE: u64 = 64 * MIB;
const BLOCKS: u64 = SIZE / BS;

struct Dev {
    u: UnlockedFs,
    i: Integrity,
}

async fn dev(srv: &Server, key: &DeviceKey) -> Dev {
    let c = Client::connect(
        &srv.url,
        ClientOptions {
            origin: Some(srv.url.clone()),
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

/// A server claimed by key 1, fs 1 with slots for keys 1–3; the CLI runs
/// as key 2 (its key file in the temp dir), key 3 is another writer.
struct World {
    srv: Server,
    dir: tempfile::TempDir,
    key_file: PathBuf,
}

async fn world() -> World {
    let srv = Server::start().await;
    let (admin, w, other) = (key(1), key(2), key(3));
    srv.claim(&admin, &[&w, &other]).await;
    let a = srv.session(&admin).await;
    a.fs(1)
        .init_for(&[admin.public(), w.public(), other.public()], false)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("dev.json");
    w.save(&key_file).unwrap();
    World { srv, dir, key_file }
}

impl World {
    fn cmd(&self, extra: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(["--url", &self.srv.url, "--fs", "1", "--vol", "disk"])
            .arg("--device-key")
            .arg(&self.key_file)
            .args(extra);
        c
    }

    /// Run a command (in a thread: the server runs on this runtime).
    async fn run(&self, extra: &[&str]) -> Output {
        let mut c = self.cmd(extra);
        tokio::task::spawn_blocking(move || c.output().unwrap())
            .await
            .unwrap()
    }

    /// Run a command that must succeed: its stdout and stderr.
    async fn ok(&self, extra: &[&str]) -> (String, String) {
        let o = self.run(extra).await;
        let (out, err) = (
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        );
        assert!(o.status.success(), "{extra:?}: {}\n{err}", o.status);
        (out, err)
    }

    /// Run a command that must fail with exit 1: its stderr.
    async fn fails(&self, extra: &[&str]) -> String {
        let o = self.run(extra).await;
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        assert_eq!(o.status.code(), Some(1), "{extra:?}: {err}");
        err
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The default progress file: the volume pin's path + `.import`.
    fn progress(&self) -> PathBuf {
        let mut s = self.key_file.clone().into_os_string();
        s.push(format!(".vol-1-{}.import", zen_client::hex(b"disk")));
        PathBuf::from(s)
    }

    /// The volume's `seq`, from `info`.
    async fn seq(&self) -> u64 {
        let (out, _) = self.ok(&["info"]).await;
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        v["seq"].as_u64().unwrap()
    }
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Deterministic block contents (never all zeros).
fn block(seed: u64, i: u64) -> Vec<u8> {
    let mut x = (seed << 32 | i).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut b: Vec<u8> = (0..BLOCK_SIZE)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    b[0] |= 1;
    b
}

/// The test image's blocks with data: runs across window boundaries
/// (windows are 256 blocks), single blocks, and the last block.
fn data_blocks() -> BTreeSet<u64> {
    let mut s: BTreeSet<u64> = (0..300).collect();
    s.extend(512..600);
    s.extend([1000, 5000, 9999, BLOCKS - 1]);
    s
}

/// Blocks written as zeros (allocated in the image, zeros all the same).
fn zero_blocks() -> std::ops::Range<u64> {
    2000..2100
}

/// A sparse 64 MiB image: `data_blocks` from `seed`, `zero_blocks`
/// written as zeros, holes elsewhere. Its bytes.
fn make_image(path: &Path, seed: u64) -> Vec<u8> {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(SIZE).unwrap();
    let mut all = vec![0u8; SIZE as usize];
    for i in data_blocks() {
        let b = block(seed, i);
        f.write_all_at(&b, i * BS).unwrap();
        all[(i * BS) as usize..((i + 1) * BS) as usize].copy_from_slice(&b);
    }
    let z = zero_blocks();
    f.write_all_at(&vec![0u8; ((z.end - z.start) * BS) as usize], z.start * BS)
        .unwrap();
    f.sync_all().unwrap();
    all
}

/// Which blocks of the volume are stored.
async fn stored(u: &UnlockedFs) -> BTreeSet<u64> {
    let keys = VolumeKeys::new(&u.keys(), b"disk");
    let max = u.session().info().limits.max_range_items.clamp(1, 1024) as u64;
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < BLOCKS {
        let idx: Vec<u64> = (i..(i + max).min(BLOCKS)).collect();
        let r: KvItems = u
            .session()
            .call(
                "/v1/kv/get",
                &KvGet {
                    fs: 1,
                    keys: idx.iter().map(|&b| ByteBuf::from(keys.block(b))).collect(),
                    read_version: None,
                },
            )
            .await
            .unwrap();
        for (b, it) in idx.iter().zip(r.items) {
            if it.value.is_some() {
                out.insert(*b);
            }
        }
        i += max;
    }
    out
}

fn opts() -> VolumeOptions {
    VolumeOptions {
        idle_flush: None,
        ..Default::default()
    }
}

/// The offsets of `f` that `SEEK_DATA` reports as data, by block.
fn data_extents(path: &Path) -> BTreeSet<u64> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path).unwrap();
    let mut out = BTreeSet::new();
    let mut off: i64 = 0;
    loop {
        // SAFETY: lseek on our own descriptor.
        let d = unsafe { libc::lseek(f.as_raw_fd(), off, libc::SEEK_DATA) };
        if d < 0 {
            break;
        }
        // SAFETY: as above.
        let h = unsafe { libc::lseek(f.as_raw_fd(), d, libc::SEEK_HOLE) };
        out.extend((d as u64 / BS)..(h as u64).div_ceil(BS));
        off = h;
    }
    out
}

/// A sparse image into a new volume and back out: only blocks with data
/// are stored, and the export has the image's bytes with holes elsewhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_sparse_round_trip() {
    let w = world().await;
    let img = w.path("in.img");
    let bytes = make_image(&img, 1);
    let (out, _) = w.ok(&["import", "--create", p(&img)]).await;
    assert!(out.contains("\"size\":67108864"), "{out}");
    assert!(
        !w.progress().exists(),
        "the progress file is removed at the end"
    );
    let d = dev(&w.srv, &key(3)).await;
    assert_eq!(stored(&d.u).await, data_blocks());
    let out_img = w.path("out.img");
    w.ok(&["export", p(&out_img)]).await;
    assert!(
        std::fs::read(&out_img).unwrap() == bytes,
        "the export's bytes"
    );
    let m = std::fs::metadata(&out_img).unwrap();
    let data = data_blocks().len() as u64 * BS;
    assert!(
        m.blocks() * 512 <= data + MIB,
        "allocated {} for {data} of data",
        m.blocks() * 512
    );
    // Where the file system reports holes at all, they're where the image has none.
    let ext = data_extents(&out_img);
    if ext.len() < BLOCKS as usize {
        assert!(ext.is_superset(&data_blocks()));
        for z in zero_blocks() {
            assert!(!ext.contains(&z), "zero block {z} is a hole");
        }
    }
    drop(d.i);
    assert!(!w.path("out.img.part").exists());
}

/// Into an existing volume: what the image has as zeros or holes is
/// discarded, so the volume ends equal to the image.
#[tokio::test(flavor = "multi_thread")]
async fn into_an_existing_volume() {
    let w = world().await;
    let d = dev(&w.srv, &key(3)).await;
    let v = Volume::create(&d.u, b"disk", SIZE, opts()).await.unwrap();
    let old: Vec<u64> = (0..400).chain(1900..2200).chain(4000..4100).collect();
    for &i in &old {
        v.write(i * BS, &block(9, i)).await.unwrap();
    }
    v.close().await.unwrap();
    let img = w.path("in.img");
    let bytes = make_image(&img, 2);
    w.ok(&["import", p(&img)]).await;
    assert_eq!(stored(&d.u).await, data_blocks());
    let out_img = w.path("out.img");
    w.ok(&["export", p(&out_img)]).await;
    assert!(
        std::fs::read(&out_img).unwrap() == bytes,
        "the export's bytes"
    );
}

/// Sizes that don't match, an image that isn't whole blocks, and an export
/// onto an existing file.
#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    let w = world().await;
    w.ok(&["create", "64M"]).await;
    let seq = w.seq().await;
    let big = w.path("big.img");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(SIZE + BS)
        .unwrap();
    let err = w.fails(&["import", p(&big)]).await;
    assert!(err.contains("sizes must match"), "{err}");
    assert_eq!(w.seq().await, seq, "nothing written");

    let w2 = world().await;
    let odd = w2.path("odd.img");
    std::fs::File::create(&odd)
        .unwrap()
        .set_len(SIZE + 100)
        .unwrap();
    let err = w2.fails(&["import", "--create", p(&odd)]).await;
    assert!(err.contains("not a non-zero multiple of 4,096"), "{err}");
    let err = w2.fails(&["info"]).await;
    assert!(err.contains("not_found"), "no volume created: {err}");

    let there = w.path("there.img");
    std::fs::write(&there, b"keep").unwrap();
    let err = w.fails(&["export", p(&there)]).await;
    assert!(err.contains("--force"), "{err}");
    assert_eq!(std::fs::read(&there).unwrap(), b"keep");
    w.ok(&["export", "--force", p(&there)]).await;
    assert_eq!(std::fs::metadata(&there).unwrap().len(), SIZE);
}

/// Resuming: where it stopped; after a crash between a window's commit and
/// its record; and refused when the volume or the image changed.
#[tokio::test(flavor = "multi_thread")]
async fn resuming() {
    let w = world().await;
    let img = w.path("in.img");
    let bytes = make_image(&img, 3);
    let prog = w.progress();

    // Two windows, then one more.
    w.ok(&["import", "--create", "--stop-after", "2", p(&img)])
        .await;
    let after2 = std::fs::read(&prog).unwrap();
    let (_, err) = w.ok(&["import", "--stop-after", "1", p(&img)]).await;
    assert!(err.contains("resuming at offset 2097152"), "{err}");
    // As if it died after window 3's commit, before recording it: the
    // volume is one commit past the record, by this device.
    std::fs::write(&prog, &after2).unwrap();
    let (_, err) = w.ok(&["import", p(&img)]).await;
    assert!(err.contains("resuming at offset 2097152"), "{err}");
    assert!(!prog.exists());
    let out_img = w.path("out.img");
    w.ok(&["export", p(&out_img)]).await;
    assert!(std::fs::read(&out_img).unwrap() == bytes, "after the crash");

    // Another writer after a stop: refused, until --restart.
    let w = world().await;
    let img = w.path("in.img");
    let bytes = make_image(&img, 4);
    w.ok(&["import", "--create", "--stop-after", "1", p(&img)])
        .await;
    let d = dev(&w.srv, &key(3)).await;
    let v = Volume::open(&d.u, b"disk", opts()).await.unwrap();
    v.write(3000 * BS, &block(5, 1)).await.unwrap();
    v.close().await.unwrap();
    let err = w.fails(&["import", p(&img)]).await;
    assert!(
        err.contains("the volume changed since the import stopped") && err.contains("--restart"),
        "{err}"
    );
    w.ok(&["import", "--restart", p(&img)]).await;
    let out_img = w.path("out.img");
    w.ok(&["export", p(&out_img)]).await;
    assert!(std::fs::read(&out_img).unwrap() == bytes, "after --restart");

    // The image changed after a stop.
    w.ok(&["import", "--restart", "--stop-after", "1", p(&img)])
        .await;
    std::thread::sleep(std::time::Duration::from_millis(20));
    let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.write_all_at(&bytes[..BLOCK_SIZE], 0).unwrap();
    drop(f);
    let err = w.fails(&["import", p(&img)]).await;
    assert!(err.contains("the image changed"), "{err}");
}

/// An export whose volume's writer moves the root meanwhile fails, and
/// leaves no file behind.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_while_the_root_moves() {
    let w = world().await;
    let img = w.path("in.img");
    make_image(&img, 6);
    w.ok(&["import", "--create", p(&img)]).await;
    let out_img = w.path("out.img");
    let go = w.path("go");
    let mut c = w.cmd(&[
        "export",
        "--pause-after",
        "1",
        "--pause-file",
        p(&go),
        p(&out_img),
    ]);
    let export = tokio::task::spawn_blocking(move || c.output().unwrap());
    // The export holds no lease: a writer opens beside it.
    let d = dev(&w.srv, &key(3)).await;
    let v = Volume::open(&d.u, b"disk", opts()).await.unwrap();
    // The export has started (it writes IMAGE.part first).
    let t = std::time::Instant::now();
    while !w.path("out.img.part").exists() && !out_img.exists() {
        assert!(t.elapsed().as_secs() < 60, "the export didn't start");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // A block of window 3, past where the export paused.
    v.write(520 * BS, &block(7, 7)).await.unwrap();
    v.flush().await.unwrap();
    std::fs::write(&go, b"").unwrap();
    let o = export.await.unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("the volume changed during the export"),
        "{err}"
    );
    assert!(!out_img.exists() && !w.path("out.img.part").exists());
    v.close().await.unwrap();
}
