// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Loader configuration access through the shared `extender-settings` crate.
//!
//! The reader installs the feature API configuration callbacks. The settings
//! library is linked into the loader. no separate settings library is required.
//! The reader supplies configuration access without a UI status publisher.

use core::ffi::{c_char, c_int, c_uint};
use std::ffi::CString;

use gal_feature_api::config;

use crate::ldr_loge;

/// Find the settings override directory for the adjacent base configuration. The shared
/// settings crate owns this rule.
fn override_dir(base: &str) -> String {
    let dir = extender_settings::settings_dir(std::path::Path::new(base));
    let dir = dir.to_string_lossy().into_owned();
    ldr_loge!("[GalLoader] settings override directory {}", dir);
    dir
}

// The extender-settings exports (runtime.rs, #[no_mangle]), resolved at link time.
extern "C" {
    fn extender_settings_abi() -> c_uint;
    fn extender_settings_init(
        base: *const c_char,
        dir: *const c_char,
        status: *const c_char,
    ) -> c_int;
    fn extender_settings_get_int(key: *const c_char, def: c_int) -> c_int;
    fn extender_settings_feature(name: *const c_char) -> c_int;
    fn extender_settings_loaded(name: *const c_char, state: c_int);
}

/// Initialise the linked-in settings reader against `base`.
pub fn open(base: &str) -> bool {
    let Ok(cbase) = CString::new(base) else {
        return false;
    };
    let Ok(cdir) = CString::new(override_dir(base)) else {
        return false;
    };
    // SAFETY: ABI-1 signatures of extender-settings. NUL-terminated paths.
    let ok = unsafe {
        extender_settings_abi() == 1
            && extender_settings_init(cbase.as_ptr(), cdir.as_ptr(), core::ptr::null()) == 0
    };
    if !ok {
        ldr_loge!("[GalLoader] incompatible or failed settings reader; extensions unavailable");
        return false;
    }
    config::install(
        extender_settings_get_int,
        extender_settings_feature,
        extender_settings_loaded,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_the_linked_reader_without_a_status_publisher() {
        let directory =
            std::env::temp_dir().join(format!("gal-reader-init-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let base = directory.join("gal_loader.conf");
        std::fs::write(&base, "sensor\nreader_fixture_value=42\n").unwrap();
        let initialized = open(base.to_str().unwrap());
        let value = config::get_int(c"reader_fixture_value", -1);
        std::fs::remove_file(base).unwrap();
        std::fs::remove_dir(directory).unwrap();
        assert!(initialized);
        assert_eq!(value, 42);
    }

    #[test]
    fn the_conf_key_names_the_directory() {
        let p = std::env::temp_dir().join(format!("gal-loader-test-{}.conf", std::process::id()));
        std::fs::write(&p, "sensor\nextender_settings_dir=/media/card\n").unwrap();
        assert_eq!(override_dir(p.to_str().unwrap()), "/media/card");
        let _ = std::fs::remove_file(&p);
    }
}
