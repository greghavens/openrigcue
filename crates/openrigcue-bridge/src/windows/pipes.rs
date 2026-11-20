//! Named-pipe servers relayed to openrigcue (the DynamicCab and FMTelemetry pipes).
//!
//! A pipe exists exactly while the application runs: the servers
//! here create instances only while OpenRigCue's control connection is up ([`AppState`]), and when it ends the
//! instance waiting for a client is woken (by connecting to it) and closed. Each relayed client gets its own
//! connection to OpenRigCue, made once the game's client has connected.

use std::collections::HashSet;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_READMODE_MESSAGE, PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

use super::control::AppState;
use super::socket::{Connection, Upstream};
use super::wide;
use crate::catalog::{CreateFailure, PipeFraming, PipeSpec};
use crate::relay::{accept_result, read_result, relay_client, win32, Accept, ClientEnd, PipeEnd};

// The platform-neutral pipe rules in `relay` use their own copies of these codes.
const _: () = assert!(
    win32::ERROR_BROKEN_PIPE == ERROR_BROKEN_PIPE
        && win32::ERROR_PIPE_BUSY == ERROR_PIPE_BUSY
        && win32::ERROR_MORE_DATA == ERROR_MORE_DATA
        && win32::ERROR_PIPE_CONNECTED == ERROR_PIPE_CONNECTED
);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Names of the pipes whose server is waiting in `ConnectNamedPipe`.
fn listening() -> MutexGuard<'static, HashSet<&'static str>> {
    static LISTENING: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    lock(LISTENING.get_or_init(Mutex::default))
}

/// The application stopped: wake every server waiting for a client by connecting to its instance, so it closes
/// the instance.
pub fn wake_listening() {
    let names: Vec<&'static str> = listening().iter().copied().collect();
    for name in names {
        let path = wide(&format!("\\\\.\\pipe\\{name}"));
        // SAFETY: `path` is NUL-terminated and outlives the call; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: a valid handle just opened and owned by nobody else; closed when dropped.
            drop(unsafe { OwnedHandle::from_raw_handle(handle) });
        }
    }
}

/// Start one server thread per pipe.
pub fn spawn_servers(specs: &[PipeSpec], upstream: Upstream, app: Arc<AppState>) {
    for spec in specs {
        let spec = *spec;
        let upstream = upstream.clone();
        let app = Arc::clone(&app);
        let spawned =
            thread::Builder::new().name(format!("pipe-{}", spec.name)).spawn(move || serve(spec, upstream, app));
        if let Err(error) = spawned {
            tracing::warn!(pipe = spec.name, %error, "cannot start pipe server");
        }
    }
}

/// A client whose `ReadFile` failed: its instance stays open until OpenRigCue has drained and dropped it (closed
/// the relay connection): only clients that have ended and have nothing left to read are closed.
struct Ended {
    /// Closed when this entry is dropped.
    _pipe: PipeInstance,
    relay: Connection,
}

/// Serve one pipe for the rest of the run.
///
/// Two server loops, chosen by the pipe's [`PipeSpec`]:
/// - sequential: create an instance (a failure ends the thread), wait for a client, relay it, disconnect, close
///   the handle and pause before the next instance;
/// - concurrent: close ended clients, create an instance (pausing after `ERROR_PIPE_BUSY`), wait for a client,
///   then give it its own reader thread, or close the instance when `ConnectNamedPipe` returned 0; one listening
///   instance at a time.
fn serve(spec: PipeSpec, upstream: Upstream, app: Arc<AppState>) {
    let ended: Arc<Mutex<Vec<Ended>>> = Arc::default();
    loop {
        if !app.is_running() {
            lock(&ended).clear();
            app.wait_running();
        }
        close_drained(&ended);
        let mut pipe = match PipeInstance::create(&spec) {
            Ok(pipe) => pipe,
            Err(error) => match spec.create_failure {
                CreateFailure::EndServer => {
                    tracing::warn!(pipe = spec.name, %error, "CreateNamedPipe failed, pipe no longer served");
                    return;
                }
                CreateFailure::SleepWhenBusy(delay) => {
                    if error.raw_os_error() == Some(win32::ERROR_PIPE_BUSY as i32) {
                        thread::sleep(delay);
                    }
                    continue;
                }
            },
        };
        listening().insert(spec.name);
        let accepted = pipe.accept(&spec);
        listening().remove(spec.name);
        if !app.is_running() {
            if spec.disconnects_client {
                pipe.disconnect();
            }
            continue;
        }
        match accepted {
            Accept::Serve => {}
            Accept::CloseUnread => {
                tracing::info!(pipe = spec.name, "client connected before ConnectNamedPipe, instance closed unread");
                continue;
            }
            Accept::StopServing(error) => {
                tracing::warn!(pipe = spec.name, %error, "ConnectNamedPipe failed, pipe no longer served");
                return;
            }
        }
        tracing::info!(pipe = spec.name, "client connected");
        let relay = match upstream.connect(spec.name) {
            Ok(relay) => relay,
            Err(error) => {
                tracing::warn!(pipe = spec.name, %error, "OpenRigCue relay not reachable, client closed");
                if spec.disconnects_client {
                    pipe.disconnect();
                }
                continue;
            }
        };
        if spec.concurrent_clients {
            let ended = Arc::clone(&ended);
            let spawned = thread::Builder::new()
                .name(format!("pipe-{}-client", spec.name))
                .spawn(move || read_client(spec, pipe, relay, &ended));
            if let Err(error) = spawned {
                tracing::warn!(pipe = spec.name, %error, "cannot start pipe reader");
            }
            continue;
        }
        let mut relay = relay;
        let end = relay_client(&spec, &mut pipe, &mut relay);
        match end {
            Ok(ClientEnd::StopServing) => {
                tracing::warn!(pipe = spec.name, "packet shorter than its header, pipe no longer served");
                return;
            }
            Ok(ClientEnd::NextClient) => tracing::info!(pipe = spec.name, "client session ended"),
            Err(error) => tracing::info!(pipe = spec.name, %error, "client session ended"),
        }
        if spec.disconnects_client {
            pipe.disconnect();
        }
        drop(pipe);
        drop(relay);
        thread::sleep(spec.rearm_delay);
    }
}

