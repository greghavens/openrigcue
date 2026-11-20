#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{comports, list_ports, port_details, ports_with_details, try_comports};

/// Other platforms: the serialport crate's enumeration (no USB identity yet).
#[cfg(not(target_os = "linux"))]
pub fn list_ports() -> Vec<crate::discovery::PortInfo> {
    let mut ports: Vec<_> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| crate::discovery::PortInfo { path: p.port_name, stable_path: None, usb: None })
        .collect();
    ports.sort_by(|a, b| a.path.cmp(&b.path));
    ports
}

/// Other platforms: entries with the default description and hardware id only.
#[cfg(not(target_os = "linux"))]
pub fn port_details(device: &str) -> crate::discovery::ListPortInfo {
    let name = std::path::Path::new(device).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::discovery::ListPortInfo::new(device, &name)
}

/// Other platforms: [`port_details`] of every listed port.
#[cfg(not(target_os = "linux"))]
pub fn comports() -> Vec<crate::discovery::ListPortInfo> {
    list_ports().iter().map(|p| port_details(&p.path)).collect()
}

#[cfg(not(target_os = "linux"))]
pub fn try_comports() -> std::io::Result<Vec<crate::discovery::ListPortInfo>> {
    Ok(comports())
}

#[cfg(not(target_os = "linux"))]
pub fn ports_with_details() -> std::io::Result<Vec<(crate::discovery::PortInfo, crate::discovery::ListPortInfo)>> {
    Ok(list_ports()
        .into_iter()
        .map(|port| {
            let details = port_details(&port.path);
            (port, details)
        })
        .collect())
}
