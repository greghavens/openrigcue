//! Access to `sqlitedict`-format files (`profile.db`, `metadata.db`).
//!
//! A sqlitedict table is `CREATE TABLE "<name>" (key TEXT PRIMARY KEY, value BLOB)` with pickled
//! values; iteration order is `ORDER BY rowid`. Values go through the restricted pickle reader.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

mod worker;
use worker::{Selection, Worker};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::pickle::{self, PickleError};
use super::value::{Dict, Key, LegacyValue};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("cannot start SQLite worker for {path}: {source}")]
    Worker { path: String, source: std::io::Error },
    #[error("{path} table {table:?}: connection is closed")]
    Closed { path: String, table: String },
    #[error("{path} table {table:?}: KeyError: {key:?}")]
    MissingKey { path: String, table: String, key: String },
    #[error("cannot access {path}: {source}")]
    Sqlite { path: String, source: rusqlite::Error },
    #[error("{path} table {table:?}: sqlitedict keys must be strings")]
    InvalidKey { path: String, table: String },
    #[error("{path} table {table:?} key {key:?}: {source}")]
    Pickle { path: String, table: String, key: String, source: PickleError },
    #[error("cannot decode value from {path}: {class}: {message}")]
    ValueConversion { path: String, class: &'static str, message: String },
}

impl DbError {
    /// The underlying failure before formatting or encoding it for a destination.
    /// Pickle constructors may carry non-scalar text and failure metadata.
    pub fn native(&self, member: &str) -> Option<pickle::NativeException> {
        if let Self::Pickle { source, .. } = self {
            return source.native();
        }
        self.native_exception(member).map(|(class, message)| {
            pickle::NativeException::new(class, openrigcue_model::native_value::PythonString::from(message.as_str()))
        })
    }

    /// Preserve the underlying failure at a dictionary call boundary while the
    /// Display implementation continues carrying path/table diagnostics.
    pub fn native_exception(&self, member: &str) -> Option<(&'static str, String)> {
        match self {
            Self::Pickle { source, .. } => source.native_exception(),
            Self::ValueConversion { class, message, .. } => Some((class, message.clone())),
            Self::MissingKey { key, .. } => Some(("KeyError", openrigcue_model::python_repr::repr_str(key))),
            Self::Closed { .. } if matches!(member, "select_one" | "select" | "execute") => {
                Some(("AttributeError", format!("'NoneType' object has no attribute '{member}'")))
            }
            Self::Sqlite { source, .. } => {
                let (error, message) = match source {
                    rusqlite::Error::SqliteFailure(error, Some(message)) => (error, message),
                    rusqlite::Error::SqlInputError { error, msg, .. } => (error, msg),
                    _ => return None,
                };
                let class = match error.extended_code & 255 {
                    0 => return None,
                    2 | 12 => "InternalError",
                    7 => return Some(("MemoryError", String::new())),
                    1 | 3..=6 | 8..=10 | 13..=17 => "OperationalError",
                    18 => "DataError",
                    19 | 20 => "IntegrityError",
                    21 | 25 => "InterfaceError",
                    _ => "DatabaseError",
                };
                Some((class, message.clone()))
            }
            _ => None,
        }
    }
}

/// Names of the tables in a sqlitedict file.
pub fn table_names(path: &Path) -> Result<Vec<String>, DbError> {
    let conn = open(path)?;
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY rowid")
        .map_err(|source| sqlite_error(path, source))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| sqlite_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error(path, source))?;
    Ok(names)
}

/// Every entry of `table`, decoded, in sqlitedict iteration order. A missing table reads as empty:
/// tables are created on first open.
pub fn read_table(path: &Path, table: &str) -> Result<Dict, DbError> {
    read_rows(path, table)?.map(|row| row.map(|(key, value)| (Key::from(key), value))).collect()
}

/// Ordered rows decode on demand. The queued form starts its SELECT on first
/// item, retains the dictionary and checks deferred errors after each receive.
#[derive(Debug)]
pub struct Rows {
    path: PathBuf,
    table: String,
    source: RowSource,
}

#[derive(Debug)]
enum RowSource {
    Snapshot(std::vec::IntoIter<(String, rusqlite::types::Value)>),
    Queued { table: Arc<TableInner>, selection: Option<Selection> },
    Finished,
}

/// Key iteration: select only keys, lazily, retaining the
/// dictionary until iteration ends. Values are never fetched or unpickled.
#[derive(Debug)]
pub struct Keys {
    table: Option<Arc<TableInner>>,
    selection: Option<Selection>,
}

impl Iterator for Keys {
    type Item = Result<String, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        let table = self.table.as_ref()?;
        if self.selection.is_none() {
            match table.connection() {
                Ok(worker) => {
                    self.selection = Some(
                        worker
                            .select(format!("SELECT key FROM {} ORDER BY rowid", identifier(&table.table)), Vec::new()),
                    )
                }
                Err(error) => {
                    self.table = None;
                    return Some(Err(error));
                }
            }
        }
        let result = self
            .selection
            .as_mut()
            .expect("started iterator")
            .next()
            .map(|row| row.and_then(|row| sql_string(&table.path, &row[0])));
        if result.as_ref().is_none_or(Result::is_err) {
            self.selection = None;
            self.table = None;
        }
        result
    }
}

