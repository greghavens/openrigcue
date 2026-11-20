//! Serial device add/remove events that wake the accessory connection scan, so it does not reopen every
//! port once a second.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Delay of the settle dispatch after the last device change.
pub const DEVICECHANGE_SETTLE: Duration = Duration::from_secs(1);
/// Delay of the follow-up dispatch after the last device change.
pub const DEVICECHANGE_FOLLOWUP: Duration = Duration::from_secs(4);

/// One device added or removed, as the udev monitor reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceChange {
    /// The device is a serial port (`tty` subsystem).
    pub serial: bool,
}

/// The wake-up state: the event flag and the pending timer dispatches.
#[derive(Debug, Default)]
struct Pending {
    raised: bool,
    /// The settle dispatch, re-armed by every event.
    settle: Option<Instant>,
    /// The follow-up dispatch, re-armed by every event.
    follow_up: Option<Instant>,
}

impl Pending {
    /// A device change seen at `now`: any device added or removed
    /// sets the event at once and restarts the settle timer
    /// and the follow-up timer.
    fn device_event(&mut self, _change: DeviceChange, now: Instant) {
        self.raised = true;
        self.settle = Some(now + DEVICECHANGE_SETTLE);
        self.follow_up = Some(now + DEVICECHANGE_FOLLOWUP);
    }

    /// Whether the event is set at `now`: raised, or a timer dispatch is due (which sets it). Due timers
    /// are consumed, so timers that fired together set the event once.
    fn take_due(&mut self, now: Instant) -> bool {
        let mut set = std::mem::take(&mut self.raised);
        for timer in [&mut self.settle, &mut self.follow_up] {
            if timer.is_some_and(|at| at <= now) {
                *timer = None;
                set = true;
            }
        }
        set
    }

