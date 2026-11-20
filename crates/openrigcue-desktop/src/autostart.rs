//! XDG login registration. The launcher still reads the StartWithWindows setting
//! row before splash/config access. An existing desktop entry belongs to its user
//! and is left intact, including Hidden=true overrides.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::Path;

fn argument(value: &OsStr) -> io::Result<String> {
    let value = value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "desktop entry requires UTF-8 paths"))?;
    if value.chars().any(char::is_control) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "desktop entry argument contains a control character"));
    }
    // Desktop-value unescaping precedes Exec quoting. These are argv values;
    // no shell ever evaluates them. Literal % must survive field-code expansion.
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

pub fn entry(executable: &Path, arguments: &[OsString]) -> io::Result<String> {
    if !executable.is_absolute() || executable.as_os_str().as_encoded_bytes().contains(&b'=') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "desktop executable requires an absolute path without '='",
        ));
    }
    let mut exec = argument(executable.as_os_str())?;
    for value in arguments {
        exec.push(' ');
        exec.push_str(&argument(value)?);
    }
    Ok(format!("[Desktop Entry]\nType=Application\nName=OpenRigCue\nExec={exec}\nIcon=openrigcue\nTerminal=false\n"))
}

/// Install once, never rewriting an existing user entry. This function is called
/// by the product on a GUI launch; tests pass only disposable directories.
pub fn install(config_home: &Path, executable: &Path, arguments: &[OsString]) -> io::Result<()> {
    let contents = entry(executable, arguments)?;
    let directory = config_home.join("autostart");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("openrigcue.desktop");
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    if let Err(error) = file.write_all(contents.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_quotes_argv_and_rejects_desktop_entry_injection() {
        let output = entry(
            Path::new("/apps/Open Rig%%/OpenRigCue.AppImage"),
            &["run".into(), "--login-calling".into(), "--config-dir".into(), "/tmp/a\"$`\\b".into()],
        )
        .unwrap();
        assert!(output.contains("Exec=\"/apps/Open Rig%%%%/OpenRigCue.AppImage\" \"run\" \"--login-calling\""));
        assert!(output.contains(r#""/tmp/a\\"\\$\\`\\\\b""#));
        assert!(entry(Path::new("/tmp/a\nHidden=true"), &[]).is_err());
        assert!(entry(Path::new("/tmp/a=b"), &[]).is_err());
        assert!(entry(Path::new("openrigcue"), &[]).is_err());
    }
    #[test]
    fn registration_preserves_existing_user_disable_entry() {
        let directory = std::env::temp_dir().join(format!("openrigcue-autostart-test-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("autostart")).unwrap();
        let file = directory.join("autostart/openrigcue.desktop");
        std::fs::write(&file, "[Desktop Entry]\nHidden=true\n").unwrap();
        install(&directory, Path::new("/apps/openrigcue"), &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[Desktop Entry]\nHidden=true\n");
        std::fs::remove_file(&file).unwrap();
        install(&directory, Path::new("/apps/openrigcue"), &["run".into(), "--login-calling".into()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            entry(Path::new("/apps/openrigcue"), &["run".into(), "--login-calling".into()]).unwrap()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
