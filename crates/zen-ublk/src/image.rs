//! Raw images in and out of a volume (docs/BLK.md §2): `import` copies an
//! image into a volume, resumably; `export` writes a volume out as a sparse
//! file.
//!
//! An import writes the image in windows of [`Volume::blocks_per_commit`]
//! blocks, each one exactly one commit, and records after each window how
//! far it got and the root it left (the progress file). So a resume is
//! decided by the volume's `seq`: the recorded root, or one commit past it
//! by this device (a crash after a window's commit, before its record).

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zen_blk::{BLOCK_SIZE, Volume, VolumeOptions};
use zen_client::{Error, Result, UnlockedFs, hex};

const BS: u64 = BLOCK_SIZE as u64;
/// Blocks an export reads at a time.
const EXPORT_WINDOW: u64 = 256;
/// How often an import reports its progress.
const REPORT_EVERY: u64 = 64 << 20;

fn err(code: &'static str, m: impl Into<String>) -> Error {
    Error::local(code, m)
}

fn io(what: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Io(format!("{}: {e}", what.display()))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Io(e.to_string()))?
}

/// What identifies an image between an import's runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageId {
    size: u64,
    mtime_ns: i64,
    dev: u64,
    ino: u64,
}

impl ImageId {
    fn of(f: &File, size: u64) -> std::io::Result<ImageId> {
        let m = f.metadata()?;
        Ok(ImageId {
            size,
            mtime_ns: m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }
}

/// An import's progress file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Progress {
    image: ImageId,
    /// The import created the volume, so it holds nothing past `offset`.
    created: bool,
    /// Every block before it is the image's.
    offset: u64,
    /// The root the import left there.
    seq: u64,
    rh: [u8; 32],
}

impl Progress {
    fn to_json(&self) -> String {
        serde_json::json!({
            "image": {
                "size": self.image.size,
                "mtime_ns": self.image.mtime_ns,
                "dev": self.image.dev,
                "ino": self.image.ino,
            },
            "created": self.created,
            "offset": self.offset,
            "seq": self.seq,
            "rh": hex(&self.rh),
        })
        .to_string()
    }

    fn from_json(s: &str) -> Option<Progress> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        let i = v.get("image")?;
        let rh = zen_client::unhex(v.get("rh")?.as_str()?)?;
        Some(Progress {
            image: ImageId {
                size: i.get("size")?.as_u64()?,
                mtime_ns: i.get("mtime_ns")?.as_i64()?,
                dev: i.get("dev")?.as_u64()?,
                ino: i.get("ino")?.as_u64()?,
            },
            created: v.get("created")?.as_bool()?,
            offset: v.get("offset")?.as_u64()?,
            seq: v.get("seq")?.as_u64()?,
            rh: rh.try_into().ok()?,
        })
    }

    fn load(path: &Path) -> Result<Option<Progress>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Progress::from_json(&s)
                .map(Some)
                .ok_or_else(|| err("format", format!("{}: not a progress file", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(path)(e)),
        }
    }

    async fn save(&self, path: &Path) -> Result<()> {
        let (p, s) = (path.to_path_buf(), self.to_json());
        blocking(move || zen_client::write_private(&p, s.as_bytes())).await
    }
}

/// An import.
#[derive(Debug, Clone)]
pub struct Import {
    /// The image: a regular file or a block device.
    pub image: PathBuf,
    /// Create the volume at the image's size.
    pub create: bool,
    /// The progress file.
    pub progress: PathBuf,
    /// Ignore the progress file and start over.
    pub restart: bool,
    /// Stop (successfully) after this many windows, leaving the progress
    /// file (for tests).
    pub stop_after: Option<u64>,
    /// The volume's options (its idle flush is turned off).
    pub vol: VolumeOptions,
    /// This device's fingerprint (the writer a resumed root must name).
    pub device: [u8; 32],
}

/// The image's size: a non-zero multiple of 4,096.
fn image_size(f: &mut File, path: &Path) -> Result<u64> {
    use std::io::Seek;
    let size = f.seek(std::io::SeekFrom::End(0)).map_err(io(path))?;
    if size == 0 || !size.is_multiple_of(BS) {
        return Err(err(
            "param",
            format!(
                "{}: {size} bytes, not a non-zero multiple of 4,096",
                path.display()
            ),
        ));
    }
    Ok(size)
}