    /// The next timer dispatch.
    fn next_timer(&self) -> Option<Instant> {
        match (self.settle, self.follow_up) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// A raised-until-cleared wake-up flag between the device watcher and the connection thread:
/// set by device events and timer dispatches, waited on and cleared by the connection scan.
#[derive(Debug, Default)]
pub struct ScanTrigger {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl ScanTrigger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the event.
    pub fn raise(&self) {
        self.pending.lock().unwrap().raised = true;
        self.wake.notify_all();
    }

    /// A device added or removed: sets the event and restarts both timers.
    pub fn device_event(&self, change: DeviceChange) {
        self.pending.lock().unwrap().device_event(change, Instant::now());
        self.wake.notify_all();
    }

    /// Waits for the event, then clears it: returns at once when already set, otherwise when
    /// set (by an event or a due timer dispatch) or after `timeout` (`None` waits for an event).
    /// Returns whether it was set; `false` means the timeout passed.
    pub fn wait_and_clear(&self, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut pending = self.pending.lock().unwrap();
        loop {
            let now = Instant::now();
            if pending.take_due(now) {
                return true;
            }
            if deadline.is_some_and(|d| d <= now) {
                return false;
            }
            let until = match (deadline, pending.next_timer()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            pending = match until {
                Some(at) => self.wake.wait_timeout(pending, at.saturating_duration_since(now)).unwrap().0,
                None => self.wake.wait(pending).unwrap(),
            };
        }
    }
}

/// A udev monitor message's `ACTION` and `SUBSYSTEM` properties.
///
/// The udev netlink format: the `libudev\0` prefix, the magic 0xfeedcafe (big-endian), then the host-endian
/// offset and length of the `KEY=VALUE\0` properties.
fn udev_properties(message: &[u8]) -> Option<(&[u8], &[u8])> {
    const PREFIX: &[u8] = b"libudev\0";
    const MAGIC: u32 = 0xfeed_cafe;
    let word = |at: usize| message.get(at..at + 4).map(|b| [b[0], b[1], b[2], b[3]]);
    if !message.starts_with(PREFIX) || word(8).map(u32::from_be_bytes) != Some(MAGIC) {
        return None;
    }
    let offset = word(16).map(u32::from_ne_bytes)? as usize;
    let length = word(20).map(u32::from_ne_bytes)? as usize;
    let properties = message.get(offset..offset.saturating_add(length))?;
    let value = |key: &[u8]| {
        properties.split(|b| *b == 0).find_map(|entry| entry.strip_prefix(key).and_then(|rest| rest.strip_prefix(b"=")))
    };
    Some((value(b"ACTION")?, value(b"SUBSYSTEM").unwrap_or_default()))
}

/// The device added or removed that a udev monitor message reports (`None` for other actions and other
/// messages).
///
/// Only devices being added or removed count; `serial` marks the `tty`
/// subsystem (the serial ports).
pub fn device_change(message: &[u8]) -> Option<DeviceChange> {
    let (action, subsystem) = udev_properties(message)?;
    matches!(action, b"add" | b"remove").then_some(DeviceChange { serial: subsystem == b"tty" })
}

/// Watch udev's netlink broadcasts and report every device added or removed to `trigger`, which applies the
/// timer rules.
///
/// The watcher reads a netlink socket with a 1 s timeout until stopped.
#[cfg(target_os = "linux")]
pub fn watch_udev(
    trigger: std::sync::Arc<ScanTrigger>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;

    use nix::sys::socket::{
        bind, recv, setsockopt, socket, sockopt, AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType,
    };
    use nix::sys::time::{TimeVal, TimeValLike};
    use tracing::debug;

    /// Multicast group of udevd's processed events (libudev's `UDEV_MONITOR_UDEV`).
    const UDEV_GROUP: u32 = 2;
    let fd =
        socket(AddressFamily::Netlink, SockType::Datagram, SockFlag::SOCK_CLOEXEC, SockProtocol::NetlinkKObjectUEvent)?;
    bind(fd.as_raw_fd(), &NetlinkAddr::new(0, UDEV_GROUP))?;
    setsockopt(&fd, sockopt::ReceiveTimeout, &TimeVal::seconds(1))?;
    std::thread::Builder::new().name("usb_monitor".into()).spawn(move || {
        let mut buffer = vec![0u8; 16 * 1024];
        while !stop.load(Ordering::SeqCst) {
            match recv(fd.as_raw_fd(), &mut buffer, MsgFlags::empty()) {
                Ok(n) => {
                    if let Some(change) = device_change(&buffer[..n]) {
                        debug!(serial = change.serial, "usb_device_monitor: device event");
                        trigger.device_event(change);
                    }
                }
                Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
                Err(e) => {
                    debug!("usb_device_monitor linux serial watch loop failed: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A udev monitor message as udevd broadcasts it (32-byte header in host order after the magic).
    fn udev_message(properties: &[&str]) -> Vec<u8> {
        let mut props = Vec::new();
        for p in properties {
            props.extend_from_slice(p.as_bytes());
            props.push(0);
        }
        let mut m = b"libudev\0".to_vec();
        m.extend_from_slice(&0xfeed_cafeu32.to_be_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&(props.len() as u32).to_ne_bytes());
        m.resize(40, 0);
        m.extend(props);
        m
    }

    /// Only udev messages of a device added or removed are device changes; `serial` marks the tty subsystem.
    #[test]
    fn udev_message_filter() {
        let serial = Some(DeviceChange { serial: true });
        assert_eq!(
            device_change(&udev_message(&["ACTION=add", "DEVPATH=/devices/x/ttyUSB0", "SUBSYSTEM=tty"])),
            serial
        );
        assert_eq!(device_change(&udev_message(&["SUBSYSTEM=tty", "ACTION=remove"])), serial);
        assert_eq!(device_change(&udev_message(&["ACTION=change", "SUBSYSTEM=tty"])), None);
        assert_eq!(
            device_change(&udev_message(&["ACTION=add", "SUBSYSTEMX=tty"])),
            Some(DeviceChange { serial: false })
        );
        // Kernel-format uevents (group 1) are not udev messages.
        assert_eq!(device_change(b"add@/devices/x/ttyUSB0\0ACTION=add\0SUBSYSTEM=tty\0"), None);
        let mut truncated = udev_message(&["ACTION=add", "SUBSYSTEM=tty"]);
        truncated.truncate(44);
        assert_eq!(device_change(&truncated), None);
    }

    /// Devices of every subsystem being added or removed are changes; `serial` marks the tty subsystem.
    #[test]
    fn device_change_of_any_subsystem() {
        assert_eq!(
            device_change(&udev_message(&["ACTION=add", "SUBSYSTEM=hidraw"])),
            Some(DeviceChange { serial: false })
        );
        assert_eq!(
            device_change(&udev_message(&["ACTION=remove", "SUBSYSTEM=tty"])),
            Some(DeviceChange { serial: true })
        );
        assert_eq!(device_change(&udev_message(&["ACTION=bind", "SUBSYSTEM=usb"])), None);
        assert_eq!(device_change(&udev_message(&["ACTION=change", "SUBSYSTEM=tty"])), None);
    }

    /// Any device change sets the event at once, again
    /// 1 s after the last change (settle) and 4 s after it (follow-up); a new change restarts both timers, and
    /// timers already due when the scan waits set it once.
    #[test]
    fn windows_watcher_dispatch() {
        let t0 = Instant::now();
        let s = |secs: f64| t0 + Duration::from_secs_f64(secs);
        let mut p = Pending::default();
        p.device_event(DeviceChange { serial: false }, t0);
        assert!(p.take_due(t0));
        assert_eq!(p.next_timer(), Some(s(1.0)));
        // A second change 0.5 s later restarts the settle and follow-up timers.
        p.device_event(DeviceChange { serial: true }, s(0.5));
        assert!(p.take_due(s(0.5)));
        assert!(!p.take_due(s(1.2)));
        assert_eq!(p.next_timer(), Some(s(1.5)));
        assert!(p.take_due(s(1.5)));
        assert_eq!(p.next_timer(), Some(s(4.5)));
        assert!(!p.take_due(s(4.0)));
        assert!(p.take_due(s(4.5)));
        assert_eq!(p.next_timer(), None);
        // Settle and follow-up both overdue: one wake.
        p.device_event(DeviceChange { serial: true }, t0);
        assert!(p.take_due(s(10.0)));
        assert!(!p.take_due(s(10.0)));
    }

    /// The trigger waits for a due timer dispatch, without an event.
    #[test]
    fn trigger_wakes_on_settle_timer() {
        let trigger = ScanTrigger::new();
        trigger.pending.lock().unwrap().settle = Some(Instant::now() + Duration::from_millis(20));
        let started = Instant::now();
        trigger.wait_and_clear(None);
        assert!(started.elapsed() >= Duration::from_millis(20));
        trigger.device_event(DeviceChange { serial: false });
        trigger.wait_and_clear(Some(Duration::from_secs(5)));
        assert!(trigger.pending.lock().unwrap().next_timer().is_some(), "settle and follow-up armed");
    }

    /// Event semantics: a raise before the wait is not lost; the wait clears it; the timeout
    /// returns without an event.
    #[test]
    fn trigger_wait_and_clear() {
        let trigger = Arc::new(ScanTrigger::new());
        trigger.raise();
        trigger.wait_and_clear(None);
        assert!(!trigger.pending.lock().unwrap().raised);
        trigger.wait_and_clear(Some(Duration::from_millis(5)));
        let waiter = {
            let trigger = Arc::clone(&trigger);
            std::thread::spawn(move || trigger.wait_and_clear(None))
        };
        trigger.raise();
        waiter.join().unwrap();
    }
}
