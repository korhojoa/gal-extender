// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Publish cluster takeover state in /ramdisk/pps/cluster/gal. Taking is set before
//! native navigation is hidden. Streaming is set on the first isoTX2 block. Both clear
//! when native navigation returns. Each publication uses one nontruncating write.
//! Failure is logged and is nonfatal. The path uses the selected PPS root. stack
//! buffers bound the writer work.

use core::ffi::{c_int, CStr};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::log::{cl_log, cl_loge};
use crate::sys;

const PPS_ROOT: &str = "/ramdisk/pps";
const TAKEOVER_DIR: &str = "/cluster";
const TAKEOVER_OBJ: &str = "/cluster/gal";

/// NUL-terminated takeover directory and path. Limit each to 159 bytes plus NUL.
static DIR: OnceLock<[u8; 161]> = OnceLock::new();
static PATH: OnceLock<[u8; 161]> = OnceLock::new();
static TAKING: AtomicBool = AtomicBool::new(false);
static STREAMING: AtomicBool = AtomicBool::new(false);

fn join(root: &str, tail: &str) -> [u8; 161] {
    let mut b = [0u8; 161];
    let mut n = 0;
    for &c in root.as_bytes().iter().chain(tail.as_bytes()) {
        if n < 159 && c != 0 {
            b[n] = c;
            n += 1;
        }
    }
    b
}

/// `cluster_takeover_path_init`.
pub fn path_init() {
    let root = std::env::var_os("GAL_PPS_ROOT")
        .filter(|r| !r.is_empty())
        .and_then(|r| r.into_string().ok());
    let root = root.as_deref().unwrap_or(PPS_ROOT);
    let _ = DIR.set(join(root, TAKEOVER_DIR));
    let _ = PATH.set(join(root, TAKEOVER_OBJ));
}

/// A fixed text buffer (no allocation).
struct Buf {
    b: [u8; 64],
    n: usize,
}

impl Buf {
    fn new() -> Self {
        Buf { b: [0; 64], n: 0 }
    }
    fn bytes(&self) -> &[u8] {
        &self.b[..self.n]
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(self.bytes()).unwrap_or("?")
    }
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.b.len() - 1 - self.n;
        let k = s.len().min(room);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn cstr(b: &[u8; 161]) -> Option<&CStr> {
    CStr::from_bytes_until_nul(b).ok()
}

/// `cluster_takeover_write`: one write of `text`. `what` names it in the log.
fn publish(what: &str, text: &[u8]) {
    let (Some(dir), Some(path)) = (DIR.get(), PATH.get()) else {
        return;
    };
    let (Some(dir), Some(path)) = (cstr(dir), cstr(path)) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let shown = path.to_str().unwrap_or("?");
    // PPS: a directory. EEXIST is the norm
    // SAFETY: a NUL-terminated path.
    let _ = unsafe { sys::mkdir(dir.as_ptr(), 0o777) };
    // SAFETY: a NUL-terminated path. the mode is the variadic argument.
    let fd = unsafe { sys::open(path.as_ptr(), sys::O_WRONLY | sys::O_CREAT, 0o666 as c_int) };
    if fd < 0 {
        let e = qnx_errno::errno();
        cl_loge!(
            "[GalCluster] takeover: publishing {} to {} FAILED at open errno={} ({}) -- not fatal",
            what,
            shown,
            e,
            sys::strerror_str(e)
        );
        return;
    }
    // SAFETY: a live buffer. our descriptor.
    let n = unsafe { sys::write(fd, text.as_ptr().cast(), text.len()) };
    if usize::try_from(n).ok() == Some(text.len()) {
        cl_log!("[GalCluster] takeover: published {} to {}", what, shown);
    } else {
        let e = if n < 0 { qnx_errno::errno() } else { 0 };
        cl_loge!(
            "[GalCluster] takeover: publishing {} to {} FAILED at write ({} of {}) errno={} ({}) -- not fatal",
            what,
            shown,
            n,
            text.len(),
            e,
            sys::strerror_str(e)
        );
    }
    // SAFETY: our descriptor.
    unsafe { sys::close(fd) };
}

/// Print uptime as an unsigned 32-bit millisecond count.
fn uptime_ms() -> u64 {
    (crate::log::mono_us() / 1000) & 0xFFFF_FFFF
}

/// `cluster_takeover_taking`: once per session.
pub fn taking() {
    if TAKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let ms = uptime_ms();
    let (mut text, mut what) = (Buf::new(), Buf::new());
    let _ = write!(text, "taking::1\ntaking_ms::{ms}\n");
    let _ = write!(what, "taking=1 taking_ms={ms}");
    publish(what.as_str(), text.bytes());
}

/// `cluster_takeover_streaming`: once per session, at the first block
/// written to isoTX2.
pub fn streaming() {
    if STREAMING.swap(true, Ordering::AcqRel) {
        return;
    }
    let ms = uptime_ms();
    let (mut text, mut what) = (Buf::new(), Buf::new());
    let _ = write!(text, "streaming::1\nstreaming_ms::{ms}\n");
    let _ = write!(what, "streaming=1 streaming_ms={ms}");
    publish(what.as_str(), text.bytes());
}

/// `cluster_takeover_clear`: nav has the panel again.
pub fn clear() {
    let was_taking = TAKING.swap(false, Ordering::AcqRel);
    let was_streaming = STREAMING.swap(false, Ordering::AcqRel);
    if !was_taking && !was_streaming {
        return;
    }
    publish("taking=0 streaming=0", b"taking::0\nstreaming::0\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_cut_like_snprintf() {
        let b = join("/x", "/cluster/gal");
        assert_eq!(cstr(&b).unwrap().to_bytes(), b"/x/cluster/gal");
        let long = "a".repeat(300);
        let b = join(&long, "/cluster/gal");
        assert_eq!(cstr(&b).unwrap().to_bytes().len(), 159);
    }

    #[test]
    fn the_texts() {
        let mut t = Buf::new();
        let _ = write!(t, "taking::1\ntaking_ms::{}\n", 57_704u64);
        assert_eq!(t.bytes(), b"taking::1\ntaking_ms::57704\n");
    }
}
