//! Windows named pipes reached through the bridge's pipe relay.
//!
//! Two listeners serve named pipes (`\\.\pipe\FMTelemetry`, `\\.\pipe\DynamicCabPipe`) that the game
//! side connects to. Inside a Proton prefix such a pipe exists only in the prefix's wineserver, so the bridge owns
//! the Windows pipe server and relays each pipe client over its own stream connection to openrigcue
//! (`openrigcue-bridge` `windows::socket::Upstream::connect`):
//!
//! - first the Unix socket [`relay_socket_path`] (`$XDG_RUNTIME_DIR/openrigcue/pipe-<name>`), for Wine versions
//!   with AF_UNIX support;
//! - otherwise localhost TCP on [`RELAY_TCP_PORT`] (Wine 10 has no AF_UNIX, so this is the path under current
//!   Proton). One TCP port serves every pipe: the first frame of a TCP connection is the pipe name, and
//!   [`TcpRelay`] hands the connection to the pipe's [`RelayListener`].
//!
//! The bridge connects once per Windows pipe client and closes the connection when that client disconnects or a
//! `ReadFile` on its pipe instance fails (including `ERROR_MORE_DATA`). Every chunk in either direction is framed
//! as a little-endian `u32` length followed by the bytes: towards openrigcue one frame per successful `ReadFile`
//! with a 4096-byte buffer, towards the game one `WriteFile` per frame.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The buffer size of the bridge's `ReadFile` calls; a relay frame is never longer.
pub const MAX_CHUNK: usize = 4096;

/// Localhost TCP port the bridge relays to when the Unix socket cannot be reached (`openrigcue-bridge`
/// `relay::DEFAULT_RELAY_TCP_PORT`, its `--pipe-tcp-port` default). An OpenRigCue port, outside every
/// configured telemetry port.
pub const RELAY_TCP_PORT: u16 = 33010;

/// The address [`TcpRelay`] listens on by default: `127.0.0.1:33010`, where the bridge connects.
pub fn default_tcp_address() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, RELAY_TCP_PORT))
}

/// How long a new TCP connection may take to send its pipe-name frame.
const NAME_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause of the TCP accept loop when no connection is pending (it also bounds how late a stop is seen).
const TCP_ACCEPT_POLL: Duration = Duration::from_millis(100);

/// The runtime directory the relay sockets live under: `$XDG_RUNTIME_DIR` (the system temporary directory stands
/// in when `XDG_RUNTIME_DIR` is unset). The bridge resolves the same directory (`openrigcue-bridge`
/// `paths::relay_socket_path`).
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// Unix socket that relays the Windows pipe `\\.\pipe\<pipe_name>`: `$XDG_RUNTIME_DIR/openrigcue/pipe-<pipe_name>`
/// (see [`runtime_dir`]).
pub fn relay_socket_path(pipe_name: &str) -> PathBuf {
    relay_socket_path_in(&runtime_dir(), pipe_name)
}

/// The relay socket of `pipe_name` below the runtime directory `runtime_dir`:
/// `<runtime_dir>/openrigcue/pipe-<pipe_name>`.
pub fn relay_socket_path_in(runtime_dir: &Path, pipe_name: &str) -> PathBuf {
    runtime_dir.join("openrigcue").join(format!("pipe-{pipe_name}"))
}

/// Binds the relay socket of `pipe_name` below `runtime_dir`, replacing a stale socket left by an earlier run.
fn bind_unix(runtime_dir: &Path, pipe_name: &str) -> io::Result<UnixListener> {
    let path = relay_socket_path_in(runtime_dir, pipe_name);
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_socket()) {
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// One relayed pipe client: the bridge's connection over the Unix socket or over TCP.
#[derive(Debug)]
pub enum RelayStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for RelayStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            RelayStream::Unix(s) => s.read(buf),
            RelayStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for RelayStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            RelayStream::Unix(s) => s.write(buf),
            RelayStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            RelayStream::Unix(s) => s.flush(),
            RelayStream::Tcp(s) => s.flush(),
        }
    }
}