impl Iterator for Rows {
    type Item = Result<(String, LegacyValue), DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        let raw = match &mut self.source {
            RowSource::Snapshot(rows) => rows.next().map(Ok),
            RowSource::Queued { table, selection } => {
                if selection.is_none() {
                    match table.connection() {
                        Ok(worker) => {
                            *selection = Some(worker.select(
                                format!("SELECT key, value FROM {} ORDER BY rowid", identifier(&self.table)),
                                Vec::new(),
                            ));
                        }
                        Err(error) => {
                            self.source = RowSource::Finished;
                            return Some(Err(error));
                        }
                    }
                }
                selection.as_mut().expect("started iterator").next().map(|row| {
                    let row = row?;
                    let key = sql_string(&self.path, &row[0])?;
                    Ok((key, row[1].clone()))
                })
            }
            RowSource::Finished => return None,
        };
        let result = raw.map(|row| {
            let (key, raw) = row?;
            let bytes = sql_blob(&self.path, &raw)?;
            decode_row(&self.path, &self.table, &key, &bytes).map(|value| (key, value))
        });
        if result.as_ref().is_none_or(Result::is_err) {
            self.source = RowSource::Finished;
        }
        result
    }
}

fn sql_string(path: &Path, value: &rusqlite::types::Value) -> Result<String, DbError> {
    match value {
        rusqlite::types::Value::Text(text) => Ok(text.clone()),
        other => Err(sqlite_error(path, rusqlite::Error::InvalidColumnType(0, "key".into(), other.data_type()))),
    }
}

fn sql_blob(path: &Path, value: &rusqlite::types::Value) -> Result<Vec<u8>, DbError> {
    let failure = |class, message: &str| DbError::ValueConversion {
        path: path.display().to_string(),
        class,
        message: message.into(),
    };
    match value {
        rusqlite::types::Value::Blob(bytes) => Ok(bytes.clone()),
        rusqlite::types::Value::Text(_) => Err(failure("TypeError", "string argument without an encoding")),
        rusqlite::types::Value::Real(_) => Err(failure("TypeError", "cannot convert 'float' object to bytes")),
        rusqlite::types::Value::Null => Err(failure("TypeError", "cannot convert 'NoneType' object to bytes")),
        rusqlite::types::Value::Integer(size) => {
            if *size < 0 {
                return Err(failure("ValueError", "negative count"));
            }
            // An integer stands for that many NUL bytes (none for zero). Avoid
            // allocating that whole buffer: pickle rejects its first opcode.
            Ok(if *size == 0 { Vec::new() } else { vec![0] })
        }
    }
}

/// Read-only ordered iteration without eagerly decoding unrelated or later rows.
pub fn read_rows(path: &Path, table: &str) -> Result<Rows, DbError> {
    if !table_names(path)?.iter().any(|t| t == table) {
        return Ok(Rows {
            path: path.to_owned(),
            table: table.to_owned(),
            source: RowSource::Snapshot(Vec::new().into_iter()),
        });
    }
    let conn = open(path)?;
    connection_rows(&conn, path, table)
}

fn connection_rows(conn: &Connection, path: &Path, table: &str) -> Result<Rows, DbError> {
    let mut stmt = conn
        .prepare(&format!("SELECT key, value FROM {} ORDER BY rowid", identifier(table)))
        .map_err(|source| sqlite_error(path, source))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, rusqlite::types::Value>(1)?)))
        .map_err(|source| sqlite_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error(path, source))?;
    Ok(Rows { path: path.to_owned(), table: table.to_owned(), source: RowSource::Snapshot(rows.into_iter()) })
}

/// Read one row without creating a file/table or decoding unrelated entries.
/// A missing table/key answers None; I/O and that row's pickle errors are retained.
pub fn read_item(path: &Path, table: &str, key: &str) -> Result<Option<LegacyValue>, DbError> {
    let connection = open(path)?;
    let exists: bool = connection
        .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)", [table], |row| {
            row.get(0)
        })
        .map_err(|source| sqlite_error(path, source))?;
    if !exists {
        return Ok(None);
    }
    let raw: Option<rusqlite::types::Value> = connection
        .query_row(&format!("SELECT value FROM {} WHERE key = ?", identifier(table)), [key], |row| row.get(0))
        .optional()
        .map_err(|source| sqlite_error(path, source))?;
    raw.map(|raw| sql_blob(path, &raw).and_then(|bytes| decode_row(path, table, key, &bytes))).transpose()
}

fn decode_row(path: &Path, table: &str, key: &str, bytes: &[u8]) -> Result<LegacyValue, DbError> {
    pickle::decode(bytes).map_err(|source| DbError::Pickle {
        path: path.display().to_string(),
        table: table.to_owned(),
        key: key.to_owned(),
        source,
    })
}

