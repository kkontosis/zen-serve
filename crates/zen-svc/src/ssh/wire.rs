//! SSH encodings (RFC 4251 §5): `byte`, `boolean`, `uint32`, `uint64`,
//! `string`, `mpint` and `name-list`, decoded strictly.

/// A message that doesn't parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Malformed;

pub(crate) type Result<T> = std::result::Result<T, Malformed>;

/// Reads a message's fields in order.
pub(crate) struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Reader<'a> {
        Reader { b }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(Malformed);
        }
        let (a, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(a)
    }

    /// `n` bytes as they are.
    pub(crate) fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// RFC 4251: any non-zero byte is true; only 0 and 1 are accepted here.
    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed),
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    pub(crate) fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// A string that must be UTF-8.
    pub(crate) fn text(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.string()?).map_err(|_| Malformed)
    }

    /// A name-list: comma-separated, non-empty US-ASCII names.
    pub(crate) fn names(&mut self) -> Result<Vec<&'a str>> {
        let s = self.string()?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        if !s.iter().all(|&c| (0x21..0x7f).contains(&c)) {
            return Err(Malformed);
        }
        let s = std::str::from_utf8(s).expect("ASCII");
        let names: Vec<&str> = s.split(',').collect();
        if names.iter().any(|n| n.is_empty() || n.len() > 64) {
            return Err(Malformed);
        }
        Ok(names)
    }

    /// A non-negative mpint, minimal as RFC 4251 requires: its magnitude,
    /// big-endian, without leading zeros.
    pub(crate) fn mpint(&mut self) -> Result<&'a [u8]> {
        let s = self.string()?;
        match s {
            [] => Ok(s),
            [0] => Err(Malformed),
            [0, b, ..] if b & 0x80 == 0 => Err(Malformed),
            [0, rest @ ..] => Ok(rest),
            [b, ..] if b & 0x80 != 0 => Err(Malformed),
            _ => Ok(s),
        }
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.b)
    }

    /// Nothing may follow.
    pub(crate) fn done(&self) -> Result<()> {
        if self.b.is_empty() {
            Ok(())
        } else {
            Err(Malformed)
        }
    }
}

/// Builds a message.
#[derive(Default)]
pub(crate) struct Writer(pub(crate) Vec<u8>);

impl Writer {
    pub(crate) fn new(typ: u8) -> Writer {
        Writer(vec![typ])
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Writer {
        self.0.push(v);
        self
    }

    pub(crate) fn bool(&mut self, v: bool) -> &mut Writer {
        self.u8(v as u8)
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Writer {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn string(&mut self, s: &[u8]) -> &mut Writer {
        self.u32(u32::try_from(s.len()).expect("strings fit 32 bits"));
        self.0.extend_from_slice(s);
        self
    }

    pub(crate) fn names(&mut self, names: &[&str]) -> &mut Writer {
        self.string(names.join(",").as_bytes())
    }

    /// A non-negative mpint from its big-endian magnitude.
    pub(crate) fn mpint(&mut self, m: &[u8]) -> &mut Writer {
        let m = &m[m.iter().position(|&b| b != 0).unwrap_or(m.len())..];
        if m.first().is_some_and(|b| b & 0x80 != 0) {
            self.u32(m.len() as u32 + 1).u8(0);
            self.0.extend_from_slice(m);
            self
        } else {
            self.string(m)
        }
    }

    pub(crate) fn raw(&mut self, b: &[u8]) -> &mut Writer {
        self.0.extend_from_slice(b);
        self
    }

    pub(crate) fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

/// `string`-encodes a byte string.
pub(crate) fn string(s: &[u8]) -> Vec<u8> {
    Writer::default().string(s).finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpints_are_minimal() {
        let enc = |m: &[u8]| Writer::default().mpint(m).finish();
        assert_eq!(enc(&[]), [0, 0, 0, 0]);
        assert_eq!(enc(&[0, 0]), [0, 0, 0, 0]);
        assert_eq!(enc(&[0x7f]), [0, 0, 0, 1, 0x7f]);
        assert_eq!(enc(&[0, 0x80]), [0, 0, 0, 2, 0, 0x80]);
        for m in [&[][..], &[1], &[0x80, 1], &[0x7f, 0xff]] {
            let e = enc(m);
            assert_eq!(Reader::new(&e).mpint().unwrap(), m);
        }
        for bad in [
            &[0, 0, 0, 1, 0][..],
            &[0, 0, 0, 2, 0, 1],
            &[0, 0, 0, 1, 0x80],
        ] {
            assert_eq!(Reader::new(bad).mpint(), Err(Malformed));
        }
    }

    #[test]
    fn names_are_strict() {
        let enc = |s: &[u8]| string(s);
        assert_eq!(Reader::new(&enc(b"")).names().unwrap(), Vec::<&str>::new());
        assert_eq!(Reader::new(&enc(b"a,b@c")).names().unwrap(), ["a", "b@c"]);
        for bad in [&b"a,,b"[..], b"a,", b",a", b"a b", b"\xc3\xa9"] {
            assert_eq!(Reader::new(&enc(bad)).names(), Err(Malformed));
        }
    }
}