/// The pipes the TCP relay can hand connections to, by pipe name. Each entry carries the id of the listener
/// that registered it, so a listener dropped after a newer one took the name leaves the newer one in place.
type Routes = HashMap<String, (u64, Sender<TcpStream>)>;

fn routes() -> MutexGuard<'static, Routes> {
    static ROUTES: OnceLock<Mutex<Routes>> = OnceLock::new();
    ROUTES.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// Routes the TCP relay connections named `name` to the returned receiver (replacing an earlier route of that
/// name), with the route's id.
fn register_route(name: &str) -> (u64, Receiver<TcpStream>) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let route_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    routes().insert(name.to_owned(), (route_id, sender));
    (route_id, receiver)
}

/// A route on the shared [`TcpRelay`] only (no Unix socket), for the life of the process: the bridges' control
/// connections ([`crate::shared_memory::held::CONTROL_ROUTE`]).
pub(crate) fn tcp_route(name: &str) -> Receiver<TcpStream> {
    register_route(name).1
}

/// Where the relay connections of one pipe arrive: its Unix socket and its route on the shared [`TcpRelay`].
pub(crate) struct RelayListener {
    pipe_name: String,
    route_id: u64,
    unix: Option<UnixListener>,
    tcp: Receiver<TcpStream>,
}

impl RelayListener {
    /// Listens for the relay of `pipe_name` on both transports the bridge uses: the Unix socket below
    /// `runtime_dir` (normally [`runtime_dir()`]) and the TCP route. When the Unix socket cannot be bound (it is
    /// logged) the pipe is still reached over TCP, the transport of current Wine.
    pub(crate) fn bind(runtime_dir: &Path, pipe_name: &str) -> RelayListener {
        let unix = bind_unix(runtime_dir, pipe_name)
            .map_err(|error| tracing::warn!(pipe = pipe_name, %error, "pipe relay Unix socket unavailable; TCP only"))
            .ok();
        let (route_id, tcp) = register_route(pipe_name);
        RelayListener { pipe_name: pipe_name.to_owned(), route_id, unix, tcp }
    }

    /// The next relayed client, without waiting: the Unix socket first, then the TCP connections routed to this
    /// pipe. The stream is blocking.
    pub(crate) fn accept(&self) -> io::Result<Option<RelayStream>> {
        if let Some(unix) = &self.unix {
            match unix.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    return Ok(Some(RelayStream::Unix(stream)));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        match self.tcp.try_recv() {
            Ok(stream) => Ok(Some(RelayStream::Tcp(stream))),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }
}

impl Drop for RelayListener {
    fn drop(&mut self) {
        let mut routes = routes();
        if routes.get(&self.pipe_name).is_some_and(|(id, _)| *id == self.route_id) {
            routes.remove(&self.pipe_name);
        }
    }
}

/// Whether the TCP relay has a route for `pipe_name` right now.
#[cfg(test)]
pub(crate) fn is_routed(pipe_name: &str) -> bool {
    routes().contains_key(pipe_name)
}

/// Reads the pipe-name frame of a new TCP relay connection and hands the connection to that pipe's listener.
/// A connection that names no listening pipe, or sends no valid name in time, is closed.
fn route_connection(mut stream: TcpStream) {
    let name = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(NAME_FRAME_TIMEOUT)))
        .and_then(|()| read_chunk(&mut stream))
        .and_then(|frame| frame.ok_or_else(|| io::ErrorKind::UnexpectedEof.into()))
        .and_then(|frame| String::from_utf8(frame).map_err(|_| io::ErrorKind::InvalidData.into()));
    let name = match name.and_then(|name| stream.set_read_timeout(None).map(|()| name)) {
        Ok(name) => name,
        Err(error) => {
            tracing::debug!(%error, "pipe relay TCP connection without a pipe name");
            return;
        }
    };
    let delivered = routes().get(&name).is_some_and(|(_, sender)| sender.send(stream).is_ok());
    if !delivered {
        tracing::debug!(pipe = name, "pipe relay TCP connection for a pipe nobody serves");
    }
}

