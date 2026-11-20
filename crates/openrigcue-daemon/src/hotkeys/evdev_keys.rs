//! Keyboard events from `/dev/input/event*` for the keyboard hotkeys.
//!
//! A system-wide key event source for Linux. Devices are read
//! without `EVIOCGRAB`, so the desktop (X11 or Wayland) still receives every key; nothing is grabbed or injected.
//! Every second the scanner lists the devices and opens new keyboards (one reader thread each).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use evdev::{Device, EventSummary};
use rustix::event::{PollFd, PollFlags, Timespec};

use super::access::{keyboards, InputRoots};
use super::keyboard::KeyEvent;

/// How often devices are listed again.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Longest a reader waits before it checks for a stop request.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Key codes of the keyboard part of the key space (`BTN_MISC` and above are buttons).
const KEY_CODES_END: u16 = 0x100;

type Readers = Arc<Mutex<Vec<JoinHandle<()>>>>;

/// The scanner and keyboard readers. Dropping it stops them and waits for them.
pub struct KeyboardReaders {
    stop: Arc<AtomicBool>,
    scanner: Option<JoinHandle<()>>,
    readers: Readers,
}

impl KeyboardReaders {
    /// Starts the scanner; `events` receives the key events of every keyboard it opens. After every pass
    /// `denied` says whether no keyboard is being read and the system refused to open one for lack of
    /// permission.
    pub fn start(roots: InputRoots, events: Sender<KeyEvent>, denied: Arc<AtomicBool>) -> io::Result<KeyboardReaders> {
        let stop = Arc::new(AtomicBool::new(false));
        let readers = Readers::default();
        let (flag, spawned) = (stop.clone(), readers.clone());
        let scanner = thread::Builder::new()
            .name("hotkeys_scan".into())
            .spawn(move || scan_loop(&roots, &events, &flag, &spawned, &denied))?;
        Ok(KeyboardReaders { stop, scanner: Some(scanner), readers })
    }
}

impl Drop for KeyboardReaders {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(scanner) = self.scanner.take() {
            let _ = scanner.join();
        }
        let readers = std::mem::take(&mut *self.readers.lock().unwrap_or_else(PoisonError::into_inner));
        for reader in readers {
            let _ = reader.join();
        }
    }
}

fn scan_loop(
    roots: &InputRoots,
    events: &Sender<KeyEvent>,
    stop: &Arc<AtomicBool>,
    readers: &Readers,
    denied: &AtomicBool,
) {
    let open: Arc<Mutex<BTreeSet<PathBuf>>> = Arc::default();
    while !stop.load(Ordering::SeqCst) {
        let mut refused = false;
        for (path, _name) in keyboards(roots) {
            if open.lock().unwrap_or_else(PoisonError::into_inner).contains(&path) {
                continue;
            }
            // A node this user cannot open is tried again at the next scan.
            match Device::open(&path) {
                Ok(device) => start_reader(&path, device, events, stop, &open, readers),
                Err(error) => refused |= error.kind() == io::ErrorKind::PermissionDenied,
            }
        }
        // Keyboard access is unavailable while no keyboard is read and a keyboard was refused.
        let none_open = open.lock().unwrap_or_else(PoisonError::into_inner).is_empty();
        denied.store(refused && none_open, Ordering::SeqCst);
        let mut waited = Duration::ZERO;
        while waited < RESCAN_INTERVAL && !stop.load(Ordering::SeqCst) {
            thread::sleep(STOP_CHECK_INTERVAL);
            waited += STOP_CHECK_INTERVAL;
        }
    }
}

fn start_reader(
    path: &Path,
    device: Device,
    events: &Sender<KeyEvent>,
    stop: &Arc<AtomicBool>,
    open: &Arc<Mutex<BTreeSet<PathBuf>>>,
    readers: &Readers,
) {
    let name = device.name().unwrap_or_default().to_owned();
    open.lock().unwrap_or_else(PoisonError::into_inner).insert(path.to_path_buf());
    let (path, events, stop, open) = (path.to_path_buf(), events.clone(), stop.clone(), open.clone());
    tracing::info!(%name, path = %path.display(), "keyboard opened for hotkeys");
    let spawned = thread::Builder::new().name("hotkeys_keyboard".into()).spawn(move || {
        read_keys(device, &events, &stop);
        open.lock().unwrap_or_else(PoisonError::into_inner).remove(&path);
    });
    match spawned {
        Ok(reader) => {
            let mut readers = readers.lock().unwrap_or_else(PoisonError::into_inner);
            readers.retain(|reader| !reader.is_finished());
            readers.push(reader);
        }
        Err(error) => tracing::warn!(%error, "could not start a keyboard reader"),
    }
}

