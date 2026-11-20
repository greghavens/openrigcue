//! Serial transport, port enumeration and time, behind traits so every driver runs against fakes in tests.

use chrono::Timelike;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// An open byte link to a device.
pub trait SerialLink: Send {
    /// Write the bytes; returns how many were written.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
    /// Read one byte, waiting at most the link's read timeout; `None` on timeout.
    fn read_byte(&mut self) -> io::Result<Option<u8>>;
    /// Everything that has arrived, without waiting.
    fn read_available(&mut self) -> io::Result<Vec<u8>>;
    /// Number of bytes waiting.
    fn bytes_waiting(&mut self) -> io::Result<usize>;
    /// A second handle on the same port, so a receive thread can read while another thread writes.
    fn try_clone(&self) -> io::Result<Box<dyn SerialLink>>;
}

/// Parameters for opening a port; always 8 data bits, no parity, one stop bit, no flow control, which is
/// what every accessory driver uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortSettings {
    pub baud_rate: u32,
    /// Read timeout; `None` blocks.
    pub read_timeout: Option<Duration>,
}

/// Opens serial ports by device path.
pub trait PortOpener {
    fn open(&self, device: &str, settings: PortSettings) -> io::Result<Box<dyn SerialLink>>;
}

/// One enumerated port: device node, human description and hardware identity string.
/// USB identities are strings; an unreadable PNP id remains `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortEntry {
    pub device: String,
    pub description: String,
    pub hwid: Option<String>,
}

/// Listing failures and raw identity errors are handled differently by the port scans.
#[derive(Debug, thiserror::Error)]
pub enum PortScanError {
    #[error(transparent)]
    Listing(#[from] io::Error),
    #[error("TypeError: argument of type 'NoneType' is not iterable")]
    MissingHardwareId,
}

impl PortEntry {
    /// Build the entry of a USB port (`USB VID:PID=VVVV:PPPP ...`), which substring tests such as
    /// `1A86:7523` are made against.
    pub fn usb(device: &str, vendor_id: u16, product_id: u16, serial: Option<&str>, product: Option<&str>) -> Self {
        let serial_part = serial.map(|s| format!(" SER={s}")).unwrap_or_default();
        PortEntry {
            device: device.to_string(),
            description: product.unwrap_or("n/a").to_string(),
            hwid: Some(format!("USB VID:PID={vendor_id:04X}:{product_id:04X}{serial_part}")),
        }
    }

    /// The `(vid, pid)` test: whether the identity contains `VVVV:PPPP`.
    pub fn hwid_contains(&self, id: &str) -> Result<bool, PortScanError> {
        self.hwid.as_ref().map(|hwid| hwid.contains(id)).ok_or(PortScanError::MissingHardwareId)
    }

