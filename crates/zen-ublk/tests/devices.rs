//! zen-ublk as a real block device (feature `device-tests`; needs root,
//! FUSE, loop devices, e2fsprogs; the ublk tests also feature `ublk` and
//! the ublk driver). The server runs in-process on a runtime of its own;
//! `zen-ublk` runs as a child process, so a writer can be killed for real.
//!
//! The ublk tests skip, saying so, only when the kernel has no
//! `/dev/ublk-control`; with `ZEN_TEST_REQUIRE_UBLK=1` (CI) they fail.
//!
//! The benches are `#[ignore]`d (docs/BLK.md §7–§8, docs/STATS.md §6.3):
//! `bench_fuse` and `bench_ublk` (ext4 on a volume), `bench_file_cdc` and
//! `bench_file_fixed` (ext4 on an image kept as a zen-fs file, through
//! zen-mount and a loop device: `npm run build` first, or `$ZEN_MOUNT` for
//! its `cli.js`), and `bench_image` (import and export). Run them with
//! `cargo test --release -p zen-ublk --features device-tests[,ublk] --test
//! devices bench -- --ignored --nocapture --test-threads 1`, as root.

#[path = "../../zen-client/tests/common/mod.rs"]
mod common;

use common::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zen-ublk");

/// The tests' lease (the default is 30 s). It is renewed every third of it,
/// so a renewal has two thirds of it to land. That covers a renewal waiting
/// behind the writer's own 1 MiB commits on a debug server with several
/// tests at once: with 2 s (1.33 s of slack) a FUSE test lost its lease on
/// CI twice.
const LEASE_MS: &str = "6000";
/// Longer than the lease: a killed writer's lease has lapsed.
const LAPSE: Duration = Duration::from_millis(6500);

/// A server claimed by key 1, fs 1 with slots for key 1 and the writer,
/// and the writer's device key file.
struct World {
    srv: Server,
    dir: tempfile::TempDir,
    key_file: PathBuf,
    /// Options every command gets: short leases and idle flushes for the
    /// tests, the defaults for the benches.
    tuning: Vec<&'static str>,
}

/// The server's runtime. The test bodies call blocking code (`losetup`,
/// `mkfs`, `mount`, writes into a FUSE mount, `sync`) from async code; on a
/// shared runtime a blocked worker could delay the server's answer to a
/// writer's lease renewal past the lease's slack (`lease_lost`).
fn server_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("test-server")
            .enable_all()
            .build()
            .unwrap()
    })
}

async fn world() -> World {
    let srv = server_runtime().spawn(Server::start()).await.unwrap();
    let (admin, w) = (key(1), key(2));
    srv.claim(&admin, &[&w]).await;
    let a = srv.session(&admin).await;
    a.fs(1)
        .init_for(&[admin.public(), w.public()], false)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("dev.json");
    w.save(&key_file).unwrap();
    World {
        srv,
        dir,
        key_file,
        tuning: vec!["--lease-ttl-ms", LEASE_MS, "--idle-flush-ms", "1000"],
    }
}

impl World {
    fn cmd(&self, extra: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(["--url", &self.srv.url, "--fs", "1", "--vol", "disk"])
            .arg("--device-key")
            .arg(&self.key_file)
            .args(&self.tuning)
            .args(extra);
        c
    }

    /// Run a command that exits (in a thread: the server runs on this runtime).
    async fn run(&self, extra: &[&str]) -> std::process::Output {
        let mut c = self.cmd(extra);
        tokio::task::spawn_blocking(move || c.output().unwrap())
            .await
            .unwrap()
    }