/// Read messages until a `ReadFile` fails, then mark the client ended for OpenRigCue
/// (its side of the relay sees the end of the stream) and leave the instance open until OpenRigCue drops it.
fn read_client(spec: PipeSpec, mut pipe: PipeInstance, mut relay: Connection, ended: &Mutex<Vec<Ended>>) {
    match relay_client(&spec, &mut pipe, &mut relay) {
        Ok(_) => tracing::info!(pipe = spec.name, "client ended"),
        Err(error) => tracing::info!(pipe = spec.name, %error, "client ended"),
    }
    relay.shutdown_write();
    lock(ended).push(Ended { _pipe: pipe, relay });
}

/// Close the instances of ended clients OpenRigCue has dropped.
fn close_drained(ended: &Mutex<Vec<Ended>>) {
    lock(ended).retain_mut(|client| !client.relay.peer_closed());
}

/// One server instance of a named pipe; its handle is closed on drop.
struct PipeInstance(OwnedHandle);

fn pipe_mode(framing: PipeFraming) -> NAMED_PIPE_MODE {
    match framing {
        PipeFraming::Byte => PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
        PipeFraming::Message => PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
    }
}

impl PipeInstance {
    /// `CreateNamedPipeW(\\.\pipe\<name>, PIPE_ACCESS_DUPLEX, mode, instances, buffer, buffer, timeout, NULL)`.
    fn create(spec: &PipeSpec) -> io::Result<PipeInstance> {
        let name = wide(&format!("\\\\.\\pipe\\{}", spec.name));
        // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                pipe_mode(spec.framing),
                spec.max_instances,
                spec.buffer_size,
                spec.buffer_size,
                spec.default_timeout_ms,
                ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a valid pipe handle just created and owned by nobody else.
        Ok(PipeInstance(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    /// Block in `ConnectNamedPipe(pipe, NULL)` until a client connects, and decide by the pipe's
    /// [`crate::catalog::ConnectRule`] (see [`accept_result`]).
    fn accept(&mut self, spec: &PipeSpec) -> Accept {
        // SAFETY: synchronous ConnectNamedPipe on a live pipe handle.
        let succeeded = unsafe { ConnectNamedPipe(self.0.as_raw_handle(), ptr::null_mut()) } != 0;
        // SAFETY: reads the calling thread's last error.
        let error = if succeeded { 0 } else { unsafe { GetLastError() } };
        accept_result(spec.connect, succeeded, error)
    }

    /// `DisconnectNamedPipe`, called after each client of a pipe that disconnects its clients.
    fn disconnect(&mut self) {
        // SAFETY: disconnects the client of a live pipe handle (no effect if none is connected).
        unsafe { DisconnectNamedPipe(self.0.as_raw_handle()) };
    }
}

impl PipeEnd for PipeInstance {
    /// One synchronous `ReadFile` into `buf`; see [`read_result`] for how a failure ends the client.
    fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let mut read = 0u32;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: `buf` is writable for `len` bytes; synchronous read.
        let ok = unsafe { ReadFile(self.0.as_raw_handle(), buf.as_mut_ptr(), len, &mut read, ptr::null_mut()) };
        // SAFETY: reads the calling thread's last error.
        let error = if ok != 0 { 0 } else { unsafe { GetLastError() } };
        read_result(ok != 0, read, error)
    }

    /// Synchronous `WriteFile` calls until the whole reply is written.
    fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        let mut done = 0;
        while done < data.len() {
            let rest = &data[done..];
            let mut written = 0u32;
            let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
            // SAFETY: `rest` is readable for `len` bytes; synchronous write.
            let ok = unsafe { WriteFile(self.0.as_raw_handle(), rest.as_ptr(), len, &mut written, ptr::null_mut()) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            done += written as usize;
        }
        Ok(())
    }
}
