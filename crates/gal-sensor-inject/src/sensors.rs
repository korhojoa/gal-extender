// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! EXLAP sensor state and native report calls. SRC protects the captured `SensorSource`.
//! Hold SRC for every report, field access and outgoing message. Session teardown
//! clears it under the same lock and waits for any in-flight call. Lock order is SRC,
//! then the loader send lock. never reverse it.

use core::ffi::CStr;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use gal_feature_api::{config, send_lock};
use gal_vendor_abi::offsets as off;
use gal_vendor_abi::symbols as sym;
use gal_vendor_abi::Symbol;

use crate::log::{s_log, s_loge, CBytes};
use crate::parse;

// ---------------------------------------------------------------------------
// The captured objects
// ---------------------------------------------------------------------------

/// The `SensorSource` gal handed to `addDiscoveryInfo` (0: none, or the
/// session ended). See the module docs.
pub static SRC: Mutex<usize> = Mutex::new(0);
/// Capture the `InputSource` for a single diagnostic log. It is not used to send input.
pub static INPUT_SRC: AtomicUsize = AtomicUsize::new(0);

/// Hold [`SRC`].
pub fn src() -> MutexGuard<'static, usize> {
    SRC.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Per-type selection
// ---------------------------------------------------------------------------

/// Each sensor key selects its advertised and reported type. Value 1 enables both. zero
/// disables both. The stock response already advertises parking brake type7. its key
/// selects this module override. Other extra types are disabled by default.
pub const TYPE_KEYS: [(u8, &CStr, i32); 12] = [
    (2, c"sensor_send_compass", 0),
    (4, c"sensor_send_rpm", 0),
    (7, c"sensor_send_parking_brake", 1),
    (11, c"sensor_send_environment", 0),
    (12, c"sensor_send_hvac", 0),
    (14, c"sensor_send_dead_reckoning", 0),
    (15, c"sensor_send_passenger", 0),
    (16, c"sensor_send_door", 0),
    (17, c"sensor_send_light", 0),
    (18, c"sensor_send_tire", 0),
    (19, c"sensor_send_accel", 0),
    (20, c"sensor_send_gyro", 0),
];

/// True when sensor type `t` is selected (its key is not 0). A type with no
/// key is always selected.
#[must_use]
pub fn selected(t: u8) -> bool {
    TYPE_KEYS
        .iter()
        .find(|(k, _, _)| *k == t)
        .is_none_or(|(_, key, def)| config::get_int(key, *def) != 0)
}

/// The `SensorSource` fields `queueOutgoing` needs, read while [`SRC`] is
/// held (`sendSensorBatch`'s own offsets).
#[derive(Clone, Copy)]
pub struct Channel {
    pub started: u8,
    pub chan: u8,
    pub router: usize,
}

/// # Safety
/// `ss` is the live `SensorSource` (the caller holds [`SRC`]).
#[must_use]
pub unsafe fn channel_of(ss: usize) -> Channel {
    let p = ss as *const u8;
    Channel {
        started: *p.add(off::SENSOR_SOURCE_STARTED),
        chan: *p.add(off::SENSOR_SOURCE_CHANNEL),
        router: p
            .add(off::SENSOR_SOURCE_ROUTER)
            .cast::<usize>()
            .read_unaligned(),
    }
}

// ---------------------------------------------------------------------------
// The resolved calls (sensor_resolve: once each, a failure retried on the
// next session)
// ---------------------------------------------------------------------------

/// One resolved symbol (0: not resolved).
pub struct Resolved(AtomicUsize);

