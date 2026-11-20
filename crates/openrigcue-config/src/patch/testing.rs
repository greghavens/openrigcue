//! Fixtures for the patch tests: throw-away prefixes and port tables.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use super::env::{ModulePorts, PortKey};
use super::prefix::WinePrefix;

/// A fresh scratch directory for one test.
pub fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openrigcue-config-b-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// An empty Proton-style prefix (`drive_c/users/steamuser/Documents`).
pub fn prefix_fixture(tag: &str) -> WinePrefix {
    let root = scratch(tag);
    fs::create_dir_all(root.join("drive_c/users/steamuser/Documents")).unwrap();
    WinePrefix::open(root)
}

/// The same port for every key.
pub fn ports(port: u16) -> impl Fn(PortKey) -> Option<u16> {
    move |_| Some(port)
}

/// Module ports for tests of modules that only read the configured port.
pub fn no_module_ports() -> &'static ModulePorts {
    static EMPTY: OnceLock<ModulePorts> = OnceLock::new();
    EMPTY.get_or_init(ModulePorts::default)
}
