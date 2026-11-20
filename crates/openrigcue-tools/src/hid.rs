//! WeAct and DOF HID bootloaders: the packet layouts those bootloaders accept.
//! https://dofreality.com/tools/ documents the current ASCII bank commands.
use crate::{firmware::Payload, usb};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

pub trait Reports {
    fn write(&mut self, data: &[u8; 64]) -> Result<(), String>;
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>, String>;
    fn delay(&mut self, delay: Duration) {
        std::thread::sleep(delay);
    }
}
pub struct LinuxReports {
    file: fs::File,
}
#[allow(unsafe_code)]
impl LinuxReports {
    pub fn open(id: &str) -> Result<Self, String> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
        let devices = usb::enumerate(Path::new("/sys/bus/usb/devices"))?;
        let device = usb::find(&devices, id)?;
        if device.vendor != 0x0483 || device.product != 0x572a {
            return Err("Select a DOF/WeAct HID bootloader (0483:572a)".into());
        }
        let paths: Vec<_> = device.ports.iter().filter(|p| p.starts_with("/dev/hidraw")).collect();
        if paths.len() != 1 {
            return Err("The selected controller must expose one HID interface".into());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(paths[0])
            .map_err(|e| e.to_string())?;
        // SAFETY: flock only operates on the live descriptor; cooperative ownership prevents two tool windows writing it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err("The HID controller is in use by another tool".into());
        }
        Ok(Self { file })
    }
}
#[allow(unsafe_code)]
impl Reports for LinuxReports {
    fn write(&mut self, data: &[u8; 64]) -> Result<(), String> {
        let mut report = [0; 65];
        report[1..].copy_from_slice(data);
        match self.file.write(&report) {
            Ok(65) => Ok(()),
            Ok(n) => Err(format!("Incomplete HID report ({n}/65 bytes)")),
            Err(e) => Err(e.to_string()),
        }
    }
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>, String> {
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll receives one initialized pollfd that remains valid for the call.
        let n = unsafe { libc::poll(&mut p, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if n == 0 {
            return Err("Timed out waiting for a controller acknowledgement".into());
        }
        let mut b = vec![0; 64];
        let n = self.file.read(&mut b).map_err(|e| e.to_string())?;
        b.truncate(n);
        Ok(b)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Weact,
    DofLegacyEncoder,
    DofLegacyCueing,
    DofLegacyCueingV1,
    DofEncoder,
    DofCueing,
}
impl Protocol {
    pub fn confirmed_product(self, product: &str) -> bool {
        match self {
            Self::Weact => product.contains("HID Bootloader") && !product.contains("HID-2"),
            _ => product.contains("HID-2") || product.contains("DOF") || product.contains("WeAct"),
        }
    }
}

fn binary_command(command: u8, parameters: &[u8]) -> [u8; 64] {
    let mut packet = [0; 64];
    packet[..6].copy_from_slice(b"WeAct:");
    packet[6] = command;
    packet[7..7 + parameters.len()].copy_from_slice(parameters);
    packet
}
fn ascii_command(command: &str) -> [u8; 64] {
    let mut packet = [0; 64];
    packet[..command.len()].copy_from_slice(command.as_bytes());
    packet[command.len()] = b'\n';
    packet
}
fn acknowledge(io: &mut impl Reports, command: Option<u8>) -> Result<Vec<u8>, String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let reply = io.read(Duration::from_secs(3))?;
        if reply.len() >= 7 && &reply[..6] == b"WeAct:" && command.is_none_or(|c| reply[6] == c) {
            return Ok(reply);
        }
    }
    Err("Controller did not acknowledge the HID operation".into())
}
pub fn information(io: &mut impl Reports) -> Result<String, String> {
    io.write(&binary_command(2, &[]))?;
    let bytes = acknowledge(io, None)?;
    Ok(String::from_utf8_lossy(&bytes[6..]).trim_matches(char::from(0)).to_owned())
}
pub fn reboot(io: &mut impl Reports, protocol: Protocol) -> Result<(), String> {
    let packet = match protocol {
        Protocol::DofEncoder => ascii_command("WeAct:JT"),
        Protocol::DofCueing => ascii_command("WeAct:JY"),
        _ => binary_command(1, &[]),
    };
    io.write(&packet)
}
pub fn erase(io: &mut impl Reports, protocol: Protocol) -> Result<(), String> {
    match protocol {
        Protocol::Weact => {
            information(io)?;
            io.write(&binary_command(4, &[]))?;
            acknowledge(io, None)?;
        }
        Protocol::DofEncoder | Protocol::DofCueing => {
            io.write(&ascii_command(if protocol == Protocol::DofEncoder { "WeAct:-T" } else { "WeAct:-Y" }))?;
            io.delay(Duration::from_millis(250));
        }
        Protocol::DofLegacyEncoder => {
            io.write(&binary_command(b'-', &[3, 24, 0]))?;
            io.delay(Duration::from_millis(230));
            io.write(&binary_command(b'-', &[4, 32, 0]))?;
            io.delay(Duration::from_millis(230));
        }
        Protocol::DofLegacyCueing => {
            io.write(&binary_command(b'-', &[5, 64, 0]))?;
            io.delay(Duration::from_millis(230));
        }
        Protocol::DofLegacyCueingV1 => {
            io.write(&binary_command(b'-', &[5, 64, 0]))?;
            io.delay(Duration::from_millis(230));
        }
    }
    Ok(())
}

/// Returns only after transfer completion. The vendor HID protocol does not offer byte-for-byte readback.
pub fn program(
    io: &mut impl Reports,
    image: &Payload,
    protocol: Protocol,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), String> {
    if image.bytes.is_empty() || image.bytes.len() > 256 * 1024 {
        return Err("HID image is empty or exceeds controller flash".into());
    }
    let expected_address = match protocol {
        Protocol::Weact => 0x08004000,
        Protocol::DofLegacyEncoder | Protocol::DofEncoder => 0x0800c000,
        Protocol::DofLegacyCueing | Protocol::DofCueing => 0x08026400,
        Protocol::DofLegacyCueingV1 => 0x08020000,
    };
    if image.address != expected_address {
        return Err(format!("Selected bootloader requires load address 0x{expected_address:08x}"));
    }
    if matches!(protocol, Protocol::DofEncoder | Protocol::DofLegacyEncoder) && image.bytes.len() > 80 * 1024 {
        return Err("Encoder image exceeds its firmware partition".into());
    }
    if matches!(protocol, Protocol::DofCueing | Protocol::DofLegacyCueing | Protocol::DofLegacyCueingV1)
        && image.bytes.len() > 102 * 1024
    {
        return Err("Motion image exceeds its firmware partition".into());
    }
    match protocol {
        Protocol::Weact => {
            if image.address != 0x08004000 {
                return Err("The standard WeAct application starts at 0x08004000".into());
            }
            erase(io, protocol)?;
            io.write(&binary_command(0, &[]))?;
        }
        Protocol::DofLegacyEncoder => {
            io.write(&binary_command(b'W', &[24, 0, 0]))?;
            io.delay(Duration::from_millis(30));
            erase(io, protocol)?;
        }
        Protocol::DofLegacyCueing => {
            io.write(&binary_command(b'W', &[76, 1, 0]))?;
            io.delay(Duration::from_millis(30));
            erase(io, protocol)?;
        }
        Protocol::DofLegacyCueingV1 => {
            io.write(&binary_command(b'W', &[64, 0, 0]))?;
            io.delay(Duration::from_millis(30));
            erase(io, protocol)?;
        }
        Protocol::DofEncoder | Protocol::DofCueing => {
            let bank = if protocol == Protocol::DofEncoder { 'T' } else { 'Y' };
            io.write(&ascii_command(&format!("WeAct:w{bank}")))?;
            io.delay(Duration::from_millis(30));
            erase(io, protocol)?;
        }
    }
    for (i, chunk) in image.bytes.chunks(1024).enumerate() {
        let mut page = [255; 1024];
        page[..chunk.len()].copy_from_slice(chunk);
        for packet in page.as_chunks::<64>().0 {
            io.write(packet)?;
            io.delay(Duration::from_millis(5));
        }
        if matches!(
            protocol,
            Protocol::Weact | Protocol::DofLegacyEncoder | Protocol::DofLegacyCueing | Protocol::DofLegacyCueingV1
        ) {
            acknowledge(io, Some(3))?;
        } else {
            io.delay(Duration::from_millis(50));
        }
        progress((i * 1024 + chunk.len()).min(image.bytes.len()), image.bytes.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        sent: Vec<[u8; 64]>,
        responses: std::collections::VecDeque<Vec<u8>>,
    }
    impl Reports for Fake {
        fn write(&mut self, p: &[u8; 64]) -> Result<(), String> {
            self.sent.push(*p);
            Ok(())
        }
        fn read(&mut self, _: Duration) -> Result<Vec<u8>, String> {
            self.responses.pop_front().ok_or("No response".into())
        }
        fn delay(&mut self, _: Duration) {}
    }
    #[test]
    fn legacy_commands_match_vendor_batch_and_pad_the_last_page() {
        let mut io = Fake { responses: [b"WeAct:\x03".to_vec()].into(), ..Default::default() };
        program(&mut io, &Payload { address: 0x0800c000, bytes: vec![42; 65] }, Protocol::DofLegacyEncoder, |_, _| {})
            .unwrap();
        assert_eq!(&io.sent[0][..10], b"WeAct:W\x18\0\0");
        assert_eq!(&io.sent[1][..10], b"WeAct:-\x03\x18\0");
        assert_eq!(&io.sent[2][..10], b"WeAct:-\x04\x20\0");
        assert_eq!(io.sent.len(), 19);
        assert_eq!(io.sent[4][0], 42);
        assert_eq!(&io.sent[4][1..], &[255; 63]);
    }
    #[test]
    fn current_bank_commands_match_web_updater() {
        let mut io = Fake::default();
        program(&mut io, &Payload { address: 0x08026400, bytes: vec![1] }, Protocol::DofCueing, |_, _| {}).unwrap();
        assert_eq!(&io.sent[0][..9], b"WeAct:wY\n");
        assert_eq!(&io.sent[1][..9], b"WeAct:-Y\n");
        reboot(&mut io, Protocol::DofCueing).unwrap();
        assert_eq!(&io.sent.last().unwrap()[..9], b"WeAct:JY\n");
    }
    #[test]
    fn weact_requires_page_ack_and_matching_address() {
        let mut io = Fake {
            responses: [b"WeAct:\x02V1.2".to_vec(), b"WeAct:\x04ok".to_vec(), b"WeAct:\x03".to_vec()].into(),
            ..Default::default()
        };
        program(&mut io, &Payload { address: 0x08004000, bytes: vec![1] }, Protocol::Weact, |_, _| {}).unwrap();
        assert_eq!(io.sent.len(), 19);
        assert!(program(&mut io, &Payload { address: 0, bytes: vec![1] }, Protocol::Weact, |_, _| {}).is_err());
    }
}
