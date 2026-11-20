//! Port selection rules shared by the accessory drivers' connection scans.

use super::link::PortEntry;
use super::numeric::python_strip;

/// The serial devices that take part in port arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Intellibox,
    Belt,
    HaptiCr,
    Sim3d,
    IFlag,
    /// The WitMotion IMU used for motion compensation.
    MotionCompensation,
}

/// Ports held by connected devices (one per accessory, plus the
/// motion platform's ports). An empty string means "none".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimedPorts {
    pub motion: Vec<String>,
    pub intellibox: String,
    pub belt: String,
    pub hapticr: String,
    pub sim3d: String,
    pub iflag: String,
    pub motion_compensation: String,
}

impl ClaimedPorts {
    /// The port a device currently holds.
    pub fn of(&self, device: Device) -> &str {
        match device {
            Device::Intellibox => &self.intellibox,
            Device::Belt => &self.belt,
            Device::HaptiCr => &self.hapticr,
            Device::Sim3d => &self.sim3d,
            Device::IFlag => &self.iflag,
            Device::MotionCompensation => &self.motion_compensation,
        }
    }

    /// Record (or clear, with `""`) the port a device holds.
    pub fn set(&mut self, device: Device, port: &str) {
        let slot = match device {
            Device::Intellibox => &mut self.intellibox,
            Device::Belt => &mut self.belt,
            Device::HaptiCr => &mut self.hapticr,
            Device::Sim3d => &mut self.sim3d,
            Device::IFlag => &mut self.iflag,
            Device::MotionCompensation => &mut self.motion_compensation,
        };
        *slot = port.to_string();
    }

    /// Whether `port` is held by another device, as seen by `asking`.
    ///
    /// Every device sees the motion platform's ports and the
    /// other devices' ports, except that the belt does not look at the HaptiCR port and the IMU does not
    /// look at its own port.
    pub fn in_use_for(&self, asking: Device, port: &str) -> bool {
        let others: &[Device] = match asking {
            Device::Intellibox => {
                &[Device::MotionCompensation, Device::Belt, Device::HaptiCr, Device::IFlag, Device::Sim3d]
            }
            Device::Belt => &[Device::MotionCompensation, Device::Intellibox, Device::IFlag, Device::Sim3d],
            Device::HaptiCr => {
                &[Device::MotionCompensation, Device::Belt, Device::Intellibox, Device::IFlag, Device::Sim3d]
            }
            Device::Sim3d => {
                &[Device::MotionCompensation, Device::Belt, Device::HaptiCr, Device::Intellibox, Device::IFlag]
            }
            Device::IFlag => {
                &[Device::MotionCompensation, Device::Belt, Device::HaptiCr, Device::Intellibox, Device::Sim3d]
            }
            Device::MotionCompensation => {
                &[Device::Belt, Device::HaptiCr, Device::Intellibox, Device::IFlag, Device::Sim3d]
            }
        };
        self.motion.iter().any(|p| p == port) || others.iter().any(|d| self.of(*d) == port)
    }
}

/// Which device nodes a scan never touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusions {
    /// Only the Raspberry Pi UART `/dev/ttyAMA0` (belt, HaptiCR, SIM3D, IMU).
    RaspberryUart,
    /// The excluded serial port list (Intellibox, iFlag).
    Standard,
}

impl Exclusions {
    pub fn excludes(self, device: &str) -> bool {
        match self {
            Exclusions::RaspberryUart => device == "/dev/ttyAMA0",
            Exclusions::Standard => {
                matches!(device, "/dev/cu.debug-console" | "/dev/cu.Bluetooth-Incoming-Port" | "/dev/ttyAMA0")
            }
        }
    }
}

/// `APP.allowed_com_ports`: a comma separated allow-list; empty allows every port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedPorts(Vec<String>);

impl AllowedPorts {
    /// Parse the raw config value: comma-separated names, trimmed and upper-cased, empty ones dropped.
    pub fn parse(raw: &str) -> Self {
        AllowedPorts(raw.split(',').map(python_strip).filter(|p| !p.is_empty()).map(str::to_uppercase).collect())
    }

    /// Whether the trimmed, upper-cased port name is in the list.
    pub fn allows(&self, port: &str) -> bool {
        self.0.is_empty() || self.0.contains(&python_strip(port).to_uppercase())
    }
}