/// The TCP side of the pipe relay: one localhost listener for every pipe (the bridge's fallback when Wine has
/// no AF_UNIX). Connections are routed by their first frame, the pipe name, to the [`RelayListener`] of that
/// pipe in this process.
pub struct TcpRelay {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TcpRelay {
    /// Binds `address` (normally [`default_tcp_address`]) and starts routing connections.
    pub fn start(address: SocketAddr) -> io::Result<TcpRelay> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::Builder::new().name("pipe_relay_tcp".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        if let Err(error) = thread::Builder::new()
                            .name("pipe_relay_name".into())
                            .spawn(move || route_connection(stream))
                        {
                            tracing::warn!(%error, "pipe relay TCP connection not routed");
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TCP_ACCEPT_POLL),
                    Err(error) => {
                        tracing::warn!(%error, "pipe relay TCP accept failed");
                        thread::sleep(TCP_ACCEPT_POLL);
                    }
                }
            }
        })?;
        Ok(TcpRelay { address, stop, handle: Some(handle) })
    }

    /// The address actually bound.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Stops accepting; connections already handed to a pipe stay with it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TcpRelay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reads one relay frame; `Ok(None)` when the bridge closed the connection between frames.
pub(crate) fn read_chunk(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_CHUNK {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "relay frame longer than the pipe read buffer"));
    }
    let mut chunk = vec![0; len];
    stream.read_exact(&mut chunk)?;
    Ok(Some(chunk))
}

