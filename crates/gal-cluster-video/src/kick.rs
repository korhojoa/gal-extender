// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Bounded keyframe requests after cluster sink or HMI changes.
//!
//! A stream can start before the HMI finishes its own cluster fade-in. Its
//! initial keyframe can then be missed. While a stream is active, a fresh
//! keyframe can be requested by a native-to-projected focus transition.
//!
//! Triggers are a `gfx_state` rising edge and a later HMI handover timestamp.
//! Requests have a minimum interval. Repeated sink edges increase the wait
//! with exponential backoff, which bounds a restart/resynchronization loop.
//! A new independent stream, transport close, or quiet interval resets it.
//! Every request or suppression records its trigger and monotonic time.
//! The routing thread calls this path while the MOST lock is held.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::gate;
use crate::log::{cl_log, cl_loge, now_ms};
use crate::{pipe, route, sdr};

/// `CLUSTER_IDR_KICK_MIN_MS`.
pub const MIN_MS: u64 = 2000;
/// `s_kick_last_ms`: `now_ms()` of the last kick.
static LAST_MS: AtomicU64 = AtomicU64::new(0);
/// `s_stream_up_ms`: the uptime (ms, a 32-bit `unsigned long`) of this
/// session's first frame.
pub static STREAM_UP_MS: AtomicU32 = AtomicU32::new(0);
/// `s_hmi_kick_read_ms`: the 4 Hz throttle of the object read.
static HMI_READ_MS: AtomicU64 = AtomicU64::new(0);
/// `s_hmi_kick_done`: the deadline handled (+1), 0 none.
static HMI_DONE: AtomicU32 = AtomicU32::new(0);

/// Uptime in milliseconds, narrowed to an unsigned 32-bit value.
#[allow(clippy::cast_possible_truncation)] // the low 32 bits of the timer value
pub fn uptime_ms32() -> u32 {
    (crate::log::mono_us() / 1000) as u32
}

// Each gfx_state transition from zero to one requests a kick when its backoff permits.
// Otherwise the kick waits for a later routing pass. The delay doubles from
// GFX_BACKOFF_MIN_MS to GFX_BACKOFF_MAX_MS. A new session or GFX_QUIET_MS without an
// edge resets the delay.

/// The first backoff wait.
pub const GFX_BACKOFF_MIN_MS: u32 = 2_000;
/// The longest backoff wait.
pub const GFX_BACKOFF_MAX_MS: u32 = 60_000;
/// No 0->1 for this long resets the backoff.
pub const GFX_QUIET_MS: u32 = 120_000;

/// `gfx_state` kicks since the backoff last reset.
static GFX_KICKS: AtomicU32 = AtomicU32::new(0);
/// The uptime (ms) of the last `gfx_state` kick. 0 none.
static GFX_UP_MS: AtomicU32 = AtomicU32::new(0);
/// The uptime (ms) of the last 0->1 edge. 0 none.
static GFX_EDGE_MS: AtomicU32 = AtomicU32::new(0);
/// A 0->1 is waiting for the backoff to end.
static GFX_PENDING: AtomicBool = AtomicBool::new(false);

/// The backoff wait after `kicks` kicks: 0 for none, then MIN doubling to MAX.
pub fn gfx_backoff_ms(kicks: u32) -> u32 {
    if kicks == 0 {
        return 0;
    }
    let shift = (kicks - 1).min(16);
    GFX_BACKOFF_MIN_MS
        .saturating_mul(1 << shift)
        .min(GFX_BACKOFF_MAX_MS)
}

/// `cluster_gfx_kick_rearm`: a new stream session resets the backoff. `why`
/// names the event. logged only when the backoff was in use.
pub fn gfx_rearm(why: &str) {
    let kicks = GFX_KICKS.swap(0, Ordering::Relaxed);
    GFX_PENDING.store(false, Ordering::Relaxed);
    if kicks == 0 {
        return;
    }
    cl_log!(
        "[GalCluster] IDR kick: gfx_state backoff reset at uptime {}ms ({}; {} kick(s), the last at uptime {}ms)",
        uptime_ms32(),
        why,
        kicks,
        GFX_UP_MS.load(Ordering::Relaxed)
    );
}