fn open(path: &Path) -> Result<Connection, DbError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|source| sqlite_error(path, source))
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> DbError {
    DbError::Sqlite { path: path.display().to_string(), source }
}

/// Assigns one row and commits it. `REPLACE` gives an existing
/// key a new rowid, so updating a row moves it to the end of table iteration.
pub fn set_item(path: &Path, table: &str, key: &str, value: &LegacyValue) -> Result<(), DbError> {
    let connection = writable(path, table)?;
    put(&connection, path, table, key, value)
}

/// Opens/creates the table, without rewriting existing rows.
pub fn ensure_table(path: &Path, table: &str) -> Result<(), DbError> {
    writable(path, table).map(|_| ())
}

/// Upgrade storage without unpickling any values, including unrelated opaque
/// rows. Source access is read-only and destination insertion is transactional.
pub(super) fn copy_encoded_table(source: &Path, destination: &Path, table: &str) -> Result<(), DbError> {
    let mut target = writable(destination, table)?;
    if !source.is_file() {
        return Ok(());
    }
    let input = open(source)?;
    let exists: bool = input
        .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)", [table], |row| {
            row.get(0)
        })
        .map_err(|source_error| sqlite_error(source, source_error))?;
    if !exists {
        return Ok(());
    }
    let mut statement = input
        .prepare(&format!("SELECT key,value FROM {} ORDER BY rowid", identifier(table)))
        .map_err(|error| sqlite_error(source, error))?;
    let mut rows = statement.query([]).map_err(|error| sqlite_error(source, error))?;
    let transaction = target.transaction().map_err(|error| sqlite_error(destination, error))?;
    while let Some(row) = rows.next().map_err(|error| sqlite_error(source, error))? {
        let key: rusqlite::types::Value = row.get(0).map_err(|error| sqlite_error(source, error))?;
        let value: rusqlite::types::Value = row.get(1).map_err(|error| sqlite_error(source, error))?;
        transaction
            .execute(
                &format!("INSERT INTO {} (key,value) VALUES (?,?)", identifier(table)),
                rusqlite::params![key, value],
            )
            .map_err(|error| sqlite_error(destination, error))?;
    }
    transaction.commit().map_err(|error| sqlite_error(destination, error))
}

/// One shared autocommit dictionary. Each assignment/delete queues SQL then
/// waits for its blocking commit; reads decode only their requested rows.
#[derive(Debug)]
pub struct AutocommitTable {
    inner: Arc<TableInner>,
}

impl AutocommitTable {
    pub fn open(path: &Path, table: &str) -> Result<Self, DbError> {
        Ok(Self { inner: TableInner::open(path, table, true)? })
    }

    /// A dictionary that is already closed; no connection is opened.
    #[doc(hidden)]
    pub fn closed_handle(path: &Path, table: &str) -> Self {
        Self { inner: TableInner::closed(path, table) }
    }

    pub fn contains(&self, key: &str) -> Result<bool, DbError> {
        self.inner
            .connection()?
            .one(
                format!("SELECT 1 FROM {} WHERE key = ?", identifier(&self.inner.table)),
                vec![rusqlite::types::Value::Text(key.to_owned())],
            )
            .map(|row| row.is_some())
    }

    pub fn get(&self, key: &str) -> Result<Option<LegacyValue>, DbError> {
        self.inner.get(key)
    }

    pub fn set(&self, key: &str, value: &LegacyValue) -> Result<(), DbError> {
        self.inner.assign(key, Some(value))?;
        self.inner.commit()
    }

    pub fn delete(&self, key: &str) -> Result<(), DbError> {
        self.inner.assign(key, None)?;
        self.inner.commit()
    }

    /// Explicit commit, including the harmless closed-state case.
    pub fn commit(&self) -> Result<(), DbError> {
        self.inner.commit()
    }

    pub fn rows(&self) -> Result<Rows, DbError> {
        self.inner.rows()
    }

    pub fn keys(&self) -> Keys {
        self.inner.keys()
    }

    /// Close first performs a final blocking autocommit barrier.
    pub fn close(&self) -> Result<(), DbError> {
        self.inner.close(true)
    }
}

/// Non-autocommit dictionary: assignments queue without a completion barrier.
/// The last owner forces a queued close without joining or committing.
#[derive(Debug)]
pub struct PendingTable {
    inner: Arc<TableInner>,
}

impl PendingTable {
    pub fn open(path: &Path, table: &str) -> Result<Self, DbError> {
        Ok(Self { inner: TableInner::open(path, table, false)? })
    }

    /// A dictionary that is already closed; no connection is opened.
    #[doc(hidden)]
    pub fn closed_handle(path: &Path, table: &str) -> Self {
        Self { inner: TableInner::closed(path, table) }
    }

    pub fn assign(&self, key: &str, value: Option<&LegacyValue>) -> Result<(), DbError> {
        self.inner.assign(key, value)
    }

