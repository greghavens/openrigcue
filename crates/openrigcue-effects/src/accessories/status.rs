//! Device status as shown in the sidebar and the connection a device
//! slot holds.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

/// A module parameter whose owner also performs device I/O.
#[derive(Debug, Clone, Default)]
pub(super) struct PublishedParameter<T>(Arc<RwLock<T>>);

impl<T> PublishedParameter<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Arc::new(RwLock::new(value)))
    }

    pub(super) fn set(&self, value: T) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = value;
    }
}

impl<T: Clone> PublishedParameter<T> {
    pub(super) fn get(&self) -> T {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The status parameter remains readable while the device slot is doing serial I/O.
#[derive(Debug, Clone)]
pub(super) struct PublishedStatus(Arc<RwLock<String>>);

impl Default for PublishedStatus {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(DeviceStatus::NotConnected.to_string())))
    }
}

impl PublishedStatus {
    pub(super) fn get(&self) -> String {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// A held port is a separate parameter, including while a slot probes or closes a device.
#[derive(Debug, Clone, Default)]
pub(super) struct PublishedPort(Arc<RwLock<String>>);

impl PublishedPort {
    pub(super) fn get(&self) -> String {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub(super) fn set(&self, value: &str) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = value.into();
    }
}

/// Why an accessory is not connected, with its display text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceStatus {
    #[default]
    NotConnected,
    /// Belt: the ESP32 answered but no drive did.
    MotorOffline,
    /// Belt / HaptiCR: firmware older than the shipped one; flashing is manual.
    FirmwareUpdateRequired,
    /// A progress text (Intellibox flashing: "Flashing new firmware", "Reconnecting", ...).
    Progress(String),
}

impl fmt::Display for DeviceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceStatus::NotConnected => f.write_str("Not Connected"),
            DeviceStatus::MotorOffline => f.write_str("Motor Offline"),
            DeviceStatus::FirmwareUpdateRequired => f.write_str("Firmware update required — flash manually"),
            DeviceStatus::Progress(text) => f.write_str(text),
        }
    }
}

/// A device slot's connection: the open device exists exactly while the slot is connected.
#[derive(Debug)]
pub enum Connection<D> {
    Offline(DeviceStatus),
    Connected(D),
}

impl<D> Default for Connection<D> {
    fn default() -> Self {
        Connection::Offline(DeviceStatus::NotConnected)
    }
}

impl<D> Connection<D> {
    /// Publish the parameter before releasing an old device (which can wait for its I/O thread).
    pub(super) fn set(&mut self, next: Self, status: &PublishedStatus) {
        *status.0.write().unwrap_or_else(PoisonError::into_inner) = next.to_string();
        *self = next;
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Connection::Connected(_))
    }

    pub fn device(&self) -> Option<&D> {
        match self {
            Connection::Connected(device) => Some(device),
            Connection::Offline(_) => None,
        }
    }

    pub fn device_mut(&mut self) -> Option<&mut D> {
        match self {
            Connection::Connected(device) => Some(device),
            Connection::Offline(_) => None,
        }
    }

    /// The offline status, `None` while connected.
    pub fn offline_status(&self) -> Option<&DeviceStatus> {
        match self {
            Connection::Offline(status) => Some(status),
            Connection::Connected(_) => None,
        }
    }
}

impl<D> fmt::Display for Connection<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Connection::Offline(status) => status.fmt(f),
            Connection::Connected(_) => f.write_str("Connected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn disconnected_status_is_published_before_device_drop_finishes() {
        struct WaitingDrop {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for WaitingDrop {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
        }
        let status = PublishedStatus::default();
        let (entered, waiting) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let mut connection = Connection::default();
        connection.set(Connection::Connected(WaitingDrop { entered, release: resume }), &status);
        let publisher = status.clone();
        let close =
            std::thread::spawn(move || connection.set(Connection::Offline(DeviceStatus::NotConnected), &publisher));
        let dropping = waiting.recv_timeout(Duration::from_secs(2));
        let (send, receive) = mpsc::channel();
        let read = std::thread::spawn(move || send.send(status.get()).unwrap());
        let shown = receive.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        close.join().unwrap();
        read.join().unwrap();
        assert!(dropping.is_ok(), "device drop reached its wait");
        assert_eq!(shown.unwrap(), "Not Connected");
    }
}
