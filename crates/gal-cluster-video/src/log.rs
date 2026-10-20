// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Timestamped cluster diagnostics and fixed-buffer formatting helpers.
//! The retired crash recorder and message byte-dump paths are removed.

use core::fmt;
#[cfg(test)]
use core::fmt::Write as _;

#[cfg(test)]
use qnx_slog::log::Line;

macro_rules! cl_log {
    ($($a:tt)*) => {
        ::qnx_slog::log::stamped(::qnx_slog::sys::SLOG_INFO, format_args!($($a)*))
    };
}
macro_rules! cl_loge {
    ($($a:tt)*) => {
        ::qnx_slog::log::stamped(::qnx_slog::sys::SLOG_ERROR, format_args!($($a)*))
    };
}
pub(crate) use {cl_log, cl_loge};

/// Print a pointer as lowercase hexadecimal without a prefix. A null pointer is zero.
#[derive(Clone, Copy)]
pub struct CPtr(pub usize);

impl CPtr {
    pub fn of<T>(p: *const T) -> CPtr {
        CPtr(p as usize)
    }
}

impl fmt::Display for CPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// Print each byte as two lowercase hexadecimal digits followed by a space.
pub struct HexSp<'a>(pub &'a [u8]);

impl fmt::Display for HexSp<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x} ")?;
        }
        Ok(())
    }
}

/// Monotonic milliseconds (`now_ms`, `CLOCK_MONOTONIC`).
pub fn now_ms() -> u64 {
    qnx_time::mono_ms()
}

/// Monotonic microseconds (`mono_us`).
pub fn mono_us() -> u64 {
    let ts = qnx_time::now(qnx_time::CLOCK_MONOTONIC);
    u64::try_from(ts.tv_sec).unwrap_or(0) * 1_000_000
        + u64::try_from(ts.tv_nsec).unwrap_or(0) / 1000
}

/// A `c_long` as i64.
#[allow(
    clippy::useless_conversion,
    reason = "c_long is i32 on the QNX target and i64 on the 64-bit host"
)]
pub fn long(x: core::ffi::c_long) -> i64 {
    i64::from(x)
}

/// `CLOCK_REALTIME` seconds (`time_t` comparisons).
pub fn wall_s() -> i64 {
    long(qnx_time::now(qnx_time::CLOCK_REALTIME).tv_sec)
}

/// `CLOCK_MONOTONIC` seconds: the uptime on QNX.
pub fn uptime_s() -> i64 {
    long(qnx_time::now(qnx_time::CLOCK_MONOTONIC).tv_sec)
}

/// `(unsigned long)x` of a 64-bit value on the 32-bit target: the low word.
#[allow(clippy::cast_possible_truncation)]
pub fn ul(x: u64) -> u32 {
    x as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_print_like_qnx_percent_p() {
        let mut l: Line<32> = Line::new();
        let _ = write!(l, "{} {}", CPtr(0x0032_4320), CPtr(0));
        assert_eq!(l.as_str(), "324320 0");
    }

    #[test]
    fn hex_has_a_trailing_space() {
        let mut l: Line<32> = Line::new();
        let _ = write!(l, "{}", HexSp(&[0x0a, 0x40, 0xff]));
        assert_eq!(l.as_str(), "0a 40 ff ");
    }

    #[test]
    fn unsigned_long_is_the_low_word() {
        assert_eq!(ul(0x1_0000_0005), 5);
    }
}
