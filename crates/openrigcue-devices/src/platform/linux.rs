//! Serial port enumeration from `/dev` and sysfs, without libudev. Nothing here opens a device.

use std::fs;
use std::path::{Path, PathBuf};

use crate::discovery::{ListPortInfo, PortInfo, UsbIdentity};

mod realpath;
use realpath::realpath;

/// Device-name prefixes of serial ports, in listing order.
const DEVICE_PREFIXES: [&str; 7] = ["ttyS", "ttyUSB", "ttyXRUSB", "ttyACM", "ttyAMA", "rfcomm", "ttyAP"];

/// Where the port listing looks; replaced in tests.
pub struct Roots {
    pub dev: PathBuf,
    pub sys_class_tty: PathBuf,
    pub by_id: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Roots {
            dev: PathBuf::from("/dev"),
            sys_class_tty: PathBuf::from("/sys/class/tty"),
            by_id: PathBuf::from("/dev/serial/by-id"),
        }
    }
}

fn stable_alias(by_id: &Path, dev: &Path) -> Option<String> {
    let target = fs::canonicalize(dev).ok()?;
    fs::read_dir(by_id)
        .ok()?
        .flatten()
        .find(|entry| fs::canonicalize(entry.path()).ok().as_deref() == Some(target.as_path()))
        .map(|entry| entry.path().to_string_lossy().into_owned())
}

/// The device nodes matching the seven prefixes, before their sysfs entries are read.
fn port_paths(roots: &Roots) -> Vec<PathBuf> {
    DEVICE_PREFIXES
        .iter()
        .flat_map(|prefix| {
            fs::read_dir(&roots.dev)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(move |name| name.starts_with(prefix))
                .map(|name| roots.dev.join(name))
        })
        .collect()
}

/// Best-effort summary for diagnostic callers; scan paths use the checked result.
pub fn list_ports_in(roots: &Roots) -> Vec<PortInfo> {
    ports_with_details_in(roots).unwrap_or_default().into_iter().map(|(port, _)| port).collect()
}

pub fn list_ports() -> Vec<PortInfo> {
    list_ports_in(&Roots::default())
}

/// The first line of the file, stripped; `None` when it cannot be read.
fn read_line(path: &Path) -> std::io::Result<Option<String>> {
    let Ok(bytes) = fs::read(path) else { return Ok(None) };
    let text = String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let first = text.split(['\n', '\r']).next().unwrap_or("");
    Ok(Some(openrigcue_model::python_numeric::python_strip(first).to_owned()))
}

fn usb_id(text: Option<String>) -> std::io::Result<u16> {
    let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let text = text.ok_or_else(|| invalid("int() can't convert non-string with explicit base".to_owned()))?;
    let normalized = openrigcue_model::python_numeric::decimal_ascii(&text);
    let body = normalized.strip_prefix('+').unwrap_or(&normalized);
    let digits = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X"));
    let body = digits.map(|s| s.strip_prefix('_').unwrap_or(s)).unwrap_or(body);
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return Err(invalid(format!(
            "invalid literal for int() with base 16: {}",
            crate::motion::python::str_repr(&text)
        )));
    }
    u16::from_str_radix(&body.replace('_', ""), 16).map_err(|_| {
        invalid(format!("invalid literal for int() with base 16: {}", crate::motion::python::str_repr(&text)))
    })
}

