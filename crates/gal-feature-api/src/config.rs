// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Read `gal_loader.conf` through the linked extender-settings library.
//! The loader installs the reader callbacks and selected path before feature
//! initialization. Before that, values use their defaults and no feature is enabled.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, CStr};
use core::sync::atomic::{AtomicUsize, Ordering};

/// `extender_settings_get_int(key, def)`.
pub type GetIntFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
/// `extender_settings_feature(name)`: non-zero when the bare word is in the conf.
pub type FeatureFn = unsafe extern "C" fn(*const c_char) -> c_int;
/// `extender_settings_loaded(name, state)`: -1 loading, 0 disabled, 1 loaded.
pub type LoadedFn = unsafe extern "C" fn(*const c_char, c_int);

static GET_INT: AtomicUsize = AtomicUsize::new(0);
static FEATURE: AtomicUsize = AtomicUsize::new(0);
static LOADED: AtomicUsize = AtomicUsize::new(0);

/// Install the reader's calls (loader constructor).
pub fn install(get_int: GetIntFn, feature: FeatureFn, loaded: LoadedFn) {
    GET_INT.store(get_int as usize, Ordering::Release);
    FEATURE.store(feature as usize, Ordering::Release);
    LOADED.store(loaded as usize, Ordering::Release);
}

/// An integer setting, or `def` when the key is absent or no conf is open.
#[must_use]
pub fn get_int(key: &CStr, def: i32) -> i32 {
    get_int_raw(key.as_ptr(), def)
}

fn get_int_raw(key: *const c_char, def: c_int) -> c_int {
    let f = GET_INT.load(Ordering::Acquire);
    if f == 0 {
        return def;
    }
    // SAFETY: stored by `install` from a GetIntFn.
    unsafe { core::mem::transmute::<usize, GetIntFn>(f)(key, def) }
}

/// True when `name` is enabled (a bare word) in the conf.
#[must_use]
pub fn feature_enabled(name: &CStr) -> bool {
    let f = FEATURE.load(Ordering::Acquire);
    // SAFETY: stored by `install` from a FeatureFn.
    f != 0 && unsafe { core::mem::transmute::<usize, FeatureFn>(f)(name.as_ptr()) } != 0
}

/// Report a feature's state to the settings status (-1, 0, 1).
pub fn set_loaded(name: &CStr, state: i32) {
    let f = LOADED.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: stored by `install` from a LoadedFn.
        unsafe { core::mem::transmute::<usize, LoadedFn>(f)(name.as_ptr(), state) };
    }
}

/// The conf path the loader opened, NUL-terminated, "" until set.
struct CfgPath(UnsafeCell<[u8; CFG_PATH_CAP]>);
// SAFETY: written once by the constructor before any reader exists.
unsafe impl Sync for CfgPath {}
/// Configuration paths have capacity for 255 bytes and a terminating NUL.
const CFG_PATH_CAP: usize = 256;
static CFG_PATH: CfgPath = CfgPath(UnsafeCell::new([0; CFG_PATH_CAP]));

/// Set the conf path (loader constructor only, before features load). Cut at
/// 255 bytes.
pub fn set_path(p: &str) {
    let n = p.len().min(CFG_PATH_CAP - 1);
    // SAFETY: constructor only, before any reader (see CfgPath).
    unsafe {
        let b = &mut *CFG_PATH.0.get();
        b[..n].copy_from_slice(&p.as_bytes()[..n]);
        b[n] = 0;
    }
}

/// The conf path as a C string with a static address.
#[must_use]
pub fn path_ptr() -> *const c_char {
    CFG_PATH.0.get().cast()
}

/// The conf path.
#[must_use]
pub fn path() -> &'static CStr {
    // SAFETY: always NUL-terminated (zeroed, then set_path writes a NUL). the
    // buffer is static and written only before any reader.
    unsafe { CStr::from_ptr(path_ptr()) }
}
