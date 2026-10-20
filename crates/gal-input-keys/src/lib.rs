// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Routes steering-wheel keys to the active built-in cluster menu.
//! Keys the menu does not consume continue to the phone unchanged.

use core::ffi::{c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};

use gal_feature_api::hooks::{self, KeyEvent};
use gal_feature_api::FeatureModule;

pub const MODULE: FeatureModule = FeatureModule {
    name: "input",
    init,
};
const VERSION: &str = "6-rs";
type UiKeyFn = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;

fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    if let Ok(result) = catch_unwind(AssertUnwindSafe(f)) {
        result
    } else {
        qnx_slog::ms_log!(
            "GalInput",
            "PANIC contained in {site}; the event goes on to gal"
        );
        fallback
    }
}

fn ui_key_handler() -> Option<UiKeyFn> {
    gal_cluster_menu::key_handler()
}

fn key(ev: &KeyEvent) -> bool {
    match ui_key_handler() {
        // SAFETY: the active built-in menu exports this three-int callback.
        Some(handler) => unsafe {
            handler(ev.keycode, c_int::from(ev.down), c_int::from(ev.extra != 0)) != 0
        },
        None => false,
    }
}

fn on_key(_this: *mut c_void, ev: &KeyEvent) -> bool {
    guarded("input key hook", false, || key(ev))
}

fn init() {
    qnx_slog::ms_log!("GalInput", "input v{}", VERSION);
    hooks::INPUT_KEY.register(on_key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_pass_through_without_an_active_menu() {
        let event = KeyEvent {
            ts: 0,
            seq: 1,
            keycode: 87,
            down: true,
            extra: 0,
            long_press: false,
        };
        assert!(ui_key_handler().is_none());
        assert!(!on_key(core::ptr::null_mut(), &event));
    }

    #[test]
    fn panic_preserves_forwarding() {
        assert!(!guarded("test", false, || -> bool { panic!("deliberate") }));
        assert!(guarded("test", false, || true));
    }

    #[test]
    fn module_name_is_input() {
        assert_eq!(MODULE.name, "input");
    }
}
