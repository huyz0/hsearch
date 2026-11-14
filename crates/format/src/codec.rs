//! Bounds-checked little-endian reader and single-pass writer.
#![allow(clippy::indexing_slicing)] // the header buffer is a fixed 40 bytes with constant ranges
use crate::crc::crc32c;
use crate::envelope::{ENVELOPE_VERSION, HEADER_LEN, Kind, MAGIC};
use crate::{FormatError, Id, Result};

/// Cursor over untrusted bytes. Every read is checked; no method panics.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.buf.len() {
            return Err(FormatError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn id(&mut self) -> Result<Id> {
        self.array()
    }

    /// Read a `u32` element count. It must not exceed `max` and `count * min_elem` must fit in
    /// the bytes that remain, so a caller may allocate `count` elements without risk.
    pub(crate) fn count(
        &mut self,
        max: usize,
        min_elem: usize,
        what: &'static str,
    ) -> Result<usize> {
        let n = usize::try_from(self.u32()?).map_err(|_| FormatError::TooLarge(what))?;
        if n > max {
            return Err(FormatError::TooLarge(what));
        }
        if n.checked_mul(min_elem).is_none_or(|b| b > self.buf.len()) {
            return Err(FormatError::Truncated);
        }
        Ok(n)
    }

    /// A string of at most `max` bytes with a `u8` length prefix, valid UTF-8.
    pub(crate) fn short_str(&mut self, max: usize, what: &'static str) -> Result<&'a str> {
        let n = usize::from(self.u8()?);
        if n > max {
            return Err(FormatError::TooLarge(what));
        }
        std::str::from_utf8(self.take(n)?).map_err(|_| FormatError::Malformed(what))
    }

    /// A string of at most `max` bytes with a `u16` length prefix, valid UTF-8.
    pub(crate) fn long_str(&mut self, max: usize, what: &'static str) -> Result<&'a str> {
        let n = usize::from(self.u16()?);
        if n > max {
            return Err(FormatError::TooLarge(what));
        }
        std::str::from_utf8(self.take(n)?).map_err(|_| FormatError::Malformed(what))
    }

    /// The body must end exactly here.
    pub(crate) fn finish(&self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(FormatError::Trailing)
        }
    }
}

/// Appends one envelope to a caller-owned buffer in a single pass: the header is reserved,
/// the body is written in place, and `finish` patches the header. On any error the buffer is
/// restored to its length at `begin`.
pub(crate) struct Writer<'o> {
    out: &'o mut Vec<u8>,
    start: usize,
    kind: Kind,
    kind_version: u16,
    mandatory: u64,
}

impl<'o> Writer<'o> {
    pub(crate) fn begin(
        out: &'o mut Vec<u8>,
        kind: Kind,
        kind_version: u16,
        mandatory: u64,
    ) -> Self {
        let start = out.len();
        out.resize(start + HEADER_LEN, 0);
        Self {
            out,
            start,
            kind,
            kind_version,
            mandatory,
        }
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    pub(crate) fn count(&mut self, n: usize, max: usize, what: &'static str) -> Result<()> {
        if n > max {
            return Err(FormatError::TooLarge(what));
        }
        self.u32(u32::try_from(n).map_err(|_| FormatError::TooLarge(what))?);
        Ok(())
    }

    pub(crate) fn long_str(&mut self, s: &str, max: usize, what: &'static str) -> Result<()> {
        if s.len() > max {
            return Err(FormatError::TooLarge(what));
        }
        self.u16(u16::try_from(s.len()).map_err(|_| FormatError::TooLarge(what))?);
        self.bytes(s.as_bytes());
        Ok(())
    }

    pub(crate) fn short_str(&mut self, s: &str, max: usize, what: &'static str) -> Result<()> {
        if s.len() > max {
            return Err(FormatError::TooLarge(what));
        }
        self.u8(u8::try_from(s.len()).map_err(|_| FormatError::TooLarge(what))?);
        self.bytes(s.as_bytes());
        Ok(())
    }

    /// Abandon the object, restoring the buffer.
    pub(crate) fn abort<T>(self, e: FormatError) -> Result<T> {
        self.out.truncate(self.start);
        Err(e)
    }

    pub(crate) fn finish(self) -> Result<()> {
        let body_start = self.start + HEADER_LEN;
        let body_len = self.out.len() - body_start;
        let max = self.kind.max_body();
        let Ok(len32) = u32::try_from(body_len) else {
            return self.abort(FormatError::TooLarge("body"));
        };
        if body_len > max {
            return self.abort(FormatError::TooLarge("body"));
        }
        let body_crc = crc32c(self.out.get(body_start..).unwrap_or_default());
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&MAGIC);
        h[4..6].copy_from_slice(&ENVELOPE_VERSION.to_le_bytes());
        h[6..8].copy_from_slice(&self.kind.code().to_le_bytes());
        h[8..10].copy_from_slice(&self.kind_version.to_le_bytes());
        // 10..12 flags, reserved zero
        h[12..20].copy_from_slice(&self.mandatory.to_le_bytes());
        // 20..28 optional capabilities, none defined
        h[28..32].copy_from_slice(&len32.to_le_bytes());
        h[32..36].copy_from_slice(&body_crc.to_le_bytes());
        let header_crc = crc32c(&h[..36]);
        h[36..40].copy_from_slice(&header_crc.to_le_bytes());
        if let Some(dst) = self.out.get_mut(self.start..body_start) {
            dst.copy_from_slice(&h);
        }
        Ok(())
    }
}