/// The first offset at or after `off` with data (`lseek(SEEK_DATA)`):
/// `None` when there is none, `Some(off)` when the file can't say.
fn seek_data(f: &File, off: u64) -> Option<u64> {
    use std::os::fd::AsRawFd;
    // SAFETY: lseek on a file descriptor `f` owns; it touches no memory.
    let r = unsafe { libc::lseek(f.as_raw_fd(), off as libc::off_t, libc::SEEK_DATA) };
    if r >= 0 {
        return Some(r as u64);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ENXIO) => None,
        _ => Some(off),
    }
}

fn is_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

/// Runs of whole blocks in `buf`: `(first block, blocks, zero)`.
fn runs(buf: &[u8]) -> Vec<(usize, usize, bool)> {
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    for (i, b) in buf.chunks(BLOCK_SIZE).enumerate() {
        let z = is_zero(b);
        match out.last_mut() {
            Some((_, n, lz)) if *lz == z => *n += 1,
            _ => out.push((i, 1, z)),
        }
    }
    out
}

/// Import `o.image` into volume `name`, and return the volume (open, its
/// writes committed; the caller closes it).
pub async fn import(fs: &UnlockedFs, name: &[u8], o: Import) -> Result<Volume> {
    let mut f = File::open(&o.image).map_err(io(&o.image))?;
    let size = image_size(&mut f, &o.image)?;
    let id = ImageId::of(&f, size).map_err(io(&o.image))?;
    let f = Arc::new(f);
    let previous = if o.restart {
        None
    } else {
        Progress::load(&o.progress)?
    };
    if let Some(p) = &previous
        && p.image != id
    {
        return Err(err(
            "image_changed",
            format!(
                "the image changed since the import stopped ({}); --restart to start over",
                o.progress.display()
            ),
        ));
    }
    let vo = VolumeOptions {
        idle_flush: None,
        ..o.vol.clone()
    };
    let (v, created) = match &previous {
        Some(p) => (Volume::open(fs, name, vo).await?, p.created),
        None if o.create => match Volume::create(fs, name, size, vo.clone()).await {
            Ok(v) => (v, true),
            // Started over: the volume this import created is an existing one now.
            Err(e) if e.is("exists") && o.restart => (Volume::open(fs, name, vo).await?, false),
            Err(e) => return Err(e),
        },
        None => (Volume::open(fs, name, vo).await?, false),
    };
    if v.size() != size {
        let vs = v.size();
        v.close().await?;
        return Err(err(
            "param",
            format!("the image is {size} bytes and the volume {vs}; sizes must match"),
        ));
    }
    let mut offset = 0;
    if let Some(p) = &previous {
        let (seq, rh) = v.root();
        let rec = v.root_record().await;
        let ours = (seq == p.seq && rh == p.rh)
            || (seq == p.seq + 1 && rec.prev == p.rh && rec.writer == o.device);
        if !ours {
            v.close().await?;
            return Err(err(
                "volume_changed",
                format!(
                    "the volume changed since the import stopped (seq {}, now {seq}); --restart to start over",
                    p.seq
                ),
            ));
        }
        offset = p.offset;
        eprintln!("zen-ublk: resuming at offset {offset}, seq {seq}");
    }
    let per = v.blocks_per_commit() as u64;
    let record = |offset: u64, v: &Volume| {
        let (seq, rh) = v.root();
        Progress {
            image: id.clone(),
            created,
            offset,
            seq,
            rh,
        }
    };
    record(offset, &v).save(&o.progress).await?;
    let mut windows = 0u64;
    let mut reported = offset / REPORT_EVERY;
    while offset < size {
        let end = (offset + per * BS).min(size);
        let len = end - offset;
        let data = {
            let f = f.clone();
            let path = o.image.clone();
            blocking(move || {
                if seek_data(&f, offset).is_none_or(|d| d >= end) {
                    return Ok(None);
                }
                let mut buf = vec![0u8; len as usize];
                f.read_exact_at(&mut buf, offset).map_err(io(&path))?;
                Ok(Some(buf))
            })
            .await?
        };
        match data {
            // A hole: nothing there yet on a volume this import created.
            None if created => {}
            None => v.discard(offset, len).await?,
            Some(buf) => {
                for (first, n, zero) in runs(&buf) {
                    let at = offset + first as u64 * BS;
                    let bytes = &buf[first * BLOCK_SIZE..(first + n) * BLOCK_SIZE];
                    match zero {
                        true if created => {}
                        true => v.discard(at, bytes.len() as u64).await?,
                        false => v.write(at, bytes).await?,
                    }
                }
            }
        }
        v.flush().await?;
        offset = end;
        record(offset, &v).save(&o.progress).await?;
        windows += 1;
        if offset / REPORT_EVERY > reported {
            reported = offset / REPORT_EVERY;
            eprintln!("zen-ublk: imported {} of {} MiB", offset >> 20, size >> 20);
        }
        if o.stop_after == Some(windows) && offset < size {
            return Ok(v);
        }
    }
    match std::fs::remove_file(&o.progress) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(&o.progress)(e)),
    }
    Ok(v)
}

