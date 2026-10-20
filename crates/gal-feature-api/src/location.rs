// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Where the loader's files are: the directory `libgal_loader.so` was loaded
//! from (the loader finds it at construction and sets it here), and the file
//! system that holds a path (its mount point and whether it is read-only).
//!
//! A feature never names an absolute directory of its own: its files
//! (`ev_model.bin`, `gal_launch.conf`, ...) are in [`dir`], so the install
//! works from any directory the `LD_PRELOAD` entry points at (a unit with a
//! broken SD slot can use the other one, or internal flash).

use std::sync::OnceLock;

static DIR: OnceLock<String> = OnceLock::new();

/// Set the loader directory (loader constructor, once, before any feature
/// init). Later calls are ignored.
pub fn set_dir(dir: &str) {
    let _ = DIR.set(dir.to_string());
}

/// The directory the loader was loaded from.
///
/// # Panics
/// Panics when used before the loader sets its verified directory.
#[must_use]
pub fn dir() -> &'static str {
    DIR.get()
        .expect("loader directory is not initialized")
        .as_str()
}

/// `<dir>/<name>`.
#[must_use]
pub fn file(name: &str) -> String {
    format!("{}/{}", dir(), name)
}

/// A file in the loader directory whose path is built once, on first use,
/// and then has a stable address (for statics such as `ev_model.bin`).
pub struct LoaderFile {
    name: &'static str,
    path: OnceLock<String>,
}

impl LoaderFile {
    /// The file `name` in [`dir`].
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            path: OnceLock::new(),
        }
    }

    /// The full path. Call it only after the loader set the directory (from
    /// a feature's init or later). the first call fixes the path.
    pub fn path(&self) -> &str {
        self.path.get_or_init(|| file(self.name))
    }
}

/// How [`mount_point`] found the mount point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountFound {
    /// The file system's own answer (`devctl(DCMD_FSYS_MOUNTED_AT)`).
    Devctl,
    /// The highest directory above the path on the same device (`st_dev`).
    DeviceWalk,
}

/// The mount point of the file system that holds `path` (an existing file
/// or directory), without a trailing `/` (except for `/` itself).
#[must_use]
pub fn mount_point(path: &str) -> Option<(String, MountFound)> {
    if let Some(m) = mounted_at(path) {
        return Some((m, MountFound::Devctl));
    }
    device_walk(path).map(|m| (m, MountFound::DeviceWalk))
}

/// `Some(true)` when the file system that holds `path` is mounted
/// read-only, `None` when that cannot be read (off the target).
#[must_use]
pub fn read_only(path: &str) -> Option<bool> {
    statvfs_flag(path).map(|f| f & ST_RDONLY != 0)
}

const ST_RDONLY: u32 = 0x01;

/// Strip trailing `/` (not from `/` itself).
fn trim_slash(s: &str) -> &str {
    let t = s.trim_end_matches('/');
    if t.is_empty() && s.starts_with('/') {
        "/"
    } else {
        t
    }
}

/// Walk up from `path` while the parent is on the same device.
fn device_walk(path: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;
    let p = Path::new(path);
    if !p.is_absolute() {
        return None;
    }
    let dev = std::fs::metadata(p).ok()?.dev();
    let mut top = p;
    while let Some(parent) = top.parent() {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => top = parent,
            _ => break,
        }
    }
    top.to_str().map(|s| trim_slash(s).to_string())
}

#[cfg(target_os = "nto")]
mod nto {
    use core::ffi::{c_char, c_int, c_void};

    // sys/dcmd_blk.h (QNX 6.5.0 SP1): __DIOF(_DCMD_FSYS, 17, char[256]) =
    // (256 << 16) + (0x02 << 8) + 17 + _POSIX_DEVDIR_FROM.
    pub const DCMD_FSYS_MOUNTED_AT: c_int = 0x4100_0211;
    pub const O_RDONLY: c_int = 0;

    extern "C" {
        pub fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn devctl(
            fd: c_int,
            dcmd: c_int,
            data: *mut c_void,
            nbytes: usize,
            info: *mut c_int,
        ) -> c_int;
        pub fn statvfs(path: *const c_char, buf: *mut c_void) -> c_int;
    }
}

#[cfg(target_os = "nto")]
fn mounted_at(path: &str) -> Option<String> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { nto::open(c.as_ptr(), nto::O_RDONLY) };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    // SAFETY: an open fd. the command returns at most 256 bytes into buf.
    let rc = unsafe {
        nto::devctl(
            fd,
            nto::DCMD_FSYS_MOUNTED_AT,
            buf.as_mut_ptr().cast(),
            buf.len(),
            core::ptr::null_mut(),
        )
    };
    // SAFETY: the fd opened above.
    unsafe { nto::close(fd) };
    if rc != 0 {
        return None;
    }
    let s = core::ffi::CStr::from_bytes_until_nul(&buf)
        .ok()?
        .to_str()
        .ok()?;
    s.starts_with('/').then(|| trim_slash(s).to_string())
}

#[cfg(not(target_os = "nto"))]
fn mounted_at(_path: &str) -> Option<String> {
    None
}

#[cfg(target_os = "nto")]
fn statvfs_flag(path: &str) -> Option<u32> {
    let c = std::ffi::CString::new(path).ok()?;
    // sys/statvfs.h (QNX 6.5.0): struct statvfs is 168 bytes on 32-bit ARM,
    // f_flag at byte 76 (word 19) with either _FILE_OFFSET_BITS. The buffer
    // is larger than the struct.
    let mut buf = [0u32; 64];
    // SAFETY: a NUL-terminated path. buf is larger than struct statvfs.
    if unsafe { nto::statvfs(c.as_ptr(), buf.as_mut_ptr().cast()) } != 0 {
        return None;
    }
    Some(buf[19])
}

#[cfg(not(target_os = "nto"))]
fn statvfs_flag(_path: &str) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialized_directory_and_file_join() {
        static F: LoaderFile = LoaderFile::new("gal_launch.conf");
        set_dir("/qnx/installed-loader");
        assert_eq!(dir(), "/qnx/installed-loader");
        assert_eq!(file("ev_model.bin"), "/qnx/installed-loader/ev_model.bin");
        assert_eq!(F.path(), "/qnx/installed-loader/gal_launch.conf");
    }

    #[test]
    fn trims() {
        assert_eq!(trim_slash("/"), "/");
        assert_eq!(trim_slash("/fs/sda0/"), "/fs/sda0");
        assert_eq!(trim_slash("/fs/sda0"), "/fs/sda0");
    }

    #[test]
    fn walk_finds_a_mount_point_of_the_tmp_dir() {
        let d = std::env::temp_dir();
        let (m, how) = mount_point(d.to_str().unwrap()).unwrap();
        assert_eq!(how, MountFound::DeviceWalk);
        assert!(d.starts_with(&m), "{m} is not above {}", d.display());
        assert!(mount_point("relative/path").is_none());
        assert!(mount_point("/no/such/path/here").is_none());
    }
}