    async fn create(&self, size: &str) {
        let o = self.run(&["create", size]).await;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    /// Start a daemon and wait for its `ready <path>` line.
    async fn serve(&self, extra: &[&str]) -> Daemon {
        let mut c = self.cmd(extra);
        c.stdout(Stdio::piped());
        tokio::task::spawn_blocking(move || {
            let mut child = c.spawn().unwrap();
            // A daemon not ready within 60 s is killed, so the test fails
            // instead of hanging.
            let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (r, pid) = (ready.clone(), child.id() as i32);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(60));
                if !r.load(std::sync::atomic::Ordering::SeqCst) {
                    // SAFETY: kill sends a signal to our own child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            });
            let mut out = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            out.read_line(&mut line).unwrap();
            let path = line
                .strip_prefix("ready ")
                .unwrap_or_else(|| {
                    let st = child.wait().unwrap();
                    panic!("not ready ({st}): {line:?}")
                })
                .trim()
                .to_string();
            ready.store(true, std::sync::atomic::Ordering::SeqCst);
            Daemon {
                child: Some(child),
                path: PathBuf::from(path),
            }
        })
        .await
        .unwrap()
    }

    fn mnt(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

/// A running `zen-ublk`; killed if still running when dropped.
struct Daemon {
    child: Option<Child>,
    /// The file or device it serves.
    path: PathBuf,
}

impl Daemon {
    /// SIGTERM, and the exit code.
    async fn stop(mut self) -> i32 {
        let mut c = self.child.take().unwrap();
        // SAFETY: kill sends a signal to our own child.
        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
        tokio::task::spawn_blocking(move || c.wait().unwrap().code().unwrap_or(-1))
            .await
            .unwrap()
    }

    /// SIGKILL: a crash.
    async fn kill(mut self) {
        let mut c = self.child.take().unwrap();
        c.kill().unwrap();
        tokio::task::spawn_blocking(move || c.wait().unwrap())
            .await
            .unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Undoes mounts and loop devices when dropped, even after a panic.
#[derive(Default)]
struct Cleanup {
    mounts: Vec<PathBuf>,
    loops: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for m in self.mounts.iter().rev() {
            let _ = Command::new("umount").arg("-l").arg(m).status();
        }
        for l in &self.loops {
            let _ = Command::new("losetup").args(["-d", l]).status();
        }
    }
}

fn sh(cmd: &str, args: &[&str]) -> (i32, String) {
    let o = Command::new(cmd).args(args).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    (o.status.code().unwrap_or(-1), out)
}

fn ok(cmd: &str, args: &[&str]) -> String {
    let (code, out) = sh(cmd, args);
    assert_eq!(code, 0, "{cmd} {args:?}: {out}");
    out
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A loop device over `file` (4,096-byte sectors).
fn losetup(file: &Path, read_only: bool, c: &mut Cleanup) -> String {
    let mut args = vec!["-b", "4096", "-f", "--show"];
    if read_only {
        args.push("-r");
    }
    args.push(p(file));
    let dev = ok("losetup", &args).trim().to_string();
    c.loops.push(dev.clone());
    dev
}

fn mount(dev: &str, at: &Path, opts: &str, c: &mut Cleanup) {
    ok("mount", &["-o", opts, dev, p(at)]);
    c.mounts.push(at.to_path_buf());
}

fn umount(at: &Path, c: &mut Cleanup) {
    ok("umount", &[p(at)]);
    c.mounts.retain(|m| m != at);
}

fn detach(dev: &str, c: &mut Cleanup) {
    ok("losetup", &["-d", dev]);
    c.loops.retain(|l| l != dev);
}

/// Files of seeded random contents in nested directories (relative path → bytes).
fn files(seed: u64, n: usize) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut x = seed | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut out = BTreeMap::new();
    for i in 0..n {
        let len = 1 + (next() % (192 * 1024)) as usize;
        let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        let path = PathBuf::from(format!("s{seed}/d{}/e{}/f{i}.bin", i % 3, i % 5));
        out.insert(path, data);
    }
    out
}

fn write_files(root: &Path, fs: &BTreeMap<PathBuf, Vec<u8>>) {
    for (rel, data) in fs {
        let at = root.join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, data).unwrap();
    }
}

fn check_files(root: &Path, fs: &BTreeMap<PathBuf, Vec<u8>>) {
    for (rel, data) in fs {
        let got = std::fs::read(root.join(rel)).unwrap_or_else(|e| panic!("{rel:?}: {e}"));
        assert!(got == *data, "{rel:?} differs");
    }
}

fn mkfs(dev: &str) {
    ok("mkfs.ext4", &["-q", "-F", "-b", "4096", dev]);
}

/// e2fsck after a crash: it may replay the journal (exit 1), then the fs is clean.
fn fsck_after_crash(dev: &str) {
    let (code, out) = sh("e2fsck", &["-fy", dev]);
    assert!(code == 0 || code == 1, "e2fsck -fy exited {code}: {out}");
    let (code, out) = sh("e2fsck", &["-fn", dev]);
    assert_eq!(code, 0, "not clean after the replay: {out}");
}

// -------------------------------------------------------------------- FUSE

#[tokio::test(flavor = "multi_thread")]
async fn fuse_file() {
    let w = world().await;
    w.create("64M").await;
    let dir = w.mnt("fuse");
    let d = w.serve(&["fuse", p(&dir)]).await;
    assert_eq!(d.path, dir.join("disk.img"));
    let meta = std::fs::metadata(&d.path).unwrap();
    assert_eq!(meta.len(), 64 << 20);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    // pwrite, aligned and not; fsync; read back.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&d.path)
        .unwrap();
    let block: Vec<u8> = (0..4096u32).map(|x| (x * 7) as u8).collect();
    f.write_all_at(&block, 8192).unwrap();
    f.write_all_at(b"unaligned", 4090).unwrap();
    f.sync_all().unwrap();
    let mut b = vec![0u8; 4096];
    f.read_exact_at(&mut b, 8192).unwrap();
    assert_eq!(b, block);
    let mut b = vec![0u8; 9];
    f.read_exact_at(&mut b, 4090).unwrap();
    assert_eq!(&b, b"unaligned");
    // The end: a short read, then nothing.
    let mut tail = Vec::new();
    let mut g = std::fs::File::open(&d.path).unwrap();
    std::io::Seek::seek(&mut g, std::io::SeekFrom::Start((64 << 20) - 10)).unwrap();
    g.read_to_end(&mut tail).unwrap();
    assert_eq!(tail.len(), 10);
    // No resizing.
    assert_eq!(
        f.set_len(1 << 20).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    drop((f, g));

    // A second writer: busy.
    let o = w.run(&["fuse", p(&w.mnt("other"))]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("busy"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );

    assert_eq!(d.stop().await, 0);
    // Again, and read-only.
    let d = w.serve(&["--read-only", "fuse", p(&dir)]).await;
    let mut b = vec![0u8; 4096];
    std::fs::File::open(&d.path)
        .unwrap()
        .read_exact_at(&mut b, 8192)
        .unwrap();
    assert_eq!(b, block);
    let e = std::fs::OpenOptions::new()
        .write(true)
        .open(&d.path)
        .unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{e}");
    assert_eq!(d.stop().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn fuse_ext4_round_trip() {
    let w = world().await;
    w.create("64M").await;
    let (dir, mnt) = (w.mnt("fuse"), w.mnt("ext4"));
    let data = files(1, 24);
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["fuse", p(&dir)]).await;
        let dev = losetup(&d.path, false, &mut c);
        mkfs(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        write_files(&mnt, &data);
        ok("sync", &[]);
        umount(&mnt, &mut c);
        detach(&dev, &mut c);
        assert_eq!(d.stop().await, 0);
    }
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["fuse", p(&dir)]).await;
        let dev = losetup(&d.path, false, &mut c);
        ok("e2fsck", &["-fn", &dev]);
        mount(&dev, &mnt, "rw", &mut c);
        check_files(&mnt, &data);
        umount(&mnt, &mut c);
        detach(&dev, &mut c);
        assert_eq!(d.stop().await, 0);
    }
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["--read-only", "fuse", p(&dir)]).await;
        let dev = losetup(&d.path, true, &mut c);
        mount(&dev, &mnt, "ro,noload", &mut c);
        check_files(&mnt, &data);
        umount(&mnt, &mut c);
        detach(&dev, &mut c);
        assert_eq!(d.stop().await, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fuse_writer_killed() {
    let w = world().await;
    w.create("64M").await;
    let (dir, mnt) = (w.mnt("fuse"), w.mnt("ext4"));
    let (synced, unsynced) = (files(2, 16), files(3, 16));
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["fuse", p(&dir)]).await;
        let dev = losetup(&d.path, false, &mut c);
        mkfs(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        write_files(&mnt, &synced);
        ok("sync", &[]);
        write_files(&mnt, &unsynced);
        d.kill().await;
        // The dead export: lazily away.
        let _ = sh("umount", &["-l", p(&mnt)]);
        c.mounts.clear();
        let _ = sh("losetup", &["-d", &dev]);
        c.loops.clear();
        let _ = sh("umount", &["-l", p(&dir)]);
    }
    // The lease is still held: a restart at once is refused.
    let o = w.run(&["fuse", p(&dir)]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("busy"));
    tokio::time::sleep(LAPSE).await;
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["fuse", p(&dir)]).await;
        let dev = losetup(&d.path, false, &mut c);
        fsck_after_crash(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        check_files(&mnt, &synced);
        umount(&mnt, &mut c);
        detach(&dev, &mut c);
        assert_eq!(d.stop().await, 0);
    }
}

/// An ext4 image made without mounting it (`mkfs.ext4 -d`) is imported,
/// served, mounted and changed, then exported and checked offline.
#[tokio::test(flavor = "multi_thread")]
async fn image_import_serve_export() {
    let w = world().await;
    let src = w.mnt("src");
    let data = files(7, 40);
    write_files(&src, &data);
    let img = w.dir.path().join("in.img");
    ok(
        "mkfs.ext4",
        &["-q", "-F", "-b", "4096", "-d", p(&src), p(&img), "64M"],
    );
    let o = w.run(&["import", "--create", p(&img)]).await;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (dir, mnt) = (w.mnt("fuse"), w.mnt("ext4"));
    let added: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["fuse", p(&dir)]).await;
        let dev = losetup(&d.path, false, &mut c);
        mount(&dev, &mnt, "rw", &mut c);
        check_files(&mnt, &data);
        std::fs::write(mnt.join("added"), &added).unwrap();
        ok("sync", &[]);
        umount(&mnt, &mut c);
        detach(&dev, &mut c);
        assert_eq!(d.stop().await, 0);
    }
    let out = w.dir.path().join("out.img");
    let o = w.run(&["export", p(&out)]).await;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    ok("e2fsck", &["-fn", p(&out)]);
    let dump = w.dir.path().join("added.out");
    ok(
        "debugfs",
        &["-R", &format!("dump /added {}", p(&dump)), p(&out)],
    );
    assert!(std::fs::read(&dump).unwrap() == added, "the added file");
    let first = data.keys().next().unwrap();
    ok(
        "debugfs",
        &[
            "-R",
            &format!("dump /{} {}", first.display(), p(&dump)),
            p(&out),
        ],
    );
    assert!(
        std::fs::read(&dump).unwrap() == data[first],
        "an imported file"
    );
}

// -------------------------------------------------------------------- ublk

/// Whether the kernel has ublk; without it, a test says so and skips,
/// unless `ZEN_TEST_REQUIRE_UBLK=1`.
#[cfg(feature = "ublk")]
fn have_ublk() -> bool {
    if Path::new("/dev/ublk-control").exists() {
        return true;
    }
    assert!(
        std::env::var("ZEN_TEST_REQUIRE_UBLK").as_deref() != Ok("1"),
        "no /dev/ublk-control, and ZEN_TEST_REQUIRE_UBLK=1"
    );
    eprintln!("skipped: no /dev/ublk-control (modprobe ublk_drv)");
    false
}

#[cfg(feature = "ublk")]
#[tokio::test(flavor = "multi_thread")]
async fn ublk_ext4_round_trip() {
    if !have_ublk() {
        return;
    }
    let w = world().await;
    w.create("64M").await;
    let mnt = w.mnt("ext4");
    let data = files(4, 24);
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["ublk", "--id", "41"]).await;
        let dev = p(&d.path).to_string();
        assert_eq!(dev, "/dev/ublkb41");
        mkfs(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        write_files(&mnt, &data);
        ok("sync", &[]);
        umount(&mnt, &mut c);
        assert_eq!(d.stop().await, 0);
    }
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["ublk", "--id", "41"]).await;
        let dev = p(&d.path).to_string();
        ok("e2fsck", &["-fn", &dev]);
        mount(&dev, &mnt, "rw", &mut c);
        check_files(&mnt, &data);
        umount(&mnt, &mut c);
        assert_eq!(d.stop().await, 0);
    }
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["--read-only", "ublk"]).await;
        let dev = p(&d.path).to_string();
        mount(&dev, &mnt, "ro,noload", &mut c);
        check_files(&mnt, &data);
        umount(&mnt, &mut c);
        assert_eq!(d.stop().await, 0);
    }
}

