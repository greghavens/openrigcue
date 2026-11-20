//! Winsock sequence of Update, executed on every
//! call whose message list is non-empty. The DLL imports exactly
//! socket (ord 23), bind (ord 2), sendto (ord 20), getaddrinfo and
//! freeaddrinfo from WS2_32.dll; it never calls WSAStartup (the host process
//! has Winsock initialised) and never calls closesocket, so every frame
//! creates a new socket that stays open. Return values of socket and sendto
//! are not checked; after a failed bind or getaddrinfo execution continues.
#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use core::ffi::{c_char, c_int, c_void};

use crate::telemetry::{
    bind_sockaddr, AF_INET, BIND_ADDR_LEN, DEST_ADDR_LEN, DEST_HOST, DEST_PORT, PROTOCOL, SEND_FLAGS, SOCK_DGRAM,
};

type SOCKET = usize;


#[repr(C)]
struct ADDRINFOA {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: usize,
    ai_canonname: *mut c_char,
    ai_addr: *mut c_void,
    ai_next: *mut ADDRINFOA,
}

// Import form: socket/bind/sendto by ordinal
// (23/2/20), getaddrinfo/freeaddrinfo by name.
#[link(name = "WS2_32", kind = "raw-dylib")]
extern "system" {
    #[link_ordinal(23)]
    fn socket(af: c_int, ty: c_int, protocol: c_int) -> SOCKET;
    #[link_ordinal(2)]
    fn bind(s: SOCKET, name: *const c_void, namelen: c_int) -> c_int;
    #[link_ordinal(20)]
    fn sendto(s: SOCKET, buf: *const c_char, len: c_int, flags: c_int, to: *const c_void, tolen: c_int) -> c_int;
}

#[link(name = "WS2_32", kind = "raw-dylib")]
extern "system" {
    fn getaddrinfo(node: *const c_char, service: *const c_char, hints: *const ADDRINFOA, res: *mut *mut ADDRINFOA) -> c_int;
    fn freeaddrinfo(ai: *mut ADDRINFOA);
}

#[repr(C, align(8))]
struct Storage([u8; DEST_ADDR_LEN as usize]);

/// One frame's transmission.
pub unsafe fn send_frame(text: &[u8]) {
    // socket(AF_INET, SOCK_DGRAM, 0)
    let s = socket(AF_INET, SOCK_DGRAM, PROTOCOL);

    // bind(s, {AF_INET, port 0, INADDR_ANY}, 16)
    let local = bind_sockaddr();
    let _ = bind(s, local.as_ptr() as *const c_void, BIND_ADDR_LEN);

    // getaddrinfo("127.0.0.1", "4123", {family AF_INET, socktype SOCK_DGRAM})
    let hints = ADDRINFOA {
        ai_flags: 0,
        ai_family: AF_INET,
        ai_socktype: SOCK_DGRAM,
        ai_protocol: PROTOCOL,
        ai_addrlen: 0,
        ai_canonname: core::ptr::null_mut(),
        ai_addr: core::ptr::null_mut(),
        ai_next: core::ptr::null_mut(),
    };
    let mut dest = Storage([0u8; DEST_ADDR_LEN as usize]);
    let mut res: *mut ADDRINFOA = core::ptr::null_mut();
    if getaddrinfo(
        DEST_HOST.as_ptr() as *const c_char,
        DEST_PORT.as_ptr() as *const c_char,
        &hints,
        &mut res,
    ) == 0
    {
        // memcpy(&dest, res->ai_addr, res->ai_addrlen); only the first result
        core::ptr::copy_nonoverlapping((*res).ai_addr as *const u8, dest.0.as_mut_ptr(), (*res).ai_addrlen);
        freeaddrinfo(res);
    }

    // sendto(s, text, strlen(text), 0, &dest, 0x80)
    let _ = sendto(
        s,
        text.as_ptr() as *const c_char,
        text.len() as c_int,
        SEND_FLAGS,
        dest.0.as_ptr() as *const c_void,
        DEST_ADDR_LEN,
    );
}
