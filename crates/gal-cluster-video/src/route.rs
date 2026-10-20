// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Android Auto cluster channels and messages. This module owns encoded outgoing
//! messages, endpoint objects, receive routing, endpoint registration, discovery
//! injection and bounded screen-ready workers.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;

use gal_feature_api::{channel, send_lock};
use gal_vendor_abi::endpoint::{FakeEndpoint, Vtable, IS_OPEN, OPEN_CHANNEL};
use gal_vendor_abi::offsets as off;
use gal_vendor_abi::symbols as sym;
use gal_vendor_abi::Ptr;

use crate::cfg::{as_u32, CFG};
use crate::log::{self, cl_log, cl_loge, now_ms, uptime_s, wall_s, CPtr, HexSp};
use crate::pipe::{self, lock, Most, MOST};
use crate::sdr::{self, Layout};
use crate::vfn::{self, Ctx};
use crate::{dm, gate, h264, video};

// ---------------------------------------------------------------------------
// Protocol state (routing thread, read elsewhere)
// ---------------------------------------------------------------------------

/// `g_mr`: gal's `MessageRouter` (= `GalReceiver`).
pub static MR: AtomicUsize = AtomicUsize::new(0);
static QOUT: AtomicUsize = AtomicUsize::new(0);
static REG_SVC: AtomicUsize = AtomicUsize::new(0);
/// `g_cluster_inch`, `s_nav_ch`.
pub static CLUSTER_INCH: AtomicU8 = AtomicU8::new(0xFF);
pub static NAV_CH: AtomicU8 = AtomicU8::new(0xFF);
/// `s_started`, `s_ics_sent`, `s_setup_done`.
pub static STARTED: AtomicBool = AtomicBool::new(false);
static ICS_SENT: AtomicBool = AtomicBool::new(false);
static SETUP_DONE: AtomicBool = AtomicBool::new(false);
/// `s_session_id` (the phone's), `s_ack_counter` (logs only).
pub static SESSION_ID: AtomicI32 = AtomicI32::new(0);
pub static ACK_COUNTER: AtomicU32 = AtomicU32::new(0);
/// `s_ch_taken`: ids gal's own endpoints registered.
static CH_TAKEN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static EP_REGISTERED: AtomicBool = AtomicBool::new(false);
/// `s_main_video_sink` and the main-screen PROJECTED latch.
static MAIN_VIDEO_SINK: AtomicUsize = AtomicUsize::new(0);
static MAIN_VFN_PROJECTED: AtomicBool = AtomicBool::new(false);

type QueueOutgoing = unsafe extern "C" fn(Ptr, u8, Ptr, u32) -> i32;
type RegisterService = unsafe extern "C" fn(Ptr, Ptr) -> i32;

fn qout() -> Option<QueueOutgoing> {
    let p = QOUT.load(Ordering::Acquire);
    // SAFETY: stored from MR_QUEUE_OUTGOING's typed lookup.
    (p != 0).then(|| unsafe { core::mem::transmute::<usize, QueueOutgoing>(p) })
}

fn reg_svc() -> Option<RegisterService> {
    let p = REG_SVC.load(Ordering::Acquire);
    // SAFETY: stored from MR_REGISTER_SERVICE's typed lookup.
    (p != 0).then(|| unsafe { core::mem::transmute::<usize, RegisterService>(p) })
}

fn mr() -> Ptr {
    MR.load(Ordering::Acquire) as Ptr
}

/// `g_qout(g_mr, ch, msg, len)` when both are known.
fn queue(ch: u8, msg: &[u8]) -> Option<i32> {
    let q = qout()?;
    let m = mr();
    if m.is_null() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: gal's MessageRouter::queueOutgoing copies the bytes.
    Some(unsafe { q(m, ch, msg.as_ptr().cast_mut().cast(), msg.len() as u32) })
}

/// `phase2_resolve`.
fn phase2_resolve() {
    if qout().is_none() {
        match sym::MR_QUEUE_OUTGOING.resolve() {
            Some(f) => {
                QOUT.store(f as usize, Ordering::Release);
                cl_log!("[GalCluster] phase2: queueOutgoing -> {}", CPtr(f as usize));
            }
            None => cl_loge!("[GalCluster] phase2: queueOutgoing dlsym FAILED"),
        }
    }
    if reg_svc().is_none() {
        match sym::MR_REGISTER_SERVICE.resolve() {
            Some(f) => {
                REG_SVC.store(f as usize, Ordering::Release);
                cl_log!(
                    "[GalCluster] phase2: registerService -> {}",
                    CPtr(f as usize)
                );
            }
            None => cl_loge!("[GalCluster] phase2: registerService dlsym FAILED"),
        }
    }
}

// ---------------------------------------------------------------------------
// Sends
// ---------------------------------------------------------------------------

/// `try_send_ics`: `InstrumentClusterStart` on the NAVIGATION channel.
fn try_send_ics(nav_ch: u8) {
    let ics = sdr::NAV_INSTRUMENT_CLUSTER_START.to_be_bytes();
    let Some(rc) = queue(nav_ch, &ics) else {
        cl_loge!(
            "[GalCluster] try_send_ics nav_ch={}: qout={} mr={} \u{2014} deferred",
            nav_ch,
            CPtr(QOUT.load(Ordering::Acquire)),
            CPtr(MR.load(Ordering::Acquire))
        );
        return;
    };
    cl_loge!(
        "[GalCluster] nav_ch={} sent InstrumentClusterStart rc={}",
        nav_ch,
        rc
    );
    if rc == 0 {
        ICS_SENT.store(true, Ordering::Release);
    }
}

/// The caller holds a send pin. Hold the loader send lock for emission, including calls
/// from worker threads.
fn vfn_emit_pinned(ch: u8, focus: u32, unsolicited: u32) {
    let mut msg = [0u8; 10];
    let n = sdr::video_focus_notification(&mut msg, focus, unsolicited);
    let rc = if qout().is_some() && !mr().is_null() {
        let _g = send_lock::hold();
        queue(ch, &msg[..n]).unwrap_or(-1)
    } else {
        -1
    };
    cl_log!(
        "[GalCluster] ch={} sent VideoFocusNotification focus={} unsolicited={} rc={}",
        ch,
        focus,
        unsolicited,
        rc
    );
}

/// `send_video_focus_notification`: pin, emit, unpin.
pub fn send_video_focus_notification(ch: u8, focus: u32, unsolicited: u32) {
    vfn::pin();
    vfn_emit_pinned(ch, focus, unsolicited);
    vfn::unpin();
}

/// `send_av_setup_response`.
fn send_av_setup_response(ch: u8) {
    SESSION_ID.store(0, Ordering::Relaxed);
    ACK_COUNTER.store(0, Ordering::Relaxed);
    let mut msg = [0u8; 24];
    let n = sdr::setup_response(&mut msg, as_u32(CFG.cluster_max_unacked()));
    cl_loge!(
        "[GalCluster] ch={} SETUP_RESPONSE bytes[{}]: {}",
        ch,
        n,
        HexSp(&msg[..n.min(20)])
    );
    let rc = queue(ch, &msg[..n]).unwrap_or(-1);
    cl_loge!(
        "[GalCluster] ch={} sent AVChannelSetupResponse (Config) rc={}",
        ch,
        rc
    );
}

/// `send_media_ack`: ack one frame (ack count 1).
pub fn send_media_ack(ch: u8) {
    let n_ack = ACK_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let sid = SESSION_ID.load(Ordering::Relaxed);
    let mut msg = [0u8; 16];
    let n = sdr::media_ack(&mut msg, sid);
    if queue(ch, &msg[..n]).is_none() {
        cl_loge!(
            "[GalCluster] ch={} ACK#{}: g_qout={} g_mr={} \u{2014} ACK NOT SENT",
            ch,
            n_ack,
            CPtr(QOUT.load(Ordering::Acquire)),
            CPtr(MR.load(Ordering::Acquire))
        );
    }
    if n_ack % pipe::LOG_PERIOD_FRAMES == 1 {
        cl_log!("[GalCluster] ch={} ACK#{} session_id={}", ch, n_ack, sid);
    }
}