/// An export.
#[derive(Debug, Clone)]
pub struct Export {
    /// The file to write.
    pub image: PathBuf,
    /// Replace an existing file.
    pub force: bool,
    /// After this many windows, wait until the file exists (for tests).
    pub pause: Option<(u64, PathBuf)>,
}

/// `IMAGE.part`.
fn part_of(image: &Path) -> PathBuf {
    let mut s = image.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Write volume `v` to `o.image` as a sparse file: blocks that are absent
/// or all zeros stay holes. Through `IMAGE.part`, renamed at the end.
pub async fn export(v: &Volume, o: &Export) -> Result<()> {
    match std::fs::symlink_metadata(&o.image) {
        Ok(m) if !m.is_file() => {
            return Err(err(
                "param",
                format!("{}: not a regular file", o.image.display()),
            ));
        }
        Ok(_) if !o.force => {
            return Err(err(
                "exists",
                format!("{}: exists; --force to replace it", o.image.display()),
            ));
        }
        _ => {}
    }
    let part = part_of(&o.image);
    let r = write_part(v, o, &part).await;
    if let Err(e) = r {
        let _ = std::fs::remove_file(&part);
        if e.is("volume_changed") {
            return Err(err(
                "volume_changed",
                "the volume changed during the export; stop its writer (or fsfreeze its filesystem) and export again",
            ));
        }
        return Err(e);
    }
    let (part2, image) = (part.clone(), o.image.clone());
    blocking(move || {
        std::fs::rename(&part2, &image).map_err(io(&image))?;
        let dir = image
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(dir).and_then(|d| d.sync_all()).map_err(io(dir))
    })
    .await
}

async fn write_part(v: &Volume, o: &Export, part: &Path) -> Result<()> {
    let f = File::create(part).map_err(io(part))?;
    let size = v.size();
    f.set_len(size).map_err(io(part))?;
    let f = Arc::new(f);
    let mut offset = 0;
    let mut windows = 0u64;
    while offset < size {
        let len = (EXPORT_WINDOW * BS).min(size - offset);
        let mut buf = vec![0u8; len as usize];
        v.read(offset, &mut buf).await?;
        let (f2, p) = (f.clone(), part.to_path_buf());
        blocking(move || {
            for (first, n, zero) in runs(&buf) {
                if !zero {
                    let bytes = &buf[first * BLOCK_SIZE..(first + n) * BLOCK_SIZE];
                    f2.write_all_at(bytes, offset + first as u64 * BS)
                        .map_err(io(&p))?;
                }
            }
            Ok(())
        })
        .await?;
        offset += len;
        windows += 1;
        if let Some((n, file)) = &o.pause
            && *n == windows
        {
            while !file.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let p = part.to_path_buf();
    blocking(move || f.sync_all().map_err(io(&p))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_round_trip() {
        let p = Progress {
            image: ImageId {
                size: 1 << 20,
                mtime_ns: 1_700_000_000_123_456_789,
                dev: 2049,
                ino: 77,
            },
            created: true,
            offset: 4096 * 3,
            seq: 9,
            rh: [7; 32],
        };
        assert_eq!(Progress::from_json(&p.to_json()), Some(p));
        assert_eq!(Progress::from_json("{}"), None);
    }

    #[test]
    fn block_runs() {
        let mut b = vec![0u8; BLOCK_SIZE * 5];
        b[BLOCK_SIZE] = 1;
        b[BLOCK_SIZE * 2 + 7] = 1;
        b[BLOCK_SIZE * 5 - 1] = 1;
        assert_eq!(
            runs(&b),
            vec![(0, 1, true), (1, 2, false), (3, 1, true), (4, 1, false)]
        );
    }
}
