// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Interpose receiver methods by their mangled names. Owned work runs inside guard;
//! forwarding uses the original symbol from `RTLD_NEXT`. Native references are pointers.
//! Boolean and byte parameters use their declared widths. A 64-bit timestamp uses an
//! aligned register pair. Preserve the declared integer words and native return
//! registers across forwarding.

use core::ffi::{c_int, c_long, c_uint, c_void};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};

use gal_feature_api::hooks::{self as api, dispatch as hd, FuelData, KeyEvent, SpeedData};
use gal_feature_api::send_lock;
use gal_feature_api::session::{self, End};

use crate::sys;
use crate::{guard, ldr_log};
use qnx_dl::NextFn;

type P = *mut c_void;
type CP = *const c_void;

/// Preserve native return registers r0 and r1. The mangled parameter name does not
/// encode the return type. This representation passes through boolean, integer, pointer
/// and 64-bit results. callers of void functions ignore them. A path that does not call
/// the original must select its result from the native caller contract.
type GalRet = u64;

/// gal's real definition of a mangled method: found through `RTLD_NEXT` on
/// first use, cached, and typed with its signature.
macro_rules! real {
    ($name:ident: $ty:ty = $sym:expr) => {
        // SAFETY: gal defines this symbol with exactly this signature (the
        // mangled name encodes it. see the ABI notes above).
        static $name: NextFn<$ty> = unsafe { NextFn::new($sym) };
    };
}

/// Call the cached original, or evaluate `$else` when gal does not define
/// it.
macro_rules! orig {
    ($next:expr, $else:expr, |$f:ident| $call:expr) => {{
        match $next.get() {
            None => $else,
            // SAFETY: `real!` typed the pointer with gal's signature.
            Some($f) => unsafe { $call },
        }
    }};
}

// ---------------------------------------------------------------------------
// Discovery / registration observers: original first, then every callback.
// ---------------------------------------------------------------------------

real!(VS_ADD_DISC: unsafe extern "C" fn(P, P) -> GalRet =
    c"_ZN9VideoSink16addDiscoveryInfoEP24ServiceDiscoveryResponse");
real!(SS_ADD_DISC: unsafe extern "C" fn(P, P) -> GalRet =
    c"_ZN12SensorSource16addDiscoveryInfoEP24ServiceDiscoveryResponse");
real!(IS_ADD_DISC: unsafe extern "C" fn(P, P) -> GalRet =
    c"_ZN11InputSource16addDiscoveryInfoEP24ServiceDiscoveryResponse");
real!(GR_REG_SVC: unsafe extern "C" fn(P, P) -> GalRet =
    c"_ZN11GalReceiver15registerServiceEP20ProtocolEndpointBase");
real!(MR_CH_OPENED: unsafe extern "C" fn(P, u8, P) -> GalRet =
    c"_ZN13MessageRouter19notifyChannelOpenedEhRK18ChannelOpenRequest");

// Exported by entry.rs as "_ZN9VideoSink16addDiscoveryInfoEP24ServiceDiscoveryResponse"
pub extern "C" fn video_sink_add_discovery_info(this: P, resp: P) -> GalRet {
    let r = orig!(VS_ADD_DISC, 0, |f| f(this, resp));
    hd::discovery(&api::VIDEO_DISCOVERY, this, resp);
    r
}

// Exported by entry.rs as "_ZN12SensorSource16addDiscoveryInfoEP24ServiceDiscoveryResponse"
pub extern "C" fn sensor_source_add_discovery_info(this: P, resp: P) -> GalRet {
    let r = orig!(SS_ADD_DISC, 0, |f| f(this, resp));
    hd::discovery(&api::SENSOR_DISCOVERY, this, resp);
    r
}

// Exported by entry.rs as "_ZN11InputSource16addDiscoveryInfoEP24ServiceDiscoveryResponse"
pub extern "C" fn input_source_add_discovery_info(this: P, resp: P) -> GalRet {
    let r = orig!(IS_ADD_DISC, 0, |f| f(this, resp));
    hd::discovery(&api::INPUT_DISCOVERY, this, resp);
    r
}

