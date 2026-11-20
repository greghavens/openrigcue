//! Reading the game's memory. On Linux the Proton process is found through `/proc/*/cmdline`, its module base through
//! `/proc/<pid>/maps` and its memory through `/proc/<pid>/mem`.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;

/// Access to the game process.
pub trait ProcessAccess: Send {
    /// The process whose executable base name is `exe_name` (the dispatcher's process table) and the path of its
    /// image.
    fn find_process(&mut self, exe_name: &str) -> Option<(u32, String)>;
    /// Image of an already selected PID; failure is an empty image.
    /// Cached-PID adapters override this; older numeric adapters need not.
    fn image_path(&mut self, _pid: u32) -> Option<String> {
        None
    }
    /// Opens the process for reading.
    fn open(&mut self, pid: u32) -> io::Result<()>;
    /// Releases the process.
    fn close(&mut self);
    /// Base address of the loaded module whose path contains `module_name`.
    fn module_base(&mut self, module_name: &str) -> io::Result<Option<u64>>;
    /// Reads process memory into a zeroed buffer: whatever cannot be read stays zero.
    fn read(&mut self, address: u64, buffer: &mut [u8]);
    /// Whether a read was refused for lack of permission since the last call.
    fn take_refused(&mut self) -> bool {
        false
    }
}

/// The game error text of a memory read the system refused for lack of permission (for example the Yama
/// `kernel.yama.ptrace_scope` restriction on reading another program's memory).
pub const READ_REFUSED: &str = "reading the game's memory was refused: permission denied";

/// The `/proc` implementation.
#[derive(Default)]
pub struct ProcFs {
    open: Option<(u32, File)>,
    /// A read failed for lack of permission since the last [`ProcessAccess::take_refused`].
    refused: bool,
}

impl ProcFs {
    /// The executable named by a Wine/Proton command line: the base name of `argv[0]`, Windows or Unix
    /// separators.
    fn image_of(cmdline: &[u8]) -> Option<String> {
        let argv0 = cmdline.split(|&b| b == 0).next().filter(|a| !a.is_empty())?;
        Some(String::from_utf8_lossy(argv0).into_owned())
    }
}

/// Base name of a path written with `\` or `/`.
pub(crate) fn base_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Lowest start address among the `/proc/<pid>/maps` lines whose path contains `module_name`.
pub(crate) fn module_base_in_maps(maps: &str, module_name: &str) -> Option<u64> {
    maps.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, ' ');
            let range = fields.next()?;
            let path = fields.nth(4)?.trim_start();
            path.contains(module_name).then(|| u64::from_str_radix(range.split('-').next()?, 16).ok()).flatten()
        })
        .min()
}

impl ProcessAccess for ProcFs {
    fn find_process(&mut self, exe_name: &str) -> Option<(u32, String)> {
        let entries = fs::read_dir("/proc").ok()?;
        let mut found: Option<(u32, String)> = None;
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Some(image) = fs::read(entry.path().join("cmdline")).ok().and_then(|c| Self::image_of(&c)) else {
                continue;
            };
            // The process table is keyed by base name; with several matches the last one listed wins.
            if base_name(&image) == exe_name && found.as_ref().is_none_or(|(best, _)| pid > *best) {
                found = Some((pid, image));
            }
        }
        found
    }

    fn image_path(&mut self, pid: u32) -> Option<String> {
        fs::read(format!("/proc/{pid}/cmdline")).ok().and_then(|bytes| Self::image_of(&bytes))
    }

    fn open(&mut self, pid: u32) -> io::Result<()> {
        let file = File::open(format!("/proc/{pid}/mem"))?;
        self.open = Some((pid, file));
        Ok(())
    }

    fn close(&mut self) {
        self.open = None;
    }

    fn module_base(&mut self, module_name: &str) -> io::Result<Option<u64>> {
        let pid = self.open.as_ref().map(|(pid, _)| *pid).ok_or(io::ErrorKind::NotConnected)?;
        let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
        Ok(module_base_in_maps(&maps, module_name))
    }

    fn read(&mut self, address: u64, buffer: &mut [u8]) {
        let Some((_, file)) = &self.open else { return };
        let mut done = 0;
        while done < buffer.len() {
            match file.read_at(&mut buffer[done..], address.wrapping_add(done as u64)) {
                Ok(0) => break,
                Err(error) => {
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        self.refused = true;
                    }
                    break;
                }
                Ok(n) => done += n,
            }
        }
    }

    fn take_refused(&mut self) -> bool {
        std::mem::take(&mut self.refused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module base address is the
    /// lowest `/proc/<pid>/maps` mapping whose path contains the module name.
    #[test]
    fn module_base_from_maps() {
        let maps = "\
140000000-140001000 r--p 00000000 00:2a 123 /games/Products/elite-dangerous-odyssey-64/EliteDangerous64.exe
140001000-145000000 r-xp 00001000 00:2a 123 /games/Products/elite-dangerous-odyssey-64/EliteDangerous64.exe
7f0000000000-7f0000001000 r--p 00000000 00:2a 9 /usr/lib/wine/x86_64-windows/ntdll.dll
7ffd0000-7ffd1000 rw-p 00000000 00:00 0 [stack]";
        assert_eq!(module_base_in_maps(maps, "EliteDangerous64.exe"), Some(0x1_4000_0000));
        assert_eq!(module_base_in_maps(maps, "missing.exe"), None);
    }

    /// The process image is
    /// `argv[0]` of `/proc/<pid>/cmdline` and its base name, with either path separator.
    #[test]
    fn image_and_base_name() {
        let cmdline = b"C:\\Program Files\\Frontier\\Products\\elite-dangerous-64\\EliteDangerous64.exe\0/novid\0";
        let image = ProcFs::image_of(cmdline).unwrap();
        assert_eq!(base_name(&image), "EliteDangerous64.exe");
        assert_eq!(base_name("/usr/bin/wine64"), "wine64");
        assert_eq!(ProcFs::image_of(b"\0"), None);
    }
}