/// The filters every scan applies before looking at a port's USB id.
#[derive(Debug, Clone)]
pub struct ScanRules<'a> {
    pub device: Device,
    pub exclusions: Exclusions,
    /// The device section's `com_port` ('' searches every port).
    pub configured_port: &'a str,
    pub allowed: &'a AllowedPorts,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection, consulted on the first scan only.
    pub cached_port: Option<&'a str>,
    pub first_scan: bool,
}

impl ScanRules<'_> {
    /// Whether the scan may try `port`, checked in this order: excluded node, fixed port, allow-list,
    /// held by another device, and on the first scan only the cached port.
    pub fn may_try(&self, port: &PortEntry) -> bool {
        let device = port.device.as_str();
        if self.exclusions.excludes(device) {
            return false;
        }
        if !self.configured_port.is_empty() && self.configured_port != device {
            return false;
        }
        if !self.allowed.allows(device) || self.claimed.in_use_for(self.device, device) {
            return false;
        }
        match (self.first_scan, self.cached_port) {
            (true, Some(cached)) => cached == device,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(device: &str) -> PortEntry {
        PortEntry::usb(device, 0x1a86, 0x7523, None, None)
    }

    /// The allow-list.
    #[test]
    fn allowed_ports_list_is_case_insensitive_and_empty_allows_all() {
        assert!(AllowedPorts::parse("").allows("/dev/ttyUSB0"));
        assert!(AllowedPorts::parse(" , ").allows("COM3"));
        let list = AllowedPorts::parse("com3, /dev/ttyusb2");
        assert!(list.allows("COM3"));
        assert!(list.allows("/dev/ttyUSB2"));
        assert!(!list.allows("/dev/ttyUSB0"));
        // Trimming also removes the U+001C..U+001F separators, which Rust's trim() keeps.
        let padded = AllowedPorts::parse("\u{1c}/dev/ttyUSB4\u{1f}");
        assert!(padded.allows("/dev/ttyUSB4"));
        assert!(padded.allows("\u{1d}/dev/ttyusb4 "));
    }

    /// Ports in use as seen by the belt, the IMU and the iFlag.
    #[test]
    fn ports_in_use_lists_follow_each_driver() {
        let claimed = ClaimedPorts {
            motion: vec!["/dev/ttyUSB1".into()],
            hapticr: "/dev/ttyUSB5".into(),
            motion_compensation: "/dev/ttyUSB0".into(),
            ..Default::default()
        };
        assert!(claimed.in_use_for(Device::Intellibox, "/dev/ttyUSB1"));
        assert!(claimed.in_use_for(Device::Intellibox, "/dev/ttyUSB5"));
        assert!(!claimed.in_use_for(Device::Belt, "/dev/ttyUSB5"));
        assert!(claimed.in_use_for(Device::Belt, "/dev/ttyUSB0"));
        assert!(!claimed.in_use_for(Device::MotionCompensation, "/dev/ttyUSB0"));
        assert!(claimed.in_use_for(Device::Sim3d, "/dev/ttyUSB0"));
    }

    /// The common filter sequence of the belt, HaptiCR, SIM3D, iFlag and Intellibox scans.
    #[test]
    fn scan_rules_apply_fixed_port_and_first_scan_cache() {
        let allowed = AllowedPorts::default();
        let claimed = ClaimedPorts::default();
        let mut rules = ScanRules {
            device: Device::IFlag,
            exclusions: Exclusions::Standard,
            configured_port: "",
            allowed: &allowed,
            claimed: &claimed,
            cached_port: Some("/dev/ttyUSB4"),
            first_scan: true,
        };
        assert!(!rules.may_try(&port("/dev/ttyUSB3")));
        assert!(rules.may_try(&port("/dev/ttyUSB4")));
        assert!(!rules.may_try(&port("/dev/ttyAMA0")));
        rules.first_scan = false;
        assert!(rules.may_try(&port("/dev/ttyUSB3")));
        rules.configured_port = "/dev/ttyUSB9";
        assert!(!rules.may_try(&port("/dev/ttyUSB3")));
        assert!(Exclusions::Standard.excludes("/dev/cu.debug-console"));
        assert!(!Exclusions::RaspberryUart.excludes("/dev/cu.debug-console"));
    }
}
