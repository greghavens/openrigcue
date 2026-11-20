//! The mod icon (`icon.png`, referenced by `<iconFilename>` in modDesc.xml).
//!
//! OpenRigCue's own rig mark, rendered from
//! `crates/openrigcue-ui/res/images/openrigcue/mark.svg` with
//! `rsvg-convert --width 128 --height 128`. The rounded graphite tile and
//! restrained R monogram match the app's own identity.

/// The OpenRigCue image used by all three mod editions.
pub const ICON_PNG: &[u8] = include_bytes!("../mod/icon.png");

/// Size of the rendered `icon.png`.
pub const ICON_LEN: usize = 2902;
/// CRC32 of the rendered `icon.png`, also stored in each archive.
pub const ICON_CRC32: u32 = 0xef89_f6d7;

/// The icon bytes for a zip entry.
pub fn icon_png() -> Vec<u8> {
    ICON_PNG.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_the_openrigcue_rgba_mark() {
        assert_eq!(ICON_PNG.len(), ICON_LEN);
        assert_eq!(crc32fast::hash(ICON_PNG), ICON_CRC32);
        assert_ne!(ICON_CRC32, 0xca30_01b4, "the copied racing flag is not shipped");
        assert_eq!(&ICON_PNG[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR: 128 x 128, bit depth 8, colour type 6 (RGBA)
        assert_eq!(&ICON_PNG[12..16], b"IHDR");
        assert_eq!(&ICON_PNG[16..26], &[0, 0, 0, 128, 0, 0, 0, 128, 8, 6]);
    }
}
