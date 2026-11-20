//! The database worker thread: request/response queues and deferred-error boundaries.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};

use rusqlite::{params_from_iter, types::Value, Connection};

use super::{sqlite_error, DbError};

#[derive(Debug)]
struct Queue<T> {
    values: Mutex<VecDeque<T>>,
    available: Condvar,
}

impl<T> Queue<T> {
    fn new() -> Self {
        Self { values: Mutex::new(VecDeque::new()), available: Condvar::new() }
    }

    fn put(&self, value: T) {
        self.values.lock().unwrap_or_else(PoisonError::into_inner).push_back(value);
        self.available.notify_one();
    }

    fn get(&self) -> T {
        let mut values = self.values.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(value) = values.pop_front() {
                return value;
            }
            values = self.available.wait(values).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[derive(Debug)]
enum Response {
    Row(Vec<Value>),
    End,
}

type Responses = Weak<Queue<Response>>;

enum Request {
    Sql {
        sql: String,
        args: Vec<Value>,
        transaction: bool,
        response: Option<Responses>,
    },
    Commit(Option<Responses>),
    Close(Responses),
    // Used only by disposable schema/trigger fixtures, never by profile writes.
    Batch(String, Responses),
    #[cfg(test)]
    Inspect(Box<dyn FnOnce(&Connection) + Send>),
}

#[derive(Debug)]
struct State {
    ready: bool,
    error: Option<DbError>,
}

/// The producer owns the request queue even if the worker exits. Likewise a
/// waiting select owns its response queue. Worker death does not synthesize a
/// response/error or turn a blocked wait into a disconnected-channel error.
#[derive(Debug)]
pub(super) struct Worker {
    requests: Arc<Queue<Request>>,
    state: Arc<(Mutex<State>, Condvar)>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SQLite request")
    }
}

impl Worker {
    pub(super) fn open(path: &Path, autocommit: bool) -> Result<Arc<Self>, DbError> {
        let requests = Arc::new(Queue::new());
        let state = Arc::new((Mutex::new(State { ready: false, error: None }), Condvar::new()));
        let worker_requests = requests.clone();
        let worker_state = state.clone();
        let worker_path = path.to_owned();
        let join = thread::Builder::new()
            .name("openrigcue-sqlitedict".into())
            .spawn(move || run(&worker_path, autocommit, worker_requests, worker_state))
            .map_err(|source| DbError::Worker { path: path.display().to_string(), source })?;
        Ok(Arc::new(Self { requests, state, join: Mutex::new(Some(join)) }))
    }

    pub(super) fn check_error(&self) -> Result<(), DbError> {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !state.ready {
            state = ready.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        state.error.take().map_or(Ok(()), Err)
    }

    pub(super) fn execute(&self, sql: String, args: Vec<Value>, transaction: bool) -> Result<(), DbError> {
        self.check_error()?;
        self.requests.put(Request::Sql { sql, args, transaction, response: None });
        Ok(())
    }

    pub(super) fn select(self: &Arc<Self>, sql: String, args: Vec<Value>) -> Selection {
        Selection { worker: self.clone(), request: Some((sql, args)), responses: None, finished: false }
    }

    pub(super) fn one(self: &Arc<Self>, sql: String, args: Vec<Value>) -> Result<Option<Vec<Value>>, DbError> {
        self.select(sql, args).next().transpose()
    }

    pub(super) fn commit(&self, blocking: bool) -> Result<(), DbError> {
        self.check_error()?;
        if blocking {
            let responses = Arc::new(Queue::new());
            self.requests.put(Request::Commit(Some(Arc::downgrade(&responses))));
            let _response = responses.get();
            self.check_error()
        } else {
            self.requests.put(Request::Commit(None));
            Ok(())
        }
    }

    pub(super) fn close(&self) -> Result<(), DbError> {
        self.check_error()?;
        let responses = Arc::new(Queue::new());
        self.requests.put(Request::Close(Arc::downgrade(&responses)));
        let _response = responses.get();
        self.check_error()?;
        if let Some(join) = self.join.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = join.join();
        }
        Ok(())
    }

    pub(super) fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.check_error()?;
        let responses = Arc::new(Queue::new());
        self.requests.put(Request::Batch(sql.to_owned(), Arc::downgrade(&responses)));
        let _response = responses.get();
        self.check_error()
    }

    #[cfg(test)]
    pub(super) fn inspect(&self, inspect: impl FnOnce(&Connection) + Send + 'static) {
        let (done, receive) = std::sync::mpsc::channel();
        self.requests.put(Request::Inspect(Box::new(move |connection| {
            inspect(connection);
            done.send(()).unwrap();
        })));
        receive.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A forced close skips the deferred-error check, queues close and does not join.
        self.requests.put(Request::Close(Weak::new()));
    }
}

