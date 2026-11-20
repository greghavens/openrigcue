//! Where mirrors and relay sockets live, on the Linux side and as seen from inside Wine.

use std::path::{Path, PathBuf};

/// Linux directory that holds the mirrors; identical to `openrigcue_sources::shared_memory::mapping::MIRROR_DIR`.
pub const MIRROR_DIR: &str = "/dev/shm/openrigcue";

/// File name of the mirror of a Windows section name: `\` and `/` become `_`.
pub fn mirror_file_name(windows_name: &str) -> String {
    windows_name.chars().map(|c| if c == '\\' || c == '/' { '_' } else { c }).collect()
}

/// A Linux absolute path as Wine sees it through its `Z:` drive (`Z:\` is `/`).
pub fn wine_path(unix_path: &str) -> String {
    format!("Z:{}", unix_path.replace('/', "\\"))
}

/// A program path given on the bridge's command line as Windows understands it: Proton hands the game over as
/// a Linux absolute path, which Wine resolves only through `Z:`.
pub fn program_path(arg: &str) -> String {
    if arg.starts_with('/') {
        wine_path(arg)
    } else {
        arg.to_owned()
    }
}

/// Linux path of the Unix socket that receives the relay of pipe `name`: `$XDG_RUNTIME_DIR/openrigcue/pipe-<name>`.
pub fn relay_socket_path(runtime_dir: &Path, pipe_name: &str) -> PathBuf {
    runtime_dir.join("openrigcue").join(format!("pipe-{pipe_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_names_are_flattened() {
        assert_eq!(mirror_file_name("Local\\IRSDKMemMapFileName"), "Local_IRSDKMemMapFileName");
        assert_eq!(mirror_file_name("$rFactor2SMMP_Telemetry$"), "$rFactor2SMMP_Telemetry$");
        assert_eq!(mirror_file_name("a/b\\c"), "a_b_c");
    }

    #[test]
    fn wine_sees_linux_paths_on_z() {
        assert_eq!(wine_path(MIRROR_DIR), "Z:\\dev\\shm\\openrigcue");
        assert_eq!(
            wine_path("/run/user/1000/openrigcue/pipe-FMTelemetry"),
            "Z:\\run\\user\\1000\\openrigcue\\pipe-FMTelemetry"
        );
    }

    #[test]
    fn program_paths_from_proton_are_converted() {
        assert_eq!(
            program_path("/home/u/Steam/steamapps/common/AC/acs.exe"),
            "Z:\\home\\u\\Steam\\steamapps\\common\\AC\\acs.exe"
        );
        assert_eq!(program_path("C:\\Games\\x.exe"), "C:\\Games\\x.exe");
    }

    #[test]
    fn relay_socket_location() {
        assert_eq!(
            relay_socket_path(Path::new("/run/user/1000"), "DynamicCabPipe"),
            PathBuf::from("/run/user/1000/openrigcue/pipe-DynamicCabPipe")
        );
    }
}
