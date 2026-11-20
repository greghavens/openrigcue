use super::*;
use crate::import::sqlitedict::{self, *};
use std::sync::mpsc;
use std::time::Duration;

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            let _ = send.send(());
        }
    }
}

fn pause(worker: &Worker) -> Release {
    let (entered, receive) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    worker.requests.put(Request::Inspect(Box::new(move |_| {
        entered.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
    })));
    receive.recv_timeout(Duration::from_secs(5)).unwrap();
    Release(Some(release))
}

fn error_message(error: DbError) -> String {
    match error {
        DbError::Sqlite { source: rusqlite::Error::SqliteFailure(_, Some(message)), .. } => message,
        other => panic!("expected SQLite statement error, received {other}"),
    }
}

#[test]
fn pending_rows_start_lazily_retain_owner_and_close_after_decode_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    set_item(&path, "fixture", "a", &1.into()).unwrap();
    let table = PendingTable::open(&path, "fixture").unwrap();
    let mut rows = table.rows().unwrap();
    table.assign("b", Some(&2.into())).unwrap();
    drop(table);
    assert_eq!(rows.next().unwrap().unwrap(), ("a".into(), 1.into()));
    assert_eq!(rows.next().unwrap().unwrap(), ("b".into(), 2.into()));
    assert!(rows.next().is_none());
    assert_eq!(read_table(&path, "fixture").unwrap().str_keys(), ["a"]);
}

#[test]
fn normal_close_error_retains_dictionary_connection_and_success_clears_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    let table = PendingTable::open(&path, "fixture").unwrap();
    table.execute_batch("CREATE TRIGGER bad BEFORE INSERT ON fixture WHEN NEW.key='bad' BEGIN SELECT RAISE(FAIL,'close error'); END;").unwrap();
    table.assign("bad", Some(&1.into())).unwrap();
    let worker = table.inner.connection().unwrap();
    worker.inspect(|_| {});
    assert_eq!(error_message(table.close().unwrap_err()), "close error");
    assert!(table.inner.connection().is_ok());
    table.assign("good", Some(&2.into())).unwrap();
    assert_eq!(table.get("good").unwrap(), Some(2.into()));
    let mut rows = table.rows().unwrap();
    table.close().unwrap();
    assert!(matches!(table.get("good"), Err(DbError::Closed { .. })));
    assert!(matches!(rows.next(), Some(Err(DbError::Closed { .. }))));
    assert!(rows.next().is_none());
    table.commit().unwrap();
    table.close().unwrap();
    assert!(read_table(&path, "fixture").unwrap().is_empty());
}

#[test]
fn deletion_tests_membership_without_decoding_and_queues_the_delete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    sqlitedict::tests::write_table(&path, "fixture", &[("opaque", &[255])]);
    let table = PendingTable::open(&path, "fixture").unwrap();
    assert!(matches!(table.assign("absent", None), Err(DbError::MissingKey { .. })));
    table.assign("opaque", None).unwrap();
    assert_eq!(table.get("opaque").unwrap(), None);
    assert!(read_item(&path, "fixture", "opaque").is_err(), "uncommitted delete leaves disk row");
    table.commit().unwrap();
    assert!(read_table(&path, "fixture").unwrap().is_empty());
}

#[test]
fn forced_drop_only_queues_close_and_leaves_pending_data_uncommitted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    let table = PendingTable::open(&path, "fixture").unwrap();
    let worker = table.inner.connection().unwrap();
    table.assign("pending", Some(&1.into())).unwrap();
    worker.inspect(|_| {});
    let gate = pause(&worker);
    drop(table);
    let join = worker.join.lock().unwrap().take().unwrap();
    let requests = worker.requests.clone();
    let state = worker.state.clone();
    drop(worker);
    assert!(!join.is_finished(), "force close must not join a blocked worker");
    assert!(matches!(requests.values.lock().unwrap().front(), Some(Request::Close(_))));
    assert!(state.0.lock().unwrap().error.is_none());
    drop(gate);
    join.join().unwrap();
    assert!(read_table(&path, "fixture").unwrap().is_empty());
}

#[test]
fn initialization_errors_are_delivered_once_without_waiting_for_sql_responses() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing").join("fixture.db");
    let worker = Worker::open(&path, false).unwrap();
    assert!(worker.check_error().is_err());
    worker.check_error().unwrap();
    let join = worker.join.lock().unwrap().take().unwrap();
    join.join().unwrap();
    // execute queues even after thread death once the saved error is cleared.
    worker.execute("SELECT 1".into(), vec![], false).unwrap();
    assert!(matches!(worker.requests.values.lock().unwrap().front(), Some(Request::Sql { .. })));
    assert!(!path.exists());
}

#[test]
fn close_already_queued_can_close_database_then_raise_and_retain_dead_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    let table = PendingTable::open(&path, "fixture").unwrap();
    table.execute_batch("CREATE TRIGGER bad BEFORE INSERT ON fixture WHEN NEW.key='bad' BEGIN SELECT RAISE(FAIL,'queued close error'); END;").unwrap();
    let worker = table.inner.connection().unwrap();
    let gate = pause(&worker);
    table.assign("bad", Some(&1.into())).unwrap();
    table.assign("good", Some(&2.into())).unwrap();
    let caller = PendingTable { inner: table.inner.clone() };
    let join = std::thread::spawn(move || caller.close());
    let mut values = worker.requests.values.lock().unwrap();
    while !values.iter().any(|request| matches!(request, Request::Close(_))) {
        let result = worker.requests.available.wait_timeout(values, Duration::from_secs(5)).unwrap();
        assert!(!result.1.timed_out(), "normal close was never enqueued");
        values = result.0;
    }
    drop(values);
    drop(gate);
    assert_eq!(error_message(join.join().unwrap().unwrap_err()), "queued close error");
    assert!(table.inner.connection().is_ok(), "SqliteDict.conn clears only after successful close");
    worker.join.lock().unwrap().take().unwrap().join().unwrap();
    assert!(read_table(&path, "fixture").unwrap().is_empty(), "closed worker rolled back pending good row");
    worker.check_error().unwrap();
    // Further blocking calls would wait on the dead worker.
}

