//! Address validation of the dialogs, and opening a URL in the desktop's browser.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Whether `text` is a valid IP address: a dotted IPv4 address without leading zeros,
/// or an IPv6 address, optionally with a non-empty `%scope` suffix that holds no further `%`.
pub fn is_ip_address(text: &str) -> bool {
    if text.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let (address, scope) = match text.split_once('%') {
        Some((address, scope)) => (address, Some(scope)),
        None => (text, None),
    };
    if scope.is_some_and(|s| s.is_empty() || s.contains('%')) {
        return false;
    }
    address.parse::<Ipv6Addr>().is_ok()
}

/// Opens `url` in the desktop's default browser (`xdg-open`), not waited for.
pub fn open_browser(url: &str) {
    let spawned = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        // Reaped by a thread of its own so no zombie stays behind.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(error) => tracing::warn!(%error, url, "webbrowser.open failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Games pages' port forwarding and PlayStation address inputs: these addresses are accepted.
    #[test]
    fn accepts_what_ipaddress_accepts() {
        for ok in ["127.0.0.1", "192.168.1.25", "255.255.255.255", "::1", "fe80::1%eth0", "::ffff:10.0.0.1"] {
            assert!(is_ip_address(ok), "{ok}");
        }
        for bad in [
            "",
            "localhost",
            "192.168.1",
            "192.168.1.256",
            "010.0.0.1",
            " 10.0.0.1",
            "fe80::1%",
            "fe80::1%a%b",
            "1.2.3.4:5",
        ] {
            assert!(!is_ip_address(bad), "{bad}");
        }
    }
}