/// Writes one relay frame (one `WriteFile` on the game side).
pub(crate) fn write_chunk(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    /// Relay contract for the Windows pipe `\\.\pipe\FMTelemetry`:
    /// `$XDG_RUNTIME_DIR/openrigcue/pipe-<name>`.
    #[test]
    fn path_follows_runtime_dir_contract() {
        assert_eq!(
            relay_socket_path_in(Path::new("/run/user/1000"), "FMTelemetry"),
            PathBuf::from("/run/user/1000/openrigcue/pipe-FMTelemetry")
        );
        assert_eq!(relay_socket_path("FMTelemetry"), relay_socket_path_in(&runtime_dir(), "FMTelemetry"));
    }

    /// The bridge's TCP fallback: `127.0.0.1:33010` (`relay::DEFAULT_RELAY_TCP_PORT`).
    #[test]
    fn tcp_address_is_the_bridge_default() {
        assert_eq!(default_tcp_address(), "127.0.0.1:33010".parse().unwrap());
    }

    /// Relay framing of one `ReadFile` / `WriteFile` chunk: little-endian `u32` length + bytes; a clean
    /// end of stream between frames is the client's disconnect.
    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        write_chunk(&mut wire, b"abc").unwrap();
        write_chunk(&mut wire, b"").unwrap();
        assert_eq!(wire[..4], [3, 0, 0, 0]);
        let mut reader = io::Cursor::new(wire);
        assert_eq!(read_chunk(&mut reader).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(read_chunk(&mut reader).unwrap(), Some(Vec::new()));
        assert_eq!(read_chunk(&mut reader).unwrap(), None);
    }

    /// Relay framing: a frame longer than the 4096-byte `ReadFile` buffer, or cut short, is an
    /// error.
    #[test]
    fn oversized_or_truncated_frames_fail() {
        let mut big = io::Cursor::new(((MAX_CHUNK + 1) as u32).to_le_bytes().to_vec());
        assert!(read_chunk(&mut big).is_err());
        let mut cut = io::Cursor::new(vec![5, 0, 0, 0, 1]);
        assert!(read_chunk(&mut cut).is_err());
        let mut half_header = io::Cursor::new(vec![5, 0]);
        assert!(read_chunk(&mut half_header).is_err());
    }

    /// Waits up to two seconds for the next relayed client of `listener`.
    fn accept_within(listener: &RelayListener) -> Option<RelayStream> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(stream) = listener.accept().unwrap() {
                return Some(stream);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// A pipe name of this test process only, so the test never takes a real pipe's TCP route.
    fn test_pipe(tag: &str) -> String {
        format!("OpenRigCueTest{tag}{}", std::process::id())
    }

    /// A runtime directory of this test only, so the test never binds a socket in the real `$XDG_RUNTIME_DIR`.
    fn test_runtime_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The bridge under Wine 10 (`Upstream::connect` falling back to TCP): it connects to the TCP relay, sends
    /// the pipe name as the first frame, then relays the pipe's frames; openrigcue's replies go back on the same
    /// connection. Two pipes share the port and each connection reaches the listener its name frame names.
    #[test]
    fn tcp_connections_are_routed_by_their_name_frame() {
        let relay = TcpRelay::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let (cab, fm) = (test_pipe("Cab"), test_pipe("Fm"));
        let runtime = test_runtime_dir();
        let cab_listener = RelayListener::bind(runtime.path(), &cab);
        let fm_listener = RelayListener::bind(runtime.path(), &fm);

        let mut fm_client = TcpStream::connect(relay.address()).unwrap();
        write_chunk(&mut fm_client, fm.as_bytes()).unwrap();
        write_chunk(&mut fm_client, b"{'speed': 12.5}").unwrap();
        let mut cab_client = TcpStream::connect(relay.address()).unwrap();
        cab_client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write_chunk(&mut cab_client, cab.as_bytes()).unwrap();
        write_chunk(&mut cab_client, &[3, 0, 1]).unwrap();

        let mut fm_stream = accept_within(&fm_listener).expect("FMTelemetry-like connection routed");
        assert_eq!(read_chunk(&mut fm_stream).unwrap(), Some(b"{'speed': 12.5}".to_vec()));
        let mut cab_stream = accept_within(&cab_listener).expect("DynamicCab-like connection routed");
        assert_eq!(read_chunk(&mut cab_stream).unwrap(), Some(vec![3, 0, 1]));
        write_chunk(&mut cab_stream, &[6, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(read_chunk(&mut cab_client).unwrap(), Some(vec![6, 0, 0, 0, 0, 0, 0, 0]));
        assert!(fm_listener.accept().unwrap().is_none(), "one connection per client");
        relay.stop();
    }

    /// A TCP connection naming a pipe nobody serves is closed (the bridge then retries), and a dropped listener
    /// no longer receives connections for its name.
    #[test]
    fn unknown_pipe_names_are_closed() {
        let relay = TcpRelay::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let name = test_pipe("Gone");
        let runtime = test_runtime_dir();
        drop(RelayListener::bind(runtime.path(), &name));
        let mut client = TcpStream::connect(relay.address()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write_chunk(&mut client, name.as_bytes()).unwrap();
        assert_eq!(read_chunk(&mut client).unwrap(), None, "closed by the relay");
        relay.stop();
    }

    /// Wine versions with AF_UNIX: the bridge reaches the pipe on its Unix socket, without a name frame. The
    /// socket's directory is created private to the user (mode 0700) and a stale socket is replaced.
    #[test]
    fn unix_socket_path_still_served() {
        let name = test_pipe("Unix");
        let runtime = test_runtime_dir();
        let path = relay_socket_path_in(runtime.path(), &name);
        drop(RelayListener::bind(runtime.path(), &name));
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket(), "stale socket of an earlier run");
        let listener = RelayListener::bind(runtime.path(), &name);
        let mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let mut client = UnixStream::connect(&path).unwrap();
        write_chunk(&mut client, b"hello").unwrap();
        let mut stream = accept_within(&listener).expect("Unix connection accepted");
        assert_eq!(read_chunk(&mut stream).unwrap(), Some(b"hello".to_vec()));
    }
}
