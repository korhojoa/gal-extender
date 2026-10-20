// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! EV energy model and raw EV trip messages.
//!
//! Sensor type 23 carries the energy model. type 26 carries raw trip values.
//! The `aa-wire-patch` crate updates the model template. Values too wide for
//! their encoded field remain unchanged instead of being truncated. The
//! four-byte capacity field cannot encode a capacity of 2^28 Wh or more.
//!
//! The display-state publisher writes `/ramdisk/pps/gal/ev` once per update. It removes
//! a key when the source becomes invalid. Readers must not keep a stale value
//! when its source key is absent.

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::io::Read as _;
use std::sync::{Mutex, MutexGuard, PoisonError};

use aa_wire_patch::ev_model;
use gal_feature_api::location::LoaderFile;
use gal_feature_api::{config, pps, send_lock};
use gal_vendor_abi::symbols as sym;
use protobuf_wire::wire;

use crate::log::{s_log, s_loge, CPtr, StrError};
use crate::sensors::{self as st, channel_of, get};

/// `ev_model.bin` beside the loaded library.
pub static EV_MODEL_FILE: LoaderFile = LoaderFile::new("ev_model.bin");
const EV_MODEL_MAX: usize = 16384;
/// `raw_ev_trip.bin` in the loader directory.
pub static RAW_EV_TRIP_FILE: LoaderFile = LoaderFile::new("raw_ev_trip.bin");
/// The hand-off object, `/ramdisk/pps/gal/ev`.
pub static GAL_EV: pps::Object = pps::Object::new("ev");
const GAL_EV_STALE_MS: u64 = 30_000;

/// `s_ev_pending`: a send that wanted to happen while the sensor channel
/// was not ready.
pub static PENDING: AtomicBool = AtomicBool::new(false);
/// `sensor_ev_capacity_wh` as last read.
pub static CAPACITY_WH: AtomicI32 = AtomicI32::new(30_000);
// ---------------------------------------------------------------------------
// /ramdisk/pps/gal/ev
// ---------------------------------------------------------------------------

/// The `g_ev_*` values.
#[derive(Clone, Copy)]
pub struct GalEv {
    pub have: bool,
    pub seq: i32,
    pub updated_ms: u64,
    pub soc_pct: i32,
    pub range_m: i32,
    pub residual_wh: i32,
    poll_ms: u64,
}

static GALEV: Mutex<GalEv> = Mutex::new(GalEv {
    have: false,
    seq: -1,
    updated_ms: 0,
    soc_pct: -1,
    range_m: -1,
    residual_wh: -1,
    poll_ms: 0,
});

fn galev() -> MutexGuard<'static, GalEv> {
    GALEV.lock().unwrap_or_else(PoisonError::into_inner)
}

/// C `strtol` as `atoi` / `atol` use it (32-bit `long`): white space, a
/// sign, digits. clamped to the `i32` range. 0 when there are no digits.
#[must_use]
pub fn c_atol(s: &[u8]) -> i32 {
    let (neg, digits) = sign_digits(s);
    let mut v: i64 = 0;
    for &d in digits {
        v = (v * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX) + 1);
    }
    let v = if neg { -v } else { v };
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

/// C `strtoull(s, NULL, 10)`: white space, a sign, digits. `u64::MAX` on
/// overflow. a minus negates in unsigned arithmetic.
#[must_use]
pub fn c_strtoull(s: &[u8]) -> u64 {
    let (neg, digits) = sign_digits(s);
    let mut v: u64 = 0;
    for &d in digits {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(d - b'0')))
        {
            Some(x) => v = x,
            None => return u64::MAX,
        }
    }
    if neg {
        v.wrapping_neg()
    } else {
        v
    }
}

