//! The ublk frontend: `/dev/ublkbN` (feature `ublk`; docs/BLK.md).
//!
//! Each queue thread runs libublk's smol executor with one task per tag. A
//! request's [`Disk`] future runs on the tokio runtime. A queue thread sleeps
//! in its ring until a completion arrives, so a tag waits for the result
//! through the ring: an io_uring read of the tag's eventfd, which the tokio
//! task writes once the result is ready ([`on_ring`]). The device reports a
//! volatile write cache and FUA, so the kernel sends FLUSH and FUA writes:
//! without it, ext4 would treat buffered writes as durable.

use crate::disk::Disk;
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, sys};
use std::rc::Rc;
use tokio::runtime::Handle;

/// A tag's eventfd: the tokio side writes it, the ring reads it.
struct EventFd(i32);

impl EventFd {
    fn new() -> std::io::Result<EventFd> {
        // SAFETY: eventfd takes no pointers; the fd is owned by the result.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(EventFd(fd))
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        // SAFETY: the fd is ours and closed once.
        unsafe { libc::close(self.0) };
    }
}

/// Writes the eventfd when dropped: after the result is sent, or when the
/// task panics, so the ring's read always completes.
struct Ring(i32);

impl Drop for Ring {
    fn drop(&mut self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to an fd that outlives
        // this (the tag waits for this write before closing it).
        unsafe { libc::write(self.0, one.as_ptr().cast(), 8) };
    }
}

/// Run `f` on tokio, and wait for it through the queue's ring.
async fn on_ring<T, F>(q: &UblkQueue<'_>, efd: &EventFd, rt: &Handle, f: F) -> Option<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let ring = Ring(efd.0);
    rt.spawn(async move {
        let _ring = ring;
        let _ = tx.send(f.await);
    });
    let mut b = [0u8; 8];
    loop {
        let sqe =
            io_uring::opcode::Read::new(io_uring::types::Fd(efd.0), b.as_mut_ptr(), 8).build();
        q.ublk_submit_sqe(sqe).await;
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => return None,
            // Not written yet (an interrupted read): read again.
            Err(_) => {}
        }
    }
}

/// Give this thread a control ring if it has none. libublk keeps the
/// control io_uring per thread, made by the thread that builds a
/// `UblkCtrl`; every other thread that sends a control command (start,
/// stop, delete) needs its own.
fn ensure_ring() -> zen_client::Result<()> {
    libublk::ctrl::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            let r = io_uring::IoUring::<io_uring::squeue::Entry128>::builder()
                .build(16)
                .map_err(UblkError::IOError)?;
            *ring = Some(r);
        }
        Ok(())
    })
    .map_err(|e| ublk_err("the control ring", e))
}

/// Stop the device (its queues end, so [`serve`] returns).
pub fn stop(ctrl: &UblkCtrl) -> zen_client::Result<()> {
    ensure_ring()?;
    ctrl.kill_dev()
        .map_err(|e| ublk_err("stopping the device", e))?;
    Ok(())
}

/// Delete the device.
pub fn delete(ctrl: &UblkCtrl) -> zen_client::Result<()> {
    ensure_ring()?;
    ctrl.del_dev()
        .map_err(|e| ublk_err("deleting the device", e))?;
    Ok(())
}

/// The device's shape.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The device id (`/dev/ublkbN`); -1: the kernel picks one.
    pub id: i32,
    /// Hardware queues (one thread each).
    pub queues: u16,
    /// Requests in flight per queue.
    pub depth: u16,
}

/// The largest request: 1 MiB.
const IO_BUF: u32 = 1 << 20;

fn ublk_err(what: &str, e: UblkError) -> zen_client::Error {
    zen_client::Error::Io(format!("ublk: {what}: {e}"))
}

/// Delete device `id` when a killed daemon left it (its daemon pid is
/// gone), so a restart can add it again; a live one is `busy`.
pub fn clear_stale(id: i32) -> zen_client::Result<()> {
    // `new_simple` reads the device's info from the driver, so it fails
    // when there is no such device: look for its char device first.
    if !std::path::Path::new(&format!("/dev/ublkc{id}")).exists() {
        return Ok(());
    }
    ensure_ring()?;
    let ctrl = UblkCtrl::new_simple(id).map_err(|e| ublk_err("the control device", e))?;
    let pid = ctrl.dev_info().ublksrv_pid;
    // SAFETY: kill with signal 0 only checks that the process exists.
    if pid > 0 && unsafe { libc::kill(pid, 0) } == 0 {
        return Err(zen_client::Error::local(
            "busy",
            format!("/dev/ublkb{id} is served by process {pid}"),
        ));
    }
    eprintln!("zen-ublk: deleting /dev/ublkb{id}, left by a daemon that is gone");
    ctrl.del_dev()
        .map_err(|e| ublk_err("deleting a stale device", e))?;
    Ok(())
}

/// The control device for a new device of `o`.
pub fn control(o: &Options) -> zen_client::Result<UblkCtrl> {
    UblkCtrlBuilder::default()
        .name("zen-ublk")
        .id(o.id)
        .nr_queues(o.queues)
        .depth(o.depth)
        .io_buf_bytes(IO_BUF)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .map_err(|e| ublk_err("adding the device", e))
}

