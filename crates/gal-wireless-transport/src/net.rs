// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use core::ffi::{c_char, c_int, c_void, CStr};

#[cfg(target_os = "nto")]
mod consts {
    use core::ffi::c_int;
    pub const SOL_SOCKET: c_int = 0xffff;
    pub const SO_REUSEADDR: c_int = 0x0004;
}
#[cfg(not(target_os = "nto"))]
mod consts {
    use core::ffi::c_int;
    pub const SOL_SOCKET: c_int = 1;
    pub const SO_REUSEADDR: c_int = 2;
}
pub use consts::{SOL_SOCKET, SO_REUSEADDR};

pub const AF_INET: c_int = 2;
pub const SOCK_STREAM: c_int = 1;
pub const IPPROTO_TCP: c_int = 6;
pub const TCP_NODELAY: c_int = 1;
pub const SHUT_RDWR: c_int = 2;
pub const EINTR: c_int = 4;

/// `struct sockaddr_in` (QNX: `sin_len`, then a one-byte `sin_family`).
#[cfg(target_os = "nto")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_field_names)]
pub struct SockaddrIn {
    pub sin_len: u8,
    pub sin_family: u8,
    /// Network byte order.
    pub sin_port: u16,
    /// Network byte order.
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

/// `struct sockaddr_in` (Linux, host tests).
#[cfg(not(target_os = "nto"))]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_field_names)]
pub struct SockaddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<SockaddrIn>() == 16);

/// `socklen_t` is 32 bits on both.
pub type Socklen = u32;

/// `sizeof(struct sockaddr_in)` (16, asserted above).
#[allow(clippy::cast_possible_truncation)] // 16
pub const SOCKADDR_IN_LEN: Socklen = core::mem::size_of::<SockaddrIn>() as Socklen;
/// `sizeof(int)`.
#[allow(clippy::cast_possible_truncation)] // 4
const INT_LEN: Socklen = core::mem::size_of::<c_int>() as Socklen;

impl SockaddrIn {
    #[must_use]
    pub fn any(port: u16) -> Self {
        SockaddrIn {
            // AF_INET is 2. the field is one byte on QNX, two on Linux.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            sin_family: AF_INET as _,
            sin_port: port.to_be(),
            ..SockaddrIn::default()
        }
    }

    /// The address as `inet_ntoa` prints it.
    #[must_use]
    pub fn ip(&self) -> [u8; 4] {
        // Network order in memory: the first byte is the first number.
        self.sin_addr.to_ne_bytes()
    }

    /// `ntohs(sin_port)`.
    #[must_use]
    pub fn port(&self) -> u16 {
        u16::from_be(self.sin_port)
    }
}

extern "C" {
    pub fn socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    pub fn setsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        val: *const c_void,
        len: Socklen,
    ) -> c_int;
    pub fn bind(fd: c_int, addr: *const SockaddrIn, len: Socklen) -> c_int;
    pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    pub fn accept(fd: c_int, addr: *mut SockaddrIn, len: *mut Socklen) -> c_int;
    pub fn recv(fd: c_int, buf: *mut c_void, n: usize, flags: c_int) -> isize;
    pub fn send(fd: c_int, buf: *const c_void, n: usize, flags: c_int) -> isize;
    pub fn shutdown(fd: c_int, how: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    fn strerror(e: c_int) -> *const c_char;
    fn getenv(name: *const c_char) -> *const c_char;
}

pub fn set_int_opt(fd: c_int, level: c_int, name: c_int) {
    let one: c_int = 1;
    // SAFETY: a valid int and its size.
    unsafe { setsockopt(fd, level, name, (&raw const one).cast(), INT_LEN) };
}

#[must_use]
pub fn strerror_errno() -> &'static str {
    // SAFETY: strerror returns a NUL-terminated string that lives as long as
    // the libc (static table on QNX 6.5).
    let p = unsafe { strerror(qnx_errno::errno()) };
    if p.is_null() {
        return "?";
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("?")
}

/// `getenv(name)` as bytes, or None when unset.
#[must_use]
pub fn env(name: &CStr) -> Option<&'static [u8]> {
    // SAFETY: a NUL-terminated name. the result points into the process
    // environment, which nothing rewrites after the loader's constructor.
    let p = unsafe { getenv(name.as_ptr()) };
    if p.is_null() {
        return None;
    }
    // SAFETY: getenv returns a NUL-terminated string.
    Some(unsafe { CStr::from_ptr(p) }.to_bytes())
}