#[cfg(feature = "ublk")]
#[tokio::test(flavor = "multi_thread")]
async fn ublk_writer_killed() {
    if !have_ublk() {
        return;
    }
    let w = world().await;
    w.create("64M").await;
    let mnt = w.mnt("ext4");
    let (synced, unsynced) = (files(5, 16), files(6, 16));
    {
        let mut c = Cleanup::default();
        let d = w.serve(&["ublk", "--id", "42"]).await;
        let dev = p(&d.path).to_string();
        mkfs(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        write_files(&mnt, &synced);
        ok("sync", &[]);
        write_files(&mnt, &unsynced);
        d.kill().await;
        let _ = sh("umount", &["-l", p(&mnt)]);
        c.mounts.clear();
    }
    // The lease lapses.
    tokio::time::sleep(LAPSE).await;
    {
        // The same id: the dead device is deleted first.
        let mut c = Cleanup::default();
        let d = w.serve(&["ublk", "--id", "42"]).await;
        let dev = p(&d.path).to_string();
        fsck_after_crash(&dev);
        mount(&dev, &mnt, "rw", &mut c);
        check_files(&mnt, &synced);
        umount(&mnt, &mut c);
        assert_eq!(d.stop().await, 0);
    }
}

// -------------------------------------------------------------------- benches

/// A tarball of the repository's `crates`, `spec` and `docs` (many small
/// files; the same bytes for the same tree): its path, files and bytes.
fn source_tarball(at: &Path) -> (PathBuf, usize, u64) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = at.join("src.tar");
    ok(
        "tar",
        &[
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
            p(&out),
            "-C",
            p(&repo),
            "crates",
            "spec",
            "docs",
        ],
    );
    let list = ok("tar", &["-tvf", p(&out)]);
    let (mut n, mut bytes) = (0, 0u64);
    for l in list.lines().filter(|l| l.starts_with('-')) {
        n += 1;
        bytes += l.split_whitespace().nth(2).unwrap().parse::<u64>().unwrap();
    }
    (out, n, bytes)
}