    pub fn get(&self, key: &str) -> Result<Option<LegacyValue>, DbError> {
        self.inner.get(key)
    }

    pub fn rows(&self) -> Result<Rows, DbError> {
        self.inner.rows()
    }

    pub fn keys(&self) -> Keys {
        self.inner.keys()
    }

    pub fn commit(&self) -> Result<(), DbError> {
        self.inner.commit()
    }

    pub fn close(&self) -> Result<(), DbError> {
        self.inner.close(false)
    }

    #[doc(hidden)]
    pub fn execute_batch(&self, sql: &str) -> Result<(), DbError> {
        self.inner.execute_batch(sql)
    }
}

#[derive(Debug)]
struct TableInner {
    path: PathBuf,
    table: String,
    worker: Mutex<Option<Arc<Worker>>>,
}

impl TableInner {
    fn closed(path: &Path, table: &str) -> Arc<Self> {
        Arc::new(Self { path: path.to_owned(), table: table.to_owned(), worker: Mutex::new(None) })
    }

    fn open(path: &Path, table: &str, autocommit: bool) -> Result<Arc<Self>, DbError> {
        let worker = Worker::open(path, autocommit)?;
        worker.execute(
            format!("CREATE TABLE IF NOT EXISTS {} (key TEXT PRIMARY KEY, value BLOB)", identifier(table)),
            Vec::new(),
            false,
        )?;
        worker.commit(true)?;
        Ok(Arc::new(Self { path: path.to_owned(), table: table.to_owned(), worker: Mutex::new(Some(worker)) }))
    }

    fn connection(&self) -> Result<Arc<Worker>, DbError> {
        self.worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| DbError::Closed { path: self.path.display().to_string(), table: self.table.clone() })
    }

    fn assign(&self, key: &str, value: Option<&LegacyValue>) -> Result<(), DbError> {
        let worker = self.connection()?;
        let table = identifier(&self.table);
        let key_value = rusqlite::types::Value::Text(key.to_owned());
        match value {
            Some(value) => {
                // Encode on the producer before execute's error guard.
                let bytes = pickle::encode_checked(value).map_err(|source| DbError::Pickle {
                    path: self.path.display().to_string(),
                    table: self.table.clone(),
                    key: key.into(),
                    source,
                })?;
                worker.execute(
                    format!("REPLACE INTO {table} (key, value) VALUES (?,?)"),
                    vec![key_value, rusqlite::types::Value::Blob(bytes)],
                    true,
                )
            }
            None => {
                // Deletion tests membership without decoding the value.
                if worker.one(format!("SELECT 1 FROM {table} WHERE key = ?"), vec![key_value.clone()])?.is_none() {
                    return Err(DbError::MissingKey {
                        path: self.path.display().to_string(),
                        table: self.table.clone(),
                        key: key.to_owned(),
                    });
                }
                worker.execute(format!("DELETE FROM {table} WHERE key = ?"), vec![key_value], true)
            }
        }
    }

    /// Read the selected row, including pending assignments, without decoding
    /// any unrelated value. The SELECT is a worker completion/error barrier.
    fn get(&self, key: &str) -> Result<Option<LegacyValue>, DbError> {
        let worker = self.connection()?;
        let row = worker.one(
            format!("SELECT value FROM {} WHERE key = ?", identifier(&self.table)),
            vec![rusqlite::types::Value::Text(key.to_owned())],
        )?;
        row.map(|row| {
            let bytes = sql_blob(&self.path, &row[0])?;
            decode_row(&self.path, &self.table, key, &bytes)
        })
        .transpose()
    }

    fn rows(self: &Arc<Self>) -> Result<Rows, DbError> {
        Ok(Rows {
            path: self.path.clone(),
            table: self.table.clone(),
            source: RowSource::Queued { table: self.clone(), selection: None },
        })
    }

    fn keys(self: &Arc<Self>) -> Keys {
        Keys { table: Some(self.clone()), selection: None }
    }

    fn commit(&self) -> Result<(), DbError> {
        let worker = self.worker.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(worker) = worker {
            worker.commit(true)?;
        }
        Ok(())
    }

    /// Normal close waits, rolls back pending work and clears conn only on
    /// success. An error leaves the connection retained.
    fn close(&self, autocommit: bool) -> Result<(), DbError> {
        // The connection reference stays accessible while the worker is closing.
        // Only the reference access is synchronized, not the blocking barrier.
        let connection = self.worker.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(worker) = connection.as_ref() {
            if autocommit {
                worker.commit(true)?;
            }
            worker.close()?;
        }
        *self.worker.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    /// Schema/trigger fixture operations share the same worker connection.
    #[doc(hidden)]
    fn execute_batch(&self, sql: &str) -> Result<(), DbError> {
        self.connection()?.batch(sql)
    }
}