#[test]
fn autocommit_normal_close_checks_error_at_final_commit_and_retains_connection_on_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    let table = AutocommitTable::open(&path, "fixture").unwrap();
    let worker = table.inner.connection().unwrap();
    worker.execute("SELECT missing_column FROM fixture".into(), vec![], false).unwrap();
    worker.inspect(|_| {});
    assert!(table.close().is_err());
    assert!(table.inner.connection().is_ok());
    table.set("good", &7.into()).unwrap();
    assert_eq!(read_item(&path, "fixture", "good").unwrap(), Some(7.into()));
    table.close().unwrap();
    assert!(matches!(table.get("good"), Err(DbError::Closed { .. })));
    table.close().unwrap();
}

#[test]
fn autocommit_delete_and_buffered_iterator_retain_native_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    sqlitedict::tests::write_table(&path, "fixture", &[("opaque", &[255])]);
    let table = AutocommitTable::open(&path, "fixture").unwrap();
    assert!(table.contains("opaque").unwrap());
    assert!(matches!(table.delete("absent"), Err(DbError::MissingKey { .. })));
    table.delete("opaque").unwrap();
    assert!(!table.contains("opaque").unwrap());
    assert_eq!(read_item(&path, "fixture", "opaque").unwrap(), None);
    table.set("a", &1.into()).unwrap();
    table.set("b", &2.into()).unwrap();
    let mut rows = table.rows().unwrap();
    assert_eq!(rows.next().unwrap().unwrap(), ("a".into(), 1.into()));
    table.close().unwrap();
    assert_eq!(
        rows.next().unwrap().unwrap(),
        ("b".into(), 2.into()),
        "already buffered rows remain usable after dictionary close"
    );
    assert!(rows.next().is_none());
}

#[test]
fn closing_dictionary_keeps_connection_accessible_until_worker_finishes() {
    for autocommit in [false, true] {
        closing_connection_remains_accessible(autocommit);
    }
}

fn closing_connection_remains_accessible(autocommit: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.db");
    let table = TableInner::open(&path, "fixture", autocommit).unwrap();
    let worker = table.connection().unwrap();
    let gate = pause(&worker);
    let closing = table.clone();
    let close = std::thread::spawn(move || closing.close(autocommit));
    let mut requests = worker.requests.values.lock().unwrap();
    while !requests.iter().any(|request| {
        if autocommit {
            matches!(request, Request::Commit(_))
        } else {
            matches!(request, Request::Close(_))
        }
    }) {
        let result = worker.requests.available.wait_timeout(requests, Duration::from_secs(5)).unwrap();
        assert!(!result.1.timed_out(), "close request was never enqueued");
        requests = result.0;
    }
    drop(requests);

    // Close leaves the connection reference visible while its worker close waits.
    // Reading that reference must not wait for the worker or dictionary close.
    let reading = table.clone();
    let (send, receive) = mpsc::channel();
    let read = std::thread::spawn(move || send.send(reading.connection().is_ok()).unwrap());
    let accessible = receive.recv_timeout(Duration::from_millis(250));
    drop(gate);
    close.join().unwrap().unwrap();
    read.join().unwrap();
    assert_eq!(accessible, Ok(true), "connection reference was locked behind worker shutdown");
    assert!(matches!(table.connection(), Err(DbError::Closed { .. })));
}

#[test]
fn cursor_prefetch_failure_discards_current_row_and_terminates_worker_iteration() {
    let worker = Worker::open(Path::new(":memory:"), false).unwrap();
    worker.check_error().unwrap();
    let responses = Arc::new(Queue::new());
    // Execution successfully steps row1. Advancing fetches row1,
    // then fails stepping row2 and discards row1 before the worker loop sees it.
    // abs(MIN_I64) raises the deterministic SQLite integer-overflow error.
    worker.requests.put(Request::Sql {
        sql: "SELECT CASE column1 WHEN 1 THEN 1 ELSE abs(-9223372036854775808) END FROM (VALUES(1),(2))".into(),
        args: vec![],
        transaction: false,
        response: Some(Arc::downgrade(&responses)),
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !worker.join.lock().unwrap().as_ref().unwrap().is_finished() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let stopped = worker.join.lock().unwrap().as_ref().unwrap().is_finished();
    let count = responses.values.lock().unwrap().len();
    let error = worker.check_error();
    if !stopped {
        worker.close().unwrap();
    } else {
        worker.join.lock().unwrap().take().unwrap().join().unwrap();
    }
    assert!(stopped, "iteration error must leave the native worker loop");
    assert_eq!(count, 0, "neither row1 nor End is delivered after prefetch failure");
    assert!(error.is_ok(), "iteration errors are not saved as execute errors");
}

#[test]
fn cursor_step_before_publication_preserves_successful_rows_and_first_step_errors() {
    let worker = Worker::open(Path::new(":memory:"), false).unwrap();
    let rows =
        worker.select("SELECT column1 FROM (VALUES(1),(2),(3))".into(), vec![]).collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    let error = worker.one("SELECT abs(-9223372036854775808)".into(), vec![]).unwrap_err();
    assert_eq!(error_message(error), "integer overflow");
    assert!(worker.check_error().is_ok());
    assert_eq!(worker.one("SELECT 9".into(), vec![]).unwrap(), Some(vec![Value::Integer(9)]));
    worker.close().unwrap();
}
