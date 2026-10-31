//! `zen-ublk`: serve a zen-blk volume as a block device (docs/BLK.md):
//! one FUSE file for `losetup -b 4096`, or `/dev/ublkbN` (feature `ublk`).
//!
//! It signs in with the device key, attaches integrity with the pins file,
//! unlocks the fs and syncs, then opens the volume (its writer holds the
//! lease; `--read-only` needs none). Once serving it prints `ready <path>`.
//! SIGINT and SIGTERM, or unmounting the FUSE directory, stop it: the
//! volume is flushed, checkpointed and its lease released. Exit codes: 0 a
//! clean stop, 3 an integrity alarm (as JSON on stderr), 1 anything else
//! (`busy`: another writer holds the volume; a lost lease).

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use zen_blk::{Volume, VolumeOptions};
use zen_client::cli::{ConnectArgs, Connected, exit_code, print_error};
use zen_client::{Error, IntegrityOptions, UnlockedFs, hex};
use zen_ublk::disk::Disk;
use zen_ublk::{fuse, parse_size, vol_pin_path};

#[derive(Parser)]
#[command(
    name = "zen-ublk",
    version,
    about = "A zen-blk volume as a block device"
)]
struct Cli {
    #[command(flatten)]
    connect: ConnectArgs,
    /// The volume's name (UTF-8, 1–64 bytes).
    #[arg(long)]
    vol: String,
    /// The volume's local root pin (default: the device key file's path +
    /// `.vol-<fs>-<hex(name)>`).
    #[arg(long)]
    vol_pin: Option<PathBuf>,
    /// Open read-only: no lease, writes refused.
    #[arg(long)]
    read_only: bool,
    /// The writer's lease, renewed every third of it.
    #[arg(long, default_value_t = 30_000)]
    lease_ttl_ms: u64,
    /// Blocks per commit, and the buffered blocks that make a write flush.
    #[arg(long, default_value_t = 256)]
    batch_blocks: usize,
    /// Flush this long after the last write (0: only when asked).
    #[arg(long, default_value_t = 5_000)]
    idle_flush_ms: u64,
    /// How long a request retries a server it can't reach before EIO.
    #[arg(long, default_value_t = 60_000)]
    io_timeout_ms: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the volume (SIZE: bytes, or with K/M/G/T; a multiple of
    /// 4,096), print its root, and exit.
    Create {
        #[arg(value_parser = parse_size)]
        size: u64,
    },
    /// Print the volume's size and root (seq, RH) as JSON.
    Info,
    /// Serve DIR/disk.img until DIR is unmounted or the daemon is signalled.
    Fuse { dir: PathBuf },
    /// Serve /dev/ublkbN until signalled.
    #[cfg(feature = "ublk")]
    Ublk {
        /// The device id (-1: the kernel picks). A device of this id left
        /// by a killed daemon is deleted first.
        #[arg(long, default_value_t = -1, allow_negative_numbers = true)]
        id: i32,
        /// Hardware queues.
        #[arg(long, default_value_t = 2)]
        queues: u16,
        /// Requests in flight per queue.
        #[arg(long, default_value_t = 64)]
        depth: u16,
    },
}

fn options(cli: &Cli, name: &[u8]) -> VolumeOptions {
    VolumeOptions {
        lease_ttl: Duration::from_millis(cli.lease_ttl_ms),
        batch_blocks: cli.batch_blocks.max(1),
        idle_flush: (cli.idle_flush_ms > 0).then(|| Duration::from_millis(cli.idle_flush_ms)),
        pin: Some(vol_pin_path(&cli.connect, name, cli.vol_pin.clone())),
        ..Default::default()
    }
}

async fn open(cli: &Cli, fs: &UnlockedFs, name: &[u8]) -> Result<Volume, Error> {
    let o = options(cli, name);
    if cli.read_only {
        Volume::open_read_only(fs, name, o).await
    } else {
        Volume::open(fs, name, o).await
    }
}

fn root_json(v: &Volume) -> serde_json::Value {
    let (seq, rh) = v.root();
    serde_json::json!({"size": v.size(), "seq": seq, "root": hex(&rh)})
}

/// Wait for SIGINT or SIGTERM.
async fn signalled() -> std::io::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    Ok(())
}