/// The last component of a path, empty when there is none.
fn basename(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One entry of the port listing: the device path and subsystem
/// from `/sys/class/tty/<name>/device`, the USB fields from the interface and device directories above it, then
/// `description` and `hwid` per subsystem.
fn try_sysfs_entry(roots: &Roots, device: &str) -> std::io::Result<(ListPortInfo, Option<String>)> {
    let name = basename(Path::new(device));
    let mut info = ListPortInfo::new(device, &name);
    let link_target = if fs::symlink_metadata(device).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        Some(realpath(Path::new(device))?.to_string_lossy().into_owned())
    } else {
        None
    };
    if let Some(target) = &link_target {
        info.hwid = Some(format!("LINK={target}"));
    }
    let link = roots.sys_class_tty.join(&name).join("device");
    let device_path = if link.exists() { Some(realpath(&link)?) } else { None };
    let subsystem = device_path.as_deref().map(|d| realpath(&d.join("subsystem"))).transpose()?.map(|s| basename(&s));
    let interface_path = match (subsystem.as_deref(), device_path.as_deref()) {
        (Some("usb-serial"), Some(d)) => d.parent().map(Path::to_path_buf),
        (Some("usb"), Some(d)) => Some(d.to_path_buf()),
        _ => None,
    };
    if let Some(interface_path) = &interface_path {
        if let Some(usb_device_path) = interface_path.parent() {
            let interfaces = read_line(&usb_device_path.join("bNumInterfaces"))?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
                )
            })?;
            // Text that is not a number is tolerated for this field; a missing file is an error.
            let multiple_interfaces = openrigcue_model::python_numeric::integer_text(&interfaces)
                .filter(|text| !text.starts_with('-') && text.trim_start_matches('+').len() <= 4300)
                .map(|text| {
                    let digits = text.strip_prefix('+').unwrap_or(&text).trim_start_matches('0');
                    digits.len() > 1 || digits.as_bytes().first().is_some_and(|digit| *digit > b'1')
                })
                .unwrap_or(false);
            info.vid = Some(usb_id(read_line(&usb_device_path.join("idVendor"))?)?);
            info.pid = Some(usb_id(read_line(&usb_device_path.join("idProduct"))?)?);
            info.serial_number = read_line(&usb_device_path.join("serial"))?;
            info.location =
                Some(if multiple_interfaces { basename(interface_path) } else { basename(usb_device_path) });
            info.manufacturer = read_line(&usb_device_path.join("manufacturer"))?;
            info.product = read_line(&usb_device_path.join("product"))?;
            info.interface = read_line(&interface_path.join("interface"))?;
        }
    }
    match (subsystem.as_deref(), device_path.as_deref()) {
        (Some("usb" | "usb-serial"), _) => info.apply_usb_info(),
        (Some("pnp"), Some(d)) => {
            info.description = name.clone();
            info.hwid = read_line(&d.join("id"))?;
        }
        (Some("amba"), Some(d)) => {
            info.description = name.clone();
            info.hwid = Some(basename(d));
        }
        _ => {}
    }
    if let Some(target) = link_target {
        info.hwid
            .as_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported operand type(s) for +=: 'NoneType' and 'str'",
                )
            })?
            .push_str(&format!(" LINK={target}"));
    }
    Ok((info, subsystem))
}

fn try_sysfs_port(roots: &Roots, device: &str) -> std::io::Result<ListPortInfo> {
    try_sysfs_entry(roots, device).map(|(details, _)| details)
}

/// The port listing as [`ListPortInfo`] entries, in the order of [`list_ports_in`].
pub fn comports_in(roots: &Roots) -> std::io::Result<Vec<ListPortInfo>> {
    ports_with_details_in(roots).map(|ports| ports.into_iter().map(|(_, details)| details).collect())
}

/// [`comports_in`] on the real system.
pub fn comports() -> Vec<ListPortInfo> {
    try_comports().unwrap_or_default()
}

pub fn try_comports() -> std::io::Result<Vec<ListPortInfo>> {
    comports_in(&Roots::default())
}

/// A scan's port nodes and metadata from the same enumeration, for motion hosts.
pub fn ports_with_details() -> std::io::Result<Vec<(PortInfo, ListPortInfo)>> {
    ports_with_details_in(&Roots::default())
}

fn ports_with_details_in(roots: &Roots) -> std::io::Result<Vec<(PortInfo, ListPortInfo)>> {
    let mut ports = Vec::new();
    for device in port_paths(roots) {
        let path = device.to_string_lossy().into_owned();
        let (details, subsystem) = try_sysfs_entry(roots, &path)?;
        if subsystem.as_deref() == Some("platform") {
            continue;
        }
        let usb = details.vid.zip(details.pid).map(|(vendor_id, product_id)| UsbIdentity {
            vendor_id,
            product_id,
            serial_number: details.serial_number.clone(),
            manufacturer: details.manufacturer.clone(),
            product: details.product.clone(),
        });
        ports.push((PortInfo { path, stable_path: stable_alias(&roots.by_id, &device), usb }, details));
    }
    Ok(ports)
}

