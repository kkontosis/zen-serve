//! Content-defined chunking (spec/formats.md §11.7): a keyed gear hash with
//! normalized cuts, so an insertion changes only the chunks around it.

use crate::fs::CdcParams;
use crate::kdf::{Key32, kdf};
use crate::keys::FsKeys;
use crate::labels;

/// `K_cdc = KDF("zen/v1/fs-cdc", NK, u32(fs))`: for chunks under the fs keys.
pub fn fs_cdc_key(fs: &FsKeys) -> Key32 {
    fs.cdc_key()
}

/// `K_cdc = KDF("zen/v1/fs-cdc-file", file_key, u32(fs))`: for chunks under a file key.
pub fn file_cdc_key(fs_id: u32, file_key: &[u8; 32]) -> Key32 {
    kdf(labels::FS_CDC_FILE, file_key, &fs_id.to_be_bytes())
}

/// `G[i] = u64le(BLAKE3.keyed_hash(K_cdc, u8(i))[0..8])`.
pub fn gear_table(key: &[u8; 32]) -> [u64; 256] {
    let mut g = [0u64; 256];
    for (i, v) in g.iter_mut().enumerate() {
        let h = blake3::keyed_hash(key, &[i as u8]);
        *v = u64::from_le_bytes(h.as_bytes()[..8].try_into().expect("8 <= 32"));
    }
    g
}

/// A streaming chunker: feed it the content in pieces of any size; it reports
/// where chunks end.
#[derive(Clone)]
pub struct Chunker {
    gear: Box<[u64; 256]>,
    params: CdcParams,
    /// Bytes in the current chunk so far.
    len: u32,
    h: u64,
    shift_s: u32,
    shift_l: u32,
}

impl Chunker {
    /// A chunker under `key` (a `K_cdc`) with checked parameters.
    pub fn new(key: &[u8; 32], params: CdcParams) -> crate::Result<Self> {
        params.check()?;
        Ok(Chunker {
            gear: Box::new(gear_table(key)),
            params,
            len: 0,
            h: 0,
            shift_s: 64 - (params.avg_bits as u32 + 2),
            shift_l: 64 - (params.avg_bits as u32 - 2),
        })
    }

    /// The parameters.
    pub fn params(&self) -> CdcParams {
        self.params
    }

    /// Bytes pushed since the last cut.
    pub fn pending(&self) -> u32 {
        self.len
    }

    /// Feed `data`; returns the positions in `data` (exclusive ends) where a
    /// chunk ends, in order.
    pub fn push(&mut self, data: &[u8]) -> Vec<usize> {
        let p = self.params;
        let avg = 1u32 << p.avg_bits;
        let mut cuts = Vec::new();
        let mut i = 0;
        while i < data.len() {
            // Below `min` nothing is hashed: skip ahead.
            if self.len < p.min {
                let skip = ((p.min - self.len) as usize).min(data.len() - i);
                self.len += skip as u32;
                i += skip;
                continue;
            }
            let b = data[i];
            i += 1;
            self.h = (self.h << 1).wrapping_add(self.gear[b as usize]);
            self.len += 1;
            let cut = self.len == p.max
                || if self.len < avg {
                    self.h >> self.shift_s == 0
                } else {
                    self.h >> self.shift_l == 0
                };
            if cut {
                cuts.push(i);
                self.len = 0;
                self.h = 0;
            }
        }
        cuts
    }

    /// The end of the content: the length of the last chunk (0 when the
    /// content ended at a cut). The chunker starts afresh.
    pub fn finish(&mut self) -> u32 {
        let last = self.len;
        self.len = 0;
        self.h = 0;
        last
    }

