// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Android Auto cluster video, built into `libgal_loader.so`. Discovery adds video and
//! input endpoints. A bounded writer thread converts H.264 to MPEG transport packets
//! and writes isoTX2. Native display-manager calls control handover. PPS objects carry
//! status and HMI feedback. Keyframe requests have bounded backoff. Successful calls,
//! writes and selected contexts do not prove displayed pixels.

mod cfg;
mod crop;
pub mod cvs;
mod dm;
mod gate;
mod h264;
mod handover;
mod kick;
mod log;
#[cfg(test)]
mod mux_gate;
mod pipe;
mod route;
mod sdr;
mod sink_cycle;
mod slogread;
mod sys;
mod takeover;
pub mod ui;
mod vfn;
mod video;

use core::ffi::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};

use gal_feature_api::{hooks, FeatureModule};

use crate::log::cl_loge;

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule {
    name: "cluster",
    init,
};

/// Cargo declares semantic changes. the normal producer supplies the build number.
pub const VERSION: &str = match option_env!("GAL_CLUSTER_VERSION") {
    Some(version) => version,
    None => concat!(env!("CARGO_PKG_VERSION"), "+0"), // Host-only development identity.
};

/// Run a callback body. a panic is logged and `fallback` returned.
pub(crate) fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    if let Ok(r) = catch_unwind(AssertUnwindSafe(f)) {
        r
    } else {
        cl_loge!("[GalCluster] PANIC contained in {}; gal continues", site);
        fallback
    }
}

extern "C" {
    fn atexit(f: extern "C" fn()) -> c_int;
}

/// Restore the native cluster path at process exit.
extern "C" fn at_exit() {
    guarded("exit restore", (), || {
        if cfg::CFG.output() == 2 {
            pipe::most_close(&mut pipe::lock(&pipe::MOST));
        } else {
            dm::exit_restore();
        }
    });
}

/// `gal_feature_init`: the loader calls it once, on the constructor
/// thread, panic-guarded.
fn init() {
    ui::mark_loaded();
    // SAFETY: a NUL-terminated name.
    unsafe { sys::unsetenv(c"LD_PRELOAD".as_ptr()) };
    dm::install_fatal_restore();
    hooks::REGISTER_SERVICE.register(route::hook_register_service);
    hooks::VIDEO_DISCOVERY.register(route::hook_video_discovery);
    hooks::CHANNEL_OPENED.register(route::hook_channel_opened);
    hooks::UNEXPECTED_MSG.register(route::hook_unexpected_msg);
    hooks::TRANSPORT_READ.register(route::hook_transport_read);
    hooks::VIDEO_FOCUS_GRANT.register(route::hook_video_focus_grant);
    cfg::load();
    gate::handover_path_init();
    takeover::path_init();
    if cfg::CFG.output() != 2 {
        gate::helper_launch();
    }
    cl_loge!("[GalCluster] cluster {}", VERSION);
    pipe::warm_up();
    // SAFETY: a plain extern "C" fn with no arguments.
    unsafe { atexit(at_exit) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_module_is_named_as_the_conf_word() {
        assert_eq!(super::MODULE.name, "cluster");
    }
}
