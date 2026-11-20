//! Reading a Windows named shared-memory section from Linux.
//!
//! Inside a Proton prefix a named file mapping exists only in that prefix's wineserver. `openrigcue-bridge`, run
//! inside the game's prefix, opens the mapping by its Windows name and mirrors its bytes into
//! `/dev/shm/openrigcue/<name>` (see [`mirror_path`]). This module reads that mirror.
//!
//! A mirror stands for the section in two states: a missing file, and a file of length 0. The bridge empties a
//! per-poll mirror to length 0 when the section disappears with the game, and refills the same file when it comes
//! back (bridge `MirrorDir::withdraw`). Both read as an absent section (`NotFound`),
//! a failed open, so every listener on [`MappedView`] gets
//! the same rule.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Directory the bridge writes mirrors into.
pub const MIRROR_DIR: &str = "/dev/shm/openrigcue";

/// Mirror file for a Windows mapping name under [`MIRROR_DIR`]. Backslashes and slashes (as in
/// `Local\\acpmf_physics`) become `_`.
pub fn mirror_path(windows_name: &str) -> PathBuf {
    mirror_path_in(Path::new(MIRROR_DIR), windows_name)
}

/// Mirror file for a Windows mapping name under the mirror directory `dir` (see [`mirror_path`]).
pub fn mirror_path_in(dir: &Path, windows_name: &str) -> PathBuf {
    let file: String = windows_name.chars().map(|c| if c == '\\' || c == '/' { '_' } else { c }).collect();
    dir.join(file)
}

/// An open mirror of one named mapping.
pub struct MappedView {
    file: File,
    buf: Vec<u8>,
}

/// The error of an absent section.
fn absent() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "shared-memory section absent (mirror missing or emptied)")
}

impl MappedView {
    /// Open the mirror of `windows_name`; fails with `NotFound` while the mirror is missing or empty (bridge or
    /// game not running), which listeners treat as a failed open.
    pub fn open(windows_name: &str, size: usize) -> io::Result<Self> {
        Self::open_in(Path::new(MIRROR_DIR), windows_name, size)
    }

    /// [`MappedView::open`] on the mirror of `windows_name` under the mirror directory `dir`.
    pub fn open_in(dir: &Path, windows_name: &str, size: usize) -> io::Result<Self> {
        let file = File::open(mirror_path_in(dir, windows_name))?;
        let view = MappedView { file, buf: vec![0; size] };
        if view.is_empty()? {
            return Err(absent());
        }
        Ok(view)
    }

    /// Whether the open mirror has length 0 (`fstat`, so the file kept open is checked, not the path).
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.file.metadata()?.len() == 0)
    }

    /// Read the current contents (the first `size` bytes; missing bytes read as zero). Fails with `NotFound` when
    /// the mirror is empty once the bytes have been read: emptied before or during the read, the section is gone,
    /// and the bridge refills the same file only when the game is back.
    pub fn read(&mut self) -> io::Result<&[u8]> {
        let mut done = 0;
        while done < self.buf.len() {
            let n = self.file.read_at(&mut self.buf[done..], done as u64)?;
            if n == 0 {
                self.buf[done..].fill(0);
                break;
            }
            done += n;
        }
        if self.is_empty()? {
            return Err(absent());
        }
        Ok(&self.buf)
    }

    pub fn read_range(&self, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        read_file_range(&self.file, self.buf.len(), offset, len)
    }

    pub(crate) fn read_range_in(dir: &Path, name: &str, size: usize, offset: usize, len: usize) -> io::Result<Vec<u8>> {
        read_file_range(&File::open(mirror_path_in(dir, name))?, size, offset, len)
    }
}

fn read_file_range(file: &File, size: usize, offset: usize, len: usize) -> io::Result<Vec<u8>> {
    if offset.checked_add(len).is_none_or(|end| end > size) {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut bytes = vec![0; len];
    let mut done = 0;
    while done < len {
        let count = file.read_at(&mut bytes[done..], (offset + done) as u64)?;
        if count == 0 {
            break;
        }
        done += count;
    }
    if file.metadata()?.len() == 0 {
        return Err(absent());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn names_are_flattened() {
        assert_eq!(mirror_path("Local\\acpmf_physics"), PathBuf::from("/dev/shm/openrigcue/Local_acpmf_physics"));
        assert_eq!(mirror_path_in(Path::new("/x"), "a/b\\c"), PathBuf::from("/x/a_b_c"));
    }

    #[test]
    fn range_read_keeps_padding_bounds_and_absent_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = mirror_path_in(dir.path(), "range");
        fs::write(&path, [1, 2, 3]).unwrap();
        let view = MappedView::open_in(dir.path(), "range", 8).unwrap();
        assert_eq!(view.read_range(1, 4).unwrap(), [2, 3, 0, 0]);
        assert!(view.read_range(7, 2).is_err());
        fs::write(&path, []).unwrap();
        assert_eq!(view.read_range(0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// An open fails while the section does not exist: a missing or emptied mirror cannot be opened.
    #[test]
    fn missing_or_empty_mirror_does_not_open() {
        let (dir, name) = (tempfile::tempdir().unwrap(), "Local\\acpmf_physics");
        let path = mirror_path_in(dir.path(), name);
        assert_eq!(MappedView::open_in(dir.path(), name, 4).err().unwrap().kind(), io::ErrorKind::NotFound);
        fs::write(&path, b"").unwrap();
        assert_eq!(MappedView::open_in(dir.path(), name, 4).err().unwrap().kind(), io::ErrorKind::NotFound);
        fs::write(&path, [1, 2]).unwrap();
        assert_eq!(MappedView::open_in(dir.path(), name, 4).unwrap().read().unwrap(), &[1, 2, 0, 0]);
    }

    /// A view kept open across the bridge's withdraw (truncate to 0 in place) reads the section as absent instead
    /// of zeros, and reads the new frames once the same file is refilled.
    #[test]
    fn a_kept_view_sees_the_mirror_emptied_and_refilled() {
        let (dir, name) = (tempfile::tempdir().unwrap(), "Local\\acpmf_physics");
        let path = mirror_path_in(dir.path(), name);
        fs::write(&path, [5, 6]).unwrap();
        let mut view = MappedView::open_in(dir.path(), name, 2).unwrap();
        assert_eq!(view.read().unwrap(), &[5, 6]);
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        assert_eq!(view.read().unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::OpenOptions::new().write(true).open(&path).unwrap().write_all_at(&[7, 8], 0).unwrap();
        assert_eq!(view.read().unwrap(), &[7, 8]);
    }
}
