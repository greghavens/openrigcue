//! The Linux end of a pipe relay: a Unix stream socket through Wine's AF_UNIX support, or localhost TCP.

use std::io::{self, Read, Write};
use std::mem;
use std::net::{Ipv4Addr, TcpStream};
use std::path::Path;
use std::sync::Once;

use windows_sys::Win32::Networking::WinSock::{
    closesocket, connect, ioctlsocket, recv, send, shutdown, socket, WSAGetLastError, WSAStartup, AF_UNIX, FIONBIO,
    INVALID_SOCKET, MSG_PEEK, SD_SEND, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR, SOCK_STREAM, WSADATA,
    WSAEWOULDBLOCK,
};

use crate::paths::{relay_socket_path, wine_path};
use crate::relay::write_frame;

/// Where relays connect to.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// `$XDG_RUNTIME_DIR` (a Linux path), when set.
    runtime_dir: Option<String>,
    tcp_port: u16,
}

impl Upstream {
    pub fn from_env(tcp_port: u16) -> Upstream {
        Upstream { runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(), tcp_port }
    }

    /// Connect the relay of pipe `name`: the Unix socket when it can be reached, otherwise localhost TCP.
    pub fn connect(&self, pipe_name: &str) -> io::Result<Connection> {
        if let Some(dir) = &self.runtime_dir {
            let path = relay_socket_path(Path::new(dir), pipe_name);
            if let Ok(stream) = UnixStream::connect(&wine_path(&path.to_string_lossy())) {
                return Ok(Connection::Unix(stream));
            }
        }
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.tcp_port))?;
        stream.set_nodelay(true)?;
        write_frame(&mut stream, pipe_name.as_bytes())?;
        Ok(Connection::Tcp(stream))
    }
}

/// A connected relay socket.
pub enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Connection {
    /// No more frames to OpenRigCue: it sees the end of the stream (the client ended) and keeps its own end open
    /// until it drops the client.
    pub fn shutdown_write(&mut self) {
        match self {
            Connection::Unix(s) => s.shutdown_write(),
            Connection::Tcp(s) => {
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
        }
    }

    /// Whether OpenRigCue closed its end (or the connection failed), without waiting.
    pub fn peer_closed(&mut self) -> bool {
        match self {
            Connection::Unix(s) => s.peer_closed(),
            Connection::Tcp(s) => {
                if s.set_nonblocking(true).is_err() {
                    return true;
                }
                let mut byte = [0u8; 1];
                let closed = match s.peek(&mut byte) {
                    Ok(0) => true,
                    Ok(_) => false,
                    Err(error) => error.kind() != io::ErrorKind::WouldBlock,
                };
                let _ = s.set_nonblocking(false);
                closed
            }
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(s) => s.read(buf),
            Connection::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(s) => s.write(buf),
            Connection::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Unix(_) => Ok(()),
            Connection::Tcp(s) => s.flush(),
        }
    }
}

/// A Winsock AF_UNIX stream socket (Windows 10 1803+ API; Wine maps it to a host Unix socket).
pub struct UnixStream(SOCKET);

fn winsock_started() {
    static START: Once = Once::new();
    START.call_once(|| {
        // SAFETY: WSADATA is plain data that WSAStartup fills in; version 2.2.
        let mut data: WSADATA = unsafe { mem::zeroed() };
        unsafe { WSAStartup(0x0202, &mut data) };
    });
}

fn last_socket_error() -> io::Error {
    // SAFETY: reads the calling thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

impl UnixStream {
    /// Connect to the socket at `path` (a Windows path, which Wine translates).
    fn connect(path: &str) -> io::Result<UnixStream> {
        winsock_started();
        // SAFETY: SOCKADDR_UN is plain data; zero is a valid initial value.
        let mut address: SOCKADDR_UN = unsafe { mem::zeroed() };
        address.sun_family = AF_UNIX;
        let bytes = path.as_bytes();
        if bytes.len() >= address.sun_path.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
        }
        for (slot, &b) in address.sun_path.iter_mut().zip(bytes) {
            *slot = b as i8;
        }
        // SAFETY: plain socket creation.
        let s = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if s == INVALID_SOCKET {
            return Err(last_socket_error());
        }
        let stream = UnixStream(s);
        // SAFETY: `address` is a valid SOCKADDR_UN and the length passed is its size.
        let rc = unsafe {
            connect(stream.0, &address as *const SOCKADDR_UN as *const SOCKADDR, mem::size_of::<SOCKADDR_UN>() as i32)
        };
        if rc == SOCKET_ERROR {
            return Err(last_socket_error());
        }
        Ok(stream)
    }
}

impl UnixStream {
    fn shutdown_write(&mut self) {
        // SAFETY: shuts down the sending side of a socket this value owns.
        unsafe { shutdown(self.0, SD_SEND) };
    }

    fn peer_closed(&mut self) -> bool {
        let mut on: u32 = 1;
        // SAFETY: switches the owned socket to non-blocking mode; `on` outlives the call.
        if unsafe { ioctlsocket(self.0, FIONBIO, &mut on) } != 0 {
            return true;
        }
        let mut byte = [0u8; 1];
        // SAFETY: `byte` is writable for one byte.
        let n = unsafe { recv(self.0, byte.as_mut_ptr(), 1, MSG_PEEK) };
        // SAFETY: reads the calling thread's last Winsock error.
        let would_block = n == SOCKET_ERROR && unsafe { WSAGetLastError() } == WSAEWOULDBLOCK;
        let mut off: u32 = 0;
        // SAFETY: back to blocking mode.
        unsafe { ioctlsocket(self.0, FIONBIO, &mut off) };
        n == 0 || (n == SOCKET_ERROR && !would_block)
    }
}

/// Clamp a buffer length to what one Winsock call takes.
fn call_len(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is writable for the length passed.
        let n = unsafe { recv(self.0, buf.as_mut_ptr(), call_len(buf.len()), 0) };
        usize::try_from(n).map_err(|_| last_socket_error())
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is readable for the length passed.
        let n = unsafe { send(self.0, buf.as_ptr(), call_len(buf.len()), 0) };
        usize::try_from(n).map_err(|_| last_socket_error())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UnixStream {
    fn drop(&mut self) {
        // SAFETY: the socket is owned by this value and closed once.
        unsafe { closesocket(self.0) };
    }
}
