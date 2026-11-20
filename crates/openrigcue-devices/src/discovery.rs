//! Serial port discovery and the port filters every controller scan applies.

use serde::Serialize;

/// A serial port found on the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortInfo {
    /// Device node, e.g. `/dev/ttyUSB0`.
    pub path: String,
    /// Stable alias when available, e.g. `/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0`.
    pub stable_path: Option<String>,
    pub usb: Option<UsbIdentity>,
}

/// USB descriptor fields of the device behind a port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsbIdentity {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

impl PortInfo {
    /// Short human description, e.g. `1a86:7523 USB Serial`.
    pub fn describe(&self) -> String {
        match &self.usb {
            Some(u) => format!(
                "{:04x}:{:04x} {}",
                u.vendor_id,
                u.product_id,
                u.product.as_deref().or(u.manufacturer.as_deref()).unwrap_or("")
            )
            .trim_end()
            .to_string(),
            None => "non-USB".to_string(),
        }
    }
}

/// Every port the scans see, in platform listing order.
pub fn list_ports() -> Vec<PortInfo> {
    crate::platform::list_ports()
}

/// One entry of the port listing, with the attributes the
/// scans read: `device`, `name`, `description`, `hwid` and the USB fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListPortInfo {
    pub device: String,
    pub name: String,
    /// `'n/a'` unless the port's subsystem sets it.
    pub description: String,
    /// `'n/a'` unless the subsystem sets it; an unreadable PNP id is the none value.
    pub hwid: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub interface: Option<String>,
}

impl ListPortInfo {
    /// The entry of `device` alone: `description` and `hwid` `'n/a'`, no USB fields.
    pub fn new(device: &str, name: &str) -> Self {
        ListPortInfo {
            device: device.to_string(),
            name: name.to_string(),
            description: "n/a".into(),
            hwid: Some("n/a".into()),
            vid: None,
            pid: None,
            serial_number: None,
            location: None,
            manufacturer: None,
            product: None,
            interface: None,
        }
    }

    /// The USB description: `'<product> - <interface>'`, else the product, else the name (none prints as
    /// `'None'`).
    pub fn usb_description(&self) -> String {
        let product = self.product.as_deref().unwrap_or("None");
        match (&self.interface, &self.product) {
            (Some(interface), _) => format!("{product} - {interface}"),
            (None, Some(product)) => product.clone(),
            (None, None) => self.name.clone(),
        }
    }

    /// The USB hardware id: `'USB VID:PID=VVVV:PPPP[ SER=…][ LOCATION=…]'`.
    pub fn usb_info(&self) -> String {
        format!(
            "USB VID:PID={:04X}:{:04X}{}{}",
            self.vid.unwrap_or(0),
            self.pid.unwrap_or(0),
            self.serial_number.as_ref().map(|s| format!(" SER={s}")).unwrap_or_default(),
            self.location.as_ref().map(|l| format!(" LOCATION={l}")).unwrap_or_default()
        )
    }

    /// Sets `description` and `hwid` from the USB fields.
    pub fn apply_usb_info(&mut self) {
        self.description = self.usb_description();
        self.hwid = Some(self.usb_info());
    }

    /// The hardware id as text for diagnostics, without changing the stored value.
    pub fn hwid_text(&self) -> &str {
        self.hwid.as_deref().unwrap_or("None")
    }

    /// Whether the stored hardware id contains `needle`, before any diagnostic conversion.
    pub fn hwid_contains(&self, needle: &str) -> Result<bool, &'static str> {
        self.hwid.as_ref().map(|hwid| hwid.contains(needle)).ok_or("argument of type 'NoneType' is not iterable")
    }
}

/// The port listing: every port the scans see, with its description and hwid.
pub fn comports() -> Vec<ListPortInfo> {
    crate::platform::comports()
}

/// The port listing with its failures, retaining malformed/missing sysfs errors.
pub fn try_comports() -> std::io::Result<Vec<ListPortInfo>> {
    crate::platform::try_comports()
}

pub fn ports_with_details() -> std::io::Result<Vec<(PortInfo, ListPortInfo)>> {
    crate::platform::ports_with_details()
}

/// The listing entry of `device`, read from the system now.
pub fn port_details(device: &str) -> ListPortInfo {
    crate::platform::port_details(device)
}