/// Removes one row and commits, leaving the other tables and their iteration order intact.
pub fn delete_item(path: &Path, table: &str, key: &str) -> Result<(), DbError> {
    let connection = writable(path, table)?;
    connection
        .execute(&format!("DELETE FROM {} WHERE key = ?", identifier(table)), [key])
        .map_err(|source| sqlite_error(path, source))?;
    Ok(())
}

/// Replaces one complete table in a transaction, in the given order. This writes a migrated snapshot;
/// individual UI operations use [`set_item`] / [`delete_item`], which commit per row.
pub fn replace_table(path: &Path, table: &str, entries: &Dict) -> Result<(), DbError> {
    if entries.keys().any(|key| !matches!(key, Key::Str(_))) {
        return Err(DbError::InvalidKey { path: path.display().to_string(), table: table.to_owned() });
    }
    let mut connection = writable(path, table)?;
    let transaction = connection.transaction().map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute(&format!("DELETE FROM {}", identifier(table)), [])
        .map_err(|source| sqlite_error(path, source))?;
    for (key, value) in entries.iter() {
        let Key::Str(key) = key else { unreachable!("validated before opening the database") };
        put(&transaction, path, table, key, value)?;
    }
    transaction.commit().map_err(|source| sqlite_error(path, source))
}

fn identifier(table: &str) -> String {
    format!("\"{}\"", table.replace('"', "\"\""))
}

fn native_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    // Foreign keys stay off; this is explicit so alternate SQLite builds agree.
    // These settings are applied before the table is created.
    // In particular an existing WAL file is returned to DELETE journal mode.
    connection.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF")?;
    Ok(connection)
}

fn writable(path: &Path, table: &str) -> Result<Connection, DbError> {
    let connection = native_connection(path).map_err(|source| sqlite_error(path, source))?;
    connection
        .execute(&format!("CREATE TABLE IF NOT EXISTS {} (key TEXT PRIMARY KEY, value BLOB)", identifier(table)), [])
        .map_err(|source| sqlite_error(path, source))?;
    Ok(connection)
}

