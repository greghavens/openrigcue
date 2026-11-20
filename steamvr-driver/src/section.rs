//! Named sections (`OpenFileMappingW` / `MapViewOfFile` / `CloseHandle`) on Linux.
//!
//! OpenRigCue publishes a Windows section `<name>` as the file `<mirror dir>/<name with \ and / as _>`
//! (`/dev/shm/openrigcue`, crates/openrigcue-bridge/src/paths.rs and crates/openrigcue-sources mapping.rs);
//! the daemon writes `Local_SimRacingStudioMotionRigPose` in place (crates/openrigcue-daemon/src/accessories.rs),
//! the bridge mirrors Proton-side sections and empties (length 0) the mirror of a section that no longer exists.
//!
//! How the section operations work here:
//! * opening a section is `open(O_RDWR)` of the mirror file; a missing file or
//!   one of length 0 (no section) fails (NULL handle). The descriptor is the handle.
//! * mapping a view makes a duplicate descriptor of the section file (the view, an
//!   object separate from the handle, never closed: views are never unmapped). The view
//!   reads the first 4096 bytes; bytes past the end of a non-empty file read as zero (a section is page-sized).
//!   A view of a section whose mirror is later emptied keeps its last bytes (see `read_view`).
//! * a failing read of a view (an I/O error of its backing store) is an in-page error,
//!   which the device handles as a failed frame.
//! * closing a handle is `close(fd)`; the last error is errno, given as the Win32 code (`last_error`).

use std::collections::HashMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Directory of the Linux side of the named sections.
pub const MIRROR_DIR: &str = "/dev/shm/openrigcue";

/// The section of OpenVR-MotionCompensation.
pub const OVRMC_SECTION: &str = "Local\\OVRMC_MMFv1";
/// The rig pose section.
pub const RIG_POSE_SECTION: &str = "Local\\SimRacingStudioMotionRigPose";
/// The size of a view (4096).
pub const VIEW_SIZE: usize = 4096;

/// A handle value; `NULL_HANDLE` is the Windows NULL handle.
pub type Handle = i32;
pub const NULL_HANDLE: Handle = -1;

/// The page fault of a view whose backing store cannot be read (`EXCEPTION_IN_PAGE_ERROR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InPageError;

/// Linux file name of a section name: `\` and `/` become `_`.
pub fn mirror_file_name(windows_name: &str) -> String {
    windows_name.chars().map(|c| if c == '\\' || c == '/' { '_' } else { c }).collect()
}

fn mirror_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-mirror-dir"))]
    if let Some(dir) = std::env::var_os("OPENRIGCUE_TEST_MIRROR_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(MIRROR_DIR)
}

/// Opens the named section; NULL when there is none.
pub fn open_file_mapping(name: &str) -> Handle {
    let path = mirror_dir().join(mirror_file_name(name));
    let Ok(cpath) = CString::new(path.into_os_string().into_encoded_bytes()) else {
        return NULL_HANDLE;
    };
    // SAFETY: plain libc calls on a NUL-terminated path and a descriptor we own.
    unsafe {
        let fd = libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            return NULL_HANDLE;
        }
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 || st.st_size == 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            let errno = *libc::__errno_location();
            libc::close(fd);
            *libc::__errno_location() = if errno == 0 { libc::ENOENT } else { errno };
            return NULL_HANDLE;
        }
        fd
    }
}

/// Maps a view of a section: a view is an object of its own, separate from the mapping
/// handle (a duplicate descriptor of the section file, with the view's page contents).
/// Closing the mapping handle leaves the view readable, and nothing ever unmaps a view: each reconnect leaks one.
/// Fails (NULL view) when no descriptor can be made.
pub fn map_view_of_file(handle: Handle) -> Handle {
    // SAFETY: duplicating a descriptor value; an invalid one only fails with EBADF.
    let view = unsafe { libc::fcntl(handle, libc::F_DUPFD_CLOEXEC, 0) };
    if view < 0 {
        return NULL_HANDLE;
    }
    // A new view of a section shows its current bytes; until the first read they are the section's zero fill.
    views().insert(view, Box::new([0u8; VIEW_SIZE]));
    view
}

/// The page contents of every view mapped so far (views are never unmapped).
fn views() -> std::sync::MutexGuard<'static, HashMap<Handle, Box<[u8; VIEW_SIZE]>>> {
    static VIEWS: OnceLock<Mutex<HashMap<Handle, Box<[u8; VIEW_SIZE]>>>> = OnceLock::new();
    VIEWS.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Closes a handle: false on failure, errno then holds the reason.
pub fn close_handle(handle: Handle) -> bool {
    // SAFETY: closing a descriptor value; an invalid one only fails with EBADF.
    unsafe { libc::close(handle) == 0 }
}