    pub fn hwid_contains_any(&self, ids: &[&str]) -> Result<bool, PortScanError> {
        for id in ids {
            if self.hwid_contains(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The identity as text, used only for diagnostics and conversions.
    pub fn hwid_text(&self) -> &str {
        self.hwid.as_deref().unwrap_or("None")
    }
}

/// Lists the serial ports present, in enumeration order; `Err` when
/// the listing fails.
pub trait PortLister {
    fn ports(&self) -> io::Result<Vec<PortEntry>>;
}

/// Time source and sleeping, so connection procedures with multi-second waits can be tested instantly.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
    /// The local wall-clock time (no time zone) at microsecond resolution, which moves
    /// with clock and DST changes (some timers subtract two of these).
    fn local_now(&self) -> chrono::NaiveDateTime {
        local_now()
    }
    /// Wall-clock seconds since the epoch (moves when the clock is set).
    fn time(&self) -> f64 {
        system_time()
    }
}

/// Epoch seconds as a float: the nanosecond count is converted once.
pub fn system_time() -> f64 {
    let nanos = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    };
    if nanos % 1_000_000_000 == 0 {
        (nanos / 1_000_000_000) as f64
    } else {
        nanos as f64 / 1e9
    }
}

/// The seconds field of the difference normalized to days and seconds (0..86399; a negative
/// difference has -1 days and a large seconds field).
pub fn timedelta_seconds(later: chrono::NaiveDateTime, earlier: chrono::NaiveDateTime) -> i64 {
    let micros = (later - earlier).num_microseconds().expect("datetime range difference fits microseconds");
    micros.div_euclid(1_000_000).rem_euclid(86_400)
}

/// The local time without a time zone, truncated to microseconds.
pub fn local_now() -> chrono::NaiveDateTime {
    let now = chrono::Local::now().naive_local();
    now.with_nanosecond(now.nanosecond() / 1_000 * 1_000).unwrap_or(now)
}

/// The difference in seconds: the microsecond count divided once by 10^6.
pub fn total_seconds(later: chrono::NaiveDateTime, earlier: chrono::NaiveDateTime) -> f64 {
    let micros = (later - earlier).num_microseconds().expect("datetime range difference fits microseconds");
    openrigcue_model::native_arithmetic::integer_true_divide(&micros.into(), &1_000_000.into())
        .expect("non-zero divisor")
}

/// Wall-clock time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Whether `error` says the port's device is gone (USB bridge unplugged) or the port is closed.
/// On Linux the
/// `serialport` crate gives `BrokenPipe` for a hung-up or invalid descriptor (`POLLHUP`/`POLLNVAL`) and
/// passes `EIO`, `ENXIO`, `ENODEV` and `EBADF` through as their text.
pub fn is_port_gone(error: &io::Error) -> bool {
    const GONE: [i32; 4] = [5, 6, 9, 19]; // EIO, ENXIO, EBADF, ENODEV
    if error.kind() == io::ErrorKind::BrokenPipe || error.raw_os_error().is_some_and(|code| GONE.contains(&code)) {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        use nix::errno::Errno;
        let text = error.to_string();
        [Errno::EIO, Errno::ENXIO, Errno::EBADF, Errno::ENODEV].iter().any(|e| text == e.desc())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Real serial ports through the `serialport` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPorts;

/// Upper bound used for "blocking" reads; the `serialport` crate needs a finite timeout.
const BLOCKING_READ: Duration = Duration::from_secs(3600);

struct SerialPortLink {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialLink for SerialPortLink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.port.write_all(bytes)?;
        self.port.flush()?;
        Ok(bytes.len())
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        match self.port.read(&mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_available(&mut self) -> io::Result<Vec<u8>> {
        let waiting = self.bytes_waiting()?;
        let mut buffer = vec![0u8; waiting];
        if waiting > 0 {
            let read = self.port.read(&mut buffer)?;
            buffer.truncate(read);
        }
        Ok(buffer)
    }

    fn bytes_waiting(&mut self) -> io::Result<usize> {
        Ok(self.port.bytes_to_read().map_err(io::Error::from)? as usize)
    }

    fn try_clone(&self) -> io::Result<Box<dyn SerialLink>> {
        Ok(Box::new(SerialPortLink { port: self.port.try_clone().map_err(io::Error::from)? }))
    }
}

impl PortOpener for SystemPorts {
    fn open(&self, device: &str, settings: PortSettings) -> io::Result<Box<dyn SerialLink>> {
        let result = serialport::new(device, settings.baud_rate)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(settings.read_timeout.unwrap_or(BLOCKING_READ))
            .open();
        #[cfg(target_os = "linux")]
        openrigcue_model::serial_access::record(
            device,
            result.as_ref().err().map(|error| match error.kind() {
                serialport::ErrorKind::Io(kind) => kind,
                _ => io::ErrorKind::Other,
            }),
        );
        let port = result.map_err(io::Error::from)?;
        Ok(Box::new(SerialPortLink { port }))
    }
}

impl PortLister for SystemPorts {
    fn ports(&self) -> io::Result<Vec<PortEntry>> {
        comports(Path::new("/dev"), Path::new("/sys/class/tty"))
    }
}

/// The device-node patterns of the port listing, globbed in this order.
const PORT_PATTERNS: [&str; 7] = ["ttyS", "ttyUSB", "ttyXRUSB", "ttyACM", "ttyAMA", "rfcomm", "ttyAP"];

/// The entries of `<dev>` starting with a prefix: the directory's entries in its own order, hidden names left
/// out, nothing when the directory cannot be read.
fn glob_prefix(dev: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dev) else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && name.starts_with(prefix))
        .map(|name| dev.join(name))
        .collect()
}

/// The first line of a sysfs file, trimmed; `None` where opening or reading fails.
/// Text that is not UTF-8 is an error.
fn read_line(path: &Path) -> io::Result<Option<String>> {
    let Ok(bytes) = std::fs::read(path) else { return Ok(None) };
    let text = String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let line = text.split(['\n', '\r']).next().unwrap_or_default();
    Ok(Some(super::numeric::python_strip(line).to_owned()))
}

/// Reads a hexadecimal integer: missing text is a type error, text that is not hexadecimal a value error
/// (accepted: white space, a sign, an optional `0x` prefix, single underscores between digits or after the prefix).
fn python_hex(text: Option<String>, path: &Path) -> io::Result<i64> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{what}: {}", path.display()));
    let text = text.ok_or_else(|| invalid("int() argument must be a string, not 'NoneType'"))?;
    let bad = || invalid("invalid literal for int() with base 16");
    let text = super::numeric::python_strip(&text);
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (prefixed, digits) = match unsigned.strip_prefix("0x").or_else(|| unsigned.strip_prefix("0X")) {
        Some(rest) => (true, rest),
        None => (false, unsigned),
    };
    let body = if prefixed { digits.strip_prefix('_').unwrap_or(digits) } else { digits };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return Err(bad());
    }
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    let value = i64::from_str_radix(&clean, 16).map_err(|_| bad())?;
    Ok(if negative { -value } else { value })
}

/// Formats `value` as upper-case hexadecimal, zero-padded to four characters after the sign.
fn hex4(value: i64) -> String {
    if value < 0 {
        format!("-{:03X}", value.unsigned_abs())
    } else {
        format!("{value:04X}")
    }
}

/// The last component of `path`.
fn basename(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One device's sysfs description and its subsystem.
fn sysfs_entry(device: &Path, tty_class: &Path) -> io::Result<(PortEntry, Option<String>)> {
    let name = basename(device);
    let link = std::fs::symlink_metadata(device).is_ok_and(|m| m.file_type().is_symlink());
    let mut entry = PortEntry {
        device: device.display().to_string(),
        description: "n/a".into(),
        hwid: Some(if link {
            format!("LINK={}", std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf()).display())
        } else {
            "n/a".into()
        }),
    };
    let class_device = tty_class.join(&name).join("device");
    let (device_path, subsystem) = match std::fs::canonicalize(&class_device) {
        Ok(path) => {
            let subsystem = std::fs::canonicalize(path.join("subsystem")).map(|p| basename(&p)).unwrap_or_else(|_| {
                // A path that cannot be resolved stays as it is.
                "subsystem".into()
            });
            (Some(path), Some(subsystem))
        }
        Err(_) => (None, None),
    };
    let interface_path = match (subsystem.as_deref(), &device_path) {
        (Some("usb-serial"), Some(path)) => path.parent().map(Path::to_path_buf),
        (Some("usb"), Some(path)) => Some(path.clone()),
        _ => None,
    };
    if let Some(interface_path) = interface_path {
        let usb_device = interface_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let interfaces = match read_line(&usb_device.join("bNumInterfaces"))? {
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
                ))
            }
            // An interface count that does not read counts as 1.
            Some(text) => super::numeric::python_int_value(&text).unwrap_or(super::numeric::PyInt::Small(1)),
        };
        let vid = python_hex(read_line(&usb_device.join("idVendor"))?, &usb_device)?;
        let pid = python_hex(read_line(&usb_device.join("idProduct"))?, &usb_device)?;
        let serial = read_line(&usb_device.join("serial"))?;
        let location = if interfaces.greater_than(1) { basename(&interface_path) } else { basename(&usb_device) };
        let product = read_line(&usb_device.join("product"))?;
        let interface = read_line(&interface_path.join("interface"))?;
        // USB description and identity strings.
        entry.description = match (&interface, &product) {
            (Some(interface), product) => format!("{} - {interface}", product.as_deref().unwrap_or("None")),
            (None, Some(product)) => product.clone(),
            (None, None) => name.clone(),
        };
        let serial_part = serial.map(|s| format!(" SER={s}")).unwrap_or_default();
        entry.hwid = Some(format!("USB VID:PID={}:{}{serial_part} LOCATION={location}", hex4(vid), hex4(pid)));
    } else if subsystem.as_deref() == Some("pnp") {
        entry.description = name.clone();
        // The identity is the device's `id` file.
        let path = device_path.as_deref().unwrap_or(Path::new("")).join("id");
        entry.hwid = read_line(&path)?;
    } else if subsystem.as_deref() == Some("amba") {
        entry.description = name.clone();
        entry.hwid = Some(device_path.as_deref().map(basename).unwrap_or_default());
    }
    if link {
        let target = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
        let hwid = entry.hwid.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unsupported operand type(s) for +=: 'NoneType' and 'str'")
        })?;
        hwid.push_str(&format!(" LINK={}", target.display()));
    }
    Ok((entry, subsystem))
}

