//! Where openrigcue keeps its configuration, and how files are written.
//!
//! The folder is `$XDG_CONFIG_HOME/openrigcue/` (default `~/.config/openrigcue/`).

use std::path::{Path, PathBuf};

/// A configuration store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: cannot serialize: {message}")]
    Serialize { path: PathBuf, message: String },
    #[error("{path}: {source}")]
    Database { path: PathBuf, source: Box<crate::import::sqlitedict::DbError>, operation: &'static str },
    #[error("{path}: {source}")]
    Native { path: PathBuf, source: Box<crate::import::pickle::NativeException> },
}

impl StoreError {
    pub fn database(path: &Path, operation: &'static str, source: crate::import::sqlitedict::DbError) -> Self {
        Self::Database { path: path.to_owned(), source: Box::new(source), operation }
    }

    pub fn native_detail(&self) -> String {
        if let Self::Native { source, .. } = self {
            return source.to_string();
        }
        if let Self::Database { source, operation, .. } = self {
            if let Some(exception) = source.native(operation) {
                return exception.to_string();
            }
        }
        format!("RuntimeError: {self}")
    }

    /// Preserve the failure's raw text and metadata until a real destination
    /// chooses its encoding; `native_detail` is the stderr-style view.
    pub fn native(&self) -> Option<crate::import::pickle::NativeException> {
        match self {
            Self::Database { source, operation, .. } => source.native(operation),
            Self::Native { source, .. } => Some((**source).clone()),
            _ => None,
        }
    }

    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        StoreError::Io { path: path.to_owned(), source }
    }

    pub(crate) fn parse(path: &Path, err: impl std::fmt::Display) -> Self {
        StoreError::Parse { path: path.to_owned(), message: err.to_string() }
    }

    pub(crate) fn serialize(path: &Path, err: impl std::fmt::Display) -> Self {
        StoreError::Serialize { path: path.to_owned(), message: err.to_string() }
    }
}

/// The openrigcue configuration directory and the files inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDir {
    root: PathBuf,
}

impl ConfigDir {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        ConfigDir { root: root.into() }
    }

    /// `$XDG_CONFIG_HOME/openrigcue`, or `$HOME/.config/openrigcue`; `None` when neither variable is set.
    pub fn from_env() -> Option<Self> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".config"),
        };
        Some(ConfigDir::at(base.join("openrigcue")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Global settings (`config.ini`).
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("config.ini")
    }

    /// Older TOML configuration, read only when there is no `config.ini`.
    pub fn old_settings_file(&self) -> PathBuf {
        self.root.join("settings.toml")
    }

    /// Per-game and vehicle tunes: an SQLite file of pickled rows.
    pub fn profiles_file(&self) -> PathBuf {
        self.root.join("profile.db")
    }

    /// Speaker layouts, joystick mappings and notification tables, in the same SQLite format.
    pub fn metadata_file(&self) -> PathBuf {
        self.root.join("metadata.db")
    }

    /// Launcher flags, preserved when reset removes config/profile/metadata files.
    pub fn registry_file(&self) -> PathBuf {
        crate::import::registry::path(&self.root)
    }

    /// The older per-game TOML directory, read only during migration.
    pub fn tunes_dir(&self) -> PathBuf {
        self.root.join("tunes")
    }

    /// Speaker layouts (`metadata.db` table `shakers`).
    pub fn shaker_layouts_file(&self) -> PathBuf {
        self.root.join("shaker_layouts.toml")
    }

    /// Joystick axis profiles (`metadata.db` table `joystick`).
    pub fn joysticks_file(&self) -> PathBuf {
        self.root.join("joysticks.toml")
    }

    /// Dismissed notifications (`metadata.db` tables `game_config_notifications`, `notifications`).
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.toml")
    }
}

/// Writes `contents` to `path` so that a reader, or the next start after a crash or power loss, sees either the
/// previous file or the new one whole: the bytes go to a temporary file in the same directory, are flushed to
/// disk, and the temporary file is renamed over the target; the directory is then flushed so the rename itself
/// is durable. The temporary name is unique per call, so concurrent writers of one file never share it; a failed
/// write removes its temporary file and leaves the target as it was.
///
/// Not used for the configuration files ([`write_in_place`]); kept for the callers outside this crate that
/// write their own files with it.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{file_name}.{}.{serial}.tmp", std::process::id()));
    let written = (|| {
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| StoreError::io(&tmp, e))?;
        file.write_all(contents).map_err(|e| StoreError::io(&tmp, e))?;
        file.sync_all().map_err(|e| StoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| StoreError::io(path, e))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // The rename is durable once the directory entry is on disk.
    std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| StoreError::io(dir, e))
}

/// Reads a TOML file; `Ok(None)` when it does not exist.
pub(crate) fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map(Some).map_err(|e| StoreError::parse(path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::io(path, e)),
    }
}

/// Writes `contents` over `path` directly: the file is opened for writing (truncating it) and the
/// text is written; there is no temporary file, flush to disk or rename. The folder is created
/// first when missing.
pub(crate) fn write_in_place(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
    }
    std::fs::write(path, contents).map_err(|e| StoreError::io(path, e))
}

/// Serializes `value` as TOML and writes it in place ([`write_in_place`]).
pub(crate) fn write_toml<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let text = toml::to_string(value).map_err(|e| StoreError::serialize(path, e))?;
    write_in_place(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_of_the_config_directory() {
        let dir = ConfigDir::at("/x/openrigcue");
        assert_eq!(dir.settings_file(), Path::new("/x/openrigcue/config.ini"));
        assert_eq!(dir.profiles_file(), Path::new("/x/openrigcue/profile.db"));
        assert_eq!(dir.metadata_file(), Path::new("/x/openrigcue/metadata.db"));
        assert_eq!(dir.tunes_dir(), Path::new("/x/openrigcue/tunes"));
        assert_eq!(dir.shaker_layouts_file(), Path::new("/x/openrigcue/shaker_layouts.toml"));
    }

    /// The configuration files are overwritten in place: the file keeps its inode, no other file appears.
    #[test]
    fn in_place_write_overwrites_the_file_itself() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/settings.toml");
        write_in_place(&path, b"one, longer").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        write_in_place(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b.toml");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    /// A write that cannot replace its target leaves the target and no temporary file behind.
    #[test]
    fn failed_atomic_write_keeps_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("occupied");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), b"kept").unwrap();
        assert!(write_atomic(&target, b"new").is_err());
        assert_eq!(std::fs::read(target.join("inside")).unwrap(), b"kept");
        let names: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("occupied")]);
    }

    /// Concurrent writers of one file never share a temporary file: the result is one writer's contents, whole.
    #[test]
    fn concurrent_atomic_writes_end_with_one_whole_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shared.toml");
        let contents: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 4096]).collect();
        std::thread::scope(|scope| {
            for bytes in &contents {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        write_atomic(path, bytes).unwrap();
                    }
                });
            }
        });
        let result = std::fs::read(&path).unwrap();
        assert!(contents.contains(&result));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
