//! The host part of an `AF_INET` socket address, turned into an
//! IPv4 address for a send:
//!
//! 1. host conversion: an ASCII text is taken as it is; any other text is encoded as IDNA 2003
//!    ([`super::idna2003`]), whose failure reads `encoding of hostname
//!    failed`; a result with a NUL is refused (`host name must not contain null character`);
//! 2. `""` is `INADDR_ANY`;
//! 3. `"255.255.255.255"` and `"<broadcast>"` are `INADDR_BROADCAST`;
//! 4. a dotted quad `inet_pton` accepts is taken as is, without a lookup;
//! 5. anything else goes to `getaddrinfo(host, NULL, {ai_family = AF_INET})`, whose first answer is used; a
//!    failure is a resolver error (`[Errno -2] Name or service not known`), or the OS error of `errno` for
//!    `EAI_SYSTEM`.
//!
//! The lookup is restricted to IPv4 by its hints, so a name with only IPv6 addresses fails in the resolver
//! instead of yielding an address the socket cannot use. `getaddrinfo` blocks the calling thread for as long
//! as the system resolver takes.

#![allow(unsafe_code)]

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::net::Ipv4Addr;

/// Why a host did not become an address. `Display` is the printed error text.
#[derive(Debug)]
pub(crate) enum HostError {
    /// `encoding of hostname failed`; the field is the text of the encoder's own error, which
    /// is not printed. It is kept for the debug form of this error only.
    Encoding(#[allow(dead_code)] String),
    /// `host name must not contain null character`.
    Null,
    /// A resolver error with its code and text.
    Lookup { code: i32, text: String },
    /// `EAI_SYSTEM`: the OS error of `errno`.
    System(io::Error),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Encoding(_) => f.write_str("encoding of hostname failed"),
            HostError::Null => f.write_str("host name must not contain null character"),
            HostError::Lookup { code, text } => write!(f, "[Errno {code}] {text}"),
            HostError::System(error) => f.write_str(&crate::printed::os_error(error)),
        }
    }
}

/// Host conversion: the host as the text the address conversion works on. It runs
/// before the port is looked at.
pub(crate) fn convert_host(host: &str) -> Result<Cow<'_, str>, HostError> {
    let converted = if host.is_ascii() {
        Cow::Borrowed(host)
    } else {
        Cow::Owned(super::idna2003::encode_hostname(host).map_err(HostError::Encoding)?)
    };
    if converted.contains('\0') {
        return Err(HostError::Null);
    }
    Ok(converted)
}

/// The IPv4 address a send uses for `host`.
pub(crate) fn resolve_ipv4_host(host: &str) -> Result<Ipv4Addr, HostError> {
    resolve_converted(&convert_host(host)?)
}

/// The address of a converted host.
pub(crate) fn resolve_converted(host: &str) -> Result<Ipv4Addr, HostError> {
    match host {
        "" => Ok(Ipv4Addr::UNSPECIFIED),
        "255.255.255.255" | "<broadcast>" => Ok(Ipv4Addr::BROADCAST),
        // `inet_pton(AF_INET, …)`: four decimal fields without leading zeros.
        host => match host.parse::<Ipv4Addr>() {
            Ok(address) => Ok(address),
            Err(_) => lookup(host),
        },
    }
}

#[cfg(target_os = "linux")]
fn lookup(host: &str) -> Result<Ipv4Addr, HostError> {
    use std::ffi::{c_char, c_int, CStr, CString};

    /// `struct sockaddr_in`.
    #[repr(C)]
    struct SockAddrIn {
        family: u16,
        port: u16,
        address: [u8; 4],
        zero: [u8; 8],
    }

    /// `struct addrinfo` (the Linux member order).
    #[repr(C)]
    struct AddrInfo {
        flags: c_int,
        family: c_int,
        socktype: c_int,
        protocol: c_int,
        addrlen: u32,
        addr: *mut SockAddrIn,
        canonname: *mut c_char,
        next: *mut AddrInfo,
    }

    extern "C" {
        fn getaddrinfo(
            node: *const c_char,
            service: *const c_char,
            hints: *const AddrInfo,
            result: *mut *mut AddrInfo,
        ) -> c_int;
        fn freeaddrinfo(result: *mut AddrInfo);
        fn gai_strerror(code: c_int) -> *const c_char;
    }

    const AF_INET: c_int = 2;
    const EAI_SYSTEM: c_int = -11;

    let node = CString::new(host).map_err(|_| HostError::Null)?;
    let hints = AddrInfo {
        flags: 0,
        family: AF_INET,
        socktype: 0,
        protocol: 0,
        addrlen: 0,
        addr: std::ptr::null_mut(),
        canonname: std::ptr::null_mut(),
        next: std::ptr::null_mut(),
    };
    let mut answers: *mut AddrInfo = std::ptr::null_mut();
    // SAFETY: `node` is a NUL-terminated string and `hints` a fully initialised `addrinfo`, both alive for the
    // call; `answers` receives a list the resolver owns until `freeaddrinfo`.
    let code = unsafe { getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut answers) };
    if code == EAI_SYSTEM {
        return Err(HostError::System(io::Error::last_os_error()));
    }
    if code != 0 {
        // SAFETY: `gai_strerror` returns a static NUL-terminated message for any code.
        let text = unsafe { CStr::from_ptr(gai_strerror(code)) }.to_string_lossy().into_owned();
        return Err(HostError::Lookup { code, text });
    }
    // SAFETY: on success `answers` is a non-empty list of `AF_INET` entries (the hints allow no other family)
    // whose `ai_addr` points at a `sockaddr_in`; the list is released exactly once below.
    let address = unsafe {
        let first = &*answers;
        let address = if first.addr.is_null() { None } else { Some(Ipv4Addr::from((*first.addr).address)) };
        freeaddrinfo(answers);
        address
    };
    address.ok_or_else(|| HostError::Lookup { code: -2, text: "Name or service not known".into() })
}

