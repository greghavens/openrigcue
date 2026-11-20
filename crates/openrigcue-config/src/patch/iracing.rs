//! iRacing `app.ini`: memory-mapped telemetry on, 360 Hz logging off.

use super::env::{PatchEnvironment, UserFolder, UserPath};
use super::text::{self, Encoding, Newline};
use super::PatchError;

const APP_INI: UserPath = UserPath::new(UserFolder::Documents, "iRacing\\app.ini");
const LOG_360HZ: &str = "irsdkLog360Hz";
const ENABLE_MEMORY: &str = "irsdkEnableMem";

fn app_ini(env: &PatchEnvironment<'_>) -> Option<std::path::PathBuf> {
    env.prefix.map(|prefix| APP_INI.resolve(prefix))
}

/// `app.ini` exists.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    app_ini(env).is_some_and(|path| path.is_file())
}

/// The first character after `=` of the two keys (last occurrence wins) is exactly `1` for `irsdkEnableMem` and
/// `0` for `irsdkLog360Hz`, and both keys are present; any read failure is false.
pub fn patched(env: &PatchEnvironment<'_>) -> bool {
    let Some(content) = app_ini(env).and_then(|path| text::read_text(&path, Encoding::Ansi).ok()) else {
        return false;
    };
    let mut log_360hz = None;
    let mut enable_memory = None;
    for line in text::lines_with_ends(&content) {
        let mut parts = line.split('=');
        let key = parts.next().unwrap_or_default();
        let slot = match key {
            LOG_360HZ => &mut log_360hz,
            ENABLE_MEMORY => &mut enable_memory,
            _ => continue,
        };
        let Some(value) = parts.next() else { return false };
        *slot = Some(value.chars().take(1).collect::<String>());
    }
    enable_memory.as_deref() == Some("1") && log_360hz.as_deref() == Some("0")
}

/// Rewrites every line whose key is one of the two settings with a fixed text; missing keys are not
/// added and no backup is taken. Any failure is `Ok(false)`.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(path) = app_ini(env) else { return Ok(false) };
    let Ok(content) = text::read_text(&path, Encoding::Ansi) else { return Ok(false) };
    let log_line = format!("{:<40}\t; Log some telemetry at 360 Hz rather than at 60 Hz\n", format!("{LOG_360HZ}=0"));
    let memory_line = format!("{:<40}\t; enable memory based telemetry\n", format!("{ENABLE_MEMORY}=1"));
    let rewritten: String = text::lines_with_ends(&content)
        .into_iter()
        .map(|line| match line.split('=').next() {
            Some(LOG_360HZ) => log_line.as_str(),
            Some(ENABLE_MEMORY) => memory_line.as_str(),
            _ => line,
        })
        .collect();
    Ok(text::write_text(&path, &rewritten, Encoding::Ansi, Newline::Windows).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::testing::{ports, prefix_fixture};
    use std::fs;

    /// Patch rewrites both lines with the fixed padding; the patched check reads the first value character.
    #[test]
    fn app_ini_settings() {
        let prefix = prefix_fixture("iracing");
        let dir = prefix.documents().join("iRacing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.ini"), "[Misc]\r\nirsdkEnableMem=0 ; x\r\nirsdkLog360Hz=1\r\n").unwrap();
        let lookup = ports(0);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(installed(&env));
        assert!(!patched(&env));
        patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("app.ini")).unwrap(),
            "[Misc]\r\nirsdkEnableMem=1                        \t; enable memory based telemetry\r\n\
irsdkLog360Hz=0                         \t; Log some telemetry at 360 Hz rather than at 60 Hz\r\n"
        );
        assert!(patched(&env));
        fs::write(dir.join("app.ini"), "irsdkEnableMem=1\n").unwrap();
        assert!(!patched(&env));
        fs::write(dir.join("app.ini"), "irsdkEnableMem=1\nirsdkLog360Hz=0\nirsdkLog360Hz").unwrap();
        assert!(!patched(&env));
    }
}