fn drop_caches() {
    ok("sync", &[]);
    std::fs::write("/proc/sys/vm/drop_caches", "3").unwrap();
}

fn timed(f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64()
}

/// ext4 on `dev` (an empty 1 GiB volume): mkfs, a sequential write and
/// read, synchronous 4 KiB writes, and an untar. Prints a table.
fn bench_device(name: &str, dev: &str, mnt: &Path, tarball: &(PathBuf, usize, u64)) {
    let mut c = Cleanup::default();
    const MIB: u64 = 256;
    const SMALL: u64 = 256;
    let mkfs_s = timed(|| mkfs(dev));
    mount(dev, mnt, "rw", &mut c);
    let big = mnt.join("big");
    let write_s = timed(|| {
        ok(
            "dd",
            &[
                "if=/dev/urandom",
                &format!("of={}", p(&big)),
                "bs=1M",
                &format!("count={MIB}"),
                "conv=fsync",
                "status=none",
            ],
        );
    });
    drop_caches();
    let read_s = timed(|| {
        ok(
            "dd",
            &[
                &format!("if={}", p(&big)),
                "of=/dev/null",
                "bs=1M",
                "status=none",
            ],
        );
    });
    let small = mnt.join("small");
    let dsync_s = timed(|| {
        ok(
            "dd",
            &[
                "if=/dev/urandom",
                &format!("of={}", p(&small)),
                "bs=4k",
                &format!("count={SMALL}"),
                "oflag=dsync",
                "status=none",
            ],
        );
    });
    let src = mnt.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let untar_s = timed(|| {
        ok("tar", &["-xf", p(&tarball.0), "-C", p(&src)]);
        ok("sync", &[]);
    });
    umount(mnt, &mut c);
    println!("\n{name}, ext4 on a 1 GiB volume");
    println!("  mkfs.ext4                        {mkfs_s:>8.2} s");
    println!(
        "  write {MIB} MiB, conv=fsync         {:>8.1} MiB/s",
        MIB as f64 / write_s
    );
    println!(
        "  read {MIB} MiB, after drop_caches    {:>8.1} MiB/s",
        MIB as f64 / read_s
    );
    println!(
        "  4 KiB writes, oflag=dsync        {:>8.2} ms each",
        dsync_s * 1000.0 / SMALL as f64
    );
    println!(
        "  untar {} files, {:.1} MiB, sync  {untar_s:>8.2} s",
        tarball.1,
        tarball.2 as f64 / (1 << 20) as f64
    );
}