/// `kick_fresh_idr`: NATIVE then PROJECTED, at most once per 3 s.
pub fn kick_fresh_idr(most: &mut Most, ch: u8) {
    let now = wall_s();
    if now - most.idr_kick_time < 3 {
        return;
    }
    most.idr_kick_time = now;
    cl_log!(
        "[GalCluster] ch={} kicking fresh IDR via unsolicited VideoFocusNotification NATIVE->PROJECTED",
        ch
    );
    send_video_focus_notification(ch, sdr::FOCUS_NATIVE, sdr::VFN_UNSOLICITED);
    send_video_focus_notification(ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
}

// ---------------------------------------------------------------------------
// Workers (detached threads. a thread, never a forked child)
// ---------------------------------------------------------------------------

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

fn spawn_worker(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), i32> {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(64 * 1024)
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                cl_loge!("[GalCluster] PANIC contained in a worker thread; it ends");
            }
        })
        .map(drop)
        .map_err(|e| e.raw_os_error().unwrap_or(0))
}

/// `cb_resume_thread`: after the cooldown, hand the stream back.
fn cb_resume(c: Ctx) {
    let wait_s = if CFG.cb_cooldown_secs() > 0 {
        CFG.cb_cooldown_secs()
    } else {
        1
    };
    #[allow(clippy::cast_sign_loss)]
    sleep_ms(wait_s as u64 * 1000);
    if c.gen != 0 && !vfn::still_current(c) {
        vfn::worker_cancel(c, "breaker-resume cooldown");
        return;
    }
    if pipe::CB_LOCKED_OUT.load(Ordering::Acquire) {
        cl_log!("[GalCluster] cb_resume: locked out \u{2014} leaving cluster stream parked");
        return;
    }
    cl_log!(
        "[GalCluster] cb_resume: cooldown ({}s) done \u{2014} sending unsolicited VideoFocusNotification(PROJECTED) (phone restarts its encoder -> fresh IDR)",
        wait_s
    );
    send_video_focus_notification(c.ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
}

/// `cb_schedule_resume`.
pub fn cb_schedule_resume(ch: u8) {
    let (gen, cur_ch) = vfn::current_generation();
    let c = Ctx {
        ch,
        gen: if cur_ch == ch { gen } else { 0 },
    };
    if spawn_worker("cluster-resume", move || cb_resume(c)).is_err() {
        cl_loge!("[GalCluster] cb_resume: thread create failed \u{2014} cluster stays parked until reconnect");
    }
}

// Keep handover under MOST through the post-call generation check. If the
// bounded send drain expired during a native call, undo this stale open
// before another worker can acquire MOST.
fn open_for_generation(c: Ctx) -> Option<i32> {
    if !vfn::pin_if_current(c) {
        vfn::worker_cancel(c, "native handover admission");
        return None;
    }
    let mut most = lock(&MOST);
    if !vfn::still_current(c) {
        drop(most);
        vfn::unpin();
        vfn::worker_cancel(c, "native handover lock admission");
        return None;
    }
    let result = pipe::most_open(&mut most);
    let current = vfn::still_current(c);
    if !current {
        pipe::most_close(&mut most);
        dm::restore_nav();
    }
    drop(most);
    vfn::unpin();
    current.then_some(result)
}

fn park_generation(c: Ctx) {
    if !vfn::pin_if_current(c) {
        return;
    }
    let mut most = lock(&MOST);
    if vfn::still_current(c) {
        pipe::most_close(&mut most);
        // No descriptor may have opened yet. That does not discharge the
        // native hide request, so restore even when most_close had no fd.
        dm::restore_nav();
        vfn::clear(c.gen, c.ch);
    }
    drop(most);
    vfn::unpin();
}

/// `delayed_vfn_thread`: the screen-ready worker.
#[allow(clippy::too_many_lines)]
fn screen_ready(c: Ctx) {
    let cfg = &CFG;
    if !vfn::still_current(c) {
        vfn::worker_cancel(c, "entry");
        return;
    }
    let settle = i64::from(cfg.boot_settle_secs());
    if settle > 0 {
        let up = uptime_s();
        // the uptime gate, or the HMI handover (gate::boot_settling)
        if gate::boot_settling(qnx_time::now(qnx_time::CLOCK_MONOTONIC)) {
            cl_log!(
                "[GalCluster] screen-ready focus: HU still booting (uptime {}s < settle {}s) \u{2014} holding the unsolicited VideoFocusNotification(PROJECTED) so the phone's first IDR lands on an open pipe",
                up,
                settle
            );
            let mut up;
            loop {
                if !vfn::still_current(c) {
                    vfn::worker_cancel(c, "boot-settle poll");
                    return;
                }
                let mono = qnx_time::now(qnx_time::CLOCK_MONOTONIC);
                up = log::long(mono.tv_sec);
                if !gate::boot_settling(mono) {
                    break;
                }
                sleep_ms(100);
            }
            cl_log!(
                "[GalCluster] screen-ready focus: boot settle reached at uptime {}s \u{2014} continuing bring-up",
                up
            );
        }
    }
    let mut hold = crate::handover::MainHold::new(
        now_ms(),
        u64::try_from(cfg.main_focus_wait_ms()).unwrap_or(0),
        u64::try_from(cfg.main_focus_extra_ms())
            .unwrap_or(0)
            .max(1500),
    );
    cl_log!("[GalCluster] handover generation {}: waiting for main-screen PROJECTED and stabilization before first hide", c.gen);
    loop {
        if !vfn::still_current(c) {
            vfn::worker_cancel(c, "main-screen stabilization");
            return;
        }
        match hold.poll(now_ms(), MAIN_VFN_PROJECTED.load(Ordering::Acquire)) {
            crate::handover::Decision::Hide => break,
            crate::handover::Decision::Park => {
                cl_loge!("[GalCluster] handover generation {}: main-screen focus unavailable; cluster remains parked", c.gen);
                park_generation(c);
                return;
            }
            crate::handover::Decision::Wait => sleep_ms(50),
        }
    }
    // A fresh companion witness adds the native cycle check. Without it,
    // encoder-off, handover and panel settle remain the admission gates.
    let cycle = crate::sink_cycle::FirstSession::new(
        gate::sink_cycle_snapshot(),
        now_ms(),
        gate::sink_wall_ms(),
    );
    if !vfn::still_current(c) {
        vfn::worker_cancel(c, "sink cycle baseline");
        return;
    }
    if !cycle.uses_witness() {
        cl_log!("[GalCluster] handover generation {}: no fresh listener sink-cycle witness; fallback=encoder-off-handover-settle", c.gen);
    }
    cl_log!("[GalCluster] handover generation {}: main-screen hold complete; issuing initial cluster hide", c.gen);
    if open_for_generation(c).is_none() {
        return;
    }
    let mut waited = 0i32;
    let cap_ms = 10_000;
    while cfg.gate_enable() != 0 && gate::encoder_live() && waited < cap_ms {
        if !vfn::still_current(c) {
            vfn::worker_cancel(c, "MPE encoder-close gate");
            return;
        }
        if open_for_generation(c).is_none() {
            return;
        }
        sleep_ms(50);
        waited += 50;
    }
    if !vfn::still_current(c) {
        vfn::worker_cancel(c, "MPE encoder-close gate exit");
        return;
    }
    if cfg.gate_enable() != 0 && gate::encoder_live() {
        cl_loge!(
            "[GalCluster] screen-ready focus: DM still tearing down its encoder after {}ms",
            waited
        );
        park_generation(c);
        return;
    }
    cl_log!(
        "[GalCluster] screen-ready focus: DM encoder-close done in {}ms",
        waited
    );
    if cycle.uses_witness() {
        cl_log!("[GalCluster] handover generation {}: waiting for a new native sink cycle after pre-hide baseline", c.gen);
    }
    loop {
        if !vfn::still_current(c) {
            vfn::worker_cancel(c, "native sink cycle");
            return;
        }
        match cycle.poll(gate::sink_cycle_snapshot(), now_ms(), gate::sink_wall_ms()) {
            crate::sink_cycle::Decision::Ready => break,
            crate::sink_cycle::Decision::Wait => sleep_ms(50),
            crate::sink_cycle::Decision::Park => {
                cl_loge!("[GalCluster] handover generation {}: native sink cycle absent, invalid or expired; cluster remains parked", c.gen);
                park_generation(c);
                return;
            }
        }
    }
    if cycle.uses_witness() {
        cl_log!(
            "[GalCluster] handover generation {}: new native sink cycle observed; panel settle follows",
            c.gen
        );
    }
    // cluster_ready_settle_ms applies when the boot gate releases on uptime or was
    // never closed. A handover release uses cluster_handover_settle_ms.
    let by_handover = crate::gate::GATE_BY_HANDOVER.load(Ordering::Relaxed);
    let settle_ms = if by_handover {
        cfg.handover_settle_ms()
    } else {
        cfg.ready_settle_ms()
    };
    if by_handover {
        cl_log!(
            "[GalCluster] screen-ready focus: the boot gate released on the HMI handover -- cluster_handover_settle_ms={}ms applies, not cluster_ready_settle_ms={}ms",
            cfg.handover_settle_ms(),
            cfg.ready_settle_ms()
        );
    }
    if settle_ms > 0 {
        cl_log!(
            "[GalCluster] screen-ready focus: waiting {}ms for the cluster panel to clear nav before starting",
            settle_ms
        );
        #[allow(clippy::cast_sign_loss)]
        sleep_ms(settle_ms as u64);
        if !vfn::still_current(c) {
            vfn::worker_cancel(c, "panel settle");
            return;
        }
    }
    // The panel wait can include a native encoder restart. Never grant
    // cluster focus from an earlier DOWN observation.
    let Some(opened) = open_for_generation(c) else {
        return;
    };
    if gate::encoder_live() || opened < 0 {
        cl_loge!("[GalCluster] handover generation {}: encoder UP or pipe unavailable after panel settle; cluster remains parked", c.gen);
        park_generation(c);
        return;
    }
    if !cycle.still_valid(gate::sink_cycle_snapshot(), gate::sink_wall_ms()) {
        cl_loge!("[GalCluster] handover generation {}: native sink witness changed before admission; cluster remains parked", c.gen);
        park_generation(c);
        return;
    }
    if !vfn::admit(c) {
        vfn::worker_cancel(c, "frame admission");
        return;
    }
    if !vfn::pin_if_current(c) {
        vfn::worker_cancel(c, "final pre-send check");
        return;
    }
    cl_log!("[GalCluster] screen-ready focus: cluster panel ready \u{2014} sending the unsolicited VideoFocusNotification(PROJECTED)");
    vfn_emit_pinned(c.ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
    vfn::unpin();
    if !vfn::clear(c.gen, c.ch) {
        cl_loge!(
            "[GalCluster] screen-ready focus: generation {} (ch={}) was superseded between the pinned send and the clear \u{2014} the notification went out but a newer generation's pending flag was left untouched",
            c.gen,
            c.ch
        );
    }
}

/// The loader's `VideoSink::setVideoFocus` hook: gal sent a
/// `VideoFocusNotification` for one of its sinks.
pub fn on_video_focus_notification(this: Ptr, mode: i32, unsolicited: i32) {
    let main = MAIN_VIDEO_SINK.load(Ordering::Acquire);
    if main == 0 || this as usize != main {
        return;
    }
    #[allow(clippy::cast_possible_wrap)]
    if mode != sdr::FOCUS_PROJECTED as i32 {
        MAIN_VFN_PROJECTED.store(false, Ordering::Release);
        return;
    }
    if MAIN_VFN_PROJECTED.swap(true, Ordering::AcqRel) {
        return;
    }
    cl_log!(
        "[GalCluster] gal sent VideoFocusNotification(PROJECTED) for the main-screen sink (sink={} unsolicited={}) \u{2014} main stream bringing up",
        CPtr::of(this),
        unsolicited
    );
}

// ---------------------------------------------------------------------------
// The fake endpoints
// ---------------------------------------------------------------------------

/// A static endpoint object (written before registerService publishes it;
/// afterwards gal's routing thread is the only writer, through the slots).
pub struct EpCell(UnsafeCell<FakeEndpoint>);
// SAFETY: see the type docs.
unsafe impl Sync for EpCell {}

pub static VIDEO_EP: EpCell = EpCell(UnsafeCell::new(FakeEndpoint::new(
    &raw const VIDEO_VTABLE,
    0,
    0,
)));
pub static INPUT_EP: EpCell = EpCell(UnsafeCell::new(FakeEndpoint::new(
    &raw const INPUT_VTABLE,
    0,
    0,
)));

fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    crate::guarded(site, fallback, f)
}

unsafe extern "C" fn ep_noop_dtor(_: Ptr) {}
unsafe extern "C" fn ep_noop_raw(_: Ptr, _: u8, _: Ptr) {}
unsafe extern "C" fn ep_noop_disc(_: Ptr, _: Ptr) {}

/// Channel-close endpoint slot2 returns whether the channel was open. The receiver
/// clears its channel map only on a nonzero return. The native base implementation at
/// 0xefe04 follows this rule. the map-clearing call site is 0xeccbc.
unsafe extern "C" fn ep_on_channel_closed(ep: Ptr, ch: u8) -> i32 {
    // SAFETY: gal passes our endpoint (or NULL).
    let was_open = !ep.is_null() && unsafe { *ep.cast::<u8>().add(IS_OPEN) } != 0;
    guarded("onChannelClosed", i32::from(was_open), || {
        cl_log!(
            "[GalCluster] onChannelClosed ch={} session_id={} total_frames={}",
            ch,
            SESSION_ID.load(Ordering::Relaxed),
            pipe::FRAME_COUNT.load(Ordering::Relaxed)
        );
        if !ep.is_null() {
            // SAFETY: our 64-byte endpoint.
            unsafe {
                *ep.cast::<u8>().add(IS_OPEN) = 0;
                *ep.cast::<u8>().add(OPEN_CHANNEL) = 0;
            }
        }
        SESSION_ID.store(0, Ordering::Relaxed);
        ACK_COUNTER.store(0, Ordering::Relaxed);
        STARTED.store(false, Ordering::Release);
        vfn::invalidate("channel closed");
        let mut most = lock(&MOST);
        pipe::most_close(&mut most);
        drop(most);
        i32::from(was_open)
    })
}

unsafe extern "C" fn ep_may_open_channel(_: Ptr, ch: u8) -> i32 {
    guarded("mayOpenChannel", 1, || {
        cl_log!(
            "[GalCluster] mayOpenChannel ch={} -> 1 (cluster_ch={} qout={} mr={})",
            ch,
            pipe::CLUSTER_CH.load(Ordering::Acquire),
            CPtr(QOUT.load(Ordering::Acquire)),
            CPtr(MR.load(Ordering::Acquire))
        );
        1
    })
}

unsafe extern "C" fn ep_on_channel_opened(ep: Ptr, ch: u8) {
    guarded("onChannelOpened", (), || {
        cl_log!("[GalCluster] onChannelOpened ch={} ep={}", ch, CPtr::of(ep));
        if !ep.is_null() {
            // SAFETY: our endpoint.
            unsafe {
                *ep.cast::<u8>().add(IS_OPEN) = 1;
                *ep.cast::<u8>().add(OPEN_CHANNEL) = ch;
            }
        }
    });
}

unsafe extern "C" fn ep_route_message(_: Ptr, ch: u8, t: u16, sptr: Ptr) -> i32 {
    guarded("routeMessage", 1, || {
        // SAFETY: gal's shared_ptr<IoBuffer> (or NULL).
        unsafe { route_message(ch, t, sptr.cast()) }
    })
}

unsafe extern "C" fn ep_inch_may_open(_: Ptr, ch: u8) -> i32 {
    guarded("input mayOpenChannel", 1, || {
        cl_log!("[GalCluster] input_ep mayOpenChannel ch={} -> 1", ch);
        1
    })
}

unsafe extern "C" fn ep_inch_on_opened(ep: Ptr, ch: u8) {
    guarded("input onChannelOpened", (), || {
        cl_log!("[GalCluster] input_ep onChannelOpened ch={}", ch);
        if !ep.is_null() {
            // SAFETY: our endpoint.
            unsafe {
                *ep.cast::<u8>().add(IS_OPEN) = 1;
                *ep.cast::<u8>().add(OPEN_CHANNEL) = ch;
            }
        }
    });
}

unsafe extern "C" fn ep_inch_route(_: Ptr, _ch: u8, _t: u16, _sptr: Ptr) -> i32 {
    guarded("input routeMessage", 1, || 1)
}

static VIDEO_VTABLE: Vtable = Vtable {
    dtor_complete: ep_noop_dtor,
    dtor_deleting: ep_noop_dtor,
    on_channel_closed: ep_on_channel_closed,
    may_open_channel: ep_may_open_channel,
    on_channel_opened: ep_on_channel_opened,
    route_message: ep_route_message,
    handle_raw_message: ep_noop_raw,
    add_discovery_info: ep_noop_disc,
};

/// The input endpoint uses the same channel-close implementation as the video endpoint.
static INPUT_VTABLE: Vtable = Vtable {
    dtor_complete: ep_noop_dtor,
    dtor_deleting: ep_noop_dtor,
    on_channel_closed: ep_on_channel_closed,
    may_open_channel: ep_inch_may_open,
    on_channel_opened: ep_inch_on_opened,
    route_message: ep_inch_route,
    handle_raw_message: ep_noop_raw,
    add_discovery_info: ep_noop_disc,
};

fn ep_setup(cell: &EpCell, vt: &'static Vtable, ch: u8, _tag: &str) -> Ptr {
    // SAFETY: before registerService publishes it, only this thread
    // touches the object (see EpCell).
    unsafe {
        *cell.0.get() = FakeEndpoint::new(vt, ch, 0);
        (*cell.0.get()).as_ptr()
    }
}

/// `note_outbound_only_inbound`: count and (rate-limited) log.
fn note_outbound_only_inbound(ch: u8, t: u16, name: &str, len: u32, f1: u32, f2: u32) {
    let mut most = lock(&MOST);
    let tm = now_ms();
    most.oob_inbound += 1;
    if most.oob_inbound == 1 || tm.wrapping_sub(most.oob_last_log_ms) >= 10_000 {
        most.oob_last_log_ms = tm;
        let n = most.oob_inbound;
        drop(most);
        cl_loge!(
            "[GalCluster] ch={} received unexpected inbound message 0x{:04x} {} \u{2014} expected outbound only; no action taken. len={} f1={} f2={} occurrences={}",
            ch,
            t,
            name,
            len,
            f1,
            f2,
            n
        );
    }
}

/// The setup request (`AA_AV_MSG_SETUP_REQUEST`).
fn on_setup_request(ch: u8, raw: Option<&[u8]>, rawlen: u32) {
    let codec_type = match raw {
        Some(r) if rawlen > 2 => sdr::setup_codec_type(&r[2..]),
        _ => 0,
    };
    lock(&MOST).setup_ms = now_ms();
    cl_log!(
        "[GalCluster] ch={} AVChannelSetupRequest codec_type={} len={} nav_ch={} ics_sent={}",
        ch,
        codec_type,
        rawlen,
        NAV_CH.load(Ordering::Acquire),
        i32::from(ICS_SENT.load(Ordering::Acquire))
    );
    send_av_setup_response(ch);
    if CFG.output() == 2 {
        pipe::most_close(&mut lock(&MOST));
        STARTED.store(false, Ordering::Release);
        pipe::wq_start();
        let fgen = vfn::begin(ch);
        vfn::clear(fgen, ch);
        send_video_focus_notification(ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
    } else if CFG.gate_enable() != 0 {
        let fgen = vfn::begin(ch);
        let c = Ctx { ch, gen: fgen };
        let rc = spawn_worker("cluster-ready", move || screen_ready(c));
        if let Err(e) = rc {
            cl_loge!(
                "[GalCluster] screen-ready focus: thread create failed ({}); cluster remains parked",
                crate::sys::strerror_str(e)
            );
            vfn::clear(fgen, ch);
        }
    } else {
        let fgen = vfn::begin(ch);
        vfn::clear(fgen, ch);
        send_video_focus_notification(ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
    }
    SETUP_DONE.store(true, Ordering::Release);
    let nav = NAV_CH.load(Ordering::Acquire);
    if nav == 0xFF {
        cl_loge!(
            "[GalCluster] ch={} AVChannelSetupRequest: nav_ch not yet known \u{2014} ICS deferred until nav opens",
            ch
        );
    } else {
        try_send_ics(nav);
    }
}

/// `ep_routeMessage`: the cluster video channel's messages.
///
/// # Safety
/// `sptr` is NULL or gal's `shared_ptr<IoBuffer>`, valid for the call.
#[allow(clippy::too_many_lines)]
unsafe fn route_message(ch: u8, t: u16, sptr: *const u8) -> i32 {
    // SAFETY: the caller's contract.
    let (raw, rawlen) = unsafe { video::iobuf(sptr, false) };
    let body = || raw.filter(|_| rawlen > 2).map(|r| &r[2..]);
    match t {
        sdr::AV_SETUP_REQUEST => on_setup_request(ch, raw, rawlen),
        sdr::AV_START_INDICATION => {
            SESSION_ID.store(0, Ordering::Relaxed);
            ACK_COUNTER.store(0, Ordering::Relaxed);
            let mut cfg_index = 0u32;
            if let Some(b) = body() {
                sdr::for_each_varint(b, |field, val| {
                    if field == 1 {
                        #[allow(clippy::cast_possible_wrap)]
                        SESSION_ID.store(val as i32, Ordering::Relaxed);
                    } else if field == 2 {
                        cfg_index = val;
                    }
                });
            } else if raw.is_none() {
                cl_loge!(
                    "[GalCluster] ch={} AVChannelStartIndication: buf_sptr={} but iobuf_data=NULL (len={})",
                    ch,
                    CPtr::of(sptr),
                    rawlen
                );
            }
            cl_log!(
                "[GalCluster] ch={} AVChannelStartIndication session_id={} cfg_index={} len={}",
                ch,
                SESSION_ID.load(Ordering::Relaxed),
                cfg_index,
                rawlen
            );
            STARTED.store(true, Ordering::Release);
            let expected_restart = pipe::FK_EXPECT_RESTART.swap(false, Ordering::AcqRel);
            let resumed = vfn::restart(ch, expected_restart);
            cl_log!("[GalCluster] ch={} restart admission: expected={} retained={} (current encoder-DOWN evidence still required)", ch, expected_restart, resumed);
            if expected_restart {
                cl_log!("[GalCluster] focus kick: the phone re-sent AVChannelStartIndication \u{2014} recovery complete, cluster context was held throughout");
            } else {
                // A start not caused by GAL begins a new stream session.
                crate::kick::gfx_rearm("the phone started a stream gal did not cause");
            }
        }
        sdr::AV_STOP_INDICATION => {
            if CFG.output() == 2 {
                STARTED.store(false, Ordering::Release);
            }
            cl_log!(
                "[GalCluster] ch={} AVChannelStopIndication session_id={} final_ack={} frames={}",
                ch,
                SESSION_ID.load(Ordering::Relaxed),
                ACK_COUNTER.load(Ordering::Relaxed),
                pipe::FRAME_COUNT.load(Ordering::Relaxed)
            );
            SESSION_ID.store(0, Ordering::Relaxed);
            ACK_COUNTER.store(0, Ordering::Relaxed);
            let retained = vfn::stop(ch, pipe::FK_EXPECT_RESTART.load(Ordering::Acquire));
            cl_log!("[GalCluster] ch={} stop admission: frames blocked, recovery permission retained={}", ch, retained);
            let mut most = lock(&MOST);
            pipe::most_close(&mut most);
        }
        sdr::AV_VIDEO_FOCUS_NOTIFICATION => {
            let (mut focus, mut unsol) = (0u32, 0u32);
            if let Some(b) = body() {
                sdr::for_each_varint(b, |field, val| {
                    if field == sdr::VFN_FIELD_FOCUS {
                        focus = val;
                    } else if field == sdr::VFN_FIELD_UNSOLICITED {
                        unsol = val;
                    }
                });
            }
            note_outbound_only_inbound(ch, t, "VideoFocusNotification", rawlen, focus, unsol);
        }
        sdr::AV_VIDEO_FOCUS_REQUEST => {
            let (mut mode, mut reason) = (sdr::FOCUS_PROJECTED, sdr::FOCUS_REASON_UNKNOWN);
            if let Some(b) = body() {
                sdr::for_each_varint(b, |field, val| {
                    if field == sdr::VFR_FIELD_MODE {
                        mode = val;
                    } else if field == sdr::VFR_FIELD_REASON {
                        reason = val;
                    }
                });
            }
            cl_loge!(
                "[GalCluster] ch={} VideoFocusRequestNotification mode={} reason={} len={}; replying with solicited VideoFocusNotification(mode={})",
                ch,
                mode,
                reason,
                rawlen,
                mode
            );
            send_video_focus_notification(ch, mode, sdr::VFN_SOLICITED);
        }
        sdr::AV_UPDATE_UI_CONFIG_OUT => {
            note_outbound_only_inbound(ch, t, "UpdateUiConfigRequest(HU->Phone id)", rawlen, 0, 0);
        }
        sdr::AV_UPDATE_UI_CONFIG_IN => {
            cl_log!(
                "[GalCluster] ch={} UpdateUiConfigRequest (0x800A, Phone\u{2192}HU) len={}",
                ch,
                rawlen
            );
            let msg = sdr::AV_UPDATE_UI_CONFIG_REPLY.to_be_bytes();
            if queue(ch, &msg).is_none() {
                cl_loge!(
                    "[GalCluster] ch={} UpdateUiConfig reply (0x800B): qout={} mr={} \u{2014} cannot send",
                    ch,
                    CPtr(QOUT.load(Ordering::Acquire)),
                    CPtr(MR.load(Ordering::Acquire))
                );
            }
        }
        sdr::AV_MEDIA_DATA => {
            if sptr.is_null() {
                cl_loge!(
                    "[GalCluster] ch={} DATA: buf_sptr is NULL \u{2014} dropping frame",
                    ch
                );
            } else {
                // SAFETY: the caller's contract.
                unsafe { video::handle_video_data(ch, sptr) };
            }
        }
        sdr::AV_CODEC_CONFIG => match body() {
            Some(b) => {
                let mut most = lock(&MOST);
                let n = crate::crop::store(&mut most.rt.codec_cfg, b);
                most.rt.codec_cfg_len = n;
                h264::check_codec_config(&most.rt.codec_cfg[..n]);
                pipe::TABLES_SENT.store(false, Ordering::Relaxed);
                cl_log!(
                    "[GalCluster] ch={} CodecConfig (SPS/PPS) cached len={}",
                    ch,
                    n
                );
            }
            None => cl_loge!(
                "[GalCluster] ch={} CodecConfig empty (len={}) \u{2014} ignored",
                ch,
                rawlen
            ),
        },
        _ => cl_log!(
            "[GalCluster] ch={} unhandled msg_type=0x{:04x} len={}",
            ch,
            t,
            rawlen
        ),
    }
    1
}

/// `register_cluster_endpoint`: reserve, then register, the video and
/// input endpoints (once per process).
fn register_cluster_endpoint() {
    let ch = pipe::CLUSTER_CH.load(Ordering::Acquire);
    let inch = CLUSTER_INCH.load(Ordering::Acquire);
    if EP_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    let (Some(reg), false) = (reg_svc(), mr().is_null()) else {
        cl_loge!(
            "[GalCluster] register_cluster_endpoint: mr={} reg={}",
            CPtr(MR.load(Ordering::Acquire)),
            CPtr(REG_SVC.load(Ordering::Acquire))
        );
        return;
    };
    if ch == 0xFF {
        cl_loge!("[GalCluster] register_cluster_endpoint: cluster_ch not set");
        return;
    }
    let vid = channel::reserve_exact(ch);
    if !vid {
        cl_loge!(
            "[GalCluster] cluster video ch={} REFUSED by the loader channel registry \u{2014} not registering the endpoint (cluster disabled for this session; another feature already holds that id)",
            ch
        );
        return;
    }
    let ep = ep_setup(&VIDEO_EP, &VIDEO_VTABLE, ch, "video.setup");
    cl_log!(
        "[GalCluster] registering video ep={} vtable={} ch={} mr={}",
        CPtr::of(ep),
        CPtr::of(&raw const VIDEO_VTABLE),
        ch,
        CPtr(MR.load(Ordering::Acquire))
    );
    // SAFETY: our endpoint object and its vtable.
    // SAFETY: gal's registerService with our 64-byte endpoint.
    let ok = unsafe { reg(mr(), ep) };
    EP_REGISTERED.store(true, Ordering::Release);
    if ok != 0 {
        cl_log!("[GalCluster] video endpoint registered ch={} ok", ch);
    } else {
        cl_loge!("[GalCluster] video endpoint register FAILED ch={}", ch);
    }
    if inch != 0xFF {
        register_input_endpoint(reg, inch);
    }
}

/// Register the input endpoint. Return false when the channel is refused. the caller
/// then returns without further registration.
fn register_input_endpoint(reg: RegisterService, inch: u8) -> bool {
    let inr = channel::reserve_exact(inch);
    if !inr {
        cl_loge!(
            "[GalCluster] cluster input ch={} REFUSED by the loader channel registry \u{2014} skipping the input endpoint (video unaffected)",
            inch
        );
        return false;
    }
    let iep = ep_setup(&INPUT_EP, &INPUT_VTABLE, inch, "input.setup");
    cl_log!(
        "[GalCluster] registering input ep={} ch={}",
        CPtr::of(iep),
        inch
    );
    // SAFETY: our endpoint object and its vtable.
    // SAFETY: gal's registerService with our 64-byte endpoint.
    let ink = unsafe { reg(mr(), iep) };
    if ink != 0 {
        cl_log!("[GalCluster] input endpoint registered ch={} ok", inch);
    } else {
        cl_loge!("[GalCluster] input endpoint register FAILED ch={}", inch);
    }
    true
}

// ---------------------------------------------------------------------------
// The SDR (VideoSink::addDiscoveryInfo)
// ---------------------------------------------------------------------------

type SdrDtor = unsafe extern "C" fn(Ptr);
type SdrMerge = unsafe extern "C" fn(Ptr, *const c_void);
type ParseArray = unsafe extern "C" fn(Ptr, *const c_void, i32) -> bool;
type SdrNew = unsafe extern "C" fn(*const c_void) -> Ptr;
type SdrDefault = unsafe extern "C" fn() -> Ptr;
type ByteSize = unsafe extern "C" fn(*const c_void) -> i32;
type Serialize = unsafe extern "C" fn(*const c_void, Ptr, i32) -> bool;

/// The protobuf calls of `init_fn_ptrs`.
#[derive(Clone, Copy)]
struct SdrFns {
    dtor: SdrDtor,
    merge: SdrMerge,
    parse: ParseArray,
    new: SdrNew,
    default: SdrDefault,
    byte_size: Option<ByteSize>,
    serialize: Option<Serialize>,
}

static SDR_FNS: std::sync::OnceLock<Option<SdrFns>> = std::sync::OnceLock::new();

/// Resolve a symbol and log the selected implementation.
fn resolve<F: Copy>(s: &gal_vendor_abi::Symbol<F>) -> Option<F> {
    let name = s.name().to_str().unwrap_or("?");
    let f = s.resolve();
    match f {
        Some(f) => {
            // SAFETY: F is a function pointer type (the symbol table).
            let a: usize = unsafe { core::mem::transmute_copy(&f) };
            cl_log!("[GalCluster] dlsym ok: {} -> {}", name, CPtr(a));
        }
        None => cl_loge!("[GalCluster] dlsym FAILED: {}", name),
    }
    f
}

/// `init_fn_ptrs`: resolved once. None when a critical one is missing.
fn sdr_fns() -> Option<SdrFns> {
    *SDR_FNS.get_or_init(|| {
        cl_log!("[GalCluster] init_fn_ptrs: resolving protobuf symbols");
        let dtor = resolve(&sym::SDR_DELETING_DTOR);
        let merge = resolve(&sym::SDR_MERGE_FROM);
        let parse = resolve(&sym::MSG_PARSE_FROM_ARRAY);
        let new = resolve(&sym::SDR_NEW);
        let default = resolve(&sym::SDR_DEFAULT_INSTANCE);
        let byte_size = resolve(&sym::SDR_BYTE_SIZE);
        let serialize = resolve(&sym::MSG_SERIALIZE_TO_ARRAY);
        let (Some(dtor), Some(merge), Some(parse), Some(new), Some(default)) =
            (dtor, merge, parse, new, default)
        else {
            cl_loge!("[GalCluster] init FAILED \u{2014} cluster inject disabled");
            return None;
        };
        cl_log!(
            "[GalCluster] init ok (byte_size={} serialize={})",
            CPtr(byte_size.map_or(0, |f| f as usize)),
            CPtr(serialize.map_or(0, |f| f as usize))
        );
        Some(SdrFns {
            dtor,
            merge,
            parse,
            new,
            default,
            byte_size,
            serialize,
        })
    })
}

/// `alloc_temp_sdr`: a fresh empty SDR via `default_instance()->New()`.
fn alloc_temp_sdr(f: &SdrFns) -> Option<Ptr> {
    // SAFETY: protobuf-lite calls with gal's signatures.
    let def = unsafe { (f.default)() };
    if def.is_null() {
        cl_loge!("[GalCluster] alloc_temp_sdr: default_instance returned NULL");
        return None;
    }
    // SAFETY: as above.
    let obj = unsafe { (f.new)(def) };
    if obj.is_null() {
        cl_loge!("[GalCluster] alloc_temp_sdr: New() returned NULL");
        return None;
    }
    Some(obj)
}

/// The 32 KB SDR scratch (`scan_buf`, `ex_buf`, `patch_buf`): static, not
/// on gal's stack.
static SDR_BUF: Mutex<[u8; 32768]> = Mutex::new([0; 32768]);

/// `ByteSize(resp)` (one call), None without the symbol.
fn byte_size(f: &SdrFns, resp: Ptr) -> Option<i32> {
    // SAFETY: protobuf-lite call on gal's SDR.
    f.byte_size.map(|bs| unsafe { bs(resp) })
}

/// `SerializeToArray(resp, buf, n)` (one call) for a size already checked
/// to be in 1..=32768: the length, or None.
fn serialize_n(f: &SdrFns, resp: Ptr, n: i32, buf: &mut [u8; 32768]) -> Option<usize> {
    let ser = f.serialize?;
    // SAFETY: a 32 KB buffer and n <= 32768.
    if unsafe { ser(resp, buf.as_mut_ptr().cast(), n) } {
        usize::try_from(n).ok()
    } else {
        None
    }
}

/// One `ByteSize` + `SerializeToArray` when the size is in range.
fn serialize(f: &SdrFns, resp: Ptr, buf: &mut [u8; 32768]) -> Option<usize> {
    f.serialize?;
    let n = byte_size(f, resp)?;
    if n <= 0 || n > 32768 {
        return None;
    }
    serialize_n(f, resp, n, buf)
}

/// The unit's cluster layout from the conf.
pub(crate) fn layout() -> Layout {
    let c = &CFG;
    Layout {
        codec_res: as_u32(c.codec_res()),
        width_margin: as_u32(c.width_margin()),
        height_margin: as_u32(c.height_margin()),
        margin_top: c.margin_top(),
        margin_bottom: c.margin_bottom(),
        margin_left: c.margin_left(),
        margin_right: c.margin_right(),
        content_insets: c.content_insets(),
        stable_insets: c.stable_insets(),
        viewing_dist: as_u32(c.viewing_dist()),
    }
}

/// The display-id scan and the existing-SDR dump.
fn scan_existing(f: &SdrFns, resp: Ptr) -> (u32, u32) {
    let (mut max_display_id, mut cluster_display_id) = (0u32, 1u32);
    let mut buf = lock(&SDR_BUF);
    if let (Some(_), Some(_)) = (f.byte_size, f.serialize) {
        let resp_len = byte_size(f, resp).unwrap_or(0);
        cl_log!("[GalCluster] existing SDR size={} bytes", resp_len);
        if resp_len > 0 && resp_len <= 32768 {
            match serialize_n(f, resp, resp_len, &mut buf) {
                Some(n) => {
                    max_display_id = aa_wire_patch::sdr_density::max_display_id(&buf[..n]);
                    cluster_display_id = max_display_id.wrapping_add(1);
                }
                None => cl_loge!("[GalCluster] SDR serialize failed \u{2014} using display_id=1"),
            }
        } else {
            cl_loge!(
                "[GalCluster] SDR size={} out of range \u{2014} using display_id=1",
                resp_len
            );
        }
    } else {
        cl_loge!(
            "[GalCluster] serialize symbols missing (byte_size={} serialize={}) \u{2014} display_id=1",
            CPtr(f.byte_size.map_or(0, |x| x as usize)),
            CPtr(f.serialize.map_or(0, |x| x as usize))
        );
    }
    cl_log!(
        "[GalCluster] display_id: max_existing={} cluster={}",
        max_display_id,
        cluster_display_id
    );
    if let Some(n) = serialize(f, resp, &mut buf) {
        let shown = n.min(4096);
        cl_log!("[GalCluster] existing SDR total={} bytes", n);
        let mut chunk = 0usize;
        while chunk < shown {
            let clen = (shown - chunk).min(48);
            cl_log!(
                "[GalCluster] existing_sdr[{}..{}]: {}{}",
                chunk,
                chunk + clen - 1,
                HexSp(&buf[chunk..chunk + clen]),
                if n > chunk + clen { "..." } else { "" }
            );
            chunk += 48;
        }
    }
    (max_display_id, cluster_display_id)
}

/// The MAIN display's density and frame-rate patch, validated into a
/// throwaway message before it reaches `resp`.
fn patch_main(f: &SdrFns, resp: Ptr) {
    let c = &CFG;
    let main_dpi = as_u32(c.main_dpi());
    let want_fps30 = as_u32(c.main_fps()) == 30;
    if !((main_dpi > 0 || want_fps30) && f.byte_size.is_some() && f.serialize.is_some()) {
        return;
    }
    let mut buf = lock(&SDR_BUF);
    let Some(plen) = serialize(f, resp, &mut buf) else {
        return;
    };
    let patch = &mut buf[..plen];
    let mut n = 0u32;
    if main_dpi > 0 {
        let r = aa_wire_patch::sdr_density::patch_main_density(patch, main_dpi);
        n = r.patched;
        let refused = r.refused;
        if refused > 0 {
            cl_loge!(
                "[GalCluster] main_dpi={}: {} density field(s) REFUSED - the new value is a different varint length and patching in place would invalidate the enclosing lengths",
                main_dpi,
                refused
            );
        }
        if n == 0 && refused == 0 {
            cl_log!(
                "[GalCluster] main_dpi={}: nothing to patch (already set, or no main-display VideoConfiguration found)",
                main_dpi
            );
        }
    }
    let mut nf = 0u32;
    if want_fps30 {
        let r = aa_wire_patch::sdr_density::patch_main_fps30(patch);
        nf = r.patched;
        cl_log!(
            "[GalCluster] main_fps=30: {} main VideoConfiguration(s) 60->30 fps, {} refused",
            nf,
            r.refused
        );
    }
    if n == 0 && nf == 0 {
        return;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let plen_i = plen as i32;
    let vtmp = alloc_temp_sdr(f);
    // SAFETY: ParseFromArray receives a fresh validation message or the receiver
    // response and the patched buffer. The validation object remains allocated.
    let validated = vtmp.is_some_and(|v| unsafe { (f.parse)(v, patch.as_ptr().cast(), plen_i) });
    if validated {
        // SAFETY: as above.
        if unsafe { (f.parse)(resp, patch.as_ptr().cast(), plen_i) } {
            if n > 0 {
                cl_log!(
                    "[GalCluster] main_dpi: rewrote {} density field(s) -> {} (main display now {}p wide; AA widescreen breakpoint is 1240dp)",
                    n,
                    main_dpi,
                    1280 * 160 / main_dpi
                );
            }
        } else {
            cl_loge!("[GalCluster] main SDR patch: re-parse into resp FAILED after it validated - SDR may be damaged");
        }
    } else {
        cl_loge!("[GalCluster] main SDR patch: patched bytes do NOT parse - leaving the stock SDR untouched");
    }
}

/// `cluster_on_video_discovery`.
#[allow(clippy::too_many_lines)]
fn on_video_discovery(this: Ptr, resp: Ptr) {
    if this.is_null() || resp.is_null() {
        cl_loge!(
            "[GalCluster] addDiscoveryInfo: NULL self={} or resp={} \u{2014} skipping",
            CPtr::of(this),
            CPtr::of(resp)
        );
        return;
    }
    // SAFETY: gal's VideoSink. its channel id byte (gal-vendor-abi).
    let head_ch = unsafe { *this.cast::<u8>().add(off::VIDEO_SINK_CHANNEL) };
    cl_log!(
        "[GalCluster] addDiscoveryInfo self={} head_ch={} resp={}",
        CPtr::of(this),
        head_ch,
        CPtr::of(resp)
    );
    MAIN_VFN_PROJECTED.store(false, Ordering::Release);
    MAIN_VIDEO_SINK.store(this as usize, Ordering::Release);
    let Some(f) = sdr_fns() else {
        cl_loge!(
            "[GalCluster] addDiscoveryInfo: init_fn_ptrs failed \u{2014} cluster inject disabled"
        );
        return;
    };
    // The discovery patcher is linked into this library.
    let free_from = |start: u32| {
        (start..255)
            .find(|&i| !CH_TAKEN[i as usize].load(Ordering::Acquire))
            .map(|i| u8::try_from(i).unwrap_or(0xFF))
    };
    let Some(cluster_ch) = free_from(u32::from(head_ch) + 1) else {
        cl_loge!(
            "[GalCluster] no free channel_id > head_ch={} found (all taken) \u{2014} inject skipped",
            head_ch
        );
        return;
    };
    cl_log!(
        "[GalCluster] chose cluster_ch={} (head_ch={}, slot={})",
        cluster_ch,
        head_ch,
        (u32::from(cluster_ch) + 64) * 4
    );
    match free_from(u32::from(cluster_ch) + 1) {
        None => {
            cl_loge!(
                "[GalCluster] no free channel for cluster input \u{2014} inject without input"
            );
        }
        Some(i) => {
            cl_log!("[GalCluster] chose cluster_inch={}", i);
            CLUSTER_INCH.store(i, Ordering::Release);
        }
    }
    let (_, display_id) = scan_existing(&f, resp);
    patch_main(&f, resp);

    let c = &CFG;
    let mut sdr_bytes = [0u8; 512];
    let density = as_u32(c.density());
    let n = sdr::cluster_video_sdr(
        &mut sdr_bytes,
        &layout(),
        u32::from(cluster_ch),
        display_id,
        density,
        10000,
    );
    let Some(tmp) = alloc_temp_sdr(&f) else {
        cl_loge!("[GalCluster] alloc_temp_sdr failed (video)");
        return;
    };
    cl_log!(
        "[GalCluster] built cluster video SDR bytes: {} (ch={} display_id={} codec={}x{} dpi={} par=10000 margins={}x{})",
        n,
        cluster_ch,
        display_id,
        sdr::codec_size(as_u32(c.codec_res())).0,
        sdr::codec_size(as_u32(c.codec_res())).1,
        density,
        as_u32(c.width_margin()),
        as_u32(c.height_margin())
    );
    let head = n.min(48);
    cl_log!(
        "[GalCluster] video_sdr[0..{}]: {}{}",
        head.wrapping_sub(1),
        HexSp(&sdr_bytes[..head]),
        if n > 48 { "..." } else { "" }
    );
    if n > 48 {
        let tail = (n - 48).min(48);
        cl_log!(
            "[GalCluster] video_sdr[48..{}]: {}{}",
            48 + tail - 1,
            HexSp(&sdr_bytes[48..48 + tail]),
            if n > 96 { "..." } else { "" }
        );
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    // SAFETY: protobuf-lite calls on our temporary and gal's SDR.
    let ok = unsafe { (f.parse)(tmp, sdr_bytes.as_ptr().cast(), n as i32) };
    if ok {
        // SAFETY: as above.
        unsafe { (f.merge)(resp, tmp) };
        pipe::CLUSTER_CH.store(cluster_ch, Ordering::Release);
        cl_log!(
            "[GalCluster] cluster video ch={} injected into SDR OK",
            cluster_ch
        );
    } else {
        cl_loge!(
            "[GalCluster] video parse_array FAILED len={} \u{2014} cluster NOT injected",
            n
        );
    }
    // SAFETY: the temporary from New().
    unsafe { (f.dtor)(tmp) };
    if !ok {
        return;
    }
    let inch = CLUSTER_INCH.load(Ordering::Acquire);
    if inch != 0xFF {
        let mut in_sdr = [0u8; 64];
        let in_len = sdr::cluster_input_sdr(
            &mut in_sdr,
            display_id,
            sdr::codec_size(as_u32(c.codec_res())),
        );
        if in_len >= 4 && in_sdr[2] == 0x08 {
            in_sdr[3] = inch;
        } else {
            cl_loge!("[GalCluster] input SDR layout unexpected \u{2014} channel_id patch may fail");
        }
        match alloc_temp_sdr(&f) {
            None => cl_loge!("[GalCluster] alloc_temp_sdr failed (input)"),
            Some(in_tmp) => {
                cl_log!(
                    "[GalCluster] input_sdr ({} bytes, ch={} display_id={}): {}",
                    in_len,
                    inch,
                    display_id,
                    HexSp(&in_sdr[..in_len.min(48)])
                );
                #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                // SAFETY: protobuf-lite calls.
                let in_ok = unsafe { (f.parse)(in_tmp, in_sdr.as_ptr().cast(), in_len as i32) };
                if in_ok {
                    // SAFETY: as above.
                    unsafe { (f.merge)(resp, in_tmp) };
                    cl_log!(
                        "[GalCluster] cluster input ch={} display_id={} injected OK",
                        inch,
                        display_id
                    );
                } else {
                    cl_loge!("[GalCluster] input parse_array FAILED");
                }
                // SAFETY: the temporary from New().
                unsafe { (f.dtor)(in_tmp) };
            }
        }
    }
    register_cluster_endpoint();
}

// ---------------------------------------------------------------------------
// The other hooks
// ---------------------------------------------------------------------------

/// `cluster_on_register_service`: capture the router. note gal's ids.
fn on_register_service(this: Ptr, endpoint: Ptr) {
    if endpoint.is_null() {
        cl_loge!("[GalCluster] registerService: NULL endpoint");
        return;
    }
    // SAFETY: gal's ProtocolEndpointBase: the service index byte.
    let ch = unsafe {
        *endpoint
            .cast::<u8>()
            .add(gal_vendor_abi::endpoint::SERVICE_INDEX)
    };
    if ch != 0xFF {
        CH_TAKEN[usize::from(ch)].store(true, Ordering::Release);
    }
    cl_log!(
        "[GalCluster] registerService ep={} ch={}{}",
        CPtr::of(endpoint),
        ch,
        if ch == 0xFF {
            " (broadcast/invalid)"
        } else {
            ""
        }
    );
    if mr().is_null() {
        MR.store(this as usize, Ordering::Release);
        phase2_resolve();
        cl_log!(
            "[GalCluster] captured g_mr={} qout={} reg={}",
            CPtr::of(this),
            CPtr(QOUT.load(Ordering::Acquire)),
            CPtr(REG_SVC.load(Ordering::Acquire))
        );
    }
}

/// `cluster_on_channel_opened`: log every open. find the nav channel and
/// send `InstrumentClusterStart`.
fn on_channel_opened(_mr: Ptr, ch: u8, req: Ptr) {
    let cluster = pipe::CLUSTER_CH.load(Ordering::Acquire);
    let inch = CLUSTER_INCH.load(Ordering::Acquire);
    let ch_id = if req.is_null() {
        0xFF
    } else {
        // SAFETY: gal's ChannelOpenRequest (gal-vendor-abi).
        unsafe { *req.cast::<u8>().add(off::CHANNEL_OPEN_REQUEST_SERVICE) }
    };
    let is_video = ch == cluster && cluster != 0xFF;
    let is_input = ch == inch && inch != 0xFF;
    cl_loge!(
        "[GalCluster] chOpen wire_ch={} ch_id={}{}",
        ch,
        ch_id,
        if is_video {
            " ***CLUSTER-VIDEO***"
        } else if is_input {
            " (cluster-input)"
        } else {
            ""
        }
    );
    let is_nav = NAV_CH.load(Ordering::Acquire) == 0xFF && (ch_id == 10 || ch == 10);
    if is_nav {
        NAV_CH.store(ch, Ordering::Release);
        cl_loge!(
            "[GalCluster] wire_ch={} ch_id={} identified as nav channel \u{2014} sending ICS",
            ch,
            ch_id
        );
        try_send_ics(ch);
        if SETUP_DONE.load(Ordering::Acquire) && !STARTED.load(Ordering::Acquire) && cluster != 0xFF
        {
            if vfn::armed()
                || (CFG.output() != 2 && CFG.gate_enable() != 0 && !vfn::ready_for(cluster))
            {
                cl_log!(
                    "[GalCluster] ICS: an unsolicited VideoFocusNotification(PROJECTED) is already pending for cluster_ch={} \u{2014} not re-sending it here",
                    cluster
                );
            } else {
                cl_loge!(
                    "[GalCluster] resending unsolicited VideoFocusNotification(PROJECTED) on cluster_ch={} after ICS (routing now active)",
                    cluster
                );
                send_video_focus_notification(cluster, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
            }
        }
    }
}

/// `cluster_on_unexpected_msg`.
fn on_unexpected_msg(_mr: Ptr, ch: u8) {
    let cluster = pipe::CLUSTER_CH.load(Ordering::Acquire);
    if ch == cluster && cluster != 0xFF {
        cl_loge!(
            "[GalCluster] sendUnexpectedMsg ch={} *** OUR CLUSTER CHANNEL \u{2014} missed message?",
            ch
        );
    } else {
        cl_log!("[GalCluster] sendUnexpectedMsg ch={}", ch);
    }
}

/// `cluster_on_transport_read`: gal's USB reader thread. A failed read on a
/// real request is the disconnect: post-mortem, then give nav back. No
/// allocation here (fixed log buffers only). Output shutdown uses the existing
/// writer drain/join. a blocked write must return before native restoration.
fn on_transport_read(_this: Ptr, _buf: *const c_void, len: u32, ret: i32) {
    if ret <= 0 && len > 0 {
        MAIN_VFN_PROJECTED.store(false, Ordering::Release);
        vfn::invalidate("AA transport read ended");
    }
    if ret <= 0 && len > 0 && CFG.output() == 2 {
        STARTED.store(false, Ordering::Release);
        pipe::most_close(&mut lock(&MOST));
        return;
    }
    if ret <= 0 && len > 0 {
        let t = now_ms();
        if pipe::WRITE_INFLIGHT.load(Ordering::Acquire) {
            let enter = pipe::WRITE_ENTER_MS.load(Ordering::Relaxed);
            cl_loge!(
                "[GalCluster] Teardown: write still in flight \u{2014} entered {}ms ago. The writer has not returned; the breaker evaluates only after a write returns.",
                log::ul(if enter != 0 { t.wrapping_sub(enter) } else { 0 })
            );
        } else {
            let exit = pipe::WRITE_EXIT_MS.load(Ordering::Relaxed);
            let arr = video::LAST_ARRIVAL_MS.load(Ordering::Relaxed);
            cl_log!(
                "[GalCluster] Teardown: no write in flight; last write returned {}ms ago, last frame ARRIVED {}ms ago \u{2014} no new frame received.",
                log::ul(if exit != 0 { t.wrapping_sub(exit) } else { 0 }),
                log::ul(if arr != 0 { t.wrapping_sub(arr) } else { 0 })
            );
        }
    }
    if ret <= 0 && len > 0 {
        STARTED.store(false, Ordering::Release);
        pipe::FK_EXPECT_RESTART.store(false, Ordering::Release);
        crate::kick::gfx_rearm("AA transport closed");
        cl_loge!("[GalCluster] AA transport closed: stopping owned output before native restore");
        // MOST excludes routing writes. most_close joins the writer, flushes
        // and closes our descriptor before it requests the native context.
        // Do this even when HIDDEN is false: an owned output must still close.
        pipe::most_close(&mut lock(&MOST));
    }
}

// ---------------------------------------------------------------------------
// Guarded hook entry points (the loader calls these on gal's threads)
// ---------------------------------------------------------------------------

pub fn hook_register_service(this: Ptr, ep: Ptr) {
    guarded("registerService hook", (), || on_register_service(this, ep));
}

pub fn hook_video_discovery(this: Ptr, resp: Ptr) {
    guarded("video discovery hook", (), || {
        on_video_discovery(this, resp);
    });
}

pub fn hook_channel_opened(mr: Ptr, ch: u8, req: Ptr) {
    guarded("channel opened hook", (), || on_channel_opened(mr, ch, req));
}

pub fn hook_unexpected_msg(mr: Ptr, ch: u8) {
    guarded("unexpected message hook", (), || on_unexpected_msg(mr, ch));
}

pub fn hook_transport_read(this: Ptr, buf: *const c_void, len: u32, ret: i32) {
    guarded("transport read hook", (), || {
        on_transport_read(this, buf, len, ret);
    });
}

pub fn hook_video_focus_grant(this: Ptr, mode: i32, unsolicited: i32) {
    guarded("video focus notification hook", (), || {
        on_video_focus_notification(this, mode, unsolicited);
    });
}

/// `s_started`, for the UI sink.
pub fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn test_channel_message(ch: u8, message: u16) {
    // SAFETY: route_message accepts a null buffer for the message body.
    unsafe { route_message(ch, message, core::ptr::null()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vtables_put_the_input_close_on_the_video_close() {
        assert_eq!(
            VIDEO_VTABLE.on_channel_closed as usize,
            INPUT_VTABLE.on_channel_closed as usize
        );
    }
}
