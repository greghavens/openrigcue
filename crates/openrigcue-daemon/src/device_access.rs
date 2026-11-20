//! The Linux missing-controller-access check.
//! Only actual permission-denied opens of adapters supported by our udev rules
//! trigger it; a disconnected port or a busy port is not a driver failure.

use std::path::{Path, PathBuf};

use openrigcue_devices::discovery::PortInfo;

use crate::state::{ServiceEvent, ServiceState};

fn supported(port: &PortInfo) -> bool {
    port.usb.as_ref().is_some_and(|usb| {
        matches!(
            (usb.vendor_id, usb.product_id),
            (0x1a86 | 0x2341, _) | (0x0483, 0x5740) | (0x0403, 0x6001) | (0x10c4, 0xea60)
        )
    })
}

fn blocked_devices(ports: &[PortInfo], denied: &[String]) -> Vec<String> {
    ports
        .iter()
        .filter(|port| {
            supported(port) && denied.iter().any(|path| path == &port.path || Some(path) == port.stable_path.as_ref())
        })
        .map(|port| port.path.clone())
        .collect()
}

/// Called after the normal controller scan, without opening any extra ports.
pub fn check(state: &ServiceState) {
    let denied = openrigcue_model::serial_access::take_denied();
    if denied.is_empty() {
        return;
    }
    let devices = blocked_devices(&openrigcue_devices::discovery::list_ports(), &denied);
    if devices.is_empty() {
        return;
    }
    if state.config.read().metadata_contains("notifications", "linux_device_access").unwrap_or(false) {
        return;
    }
    if state.notifications.write().device_permissions(&devices) {
        state.announce(ServiceEvent::Notifications);
    }
}

fn installation_files(appdir: Option<&Path>, executable: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    let roots = appdir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(executable.and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf));
    roots
        .filter_map(|root| {
            let helper = root.join("usr/lib/openrigcue/install-device-access");
            let rules = root.join("usr/share/openrigcue/70-openrigcue.rules");
            if helper.is_file() && rules.is_file() {
                return Some((helper, rules));
            }
            let helper = root.join("lib/openrigcue/install-device-access");
            let rules = root.join("share/openrigcue/70-openrigcue.rules");
            (helper.is_file() && rules.is_file()).then_some((helper, rules))
        })
        .next()
}

/// The notification's Install action deploys the packaged permissions and asks
/// udev to apply them. PolicyKit supplies the desktop authentication dialog.
pub fn install() -> Result<(), String> {
    let (helper, rules) = installation_files(
        std::env::var_os("APPDIR").as_deref().map(Path::new),
        std::env::current_exe().ok().as_deref(),
    )
    .ok_or("Device access installer is not available in this build")?;
    let output = std::process::Command::new("pkexec")
        .arg(helper)
        .arg(rules)
        .output()
        .map_err(|error| format!("Could not start device access installation: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(if detail.trim().is_empty() {
            "Device access installation was cancelled or failed".into()
        } else {
            detail.trim().to_owned()
        })
    }
}

async fn install_notification(
    ctx: crate::api::ApiContext,
    id: u64,
) -> Result<axum::http::StatusCode, crate::api::ApiError> {
    let available = ctx.state.notifications.read().items.iter().any(|item| {
        item.id == id && matches!(item.kind, crate::notifications::NotificationKind::DevicePermissions { .. })
    });
    if !available {
        return Err(crate::api::ApiError::not_found("no device access notification"));
    }
    tokio::task::spawn_blocking(install)
        .await
        .map_err(|error| crate::api::ApiError::internal(error.to_string()))?
        .map_err(crate::api::ApiError::internal)?;
    ctx.state.notifications.write().hide(id);
    ctx.state.announce(ServiceEvent::Notifications);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub(crate) async fn direct_call(
    ctx: crate::api::ApiContext,
    call: &crate::api::direct::Call<'_>,
) -> Result<Option<axum::response::Response>, crate::api::ApiError> {
    use axum::response::IntoResponse;
    use openrigcue_ui::backend::Method;
    match (call.method, call.segments.as_slice()) {
        (Method::Post, ["device-access", id, "install"]) => {
            Ok(Some(install_notification(ctx, call.parameter(id)?).await.into_response()))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_devices::discovery::UsbIdentity;

    fn port(path: &str, vendor_id: u16, product_id: u16) -> PortInfo {
        PortInfo {
            path: path.into(),
            stable_path: Some(format!("/dev/serial/by-id/{}", path.rsplit('/').next().unwrap())),
            usb: Some(UsbIdentity { vendor_id, product_id, serial_number: None, manufacturer: None, product: None }),
        }
    }

    #[test]
    fn only_denied_supported_devices_are_reported_including_stable_names() {
        let ports = [
            port("/dev/ttyUSB0", 0x1a86, 0x7523),
            port("/dev/ttyACM0", 0x2341, 0x8036),
            port("/dev/ttyUSB1", 0x9999, 1),
        ];
        assert_eq!(
            blocked_devices(
                &ports,
                &["/dev/ttyUSB0".into(), "/dev/serial/by-id/ttyACM0".into(), "/dev/ttyUSB1".into()]
            ),
            ["/dev/ttyUSB0", "/dev/ttyACM0"]
        );
        assert!(blocked_devices(&ports, &["/dev/disconnected".into()]).is_empty());
        assert!(blocked_devices(&ports, &[]).is_empty());
    }

    #[test]
    fn installer_requires_both_the_helper_and_the_packaged_rules() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let helper = root.join("usr/lib/openrigcue/install-device-access");
        let rules = root.join("usr/share/openrigcue/70-openrigcue.rules");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::create_dir_all(rules.parent().unwrap()).unwrap();
        std::fs::write(&helper, "helper").unwrap();
        assert!(installation_files(Some(root), None).is_none());
        std::fs::write(&rules, "rules").unwrap();
        assert_eq!(installation_files(Some(root), None), Some((helper, rules)));
    }
}