async fn bench_world() -> World {
    let mut w = world().await;
    // The daemon's defaults (30 s lease, 5 s idle flush, 256-block batches).
    w.tuning = vec![];
    w
}

/// The peak RSS of a process, in MiB (`VmHWM`).
fn peak_rss_mib(pid: u32) -> f64 {
    let st = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kb: f64 = st
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    kb / 1024.0
}

/// What fs 1 stores, in MiB (its quota's bytes). Content that superseded
/// versions or an import's discards released is still counted until the
/// grace (a day by default) has passed.
async fn stored_mib(w: &World) -> f64 {
    let st = &w.srv.server.state;
    let mut t = st.store.begin(None).await.unwrap();
    let bytes = t
        .get(&zen_server::keys::quota(1, "bytes"))
        .await
        .unwrap()
        .map_or(0, |v| i64::from_le_bytes(v.try_into().unwrap()));
    bytes as f64 / (1 << 20) as f64
}

/// After a bench: the serving process's peak RSS, the time from SIGTERM to
/// its exit (for zen-mount, the drain: everything sent), and what the fs
/// stores then.
async fn report(w: &World, name: &str, d: Daemon) {
    let pid = d.child.as_ref().unwrap().id();
    let rss = peak_rss_mib(pid);
    let t = Instant::now();
    assert_eq!(d.stop().await, 0);
    let stop_s = t.elapsed().as_secs_f64();
    let stored = stored_mib(w).await;
    println!("  {name}: stop (flush or drain)    {stop_s:>8.2} s");
    println!("  {name}: peak RSS                 {rss:>8.1} MiB");
    println!("  {name}: stored, superseded incl. {stored:>8.1} MiB");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a bench: run with --ignored, in release, as root"]
async fn bench_fuse() {
    let w = bench_world().await;
    w.create("1G").await;
    let tarball = source_tarball(w.dir.path());
    let (dir, mnt) = (w.mnt("fuse"), w.mnt("ext4"));
    let mut c = Cleanup::default();
    let d = w.serve(&["fuse", p(&dir)]).await;
    let dev = losetup(&d.path, false, &mut c);
    bench_device("FUSE + loop", &dev, &mnt, &tarball);
    detach(&dev, &mut c);
    report(&w, "volume", d).await;
}

#[cfg(feature = "ublk")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a bench: run with --ignored, in release, as root"]
async fn bench_ublk() {
    if !have_ublk() {
        return;
    }
    let w = bench_world().await;
    w.create("1G").await;
    let tarball = source_tarball(w.dir.path());
    let mnt = w.mnt("ext4");
    let d = w.serve(&["ublk", "--id", "43"]).await;
    let dev = p(&d.path).to_string();
    bench_device("ublk", &dev, &mnt, &tarball);
    report(&w, "volume", d).await;
}

/// The repository's `packages/fuse`, where zen-mount's CLI and its
/// modules are.
fn fuse_pkg() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/fuse")
}

