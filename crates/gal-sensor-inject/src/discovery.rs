// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Receiver-thread sensor hooks. Discovery uses two 2048-byte stack buffers. Fixed log
//! buffers bound formatting work. Session-end callbacks retire captured receiver
//! objects before teardown.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use aa_wire_patch::sdr_splice;
use gal_feature_api::config;
use gal_feature_api::hooks::{FuelData, SpeedData};
use gal_feature_api::session::End;
use gal_vendor_abi::offsets as off;
use gal_vendor_abi::symbols as sym;
use gal_vendor_abi::{ConstPtr, Ptr};
use qnx_slog::log::Line;

use crate::ev;
use crate::log::{s_log, s_loge, CPtr};
use crate::sensors::{self as st, get};

/// The EXLAP-fed sensor types the splice can add, each when its
/// `sensor_send_*` key ([`st::TYPE_KEYS`]) is on.
const EXTRA_SENSOR_TYPES: [u8; 11] = [2, 4, 11, 12, 14, 15, 16, 17, 18, 19, 20];

/// The DHU identity fields appended with `identify_as_dhu=1` (make, model,
/// `head_unit_make`, `head_unit_model` = Google / Desktop Head Unit).
const DHU_FIELDS: &[u8] = b"\x12\x06Google\
\x1a\x11Desktop Head Unit\
\x3a\x06Google\
\x42\x11Desktop Head Unit";

/// The sensor types spliced into this session's SDR (1 << type). the
/// `SensorRequest` hook acks exactly these.
static SPLICED: AtomicU32 = AtomicU32::new(0);

// --- the SDR / protobuf calls, resolved once ------------------------------

static SDR_DONE: AtomicBool = AtomicBool::new(false);
static SDR_READY: AtomicBool = AtomicBool::new(false);
static SDR_FNS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];

type DtorFn = unsafe extern "C" fn(Ptr);
type ParseFn = unsafe extern "C" fn(Ptr, ConstPtr, i32) -> bool;
type NewFn = unsafe extern "C" fn(ConstPtr) -> Ptr;
type DefaultFn = unsafe extern "C" fn() -> Ptr;
type ByteSizeFn = unsafe extern "C" fn(ConstPtr) -> i32;
type SerializeFn = unsafe extern "C" fn(ConstPtr, Ptr, i32) -> bool;

/// `sdr_resolve()`: once per process. logs when any symbol is missing.
fn sdr_resolve() {
    if SDR_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let names = [
        sym::SDR_DELETING_DTOR.name(),
        sym::MSG_PARSE_PARTIAL_FROM_ARRAY.name(),
        sym::SDR_NEW.name(),
        sym::SDR_DEFAULT_INSTANCE.name(),
        sym::SDR_BYTE_SIZE.name(),
        sym::MSG_SERIALIZE_TO_ARRAY.name(),
    ];
    let mut all = true;
    for (slot, n) in SDR_FNS.iter().zip(names) {
        let p = qnx_dl::sym_default(n);
        slot.store(p as usize, Ordering::Release);
        all &= !p.is_null();
    }
    SDR_READY.store(all, Ordering::Release);
    if !all {
        s_loge!("[GalSensor] discovery: failed to resolve SDR proto symbols");
    }
}

/// The resolved function `i` of [`SDR_FNS`], typed `F`.
///
/// # Safety
/// `F` is the type of the symbol at index `i` and [`SDR_READY`] is set.
unsafe fn sdr_fn<F: Copy>(i: usize) -> F {
    let p = SDR_FNS[i].load(Ordering::Acquire);
    core::mem::transmute_copy::<usize, F>(&p)
}

/// `sensor_mask_to_list`: "2,4,11" for a bitmask.
fn mask_list(mask: u32, out: &mut Line<128>) {
    let mut first = true;
    for t in 0..32 {
        if mask & (1u32 << t) != 0 {
            let _ = if first {
                write!(out, "{t}")
            } else {
                write!(out, ",{t}")
            };
            first = false;
        }
    }
}