/// Best-effort details for callers outside checked scan boundaries.
pub fn port_details(device: &str) -> ListPortInfo {
    try_sysfs_port(&Roots::default(), device)
        .unwrap_or_else(|_| ListPortInfo::new(device, &basename(Path::new(device))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openrigcue-ports-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Builds a fake `/dev` + `/sys` tree: a USB adapter, a real UART, a placeholder platform UART,
    /// a node without sysfs device, and an unrelated tty.
    fn fake_tree(root: &Path) -> Roots {
        let dev = root.join("dev");
        let tty = root.join("sys/class/tty");
        let bus = root.join("sys/bus");
        let usb_dev = root.join("sys/devices/usb1/1-1");
        for d in [&dev, &tty, &bus.join("usb-serial"), &bus.join("pnp"), &bus.join("platform")] {
            fs::create_dir_all(d).unwrap();
        }
        let usb_if = usb_dev.join("1-1:1.0/ttyUSB0");
        fs::create_dir_all(&usb_if).unwrap();
        fs::write(usb_dev.join("idVendor"), "1a86\n").unwrap();
        fs::write(usb_dev.join("idProduct"), "7523\n").unwrap();
        fs::write(usb_dev.join("bNumInterfaces"), "1\n").unwrap();
        fs::write(usb_dev.join("product"), "USB Serial\n").unwrap();
        symlink(bus.join("usb-serial"), usb_if.join("subsystem")).unwrap();
        let uart = root.join("sys/devices/pnp0/00:01");
        fs::create_dir_all(&uart).unwrap();
        symlink(bus.join("pnp"), uart.join("subsystem")).unwrap();
        let placeholder = root.join("sys/devices/platform/serial8250");
        fs::create_dir_all(&placeholder).unwrap();
        symlink(bus.join("platform"), placeholder.join("subsystem")).unwrap();
        for (name, device) in [
            ("ttyUSB0", Some(&usb_if)),
            ("ttyS0", Some(&uart)),
            ("ttyS1", Some(&placeholder)),
            ("rfcomm0", None),
            ("tty1", None),
        ] {
            fs::write(dev.join(name), "").unwrap();
            fs::create_dir_all(tty.join(name)).unwrap();
            if let Some(d) = device {
                symlink(d, tty.join(name).join("device")).unwrap();
            }
        }
        Roots { dev, sys_class_tty: tty, by_id: root.join("dev/serial/by-id") }
    }

    /// The port listing: name prefixes, `subsystem != "platform"` filter.
    #[test]
    fn lists_ports_like_pyserial_comports() {
        let root = scratch("list");
        let ports = list_ports_in(&fake_tree(&root));
        let names: Vec<String> =
            ports.iter().map(|p| Path::new(&p.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["ttyS0", "ttyUSB0", "rfcomm0"]);
        let usb = ports[1].usb.as_ref().unwrap();
        assert_eq!((usb.vendor_id, usb.product_id), (0x1a86, 0x7523));
        assert!(ports[0].usb.is_none() && ports[2].usb.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// A listing entry: description/hwid per subsystem (usb-serial: product, `USB VID:PID=… LOCATION=…`;
    /// pnp: the name and the `id` file; no sysfs device: `n/a`).
    #[test]
    fn comports_entries_like_pyserial_sysfs() {
        let root = scratch("comports");
        let roots = fake_tree(&root);
        fs::write(root.join("sys/devices/pnp0/00:01/id"), "PNP0501\n").unwrap();
        let entries = comports_in(&roots).unwrap();
        let got: Vec<(String, String, String)> =
            entries.iter().map(|e| (e.name.clone(), e.description.clone(), e.hwid_text().to_owned())).collect();
        assert_eq!(
            got,
            [
                ("ttyS0".into(), "ttyS0".into(), "PNP0501".into()),
                ("ttyUSB0".into(), "USB Serial".into(), "USB VID:PID=1A86:7523 LOCATION=1-1".into()),
                ("rfcomm0".into(), "n/a".into(), "n/a".into()),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_pnp_missing_id_keeps_none_in_port_metadata() {
        let root = scratch("pnp-none");
        let roots = fake_tree(&root);
        let ports = comports_in(&roots).unwrap();
        let pnp = ports.iter().find(|port| port.name == "ttyS0").unwrap();
        assert_eq!(serde_json::to_value(pnp).unwrap()["hwid"], serde_json::Value::Null);
        let id = root.join("sys/devices/pnp0/00:01/id");
        fs::create_dir(&id).unwrap();
        assert_eq!(comports_in(&roots).unwrap()[0].hwid, None);
        fs::remove_dir(&id).unwrap();
        for text in ["", "None", "PNP0501"] {
            fs::write(&id, text).unwrap();
            assert_eq!(comports_in(&roots).unwrap()[0].hwid.as_deref(), Some(text));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn native_pnp_missing_id_alias_fails_before_listing_is_published() {
        let root = scratch("pnp-none-alias");
        let roots = fake_tree(&root);
        fs::remove_file(roots.dev.join("ttyS0")).unwrap();
        fs::write(roots.dev.join("pnp-target"), "").unwrap();
        symlink(roots.dev.join("pnp-target"), roots.dev.join("ttyS0")).unwrap();
        let result = comports_in(&roots);
        assert_eq!(result.unwrap_err().to_string(), "unsupported operand type(s) for +=: 'NoneType' and 'str'");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn checked_sysfs_enumeration_preserves_required_field_failures() {
        let root = scratch("required-fields");
        let roots = fake_tree(&root);
        let usb = root.join("sys/devices/usb1/1-1");
        fs::remove_file(usb.join("bNumInterfaces")).unwrap();
        assert!(comports_in(&roots).unwrap_err().to_string().contains("NoneType"));
        fs::write(usb.join("bNumInterfaces"), "not an integer").unwrap();
        assert!(comports_in(&roots).unwrap().iter().any(|p| p.location.as_deref() == Some("1-1")));
        fs::remove_file(usb.join("idVendor")).unwrap();
        assert_eq!(comports_in(&roots).unwrap_err().to_string(), "int() can't convert non-string with explicit base");
        fs::write(usb.join("idVendor"), "not hex").unwrap();
        assert!(comports_in(&roots).unwrap_err().to_string().contains("invalid literal for int() with base 16"));
        fs::write(usb.join("idVendor"), "+0x_1a86").unwrap();
        assert!(comports_in(&roots).unwrap().iter().any(|p| p.vid == Some(0x1a86)));
        fs::write(usb.join("bNumInterfaces"), "9".repeat(4301)).unwrap();
        assert!(comports_in(&roots).unwrap().iter().any(|p| p.location.as_deref() == Some("1-1")));
        fs::write(usb.join("bNumInterfaces"), "9".repeat(4300)).unwrap();
        assert!(comports_in(&roots).unwrap().iter().any(|p| p.location.as_deref() == Some("1-1:1.0")));
        fs::write(usb.join("manufacturer"), [0xff]).unwrap();
        assert_eq!(comports_in(&roots).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dangling_device_alias_keeps_both_resolved_link_labels() {
        let root = scratch("dangling-alias");
        let roots = fake_tree(&root);
        let alias = roots.dev.join("ttyUSBbroken");
        symlink("missing/device", &alias).unwrap();
        let details = try_sysfs_port(&roots, alias.to_str().unwrap()).unwrap();
        // The resolution also follows links above the alias (a scratch directory under a linked /home).
        let target = roots.dev.canonicalize().unwrap().join("missing/device").to_string_lossy().into_owned();
        assert_eq!(details.hwid, Some(format!("LINK={target} LINK={target}")));
        assert_eq!(details.description, "n/a");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_subsystem_target_still_preserves_the_platform_filter() {
        let root = scratch("broken-subsystem");
        let roots = fake_tree(&root);
        let subsystem = root.join("sys/devices/usb1/1-1/1-1:1.0/ttyUSB0/subsystem");
        fs::remove_file(&subsystem).unwrap();
        symlink(root.join("sys/bus/missing/platform"), &subsystem).unwrap();
        assert!(!comports_in(&roots).unwrap().iter().any(|details| details.name == "ttyUSB0"));
        fs::remove_dir_all(root).unwrap();
    }

    /// Checks the listing over disposable dev/sysfs fixtures against an external
    /// listing tool. No serial device is opened.
    #[test]
    #[ignore = "requires SRS pyserial bytecode via OPENRIGCUE_PYTHON_REFERENCE_PATH"]
    fn original_pyserial_enumeration_and_metadata() {
        let reference = std::env::var("OPENRIGCUE_PYTHON_REFERENCE_PATH").expect("original PYZ package path");
        let root = scratch("python-reference");
        let roots = fake_tree(&root);
        symlink(roots.dev.join("ttyUSB0"), roots.dev.join("ttyUSBalias")).unwrap();
        symlink("missing/device", roots.dev.join("ttyUSBbroken")).unwrap();
        symlink("ttyUSBloopB", roots.dev.join("ttyUSBloopA")).unwrap();
        symlink("ttyUSBloopA", roots.dev.join("ttyUSBloopB")).unwrap();
        for name in ["ttyUSB2", "ttyUSB12", "ttyXRUSB0", "ttyACM0", "ttyAMA0", "ttyAP0", "ttyGS0"] {
            fs::write(roots.dev.join(name), "").unwrap();
        }
        let usb = root.join("sys/devices/usb1/1-1");
        fs::write(usb.join("serial"), "SERIAL-A\n").unwrap();
        fs::write(usb.join("manufacturer"), "Vendor\n").unwrap();
        fs::write(usb.join("1-1:1.0/interface"), "Controller A\n").unwrap();
        fs::write(root.join("sys/devices/pnp0/00:01/id"), "PNP0501\n").unwrap();
        let script = r#"
import glob, os, sys
from serial.tools import list_ports_linux as ports
root = sys.argv[1]
original_glob = glob.glob
original_exists = os.path.exists
original_realpath = os.path.realpath
def remap(path):
    return root + path if path.startswith('/sys/class/tty/') else path
glob.glob = lambda pattern: original_glob(root + pattern)
os.path.exists = lambda path: original_exists(remap(path))
os.path.realpath = lambda path: original_realpath(remap(path))
fields = ('device', 'name', 'description', 'hwid', 'vid', 'pid', 'serial_number',
          'location', 'manufacturer', 'product', 'interface')
try:
    for port in ports.comports():
        print('\t'.join(str(getattr(port, key)) for key in fields))
except Exception as error:
    print('ERROR\t' + str(error))
"#;
        let limit = "9".repeat(4300);
        let over_limit = "9".repeat(4301);
        for interfaces in [
            "1",
            "2",
            "not numeric",
            "+0_2",
            "２０",
            "999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
            &limit,
            &over_limit,
        ] {
            fs::write(usb.join("bNumInterfaces"), interfaces).unwrap();
            let output = std::process::Command::new("python3")
                .args(["-B", "-c", script])
                .arg(&root)
                .env("PYTHONPATH", &reference)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let expected = String::from_utf8(output.stdout).unwrap();
            let actual: Vec<String> = comports_in(&roots)
                .unwrap()
                .into_iter()
                .map(|p| {
                    let optional = |value: Option<String>| value.unwrap_or_else(|| "None".into());
                    [
                        p.device,
                        p.name,
                        p.description,
                        optional(p.hwid),
                        p.vid.map(|n| n.to_string()).unwrap_or_else(|| "None".into()),
                        p.pid.map(|n| n.to_string()).unwrap_or_else(|| "None".into()),
                        optional(p.serial_number),
                        optional(p.location),
                        optional(p.manufacturer),
                        optional(p.product),
                        optional(p.interface),
                    ]
                    .join("\t")
                })
                .collect();
            assert_eq!(actual, expected.lines().map(str::to_owned).collect::<Vec<_>>());
        }
        fs::write(usb.join("bNumInterfaces"), "1").unwrap();
        for (field, value) in [("bNumInterfaces", None), ("idVendor", None), ("idVendor", Some("not hex"))] {
            let path = usb.join(field);
            let before = fs::read(&path).unwrap();
            match value {
                Some(value) => fs::write(&path, value).unwrap(),
                None => fs::remove_file(&path).unwrap(),
            }
            let output = std::process::Command::new("python3")
                .args(["-B", "-c", script])
                .arg(&root)
                .env("PYTHONPATH", &reference)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let expected = String::from_utf8(output.stdout).unwrap();
            let error = comports_in(&roots).unwrap_err();
            assert_eq!(format!("ERROR\t{}", error), expected.trim_end(), "{field}: {value:?}");
            fs::write(&path, before).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
