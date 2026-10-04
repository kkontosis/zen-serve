//! FoundationDB tuple-layer encoding, the subset used by zen-serve
//! (spec/keyspace.md §1): byte strings, text strings, integers and
//! versionstamps. Packing preserves order.

use crate::{STAMP_LEN, Stamp};

/// Byte string type code.
pub const BYTES: u8 = 0x01;
/// Text string type code.
pub const STRING: u8 = 0x02;
/// Zero-integer type code; non-zero integers use `INT_ZERO ± n`.
pub const INT_ZERO: u8 = 0x14;
/// Versionstamp (12 bytes) type code.
pub const VERSIONSTAMP: u8 = 0x33;

/// One tuple element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Elem {
    /// Byte string.
    Bytes(Vec<u8>),
    /// UTF-8 text.
    Str(String),
    /// Signed integer.
    Int(i64),
    /// Complete 12-byte versionstamp: 10-byte commit stamp ‖ u16 user version.
    Vs([u8; 12]),
}

/// Builder for packed keys: `Key::new().str("kv").int(fs).finish()`.
#[derive(Clone, Debug, Default)]
pub struct Key {
    buf: Vec<u8>,
    vs_at: Option<usize>,
}

impl Key {
    /// An empty tuple.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a byte-string element.
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.buf.push(BYTES);
        escape_into(&mut self.buf, b);
        self
    }

    /// Append a text element.
    pub fn str(mut self, s: &str) -> Self {
        self.buf.push(STRING);
        escape_into(&mut self.buf, s.as_bytes());
        self
    }

    /// Append an integer element.
    pub fn int(mut self, v: i64) -> Self {
        encode_int(&mut self.buf, v);
        self
    }

    /// Append a complete versionstamp element.
    pub fn vs(mut self, v: &[u8; 12]) -> Self {
        self.buf.push(VERSIONSTAMP);
        self.buf.extend_from_slice(v);
        self
    }

    /// Append an **incomplete** versionstamp whose 10-byte commit stamp is
    /// filled in at commit time; `user_version` disambiguates within a
    /// commit. Finish with [`Key::finish_incomplete`].
    pub fn vs_incomplete(mut self, user_version: u16) -> Self {
        assert!(self.vs_at.is_none(), "one incomplete versionstamp per key");
        self.buf.push(VERSIONSTAMP);
        self.vs_at = Some(self.buf.len());
        self.buf.extend_from_slice(&[0; STAMP_LEN]);
        self.buf.extend_from_slice(&user_version.to_be_bytes());
        self
    }

    /// Append raw bytes (a raw suffix after a packed prefix).
    pub fn raw(mut self, b: &[u8]) -> Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// The packed key.
    pub fn finish(self) -> Vec<u8> {
        assert!(self.vs_at.is_none(), "use finish_incomplete");
        self.buf
    }

    /// Split around the incomplete versionstamp: `(prefix, suffix)` such
    /// that the final key is `prefix ‖ stamp(10) ‖ suffix`.
    pub fn finish_incomplete(self) -> (Vec<u8>, Vec<u8>) {
        let at = self.vs_at.expect("no incomplete versionstamp");
        let mut prefix = self.buf;
        let suffix = prefix.split_off(at)[STAMP_LEN..].to_vec();
        (prefix, suffix)
    }
}

fn escape_into(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        out.push(c);
        if c == 0 {
            out.push(0xFF);
        }
    }
    out.push(0);
}

fn encode_int(out: &mut Vec<u8>, v: i64) {
    if v == 0 {
        out.push(INT_ZERO);
        return;
    }
    let mag = v.unsigned_abs();
    let n = 8 - (mag.leading_zeros() / 8) as usize;
    let be = if v > 0 { mag } else { !mag }.to_be_bytes();
    if v > 0 {
        out.push(INT_ZERO + n as u8);
    } else {
        out.push(INT_ZERO - n as u8);
    }
    out.extend_from_slice(&be[8 - n..]);
}

/// Pack a list of elements.
pub fn pack(elems: &[Elem]) -> Vec<u8> {
    let mut k = Key::new();
    for e in elems {
        k = match e {
            Elem::Bytes(b) => k.bytes(b),
            Elem::Str(s) => k.str(s),
            Elem::Int(i) => k.int(*i),
            Elem::Vs(v) => k.vs(v),
        };
    }
    k.finish()
}

/// Decode error.
#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError;

/// Unpack the leading elements of `buf`. Returns the elements and the
/// unconsumed rest (e.g. a raw suffix). Decoding stops at the first byte that
/// is not a known type code.
pub fn unpack_prefix(mut buf: &[u8], max: usize) -> Result<(Vec<Elem>, &[u8]), DecodeError> {
    let mut out = Vec::new();
    while out.len() < max && !buf.is_empty() {
        let code = buf[0];
        match code {
            BYTES | STRING => {
                let mut v = Vec::new();
                let mut i = 1;
                loop {
                    match buf.get(i) {
                        None => return Err(DecodeError),
                        Some(0) if buf.get(i + 1) == Some(&0xFF) => {
                            v.push(0);
                            i += 2;
                        }
                        Some(0) => {
                            i += 1;
                            break;
                        }
                        Some(&c) => {
                            v.push(c);
                            i += 1;
                        }
                    }
                }
                buf = &buf[i..];
                out.push(if code == BYTES {
                    Elem::Bytes(v)
                } else {
                    Elem::Str(String::from_utf8(v).map_err(|_| DecodeError)?)
                });
            }
            0x0c..=0x1c => {
                let n = code.abs_diff(INT_ZERO) as usize;
                let body = buf.get(1..1 + n).ok_or(DecodeError)?;
                let mut be = [0u8; 8];
                be[8 - n..].copy_from_slice(body);
                let raw = u64::from_be_bytes(be);
                let v = if code >= INT_ZERO {
                    i64::try_from(raw).map_err(|_| DecodeError)?
                } else {
                    let mask = if n == 8 {
                        u64::MAX
                    } else {
                        (1u64 << (8 * n)) - 1
                    };
                    let mag = !raw & mask;
                    0i64.checked_sub_unsigned(mag).ok_or(DecodeError)?
                };
                out.push(Elem::Int(v));
                buf = &buf[1 + n..];
            }
            VERSIONSTAMP => {
                let body = buf.get(1..13).ok_or(DecodeError)?;
                out.push(Elem::Vs(body.try_into().expect("12 bytes")));
                buf = &buf[13..];
            }
            _ => break,
        }
    }
    Ok((out, buf))
}

