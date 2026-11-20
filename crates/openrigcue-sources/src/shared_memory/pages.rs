//! The mirror opener of the shm-a listeners: shm-b's [`block_reader`](super::block_reader) access with the
//! bridge's "an empty mirror is an absent section" rule.
//!
//! Every listener opens its mappings afresh on each read ([`copy_block`](super::block_reader::copy_block));
//! for held sections the bridge keeps the mirror's last frame, so reopening reads the same bytes an open
//! mapping would hold.

/// Opens the bridge mirrors under [`super::mapping::MIRROR_DIR`]. A missing mirror, or one the bridge has emptied
/// (length 0, its mark for a per-poll section that disappeared with the game), fails to open and to read with
/// `NotFound`, a failed open. The rule lives in [`MappedView`](super::mapping::MappedView), so
/// this is the opener every shared-memory listener uses.
pub use super::block_reader::BridgeMirrors as LiveMirrors;

#[cfg(test)]
mod tests {
    use crate::shared_memory::block_reader::{copy_block, BlockOpener, MappingSpec, MirrorsIn};
    use crate::shared_memory::mapping::mirror_path_in;
    use std::fs;
    use std::io;

    /// A mapping read from a temporary mirror directory, so the test never touches the real
    /// `/dev/shm/openrigcue`. [`MirrorsIn`] opens through the same [`MappedView`](super::super::mapping::MappedView)
    /// as [`LiveMirrors`](super::LiveMirrors), where the rule lives.
    fn spec() -> MappingSpec {
        MappingSpec { name: "Local\\acpmf_physics", size: 4 }
    }

    /// Each read opens the mapping afresh,
    /// so a section that disappears fails at once and one that comes back (a new mirror file, or the bridge's
    /// emptied mirror refilled in place) is read again without restarting the listener.
    #[test]
    fn each_read_sees_the_current_mirror() {
        let (spec, dir) = (spec(), tempfile::tempdir().unwrap());
        let path = mirror_path_in(dir.path(), spec.name);
        let mut mirrors = MirrorsIn(dir.path().to_path_buf());
        assert_eq!(copy_block(&mut mirrors, spec).unwrap_err().kind(), io::ErrorKind::NotFound);

        fs::write(&path, [1, 2]).unwrap();
        assert_eq!(copy_block(&mut mirrors, spec).unwrap(), vec![1, 2, 0, 0]);

        fs::write(&path, b"").unwrap(); // the bridge's withdraw: truncated to length 0
        assert_eq!(copy_block(&mut mirrors, spec).unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::write(&path, [3, 3, 3, 3]).unwrap(); // refilled in place
        assert_eq!(copy_block(&mut mirrors, spec).unwrap(), vec![3, 3, 3, 3]);

        fs::remove_file(&path).unwrap();
        assert_eq!(copy_block(&mut mirrors, spec).unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::write(&path, [9, 8, 7, 6, 5]).unwrap(); // a new file (new inode)
        assert_eq!(copy_block(&mut mirrors, spec).unwrap(), vec![9, 8, 7, 6]);
    }

    /// A reader kept across an emptying of its mirror reports the section absent instead of zeros.
    #[test]
    fn an_open_reader_sees_the_mirror_emptied() {
        let (spec, dir) = (MappingSpec { size: 2, ..spec() }, tempfile::tempdir().unwrap());
        let path = mirror_path_in(dir.path(), spec.name);
        fs::write(&path, [5, 6]).unwrap();
        let mut reader = MirrorsIn(dir.path().to_path_buf()).open(spec).unwrap();
        assert_eq!(reader.read().unwrap(), &[5, 6]);
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        assert_eq!(reader.read().unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