fn put(connection: &Connection, path: &Path, table: &str, key: &str, value: &LegacyValue) -> Result<(), DbError> {
    let bytes = pickle::encode_checked(value).map_err(|source| DbError::Pickle {
        path: path.display().to_string(),
        table: table.into(),
        key: key.into(),
        source,
    })?;
    connection
        .execute(
            &format!("REPLACE INTO {} (key, value) VALUES (?, ?)", identifier(table)),
            rusqlite::params![key, bytes],
        )
        .map_err(|source| sqlite_error(path, source))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn sqlite_engine_matches_the_original_version_and_empty_concat_arguments() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let joined: String = connection.query_row("SELECT concat_ws('|', '', 'x', '')", [], |row| row.get(0)).unwrap();
        assert_eq!(joined, "x|", "original 3.50.4 retains empty arguments after the first nonempty one");
        assert_eq!(rusqlite::version(), "3.50.4");
        let source: String = connection.query_row("SELECT sqlite_source_id()", [], |row| row.get(0)).unwrap();
        assert_eq!(source, "2025-07-30 19:33:53 4d8adfb30e03f9cf27f800a2c1ba3c48fb4ca1b08b0f5ed59a4d5ecbf45e20a3");
        let foreign_keys: i64 = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap();
        assert_eq!(foreign_keys, 0, "original DLL leaves foreign keys disabled by default");
    }

    #[test]
    fn sqlite_portable_features_match_the_original_dll() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let root: f64 = connection.query_row("SELECT sqrt(9)", [], |row| row.get(0)).unwrap();
        assert_eq!(root, 3.0);
        connection.execute_batch("CREATE VIRTUAL TABLE fixture_fts USING fts4(content)").unwrap();
        let mut query = connection.prepare("PRAGMA compile_options").unwrap();
        let options: Vec<String> = query.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        for name in ["ENABLE_FTS3", "ENABLE_FTS4", "ENABLE_FTS5", "ENABLE_MATH_FUNCTIONS", "ENABLE_RTREE"] {
            assert!(options.iter().any(|option| option == name), "missing original {name}: {options:?}");
        }
        for name in [
            "ENABLE_API_ARMOR",
            "ENABLE_COLUMN_METADATA",
            "ENABLE_DBSTAT_VTAB",
            "ENABLE_FTS3_PARENTHESIS",
            "ENABLE_LOAD_EXTENSION",
            "ENABLE_MEMORY_MANAGEMENT",
            "ENABLE_STAT4",
            "SOUNDEX",
            "USE_URI",
        ] {
            assert!(!options.iter().any(|option| option == name), "extra {name}: {options:?}");
        }
    }

    #[test]
    fn nonblob_values_fail_at_native_bytes_conversion_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.db");
        ensure_table(&path, "unnamed").unwrap();
        let connection = Connection::open(&path).unwrap();
        for (key, value) in [
            ("text", rusqlite::types::Value::Text("N.".into())),
            ("real", rusqlite::types::Value::Real(1.5)),
            ("null", rusqlite::types::Value::Null),
            ("negative", rusqlite::types::Value::Integer(-1)),
            ("zero", rusqlite::types::Value::Integer(0)),
            ("one", rusqlite::types::Value::Integer(1)),
        ] {
            connection.execute("INSERT INTO unnamed(key,value) VALUES (?1,?2)", rusqlite::params![key, value]).unwrap();
        }
        let table = AutocommitTable::open(&path, "unnamed").unwrap();
        for (key, class, message) in [
            ("text", "TypeError", "string argument without an encoding"),
            ("real", "TypeError", "cannot convert 'float' object to bytes"),
            ("null", "TypeError", "cannot convert 'NoneType' object to bytes"),
            ("negative", "ValueError", "negative count"),
            ("zero", "EOFError", "Ran out of input"),
            ("one", "UnpicklingError", "invalid load key, '\\x00'."),
        ] {
            for result in [table.get(key), read_item(&path, "unnamed", key)] {
                assert_eq!(result.unwrap_err().native_exception("select_one"), Some((class, message.into())), "{key}");
            }
        }
        table.close().unwrap();
    }

    #[test]
    fn nonblob_iteration_decodes_only_when_the_row_is_requested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.db");
        set_item(&path, "unnamed", "first", &LegacyValue::Int(12)).unwrap();
        Connection::open(&path).unwrap().execute("INSERT INTO unnamed(key,value) VALUES ('bad','N.')", []).unwrap();
        set_item(&path, "unnamed", "later", &LegacyValue::Int(14)).unwrap();
        let table = PendingTable::open(&path, "unnamed").unwrap();
        for mut rows in [read_rows(&path, "unnamed").unwrap(), table.rows().unwrap()] {
            assert_eq!(rows.next().unwrap().unwrap(), ("first".into(), LegacyValue::Int(12)));
            assert_eq!(
                rows.next().unwrap().unwrap_err().native_exception("select"),
                Some(("TypeError", "string argument without an encoding".into()))
            );
            assert!(rows.next().is_none());
        }
        assert_eq!(table.get("later").unwrap(), Some(LegacyValue::Int(14)));
        table.close().unwrap();
    }
    use crate::import::value::LegacyValue;

    #[test]
    fn key_iteration_is_lazy_and_never_decodes_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.db");
        let database = AutocommitTable::open(&path, "unnamed").unwrap();
        database.set("first", &LegacyValue::None).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![255_u8]])
            .unwrap();
        let keys = database.keys();
        database.set("later", &LegacyValue::Bool(false)).unwrap();
        let mut rows = database.rows().unwrap();
        drop(database);
        assert_eq!(keys.collect::<Result<Vec<_>, _>>().unwrap(), ["first", "opaque", "later"]);
        assert_eq!(rows.next().unwrap().unwrap(), ("first".into(), LegacyValue::None));
        assert!(rows.next().unwrap().is_err(), "items still decodes the opaque value");
        assert!(rows.next().is_none());
    }

    #[test]
    fn key_iteration_observes_pending_transactions_and_defers_closed_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.db");
        let database = PendingTable::open(&path, "unnamed").unwrap();
        let independent = AutocommitTable::open(&path, "unnamed").unwrap();
        let keys = database.keys();
        database.assign("pending", Some(&LegacyValue::None)).unwrap();
        assert_eq!(keys.collect::<Result<Vec<_>, _>>().unwrap(), ["pending"]);
        assert!(independent.keys().next().is_none());
        database.commit().unwrap();
        assert_eq!(independent.keys().collect::<Result<Vec<_>, _>>().unwrap(), ["pending"]);
        database.assign("rolled-back", Some(&LegacyValue::None)).unwrap();
        let mut keys = database.keys();
        database.close().unwrap();
        assert!(keys.next().unwrap().is_err());
        assert!(keys.next().is_none());
        assert_eq!(independent.keys().collect::<Result<Vec<_>, _>>().unwrap(), ["pending"]);
        independent.close().unwrap();
    }

    #[test]
    fn native_default_retains_rows_with_unresolved_foreign_keys() {
        let directory = tempfile::tempdir().unwrap();
        let mut failures = Vec::new();
        for mode in ["snapshot", "pending", "autocommit"] {
            let path = directory.path().join(format!("{mode}.db"));
            // Existing databases may contain schemas beyond the standard dictionary.
            // Foreign-key enforcement is disabled on these connections.
            Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "CREATE TABLE parent (key TEXT PRIMARY KEY);
                     CREATE TABLE unnamed (key TEXT PRIMARY KEY REFERENCES parent(key), value BLOB);",
                )
                .unwrap();
            let value = LegacyValue::Int(7);
            let result = match mode {
                "snapshot" => set_item(&path, "unnamed", "orphan", &value),
                "pending" => {
                    let table = PendingTable::open(&path, "unnamed").unwrap();
                    table.assign("orphan", Some(&value)).and_then(|()| table.commit())
                }
                "autocommit" => {
                    let table = AutocommitTable::open(&path, "unnamed").unwrap();
                    table.set("orphan", &value).and_then(|()| table.commit())
                }
                _ => unreachable!(),
            };
            if let Err(error) = result {
                failures.push(format!("{mode}: {error}"));
                continue;
            }
            assert_eq!(read_item(&path, "unnamed", "orphan").unwrap(), Some(value));
        }
        assert!(failures.is_empty(), "native dictionary writes failed: {failures:#?}");
    }

    #[test]
    fn single_row_read_is_read_only_and_does_not_decode_unrelated_pickles() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        assert!(read_item(&path, "registry", "flag").is_err());
        assert!(!path.exists());
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE \"reg\"\"istry\" (key TEXT PRIMARY KEY, value BLOB); INSERT INTO \"reg\"\"istry\" VALUES ('opaque',X'ff')").unwrap();
        connection
            .execute("INSERT INTO \"reg\"\"istry\" VALUES ('flag',?)", [pickle::encode(&LegacyValue::from("0"))])
            .unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(read_item(&path, "reg\"istry", "flag").unwrap(), Some(LegacyValue::from("0")));
        assert_eq!(read_item(&path, "reg\"istry", "missing").unwrap(), None);
        assert_eq!(read_item(&path, "missing-table", "flag").unwrap(), None);
        assert!(matches!(read_item(&path, "reg\"istry", "opaque"), Err(DbError::Pickle { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(table_names(&path).unwrap(), ["reg\"istry"]);
    }

    #[test]
    fn autocommit_connection_reads_lazily_and_updates_only_the_assigned_row() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.db");
        write_table(
            &path,
            "cache",
            &[
                ("opaque", &[255]),
                ("first", &pickle::encode(&LegacyValue::Int(1))),
                ("second", &pickle::encode(&LegacyValue::Int(2))),
            ],
        );
        write_table(&path, "other", &[("unchanged", &[255, 0, 1])]);
        let cache = AutocommitTable::open(&path, "cache").unwrap();
        assert!(cache.contains("opaque").unwrap(), "key presence does not decode a pickle");
        assert_eq!(cache.get("missing").unwrap(), None);
        cache.set("first", &LegacyValue::Int(3)).unwrap();
        assert_eq!(cache.get("first").unwrap(), Some(LegacyValue::Int(3)));
        assert!(matches!(cache.get("opaque"), Err(DbError::Pickle { .. })));
        let connection = Connection::open(&path).unwrap();
        let keys: Vec<String> = connection
            .prepare("SELECT key FROM cache ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(keys, ["opaque", "second", "first"]);
        let opaque: Vec<u8> =
            connection.query_row("SELECT value FROM cache WHERE key='opaque'", [], |row| row.get(0)).unwrap();
        let other: Vec<u8> =
            connection.query_row("SELECT value FROM other WHERE key='unchanged'", [], |row| row.get(0)).unwrap();
        assert_eq!(opaque, [255]);
        assert_eq!(other, [255, 0, 1]);
        set_item(&path, "cache", "first", &LegacyValue::Str("external".into())).unwrap();
        assert_eq!(
            cache.get("first").unwrap(),
            Some(LegacyValue::Str("external".into())),
            "reads use the shared table, not a stale mirror"
        );
    }

    /// Writes a sqlitedict-format table for tests.
    pub(crate) fn write_table(path: &Path, table: &str, rows: &[(&str, &[u8])]) {
        let conn = Connection::open(path).unwrap();
        conn.execute(&format!("CREATE TABLE IF NOT EXISTS \"{table}\" (key TEXT PRIMARY KEY, value BLOB)"), [])
            .unwrap();
        for (k, v) in rows {
            conn.execute(&format!("REPLACE INTO \"{table}\" (key, value) VALUES (?,?)"), rusqlite::params![k, v])
                .unwrap();
        }
    }

    /// Item iteration order and decoding through the restricted reader.
    #[test]
    fn reads_tables_in_rowid_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.db");
        write_table(&path, "shakers", &[("zeta", b"\x80\x04K\x01."), ("alpha", b"\x80\x04K\x02.")]);
        let d = read_table(&path, "shakers").unwrap();
        assert_eq!(d.str_keys(), ["zeta", "alpha"]);
        assert_eq!(d.get("alpha"), Some(&LegacyValue::Int(2)));
        assert!(read_table(&path, "joystick").unwrap().is_empty());
        assert_eq!(table_names(&path).unwrap(), ["shakers"]);
    }

    #[test]
    fn pickle_errors_name_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.db");
        write_table(&path, "unnamed", &[("ac", b"\x80\x04cos\nsystem\n.")]);
        let err = read_table(&path, "unnamed").unwrap_err();
        assert!(matches!(err, DbError::Pickle { ref key, .. } if key == "ac"), "{err}");
    }

    #[test]
    fn assignment_commits_and_moves_row_without_changing_other_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.db");
        let table = "a\"b";
        set_item(&path, "joystick", "Default", &LegacyValue::Dict(Dict::new())).unwrap();
        set_item(&path, table, "zeta", &LegacyValue::None).unwrap();
        set_item(&path, table, "alpha", &1.into()).unwrap();
        set_item(&path, table, "zeta", &true.into()).unwrap();
        let values = read_table(&path, table).unwrap();
        assert_eq!(values.str_keys(), ["alpha", "zeta"]);
        assert_eq!(values.get("zeta"), Some(&LegacyValue::Bool(true)));
        delete_item(&path, table, "alpha").unwrap();
        assert_eq!(read_table(&path, table).unwrap().str_keys(), ["zeta"]);
        assert_eq!(read_table(&path, "joystick").unwrap().str_keys(), ["Default"]);
    }

    #[test]
    fn pending_table_commits_all_assignments_and_drop_discards_uncommitted_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.db");
        set_item(&path, "unnamed", "a", &1.into()).unwrap();
        set_item(&path, "unnamed", "b", &2.into()).unwrap();
        let table = PendingTable::open(&path, "unnamed").unwrap();
        table.assign("a", Some(&7.into())).unwrap();
        assert_eq!(read_table(&path, "unnamed").unwrap().get("a"), Some(&1.into()));
        table.assign("b", Some(&8.into())).unwrap();
        table.commit().unwrap();
        let stored = read_table(&path, "unnamed").unwrap();
        assert_eq!(stored.get("a"), Some(&7.into()));
        assert_eq!(stored.get("b"), Some(&8.into()));
        table.assign("a", Some(&9.into())).unwrap();
        drop(table);
        assert_eq!(read_table(&path, "unnamed").unwrap(), stored);
    }

    #[test]
    fn snapshot_replacement_is_ordered_and_rollback_keeps_previous_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.db");
        set_item(&path, "unnamed", "old", &LegacyValue::None).unwrap();
        let entries =
            [(Key::from("z"), LegacyValue::Int(2)), (Key::from("a"), LegacyValue::Float(2.0))].into_iter().collect();
        replace_table(&path, "unnamed", &entries).unwrap();
        assert_eq!(read_table(&path, "unnamed").unwrap(), entries);
        // SQLite raises on the second insert, after the delete and first insert. Neither becomes visible.
        Connection::open(&path).unwrap().execute_batch(
            "CREATE TRIGGER reject_a BEFORE INSERT ON unnamed WHEN NEW.key = 'a' BEGIN SELECT RAISE(ABORT, 'blocked'); END;"
        ).unwrap();
        assert!(replace_table(&path, "unnamed", &entries).is_err());
        assert_eq!(read_table(&path, "unnamed").unwrap(), entries);
    }

    #[test]
    #[ignore = "requires python3; run explicitly for storage interoperability"]
    fn python_sqlite_and_pickle_read_the_written_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.db");
        set_item(&path, "unnamed", "z", &LegacyValue::None).unwrap();
        set_item(&path, "unnamed", "a", &LegacyValue::Int(10)).unwrap();
        let tune = [
            (Key::from("enabled"), LegacyValue::Bool(false)),
            (Key::from("profile"), LegacyValue::None),
            (Key::from("gain"), LegacyValue::Float(10.0)),
        ]
        .into_iter()
        .collect();
        set_item(&path, "unnamed", "z", &LegacyValue::Dict(tune)).unwrap();
        let script = r#"