// Export the receiver registerService interposer. Forward its original result. native
// callers branch on that result.
pub extern "C" fn gal_receiver_register_service(this: P, ep: P) -> GalRet {
    let r = orig!(GR_REG_SVC, 0, |f| f(this, ep));
    hd::discovery(&api::REGISTER_SERVICE, this, ep);
    r
}

// Exported by entry.rs as "_ZN13MessageRouter19notifyChannelOpenedEhRK18ChannelOpenRequest"
pub extern "C" fn message_router_notify_channel_opened(mr: P, ch: u8, req: P) -> GalRet {
    let r = orig!(MR_CH_OPENED, 0, |f| f(mr, ch, req));
    hd::channel_opened(mr, ch, req);
    r
}

// ---------------------------------------------------------------------------
// Video focus
// ---------------------------------------------------------------------------

real!(VS_FOCUS_REQ: unsafe extern "C" fn(P, CP) -> c_int =
    c"_ZN9VideoSink23handleVideoFocusRequestERK29VideoFocusRequestNotification");
real!(VS_SET_FOCUS: unsafe extern "C" fn(P, c_int, c_int) -> GalRet =
    c"_ZN9VideoSink13setVideoFocusEib");

/// Track the last sink and the first sink separately. Each event log identifies its
/// sink pointer.
static VIDEO_SINK: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static VIDEO_SINK_FIRST: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

