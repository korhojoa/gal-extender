// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The libc calls the cluster makes directly, with QNX 6.5's constants
//! (SDP `fcntl.h`, `signal.h`, `sys/wait.h`). File access here is plain
//! open/read/write on stack buffers: these run on gal's threads and must not
//! allocate.

use core::ffi::{c_char, c_int, c_void, CStr};

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
/// QNX `fcntl.h` line 107.
#[cfg(target_os = "nto")]
pub const O_CREAT: c_int = 0o400;
#[cfg(not(target_os = "nto"))]
pub const O_CREAT: c_int = 0o100;

extern "C" {
    pub fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    pub fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    pub fn close(fd: c_int) -> c_int;
    /// `mode_t` is 32 bits on QNX 6.5 and on the host.
    pub fn mkdir(path: *const c_char, mode: u32) -> c_int;
    pub fn unsetenv(name: *const c_char) -> c_int;
    pub fn strerror(e: c_int) -> *const c_char;
    pub fn raise(sig: c_int) -> c_int;

}

/// `strerror(e)` as a str ("?" when not UTF-8).
pub fn strerror_str(e: c_int) -> &'static str {
    // SAFETY: strerror returns a static NUL-terminated string.
    let p = unsafe { strerror(e) };
    if p.is_null() {
        return "?";
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("?")
}

/// Read up to `buf.len()` bytes of `path` (one `read`). None when the file
/// cannot be opened.
pub fn read_file(path: &CStr, buf: &mut [u8]) -> Option<usize> {
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { open(path.as_ptr(), O_RDONLY) };
    if fd < 0 {
        return None;
    }
    let mut n = 0usize;
    while n < buf.len() {
        // SAFETY: the unfilled tail of `buf`.
        let r = unsafe { read(fd, buf[n..].as_mut_ptr().cast(), buf.len() - n) };
        match usize::try_from(r) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    // SAFETY: our descriptor.
    unsafe { close(fd) };
    Some(n)
}

/// Signals and `sigaction` (QNX `signal.h`): numbers lines 96-112,
/// `SA_NODEFER` line 164, `_NSIG` 57 (line 134), `struct sigaction` lines
/// 73-89 (handler, flags, an 8-byte `sigset_t`).
pub mod sig {
    use core::ffi::c_int;

    pub const SIGHUP: c_int = 1;
    pub const SIGINT: c_int = 2;
    pub const SIGQUIT: c_int = 3;
    pub const SIGILL: c_int = 4;
    pub const SIGABRT: c_int = 6;
    pub const SIGFPE: c_int = 8;
    pub const SIGBUS: c_int = 10;
    pub const SIGSEGV: c_int = 11;
    pub const SIGTERM: c_int = 15;
    /// `_NSIG`.
    pub const NSIG: usize = 57;
    pub const SA_NODEFER: c_int = 0x0010;
    pub const SIG_BLOCK: c_int = 0;

    /// `struct sigaction` on QNX 6.5 (16 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SigAction {
        pub handler: usize,
        pub flags: c_int,
        pub mask: [u32; 2],
    }

    impl SigAction {
        pub const ZERO: SigAction = SigAction {
            handler: 0,
            flags: 0,
            mask: [0; 2],
        };
    }

    #[cfg(all(target_os = "nto", target_pointer_width = "32"))]
    const _: () = assert!(core::mem::size_of::<SigAction>() == 16);

    #[cfg(target_os = "nto")]
    extern "C" {
        pub fn sigaction(sig: c_int, act: *const SigAction, old: *mut SigAction) -> c_int;
        pub fn pthread_sigmask(how: c_int, set: *const [u32; 2], old: *mut [u32; 2]) -> c_int;
    }

    /// The host build never installs handlers (unit tests only).
    #[cfg(not(target_os = "nto"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn sigaction(_: c_int, _: *const SigAction, _: *mut SigAction) -> c_int {
        -1
    }

    #[cfg(not(target_os = "nto"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn pthread_sigmask(_: c_int, _: *const [u32; 2], _: *mut [u32; 2]) -> c_int {
        0
    }
}
