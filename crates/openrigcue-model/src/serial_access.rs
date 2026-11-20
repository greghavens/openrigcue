//! Failed device opens shared by the motion and accessory transports.

use std::collections::BTreeSet;
use std::sync::Mutex;

static DENIED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Record an actual permission failure. A successful open clears a stale
/// failure for that path. No extra device is opened by this diagnostic.
pub fn record(path: &str, error: Option<std::io::ErrorKind>) {
    update(&mut DENIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner), path, error);
}

fn update(denied: &mut BTreeSet<String>, path: &str, error: Option<std::io::ErrorKind>) {
    match error {
        Some(std::io::ErrorKind::PermissionDenied) => {
            denied.insert(path.to_owned());
        }
        None => {
            denied.remove(path);
        }
        _ => {}
    }
}

/// Drain failures after a controller scan.
pub fn take_denied() -> Vec<String> {
    std::mem::take(&mut *DENIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_permission_failures_are_recorded_and_recovery_clears_them() {
        let mut denied = BTreeSet::new();
        update(&mut denied, "busy", Some(std::io::ErrorKind::ResourceBusy));
        update(&mut denied, "missing", Some(std::io::ErrorKind::NotFound));
        assert!(denied.is_empty());
        update(&mut denied, "controller", Some(std::io::ErrorKind::PermissionDenied));
        update(&mut denied, "controller", Some(std::io::ErrorKind::PermissionDenied));
        assert_eq!(denied.len(), 1);
        update(&mut denied, "controller", None);
        assert!(denied.is_empty());
    }
}