fn capture_sink(this: P) {
    VIDEO_SINK.store(this, Ordering::Relaxed);
    let _ = VIDEO_SINK_FIRST.compare_exchange(
        core::ptr::null_mut(),
        this,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

/// (mode, reason) from a `VideoFocusRequestNotification`: has-bits at +0x20,
/// reason at +0x2c, mode at +0x30 when has-bit 0x4 is set (RE-confirmed).
pub fn decode_focus(head: &[u8; 0x34]) -> (i32, i32) {
    let rd = |o: usize| i32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
    let hasbits = rd(gal_vendor_abi::offsets::VIDEO_FOCUS_REQUEST_HASBITS).cast_unsigned();
    let reason = rd(gal_vendor_abi::offsets::VIDEO_FOCUS_REQUEST_REASON);
    let mode = if hasbits & 0x4 != 0 {
        rd(gal_vendor_abi::offsets::VIDEO_FOCUS_REQUEST_MODE)
    } else {
        0
    };
    (mode, reason)
}

// Exported by entry.rs as "_ZN9VideoSink23handleVideoFocusRequestERK29VideoFocusRequestNotification"
pub extern "C" fn video_sink_handle_video_focus_request(this: P, req: CP) -> c_int {
    capture_sink(this);
    let (mode, reason) = guard("handleVideoFocusRequest", || {
        let (mode, reason) = if req.is_null() {
            (0, 0)
        } else {
            let mut head = [0u8; 0x34];
            // SAFETY: the notification object is larger than 0x34 bytes
            // (fields up to +0x30 are RE-confirmed). byte copy, no alignment.
            unsafe {
                core::ptr::copy_nonoverlapping(req.cast::<u8>(), head.as_mut_ptr(), head.len());
            };
            decode_focus(&head)
        };
        let first = this == VIDEO_SINK_FIRST.load(Ordering::Relaxed);
        ldr_log!(
            "[GalLoader] videoFocusRequest: sink={:p}{} mode={} ({}) reason={}",
            this,
            if first { " (first observed sink)" } else { "" },
            mode,
            match mode {
                2 => "NATIVE/exit",
                1 => "PROJECTED/AA",
                _ => "?",
            },
            reason
        );
        (mode, reason)
    })
    .unwrap_or((0, 0));
    let ret = orig!(VS_FOCUS_REQ, 1, |f| f(this, req));
    hd::video_focus(this, mode, reason, req);
    ret
}

// Exported by entry.rs as "_ZN9VideoSink13setVideoFocusEib"
pub extern "C" fn video_sink_set_video_focus(this: P, mode: c_int, unsolicited: c_int) -> GalRet {
    guard("setVideoFocus", || {
        ldr_log!(
            "[GalLoader] setVideoFocus: sink={:p} mode={} unsolicited={}",
            this,
            mode,
            unsolicited
        );
    });
    capture_sink(this);
    let r = orig!(VS_SET_FOCUS, 0, |f| f(this, mode, unsolicited));
    // After gal's own grant has gone out.
    hd::video_focus_grant(this, mode, unsolicited);
    r
}

/// Notify the phone of a focus mode through gal's real setVideoFocus on the
/// captured sink (does not re-enter the interposer). Installed as
/// `LoaderServices::video_request_focus`.
pub fn request_focus(mode: c_int, unsolicited: c_int) -> bool {
    let sink = VIDEO_SINK.load(Ordering::Relaxed);
    if sink.is_null() {
        return false;
    }
    orig!(VS_SET_FOCUS, false, |f| {
        f(sink, mode, unsolicited);
        true
    })
}

// ---------------------------------------------------------------------------
// Session teardown and serialized sends
// ---------------------------------------------------------------------------

real!(CTRL_PING: unsafe extern "C" fn(P, i64, u8) -> GalRet =
    c"_ZN10Controller15sendPingRequestExb");
real!(CTRL_PING_RESP: unsafe extern "C" fn(P, CP) -> GalRet =
    c"_ZN10Controller18handlePingResponseERK12PingResponse");
real!(CTRL_PING_REQ: unsafe extern "C" fn(P, CP) -> GalRet =
    c"_ZN10Controller17handlePingRequestERK11PingRequest");
real!(CTRL_BYEBYE_REQ: unsafe extern "C" fn(P, c_int) -> GalRet =
    c"_ZN10Controller17sendByeByeRequestE12ByeByeReason");
real!(CTRL_BYEBYE_RESP: unsafe extern "C" fn(P) -> GalRet =
    c"_ZN10Controller18sendByeByeResponseEv");
real!(CH_ENQUEUE: unsafe extern "C" fn(P, P) -> c_long =
    c"_ZN7Channel15enqueueOutgoingERK10shared_ptrI5FrameE");

static PING_SENT: AtomicU32 = AtomicU32::new(0);
static PING_ANSWERED: AtomicU32 = AtomicU32::new(0);
static PING_FROM_PHONE: AtomicU32 = AtomicU32::new(0);
static PING_BACKLOG: AtomicBool = AtomicBool::new(false);
/// Every outbound frame funnels through here. one global recursive lock
/// serializes the enqueue across gal's threads and ours (the
/// intermittent SIGABRT). Returns gal's r0 whatever its real width.
// Exported by entry.rs as "_ZN7Channel15enqueueOutgoingERK10shared_ptrI5FrameE"
pub extern "C" fn channel_enqueue_outgoing(this: P, frame_ref: P) -> c_long {
    let Some(f) = CH_ENQUEUE.get() else {
        return 0;
    };
    if !send_lock::ready() {
        // SAFETY: gal's Channel::enqueueOutgoing(this, const shared_ptr<Frame>&).
        return unsafe { f(this, frame_ref) };
    }
    send_lock::lock();
    // SAFETY: as above.
    let r = unsafe { f(this, frame_ref) };
    send_lock::unlock();
    r
}

// Exported by entry.rs as "_ZN10Controller15sendPingRequestExb"
pub extern "C" fn controller_send_ping_request(this: P, ts: i64, b: u8) -> GalRet {
    let sent = PING_SENT.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    // Hold the send lock across the complete ping send to prevent a serializer race.
    send_lock::lock();
    guard("sendPingRequest", || {
        // Healthy baseline is 2 outstanding. one line in, one line out.
        let answered = PING_ANSWERED.load(Ordering::Relaxed);
        let outstanding = sent.wrapping_sub(answered);
        if outstanding >= 4 && !PING_BACKLOG.load(Ordering::Relaxed) {
            PING_BACKLOG.store(true, Ordering::Relaxed);
            ldr_log!(
                "[GalLoader] PING BACKLOG: {} outstanding at ping #{} (answered={}) \
                 - healthy baseline is 2",
                outstanding,
                sent,
                answered
            );
            transport_report("PING BACKLOG");
        } else if outstanding <= 2 && PING_BACKLOG.load(Ordering::Relaxed) {
            PING_BACKLOG.store(false, Ordering::Relaxed);
            ldr_log!(
                "[GalLoader] PING BACKLOG CLEARED at ping #{} (answered={}, outstanding={})",
                sent,
                answered,
                outstanding
            );
        }
    });
    let r = orig!(
        CTRL_PING,
        {
            ldr_log!("[GalLoader] FATAL-ISH: sendPingRequest orig=NULL - call SWALLOWED");
            0
        },
        |f| f(this, ts, b)
    );
    send_lock::unlock();
    r
}

// Exported by entry.rs as "_ZN10Controller18handlePingResponseERK12PingResponse"
pub extern "C" fn controller_handle_ping_response(this: P, resp: CP) -> GalRet {
    PING_ANSWERED.fetch_add(1, Ordering::Relaxed);
    orig!(
        CTRL_PING_RESP,
        {
            ldr_log!("[GalLoader] FATAL-ISH: handlePingResponse orig=NULL - call SWALLOWED");
            0
        },
        |f| f(this, resp)
    )
}

// Exported by entry.rs as "_ZN10Controller17handlePingRequestERK11PingRequest"
pub extern "C" fn controller_handle_ping_request(this: P, req: CP) -> GalRet {
    PING_FROM_PHONE.fetch_add(1, Ordering::Relaxed);
    orig!(
        CTRL_PING_REQ,
        {
            ldr_log!("[GalLoader] FATAL-ISH: handlePingRequest orig=NULL - call SWALLOWED");
            0
        },
        |f| f(this, req)
    )
}

fn ping_counts() -> (u32, u32, u32, u32) {
    let s = PING_SENT.load(Ordering::Relaxed);
    let a = PING_ANSWERED.load(Ordering::Relaxed);
    (
        s,
        a,
        s.wrapping_sub(a),
        PING_FROM_PHONE.load(Ordering::Relaxed),
    )
}

// Exported by entry.rs as "_ZN10Controller17sendByeByeRequestE12ByeByeReason"
pub extern "C" fn controller_send_byebye_request(this: P, reason: c_int) -> GalRet {
    guard("sendByeByeRequest", || {
        let (s, a, o, p) = ping_counts();
        ldr_log!(
            "[GalLoader] *** DISCONNECT INITIATED BY US (HU): sendByeByeRequest(reason={}) \
             - pings sent={} answered={} outstanding={}, phone-pings={}",
            reason,
            s,
            a,
            o,
            p
        );
        transport_report("ByeByeRequest (we initiated)");
        crate::channel::report("ByeByeRequest (we initiated)");
    });
    orig!(CTRL_BYEBYE_REQ, 0, |f| f(this, reason))
}

// Exported by entry.rs as "_ZN10Controller18sendByeByeResponseEv"
pub extern "C" fn controller_send_byebye_response(this: P) -> GalRet {
    guard("sendByeByeResponse", || {
        let (s, a, o, p) = ping_counts();
        ldr_log!(
            "[GalLoader] *** DISCONNECT INITIATED BY THE PHONE: sendByeByeResponse() \
             - pings sent={} answered={} outstanding={}, phone-pings={}",
            s,
            a,
            o,
            p
        );
        transport_report("ByeByeResponse (phone initiated)");
        crate::channel::report("ByeByeResponse (phone initiated)");
    });
    orig!(CTRL_BYEBYE_RESP, 0, |f| f(this))
}

// ---------------------------------------------------------------------------
// Inbound SensorRequest and InputSource keys: a callback returning non-zero
// suppresses gal's own handler.
// ---------------------------------------------------------------------------

real!(SS_SENSOR_REQ: unsafe extern "C" fn(P, CP) -> GalRet =
    c"_ZN12SensorSource19handleSensorRequestERK13SensorRequest");
real!(IS_SEND_KEY: unsafe extern "C" fn(P, u64, c_uint, u8, u8, c_uint) -> GalRet =
    c"_ZN11InputSource7sendKeyEyjbbj");
real!(IS_LONG_PRESS: unsafe extern "C" fn(P, u64, c_uint, c_uint) -> GalRet =
    c"_ZN11InputSource18reportKeyLongPressEyjj");

/// `STATUS_SUCCESS`: what a request a feature handled reports to gal.
const GAL_STATUS_SUCCESS: GalRet = 0;

// Exported by entry.rs as "_ZN12SensorSource19handleSensorRequestERK13SensorRequest"
// SensorSource::routeMessage returns this value (0 success, -253 on
// a parse failure), so the hook returns gal's value, or success when a
// feature handled the request.
pub extern "C" fn sensor_source_handle_sensor_request(this: P, req: CP) -> GalRet {
    if hd::sensor_request(this, req) {
        GAL_STATUS_SUCCESS
    } else {
        orig!(SS_SENSOR_REQ, GAL_STATUS_SUCCESS, |f| f(this, req))
    }
}

/// Dispatch sendKey with the timestamp high word, keycode, press state and byte flag,
/// plus the complete typed timestamp. Export the original mangled `InputSource` entry.
pub extern "C" fn input_source_send_key(
    this: P,
    ts: u64,
    kc: c_uint,
    down: u8,
    b2: u8,
    m: c_uint,
) -> GalRet {
    let ev = KeyEvent {
        ts,
        seq: (ts >> 32) as u32,
        keycode: kc.cast_signed(),
        down: down != 0,
        extra: u32::from(b2 != 0),
        long_press: false,
    };
    if hd::input_key(this, &ev) {
        // A feature took the key. Checked: gal's callers of
        // sendKey do not read the result.
        0
    } else {
        orig!(IS_SEND_KEY, 0, |f| f(this, ts, kc, down, b2, m))
    }
}

/// Dispatch long press with zero, keycode, active flag and duration. Export the
/// original mangled `InputSource` entry.
pub extern "C" fn input_source_report_key_long_press(
    this: P,
    ts: u64,
    kc: c_uint,
    x: c_uint,
) -> GalRet {
    let ev = KeyEvent {
        ts,
        seq: 0,
        keycode: kc.cast_signed(),
        down: true,
        extra: x,
        long_press: true,
    };
    if hd::input_key(this, &ev) {
        // A feature took the key. Checked: gal and its AA
        // library have no direct caller that reads the result.
        0
    } else {
        orig!(IS_LONG_PRESS, 0, |f| f(this, ts, kc, x))
    }
}

// ---------------------------------------------------------------------------
// Sensor mutators: callbacks may rewrite the values before gal sends them.
// ---------------------------------------------------------------------------

real!(SS_FUEL: unsafe extern "C" fn(P, c_int, c_int, c_int, c_int, c_int, c_int) -> GalRet =
    c"_ZN12SensorSource14reportFuelDataEbibibb");
real!(SS_SPEED: unsafe extern "C" fn(P, c_int, u8, u8, u8, c_int) -> GalRet =
    c"_ZN12SensorSource15reportSpeedDataEibbbi");

// Exported by entry.rs as "_ZN12SensorSource14reportFuelDataEbibibb"
pub extern "C" fn sensor_source_report_fuel_data(
    this: P,
    has_level: c_int,
    level: c_int,
    has_range: c_int,
    range_m: c_int,
    has_low: c_int,
    low_fuel: c_int,
) -> GalRet {
    // gal's caller branches on the result. return gal's value.
    let mut d = FuelData {
        has_level,
        level,
        has_range,
        range_m,
        has_low,
        low_fuel,
    };
    hd::fuel(this, &mut d);
    orig!(SS_FUEL, 0, |f| f(
        this,
        d.has_level,
        d.level,
        d.has_range,
        d.range_m,
        d.has_low,
        d.low_fuel
    ))
}

// Exported by entry.rs as "_ZN12SensorSource15reportSpeedDataEibbbi"
pub extern "C" fn sensor_source_report_speed_data(
    this: P,
    speed_e3: c_int,
    has_cruise: c_uint,
    cruise_engaged: c_uint,
    has_cruise_speed: c_uint,
    cruise_speed_e3: c_int,
) -> GalRet {
    let mut d = speed_data_from_native_words(
        speed_e3,
        has_cruise,
        cruise_engaged,
        has_cruise_speed,
        cruise_speed_e3,
    );
    hd::speed(this, &mut d);
    orig!(SS_SPEED, 0, |f| f(
        this,
        d.speed_e3,
        u8::from(d.has_cruise != 0),
        u8::from(d.cruise_engaged != 0),
        u8::from(d.has_cruise_speed != 0),
        d.cruise_speed_e3
    ))
}

fn speed_data_from_native_words(
    speed_e3: c_int,
    has_cruise: c_uint,
    cruise_engaged: c_uint,
    has_cruise_speed: c_uint,
    cruise_speed_e3: c_int,
) -> SpeedData {
    // Native f83d8 reads the stacked bool with ldrb. An extern u8 parameter
    // lets LLVM assume caller extension and retain poisoned upper padding.
    // Accept entire ABI words so these masks cannot be optimized away.
    SpeedData {
        speed_e3,
        has_cruise: (has_cruise & 0xff).cast_signed(),
        cruise_engaged: (cruise_engaged & 0xff).cast_signed(),
        has_cruise_speed: (has_cruise_speed & 0xff).cast_signed(),
        cruise_speed_e3,
    }
}

// ---------------------------------------------------------------------------
// Transport byte-flow witness and the wireless providers
// ---------------------------------------------------------------------------

real!(TR_READ: TransportIo = c"_ZN9Transport10readOrFailEPvj");
real!(TR_WRITE: TransportIo = c"_ZN9Transport11writeOrFailEPvj");

static TR_READS: AtomicU32 = AtomicU32::new(0);
static TR_WRITES: AtomicU32 = AtomicU32::new(0);
static TR_RBYTES: AtomicU32 = AtomicU32::new(0);
static TR_WBYTES: AtomicU32 = AtomicU32::new(0);
static TR_LAST_READ_MS: AtomicU32 = AtomicU32::new(0);
static TR_LAST_WRITE_MS: AtomicU32 = AtomicU32::new(0);
static TR_LAST_RET: AtomicI32 = AtomicI32::new(0);

/// Readable from a DIFFERENT thread than one that may be stuck.
pub fn transport_report(what: &str) {
    let t = sys::now_ms();
    let lr = TR_LAST_READ_MS.load(Ordering::Relaxed);
    let lw = TR_LAST_WRITE_MS.load(Ordering::Relaxed);
    ldr_log!(
        "[GalLoader] TRANSPORT at {}: reads={} ok ({} bytes) last {}ms ago (ret={} \
         is a success flag, not a count); writes={} ({} bytes) last {}ms ago",
        what,
        TR_READS.load(Ordering::Relaxed),
        TR_RBYTES.load(Ordering::Relaxed),
        if lr != 0 { t.wrapping_sub(lr) } else { 0 },
        TR_LAST_RET.load(Ordering::Relaxed),
        TR_WRITES.load(Ordering::Relaxed),
        TR_WBYTES.load(Ordering::Relaxed),
        if lw != 0 { t.wrapping_sub(lw) } else { 0 }
    );
}

type TransportIo = unsafe extern "C" fn(P, P, c_uint) -> c_int;

// Exported by entry.rs as "_ZN9Transport10readOrFailEPvj"
pub extern "C" fn transport_read_or_fail(this: P, buf: P, len: c_uint) -> c_int {
    let ret = match hd::read_provider(this, buf, len) {
        Some(r) => r,
        None => orig!(TR_READ, -1, |f| f(this, buf, len)),
    };
    if ret > 0 {
        // readOrFail returns 1 for success, not a byte count. the bytes are
        // `len` (measured on-car).
        TR_READS.fetch_add(1, Ordering::Relaxed);
        TR_RBYTES.fetch_add(len, Ordering::Relaxed);
        TR_LAST_READ_MS.store(sys::now_ms(), Ordering::Relaxed);
    }
    TR_LAST_RET.store(ret, Ordering::Relaxed);
    hd::transport_read(this, buf, len, ret);
    ret
}

// Exported by entry.rs as "_ZN9Transport11writeOrFailEPvj"
pub extern "C" fn transport_write_or_fail(this: P, buf: P, len: c_uint) -> c_int {
    hd::transport_write(this, buf, len);
    TR_WRITES.fetch_add(1, Ordering::Relaxed);
    TR_WBYTES.fetch_add(len, Ordering::Relaxed);
    TR_LAST_WRITE_MS.store(sys::now_ms(), Ordering::Relaxed);
    let ret = match hd::write_provider(this, buf, len) {
        Some(r) => r,
        None => orig!(TR_WRITE, -1, |f| f(this, buf, len)),
    };
    // A failed write ends the session as surely as a failed read: tell the
    // built-in modules (gal-feature-api `session`).
    if ret <= 0 && len > 0 {
        guard("session end (write)", || {
            session::dispatch(End::WriteFailed);
        });
    }
    ret
}

// ---------------------------------------------------------------------------
// Notify built-in modules before native receiver teardown, so they stop
// using session objects before the receiver releases them.
// ---------------------------------------------------------------------------

real!(GR_PREPARE_SHUTDOWN: unsafe extern "C" fn(P) -> GalRet =
    c"_ZN11GalReceiver15prepareShutdownEv");
real!(GR_SHUTDOWN: unsafe extern "C" fn(P) -> GalRet = c"_ZN11GalReceiver8shutdownEv");

// Exported by entry.rs as "_ZN11GalReceiver15prepareShutdownEv"
pub extern "C" fn gal_receiver_prepare_shutdown(this: P) -> GalRet {
    guard("GalReceiver::prepareShutdown", || {
        ldr_log!("[GalLoader] GalReceiver::prepareShutdown - session teardown");
        session::dispatch(End::PrepareShutdown);
    });
    orig!(GR_PREPARE_SHUTDOWN, 0, |f| f(this))
}

// Exported by entry.rs as "_ZN11GalReceiver8shutdownEv"
pub extern "C" fn gal_receiver_shutdown(this: P) -> GalRet {
    guard("GalReceiver::shutdown", || {
        ldr_log!("[GalLoader] GalReceiver::shutdown - session teardown");
        session::dispatch(End::Shutdown);
    });
    orig!(GR_SHUTDOWN, 0, |f| f(this))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_binding_matches_retained_firmware_export() {
        // The speed callback uses this exact exported symbol.
        let actual = "_ZN12SensorSource15reportSpeedDataEibbbi";
        assert_eq!(SS_SPEED.name().to_str().unwrap(), actual);
        let exports: Vec<_> = include_str!("../exports.txt").lines().collect();
        assert_eq!(exports.iter().filter(|name| **name == actual).count(), 1);
        assert!(!exports.contains(&"_ZN12SensorSource15reportSpeedDataEibibi"));
        let _: extern "C" fn(P, c_int, c_uint, c_uint, c_uint, c_int) -> GalRet =
            sensor_source_report_speed_data;
        let _: &NextFn<unsafe extern "C" fn(P, c_int, u8, u8, u8, c_int) -> GalRet> = &SS_SPEED;
    }

    #[test]
    fn speed_native_words_discard_only_boolean_padding() {
        let data =
            speed_data_from_native_words(-23456, 0xa5a5_a500, 0x5a5a_5a01, 0xa5a5_a500, -876_543);
        assert_eq!(data.speed_e3, -23456);
        assert_eq!(data.has_cruise, 0);
        assert_eq!(data.cruise_engaged, 1);
        assert_eq!(data.has_cruise_speed, 0);
        assert_eq!(data.cruise_speed_e3, -876_543);
        let data = speed_data_from_native_words(12345, 1, 0, 0x5a5a_5a01, 0x0234_5678);
        assert_eq!(data.has_cruise, 1);
        assert_eq!(data.cruise_engaged, 0);
        assert_eq!(data.has_cruise_speed, 1);
        assert_eq!(data.cruise_speed_e3, 0x0234_5678);
    }

    #[test]
    fn focus_decode_uses_hasbit() {
        let mut h = [0u8; 0x34];
        h[0x2c] = 4; // reason USER_SELECTION
        h[0x30] = 2; // mode NATIVE
        assert_eq!(decode_focus(&h), (0, 4));
        h[0x20] = 0x4;
        assert_eq!(decode_focus(&h), (2, 4));
    }
}
