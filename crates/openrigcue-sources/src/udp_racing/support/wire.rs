//! Little-endian field access for fixed-layout packets.
//!
//! Decoding fails when the buffer is
//! shorter than the layout. [`Fields`] reports such a short read as [`Short`].

/// A packet was shorter than its layout requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short;

/// Read-only view of a datagram with bounds-checked little-endian readers.
#[derive(Debug, Clone, Copy)]
pub struct Fields<'a> {
    data: &'a [u8],
}

impl<'a> Fields<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Fields { data }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn bytes(&self, offset: usize, len: usize) -> Result<&'a [u8], Short> {
        let end = offset.checked_add(len).ok_or(Short)?;
        self.data.get(offset..end).ok_or(Short)
    }

    fn array<const N: usize>(&self, offset: usize) -> Result<[u8; N], Short> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(offset, N)?);
        Ok(out)
    }

    pub fn u8(&self, offset: usize) -> Result<u8, Short> {
        self.data.get(offset).copied().ok_or(Short)
    }

    pub fn i8(&self, offset: usize) -> Result<i8, Short> {
        Ok(self.u8(offset)? as i8)
    }

    pub fn u16(&self, offset: usize) -> Result<u16, Short> {
        Ok(u16::from_le_bytes(self.array(offset)?))
    }

    pub fn i16(&self, offset: usize) -> Result<i16, Short> {
        Ok(i16::from_le_bytes(self.array(offset)?))
    }

    pub fn u32(&self, offset: usize) -> Result<u32, Short> {
        Ok(u32::from_le_bytes(self.array(offset)?))
    }

    pub fn i32(&self, offset: usize) -> Result<i32, Short> {
        Ok(i32::from_le_bytes(self.array(offset)?))
    }

    /// A `float32` widened to `f64`.
    pub fn f32(&self, offset: usize) -> Result<f64, Short> {
        Ok(f64::from(f32::from_le_bytes(self.array(offset)?)))
    }
}

/// UTF-8 text with invalid sequences dropped.
pub fn decode_utf8_ignoring_errors(raw: &[u8]) -> String {
    let mut out = String::new();
    for chunk in raw.utf8_chunks() {
        out.push_str(chunk.valid());
    }
    out
}

/// ISO 8859-1 text: every byte is one code point.
pub fn decode_latin1(raw: &[u8]) -> String {
    raw.iter().map(|&b| char::from(b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_and_reports_short_buffers() {
        let f = Fields::new(&[1, 0, 0, 0, 0, 0, 0x80, 0x3f]);
        assert_eq!(f.u32(0), Ok(1));
        assert_eq!(f.f32(4), Ok(1.0));
        assert_eq!(f.u32(5), Err(Short));
        assert_eq!(f.bytes(usize::MAX, 2), Err(Short));
    }

    #[test]
    fn text_decoders() {
        assert_eq!(decode_utf8_ignoring_errors(b"ab\xffc"), "abc");
        assert_eq!(decode_latin1(b"\xe9"), "\u{e9}");
    }
}