/// Unpack a whole packed tuple.
pub fn unpack(buf: &[u8]) -> Result<Vec<Elem>, DecodeError> {
    let (elems, rest) = unpack_prefix(buf, usize::MAX)?;
    if rest.is_empty() {
        Ok(elems)
    } else {
        Err(DecodeError)
    }
}

/// The smallest key greater than every key that starts with `prefix`, or
/// `None` if there is none (`prefix` is empty or all `0xFF`).
pub fn strinc(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(&last) = p.last() {
        if last == 0xFF {
            p.pop();
        } else {
            *p.last_mut().expect("non-empty") += 1;
            return Some(p);
        }
    }
    None
}

/// `[prefix, strinc(prefix))`; an unbounded end becomes `[0xFF]`.
pub fn prefix_range(prefix: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (
        prefix.to_vec(),
        strinc(prefix).unwrap_or_else(|| vec![0xFF]),
    )
}

/// Complete a versionstamp element from a stamp and a user version.
pub fn complete_vs(stamp: &Stamp, user_version: u16) -> [u8; 12] {
    let mut v = [0u8; 12];
    v[..STAMP_LEN].copy_from_slice(stamp);
    v[STAMP_LEN..].copy_from_slice(&user_version.to_be_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    // Encodings from the FoundationDB tuple-layer spec and Python bindings.
    #[test]
    fn known_encodings() {
        assert_eq!(
            pack(&[Elem::Bytes(b"foo\x00bar".to_vec())]),
            b"\x01foo\x00\xffbar\x00"
        );
        assert_eq!(
            pack(&[Elem::Str("F\u{d4}O\u{0}bar".into())]),
            b"\x02F\xc3\x94O\x00\xffbar\x00"
        );
        let ints: &[(i64, &[u8])] = &[
            (0, b"\x14"),
            (1, b"\x15\x01"),
            (255, b"\x15\xff"),
            (256, b"\x16\x01\x00"),
            (-1, b"\x13\xfe"),
            (-255, b"\x13\x00"),
            (-256, b"\x12\xfe\xff"),
            (-5551212, b"\x11\xabK\x93"),
            (i64::MAX, b"\x1c\x7f\xff\xff\xff\xff\xff\xff\xff"),
            (i64::MIN, b"\x0c\x7f\xff\xff\xff\xff\xff\xff\xff"),
        ];
        for (v, enc) in ints {
            assert_eq!(pack(&[Elem::Int(*v)]), *enc, "{v}");
            assert_eq!(unpack(enc).unwrap(), vec![Elem::Int(*v)], "{v}");
        }
    }

    #[test]
    fn roundtrip_and_order() {
        let mut samples = vec![
            vec![Elem::Str("a".into()), Elem::Bytes(vec![])],
            vec![Elem::Str("a".into()), Elem::Bytes(vec![0])],
            vec![Elem::Str("a".into()), Elem::Bytes(vec![0, 1])],
            vec![Elem::Str("a".into()), Elem::Bytes(vec![1])],
            vec![Elem::Str("a".into()), Elem::Int(-300)],
            vec![Elem::Str("a".into()), Elem::Int(-3)],
            vec![Elem::Str("a".into()), Elem::Int(0)],
            vec![Elem::Str("a".into()), Elem::Int(7)],
            vec![Elem::Str("a".into()), Elem::Int(300)],
            vec![Elem::Str("b".into()), Elem::Vs([1; 12])],
            vec![Elem::Str("b".into()), Elem::Vs([2; 12])],
        ];
        let packed: Vec<Vec<u8>> = samples.iter().map(|t| pack(t)).collect();
        for w in packed.windows(2) {
            assert!(w[0] < w[1]);
        }
        for (t, p) in samples.drain(..).zip(&packed) {
            assert_eq!(unpack(p).unwrap(), t);
        }
    }

    #[test]
    fn incomplete_versionstamp_split() {
        let (prefix, suffix) = Key::new()
            .str("log")
            .int(7)
            .vs_incomplete(3)
            .bytes(b"k")
            .finish_incomplete();
        let stamp = [9u8; 10];
        let mut full = prefix.clone();
        full.extend_from_slice(&stamp);
        full.extend_from_slice(&suffix);
        assert_eq!(
            unpack(&full).unwrap(),
            vec![
                Elem::Str("log".into()),
                Elem::Int(7),
                Elem::Vs(complete_vs(&stamp, 3)),
                Elem::Bytes(b"k".to_vec()),
            ]
        );
    }

    #[test]
    fn strinc_works() {
        assert_eq!(strinc(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(strinc(b"a\xff\xff"), Some(b"b".to_vec()));
        assert_eq!(strinc(b"\xff"), None);
    }
}