/// A 0->1 edge: reset the backoff after a quiet spell, then kick or hold.
fn gfx_edge(ch: u8) {
    let up = uptime_ms32();
    let prev = GFX_EDGE_MS.swap(up, Ordering::Relaxed);
    if prev != 0 && up.wrapping_sub(prev) >= GFX_QUIET_MS {
        gfx_rearm("no gfx_state 0->1 for the quiet time");
    }
    GFX_PENDING.store(true, Ordering::Relaxed);
    gfx_try(ch, true);
}

/// Whether the retry deadline has elapsed, using wrapping 32-bit uptime.
fn gfx_due(kicks: u32, last: u32, now: u32) -> bool {
    kicks == 0 || now.wrapping_sub(last) >= gfx_backoff_ms(kicks)
}

/// Send the held kick when the backoff allows it. `edge`: called for a new
/// 0->1 (a hold is logged once, at the edge).
fn gfx_try(ch: u8, edge: bool) {
    if !GFX_PENDING.load(Ordering::Relaxed) {
        return;
    }
    let up = uptime_ms32();
    let kicks = GFX_KICKS.load(Ordering::Relaxed);
    let wait = gfx_backoff_ms(kicks);
    let last = GFX_UP_MS.load(Ordering::Relaxed);
    if !gfx_due(kicks, last, up) {
        if edge {
            cl_loge!(
                "[GalCluster] IDR kick HELD: trigger=gfx_state 0->1 at uptime {}ms, backoff {}ms after kick {} (at uptime {}ms); eligible at uptime {}ms",
                up,
                wait,
                kicks,
                last,
                last.wrapping_add(wait)
            );
        }
        return;
    }
    if idr_kick(ch, "gfx_state 0->1") {
        GFX_PENDING.store(false, Ordering::Relaxed);
        GFX_KICKS.store(kicks.saturating_add(1), Ordering::Relaxed);
        GFX_UP_MS.store(up, Ordering::Relaxed);
        cl_log!(
            "[GalCluster] IDR kick: gfx_state kick {} at uptime {}ms; the next waits {}ms",
            kicks.saturating_add(1),
            up,
            gfx_backoff_ms(kicks.saturating_add(1))
        );
    }
}

/// `cluster_idr_kick`. True when the kick went out, false when the spacing
/// suppressed it.
pub fn idr_kick(ch: u8, trigger: &str) -> bool {
    let nowk = now_ms();
    let up = uptime_ms32();
    let last = LAST_MS.load(Ordering::Relaxed);
    if last != 0 && nowk.wrapping_sub(last) < MIN_MS {
        cl_loge!(
            "[GalCluster] IDR kick SUPPRESSED: trigger={} at uptime {}ms, {}ms after the last kick (at most one per {}ms)",
            trigger,
            up,
            crate::log::ul(nowk.wrapping_sub(last)),
            MIN_MS
        );
        return false;
    }
    LAST_MS.store(nowk, Ordering::Relaxed);
    pipe::FK_EXPECT_RESTART.store(true, Ordering::Release);
    route::send_video_focus_notification(ch, sdr::FOCUS_NATIVE, sdr::VFN_UNSOLICITED);
    route::send_video_focus_notification(ch, sdr::FOCUS_PROJECTED, sdr::VFN_UNSOLICITED);
    cl_loge!(
        "[GalCluster] IDR kick: trigger={} at uptime {}ms -- toggled focus NATIVE->PROJECTED back to back; expect a fresh CodecConfig + IDR, cluster context held",
        trigger,
        up
    );
    true
}