impl ListPortInfo {
    /// The entry for `port` from its listed USB fields alone (no interface text, no
    /// location): for hosts that do not read the system.
    pub fn from_listing(port: &PortInfo) -> Self {
        let name = std::path::Path::new(&port.path).file_name().map(|n| n.to_string_lossy().into_owned());
        let mut info = ListPortInfo::new(&port.path, name.as_deref().unwrap_or(""));
        if let Some(usb) = &port.usb {
            info.vid = Some(usb.vendor_id);
            info.pid = Some(usb.product_id);
            info.serial_number = usb.serial_number.clone();
            info.manufacturer = usb.manufacturer.clone();
            info.product = usb.product.clone();
            info.apply_usb_info();
        }
        info
    }
}

/// Every serial port that looks like a USB device (on-board UARTs are skipped). Used by the CLI.
pub fn serial_ports() -> Vec<PortInfo> {
    list_ports().into_iter().filter(|p| p.usb.is_some()).collect()
}

/// Ports no controller scan may touch.
pub const EXCLUDED_SERIAL_PORTS: [&str; 3] =
    ["/dev/cu.debug-console", "/dev/cu.Bluetooth-Incoming-Port", "/dev/ttyAMA0"];

/// Whether `port` may be scanned under the `[APP] allowed_com_ports` setting (comma-separated, case-insensitive).
/// An empty setting, or one without any non-blank entry, allows every port.
pub fn allowed_search_com_ports(port: &str, allowed_com_ports: &str) -> bool {
    let allowed: Vec<String> = allowed_com_ports
        .split(',')
        .map(openrigcue_model::python_numeric::python_strip)
        .filter(|entry| !entry.is_empty())
        .map(str::to_uppercase)
        .collect();
    allowed.is_empty() || allowed.contains(&port.to_uppercase())
}

/// Ports currently held by accessory devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimedPorts {
    /// Motion compensation IMU.
    pub motion_compensation: Option<String>,
    pub belt: Option<String>,
    pub hapticr: Option<String>,
    pub intellibox: Option<String>,
    pub iflag: Option<String>,
    pub sim3d: Option<String>,
}

impl ClaimedPorts {
    /// Whether `port` is one of the claimed ports.
    pub fn contains(&self, port: &str) -> bool {
        [&self.motion_compensation, &self.belt, &self.hapticr, &self.intellibox, &self.iflag, &self.sim3d]
            .into_iter()
            .any(|claimed| claimed.as_deref() == Some(port))
    }
}

/// Whether a motion-controller scan may probe `port`: not excluded, allowed by `[APP] allowed_com_ports`, not
/// held by an accessory (the six accessory ports of [`ClaimedPorts`]).
pub fn scan_may_probe(port: &str, allowed_com_ports: &str, claimed: &ClaimedPorts) -> bool {
    !EXCLUDED_SERIAL_PORTS.contains(&port)
        && allowed_search_com_ports(port, allowed_com_ports)
        && !claimed.contains(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowed-ports setting: an empty list allows all.
    #[test]
    fn allowed_ports_empty_setting_allows_everything() {
        assert!(allowed_search_com_ports("/dev/ttyUSB0", ""));
        assert!(allowed_search_com_ports("/dev/ttyUSB0", " , ,"));
    }

    /// The allowed-ports setting: entries are stripped and compared upper-cased.
    #[test]
    fn allowed_ports_compare_trimmed_and_case_insensitive() {
        assert!(allowed_search_com_ports("com3", "COM1, Com3 "));
        assert!(!allowed_search_com_ports("COM2", "COM1, Com3 "));
        assert!(allowed_search_com_ports("/dev/ttyUSB1", "/dev/ttyusb1"));
        for space in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
            assert!(allowed_search_com_ports("COM3", &format!("{space}com3{space}")));
            assert!(allowed_search_com_ports("COM3", &format!("{space},{space}")));
            assert!(!allowed_search_com_ports("COM2", &format!("{space}com3{space}")));
        }
    }

    /// The scan filters of a motion-controller scan.
    #[test]
    fn scan_filters_excluded_disallowed_and_claimed_ports() {
        let claimed = ClaimedPorts { motion_compensation: Some("/dev/ttyUSB0".into()), ..ClaimedPorts::default() };
        assert!(!scan_may_probe("/dev/ttyAMA0", "", &claimed));
        assert!(!scan_may_probe("/dev/ttyUSB0", "", &claimed));
        assert!(!scan_may_probe("/dev/ttyUSB1", "/dev/ttyACM0", &claimed));
        assert!(scan_may_probe("/dev/ttyUSB1", "", &claimed));
    }
}
