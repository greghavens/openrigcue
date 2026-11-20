//! Linux USB topology and diagnostics. Enumeration reads sysfs; reset/authorization are explicit actions.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    pub class: String,
    pub subclass: String,
    pub protocol: String,
    pub driver: Option<String>,
    pub endpoints: Vec<Endpoint>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: String,
    pub attributes: String,
    pub max_packet_size: String,
    pub interval: String,
    pub direction: String,
    pub transfer_type: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Descriptor {
    pub offset: usize,
    pub kind: u8,
    pub name: String,
    pub bytes: Vec<u8>,
    pub fields: std::collections::BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub parent: Option<String>,
    pub bus: u16,
    pub address: u16,
    pub vendor: u16,
    pub product: u16,
    pub manufacturer: String,
    pub name: String,
    pub serial: String,
    pub usb_version: String,
    pub speed: String,
    pub power: String,
    pub device_class: String,
    pub authorized: bool,
    pub driver: Option<String>,
    pub interfaces: Vec<Interface>,
    pub ports: Vec<String>,
    pub descriptors: Vec<Descriptor>,
    pub path: PathBuf,
}
fn field(path: &Path, key: &str) -> String {
    fs::read_to_string(path.join(key)).unwrap_or_default().trim().into()
}
fn hex(path: &Path, key: &str) -> u16 {
    u16::from_str_radix(&field(path, key), 16).unwrap_or(0)
}
fn number(path: &Path, key: &str) -> u16 {
    field(path, key).parse().unwrap_or(0)
}
fn driver(path: &Path) -> Option<String> {
    fs::read_link(path.join("driver")).ok().and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
}
pub fn decode_descriptors(bytes: &[u8]) -> Result<Vec<Descriptor>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes.len() - at < 2 {
            return Err(format!("Truncated USB descriptor at {at}"));
        }
        let n = bytes[at] as usize;
        if n < 2 || n > bytes.len() - at {
            return Err(format!("Invalid USB descriptor length at {at}"));
        }
        let kind = bytes[at + 1];
        let name = match kind {
            1 => "Device",
            2 => "Configuration",
            3 => "String",
            4 => "Interface",
            5 => "Endpoint",
            6 => "Device qualifier",
            7 => "Other-speed configuration",
            9 => "Hub",
            11 => "Interface association",
            15 => "BOS",
            16 => "Device capability",
            33 => "HID",
            34 => "HID report",
            36 => "Class-specific interface",
            37 => "Class-specific endpoint",
            41 => "Hub",
            42 => "SuperSpeed hub",
            48 => "SuperSpeed endpoint companion",
            _ => "Unknown",
        };
        let raw = &bytes[at..at + n];
        let mut fields = std::collections::BTreeMap::new();
        let mut add = |key: &str, value: String| {
            fields.insert(key.into(), value);
        };
        let word = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        match kind {
            1 if n >= 18 => {
                add("USB version", format!("{:x}.{:02x}", raw[3], raw[2]));
                add("Vendor ID", format!("{:04x}", word(8)));
                add("Product ID", format!("{:04x}", word(10)));
                add("Class", class_name(raw[4]).into());
                add("Device release", format!("{:04x}", word(12)));
                add("Endpoint 0 max packet size", raw[7].to_string());
                add("Configurations", raw[17].to_string());
            }
            2 if n >= 9 => {
                add("Total length", word(2).to_string());
                add("Interfaces", raw[4].to_string());
                add("Configuration", raw[5].to_string());
                add("Attributes", format!("0x{:02x}", raw[7]));
                add("Maximum power units", raw[8].to_string());
            }
            3 => {
                add(
                    "Text",
                    String::from_utf16_lossy(
                        &raw[2..].as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect::<Vec<_>>(),
                    ),
                );
            }
            4 if n >= 9 => {
                add("Number", raw[2].to_string());
                add("Alternate setting", raw[3].to_string());
                add("Endpoints", raw[4].to_string());
                add("Class", class_name(raw[5]).into());
                add("Subclass", format!("0x{:02x}", raw[6]));
                add("Protocol", format!("0x{:02x}", raw[7]));
            }
            5 if n >= 7 => {
                add("Address", format!("0x{:02x}", raw[2]));
                add("Direction", if raw[2] & 128 != 0 { "IN" } else { "OUT" }.into());
                add("Transfer", ["Control", "Isochronous", "Bulk", "Interrupt"][(raw[3] & 3) as usize].into());
                add("Max packet size", (word(4) & 0x7ff).to_string());
                add("Interval", raw[6].to_string());
            }
            _ => {}
        }
        out.push(Descriptor { offset: at, kind, name: name.into(), bytes: raw.to_vec(), fields });
        at += n;
    }
    Ok(out)
}
pub fn class_name(class: u8) -> &'static str {
    match class {
        0 => "Defined per interface",
        1 => "Audio",
        2 => "Communications",
        3 => "Human interface",
        5 => "Physical",
        6 => "Imaging",
        7 => "Printer",
        8 => "Mass storage",
        9 => "Hub",
        10 => "CDC data",
        11 => "Smart card",
        13 => "Content security",
        14 => "Video",
        15 => "Healthcare",
        16 => "Audio/video",
        17 => "Billboard",
        18 => "USB Type-C bridge",
        0xdc => "Diagnostic",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application specific",
        0xff => "Vendor specific",
        _ => "Unknown",
    }
}
pub fn enumerate(root: &Path) -> Result<Vec<Device>, String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(root).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        if !path.join("idVendor").is_file() {
            continue;
        }
        let mut interfaces = Vec::new();
        for iface in fs::read_dir(&path).into_iter().flatten().flatten() {
            let p = iface.path();
            if !p.join("bInterfaceClass").is_file() {
                continue;
            }
            let mut endpoints = Vec::new();
            for ep in fs::read_dir(&p).into_iter().flatten().flatten() {
                let p = ep.path();
                if p.join("bEndpointAddress").is_file() {
                    endpoints.push(Endpoint {
                        address: field(&p, "bEndpointAddress"),
                        attributes: field(&p, "bmAttributes"),
                        max_packet_size: field(&p, "wMaxPacketSize"),
                        interval: field(&p, "bInterval"),
                        direction: field(&p, "direction"),
                        transfer_type: field(&p, "type"),
                    });
                }
            }
            interfaces.push(Interface {
                name: iface.file_name().to_string_lossy().into_owned(),
                class: field(&p, "bInterfaceClass"),
                subclass: field(&p, "bInterfaceSubClass"),
                protocol: field(&p, "bInterfaceProtocol"),
                driver: driver(&p),
                endpoints,
            });
        }
        let physical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let mut ports = Vec::new();
        for sub in ["tty", "hidraw"] {
            for p in walk(&physical, 4) {
                if p.parent().and_then(Path::file_name).is_some_and(|s| s == sub) {
                    ports.push(format!("/dev/{}", p.file_name().unwrap_or_default().to_string_lossy()));
                }
            }
        }
        ports.sort();
        ports.dedup();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        let parent = if id.starts_with("usb") {
            None
        } else {
            Some(
                id.rsplit_once('.')
                    .map_or_else(|| format!("usb{}", id.split('-').next().unwrap_or("")), |(p, _)| p.into()),
            )
        };
        let descriptors =
            fs::read(path.join("descriptors")).ok().and_then(|b| decode_descriptors(&b).ok()).unwrap_or_default();
        out.push(Device {
            id,
            parent,
            bus: number(&path, "busnum"),
            address: number(&path, "devnum"),
            vendor: hex(&path, "idVendor"),
            product: hex(&path, "idProduct"),
            manufacturer: field(&path, "manufacturer"),
            name: field(&path, "product"),
            serial: field(&path, "serial"),
            usb_version: field(&path, "version"),
            speed: field(&path, "speed"),
            power: field(&path, "bMaxPower"),
            device_class: field(&path, "bDeviceClass"),
            authorized: field(&path, "authorized") != "0",
            driver: driver(&path),
            interfaces,
            ports,
            descriptors,
            path: physical,
        });
    }
    let route = |s: &str| {
        s.split_once('-')
            .map(|(_, p)| p.split('.').map(|n| n.parse::<u16>().unwrap_or(0)).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    out.sort_by_key(|d| (d.bus, route(&d.id)));
    Ok(out)
}
fn walk(root: &Path, depth: usize) -> Vec<PathBuf> {
    if depth == 0 {
        return vec![];
    }
    let mut out = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        if e.file_type().is_ok_and(|f| f.is_dir()) {
            out.push(p.clone());
            out.extend(walk(&p, depth - 1));
        }
    }
    out
}
pub fn find<'a>(devices: &'a [Device], id: &str) -> Result<&'a Device, String> {
    devices.iter().find(|d| d.id == id).ok_or_else(|| format!("USB device {id} is no longer connected"))
}
pub fn text_report(devices: &[Device]) -> String {
    let mut s = String::from("OpenRigCue USB device report\n");
    for d in devices {
        s.push_str(&format!("\n{}  {:04x}:{:04x}  {} {}\n  Bus {:03} device {:03}; {} Mbit/s; USB {}; {}\n  Serial: {}\n  Driver: {}\n  Ports: {}\n",d.id,d.vendor,d.product,d.manufacturer,d.name,d.bus,d.address,d.speed,d.usb_version,d.power,d.serial,d.driver.as_deref().unwrap_or("none"),d.ports.join(", ")));
        for i in &d.interfaces {
            s.push_str(&format!(
                "  Interface {}: class {} / {} / {}, driver {}\n",
                i.name,
                i.class,
                i.subclass,
                i.protocol,
                i.driver.as_deref().unwrap_or("none")
            ));
            for ep in &i.endpoints {
                s.push_str(&format!(
                    "    Endpoint {} {} {} packet {} interval {}\n",
                    ep.address, ep.direction, ep.transfer_type, ep.max_packet_size, ep.interval
                ));
            }
        }
        for desc in &d.descriptors {
            s.push_str(&format!(
                "  {:04x} {}: {}\n",
                desc.offset,
                desc.name,
                desc.bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
            ));
            for (key, value) in &desc.fields {
                s.push_str(&format!("    {key}: {value}\n"));
            }
        }
    }
    s
}
/// Processes currently holding one of this USB device's nodes; never changes processes.
pub fn holders(device: &Device) -> Vec<(u32, String)> {
    let mut nodes = device.ports.clone();
    nodes.push(format!("/dev/bus/usb/{:03}/{:03}", device.bus, device.address));
    let mut out = Vec::new();
    for p in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let used = fs::read_dir(p.path().join("fd"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|target| nodes.iter().any(|n| target == Path::new(n))));
        if used {
            out.push((pid, field(&p.path(), "comm")));
        }
    }
    out.sort();
    out
}
pub fn xml_report(devices: &[Device]) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<UsbTreeViewReport platform=\"Linux\">\n");
    for d in devices {
        s.push_str(&format!("  <Device id=\"{}\" parent=\"{}\" vendor=\"{:04x}\" product=\"{:04x}\" bus=\"{}\" address=\"{}\"><Name>{}</Name><Manufacturer>{}</Manufacturer><Serial>{}</Serial><Speed>{}</Speed><Report>{}</Report><Snapshot>{}</Snapshot></Device>\n",esc(&d.id),esc(d.parent.as_deref().unwrap_or("")),d.vendor,d.product,d.bus,d.address,esc(&d.name),esc(&d.manufacturer),esc(&d.serial),esc(&d.speed),esc(&text_report(std::slice::from_ref(d))),esc(&serde_json::to_string(d).unwrap_or_default())));
    }
    s.push_str("</UsbTreeViewReport>\n");
    s
}
pub fn import_report(text: &str) -> Result<Vec<Device>, String> {
    if text.len() > 16 * 1024 * 1024 {
        return Err("USB report exceeds 16 MiB".into());
    }
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text).map_err(|e| e.to_string());
    }
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let devices = doc
        .descendants()
        .filter(|n| n.has_tag_name("Snapshot"))
        .map(|n| serde_json::from_str(n.text().unwrap_or("")).map_err(|e| e.to_string()))
        .collect::<Result<Vec<Device>, String>>()?;
    if devices.is_empty() {
        return Err("This report does not contain an OpenRigCue USB snapshot".into());
    }
    Ok(devices)
}
pub fn set_authorized(device: &Device, authorized: bool) -> Result<(), String> {
    fs::write(device.path.join("authorized"), if authorized { b"1" } else { b"0" })
        .map_err(|e| format!("USB authorization: {e}"))
}
pub fn restart_driver(device: &Device) -> Result<(), String> {
    let driver = fs::canonicalize(device.path.join("driver")).map_err(|e| e.to_string())?;
    fs::write(driver.join("unbind"), device.id.as_bytes()).map_err(|e| e.to_string())?;
    fs::write(driver.join("bind"), device.id.as_bytes()).map_err(|e| format!("USB driver rebind failed: {e}"))
}
#[allow(unsafe_code)]
pub fn reset_port(device: &Device) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let path = format!("/dev/bus/usb/{:03}/{:03}", device.bus, device.address);
    let f = fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| e.to_string())?;
    // SAFETY: USBDEVFS_RESET takes no argument; the fd is held for the call.
    if unsafe { libc::ioctl(f.as_raw_fd(), 0x5514) } < 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_parser_validates_lengths() {
        assert!(decode_descriptors(&[0, 1]).is_err());
        assert!(decode_descriptors(&[4, 1, 0]).is_err());
        let d = decode_descriptors(&[4, 3, b'A', 0, 2, 5]).unwrap();
        assert_eq!(d[1].name, "Endpoint");
        assert_eq!(d[0].bytes, [4, 3, 65, 0]);
    }
    #[test]
    fn sysfs_topology_ports_and_report() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("2-1.3");
        fs::create_dir_all(p.join("2-1.3:1.0/tty/ttyACM5")).unwrap();
        for (k, v) in [
            ("idVendor", "0483"),
            ("idProduct", "5740"),
            ("manufacturer", "DOF & Reality"),
            ("product", "Controller"),
            ("busnum", "2"),
            ("devnum", "7"),
            ("speed", "12"),
        ] {
            fs::write(p.join(k), v).unwrap();
        }
        let d = enumerate(t.path()).unwrap();
        assert_eq!(d[0].parent.as_deref(), Some("2-1"));
        assert_eq!(d[0].ports, ["/dev/ttyACM5"]);
        assert!(xml_report(&d).contains("DOF &amp; Reality"));
        assert!(text_report(&d).contains("0483:5740"));
    }
}