/// Unmount a FUSE directory: `umount2` as root, else `fusermount3 -u`.
fn unmount(dir: &Path) -> std::io::Result<()> {
    // SAFETY: geteuid can't fail and touches no memory.
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
        if unsafe { libc::umount2(c.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }
    let st = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(dir)
        .status()?;
    if st.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("fusermount3 -u: {st}")))
    }
}

async fn serve_fuse(disk: &Disk, dir: &Path) -> Result<(), Error> {
    let export = fuse::Export::new(disk.clone(), tokio::runtime::Handle::current());
    let session = fuse::mount(export, dir, disk.read_only())
        .map_err(|e| Error::Io(format!("mounting {}: {e}", dir.display())))?;
    println!("ready {}", dir.join(fuse::FILE).display());
    let mut stop = Box::pin(signalled());
    let mut unmounted = false;
    loop {
        tokio::select! {
            r = &mut stop, if !unmounted => {
                r?;
                match unmount(dir) {
                    Ok(()) => unmounted = true,
                    Err(e) => {
                        eprintln!("zen-ublk: can't unmount {} ({e}); still serving", dir.display());
                        stop = Box::pin(signalled());
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        if session.guard.is_finished() {
            break;
        }
    }
    let r = tokio::task::spawn_blocking(move || session.join()).await;
    if let Ok(Err(e)) = r {
        eprintln!("zen-ublk: the FUSE session ended with {e}");
    }
    Ok(())
}

#[cfg(feature = "ublk")]
async fn serve_ublk(disk: &Disk, o: zen_ublk::ublk::Options) -> Result<(), Error> {
    use std::sync::Arc;
    use zen_ublk::ublk;
    if o.id >= 0 {
        tokio::task::spawn_blocking(move || ublk::clear_stale(o.id))
            .await
            .map_err(|e| Error::Io(e.to_string()))??;
    }
    let ctrl = Arc::new(
        tokio::task::spawn_blocking(move || ublk::control(&o))
            .await
            .map_err(|e| Error::Io(e.to_string()))??,
    );
    let (c, d, rt) = (
        ctrl.clone(),
        disk.clone(),
        tokio::runtime::Handle::current(),
    );
    let mut served = tokio::task::spawn_blocking(move || {
        ublk::serve(&c, d, rt, |path| println!("ready {path}"))
    });
    let r = tokio::select! {
        r = &mut served => r,
        s = signalled() => {
            s?;
            let c = ctrl.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || ublk::stop(&c)).await {
                eprintln!("zen-ublk: {e}");
            }
            served.await
        }
    };
    let c = ctrl.clone();
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || ublk::delete(&c)).await {
        eprintln!("zen-ublk: {e}");
    }
    r.map_err(|e| Error::Io(e.to_string()))?
}

/// The exit code (a served disk's failure was printed when it happened).
async fn run(cli: Cli) -> Result<u8, Error> {
    let Connected {
        fs: u, integrity, ..
    } = cli.connect.connect(IntegrityOptions::default()).await?;
    let name = cli.vol.as_bytes().to_vec();
    let disk = match &cli.command {
        Command::Create { size } => {
            let v = Volume::create(&u, &name, *size, options(&cli, &name)).await?;
            println!("{}", root_json(&v));
            v.close().await?;
            integrity.close().await?;
            return Ok(0);
        }
        Command::Info => {
            let v = Volume::open_read_only(&u, &name, options(&cli, &name)).await?;
            println!("{}", root_json(&v));
            v.close().await?;
            integrity.close().await?;
            return Ok(0);
        }
        _ => Disk::new(
            open(&cli, &u, &name).await?,
            Duration::from_millis(cli.io_timeout_ms),
        ),
    };
    let served = match &cli.command {
        Command::Fuse { dir } => serve_fuse(&disk, dir).await,
        #[cfg(feature = "ublk")]
        Command::Ublk { id, queues, depth } => {
            serve_ublk(
                &disk,
                zen_ublk::ublk::Options {
                    id: *id,
                    queues: *queues,
                    depth: *depth,
                },
            )
            .await
        }
        Command::Create { .. } | Command::Info => unreachable!(),
    };
    let closed = disk.close().await;
    let ck = integrity.close().await;
    served?;
    if let Err(f) = closed {
        return Ok(f.code);
    }
    ck?;
    Ok(0)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            print_error("zen-ublk", &e);
            ExitCode::from(exit_code(&e))
        }
    }
}