/// Set fs 1's chunking (as the admin, key 1) through `@zen/client`.
fn set_chunking(w: &World, chunking: &str) {
    let admin = w.dir.path().join("admin.json");
    key(1).save(&admin).unwrap();
    const SCRIPT: &str = r#"
import { readFile } from 'node:fs/promises';
import { bytes, connect, zw } from '@zen/client';
const d = JSON.parse(await readFile(process.env.ZEN_KEY, 'utf8'));
const client = await connect(process.env.ZEN_URL);
const s = await client.signInDevice({
  identity: zw.SigningIdentity.fromSeed(bytes.fromHex(d.identity_seed)),
  device: zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed)),
  cert: bytes.fromHex(d.cert),
});
const fs = await s.fs(1).unlock({ device: zw.DeviceSecret.fromSeed(bytes.fromHex(d.device_seed)) });
await fs.setPolicy({ chunking: process.env.ZEN_CHUNKING });
process.exit(0);
"#;
    let st = Command::new("node")
        .args(["--input-type=module", "-e", SCRIPT])
        .current_dir(fuse_pkg())
        .env("ZEN_KEY", &admin)
        .env("ZEN_URL", &w.srv.url)
        .env("ZEN_CHUNKING", chunking)
        .status()
        .unwrap();
    assert!(st.success(), "setting the policy");
}