impl Resolved {
    const fn new() -> Self {
        Resolved(AtomicUsize::new(0))
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
    /// The function, typed as `s` types it.
    pub fn get<F: Copy>(&self, _s: &Symbol<F>) -> Option<F> {
        const { assert!(core::mem::size_of::<F>() == core::mem::size_of::<usize>()) };
        let p = self.0.load(Ordering::Acquire);
        // SAFETY: stored by `resolve` from a lookup of `s` (F is its type).
        (p != 0).then(|| unsafe { core::mem::transmute_copy::<usize, F>(&p) })
    }
    /// `DLSYM(sym, name)`: look it up unless already set. log the outcome.
    fn resolve<F: Copy>(&self, s: &Symbol<F>) {
        if self.is_set() {
            return;
        }
        let p = qnx_dl::sym_default(s.name());
        if p.is_null() {
            s_loge!(
                "[GalSensor] FAILED to resolve {}",
                CBytes(s.name().to_bytes())
            );
        } else {
            self.0.store(p as usize, Ordering::Release);
            s_log!("[GalSensor] resolved {}", CBytes(s.name().to_bytes()));
        }
    }
}

pub static FN_ENV: Resolved = Resolved::new();
pub static FN_RPM: Resolved = Resolved::new();
pub static FN_FUEL: Resolved = Resolved::new();
pub static FN_TYRE: Resolved = Resolved::new();
pub static FN_ACCEL: Resolved = Resolved::new();
pub static FN_DOOR: Resolved = Resolved::new();
pub static FN_LIGHT: Resolved = Resolved::new();
pub static FN_HVAC: Resolved = Resolved::new();
pub static FN_PARK: Resolved = Resolved::new();
pub static FN_COMPASS: Resolved = Resolved::new();
pub static FN_DR: Resolved = Resolved::new();
pub static FN_GYRO: Resolved = Resolved::new();
pub static FN_PASSENGER: Resolved = Resolved::new();
pub static FN_QUEUE: Resolved = Resolved::new();

/// `sensor_resolve()`: the EXLAP thread, after the first subscriptions.
pub fn resolve_all() {
    FN_ENV.resolve(&sym::SS_REPORT_ENVIRONMENT);
    FN_RPM.resolve(&sym::SS_REPORT_RPM);
    FN_FUEL.resolve(&sym::SS_REPORT_FUEL);
    FN_TYRE.resolve(&sym::SS_REPORT_TIRE_PRESSURE);
    FN_ACCEL.resolve(&sym::SS_REPORT_ACCELEROMETER);
    FN_DOOR.resolve(&sym::SS_REPORT_DOOR);
    FN_LIGHT.resolve(&sym::SS_REPORT_LIGHT);
    FN_HVAC.resolve(&sym::SS_REPORT_HVAC);
    FN_PARK.resolve(&sym::SS_REPORT_PARKING_BRAKE);
    FN_COMPASS.resolve(&sym::SS_REPORT_COMPASS);
    FN_DR.resolve(&sym::SS_REPORT_DEAD_RECKONING);
    FN_GYRO.resolve(&sym::SS_REPORT_GYROSCOPE);
    FN_PASSENGER.resolve(&sym::SS_REPORT_PASSENGER);
    FN_QUEUE.resolve(&sym::MR_QUEUE_OUTGOING);
}

// ---------------------------------------------------------------------------
// EXLAP state (written by the EXLAP thread, read by gal's threads)
// ---------------------------------------------------------------------------

macro_rules! cell {
    ($($name:ident = $v:expr),* $(,)?) => {
        $( pub static $name: AtomicI32 = AtomicI32::new($v); )*
    };
}

cell! {
    FUEL_PCT = -1,
    TEMP_E3 = 0, HAS_TEMP = 0,
    RPM_E3 = 0, HAS_RPM = 0,
    ACCEL_X = 0, ACCEL_Y = 0, HAS_ACCEL = 0,
    TYRE_0 = 0, TYRE_1 = 0, TYRE_2 = 0, TYRE_3 = 0, HAS_TYRE = 0,
    DOOR_HOOD = 0, DOOR_BOOT = 0, DOOR_ANY = 0, DOOR_DRIVER = 0, HAS_DOOR = 0,
    HEADLIGHT = 0, HAS_HEADLIGHT = 0, INDICATOR = 0, HAZARD = 0, HAS_LIGHT = 0,
    HVAC_TARGET_E3 = 0, HAS_HVAC = 0,
    HEADING = 0, HAS_HEADING = 0,
    STEER_E3 = 0, HAS_STEER = 0,
    WHL_0 = 0, WHL_1 = 0, WHL_2 = 0, WHL_3 = 0, HAS_WHL = 0,
    YAW_E3 = 0, HAS_YAW = 0,
    ACC_ACTIVE = 0, HAS_ACC = 0,
    PARK_BRAKE = 0, HAS_PARK = 0,
    PASSENGER = 0, HAS_PASSENGER = 0,
    RECUPERATION = 0, HAS_RECUP = 0,
    HEV_MODE = 0, HAS_HEV_MODE = 0,
}

pub fn get(c: &AtomicI32) -> i32 {
    c.load(Ordering::Relaxed)
}
pub fn set(c: &AtomicI32, v: i32) {
    c.store(v, Ordering::Relaxed);
}

/// C's `(int)x` for a double in range (truncation toward zero. Rust
/// saturates out of range where C is undefined).
#[allow(clippy::cast_possible_truncation)] // the wire value wraps at its declared width
pub fn to_int(x: f64) -> i32 {
    x as i32
}

// ---------------------------------------------------------------------------
// push_*: every report goes out under SRC and the send lock
// ---------------------------------------------------------------------------

/// Call the report function under SRC and the send lock. Do nothing when no
/// `SensorSource` exists or the type is disabled.
fn report(t: u8, call: impl FnOnce(gal_vendor_abi::Ptr)) {
    if !selected(t) {
        return;
    }
    let g = src();
    if *g == 0 {
        return;
    }
    send_lock::lock();
    call(*g as gal_vendor_abi::Ptr);
    send_lock::unlock();
    drop(g);
}

/// Return whether a `SensorSource` is captured.
fn have_src() -> bool {
    *src() != 0
}

pub fn push_environment() {
    let Some(f) = FN_ENV.get(&sym::SS_REPORT_ENVIRONMENT) else {
        return;
    };
    if !have_src() || get(&HAS_TEMP) == 0 {
        return;
    }
    let t = get(&TEMP_E3);
    // SAFETY: gal's reportEnvironmentData on the live SensorSource.
    report(11, |ss| unsafe { f(ss, 1, t, 0, 0, 0, 0) });
}

pub fn push_rpm() {
    let Some(f) = FN_RPM.get(&sym::SS_REPORT_RPM) else {
        return;
    };
    if !have_src() || get(&HAS_RPM) == 0 {
        return;
    }
    let v = get(&RPM_E3);
    // SAFETY: as push_environment.
    report(4, |ss| unsafe { f(ss, v) });
}

pub fn push_tyre() {
    let Some(f) = FN_TYRE.get(&sym::SS_REPORT_TIRE_PRESSURE) else {
        return;
    };
    if !have_src() || get(&HAS_TYRE) == 0 {
        return;
    }
    let arr = [get(&TYRE_0), get(&TYRE_1), get(&TYRE_2), get(&TYRE_3)];
    // SAFETY: use the environment-report signature with four integer arguments.
    report(18, |ss| unsafe { f(ss, arr.as_ptr(), 4) });
}

pub fn push_accel() {
    let Some(f) = FN_ACCEL.get(&sym::SS_REPORT_ACCELEROMETER) else {
        return;
    };
    if !have_src() || get(&HAS_ACCEL) == 0 {
        return;
    }
    let (x, y) = (get(&ACCEL_X), get(&ACCEL_Y));
    // SAFETY: as push_environment.
    report(19, |ss| unsafe { f(ss, 1, x, 1, y, 0, 0) });
}

pub fn push_door() {
    let Some(f) = FN_DOOR.get(&sym::SS_REPORT_DOOR) else {
        return;
    };
    if !have_src() || get(&HAS_DOOR) == 0 {
        return;
    }
    let (hood, boot, any, driver) = (
        get(&DOOR_HOOD),
        get(&DOOR_BOOT),
        get(&DOOR_ANY),
        get(&DOOR_DRIVER),
    );
    // Pass a zero integer address with element count zero.
    let dummy: i32 = 0;
    // SAFETY: as push_environment. a count of 0, the array is not read.
    report(16, |ss| unsafe {
        f(ss, hood, boot, any, driver, (&raw const dummy).cast(), 0);
    });
}

pub fn push_light() {
    let Some(f) = FN_LIGHT.get(&sym::SS_REPORT_LIGHT) else {
        return;
    };
    if !have_src() || get(&HAS_LIGHT) == 0 {
        return;
    }
    let (hh, h, i, z) = (
        get(&HAS_HEADLIGHT),
        get(&HEADLIGHT),
        get(&INDICATOR),
        get(&HAZARD),
    );
    // SAFETY: as push_environment.
    report(17, |ss| unsafe { f(ss, hh, h, 1, i, 1, z) });
}

/// No configured EXLAP credential supplies this HVAC set point. this sender is unused.
#[allow(dead_code)]
pub fn push_hvac() {
    let Some(f) = FN_HVAC.get(&sym::SS_REPORT_HVAC) else {
        return;
    };
    if !have_src() || get(&HAS_HVAC) == 0 {
        return;
    }
    let t = get(&HVAC_TARGET_E3);
    // SAFETY: as push_environment.
    report(12, |ss| unsafe { f(ss, 1, t, 0, 0) });
}

pub fn push_compass() {
    let Some(f) = FN_COMPASS.get(&sym::SS_REPORT_COMPASS) else {
        return;
    };
    if !have_src() || get(&HAS_HEADING) == 0 {
        return;
    }
    let h = get(&HEADING);
    // SAFETY: as push_environment.
    report(2, |ss| unsafe { f(ss, h) });
}

pub fn push_dead_reckoning() {
    let Some(f) = FN_DR.get(&sym::SS_REPORT_DEAD_RECKONING) else {
        return;
    };
    if !have_src() {
        return;
    }
    let (hs, hw) = (get(&HAS_STEER), get(&HAS_WHL));
    if hs == 0 && hw == 0 {
        return;
    }
    let whl = [get(&WHL_0), get(&WHL_1), get(&WHL_2), get(&WHL_3)];
    let steer = get(&STEER_E3);
    let (p, n) = if hw != 0 {
        (whl.as_ptr(), 4)
    } else {
        (core::ptr::null(), 0)
    };
    // SAFETY: as push_environment. `p` is NULL or four ints.
    report(14, |ss| unsafe { f(ss, hs, steer, p, n) });
}

pub fn push_gyro() {
    let Some(f) = FN_GYRO.get(&sym::SS_REPORT_GYROSCOPE) else {
        return;
    };
    if !have_src() || get(&HAS_YAW) == 0 {
        return;
    }
    let z = get(&YAW_E3);
    // SAFETY: as push_environment.
    report(20, |ss| unsafe { f(ss, 0, 0, 0, 0, 1, z) });
}

pub fn push_parking_brake() {
    let Some(f) = FN_PARK.get(&sym::SS_REPORT_PARKING_BRAKE) else {
        return;
    };
    if !have_src() || get(&HAS_PARK) == 0 {
        return;
    }
    let v = get(&PARK_BRAKE);
    // SAFETY: as push_environment.
    report(7, |ss| unsafe { f(ss, v) });
}

pub fn push_passenger() {
    let Some(f) = FN_PASSENGER.get(&sym::SS_REPORT_PASSENGER) else {
        return;
    };
    if !have_src() || get(&HAS_PASSENGER) == 0 {
        return;
    }
    let v = get(&PASSENGER);
    // SAFETY: as push_environment.
    report(15, |ss| unsafe { f(ss, v) });
}

// ---------------------------------------------------------------------------
// The <Dat> dispatch (exlap_dispatch)
// ---------------------------------------------------------------------------

/// `exlap_dispatch(url, dat_xml)`.
#[allow(clippy::too_many_lines)] // one arm per sensor URL
pub fn dispatch(url: &[u8], dat: &[u8]) {
    let field = |name: &[u8]| parse::obj_field(dat, name);
    let scalar = |tag: &[u8]| parse::scalar(dat, tag);
    match url {
        b"outsideTemperature" => {
            if let Some(v) = field(b"temperature") {
                set(&TEMP_E3, to_int(v * 1000.0));
                set(&HAS_TEMP, 1);
                push_environment();
                crate::ev::push_ev_model();
            }
        }
        b"engineSpeed" => {
            if let Some(s) = scalar(b"Abs") {
                set(&RPM_E3, to_int(s.num * 1000.0));
                set(&HAS_RPM, 1);
                push_rpm();
            }
        }
        b"tankLevelPrimary" => {
            if let Some(v) = field(b"level") {
                let pct = to_int(v * 100.0 + 0.5).clamp(0, 100);
                let changed = pct != get(&FUEL_PCT);
                set(&FUEL_PCT, pct);
                if changed {
                    crate::ev::push_ev_model();
                }
            }
        }
        b"lateralAcceleration" => {
            if let Some(s) = scalar(b"Abs") {
                set(&ACCEL_Y, to_int(s.num * 1000.0));
                set(&HAS_ACCEL, 1);
                push_accel();
            }
        }
        b"longitudinalAcceleration" => {
            if let Some(s) = scalar(b"Abs") {
                set(&ACCEL_X, to_int(s.num * 1000.0));
                set(&HAS_ACCEL, 1);
                push_accel();
            }
        }
        b"doorState" => {
            let mut got = false;
            let mut get_f = |name: &[u8]| match field(name) {
                Some(v) => {
                    got = true;
                    v
                }
                None => 0.0,
            };
            let hood = get_f(b"hoodState");
            let boot = get_f(b"trunkState");
            let drv = get_f(b"driverDoorState");
            let codrv = get_f(b"coDriverDoorState");
            let fl = get_f(b"fondDoorLeftState");
            let fr = get_f(b"fondDoorRightState");
            if got {
                set(&DOOR_HOOD, i32::from(hood != 0.0));
                set(&DOOR_BOOT, i32::from(boot != 0.0));
                set(&DOOR_DRIVER, i32::from(drv != 0.0));
                set(
                    &DOOR_ANY,
                    i32::from(drv != 0.0 || codrv != 0.0 || fl != 0.0 || fr != 0.0),
                );
                set(&HAS_DOOR, 1);
                push_door();
            }
        }
        b"lightState_front" => {
            if let Some(s) = scalar(b"Enm").or_else(|| scalar(b"Abs")) {
                set(&HEADLIGHT, to_int(s.num));
                set(&HAS_HEADLIGHT, 1);
                set(&HAS_LIGHT, 1);
                push_light();
            }
        }
        b"blinkingState" => {
            if let Some(s) = scalar(b"Enm").or_else(|| scalar(b"Abs")) {
                let v = to_int(s.num);
                set(&INDICATOR, v);
                set(&HAZARD, i32::from(v == 3));
                set(&HAS_LIGHT, 1);
                push_light();
            }
        }
        b"tyrePressures" => {
            if let (Some(fl), Some(fr), Some(rl), Some(rr)) = (
                field(b"pressureFrontLeft"),
                field(b"pressureFrontRight"),
                field(b"pressureRearLeft"),
                field(b"pressureRearRight"),
            ) {
                set(&TYRE_0, to_int(fl * 100.0 + 0.5));
                set(&TYRE_1, to_int(fr * 100.0 + 0.5));
                set(&TYRE_2, to_int(rl * 100.0 + 0.5));
                set(&TYRE_3, to_int(rr * 100.0 + 0.5));
                set(&HAS_TYRE, 1);
                push_tyre();
            }
        }
        b"Nav_Heading" => {
            if let Some(s) = scalar(b"Abs").or_else(|| scalar(b"Rel")) {
                // C: (int)(val + 0.5) % 360, the remainder keeps the sign.
                set(&HEADING, to_int(s.num + 0.5) % 360);
                set(&HAS_HEADING, 1);
                push_compass();
            }
        }
        b"wheelAngle" => {
            if let Some(s) = scalar(b"Abs") {
                set(&STEER_E3, to_int(s.num * 1000.0));
                set(&HAS_STEER, 1);
                push_dead_reckoning();
            }
        }
        b"espTyreVelocities" => {
            let four = |a: &[u8], b: &[u8], c: &[u8], d: &[u8]| {
                Some((field(a)?, field(b)?, field(c)?, field(d)?))
            };
            let got = four(b"VL", b"VR", b"HL", b"HR")
                .or_else(|| four(b"frontLeft", b"frontRight", b"rearLeft", b"rearRight"));
            if let Some((fl, fr, rl, rr)) = got {
                set(&WHL_0, to_int(fl * 277.78 + 0.5));
                set(&WHL_1, to_int(fr * 277.78 + 0.5));
                set(&WHL_2, to_int(rl * 277.78 + 0.5));
                set(&WHL_3, to_int(rr * 277.78 + 0.5));
                set(&HAS_WHL, 1);
                push_dead_reckoning();
            } else {
                s_loge!(
                    "[GalSensor] espTyreVelocities: field names not found \u{2014} \
                     unrecognized sensor fields in <Dat>"
                );
            }
        }
        b"yawRate" => {
            if let Some(s) = scalar(b"Abs") {
                set(&YAW_E3, to_int(s.num * 1000.0));
                set(&HAS_YAW, 1);
                push_gyro();
            }
        }
        b"accIsActive" => {
            if let Some(s) = scalar(b"Act").or_else(|| scalar(b"Abs")) {
                set(&ACC_ACTIVE, i32::from(s.flag));
                set(&HAS_ACC, 1);
            }
        }
        b"parkingBrake" => {
            if let Some(s) = scalar(b"Act").or_else(|| scalar(b"Abs")) {
                set(&PARK_BRAKE, i32::from(s.flag));
                set(&HAS_PARK, 1);
                push_parking_brake();
            }
        }
        b"Comfort_CarSeatsStatus" => {
            let occ = field(b"frontPassenger")
                .or_else(|| field(b"passenger"))
                .or_else(|| field(b"coDriver"))
                .or_else(|| field(b"roleOccupant"));
            if let Some(o) = occ {
                set(&PASSENGER, i32::from(o != 0.0));
                set(&HAS_PASSENGER, 1);
                push_passenger();
                s_log!(
                    "[GalSensor] Comfort_CarSeatsStatus: passenger={}",
                    get(&PASSENGER)
                );
            } else {
                s_loge!(
                    "[GalSensor] Comfort_CarSeatsStatus: no occupancy field found \u{2014} \
                     unrecognized sensor fields in <Dat>"
                );
            }
        }
        b"recuperationLevel" => {
            if let Some(s) = scalar(b"Abs").or_else(|| scalar(b"Enm")) {
                set(&RECUPERATION, to_int(s.num));
                set(&HAS_RECUP, 1);
            }
        }
        b"hevOperationMode" => {
            if let Some(s) = scalar(b"Enm").or_else(|| scalar(b"Abs")) {
                set(&HEV_MODE, to_int(s.num));
                set(&HAS_HEV_MODE, 1);
            }
        }
        // No Android Auto sensor is mapped for these two fields.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no conf (every key at its default), only the stock parking brake
    /// override is on. every type the stock list lacks is off.
    #[test]
    fn per_type_defaults() {
        assert!(selected(7), "parking brake override defaults on");
        for t in [2u8, 4, 11, 12, 14, 15, 16, 17, 18, 19, 20] {
            assert!(!selected(t), "type {t} defaults off");
        }
        assert!(selected(23), "a type without a key is always selected");
        assert_eq!(TYPE_KEYS.iter().filter(|(_, _, d)| *d != 0).count(), 1);
    }
}
