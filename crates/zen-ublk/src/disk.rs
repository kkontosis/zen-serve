//! The adapter both frontends call: block requests on a [`Volume`], errors
//! as errnos, transient failures retried, and fail-closed after an alarm or
//! a lost lease (docs/BLK.md).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zen_blk::Volume;
use zen_client::Error;

/// Why the disk stopped serving: every request fails with `EIO` from then on.
#[derive(Debug, Clone)]
pub struct Failure {
    /// The exit code it gives the daemon: 3 for an alarm, else 1.
    pub code: u8,
    /// What happened (an alarm as JSON).
    pub message: String,
}

struct Inner {
    vol: Volume,
    io_timeout: Duration,
    failed: Mutex<Option<Failure>>,
}

/// A volume served as a disk (cheap to clone: clones share it).
#[derive(Clone)]
pub struct Disk(Arc<Inner>);

/// Whether an error may pass: no answer, or a server that is unavailable
/// for now (the transport already retried 503 and 429 a few times).
fn transient(e: &Error) -> bool {
    e.is("network") || e.status() >= 500
}

impl Disk {
    /// Serve `vol`; transient errors are retried for up to `io_timeout`.
    pub fn new(vol: Volume, io_timeout: Duration) -> Disk {
        Disk(Arc::new(Inner {
            vol,
            io_timeout,
            failed: Mutex::new(None),
        }))
    }

    /// The volume.
    pub fn volume(&self) -> &Volume {
        &self.0.vol
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.0.vol.size()
    }

    /// Whether writes are refused (`EROFS`).
    pub fn read_only(&self) -> bool {
        !self.0.vol.is_writer()
    }

    /// Why the disk stopped serving, if it did.
    pub fn failure(&self) -> Option<Failure> {
        self.0.failed.lock().expect("disk state").clone()
    }

    /// The errno of `e`, failing the disk for good on an alarm or a lost lease.
    fn errno(&self, e: Error) -> i32 {
        let fatal = match &e {
            Error::Integrity(a) => Some(Failure {
                code: 3,
                message: a.to_json().to_string(),
            }),
            e if e.is("lease_lost") => Some(Failure {
                code: 1,
                message: format!("zen-ublk: {e}"),
            }),
            _ => None,
        };
        if let Some(f) = fatal {
            let mut failed = self.0.failed.lock().expect("disk state");
            if failed.is_none() {
                eprintln!("{}", f.message);
                eprintln!("zen-ublk: every request fails with EIO from now on");
                *failed = Some(f);
            }
            return libc::EIO;
        }
        match e.code() {
            "range" => libc::EINVAL,
            "read_only" => libc::EROFS,
            "volume_changed" => {
                eprintln!(
                    "zen-ublk: the volume changed under this read-only export; restart it to read the new root"
                );
                libc::EIO
            }
            _ => {
                eprintln!("zen-ublk: {e}");
                libc::EIO
            }
        }
    }

    /// Run `f` until it succeeds, fails for good, or transient failures
    /// outlast the I/O timeout.
    async fn run<T, F, Fut>(&self, mut f: F) -> Result<T, i32>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = zen_client::Result<T>>,
    {
        let start = Instant::now();
        let mut wait = Duration::from_millis(50);
        loop {
            if self.failure().is_some() {
                return Err(libc::EIO);
            }
            match f().await {
                Ok(v) => return Ok(v),
                Err(e) if transient(&e) && start.elapsed() + wait < self.0.io_timeout => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(2));
                }
                Err(e) => return Err(self.errno(e)),
            }
        }
    }

    /// Read `len` bytes at `off`.
    pub async fn read(&self, off: u64, len: usize) -> Result<Vec<u8>, i32> {
        self.run(|| async move {
            let mut b = vec![0u8; len];
            self.0.vol.read(off, &mut b).await.map(|_| b)
        })
        .await
    }

    /// Write `data` at `off`; with `fua`, durable before it returns.
    pub async fn write(&self, off: u64, data: &[u8], fua: bool) -> Result<(), i32> {
        self.run(|| self.0.vol.write(off, data)).await?;
        if fua { self.flush().await } else { Ok(()) }
    }

    /// Make every write before it durable.
    pub async fn flush(&self) -> Result<(), i32> {
        if self.read_only() {
            return Ok(());
        }
        self.run(|| self.0.vol.flush()).await
    }

    /// Discard `[off, off + len)` (also write-zeroes: it reads as zeros).
    pub async fn discard(&self, off: u64, len: u64) -> Result<(), i32> {
        self.run(|| self.0.vol.discard(off, len)).await
    }

    /// Flush, checkpoint and release the lease; a failed disk gives the
    /// failure it stopped with (printed when it happened, as a close's error is).
    pub async fn close(&self) -> Result<(), Failure> {
        if let Some(f) = self.failure() {
            return Err(f);
        }
        self.0.vol.clone().close().await.map_err(|e| {
            zen_client::cli::print_error("zen-ublk", &e);
            Failure {
                code: zen_client::cli::exit_code(&e),
                message: e.to_string(),
            }
        })
    }
}
