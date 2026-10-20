// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Sensor feature, built into `libgal_loader.so`.
//!
//! An EXLAP worker reads vehicle values through the published `exlap` crate
//! and calls the receiver's sensor interfaces. Discovery can advertise extra
//! sensor types and the EV model. The model and raw EV trip messages use the
//! receiver's outgoing queue under the shared send lock.
//!
//! The display-state publisher publishes EV values at `/ramdisk/pps/gal/ev`. Each source
//! object is dropped before its session is released. Callbacks and the worker
//! contain panics. Field mappings are partly inferred. a successful build is
//! not evidence that every reported vehicle value is correct.

mod client;
mod discovery;
mod ev;
mod log;
pub mod parse;
mod sensors;

use std::panic::{catch_unwind, AssertUnwindSafe};

use gal_feature_api::hooks::{self, FuelData, SpeedData};
use gal_feature_api::session::{self, End};
use gal_feature_api::FeatureModule;
use gal_vendor_abi::{ConstPtr, Ptr};

use crate::log::{s_log, s_loge, StrError};

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule {
    name: "sensor",
    init,
};

/// The version in the banner: the single copy. Bump per change.
pub const VERSION: &str = "22-rs";

/// Run a callback body. a panic is logged and `fallback` returned.
fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    if let Ok(r) = catch_unwind(AssertUnwindSafe(f)) {
        r
    } else {
        s_loge!("[GalSensor] PANIC contained in {}; gal continues", site);
        fallback
    }
}

fn on_sensor_discovery(this: Ptr, sdr: Ptr) {
    guarded("sensor discovery hook", (), || {
        discovery::on_sensor_discovery(this, sdr);
    });
}

fn on_input_discovery(this: Ptr, sdr: Ptr) {
    guarded("input discovery hook", (), || {
        discovery::on_input_discovery(this, sdr);
    });
}

fn on_sensor_request(this: Ptr, req: ConstPtr) -> bool {
    guarded("sensor request hook", false, || {
        discovery::on_sensor_request(this, req)
    })
}

fn on_transport_read(this: Ptr, buf: ConstPtr, len: u32, ret: i32) {
    guarded("transport read hook", (), || {
        discovery::on_transport_read(this, buf, len, ret);
    });
}

fn on_fuel(this: Ptr, d: &mut FuelData) {
    guarded("fuel hook", (), || discovery::on_fuel(this, d));
}

fn on_speed(this: Ptr, d: &mut SpeedData) {
    guarded("speed hook", (), || discovery::on_speed(this, d));
}

fn on_session_end(why: End) {
    guarded("session end", (), || discovery::on_session_end(why));
}

/// The loader calls it once, on the constructor thread, panic-guarded.
fn init() {
    s_loge!("[GalSensor] sensor v{}", VERSION);
    // First use of each lock here, not on gal's threads.
    drop(sensors::src());
    ev::warm_up();
    ev::warm_up_batch();
    // The file paths in the loader directory are built (allocated) on
    // first use: here too.
    let _ = (ev::EV_MODEL_FILE.path(), ev::RAW_EV_TRIP_FILE.path());

    hooks::SENSOR_DISCOVERY.register(on_sensor_discovery);
    hooks::INPUT_DISCOVERY.register(on_input_discovery);
    hooks::SENSOR_REQUEST.register(on_sensor_request);
    hooks::TRANSPORT_READ.register(on_transport_read);
    hooks::FUEL.register(on_fuel);
    hooks::SPEED.register(on_speed);
    session::on_end(on_session_end);

    s_log!("[GalSensor] gal_feature_init: starting EXLAP client thread");
    let spawned = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| loop {
            // thread_main never returns. a panic that escaped it is
            // contained and the client starts over.
            if catch_unwind(client::thread_main).is_err() {
                s_loge!("[GalSensor] PANIC contained in the EXLAP thread; restarting it");
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        });
    if let Err(e) = spawned {
        s_loge!(
            "[GalSensor] pthread_create failed: {}",
            StrError(e.raw_os_error().unwrap_or(0))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_is_named_sensor() {
        assert_eq!(MODULE.name, "sensor");
    }

    #[test]
    fn a_panic_in_a_hook_is_contained() {
        assert!(!guarded("test", false, || -> bool { panic!("deliberate") }));
    }
}
