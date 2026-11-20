//! Serial transport behind a small trait so protocol code can be tested without hardware.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub(crate) fn record_open_result(path: &str, error: Option<&serialport::Error>) {
    let kind = error.map(|error| match error.kind() {
        serialport::ErrorKind::Io(kind) => kind,
        _ => std::io::ErrorKind::Other,
    });
    openrigcue_model::serial_access::record(path, kind);
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("serial port {path}: {source}")]
    Open { path: String, source: serialport::Error },
    #[error("serial i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("serial: {0}")]
    Port(#[from] serialport::Error),
}

/// How a port is opened. Always 8N1 without flow control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    pub baud_rate: u32,
    /// `None` blocks until the bytes are written.
    pub write_timeout: Option<Duration>,
}

/// Byte link to a device. Dropping it closes the port.
pub trait Link: Send {
    /// Write every byte.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError>;
    /// Return whatever has arrived, without blocking.
    fn read_available(&mut self) -> Result<Vec<u8>, LinkError>;
    /// A read with a `timeout`: blocks in the system read until bytes arrive or
    /// `timeout` has passed, then returns what arrived (nothing on a timeout). A link without a blocking read
    /// returns what is waiting at once.
    fn read_blocking(&mut self, timeout: Duration) -> Result<Vec<u8>, LinkError> {
        let _ = timeout;
        self.read_available()
    }
    /// Discard unread input.
    fn clear_input(&mut self) -> Result<(), LinkError>;
    fn name(&self) -> &str;
}

/// A closed port, still held by the object that opened it: every use fails with
/// a port-not-open error.
pub struct ClosedLink {
    name: String,
}

impl ClosedLink {
    pub fn new(name: &str) -> Self {
        ClosedLink { name: name.to_string() }
    }

    fn error() -> LinkError {
        std::io::Error::new(std::io::ErrorKind::NotConnected, "Attempting to use a port that is not open").into()
    }
}

impl Link for ClosedLink {
    fn write_all(&mut self, _bytes: &[u8]) -> Result<(), LinkError> {
        Err(Self::error())
    }
    fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
        Err(Self::error())
    }
    fn clear_input(&mut self) -> Result<(), LinkError> {
        Err(Self::error())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// A real serial port.
pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
    name: String,
    /// The limit of one whole `write` call; `None` blocks.
    write_timeout: Option<Duration>,
}

impl SerialLink {
    /// Open `path` 8N1 without flow control, with DTR asserted, exclusively (TIOCEXCL
    /// and an exclusive `flock`).
    pub fn open(path: &str, options: &OpenOptions) -> Result<Self, LinkError> {
        Self::open_with(path, options, true)
    }

    /// [`open`](Self::open) without the exclusive lock:
    /// a port another open object still holds can be opened again.
    pub fn open_shared(path: &str, options: &OpenOptions) -> Result<Self, LinkError> {
        Self::open_with(path, options, false)
    }

    fn open_with(path: &str, options: &OpenOptions, exclusive: bool) -> Result<Self, LinkError> {
        // The serialport crate has one timeout for reads and writes: it bounds writes, and `read_blocking` sets it for
        // its read and restores it. Linux clamps `Duration::MAX` to "forever".
        let timeout = options.write_timeout.unwrap_or(Duration::MAX);
        let result =
            serialport::new(path, options.baud_rate).timeout(timeout).dtr_on_open(true).exclusive(exclusive).open();
        #[cfg(target_os = "linux")]
        record_open_result(path, result.as_ref().err());
        let port = result.map_err(|source| LinkError::Open { path: path.to_string(), source })?;
        Ok(SerialLink { port, name: path.to_string(), write_timeout: options.write_timeout })
    }
}

/// The error of a write that ran out of time.
fn write_timeout_error() -> LinkError {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "Write timeout").into()
}

impl Link for SerialLink {
    /// With a `write_timeout`, the whole call (not each low-level write) has to finish within
    /// it, else it fails with a write timeout; without one it blocks until every byte is written.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        let Some(limit) = self.write_timeout else {
            self.port.write_all(bytes)?;
            return Ok(());
        };
        let deadline = Instant::now() + limit;
        let mut rest = bytes;
        while !rest.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(write_timeout_error());
            }
            self.port.set_timeout(remaining)?;
            match self.port.write(rest) {
                Ok(written) => rest = &rest[written..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Err(write_timeout_error()),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
        let waiting = usize::try_from(self.port.bytes_to_read()?).unwrap_or(usize::MAX);
        let mut buf = vec![0; waiting];
        if waiting > 0 {
            let read = self.port.read(&mut buf)?;
            buf.truncate(read);
        }
        Ok(buf)
    }

    fn read_blocking(&mut self, timeout: Duration) -> Result<Vec<u8>, LinkError> {
        self.port.set_timeout(timeout)?;
        let mut buf = vec![0; 256];
        let read = match self.port.read(&mut buf) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(e) => {
                let _ = self.port.set_timeout(self.write_timeout.unwrap_or(Duration::MAX));
                return Err(e.into());
            }
        };
        buf.truncate(read);
        // Writes without a `write_timeout` block, as the port was opened.
        self.port.set_timeout(self.write_timeout.unwrap_or(Duration::MAX))?;
        Ok(buf)
    }

    fn clear_input(&mut self) -> Result<(), LinkError> {
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(all(test, target_os = "linux"))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct DeniedFile(std::path::PathBuf);
    impl Drop for DeniedFile {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o600));
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Exercise each real open path on an unreadable regular file. The OS rejects
    /// it before any terminal ioctl, so this test never opens a controller.
    #[test]
    fn all_native_motion_port_openers_publish_permission_denial() {
        use crate::motion::actuon::{ActuonPorts, SystemActuonPorts};
        use crate::motion::prosimu::board::{BoardPorts, SystemBoardPorts};
        let path = std::env::temp_dir().join(format!(
            "openrigcue-denied-open-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        let file = DeniedFile(path);
        std::fs::set_permissions(&file.0, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&file.0).is_ok() {
            eprintln!("permission-denial check needs an unprivileged user");
            return;
        }
        let path = file.0.to_str().unwrap();
        type Open = fn(&str) -> Result<(), LinkError>;
        let openers: [(&str, Open); 3] = [
            ("shared", |path| {
                SerialLink::open(path, &OpenOptions { baud_rate: 115200, write_timeout: None }).map(|_| ())
            }),
            ("PROSIMU", |path| SystemBoardPorts.open(path).map(|_| ())),
            ("Actuon", |path| SystemActuonPorts.open(path).map(|_| ())),
        ];
        for (name, open) in openers {
            let _ = openrigcue_model::serial_access::take_denied();
            let error = open(path).expect_err("the file cannot be opened");
            assert!(
                matches!(error, LinkError::Open { source, .. }
                if source.kind() == serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)),
                "{name}"
            );
            assert!(
                openrigcue_model::serial_access::take_denied().iter().any(|denied| denied == path),
                "{name} must reach the device-access diagnostic"
            );
        }
    }
}