#[cfg(not(target_os = "linux"))]
fn lookup(host: &str) -> Result<Ipv4Addr, HostError> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let answers = (host, 0).to_socket_addrs().map_err(HostError::System)?;
    answers
        .filter_map(|address| match address {
            SocketAddr::V4(address) => Some(*address.ip()),
            SocketAddr::V6(_) => None,
        })
        .next()
        .ok_or_else(|| HostError::Lookup { code: 11001, text: "getaddrinfo failed".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases that need no resolver.
    #[test]
    fn literal_hosts_need_no_lookup() {
        assert_eq!(resolve_ipv4_host("").unwrap(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(resolve_ipv4_host("<broadcast>").unwrap(), Ipv4Addr::BROADCAST);
        assert_eq!(resolve_ipv4_host("255.255.255.255").unwrap(), Ipv4Addr::BROADCAST);
        assert_eq!(resolve_ipv4_host("192.168.1.50").unwrap(), Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(resolve_ipv4_host("a\0b").unwrap_err().to_string(), "host name must not contain null character");
    }

    /// IDNA encoding of non-ASCII names: published IDNA 2003 vectors (the IANA test domains, RFC 3490
    /// examples) and the Nameprep steps.
    #[test]
    fn non_ascii_hosts_are_encoded_with_the_idna_2003_codec() {
        let encoded = |host: &str| convert_host(host).map(|host| host.into_owned()).map_err(|error| error.to_string());
        for (host, expected) in [
            ("bücher.example", "xn--bcher-kva.example"),
            ("münchen.de", "xn--mnchen-3ya.de"),
            ("äöü.com", "xn--4ca0bs.com"),
            ("例え.テスト", "xn--r8jz45g.xn--zckzah"),
            ("日本語.jp", "xn--wgv71a119e.jp"),
            ("ドメイン名例.jp", "xn--eckwd4c7cu47r2wf.jp"),
            ("пример.испытание", "xn--e1afmkfd.xn--80akhbyknj4f"),
            ("δοκιμή", "xn--jxalpdlp"),
            ("مثال.إختبار", "xn--mgbh0fb.xn--kgbechtv"),
            ("טעסט", "xn--deba0ad"),
            // Map (B.2 case folding, B.1 to nothing) and NFKC before Punycode; ASCII labels stay as written.
            ("BÜCHER.EXAMPLE", "xn--bcher-kva.EXAMPLE"),
            ("straße.de", "strasse.de"),
            ("ﬁle.bücher", "file.xn--bcher-kva"),
            ("a\u{ad}b.bücher", "ab.xn--bcher-kva"),
            ("ｅｘａｍｐｌｅ.bücher", "example.xn--bcher-kva"),
            // The four dots of RFC 3490 section 3.1 separate labels; a trailing dot is kept.
            ("bücher。de", "xn--bcher-kva.de"),
            ("bücher．de．", "xn--bcher-kva.de."),
            ("bücher｡de", "xn--bcher-kva.de"),
        ] {
            assert_eq!(encoded(host), Ok(expected.to_owned()), "{host}");
        }
        // An encoding failure reads "encoding of hostname failed".
        for host in [
            "bücher..de",
            "\u{ad}.bücher",
            "xn--bücher.de",
            "bücher\u{e000}.de",
            "\u{5d0}a.de",
            "\u{5d0}1.de",
            "é\u{200e}.de",
            &"é".repeat(64),
        ] {
            assert_eq!(encoded(host), Err("encoding of hostname failed".to_owned()), "{host:?}");
        }
        // Punycode copies basic code points: a NUL survives the encoding and is refused afterwards.
        assert_eq!(encoded("\0é"), Err("host name must not contain null character".to_owned()));
        // An ASCII text is not encoded: an empty label goes to the resolver as written.
        assert_eq!(encoded("a..b"), Ok("a..b".to_owned()));
        assert_eq!(resolve_ipv4_host("bücher..de").unwrap_err().to_string(), "encoding of hostname failed");
    }

    /// Names go to the resolver with IPv4 hints; its failure reads `[Errno code] text`. The fixture's
    /// hosts file is the only source of names (`hosts: files`).
    #[test]
    #[cfg(target_os = "linux")]
    fn names_resolve_through_the_system_resolver_with_ipv4_hints() {
        assert_eq!(resolve_ipv4_host("localhost").unwrap(), Ipv4Addr::LOCALHOST);
        if std::fs::read_to_string("/etc/hosts").is_ok_and(|hosts| hosts.contains("second.fixture")) {
            assert_eq!(resolve_ipv4_host("second.fixture").unwrap(), Ipv4Addr::new(127, 0, 0, 2));
            assert_eq!(
                resolve_ipv4_host("absent.fixture").unwrap_err().to_string(),
                "[Errno -2] Name or service not known"
            );
            // A dotted text `inet_pton` refuses is a name for the resolver, which reads it as `inet_aton` does.
            assert_eq!(resolve_ipv4_host("127.0.0.010").unwrap(), Ipv4Addr::new(127, 0, 0, 8));
        }
    }
}