/// The port listing over the device directory `dev` and the tty class directory
/// `tty_class` (`/sys/class/tty`): every device node of [`PORT_PATTERNS`], in glob order, with its sysfs
/// description, without the ports of the `platform` subsystem (built-in UARTs that are not present).
pub fn comports(dev: &Path, tty_class: &Path) -> io::Result<Vec<PortEntry>> {
    let mut ports = Vec::new();
    for prefix in PORT_PATTERNS {
        for device in glob_prefix(dev, prefix) {
            let (entry, subsystem) = sysfs_entry(&device, tty_class)?;
            if subsystem.as_deref() != Some("platform") {
                ports.push(entry);
            }
        }
    }
    Ok(ports)
}

#[cfg(test)]
pub(crate) mod fakes {
    //! Scripted serial links, port lists and a manual clock for driver tests.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// What a fake device has received and will answer.
    #[derive(Default)]
    pub struct FakeWire {
        pub written: Vec<u8>,
        pub incoming: VecDeque<u8>,
        /// Bytes that become readable only after the next write (a device answering a request).
        pub replies: VecDeque<Vec<u8>>,
        pub fail_writes: bool,
        /// Writes that still succeed before every further write fails (a device unplugged mid-sequence).
        pub writes_before_failure: Option<usize>,
        /// The error every read returns once nothing is left to read.
        pub read_error: Option<(io::ErrorKind, &'static str)>,
    }

