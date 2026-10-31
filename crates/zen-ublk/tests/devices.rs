//! zen-ublk as a real block device (feature `device-tests`; needs root,
//! FUSE, loop devices, e2fsprogs; the ublk tests also feature `ublk` and
//! the ublk driver). The server runs in-process; `zen-ublk` runs as a child
//! process, so a writer can be killed for real.
//!
//! The ublk tests skip, saying so, only when the kernel has no
//! `/dev/ublk-control`; with `ZEN_TEST_REQUIRE_UBLK=1` (CI) they fail.

#[path = "../../zen-client/tests/common/mod.rs"]
mod common;

use common::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_zen-ublk");

/// A server claimed by key 1, fs 1 with slots for key 1 and the writer,
/// and the writer's device key file.
struct World {
    srv: Server,
    dir: tempfile::TempDir,
    key_file: PathBuf,
}

async fn world() -> World {
    let srv = Server::start().await;
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
    World { srv, dir, key_file }
}

impl World {
    fn cmd(&self, extra: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(["--url", &self.srv.url, "--fs", "1", "--vol", "disk"])
            .arg("--device-key")
            .arg(&self.key_file)
            .args(["--lease-ttl-ms", "2000", "--idle-flush-ms", "1000"])
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
    // The lease lasts 2 s: a restart at once is refused.
    let o = w.run(&["fuse", p(&dir)]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("busy"));
    tokio::time::sleep(Duration::from_millis(2500)).await;
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
    // The lease (2 s) lapses.
    tokio::time::sleep(Duration::from_millis(2500)).await;
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