/// Set when zen-mount reports a refused queued write.
static REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start zen-mount on a new tree at `dir`, with its replica in the temp
/// dir; wait until it has mounted.
fn zen_mount(w: &World, dir: &Path) -> Daemon {
    let cli = std::env::var("ZEN_MOUNT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| fuse_pkg().join("dist/cli.js"));
    let cache = w.dir.path().join("mount-cache");
    let mut c = Command::new("node");
    c.arg(cli)
        .args([&w.srv.url, p(dir)])
        .arg("--device-key")
        .arg(&w.key_file)
        .args(["--fs", "1", "--new-tree", "--foreground", "--cache-dir"])
        .arg(&cache)
        .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(err.read_line(&mut line).unwrap() > 0, "zen-mount exited");
        if line.starts_with("mounted fs") {
            break;
        }
    }
    // Pass its stderr on (and keep the pipe drained), noting refused writes.
    std::thread::spawn(move || {
        for l in err.lines().map_while(Result::ok) {
            if l.contains("queued writes refused") {
                REFUSED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            eprintln!("[zen-mount] {l}");
        }
    });
    Daemon {
        child: Some(child),
        path: dir.join("disk.img"),
    }
}

/// The same workload on an image kept as a zen-fs file: zen-mount, a
/// sparse 1 GiB file in it, and a loop device on the file.
async fn bench_file(chunking: &str) {
    let w = bench_world().await;
    set_chunking(&w, chunking);
    let tarball = source_tarball(w.dir.path());
    let (dir, mnt) = (w.mnt("zen-mount"), w.mnt("ext4"));
    // Dropped after the daemon: a dead zen-mount's mount is taken away.
    let mut gone = Cleanup::default();
    gone.mounts.push(dir.clone());
    let d = zen_mount(&w, &dir);
    ok("truncate", &["-s", "1G", p(&d.path)]);
    let mut c = Cleanup::default();
    let dev = losetup(&d.path, false, &mut c);
    bench_device(
        &format!("zen-mount + loop ({chunking})"),
        &dev,
        &mnt,
        &tarball,
    );
    detach(&dev, &mut c);
    report(&w, "file", d).await;
    gone.mounts.clear();
    assert!(
        !REFUSED.load(std::sync::atomic::Ordering::SeqCst),
        "zen-mount's queued writes were refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a bench: run with --ignored, in release, as root (and npm run build)"]
async fn bench_file_cdc() {
    bench_file("cdc").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a bench: run with --ignored, in release, as root (and npm run build)"]
async fn bench_file_fixed() {
    bench_file("fixed").await;
}

/// `import` and `export` of a 1 GiB ext4 image holding the source tree and
/// 256 MiB of random data (made with `mkfs.ext4 -d`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a bench: run with --ignored, in release, as root"]
async fn bench_image() {
    let w = bench_world().await;
    let tarball = source_tarball(w.dir.path());
    let src = w.mnt("src");
    ok("tar", &["-xf", p(&tarball.0), "-C", p(&src)]);
    ok(
        "dd",
        &[
            "if=/dev/urandom",
            &format!("of={}", p(&src.join("random"))),
            "bs=1M",
            "count=256",
            "status=none",
        ],
    );
    let img = w.dir.path().join("in.img");
    ok(
        "mkfs.ext4",
        &["-q", "-F", "-b", "4096", "-d", p(&src), p(&img), "1G"],
    );
    let alloc = |f: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(f).unwrap().blocks() as f64 * 512.0 / (1 << 20) as f64
    };
    let in_mib = alloc(&img);
    let t = Instant::now();
    let o = w.run(&["import", "--create", p(&img)]).await;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let import_s = t.elapsed().as_secs_f64();
    let out = w.dir.path().join("out.img");
    let t = Instant::now();
    let o = w.run(&["export", p(&out)]).await;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let export_s = t.elapsed().as_secs_f64();
    assert!(std::fs::read(&img).unwrap() == std::fs::read(&out).unwrap());
    println!("\nimport and export of a 1 GiB ext4 image ({in_mib:.0} MiB allocated)");
    println!(
        "  import --create                  {import_s:>8.2} s, {:>6.1} MiB/s of data",
        in_mib / import_s
    );
    println!(
        "  export                           {export_s:>8.2} s, {:>6.1} MiB/s of data",
        in_mib / export_s
    );
    println!(
        "  exported, allocated              {:>8.1} MiB",
        alloc(&out)
    );
    println!(
        "  stored                           {:>8.1} MiB",
        stored_mib(&w).await
    );
}