    /// A link whose wire is shared with the test.
    #[derive(Clone, Default)]
    pub struct FakeLink(pub Arc<Mutex<FakeWire>>);

    impl FakeLink {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn push_incoming(&self, bytes: &[u8]) {
            self.0.lock().unwrap().incoming.extend(bytes);
        }
        pub fn queue_reply(&self, bytes: &[u8]) {
            self.0.lock().unwrap().replies.push_back(bytes.to_vec());
        }
        pub fn written(&self) -> Vec<u8> {
            self.0.lock().unwrap().written.clone()
        }
        pub fn take_written(&self) -> Vec<u8> {
            std::mem::take(&mut self.0.lock().unwrap().written)
        }
    }

    impl SerialLink for FakeLink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut wire = self.0.lock().unwrap();
            match wire.writes_before_failure {
                Some(0) => wire.fail_writes = true,
                Some(n) => wire.writes_before_failure = Some(n - 1),
                None => {}
            }
            if wire.fail_writes {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "unplugged"));
            }
            wire.written.extend_from_slice(bytes);
            if let Some(reply) = wire.replies.pop_front() {
                wire.incoming.extend(reply);
            }
            Ok(bytes.len())
        }
        fn read_byte(&mut self) -> io::Result<Option<u8>> {
            let mut wire = self.0.lock().unwrap();
            match (wire.incoming.pop_front(), wire.read_error) {
                (None, Some((kind, text))) => Err(io::Error::new(kind, text)),
                (byte, _) => Ok(byte),
            }
        }
        fn read_available(&mut self) -> io::Result<Vec<u8>> {
            Ok(self.0.lock().unwrap().incoming.drain(..).collect())
        }
        fn bytes_waiting(&mut self) -> io::Result<usize> {
            Ok(self.0.lock().unwrap().incoming.len())
        }
        fn try_clone(&self) -> io::Result<Box<dyn SerialLink>> {
            Ok(Box::new(self.clone()))
        }
    }

    /// Hands out pre-built links per device path and records what was opened.
    #[derive(Default)]
    pub struct FakeOpener {
        pub links: Mutex<Vec<(String, FakeLink)>>,
        pub opened: Mutex<Vec<(String, PortSettings)>>,
    }

    impl FakeOpener {
        pub fn with(device: &str, link: &FakeLink) -> Self {
            let opener = FakeOpener::default();
            opener.add(device, link);
            opener
        }
        pub fn add(&self, device: &str, link: &FakeLink) {
            self.links.lock().unwrap().push((device.to_string(), link.clone()));
        }
        pub fn opened(&self) -> Vec<(String, PortSettings)> {
            self.opened.lock().unwrap().clone()
        }
    }

    impl PortOpener for FakeOpener {
        fn open(&self, device: &str, settings: PortSettings) -> io::Result<Box<dyn SerialLink>> {
            self.opened.lock().unwrap().push((device.to_string(), settings));
            let links = self.links.lock().unwrap();
            links
                .iter()
                .find(|(d, _)| d == device)
                .map(|(_, l)| Box::new(l.clone()) as Box<dyn SerialLink>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, device.to_string()))
        }
    }

    /// A fixed port list.
    pub struct FakePorts(pub Vec<PortEntry>);

    impl PortLister for FakePorts {
        fn ports(&self) -> io::Result<Vec<PortEntry>> {
            Ok(self.0.clone())
        }
    }

    /// A clock that only moves when slept on.
    pub struct ManualClock {
        start: Instant,
        elapsed: Mutex<Duration>,
    }

    impl Default for ManualClock {
        fn default() -> Self {
            ManualClock { start: Instant::now(), elapsed: Mutex::new(Duration::ZERO) }
        }
    }

    impl ManualClock {
        pub fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }
        pub fn advance(&self, d: Duration) {
            *self.elapsed.lock().unwrap() += d;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.start + self.elapsed()
        }
        fn sleep(&self, duration: Duration) {
            self.advance(duration);
        }
        fn local_now(&self) -> chrono::NaiveDateTime {
            let epoch = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(12, 0, 0).unwrap();
            epoch + chrono::TimeDelta::microseconds(self.elapsed().as_micros() as i64)
        }
        fn time(&self) -> f64 {
            1_767_268_800.0 + self.elapsed().as_secs_f64()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The errors of an unplugged or closed port, as the serialport crate reports them on Linux.
    #[test]
    fn port_gone_errors() {
        assert!(is_port_gone(&io::Error::new(io::ErrorKind::BrokenPipe, "Broken pipe")));
        assert!(is_port_gone(&io::Error::from_raw_os_error(5)));
        assert!(is_port_gone(&io::Error::from_raw_os_error(19)));
        #[cfg(target_os = "linux")]
        assert!(is_port_gone(&io::Error::other(nix::errno::Errno::EIO.desc())));
        assert!(!is_port_gone(&io::Error::new(io::ErrorKind::TimedOut, "Operation timed out")));
        assert!(!is_port_gone(&io::Error::other("framing error")));
    }

    /// The port listing: the seven globs in order (each in directory order), USB
    /// details from sysfs (description, `LOCATION=` of the device or, with several interfaces, of the
    /// interface), `platform` ports hidden, other ports `n/a`; an unreadable `idVendor` is an error.
    #[test]
    fn linux_comports_like_pyserial() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("openrigcue-comports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("dev");
        let class = root.join("class");
        let devices = root.join("devices");
        let bus = root.join("bus");
        for d in [&dev, &class, &bus.join("usb-serial"), &bus.join("usb"), &bus.join("platform")] {
            std::fs::create_dir_all(d).unwrap();
        }
        for name in ["ttyACM0", "ttyUSB0", "ttyS0", "ttyS4", "tty0", ".ttyS9"] {
            std::fs::write(dev.join(name), b"").unwrap();
        }
        // ttyUSB0: an FTDI usb-serial port (tty under the interface, one interface).
        let ftdi = devices.join("usb1/1-2");
        let ftdi_if = ftdi.join("1-2:1.0");
        std::fs::create_dir_all(ftdi_if.join("ttyUSB0")).unwrap();
        symlink(bus.join("usb-serial"), ftdi_if.join("ttyUSB0/subsystem")).unwrap();
        for (file, text) in
            [("bNumInterfaces", " 1\n"), ("idVendor", "0403\n"), ("idProduct", "6001\n"), ("serial", "A1B2\n")]
        {
            std::fs::write(ftdi.join(file), text).unwrap();
        }
        std::fs::write(ftdi.join("product"), "FT232R USB UART\n").unwrap();
        std::fs::create_dir_all(class.join("ttyUSB0")).unwrap();
        symlink(ftdi_if.join("ttyUSB0"), class.join("ttyUSB0/device")).unwrap();
        // ttyACM0: a CDC-ACM interface of a two-interface device.
        let leo = devices.join("usb1/1-3");
        let leo_if = leo.join("1-3:1.0");
        std::fs::create_dir_all(&leo_if).unwrap();
        symlink(bus.join("usb"), leo_if.join("subsystem")).unwrap();
        for (file, text) in [("bNumInterfaces", " 2\n"), ("idVendor", "2341\n"), ("idProduct", "8036\n")] {
            std::fs::write(leo.join(file), text).unwrap();
        }
        std::fs::write(leo_if.join("interface"), "CDC ACM\n").unwrap();
        std::fs::create_dir_all(class.join("ttyACM0")).unwrap();
        symlink(&leo_if, class.join("ttyACM0/device")).unwrap();
        // ttyS0: a platform UART (hidden); ttyS4 has no device link (listed as n/a).
        let uart = devices.join("platform/serial8250/tty/ttyS0");
        std::fs::create_dir_all(&uart).unwrap();
        symlink(bus.join("platform"), uart.join("subsystem")).unwrap();
        std::fs::create_dir_all(class.join("ttyS0")).unwrap();
        symlink(&uart, class.join("ttyS0/device")).unwrap();

        let ports = comports(&dev, &class).unwrap();
        let shown: Vec<(String, String, String)> = ports
            .iter()
            .map(|p| (basename(Path::new(&p.device)), p.description.clone(), p.hwid_text().to_owned()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("ttyS4".into(), "n/a".into(), "n/a".into()),
                ("ttyUSB0".into(), "FT232R USB UART".into(), "USB VID:PID=0403:6001 SER=A1B2 LOCATION=1-2".into()),
                ("ttyACM0".into(), "None - CDC ACM".into(), "USB VID:PID=2341:8036 LOCATION=1-3:1.0".into()),
            ]
        );
        std::fs::remove_file(leo.join("idVendor")).unwrap();
        assert!(comports(&dev, &class).is_err(), "int(None, 16) raises out of comports()");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The USB identity string format, which the drivers' VID:PID substring tests depend on.
    #[test]
    fn usb_hwid_matches_pyserial_format() {
        let p = PortEntry::usb("/dev/ttyUSB3", 0x1a86, 0x7523, Some("ABC"), Some("USB Serial"));
        assert_eq!(p.hwid_text(), "USB VID:PID=1A86:7523 SER=ABC");
        assert!(p.hwid_contains("1A86:7523").unwrap());
        assert!(!p.hwid_contains("1a86:7523").unwrap());
        let q = PortEntry::usb("/dev/ttyACM0", 0x2341, 0x8036, None, None);
        assert_eq!(q.hwid_text(), "USB VID:PID=2341:8036");
        assert_eq!(q.description, "n/a");
    }

    #[test]
    fn native_pnp_sysfs_distinguishes_missing_empty_and_text_ids_and_alias_failure() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        let class = root.path().join("class");
        let pnp = root.path().join("pnp-device");
        let bus = root.path().join("pnp");
        for path in [&dev, &class.join("ttyS0"), &pnp, &bus] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(dev.join("ttyS0"), "").unwrap();
        symlink(&pnp, class.join("ttyS0/device")).unwrap();
        symlink(&bus, pnp.join("subsystem")).unwrap();
        let missing = comports(&dev, &class).unwrap().remove(0);
        assert_eq!(missing.hwid, None);
        assert_eq!(missing.hwid_text(), "None");
        assert!(matches!(missing.hwid_contains("1A86:"), Err(PortScanError::MissingHardwareId)));
        std::fs::create_dir(pnp.join("id")).unwrap();
        assert_eq!(comports(&dev, &class).unwrap()[0].hwid, None);
        std::fs::remove_dir(pnp.join("id")).unwrap();
        for text in ["", "None", "PNP0501", "USB VID:PID=1A86:7523"] {
            std::fs::write(pnp.join("id"), text).unwrap();
            let listed = comports(&dev, &class).unwrap().remove(0);
            assert_eq!(listed.hwid.as_deref(), Some(text));
            assert_eq!(listed.hwid_contains("1A86:7523").unwrap(), text.contains("1A86:7523"));
        }
        std::fs::remove_file(pnp.join("id")).unwrap();
        std::fs::remove_file(dev.join("ttyS0")).unwrap();
        std::fs::write(dev.join("target"), "").unwrap();
        symlink(dev.join("target"), dev.join("ttyS0")).unwrap();
        assert_eq!(
            comports(&dev, &class).unwrap_err().to_string(),
            "unsupported operand type(s) for +=: 'NoneType' and 'str'"
        );
    }
}
