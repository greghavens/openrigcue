//! What the shm-a listeners read beyond shm-b's [`Fields`] decoder: array elements, Windows wide-character
//! strings and the lenient UTF-8 decode.

use super::block_reader::Fields;

/// Array and wide-string fields of a packed structure, on top of [`Fields`]' scalar fields. Offsets past the end
/// of the copy read as zero bytes, like [`Fields`].
pub(crate) trait ArrayFields<'a> {
    /// Element `index` of an array of 32-bit floats starting at `offset`.
    fn f32_at(&self, offset: usize, index: usize) -> f64;
    /// Element `index` of an array of unsigned 32-bit integers starting at `offset`.
    fn u32_at(&self, offset: usize, index: usize) -> u32;
    /// Value of a Windows wide-character field of `len` code units (UTF-16LE): the code units before the first NUL. Unpaired
    /// surrogates become U+FFFD.
    fn wide_chars(&self, offset: usize, len: usize) -> String;
}

impl<'a> ArrayFields<'a> for Fields<'a> {
    fn f32_at(&self, offset: usize, index: usize) -> f64 {
        self.f32(offset + 4 * index)
    }

    fn u32_at(&self, offset: usize, index: usize) -> u32 {
        self.u32(offset + 4 * index)
    }

    fn wide_chars(&self, offset: usize, len: usize) -> String {
        let field = self.0.get(offset..offset + 2 * len).unwrap_or(&[]);
        let units: Vec<u16> = field
            .as_chunks::<2>()
            .0
            .iter()
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }
}

/// Lenient UTF-8 decoding: invalid sequences are dropped.
pub(crate) fn decode_utf8_ignoring_errors(bytes: &[u8]) -> String {
    bytes.utf8_chunks().map(|chunk| chunk.valid()).collect()
}

/// UTF-16LE bytes of `text`, for writing wide-character fields into test images.
#[cfg(test)]
pub(crate) fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::block_reader::fake::BlockBuilder;

    /// Lenient UTF-8 decoding (EVO names) drops invalid sequences, including a truncated one.
    #[test]
    fn lenient_decoding() {
        assert_eq!(decode_utf8_ignoring_errors(b"a\xffb\xe2\x82"), "ab");
        assert_eq!(decode_utf8_ignoring_errors(b"imola"), "imola");
    }

    /// Wide-character arrays (Windows UTF-16) stop at the first NUL (Assetto Corsa's car model and track).
    #[test]
    fn wide_strings() {
        let mut image = BlockBuilder::new(12);
        image.bytes(0, &utf16le("ks_é"));
        assert_eq!(Fields(&image.build()).wide_chars(0, 6), "ks_é");
        let mut full = BlockBuilder::new(4);
        full.bytes(0, &utf16le("ab"));
        assert_eq!(Fields(&full.build()).wide_chars(0, 2), "ab");
    }

    /// Array elements are 4 bytes apart.
    #[test]
    fn array_elements() {
        let mut image = BlockBuilder::new(12);
        image.f32(8, 2.5).u32(4, 7);
        let image = image.build();
        assert_eq!(Fields(&image).f32_at(0, 2), 2.5);
        assert_eq!(Fields(&image).u32_at(0, 1), 7);
    }
}
