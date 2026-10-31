//! The FUSE frontend: a directory with one file, `disk.img`, the volume, for
//! `losetup -b 4096` (docs/BLK.md).
//!
//! Each request runs as a task on the tokio runtime, which replies from the
//! task. Writes are write-through (no writeback cache): a write the kernel
//! saw answered is in the volume's buffer, so an `fsync`, which the kernel
//! sends after the writes it covers, commits it. Loop devices pass FLUSH on
//! as `fsync`, and DISCARD and WRITE_ZEROES as `fallocate`.

use crate::disk::Disk;
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, SessionACL, TimeOrNow,
    WriteFlags,
};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;

/// The file's name in the mounted directory.
pub const FILE: &str = "disk.img";
const DISK: INodeNo = INodeNo(2);
const TTL: Duration = Duration::from_secs(1);

/// The filesystem: the root directory and the disk file.
pub struct Export {
    disk: Disk,
    rt: Handle,
    uid: u32,
    gid: u32,
    born: SystemTime,
}

fn errno(e: i32) -> Errno {
    Errno::from_i32(e)
}

impl Export {
    /// Serve `disk` on the runtime `rt`.
    pub fn new(disk: Disk, rt: Handle) -> Export {
        // SAFETY: getuid and getgid can't fail and touch no memory.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Export {
            disk,
            rt,
            uid,
            gid,
            born: SystemTime::now(),
        }
    }

    fn attr(&self, ino: INodeNo) -> Option<FileAttr> {
        let (kind, perm, size, nlink) = match ino {
            INodeNo::ROOT => (FileType::Directory, 0o755, 0, 2),
            DISK => (
                FileType::RegularFile,
                if self.disk.read_only() { 0o400 } else { 0o600 },
                self.disk.size(),
                1,
            ),
            _ => return None,
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size / 512,
            atime: self.born,
            mtime: self.born,
            ctime: self.born,
            crtime: self.born,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: zen_blk::BLOCK_SIZE as u32,
            flags: 0,
        })
    }
}

impl Filesystem for Export {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match (parent, name.to_str()) {
            (INodeNo::ROOT, Some(FILE)) => {
                reply.entry(&TTL, &self.attr(DISK).expect("the disk"), Generation(0))
            }
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(a) = self.attr(ino) else {
            return reply.error(Errno::ENOENT);
        };
        // The size is the volume's: no truncating or growing it. Times and
        // modes are accepted and ignored.
        match size {
            Some(s) if s != a.size => reply.error(Errno::EPERM),
            _ => reply.attr(&TTL, &a),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if ino != DISK {
            return reply.error(Errno::EISDIR);
        }
        if self.disk.read_only() && flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if ino != DISK {
            return reply.error(Errno::EISDIR);
        }
        // A short read at the end, nothing past it.
        let len = (size as u64).min(self.disk.size().saturating_sub(offset)) as usize;
        if len == 0 {
            return reply.data(&[]);
        }
        let disk = self.disk.clone();
        self.rt.spawn(async move {
            match disk.read(offset, len).await {
                Ok(b) => reply.data(&b),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if ino != DISK {
            return reply.error(Errno::EISDIR);
        }
        let disk = self.disk.clone();
        let data = data.to_vec();
        self.rt.spawn(async move {
            match disk.write(offset, &data, false).await {
                Ok(()) => reply.written(data.len() as u32),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.sync(reply);
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Never ENOSYS: the kernel would take it as "nothing to sync".
        self.sync(reply);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino != INodeNo::ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        let entries = [
            (INodeNo::ROOT, FileType::Directory, "."),
            (INodeNo::ROOT, FileType::Directory, ".."),
            (DISK, FileType::RegularFile, FILE),
        ];
        for (k, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (k + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let blocks = self.disk.size() / zen_blk::BLOCK_SIZE as u64;
        reply.statfs(
            blocks,
            0,
            0,
            2,
            0,
            zen_blk::BLOCK_SIZE as u32,
            255,
            zen_blk::BLOCK_SIZE as u32,
        );
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        if ino != DISK {
            return reply.error(Errno::EISDIR);
        }
        let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        let zero = libc::FALLOC_FL_ZERO_RANGE;
        let in_size = offset
            .checked_add(length)
            .is_some_and(|end| end <= self.disk.size());
        match mode {
            // Every block exists: allocating inside the size is a no-op.
            0 if in_size => reply.ok(),
            m if (m == punch || m == zero || m == zero | libc::FALLOC_FL_KEEP_SIZE) && in_size => {
                let disk = self.disk.clone();
                self.rt.spawn(async move {
                    match disk.discard(offset, length).await {
                        Ok(()) => reply.ok(),
                        Err(e) => reply.error(errno(e)),
                    }
                });
            }
            _ => reply.error(Errno::EOPNOTSUPP),
        }
    }
}

impl Export {
    fn sync(&self, reply: ReplyEmpty) {
        let disk = self.disk.clone();
        self.rt.spawn(async move {
            match disk.flush().await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(e)),
            }
        });
    }
}

/// The session's options: as root, other users (a loop device's reads come
/// from the kernel, but `losetup` opens the file as its caller) may open it.
pub fn config(read_only: bool) -> Config {
    let mut c = Config::default();
    c.mount_options = vec![
        MountOption::FSName("zen-ublk".into()),
        MountOption::Subtype("zen-ublk".into()),
        MountOption::DefaultPermissions,
        MountOption::NoExec,
        MountOption::NoSuid,
        MountOption::NoDev,
        if read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    // SAFETY: geteuid can't fail and touches no memory.
    if unsafe { libc::geteuid() } == 0 {
        c.acl = SessionACL::All;
    }
    c.n_threads = Some(4);
    c
}

/// Mount `export` on `dir` (its file is `dir/disk.img`), in the background.
pub fn mount(
    export: Export,
    dir: &Path,
    read_only: bool,
) -> std::io::Result<fuser::BackgroundSession> {
    fuser::spawn_mount(export, dir, &config(read_only))
}