/// `modify_discovery(sdr)`.
#[allow(clippy::too_many_lines)] // one complete discovery transaction
fn modify_discovery(sdr: Ptr) {
    let do_ev = config::get_int(c"sensor_ev_advertise", 0);
    let do_dhu = config::get_int(c"identify_as_dhu", 0);
    let do_adv = EXTRA_SENSOR_TYPES.iter().any(|&t| st::selected(t));
    let do_rawev = config::get_int(c"sensor_raw_ev_trip", 0);
    if sdr.is_null() || (do_ev == 0 && do_dhu == 0 && !do_adv && do_rawev == 0) {
        return;
    }
    sdr_resolve();
    if !SDR_READY.load(Ordering::Acquire) {
        return;
    }
    // SAFETY (all six): SDR_READY, types as gal-vendor-abi's table.
    let (dtor, parse, new, default, byte_size, serialize) = unsafe {
        (
            sdr_fn::<DtorFn>(0),
            sdr_fn::<ParseFn>(1),
            sdr_fn::<NewFn>(2),
            sdr_fn::<DefaultFn>(3),
            sdr_fn::<ByteSizeFn>(4),
            sdr_fn::<SerializeFn>(5),
        )
    };

    let mut extra = [0u8; 40];
    let mut nextra = 0usize;
    for t in EXTRA_SENSOR_TYPES {
        if st::selected(t) {
            extra[nextra] = t;
            nextra += 1;
        }
    }
    if do_rawev != 0 {
        extra[nextra] = 26;
        nextra += 1;
    }

    let mut inbuf = [0u8; 2048];
    let mut outbuf = [0u8; 2048];
    // SAFETY: gal's ServiceDiscoveryResponse.
    let inlen = unsafe { byte_size(sdr) };
    let Some(inl) = usize::try_from(inlen)
        .ok()
        .filter(|&n| n > 0 && n <= inbuf.len())
    else {
        s_loge!("[GalSensor] discovery: SDR too large ({})", inlen);
        return;
    };
    // SAFETY: inbuf holds inlen bytes.
    if !unsafe { serialize(sdr, inbuf.as_mut_ptr().cast(), inlen) } {
        s_loge!("[GalSensor] discovery: SerializeToArray failed");
        return;
    }

    let mut spliced = 0u32;
    let mut outlen;
    if do_ev != 0 || nextra > 0 {
        if let Some(r) =
            sdr_splice::splice_sensors(&inbuf[..inl], &mut outbuf, do_ev != 0, &extra[..nextra])
        {
            s_log!("[GalSensor] splice: RUST ({}B)", r.out_len);
            outlen = r.out_len;
            spliced = r.spliced;
            let mut stock: Line<128> = Line::new();
            let mut added: Line<128> = Line::new();
            mask_list(r.present, &mut stock);
            mask_list(r.spliced, &mut added);
            s_log!(
                "[GalSensor] discovery: stock sensor types [{}], splicing [{}]",
                stock.as_str(),
                added.as_str()
            );
        } else {
            s_loge!("[GalSensor] splice: rust returned {}", -1);
            s_loge!("[GalSensor] discovery: no sensor service in SDR \u{2014} sensors not spliced");
            outbuf[..inl].copy_from_slice(&inbuf[..inl]);
            outlen = inl;
        }
    } else {
        outbuf[..inl].copy_from_slice(&inbuf[..inl]);
        outlen = inl;
    }

    if do_dhu != 0 && outlen + DHU_FIELDS.len() <= outbuf.len() {
        outbuf[outlen..outlen + DHU_FIELDS.len()].copy_from_slice(DHU_FIELDS);
        outlen += DHU_FIELDS.len();
    }
    if outlen == inl && do_dhu == 0 {
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // <= 2048
    let olen = outlen as i32;

    // SAFETY: gal's protobuf calls on a fresh message and on gal's SDR.
    unsafe {
        let tmp = new(default());
        if tmp.is_null() {
            return;
        }
        let ob: ConstPtr = outbuf.as_ptr().cast();
        if parse(tmp, ob, olen) && parse(sdr, ob, olen) {
            SPLICED.store(spliced, Ordering::Relaxed);
            let mut added: Line<128> = Line::new();
            mask_list(
                extra[..nextra].iter().fold(0u32, |m, &t| m | (1u32 << t)),
                &mut added,
            );
            s_log!(
                "[GalSensor] discovery modified: ev={} dhu={} extra=[{}] (SDR {}->{} bytes)",
                do_ev,
                do_dhu,
                added.as_str(),
                inlen,
                olen
            );
        } else {
            s_loge!("[GalSensor] discovery: modified SDR failed to parse \u{2014} left unchanged");
        }
        dtor(tmp);
    }
}

/// `sensor_on_sensor_discovery(self, sdr)`.
pub fn on_sensor_discovery(this: Ptr, sdr: Ptr) {
    let flush = {
        let mut g = st::src();
        if *g == 0 {
            *g = this as usize;
            drop(g);
            s_log!("[GalSensor] SensorSource captured {}", CPtr(this as usize));
            ev::PENDING.load(Ordering::Relaxed)
        } else {
            false
        }
    };
    if flush {
        ev::push_ev_model();
    }
    modify_discovery(sdr);
}

/// `sensor_on_sensor_request(self, request)`: true = handled (gal's NACK is
/// suppressed).
pub fn on_sensor_request(this: Ptr, request: ConstPtr) -> bool {
    let spliced = SPLICED.load(Ordering::Relaxed);
    if request.is_null() || spliced == 0 {
        return false;
    }
    // SAFETY: gal's SensorRequest. the type is an int at +0x30.
    let t = unsafe {
        request
            .cast::<u8>()
            .add(off::SENSOR_REQUEST_TYPE)
            .cast::<i32>()
            .read_unaligned()
    };
    if !(0..=31).contains(&t) || spliced & (1u32 << t) == 0 {
        return false;
    }
    let Some(send) = sym::SS_SEND_SENSOR_RESPONSE.resolve() else {
        s_loge!(
            "[GalSensor] sendSensorResponse unresolved \u{2014} cannot ack type {}",
            t
        );
        return false;
    };
    // SAFETY: gal's own SensorSource (`this`), on gal's thread, status 0.
    unsafe { send(this, 0) };
    s_log!(
        "[GalSensor] acked SensorRequest type={} (spliced sensor)",
        t
    );
    match t {
        23 => ev::push_ev_model(),
        26 => ev::push_raw_ev_trip(),
        _ => {}
    }
    true
}

/// `sensor_on_input_discovery(self, sdr)`.
pub fn on_input_discovery(this: Ptr, _sdr: Ptr) {
    if st::INPUT_SRC
        .compare_exchange(0, this as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        s_log!("[GalSensor] InputSource captured {}", CPtr(this as usize));
    }
}

/// `sensor_on_fuel_data`: the tank level and lsd's range.
pub fn on_fuel(_this: Ptr, d: &mut FuelData) {
    let e = ev::gal_ev_poll();
    let pct = if e.have && e.soc_pct >= 0 {
        e.soc_pct
    } else {
        get(&st::FUEL_PCT)
    };
    if pct >= 0 {
        d.has_level = 1;
        d.level = pct;
    }
    if e.have && e.range_m >= 0 {
        d.has_range = 1;
        d.range_m = e.range_m;
    }
}

/// `sensor_on_speed_data`: ACC state.
pub fn on_speed(_this: Ptr, d: &mut SpeedData) {
    if get(&st::HAS_ACC) != 0 {
        d.has_cruise = 1;
        d.cruise_engaged = get(&st::ACC_ACTIVE);
    }
}

/// `sensor_on_transport_read`: a failed real read is the phone gone.
pub fn on_transport_read(_this: Ptr, _buf: ConstPtr, len: u32, ret: i32) {
    if ret <= 0 && len > 0 {
        let mut g = st::src();
        if *g != 0 {
            *g = 0;
            drop(g);
            s_loge!("[GalSensor] AA transport closed \u{2014} stopping sensor injection");
        }
    }
}

/// Retire the captured session and wait for an in-flight sensor report to finish.
pub fn on_session_end(why: End) {
    let mut g = st::src();
    if *g != 0 {
        *g = 0;
        drop(g);
        s_loge!(
            "[GalSensor] AA session teardown ({}) \u{2014} stopping sensor injection",
            why.name()
        );
    }
}
