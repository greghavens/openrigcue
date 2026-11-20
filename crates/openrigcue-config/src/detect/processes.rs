//! Running process names from `/proc`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Name recorded for a process whose executable name is not valid UTF-8.
const UNDECODABLE_NAME: &str = "-1";

/// Executable base name → pid of every readable process.
///
/// The name is the base name of `argv[0]` from `/proc/<pid>/cmdline`, which for a Wine or Proton process is its
/// Windows executable path (`C:\…\Game.exe`), split at both `/` and `\`. Processes with an empty command line
/// (kernel threads, zombies) use `/proc/<pid>/comm`. When several processes share a name the highest pid wins.
pub fn running_processes(proc_root: &Path) -> HashMap<String, u32> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return HashMap::new();
    };
    let mut pids: Vec<u32> = entries.flatten().filter_map(|entry| entry.file_name().to_str()?.parse().ok()).collect();
    pids.sort_unstable();
    pids.into_iter().filter_map(|pid| Some((process_name(&proc_root.join(pid.to_string()))?, pid))).collect()
}

fn process_name(process_dir: &Path) -> Option<String> {
    let cmdline = fs::read(process_dir.join("cmdline")).ok()?;
    let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    let raw = if program.is_empty() {
        let comm = fs::read(process_dir.join("comm")).ok()?;
        comm.strip_suffix(b"\n").map(<[u8]>::to_vec).unwrap_or(comm)
    } else {
        program.to_vec()
    };
    let base = raw.rsplit(|byte| *byte == b'/' || *byte == b'\\').next().unwrap_or_default();
    Some(String::from_utf8(base.to_vec()).unwrap_or_else(|_| UNDECODABLE_NAME.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_process(root: &Path, pid: u32, cmdline: &[u8], comm: &[u8]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
        fs::write(dir.join("comm"), comm).unwrap();
    }

    /// Base names of the process images keyed to their pid, later pids overwrite.
    #[test]
    fn names_from_cmdline_and_comm() {
        let root = tempdir("procfs");
        fake_process(
            &root,
            40,
            b"C:\\Program Files\\Codemasters\\DiRT Rally 2.0\\dirtrally2.exe\0-x\0",
            b"dirtrally2.exe\n",
        );
        fake_process(&root, 7, b"/usr/bin/steam\0", b"steam\n");
        fake_process(&root, 9, b"", b"kworker/0:1\n");
        fake_process(&root, 12, b"Z:/games/LFS.exe\0", b"LFS.exe\n");
        fake_process(&root, 13, b"Z:/games/LFS.exe\0", b"LFS.exe\n");
        fake_process(&root, 14, b"/opt/\xff\xfe\0", b"x\n");
        fs::create_dir_all(root.join("self")).unwrap();
        let processes = running_processes(&root);
        assert_eq!(processes.get("dirtrally2.exe"), Some(&40));
        assert_eq!(processes.get("steam"), Some(&7));
        assert_eq!(processes.get("0:1"), Some(&9));
        assert_eq!(processes.get("LFS.exe"), Some(&13));
        assert_eq!(processes.get("-1"), Some(&14));
        assert_eq!(processes.len(), 5);
    }

    /// A missing /proc yields no processes.
    #[test]
    fn missing_root() {
        assert!(running_processes(Path::new("/nonexistent/openrigcue-proc")).is_empty());
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("openrigcue-config-b-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
