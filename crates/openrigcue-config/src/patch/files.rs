//! Backups.

use std::path::{Path, PathBuf};

use super::winpath;

/// Text inserted before the extension of a backup.
const BACKUP_MARKER: &str = "srs_bck";

/// Backup path of the game file `literal_name` (the name the module asks for) in the folder holding `path`.
///
/// `.ext` becomes `srs_bck.ext` throughout the Windows path: the replacement applies to every component of the
/// Windows path (the components below the prefix's `drive_c` or `dosdevices/<x>:`), so `X.xml` becomes
/// `Xsrs_bck.xml` and a folder whose name contains `.xml` is renamed in the backup path too. The result is
/// matched case-insensitively, as Windows opens an existing backup whatever the case of its name.
pub fn backup_path(path: &Path, literal_name: &str) -> PathBuf {
    let Some(dot) = literal_name.rfind('.') else {
        return path.with_file_name(literal_name);
    };
    let extension = &literal_name[dot..];
    let marked = format!("{BACKUP_MARKER}{extension}");
    let dir = path.parent().unwrap_or(Path::new(""));
    let components: Vec<_> = dir.components().collect();
    let windows_start = windows_root_index(&components).map_or(components.len(), |index| index + 1);
    let mut backup = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        let part = component.as_os_str();
        match part.to_str().filter(|_| index >= windows_start) {
            Some(text) => backup = winpath::resolve(&backup, &text.replace(extension, &marked)),
            None => backup.push(part),
        }
    }
    winpath::resolve(&backup, &literal_name.replace(extension, &marked))
}

/// Index of the last component that is a prefix drive root (`drive_c`, or `<x>:` inside `dosdevices`).
fn windows_root_index(components: &[std::path::Component<'_>]) -> Option<usize> {
    components.iter().enumerate().rev().find_map(|(index, component)| {
        let name = component.as_os_str().to_str()?;
        let is_drive = name == "drive_c"
            || (index > 0
                && components[index - 1].as_os_str() == "dosdevices"
                && name.len() == 2
                && name.ends_with(':'));
        is_drive.then_some(index)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backup naming: `.xml` becomes `srs_bck.xml` over the Windows part of the path.
    #[test]
    fn backup_names() {
        let c = "/p/pfx/drive_c/users/steamuser";
        assert_eq!(backup_path(Path::new(&format!("{c}/X.xml")), "X.xml"), PathBuf::from(format!("{c}/Xsrs_bck.xml")));
        assert_eq!(
            backup_path(Path::new(&format!("{c}/b.ini/UDP.ini")), "UDP.ini"),
            PathBuf::from(format!("{c}/bsrs_bck.ini/UDPsrs_bck.ini"))
        );
        // Folders above the drive are not part of the Windows path.
        assert_eq!(
            backup_path(Path::new("/x.lua/pfx/dosdevices/c:/a/SRSExport.lua"), "SRSExport.lua"),
            PathBuf::from("/x.lua/pfx/dosdevices/c:/a/SRSExportsrs_bck.lua")
        );
        // The literal name decides, whatever the case of the file found on disk.
        assert_eq!(
            backup_path(Path::new(&format!("{c}/SIMCONNECT.XML")), "SimConnect.xml"),
            PathBuf::from(format!("{c}/SimConnectsrs_bck.xml"))
        );
    }
}
