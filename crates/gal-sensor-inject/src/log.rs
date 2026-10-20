// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! `GalSensor` log formatting uses `CLOCK_REALTIME` in local time. The message is limited
//! to 399 bytes and the complete line to 447 bytes. Fixed stack buffers avoid
//! allocation.

use core::ffi::{c_char, c_int, CStr};
use core::fmt::{self, Write as _};

use qnx_slog::log::{write_wall_stamp, Line};

pub fn log_at(sev: c_int, args: fmt::Arguments<'_>) {
    let mut msg: Line<400> = Line::new();
    let _ = msg.write_fmt(args);
    let mut line: Line<448> = Line::new();
    write_wall_stamp(&mut line);
    let _ = line.write_str(msg.as_str());
    qnx_slog::log::raw(sev, format_args!("{}\n", line.as_str()));
}

/// `sens_log`: INFO.
macro_rules! s_log {
    ($($a:tt)*) => {
        $crate::log::log_at(::qnx_slog::sys::SLOG_INFO, format_args!($($a)*))
    };
}
/// `sens_loge`: ERROR.
macro_rules! s_loge {
    ($($a:tt)*) => {
        $crate::log::log_at(::qnx_slog::sys::SLOG_ERROR, format_args!($($a)*))
    };
}
pub(crate) use {s_log, s_loge};

/// A pointer as QNX 6.5's `%p` prints it: lower-case hex, no `0x`, NULL as
/// `0` (measured on the QNX ARM bench, crates/gal-vendor-uplink `CPtr`).
pub struct CPtr(pub usize);

impl fmt::Display for CPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// Bytes printed as `%s` prints them: up to the first NUL. a byte that is
/// not valid UTF-8 becomes U+FFFD (EXLAP and file text are ASCII in
/// practice).
pub struct CBytes<'a>(pub &'a [u8]);

impl fmt::Display for CBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = crate::parse::cstr(self.0);
        for c in b.utf8_chunks() {
            f.write_str(c.valid())?;
            if !c.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

/// `%.Ns`: at most `n` bytes of `b`, as [`CBytes`].
pub struct CBytesN<'a>(pub &'a [u8], pub usize);

impl fmt::Display for CBytesN<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = crate::parse::cstr(self.0);
        CBytes(&b[..b.len().min(self.1)]).fmt(f)
    }
}

extern "C" {
    fn strerror(e: c_int) -> *const c_char;
}

/// `strerror(e)`, as text.
pub struct StrError(pub i32);

impl fmt::Display for StrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: strerror returns a NUL-terminated static string.
        let p = unsafe { strerror(self.0) };
        if p.is_null() {
            return write!(f, "error {}", self.0);
        }
        // SAFETY: as above.
        CBytes(unsafe { CStr::from_ptr(p) }.to_bytes()).fmt(f)
    }
}

/// Read the calling thread errno immediately after a failed call.
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
