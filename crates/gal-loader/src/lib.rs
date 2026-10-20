// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! `libgal_loader.so` is the preload entry. It opens configuration beside the loaded
//! library through extender-settings and starts enabled built-in modules. Owned work contains
//! Rust panics and logs failure. Original methods keep their native forwarding
//! contract. Entry trampolines align the stack before Rust calls. Callback declarations
//! state their 64-bit argument placement.

#![allow(clippy::missing_safety_doc)]
// The constructor is not linked into the unit-test binary, and off the ARM
// target nothing is exported (entry.rs), so the bodies look unused there.
#![cfg_attr(any(test, not(target_arch = "arm")), allow(dead_code))]

// slog output and the libc surface live in qnx-slog (third_party/qnx650-crates,
// used by this loader). re-exported so `crate::log`, `crate::sys` and
// the log macros keep working unchanged. The typed dlopen/dlsym helpers come
// from qnx-dl directly.
pub use qnx_slog::{ldr_log, ldr_loge, log, slog_raw, sys};

mod channel;
mod codec;
/// The exported symbols (stack-realigning trampolines). ARM target only.
#[cfg(target_arch = "arm")]
mod entry;
mod gal;
mod location;
mod nav_focus;
mod rx;
mod settings;
mod unavailable;
mod usbdi;

use std::ffi::CString;
use std::panic::{catch_unwind, AssertUnwindSafe};

use gal_feature_api::hooks as hk;
use gal_feature_api::{config, services, FeatureModule};

/// Cargo declares semantic changes. the normal producer supplies the build number.
const VERSION: &str = match option_env!("GAL_LOADER_VERSION") {
    Some(version) => version,
    None => concat!(env!("CARGO_PKG_VERSION"), "+0"), // Host-only development identity.
};

/// Run `f`, containing any panic. Returns None when it panicked.
pub fn guard<R>(site: &'static str, f: impl FnOnce() -> R) -> Option<R> {
    if let Ok(r) = catch_unwind(AssertUnwindSafe(f)) {
        Some(r)
    } else {
        slog_raw!(
            "[GalLoader] PANIC contained in {} (see the panic line above); gal continues\n",
            site
        );
        None
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(non-string payload)");
        let (file, line) = info.location().map_or(("?", 0), |l| (l.file(), l.line()));
        // No backtrace: on QNX 6.5 std's symbolizer prints every frame as
        // "<unknown>" (the QNX ARM bench), so it would only add noise lines.
        ldr_loge!("[GalLoader] PANIC at {}:{}: {}", file, line, msg);
    }));
}

fn readable(path: &str) -> bool {
    let Ok(c) = CString::new(path) else {
        return false;
    };
    // SAFETY: NUL-terminated path.
    unsafe { sys::access(c.as_ptr(), sys::R_OK) == 0 }
}

/// The built-in Rust features. Each selected feature has one implementation.
const BUILTIN_FEATURES: &[FeatureModule] = &[
    gal_input_keys::MODULE,
    gal_vendor_uplink::MODULE,
    gal_sensor_inject::MODULE,
    gal_cluster_video::MODULE,
    #[cfg(feature = "wireless")]
    gal_wireless_transport::MODULE,
    gal_daemon_launcher::MODULE,
    gal_cluster_menu::MODULE,
];

fn load_feature(cfg: &str, name: &str) {
    let Ok(cname) = CString::new(name) else {
        return;
    };
    config::set_loaded(&cname, -1);
    if !config::feature_enabled(&cname) {
        config::set_loaded(&cname, 0);
        ldr_log!("[GalLoader] feature '{}': disabled (not in {})", name, cfg);
        return;
    }
    if let Some(module) = BUILTIN_FEATURES.iter().find(|m| m.name == name) {
        ldr_log!("[GalLoader] feature '{}': built-in Rust module", name);
        if guard("feature init", module.init).is_some() {
            config::set_loaded(&cname, 1);
            ldr_log!("[GalLoader] feature '{}': built-in init done", name);
        }
        return;
    }
    ldr_loge!("[GalLoader] feature '{}': no built-in module", name);
}