    /// The chunk lengths of `data` as a whole (the last one included).
    pub fn lengths(&mut self, data: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut prev = 0;
        for c in self.push(data) {
            out.push((c - prev) as u32);
            prev = c;
        }
        let last = self.finish();
        if last > 0 {
            out.push(last);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(n: usize, seed: &str) -> Vec<u8> {
        let mut v = vec![0u8; n];
        blake3::Hasher::new()
            .update(seed.as_bytes())
            .finalize_xof()
            .fill(&mut v);
        v
    }

    fn chunker() -> Chunker {
        Chunker::new(&[7u8; 32], CdcParams::default_for(90_000).unwrap()).unwrap()
    }

    #[test]
    fn pieces_give_the_same_cuts() {
        let data = input(1 << 20, "cdc-pieces");
        let whole = chunker().lengths(&data);
        for piece in [1usize, 7, 65_536] {
            let mut c = chunker();
            let mut cuts = Vec::new();
            let mut off = 0;
            for part in data.chunks(piece) {
                cuts.extend(c.push(part).into_iter().map(|x| x + off));
                off += part.len();
            }
            let mut lens = Vec::new();
            let mut prev = 0;
            for x in cuts {
                lens.push((x - prev) as u32);
                prev = x;
            }
            let last = c.finish();
            if last > 0 {
                lens.push(last);
            }
            assert_eq!(lens, whole, "piece {piece}");
        }
    }

    #[test]
    fn lengths_within_bounds() {
        let p = CdcParams::default_for(90_000).unwrap();
        let lens = chunker().lengths(&input(4 << 20, "cdc-bounds"));
        assert_eq!(lens.iter().map(|&l| l as usize).sum::<usize>(), 4 << 20);
        for &l in &lens[..lens.len() - 1] {
            assert!(l >= p.min && l <= p.max, "{l}");
        }
    }

    /// Chunks before and after an edit, as sets: only a few around it differ.
    fn changed(a: &[u8], b: &[u8]) -> usize {
        let split = |d: &[u8]| {
            let mut out = std::collections::HashSet::new();
            let mut off = 0usize;
            for l in chunker().lengths(d) {
                out.insert(d[off..off + l as usize].to_vec());
                off += l as usize;
            }
            out
        };
        let (sa, sb) = (split(a), split(b));
        sb.difference(&sa).count()
    }

    #[test]
    fn resyncs_after_an_insertion_and_a_deletion() {
        let a = input(2 << 20, "cdc-resync");
        let mut ins = a.clone();
        ins.splice(700_000..700_000, input(100, "x"));
        assert!(changed(&a, &ins) <= 2, "{}", changed(&a, &ins));
        let mut del = a.clone();
        del.drain(1_000_000..1_000_333);
        assert!(changed(&a, &del) <= 2, "{}", changed(&a, &del));
    }

    #[test]
    fn mean_length_near_the_average() {
        let lens = chunker().lengths(&input(64 << 20, "cdc-mean"));
        let mean = (64u64 << 20) / lens.len() as u64;
        assert!((25 * 1024..=45 * 1024).contains(&mean), "mean {mean}");
        // Normalized cuts: hardly any chunk is cut at `max` (one mask alone
        // would cut several percent there).
        let max = CdcParams::default_for(90_000).unwrap().max;
        let forced = lens.iter().filter(|&&l| l == max).count();
        assert!(
            forced * 200 < lens.len(),
            "{forced} of {} cut at max",
            lens.len()
        );
    }

    #[test]
    fn keys_change_the_cuts() {
        let data = input(1 << 20, "cdc-keys");
        let p = CdcParams::default_for(90_000).unwrap();
        let a = Chunker::new(&[1u8; 32], p).unwrap().lengths(&data);
        let b = Chunker::new(&[2u8; 32], p).unwrap().lengths(&data);
        assert_ne!(a, b);
    }

    #[test]
    fn params_checked() {
        let ok = CdcParams {
            min: 64,
            avg_bits: 8,
            max: 257,
        };
        assert!(ok.check().is_ok());
        for bad in [
            CdcParams {
                min: 63,
                avg_bits: 8,
                max: 257,
            },
            CdcParams {
                min: 256,
                avg_bits: 8,
                max: 257,
            },
            CdcParams {
                min: 64,
                avg_bits: 8,
                max: 256,
            },
            CdcParams {
                min: 64,
                avg_bits: 7,
                max: 1000,
            },
            CdcParams {
                min: 64,
                avg_bits: 25,
                max: u32::MAX,
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
            assert!(Chunker::new(&[0u8; 32], bad).is_err());
        }
        assert!(CdcParams::default_for(1_000).is_none());
        assert_eq!(CdcParams::default_for(90_000).unwrap().max, 89_936);
        assert_eq!(CdcParams::default_for(1 << 20).unwrap().max, 131_072);
    }
}
