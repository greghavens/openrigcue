//! Server start policy: the listening address, and the legacy certificate-pair check.
//! TLS with that certificate pair is not supported, so a start that finds the pair fails before serving.

use std::path::Path;

pub(crate) fn imported_native_address(
    port: &openrigcue_model::native_value::LegacyValue,
) -> std::io::Result<std::net::SocketAddr> {
    let port = port
        .integer()
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| std::io::Error::other("OverflowError: bind(): port must be 0-65535."))?;
    Ok(std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port)))
}

#[cfg(test)]
fn imported_policy(port: i128, exists: impl FnMut(&Path) -> bool) -> std::io::Result<(std::net::SocketAddr, bool)> {
    imported_native_policy(&openrigcue_model::native_value::LegacyValue::from_integer(port.into()), exists)
}

pub(super) fn imported_native_policy(
    port: &openrigcue_model::native_value::LegacyValue,
    exists: impl FnMut(&Path) -> bool,
) -> std::io::Result<(std::net::SocketAddr, bool)> {
    let legacy_ssl = legacy_ssl_requested(exists);
    Ok((imported_native_address(port)?, legacy_ssl))
}

const CERTIFICATE: &str = "/home/srsusr/srs.local+4.pem";
const PRIVATE_KEY: &str = "/home/srsusr/srs.local+4-key.pem";

pub(super) fn legacy_ssl_requested(mut exists: impl FnMut(&Path) -> bool) -> bool {
    exists(Path::new(CERTIFICATE)) && exists(Path::new(PRIVATE_KEY))
}

pub(super) fn after_bind(legacy_ssl: bool) -> std::io::Result<()> {
    if legacy_ssl {
        Err(std::io::Error::other("AttributeError: module 'ssl' has no attribute 'wrap_socket'"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_certificate_pair_is_an_existence_only_short_circuit() {
        for (certificate, key, requested, calls) in [
            (false, false, false, vec![CERTIFICATE]),
            (false, true, false, vec![CERTIFICATE]),
            (true, false, false, vec![CERTIFICATE, PRIVATE_KEY]),
            (true, true, true, vec![CERTIFICATE, PRIVATE_KEY]),
        ] {
            let mut observed = Vec::new();
            assert_eq!(
                legacy_ssl_requested(|path| {
                    observed.push(path.to_str().unwrap().to_owned());
                    if path == Path::new(CERTIFICATE) {
                        certificate
                    } else {
                        key
                    }
                }),
                requested
            );
            assert_eq!(observed, calls);
        }
    }

    #[test]
    fn original_removed_ssl_api_fails_after_binding_before_serving() {
        for requested in [false, true] {
            let events = std::cell::RefCell::new(Vec::new());
            let started = (|| {
                events.borrow_mut().push("bound");
                after_bind(requested)?;
                events.borrow_mut().push("serve");
                std::io::Result::Ok(())
            })();
            if requested {
                assert_eq!(
                    started.unwrap_err().to_string(),
                    "AttributeError: module 'ssl' has no attribute 'wrap_socket'"
                );
                assert_eq!(*events.borrow(), ["bound"]);
            } else {
                assert!(started.is_ok());
                assert_eq!(*events.borrow(), ["bound", "serve"]);
            }
        }
    }

    #[test]
    fn native_web_port_range_follows_certificate_checks_before_binding() {
        let mut visited = Vec::new();
        let result = imported_policy(70_000, |path| {
            visited.push(path.to_owned());
            true
        });
        assert_eq!(visited, [Path::new(CERTIFICATE), Path::new(PRIVATE_KEY)]);
        assert_eq!(result.unwrap_err().to_string(), "OverflowError: bind(): port must be 0-65535.");
        let (address, ssl) = imported_policy(0, |_| true).unwrap();
        assert_eq!(address.port(), 0, "zero retains the socket's ephemeral-port behavior");
        assert!(ssl);
        assert!(after_bind(ssl).is_err());
    }

    #[test]
    fn wide_native_port_checks_certificates_before_range_failure() {
        use openrigcue_model::native_value::LegacyValue;
        let port = LegacyValue::from_integer(LegacyValue::from("9".repeat(400)).py_integer().unwrap());
        let mut visited = Vec::new();
        let result = imported_native_policy(&port, |path| {
            visited.push(path.to_owned());
            true
        });
        assert_eq!(visited, [Path::new(CERTIFICATE), Path::new(PRIVATE_KEY)]);
        assert_eq!(result.unwrap_err().to_string(), "OverflowError: bind(): port must be 0-65535.");
    }
}