#[derive(Debug)]
pub(super) struct Selection {
    worker: Arc<Worker>,
    request: Option<(String, Vec<Value>)>,
    responses: Option<Arc<Queue<Response>>>,
    finished: bool,
}

impl Iterator for Selection {
    type Item = Result<Vec<Value>, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let result = (|| {
            if let Some((sql, args)) = self.request.take() {
                self.worker.check_error()?;
                let responses = Arc::new(Queue::new());
                self.worker.requests.put(Request::Sql {
                    sql,
                    args,
                    transaction: false,
                    response: Some(Arc::downgrade(&responses)),
                });
                self.responses = Some(responses);
            }
            let response = self.responses.as_ref().expect("started select").get();
            // Each item is received before the deferred-error check, including End.
            self.worker.check_error()?;
            match response {
                Response::Row(row) => Ok(Some(row)),
                Response::End => Ok(None),
            }
        })();
        match result {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => {
                self.finished = true;
                self.responses = None;
                None
            }
            Err(error) => {
                self.finished = true;
                self.responses = None;
                Some(Err(error))
            }
        }
    }
}

fn put(response: &Responses, value: Response) -> bool {
    if let Some(queue) = response.upgrade() {
        queue.put(value);
        true
    } else {
        false
    }
}

fn run(path: &Path, autocommit: bool, requests: Arc<Queue<Request>>, shared: Arc<(Mutex<State>, Condvar)>) {
    let connection = super::native_connection(path);
    let (lock, ready) = &*shared;
    {
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.ready = true;
        if let Err(source) = &connection {
            tracing::error!(path = %path.display(), error = %source, "Failed to initialize SQLite connection");
        }
        // An initialization failure is recorded before the start lock is released.
        if let Err(source) = connection {
            state.error = Some(sqlite_error(path, source));
            ready.notify_all();
            return;
        }
        ready.notify_all();
    }
    let connection = connection.expect("connection failure returned above");
    loop {
        match requests.get() {
            Request::Close(response) => {
                // Commit/close failures terminate the
                // worker without recording an error or posting an End response.
                if connection.close().is_ok() {
                    put(&response, Response::End);
                }
                return;
            }
            Request::Commit(response) => {
                if !connection.is_autocommit() && connection.execute_batch("COMMIT").is_err() {
                    return;
                }
                if let Some(response) = response {
                    put(&response, Response::End);
                }
            }
            Request::Sql { sql, args, transaction, response } => {
                let result = execute(&connection, &sql, args, transaction && !autocommit, response.as_ref());
                match result {
                    Ok(()) => {}
                    Err(QueryError::Execute(source)) => {
                        tracing::error!(path = %path.display(), error = %source, "Inner SQLite exception; re-raised at next call");
                        lock.lock().unwrap_or_else(PoisonError::into_inner).error = Some(sqlite_error(path, source));
                    }
                    Err(QueryError::Iteration) => return,
                }
                if let Some(response) = response {
                    put(&response, Response::End);
                }
                if autocommit && !connection.is_autocommit() && connection.execute_batch("COMMIT").is_err() {
                    return;
                }
            }
            Request::Batch(sql, response) => {
                if let Err(source) = connection.execute_batch(&sql) {
                    lock.lock().unwrap_or_else(PoisonError::into_inner).error = Some(sqlite_error(path, source));
                }
                put(&response, Response::End);
            }
            #[cfg(test)]
            Request::Inspect(inspect) => inspect(&connection),
        }
    }
}

enum QueryError {
    Execute(rusqlite::Error),
    Iteration,
}

impl From<rusqlite::Error> for QueryError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Execute(value)
    }
}

fn execute(
    connection: &Connection,
    sql: &str,
    args: Vec<Value>,
    transaction: bool,
    response: Option<&Responses>,
) -> Result<(), QueryError> {
    let mut statement = connection.prepare(sql)?;
    if transaction && connection.is_autocommit() {
        connection.execute_batch("BEGIN DEFERRED")?;
    }
    let count = statement.column_count();
    let mut rows = statement.query(params_from_iter(args))?;
    // Executing a statement steps it once before returning; errors at this
    // boundary are saved as the deferred error.
    let mut current = rows.next().map_err(QueryError::Execute)?;
    let Some(response) = response else {
        return Ok(());
    };
    while let Some(row) = current {
        let values = (0..count)
            .map(|index| row.get(index))
            .collect::<Result<Vec<Value>, _>>()
            .map_err(|_| QueryError::Iteration)?;
        // Advancing fetches the current values, then steps the next
        // row before yielding. A failed step discards the current row and
        // ends the worker through its unhandled iteration-error path.
        current = rows.next().map_err(|_| QueryError::Iteration)?;
        if !put(response, Response::Row(values)) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
