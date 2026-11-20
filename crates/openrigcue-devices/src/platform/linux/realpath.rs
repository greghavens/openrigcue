//! Non-strict resolution of a path's symbolic links, for the link labels of serial ports.
//! Adapted from CPython under the PSF License Version 2; see the bundled
//! Python license and third-party notice. Preserve missing tails and cycles.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

enum Part {
    Name(OsString),
    ResolvedLink(PathBuf),
}

fn parts(path: &Path) -> Vec<Part> {
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .rev()
        .map(|name| Part::Name(OsString::from_vec(name.to_vec())))
        .collect()
}

pub(super) fn realpath(filename: &Path) -> std::io::Result<PathBuf> {
    let mut rest = parts(filename);
    let mut count = rest.len();
    let mut path = if filename.is_absolute() { PathBuf::from("/") } else { std::env::current_dir()? };
    let mut seen: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    while count != 0 {
        let part = rest.pop().expect("unprocessed path part");
        let name = match part {
            Part::ResolvedLink(link) => {
                seen.insert(link, Some(path.clone()));
                continue;
            }
            Part::Name(name) => name,
        };
        count -= 1;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            path.pop();
            if path.as_os_str().is_empty() {
                path.push("/");
            }
            continue;
        }
        let newpath = path.join(name);
        if fs::symlink_metadata(&newpath).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            if let Some(cached) = seen.get(&newpath) {
                path = cached.clone().unwrap_or(newpath);
                continue;
            }
            if let Ok(target) = fs::read_link(&newpath) {
                if target.is_absolute() {
                    path = PathBuf::from("/");
                }
                seen.insert(newpath.clone(), None);
                rest.push(Part::ResolvedLink(newpath));
                let target_parts = parts(&target);
                count += target_parts.len();
                rest.extend(target_parts);
                continue;
            }
        }
        // Missing files, ENOTDIR, permission/readlink errors and loops are
        // retained in the resolved path; later '..' parts still apply.
        path = newpath;
    }
    Ok(path)
}