/// Forwards key events until the device goes away, the receiver is gone or a stop is requested.
fn read_keys(mut device: Device, events: &Sender<KeyEvent>, stop: &AtomicBool) {
    if device.set_nonblocking(true).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match wait_readable(&device, STOP_CHECK_INTERVAL) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        let batch: Vec<_> = match device.fetch_events() {
            Ok(batch) => batch.collect(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
        };
        for event in batch {
            if let EventSummary::Key(_, code, value) = event.destructure() {
                if code.0 < KEY_CODES_END && events.send(KeyEvent { code: code.0, value }).is_err() {
                    return;
                }
            }
        }
    }
}

fn wait_readable(device: &Device, timeout: Duration) -> io::Result<bool> {
    let timeout = Timespec::try_from(timeout).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    match rustix::event::poll(&mut [PollFd::new(device, PollFlags::IN)], Some(&timeout)) {
        Ok(ready) => Ok(ready > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a fake device tree in a temporary directory whose keyboard node cannot be opened as an input device
    /// (no real device is opened), the scanner runs and stops promptly when dropped.
    #[test]
    fn scanner_stops() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("dev")).unwrap();
        std::fs::write(tmp.path().join("dev/event0"), "").unwrap();
        let caps = tmp.path().join("sys/event0/device/capabilities");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(caps.join("key"), "fffffffffffffffe\n").unwrap();
        std::fs::write(caps.join("abs"), "0\n").unwrap();
        let roots = InputRoots { dev: tmp.path().join("dev"), sys: tmp.path().join("sys") };
        assert_eq!(keyboards(&roots).len(), 1);
        let (sender, _receiver) = std::sync::mpsc::channel();
        let denied = Arc::new(AtomicBool::new(false));
        let readers = KeyboardReaders::start(roots, sender, denied.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        drop(readers);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!denied.load(Ordering::SeqCst), "a node that is not an input device is not a refusal");
    }

    /// A fake keyboard node this user may not open (a plain file without permissions in a temporary directory;
    /// no real device is opened): the scanner reports that keyboard access is denied. Without keyboard nodes,
    /// and once the node can be opened as a file, it reports nothing.
    #[test]
    fn scanner_reports_refused_keyboards() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("dev")).unwrap();
        let roots = InputRoots { dev: tmp.path().join("dev"), sys: tmp.path().join("sys") };
        let wait_for = |denied: &AtomicBool, wanted: bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while denied.load(Ordering::SeqCst) != wanted && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            denied.load(Ordering::SeqCst)
        };

        // No keyboard at all: nothing was refused.
        let denied = Arc::new(AtomicBool::new(true));
        let (sender, _receiver) = std::sync::mpsc::channel();
        let readers = KeyboardReaders::start(roots.clone(), sender, denied.clone()).unwrap();
        assert!(!wait_for(&denied, false));
        drop(readers);

        let node = tmp.path().join("dev/event0");
        std::fs::write(&node, "").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).unwrap();
        let caps = tmp.path().join("sys/event0/device/capabilities");
        std::fs::create_dir_all(&caps).unwrap();
        std::fs::write(caps.join("key"), "fffffffffffffffe\n").unwrap();
        std::fs::write(caps.join("abs"), "0\n").unwrap();
        if std::fs::File::open(&node).is_ok() {
            // A user that file permissions do not bind (root) cannot be refused.
            return;
        }
        let denied = Arc::new(AtomicBool::new(false));
        let (sender, _receiver) = std::sync::mpsc::channel();
        let readers = KeyboardReaders::start(roots, sender, denied.clone()).unwrap();
        assert!(wait_for(&denied, true), "the refused keyboard is reported");
        // Readable now, though still not an input device: the next pass withdraws the report.
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!wait_for(&denied, false));
        drop(readers);
    }
}
