//! Persistent launcher flags (`registry.db`), independent of resettable metadata.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::sqlitedict::{self, DbError};
use super::value::LegacyValue;

pub const FILE_NAME: &str = "registry.db";

static INITIALIZE: Mutex<()> = Mutex::new(());
static TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

pub fn path(root: &Path) -> PathBuf {
    root.join(FILE_NAME)
}

/// Before the first upgrade write, selected reads still see the old metadata
/// table. Reading a disabled launcher flag must not initialize any files.
pub fn read_path(root: &Path) -> PathBuf {
    let independent = path(root);
    if independent.is_file() {
        independent
    } else {
        root.join("metadata.db")
    }
}

pub fn read_item(root: &Path, key: &str) -> Result<Option<LegacyValue>, DbError> {
    sqlitedict::read_item(&read_path(root), "registry", key)
}

pub fn set_item(root: &Path, key: &str, value: &LegacyValue) -> Result<(), Error> {
    prepare(root)?;
    sqlitedict::set_item(&path(root), "registry", key, value)?;
    Ok(())
}

/// Protect values in the older layout before the reset-file deletion. Corrupt
/// or inaccessible old metadata remains a handled registry-access failure.
pub fn migrate_legacy(root: &Path) -> Result<(), Error> {
    if !path(root).is_file() && root.join("metadata.db").is_file() {
        prepare(root)?;
    }
    Ok(())
}

fn prepare(root: &Path) -> Result<(), Error> {
    if path(root).is_file() {
        return Ok(());
    }
    let _guard = INITIALIZE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let destination = path(root);
    if destination.is_file() {
        return Ok(());
    }
    let temporary =
        root.join(format!(".registry-{}-{}.tmp", std::process::id(), TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| Error::Io { path: temporary.clone(), source })?;
    let cleanup = Temporary(temporary);
    sqlitedict::copy_encoded_table(&root.join("metadata.db"), &cleanup.0, "registry")?;
    publish(&cleanup.0, &destination)
}

fn publish(temporary: &Path, destination: &Path) -> Result<(), Error> {
    // Atomic create without replacement: another process may have completed
    // initialization and written live values while this snapshot was prepared.
    match std::fs::hard_link(temporary, destination) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists && destination.is_file() => Ok(()),
        Err(source) => Err(Error::Io { path: destination.to_owned(), source }),
    }
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_initializer_cannot_replace_an_already_live_registry() {
        let temporary = tempfile::tempdir().unwrap();
        let winner = Temporary(temporary.path().join("winner.tmp"));
        let stale = Temporary(temporary.path().join("stale.tmp"));
        for file in [&winner.0, &stale.0] {
            sqlitedict::set_item(file, "registry", "StartWithWindows", &"1".into()).unwrap();
        }
        let destination = path(temporary.path());
        publish(&winner.0, &destination).unwrap();
        set_item(temporary.path(), "StartWithWindows", &"0".into()).unwrap();
        set_item(temporary.path(), "MotionModel", &"H3".into()).unwrap();
        let live = std::fs::read(&destination).unwrap();

        publish(&stale.0, &destination).unwrap();

        assert!(std::fs::read(&destination).unwrap() == live, "stale initialization replaced the live database");
        assert_eq!(read_item(temporary.path(), "StartWithWindows").unwrap(), Some("0".into()));
        assert_eq!(read_item(temporary.path(), "MotionModel").unwrap(), Some("H3".into()));
        drop((winner, stale));
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1, "temporary files are removed");
    }

    #[test]
    fn non_file_destination_keeps_the_write_failure_and_removes_temporary_files() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = path(temporary.path());
        std::fs::create_dir(&destination).unwrap();
        assert!(set_item(temporary.path(), "MotionModel", &"H3".into()).is_err());
        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_selected_reads_do_not_create_registry_storage() {
        let temporary = tempfile::tempdir().unwrap();
        let metadata = temporary.path().join("metadata.db");
        sqlitedict::set_item(&metadata, "registry", "StartWithWindows", &"0".into()).unwrap();
        let before = std::fs::read(&metadata).unwrap();
        assert_eq!(read_item(temporary.path(), "StartWithWindows").unwrap(), Some("0".into()));
        assert!(!path(temporary.path()).exists());
        assert_eq!(std::fs::read(metadata).unwrap(), before);
    }

    #[test]
    fn first_write_preserves_encoded_legacy_rows_and_survives_metadata_removal() {
        let temporary = tempfile::tempdir().unwrap();
        let metadata = temporary.path().join("metadata.db");
        sqlitedict::set_item(&metadata, "registry", "StartWithWindows", &"0".into()).unwrap();
        rusqlite::Connection::open(&metadata)
            .unwrap()
            .execute("INSERT INTO registry (key,value) VALUES ('opaque',?)", [vec![0xff_u8]])
            .unwrap();
        let before = std::fs::read(&metadata).unwrap();
        set_item(temporary.path(), "MotionModel", &"H3".into()).unwrap();
        assert_eq!(std::fs::read(&metadata).unwrap(), before);
        assert_eq!(read_item(temporary.path(), "StartWithWindows").unwrap(), Some("0".into()));
        let copied: Vec<u8> = rusqlite::Connection::open(path(temporary.path()))
            .unwrap()
            .query_row("SELECT value FROM registry WHERE key='opaque'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copied, [0xff]);
        assert!(read_item(temporary.path(), "opaque").is_err());
        sqlitedict::delete_item(&path(temporary.path()), "registry", "StartWithWindows").unwrap();
        assert!(
            read_item(temporary.path(), "StartWithWindows").unwrap().is_none(),
            "old rows must not reappear after upgrade"
        );
        std::fs::remove_file(metadata).unwrap();
        assert_eq!(read_item(temporary.path(), "MotionModel").unwrap(), Some("H3".into()));
    }

    #[test]
    fn failed_legacy_upgrade_keeps_the_previous_store_authoritative() {
        let temporary = tempfile::tempdir().unwrap();
        let metadata = temporary.path().join("metadata.db");
        std::fs::write(&metadata, b"not a database").unwrap();
        assert!(set_item(temporary.path(), "MotionModel", &"H3".into()).is_err());
        assert!(!path(temporary.path()).exists());
        assert_eq!(read_path(temporary.path()), metadata);
        assert_eq!(std::fs::read(metadata).unwrap(), b"not a database");
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 1, "temporary upgrade files are cleaned up");
    }
}
