// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! MOST isochronous transmit interface. Write complete 12032-byte MPEG transport blocks.
//! `FLUSH_QUEUE` clears the driver queue. START supplies the packet depth and creates the
//! transmit worker. `SET_VERBOSE` selects the driver log level. The control codes and
//! call signatures are interoperability facts. devctl returns zero or an errno value.
//! Host implementations return ENOSYS. host tests do not exercise the device.

#![no_std]

use core::ffi::{c_int, c_void, CStr};

/// The device path.
pub const DEVICE: &CStr = c"/dev/mlb/isoTX2";

/// `_DICMD(5, 6)`: flush the transmit queue. No data.
pub const DEVCTL_FLUSH_QUEUE: u32 = 0x0000_0506;
/// `_DIOT(5, 9, int)`: create the queue for `numTsPacketsMax` packets
/// (in, a multiple of 4, at most [`START_MAX_BYTES`] bytes) and the TX thread.
pub const DEVCTL_START: u32 = 0x8004_0509;
/// `_DIOT(5, 14, int)`: the driver's trace verbosity.
pub const DEVCTL_SET_VERBOSE: u32 = 0x8004_050E;

/// Warning level: the driver's per-frame `io_open` / `io_close` info lines
/// stop.
pub const VERBOSE_QUIET: i32 = 4;
/// The driver's launch default (`-v5`).
pub const VERBOSE_STOCK: i32 = 5;

/// The largest queue START accepts, in bytes (`cmp fp, #0xdc00`).
pub const START_MAX_BYTES: u32 = 0xdc00;

/// One MPEG-TS packet.
pub const TS_PACKET_SIZE: usize = 188;
/// Packets per block: the driver takes whole blocks.
pub const TS_BLOCK_PACKETS: usize = 64;
/// One write: 64 packets.
pub const TS_BLOCK_SIZE: usize = TS_PACKET_SIZE * TS_BLOCK_PACKETS;

/// `O_WRONLY` (QNX `fcntl.h` line 77).
pub const O_WRONLY: c_int = 0o1;
/// `O_NONBLOCK` (QNX `fcntl.h` line 90).
pub const O_NONBLOCK: c_int = 0o200;

/// `EAGAIN` (QNX `errno.h` line 64). `EWOULDBLOCK` is the same value.
pub const EAGAIN: c_int = 11;

#[cfg(target_os = "nto")]
mod sys {
    use core::ffi::{c_char, c_int, c_void};
    extern "C" {
        pub fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
        pub fn devctl(
            fd: c_int,
            dcmd: c_int,
            data: *mut c_void,
            nbytes: usize,
            info: *mut c_int,
        ) -> c_int;
    }
}

#[cfg(not(target_os = "nto"))]
#[allow(clippy::missing_safety_doc, clippy::unnecessary_wraps)]
mod sys {
    //! Host stand-ins: every call fails with `ENOSYS`.
    use core::ffi::{c_char, c_int, c_void};
    const ENOSYS: c_int = 38;
    pub unsafe fn open(_: *const c_char, _: c_int) -> c_int {
        qnx_errno::set_errno(ENOSYS);
        -1
    }
    pub unsafe fn close(_: c_int) -> c_int {
        0
    }
    pub unsafe fn write(_: c_int, _: *const c_void, _: usize) -> isize {
        qnx_errno::set_errno(ENOSYS);
        -1
    }
    pub unsafe fn devctl(_: c_int, _: c_int, _: *mut c_void, _: usize, _: *mut c_int) -> c_int {
        ENOSYS
    }
}

/// `open(DEVICE, O_WRONLY [| O_NONBLOCK])`: the descriptor, or the errno.
///
/// # Errors
/// The errno of a failed `open`.
pub fn open(nonblock: bool) -> Result<c_int, c_int> {
    let flags = if nonblock {
        O_WRONLY | O_NONBLOCK
    } else {
        O_WRONLY
    };
    // SAFETY: a NUL-terminated path. no mode argument without O_CREAT.
    let fd = unsafe { sys::open(DEVICE.as_ptr(), flags) };
    if fd < 0 {
        Err(qnx_errno::errno())
    } else {
        Ok(fd)
    }
}

/// `close(fd)`.
pub fn close(fd: c_int) {
    // SAFETY: closing a descriptor the caller owns.
    unsafe { sys::close(fd) };
}

#[allow(clippy::cast_possible_wrap)] // Preserve all command bits in the signed ABI parameter.
fn devctl(fd: c_int, dcmd: u32, data: *mut c_void, n: usize) -> c_int {
    // SAFETY: `data` is NULL with n = 0, or a live int with n = 4.
    unsafe { sys::devctl(fd, dcmd as c_int, data, n, core::ptr::null_mut()) }
}

/// `FLUSH_QUEUE`. `EOK` or an errno.
#[must_use]
pub fn flush_queue(fd: c_int) -> c_int {
    devctl(fd, DEVCTL_FLUSH_QUEUE, core::ptr::null_mut(), 0)
}

/// START with `*packets` (the driver may write a value back). `EOK` or an
/// errno.
#[must_use]
pub fn start(fd: c_int, packets: &mut c_int) -> c_int {
    devctl(
        fd,
        DEVCTL_START,
        core::ptr::from_mut(packets).cast(),
        core::mem::size_of::<c_int>(),
    )
}

/// `SET_VERBOSE` to `level`. `EOK` or an errno.
#[must_use]
pub fn set_verbose(fd: c_int, level: c_int) -> c_int {
    let mut l = level;
    devctl(
        fd,
        DEVCTL_SET_VERBOSE,
        (&raw mut l).cast(),
        core::mem::size_of::<c_int>(),
    )
}

/// One `write()`: the byte count, or the errno.
///
/// # Errors
/// The errno of a failed `write` (`EAGAIN` on a full queue with
/// `O_NONBLOCK`).
pub fn write(fd: c_int, block: &[u8]) -> Result<usize, c_int> {
    // SAFETY: a live buffer of block.len() bytes.
    let n = unsafe { sys::write(fd, block.as_ptr().cast(), block.len()) };
    usize::try_from(n).map_err(|_| qnx_errno::errno())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes are the `devctl.h` macros: `(sizeof(data) << 16) + (class
    /// << 8) + cmd + direction` (`_POSIX_DEVDIR_TO` = `0x8000_0000`).
    #[test]
    fn devctl_codes_are_the_header_encoding() {
        const TO: u32 = 0x8000_0000;
        let diot = |class: u32, cmd: u32| (4 << 16) + (class << 8) + cmd + TO;
        assert_eq!(DEVCTL_START, diot(5, 9));
        assert_eq!(DEVCTL_SET_VERBOSE, diot(5, 14));
        assert_eq!(DEVCTL_FLUSH_QUEUE, (5 << 8) + 6);
        assert_eq!(TS_BLOCK_SIZE, 12032);
        // 192 packets (the cluster's default) fit the driver's limit.
        assert!(192 * TS_PACKET_SIZE <= START_MAX_BYTES as usize);
    }

    #[test]
    fn host_stand_ins_fail() {
        if cfg!(not(target_os = "nto")) {
            assert_eq!(open(false), Err(38));
            assert_eq!(flush_queue(3), 38);
            assert_eq!(write(3, &[0; 4]), Err(38));
        }
    }
}
