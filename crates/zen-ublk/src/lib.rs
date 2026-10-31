//! zen-ublk: a zen-blk volume served as a Linux block device (docs/BLK.md).
//!
//! Two frontends call one adapter ([`disk::Disk`]):
//! * [`fuse`]: one file, `DIR/disk.img`, for `losetup -b 4096`
//! * `ublk` (feature `ublk`): `/dev/ublkbN`
//!
//! Both present a disk with a volatile write cache: writes are buffered,
//! and a flush (FLUSH, FUA, `fsync`) returns once they are committed.

pub mod disk;
pub mod fuse;
#[cfg(feature = "ublk")]
pub mod ublk;

use std::path::PathBuf;
use zen_client::cli::ConnectArgs;

/// A size in bytes: a number, or one with `K`, `M`, `G` or `T` (powers of
/// 1,024), a non-zero multiple of 4,096.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, shift) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 30),
        Some('T' | 't') => (&s[..s.len() - 1], 40),
        _ => (s, 0),
    };
    let n: u64 = num.parse().map_err(|_| format!("{s}: not a size"))?;
    let size = n
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("{s}: too big"))?;
    if size == 0 || !size.is_multiple_of(zen_blk::BLOCK_SIZE as u64) {
        return Err(format!("{s}: not a non-zero multiple of 4,096 bytes"));
    }
    Ok(size)
}

/// The volume's local root pin: `--vol-pin`, or the device key's path +
/// `.vol-<fs>-<hex(name)>`, next to the pins file.
pub fn vol_pin_path(c: &ConnectArgs, name: &[u8], given: Option<PathBuf>) -> PathBuf {
    given.unwrap_or_else(|| c.beside_key(&format!(".vol-{}-{}", c.fs, zen_client::hex(name))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("64M"), Ok(64 << 20));
        assert_eq!(parse_size("1G"), Ok(1 << 30));
        assert_eq!(parse_size("2t"), Ok(2 << 40));
        for bad in ["100", "0", "1X", "", "M", "-4096"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[derive(Parser)]
    struct Args {
        #[command(flatten)]
        c: ConnectArgs,
    }

    #[test]
    fn default_paths() {
        let a = Args::parse_from([
            "t",
            "--url",
            "http://x",
            "--device-key",
            "/k/dev.json",
            "--fs",
            "7",
        ]);
        assert_eq!(a.c.pins_path(), PathBuf::from("/k/dev.json.pins-7"));
        assert_eq!(
            vol_pin_path(&a.c, b"disk", None),
            PathBuf::from("/k/dev.json.vol-7-6469736b")
        );
        assert_eq!(
            vol_pin_path(&a.c, b"disk", Some("/p".into())),
            PathBuf::from("/p")
        );
    }
}