fn loader_init() {
    install_panic_hook();
    ldr_loge!("[GalLoader] loader {}", VERSION);

    // Initialize the recursive send lock before the features.
    gal_feature_api::send_lock::init();
    services::install(services::LoaderServices {
        video_request_focus: gal::request_focus,
        usbdi_fire_insertion: usbdi::fire_insertion,
    });

    // The extension directory, before LD_PRELOAD is dropped (it is one of
    // the sources).
    let own = location::own_path();
    let resolved = location::resolve(
        std::env::var("GAL_LOADER_DIR").ok().as_deref(),
        own.as_deref(),
        std::env::var("LD_PRELOAD").ok().as_deref(),
    );
    // Drop the preload even when directory discovery fails. children must
    // never repeat this constructor.
    // SAFETY: NUL-terminated name. no other thread exists yet.
    unsafe { sys::unsetenv(c"LD_PRELOAD".as_ptr()) };
    let Some((dir, found)) = resolved else {
        ldr_loge!("[GalLoader] no absolute loader directory found - no features loaded");
        return;
    };
    ldr_loge!(
        "[GalLoader] extension dir {} (from {}; dladdr path {})",
        dir,
        found.describe(),
        own.as_deref().unwrap_or("(none)")
    );
    gal_feature_api::location::set_dir(&dir);
    match gal_feature_api::location::mount_point(&dir) {
        Some((m, how)) => ldr_log!(
            "[GalLoader] extension dir file system: mounted at {} ({:?}), {}",
            m,
            how,
            match gal_feature_api::location::read_only(&dir) {
                Some(true) => "read-only",
                Some(false) => "read-write",
                None => "mode unknown",
            }
        ),
        None => ldr_log!("[GalLoader] extension dir file system: mount point not found"),
    }

    let cfg = location::config_path(&dir);
    if !readable(&cfg) {
        ldr_loge!(
            "[GalLoader] no adjacent config at {} - no features loaded",
            cfg
        );
        return;
    }
    config::set_path(&cfg);
    ldr_log!("[GalLoader] loading config from {}", cfg);
    if !settings::open(&cfg) {
        return;
    }

    codec::configure(config::get_int(c"media_audio_codec", 0));
    nav_focus::configure(config::get_int(c"aa_suppress_navigation_active", 0));

    unavailable::report(&cfg);
    for name in [
        "cluster", "sensor", "wireless", "ui", "input", "launcher", "aauplink",
    ] {
        load_feature(&cfg, name);
    }

    ldr_log!(
        "[GalLoader] init complete: video_disc={} sensor_disc={} input_disc={} \
         register_service={} channel_opened={} unexpected_message={} fuel_callbacks={} speed_callbacks={} transport_read={} transport_write={}",
        hk::VIDEO_DISCOVERY.count(),
        hk::SENSOR_DISCOVERY.count(),
        hk::INPUT_DISCOVERY.count(),
        hk::REGISTER_SERVICE.count(),
        hk::CHANNEL_OPENED.count(),
        hk::UNEXPECTED_MSG.count(),
        hk::FUEL.count(),
        hk::SPEED.count(),
        hk::TRANSPORT_READ.count(),
        hk::TRANSPORT_WRITE.count()
    );
    channel::report("init complete");
}

extern "C" fn loader_ctor() {
    let _ = guard("loader_init", loader_init);
}

/// Load-time constructor registered in `.init_array`, before main.
#[cfg(not(test))]
#[used]
#[link_section = ".init_array"]
static LOADER_INIT: extern "C" fn() = loader_ctor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_modules() {
        let names: Vec<&str> = BUILTIN_FEATURES.iter().map(|m| m.name).collect();
        let mut expected = vec!["input", "aauplink", "sensor", "cluster"];
        if cfg!(feature = "wireless") {
            expected.push("wireless");
        }
        expected.extend(["launcher", "ui"]);
        assert_eq!(names, expected);
    }
}