/// Reads the first `N` bytes of a view.
///
/// * While the mirror file holds the section, the view shows its bytes (zero past the end of the file: a section
///   is page-sized).
/// * An empty mirror means the section's producer has gone. On Windows the driver's open mapping handle and view
///   keep that section object alive with its last contents, and a producer that comes back opens the same
///   section again (the name lives while the driver's handle does). So the view keeps showing the bytes it showed
///   last until the mirror is refilled.
/// * A failing read is the backing store of the view failing: an in-page error.
pub fn read_view<const N: usize>(view: Handle) -> Result<[u8; N], InPageError> {
    let mut page = [0u8; VIEW_SIZE];
    // SAFETY: reading into a local buffer of the given size.
    let n = unsafe { libc::pread(view, page.as_mut_ptr().cast(), VIEW_SIZE, 0) };
    if n < 0 || backing_fails(view) {
        return Err(InPageError);
    }
    let mut views = views();
    let contents = views.entry(view).or_insert_with(|| Box::new([0u8; VIEW_SIZE]));
    if n > 0 {
        **contents = page;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&contents[..N]);
    Ok(out)
}

/// Test builds only: a file `<mirror file>.in-page-error` next to the mirror makes reads of its views fail, the
/// condition of a section whose backing store cannot be read. A tmpfs mirror has no such failure of its own.
#[cfg(any(test, feature = "test-mirror-dir"))]
fn backing_fails(view: Handle) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{view}"))
        .map(|p| {
            let mut marker = p.into_os_string();
            marker.push(".in-page-error");
            std::path::Path::new(&marker).exists()
        })
        .unwrap_or(false)
}

#[cfg(not(any(test, feature = "test-mirror-dir")))]
fn backing_fails(_view: Handle) -> bool {
    false
}

/// Reads the `i`-th little-endian f64 of a byte image.
pub fn f64_at(bytes: &[u8], i: usize) -> f64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[8 * i..8 * i + 8]);
    f64::from_le_bytes(b)
}

/// The text of the current OS error: empty when there is none
/// or no text exists for it. The text is the
/// error's description followed by CR LF, in the form of a system message
/// ("Access denied.\r\n", "Invalid handle.\r\n"); on Linux the description is `strerror`.
pub fn last_error_string() -> Vec<u8> {
    // SAFETY: errno location is thread-local; strerror_r writes into our buffer.
    unsafe {
        let errno = *libc::__errno_location();
        if errno == 0 {
            return Vec::new();
        }
        let mut buf = [0u8; 256];
        if libc::strerror_r(errno, buf.as_mut_ptr().cast(), buf.len()) != 0 {
            return Vec::new();
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let mut text = String::from_utf8_lossy(&buf[..len]).into_owned();
        text.push_str("\r\n");
        std_string_of_message(&text)
    }
}

/// The byte string of an error message: the message is UTF-16LE text and its
/// length in 16-bit units is taken as a byte length. The string is
/// therefore the first `count` bytes of the UTF-16LE text: half of it, with a NUL after every ASCII character.
pub fn std_string_of_message(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    bytes[..units.len()].to_vec()
}

/// The part of a string that reaches the log: the string up to its first NUL.
pub fn c_str(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

/// The last error: errno, as the Win32 error code the same failure gives on Windows. The only failure this
/// driver reads it after is closing a NULL or closed handle, `ERROR_INVALID_HANDLE` (6) on
/// Windows and EBADF on Linux; `close` of a tmpfs file descriptor has no other failure, and any other errno is
/// passed through unchanged.
pub fn last_error() -> u32 {
    // SAFETY: errno location is thread-local.
    let errno = unsafe { *libc::__errno_location() };
    match errno {
        libc::EBADF => ERROR_INVALID_HANDLE,
        e => e as u32,
    }
}

/// `ERROR_INVALID_HANDLE`
pub const ERROR_INVALID_HANDLE: u32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_flatten_like_the_bridge() {
        assert_eq!(mirror_file_name(RIG_POSE_SECTION), "Local_SimRacingStudioMotionRigPose");
        assert_eq!(mirror_file_name(OVRMC_SECTION), "Local_OVRMC_MMFv1");
    }

    #[test]
    fn get_last_error_std_str_keeps_count_bytes_of_the_utf16_text() {
        // The message of error 5 ("Access denied.\r\n"): count 16,
        // 41 00 63 00 63 00 65 00 73 00 73 00 20 00 64 00 65 00 6e 00 69 00 65 00 64 00 2e 00 0d 00 0a 00.
        let s = std_string_of_message("Access denied.\r\n");
        assert_eq!(s, b"A\0c\0c\0e\0s\0s\0 \0d\0");
        assert_eq!(c_str(&s), b"A");
        // count 17 for ERROR_INVALID_HANDLE ("Invalid handle.\r\n"): an odd count ends on the low byte.
        let s = std_string_of_message("Invalid handle.\r\n");
        assert_eq!(s.len(), 17);
        assert_eq!(s[16], b'h');
        assert_eq!(c_str(&s), b"I");
        assert_eq!(std_string_of_message(""), b"");
    }

    #[test]
    fn close_of_a_null_handle_reports_error_invalid_handle() {
        assert!(!close_handle(NULL_HANDLE));
        assert_eq!(last_error(), 6);
    }
}