fn sign_digits(s: &[u8]) -> (bool, &[u8]) {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let n = s[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    (neg, &s[i..i + n])
}

/// One attribute of the hand-off object (the value parsed as `atoi` /
/// `strtoull` did the `key=value` file's).
fn gal_ev_attr(
    k: &[u8],
    v: &[u8],
    seq: &mut i32,
    upd: &mut u64,
    soc: &mut i32,
    range: &mut i32,
    resid: &mut i32,
) {
    match k {
        b"seq" => *seq = c_atol(v),
        b"updated_ms" => *upd = c_strtoull(v),
        b"soc_pct" => *soc = c_atol(v),
        b"range_m" => *range = c_atol(v),
        b"residual_energy_wh" => *resid = c_atol(v),
        _ => {}
    }
}

/// The hand-off values in one object text: `(seq, updated_ms, soc_pct,
/// range_m, residual_energy_wh)`, -1 / 0 when absent.
fn gal_ev_parse(text: &[u8]) -> (i32, u64, i32, i32, i32) {
    let (mut seq, mut soc, mut range, mut resid) = (-1i32, -1i32, -1i32, -1i32);
    let mut upd: u64 = 0;
    for (k, v) in pps::attrs(text) {
        gal_ev_attr(k, v, &mut seq, &mut upd, &mut soc, &mut range, &mut resid);
    }
    (seq, upd, soc, range, resid)
}

/// `gal_ev_poll()`: refresh the hand-off values, at most every 500 ms. One
/// read of the PPS object into a stack buffer, no allocation: gal's thread
/// calls this from the fuel hook.
pub fn gal_ev_poll() -> GalEv {
    let mut g = galev();
    let now = qnx_slog::sys::wall_ms();
    if g.poll_ms != 0 && now.wrapping_sub(g.poll_ms) < 500 {
        return *g;
    }
    g.poll_ms = now;
    let mut buf = [0u8; 1024];
    let Some(text) = GAL_EV.read(&mut buf) else {
        g.have = false;
        return *g;
    };
    let (seq, upd, mut soc, mut range, mut resid) = gal_ev_parse(text);

    if upd != 0 && now >= upd && (now - upd) > GAL_EV_STALE_MS {
        if g.have {
            #[allow(clippy::cast_possible_truncation)] // the timer value wraps at 32 bits
            let age = (now - upd) as u32;
            s_log!(
                "[GalSensor] gal_ev stale ({}ms old) \u{2014} dropping EV hand-off",
                age
            );
        }
        g.have = false;
        return *g;
    }
    if !(0..=100).contains(&soc) {
        soc = -1;
    }
    if !(0..=200_000).contains(&resid) {
        resid = -1;
    }
    if !(0..=2_000_000).contains(&range) {
        range = -1;
    }
    let changed = seq != g.seq;
    g.seq = seq;
    g.updated_ms = upd;
    g.soc_pct = soc;
    g.range_m = range;
    g.residual_wh = resid;
    g.have = true;
    if changed {
        s_log!(
            "[GalSensor] gal_ev seq={} soc={}% range_m={} residual_wh={} (lsd)",
            seq,
            soc,
            range,
            resid
        );
    }
    *g
}

// ---------------------------------------------------------------------------
// The EV model
// ---------------------------------------------------------------------------

/// Serialize the complete staged energy-model push with one lock shared by receiver and
/// EXLAP threads.
struct EvTx {
    payload: [u8; EV_MODEL_MAX + 2],
    len: usize,
    buf: [u8; EV_MODEL_MAX],
}

static EVTX: Mutex<EvTx> = Mutex::new(EvTx {
    payload: [0; EV_MODEL_MAX + 2],
    len: 0,
    buf: [0; EV_MODEL_MAX],
});

/// Take every lock this module uses once, on the init thread (their first
/// use allocates on QNX).
pub fn warm_up() {
    drop(galev());
    drop(EVTX.lock().unwrap_or_else(PoisonError::into_inner));
}

/// `open` + one `read` of up to `buf.len()` bytes: the count, or the errno
/// of a failed open (`Err`). A failed read gives `Ok(-1)`.
fn read_once(path: &str, buf: &mut [u8]) -> Result<i32, i32> {
    let mut f = std::fs::File::open(path).map_err(|e| e.raw_os_error().unwrap_or(0))?;
    Ok(match f.read(buf) {
        Ok(n) => i32::try_from(n).unwrap_or(i32::MAX),
        Err(_) => -1,
    })
}

/// The soc the EV lines use: lsd's HMI value over EXLAP's tank level.
fn soc_of(e: &GalEv) -> i32 {
    if e.have && e.soc_pct >= 0 {
        e.soc_pct
    } else {
        get(&st::FUEL_PCT)
    }
}

/// `ev_build_payload()`: the framed length (0: no template).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // C casts
fn build(tx: &mut EvTx, e: &GalEv) -> usize {
    let soc = soc_of(e);
    let cap = CAPACITY_WH.load(Ordering::Relaxed);
    let level: i64 = if e.have && e.residual_wh >= 0 {
        i64::from(e.residual_wh)
    } else if soc >= 0 {
        i64::from(soc) * i64::from(cap) / 100
    } else {
        -1
    };

    let n = match read_once(EV_MODEL_FILE.path(), &mut tx.buf) {
        Err(e) => {
            s_loge!(
                "[GalSensor] EV model {} absent ({}) \u{2014} EV data disabled",
                EV_MODEL_FILE.path(),
                StrError(e)
            );
            return 0;
        }
        Ok(n) if n <= 0 => {
            s_loge!("[GalSensor] EV model read failed ({})", n);
            return 0;
        }
        Ok(n) => n as usize,
    };
    let tmpl = &tx.buf[..n];
    let temp = (get(&st::HAS_TEMP) != 0).then(|| (f64::from(get(&st::TEMP_E3)) / 1000.0) as f32);
    let lvl = (level >= 0).then_some(level as u64);
    let Some(r) = ev_model::patch_energy_model(tmpl, &mut tx.payload, cap as u64, lvl, temp) else {
        return 0;
    };
    let width = |path: &[u32]| wire::locate(tmpl, path, wire::VARINT).map_or(0, |(_, l)| l);
    if r.capacity_overflow {
        s_loge!(
            "[GalSensor] EV capacity {} exceeds {}-byte field",
            cap,
            width(&ev_model::PATH_CAPACITY)
        );
    } else if !r.capacity_patched {
        s_loge!("[GalSensor] EV template: capacity field [23,1,4,1] not found");
    }
    if level >= 0 {
        if r.level_overflow {
            s_loge!(
                "[GalSensor] EV level {} exceeds {}-byte field",
                level as i32,
                width(&ev_model::PATH_LEVEL)
            );
        } else if !r.level_patched {
            s_loge!("[GalSensor] EV template: level field [23,1,3,1] not found");
        }
    }
    tx.len = r.out_len;

    tx.len
}

/// `push_ev_model()`.
pub fn push_ev_model() {
    let mut tx = EVTX.lock().unwrap_or_else(PoisonError::into_inner);
    let mut e = gal_ev_poll();
    CAPACITY_WH.store(
        config::get_int(c"sensor_ev_capacity_wh", 30_000),
        Ordering::Relaxed,
    );
    let soc = soc_of(&e);
    if soc < 0 && !(e.have && e.residual_wh >= 0) {
        return;
    }
    // ev_build_payload reads the hand-off values the poll above left.
    e = *galev();
    let n = build(&mut tx, &e);
    if n == 0 {
        return;
    }
    if config::get_int(c"sensor_ev_send", 0) == 0 {
        return;
    }
    let g = st::src();
    if *g == 0 {
        drop(g);
        PENDING.store(true, Ordering::Relaxed);
        return;
    }
    let Some(queue) = st::FN_QUEUE.get(&sym::MR_QUEUE_OUTGOING) else {
        drop(g);
        s_loge!(
            "[GalSensor] push_ev_model: queueOutgoing unresolved \u{2014} staged {}B but NOT sent",
            n
        );
        return;
    };
    // SAFETY: the live SensorSource (SRC held).
    let c = unsafe { channel_of(*g) };
    if c.started == 0 || c.router == 0 {
        drop(g);
        PENDING.store(true, Ordering::Relaxed);
        s_loge!(
            "[GalSensor] push_ev_model: sensor channel not started \
             (started={} router={}) \u{2014} staged {}B, deferred",
            c.started,
            CPtr(c.router),
            n
        );
        return;
    }
    send_lock::lock();
    #[allow(clippy::cast_possible_truncation)] // <= 16386
    // SAFETY: gal's queueOutgoing on the live session's router. the payload
    // stays valid for the call.
    unsafe {
        queue(
            c.router as gal_vendor_abi::Ptr,
            c.chan,
            tx.payload.as_mut_ptr().cast(),
            n as u32,
        );
    }
    send_lock::unlock();
    drop(g);
    PENDING.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The batch senders
// ---------------------------------------------------------------------------

/// `queue_sensor_payload(payload, len, what)`: true when sent.
pub fn queue_sensor_payload(payload: &mut [u8], what: &str) -> bool {
    let len = payload.len();
    let g = st::src();
    if *g == 0 {
        return false;
    }
    let Some(queue) = st::FN_QUEUE.get(&sym::MR_QUEUE_OUTGOING) else {
        drop(g);
        s_loge!(
            "[GalSensor] {}: queueOutgoing unresolved \u{2014} {}B NOT sent",
            what,
            len
        );
        return false;
    };
    // SAFETY: the live SensorSource (SRC held).
    let c = unsafe { channel_of(*g) };
    if c.started == 0 || c.router == 0 {
        drop(g);
        return false;
    }
    send_lock::lock();
    #[allow(clippy::cast_possible_truncation)] // <= 4112
    // SAFETY: as push_ev_model.
    unsafe {
        queue(
            c.router as gal_vendor_abi::Ptr,
            c.chan,
            payload.as_mut_ptr().cast(),
            len as u32,
        );
    }
    send_lock::unlock();
    drop(g);
    true
}

/// The raw-ev-trip buffers (4 KB + 4 KB + 16, too big for gal's stacks).
struct RawTrip {
    raw: [u8; 4096],
    out: [u8; 4096 + 16],
}
static RAWTRIP: Mutex<RawTrip> = Mutex::new(RawTrip {
    raw: [0; 4096],
    out: [0; 4096 + 16],
});

/// `push_raw_ev_trip()`.
pub fn push_raw_ev_trip() {
    let mut b = RAWTRIP.lock().unwrap_or_else(PoisonError::into_inner);
    let b = &mut *b;
    let rn = match read_once(RAW_EV_TRIP_FILE.path(), &mut b.raw) {
        Err(e) => {
            s_loge!(
                "[GalSensor] raw_ev_trip: {} absent ({})",
                RAW_EV_TRIP_FILE.path(),
                StrError(e)
            );
            return;
        }
        Ok(n) if n <= 0 => {
            s_loge!("[GalSensor] raw_ev_trip: read failed ({})", n);
            return;
        }
        Ok(n) => usize::try_from(n).unwrap_or(0),
    };
    match aa_wire_patch::batch::raw_ev_trip(&mut b.out, &b.raw[..rn]) {
        Some(n) if n > 0 => {
            queue_sensor_payload(&mut b.out[..n], "raw_ev_trip");
        }
        _ => s_loge!("[GalSensor] raw_ev_trip: encoder returned {}", -1),
    }
}

/// Warm the raw-trip lock too (see [`warm_up`]).
pub fn warm_up_batch() {
    drop(RAWTRIP.lock().unwrap_or_else(PoisonError::into_inner));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_number_parsing() {
        assert_eq!(c_atol(b"73\n"), 73);
        assert_eq!(c_atol(b"  -5x"), -5);
        assert_eq!(c_atol(b"x"), 0);
        assert_eq!(c_atol(b"99999999999"), i32::MAX);
        assert_eq!(c_strtoull(b"1759840000000\n"), 1_759_840_000_000);
        assert_eq!(c_strtoull(b"-1"), u64::MAX);
        assert_eq!(c_strtoull(b"99999999999999999999999"), u64::MAX);
    }

    #[test]
    fn hand_off_attributes_parse_like_atoi() {
        let text = b"@ev\nseq::5\nupdated_ms::42\nsoc_pct::98\nrange_m::188000\n\
                     residual_energy_wh::27800\nplug_state::1\njunk\n";
        assert_eq!(gal_ev_parse(text), (5, 42, 98, 188_000, 27_800));
        // a key deleted by the writer (invalid source) is absent: -1
        assert_eq!(
            gal_ev_parse(b"@ev\nseq::6\nupdated_ms::43\n"),
            (6, 43, -1, -1, -1)
        );
        // the old key=value lines are not attributes
        assert_eq!(gal_ev_parse(b"seq=5\n"), (-1, 0, -1, -1, -1));
    }
}
