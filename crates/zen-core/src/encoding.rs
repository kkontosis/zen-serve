//! Small helpers for the fixed binary layouts (all integers big-endian).

/// `u32be(len) || bytes`: the length-prefixed encoding used inside AAD and KDF inputs.
pub(crate) fn lp(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("length exceeds u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Minimal cursor over a byte slice for parsing fixed layouts.
pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Reader(bytes)
    }

    pub(crate) fn take(&mut self, n: usize) -> crate::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(crate::Error::Format);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> crate::Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    pub(crate) fn u8(&mut self) -> crate::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> crate::Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> crate::Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub(crate) fn lp(&mut self) -> crate::Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub(crate) fn rest(self) -> &'a [u8] {
        self.0
    }

    pub(crate) fn finish(self) -> crate::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(crate::Error::Format)
        }
    }
}