/// `cluster_hmi_kick_check`: the `hmi_ctx` trigger (one read of the
/// handover object, 4 Hz).
pub fn hmi_kick_check(ch: u8) {
    let t = now_ms();
    let last = HMI_READ_MS.load(Ordering::Relaxed);
    if last != 0 && t.wrapping_sub(last) < 250 {
        return;
    }
    HMI_READ_MS.store(t, Ordering::Relaxed);
    let Some(h) = gate::read_handover() else {
        return;
    };
    if !h.have_ctx {
        return;
    }
    let up = uptime_ms32();
    #[allow(clippy::cast_possible_truncation)] // the timer value wraps at 32 bits
    let base = gate::base_or_seen(h.have_ms, h.ms, &gate::HANDOVER_SEEN_MS, u64::from(up)) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let deadline = base.wrapping_add(gate::HANDOVER_FADE_MS as u32);
    if HMI_DONE.load(Ordering::Relaxed) == deadline.wrapping_add(1) {
        return;
    }
    let stream_up = STREAM_UP_MS.load(Ordering::Relaxed);
    if deadline <= stream_up {
        HMI_DONE.store(deadline.wrapping_add(1), Ordering::Relaxed);
        cl_log!(
            "[GalCluster] IDR kick not needed: hmi_ctx at {}ms + {}ms fadeIn is before this stream's first frame (uptime {}ms)",
            base,
            gate::HANDOVER_FADE_MS,
            stream_up
        );
        return;
    }
    if up < deadline {
        return;
    }
    HMI_DONE.store(deadline.wrapping_add(1), Ordering::Relaxed);
    let _ = idr_kick(ch, "hmi_ctx + fadeIn");
}

/// The routing hook runs with the MOST lock held. Both triggers require an active GAL
/// stream. An edge outside streaming is dropped and logged. `gfx_edge` applies backoff;
/// `gfx_try` sends a deferred kick on a later pass.
pub fn check(ch: u8, streaming: bool) {
    if streaming {
        if crate::cfg::CFG.sink_feedback() != 0 {
            if gate::SINK_GFX_RECOVERED.swap(false, Ordering::Relaxed) {
                gfx_edge(ch);
            } else {
                gfx_try(ch, false);
            }
        }
        hmi_kick_check(ch);
    } else {
        if crate::cfg::CFG.sink_feedback() != 0 && gate::SINK_GFX_RECOVERED.load(Ordering::Relaxed)
        {
            cl_loge!(
                "[GalCluster] IDR kick SUPPRESSED: trigger=gfx_state 0->1 at uptime {}ms, gal is not streaming",
                uptime_ms32()
            );
        }
        gate::SINK_GFX_RECOVERED.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_2s_to_60s() {
        let waits: Vec<u32> = (0..9).map(gfx_backoff_ms).collect();
        assert_eq!(
            waits,
            [0, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000]
        );
        assert_eq!(gfx_backoff_ms(u32::MAX), GFX_BACKOFF_MAX_MS);
    }

    #[test]
    fn later_resync_is_eligible_without_rearming_the_session() {
        assert!(gfx_due(1, 1_000, 30_000));
        assert!(gfx_due(2, 30_000, 62_000));
        assert!(gfx_due(3, 62_000, 123_000));
        assert!(!gfx_due(6, 123_000, 182_999));
        assert!(gfx_due(6, 123_000, 183_000));
    }

    #[test]
    fn held_resync_obeys_deadline_and_uptime_wrap() {
        assert!(gfx_due(0, 0, 0));
        assert!(!gfx_due(2, 10_000, 13_999));
        assert!(gfx_due(2, 10_000, 14_000));
        let last = u32::MAX - 999;
        assert!(!gfx_due(1, last, 999));
        assert!(gfx_due(1, last, 1_000));
    }

    #[test]
    fn a_new_session_clears_held_resync_and_retry_count() {
        GFX_KICKS.store(4, Ordering::Relaxed);
        GFX_PENDING.store(true, Ordering::Relaxed);
        gfx_rearm("host regression new session");
        assert_eq!(GFX_KICKS.load(Ordering::Relaxed), 0);
        assert!(!GFX_PENDING.load(Ordering::Relaxed));
    }
}