/// The device's parameters for `disk` (4,096-byte blocks, discard, a
/// volatile cache with FUA; read-only when the disk is).
fn init(dev: &mut UblkDev, disk: &Disk) -> Result<(), UblkError> {
    let size = disk.size();
    let mut attrs = sys::UBLK_ATTR_VOLATILE_CACHE | sys::UBLK_ATTR_FUA;
    if disk.read_only() {
        attrs |= sys::UBLK_ATTR_READ_ONLY;
    }
    dev.tgt.dev_size = size;
    dev.tgt.params = sys::ublk_params {
        types: sys::UBLK_PARAM_TYPE_BASIC | sys::UBLK_PARAM_TYPE_DISCARD,
        basic: sys::ublk_param_basic {
            attrs,
            logical_bs_shift: 12,
            physical_bs_shift: 12,
            io_opt_shift: 12,
            io_min_shift: 12,
            max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
            dev_sectors: size >> 9,
            ..Default::default()
        },
        discard: sys::ublk_param_discard {
            discard_granularity: zen_blk::BLOCK_SIZE as u32,
            max_discard_sectors: 1 << 21,
            max_write_zeroes_sectors: 1 << 21,
            max_discard_segments: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    Ok(())
}

/// One request: the byte count, or `-errno`.
async fn handle(
    q: &UblkQueue<'_>,
    tag: u16,
    buf: &mut [u8],
    efd: &EventFd,
    disk: &Disk,
    rt: &Handle,
) -> i32 {
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    let fua = iod.op_flags & sys::UBLK_IO_F_FUA != 0;
    let off = iod.start_sector << 9;
    let bytes = (iod.nr_sectors as usize) << 9;
    let data_op = op == sys::UBLK_IO_OP_READ || op == sys::UBLK_IO_OP_WRITE;
    if data_op && bytes > buf.len() {
        return -libc::EINVAL;
    }
    let d = disk.clone();
    let done = match op {
        sys::UBLK_IO_OP_READ => {
            on_ring(
                q,
                efd,
                rt,
                async move { d.read(off, bytes).await.map(Some) },
            )
            .await
        }
        sys::UBLK_IO_OP_WRITE => {
            let data = buf[..bytes].to_vec();
            on_ring(q, efd, rt, async move {
                d.write(off, &data, fua).await.map(|_| None)
            })
            .await
        }
        sys::UBLK_IO_OP_FLUSH => {
            on_ring(q, efd, rt, async move { d.flush().await.map(|_| None) }).await
        }
        sys::UBLK_IO_OP_DISCARD | sys::UBLK_IO_OP_WRITE_ZEROES => {
            on_ring(q, efd, rt, async move {
                d.discard(off, bytes as u64).await.map(|_| None)
            })
            .await
        }
        _ => return -libc::EOPNOTSUPP,
    };
    match done {
        Some(Ok(Some(b))) => {
            buf[..bytes].copy_from_slice(&b);
            bytes as i32
        }
        Some(Ok(None)) => bytes as i32,
        Some(Err(errno)) => -errno,
        None => -libc::EIO,
    }
}

async fn io_task(q: &UblkQueue<'_>, tag: u16, disk: Disk, rt: Handle) -> Result<(), UblkError> {
    let efd = EventFd::new()
        .map_err(|e| UblkError::OtherError(-e.raw_os_error().unwrap_or(libc::EIO)))?;
    let mut buf = IoBuf::<u8>::new(q.dev.dev_info.max_io_buf_bytes as usize);
    q.submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(&buf))
        .await?;
    loop {
        let res = handle(q, tag, buf.as_mut_slice(), &efd, &disk, &rt).await;
        q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), res)
            .await?;
    }
}

fn queue(qid: u16, dev: &UblkDev, disk: Disk, rt: Handle) {
    let q = match UblkQueue::new(qid, dev) {
        Ok(q) => Rc::new(q),
        Err(e) => {
            eprintln!("zen-ublk: queue {qid}: {e}");
            return;
        }
    };
    let exe_rc = Rc::new(smol::LocalExecutor::new());
    let exe = exe_rc.clone();
    let mut tasks = Vec::new();
    for tag in q.tags() {
        let (q, disk, rt) = (q.clone(), disk.clone(), rt.clone());
        tasks.push(exe.spawn(async move {
            match io_task(&q, tag, disk, rt).await {
                Ok(()) | Err(UblkError::QueueIsDown) => {}
                Err(e) => eprintln!("zen-ublk: queue {qid} tag {tag}: {e}"),
            }
        }));
    }
    smol::block_on(exe_rc.run(async move {
        let run_ops = || while exe.try_tick() {};
        let done = || tasks.iter().all(|t| t.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&q, Some(20), run_ops, done).await {
            eprintln!("zen-ublk: queue {qid}: {e}");
        }
    }));
}

/// Serve `disk` on the device of `ctrl` until it is stopped
/// ([`UblkCtrl::kill_dev`]); `ready` gets the block device's path once it
/// is live. Blocks: run it on a thread of its own.
pub fn serve(
    ctrl: &UblkCtrl,
    disk: Disk,
    rt: Handle,
    ready: impl FnOnce(String) + Send + Sync + 'static,
) -> zen_client::Result<()> {
    ensure_ring()?;
    let d = disk.clone();
    ctrl.run_target(
        |dev: &mut UblkDev| init(dev, &d),
        move |qid, dev: &UblkDev| queue(qid, dev, disk, rt),
        move |c: &UblkCtrl| ready(c.get_bdev_path()),
    )
    .map_err(|e| ublk_err("serving", e))?;
    Ok(())
}