import pickle, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    rows = [(key, pickle.loads(blob)) for key, blob in db.execute('SELECT key,value FROM unnamed ORDER BY rowid')]
assert [key for key, _ in rows] == ['a','z']
assert type(rows[0][1]) is int and rows[0][1] == 10
assert list(rows[1][1]) == ['enabled','profile','gain']
assert rows[1][1]['enabled'] is False and rows[1][1]['profile'] is None
assert type(rows[1][1]['gain']) is float and rows[1][1]['gain'] == 10.0
"#;
        assert!(std::process::Command::new("python3").args(["-c", script]).arg(path).status().unwrap().success());
    }
}

#[cfg(test)]
mod native_error_class_tests {
    use super::*;

    #[test]
    fn sqlite_primary_errors_keep_cpython_exception_classes() {
        for (code, class) in [
            (2, "InternalError"),
            (3, "OperationalError"),
            (4, "OperationalError"),
            (7, "MemoryError"),
            (9, "OperationalError"),
            (10, "OperationalError"),
            (11, "DatabaseError"),
            (12, "InternalError"),
            (13, "OperationalError"),
            (15, "OperationalError"),
            (16, "OperationalError"),
            (17, "OperationalError"),
            (18, "DataError"),
            (19, "IntegrityError"),
            (20, "IntegrityError"),
            (21, "InterfaceError"),
            (25, "InterfaceError"),
            (26, "DatabaseError"),
        ] {
            let error = DbError::Sqlite {
                path: "fixture.db".into(),
                source: rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("detail".into())),
            };
            let message = if code == 7 { "" } else { "detail" };
            assert_eq!(error.native_exception("execute"), Some((class, message.into())), "code={code}");
        }
    }
}
