// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Owned encoder observation and the cluster MOST-sink feedback. Encoder
//! records come from one shared reader through a live identity/generation PPS
//! contract. Missing or unhealthy current evidence holds writes off.

use core::ffi::{c_int, CStr};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use gal_feature_api::pps;

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge, now_ms};
use crate::sys;

/// The shared reader publishes state, marker, producer identity, generation
/// and monotonic heartbeat under this existing PPS object.
pub static MPE_STATE: pps::Object = pps::Object::new("mpe_state");
/// Published cluster graphics feedback: `gfx_state`, `sync_needed`,
/// `data_rate`, `seq` (was `/dev/shmem/gal_cluster`).
pub static CLUSTER_SINK: pps::Object = pps::Object::new("cluster");

/// The slog code of DM's encoder messages (`_SLOG_SETCODE(43690, 0)`).
pub const NV_MAJOR: c_int = 43690;
/// A new mark establishes a restoration/request generation, not a grant.
pub fn mpe_mark() {
    ENCODER_LIVE.store(true, Ordering::Relaxed);
    ENCODER_SEEN.store(false, Ordering::Relaxed);
    if !crate::slogread::mark() {
        cl_loge!("[GalCluster] current encoder marker unavailable; writes stay blocked");
    }
}

pub static ENCODER_LIVE: AtomicBool = AtomicBool::new(true);
pub static ENCODER_SEEN: AtomicBool = AtomicBool::new(false);

/// A decimal integer at the start of `b`, after leading spaces.
fn parse_int(b: &[u8]) -> Option<i32> {
    let s = core::str::from_utf8(b).ok()?.trim_start();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse().ok()
}

/// Start one shared owned reader, early enough to observe the first hide.
pub fn helper_launch() {
    if CFG.gate_enable() != 0 {
        crate::slogread::start();
    }
}

/// Unknown or unhealthy current producer evidence cannot authorize a write.
/// This intentionally corrects the former cached-closed fallback on failure.
pub fn encoder_live() -> bool {
    let observed = crate::slogread::level();
    let live = observed.unwrap_or(true);
    ENCODER_LIVE.store(live, Ordering::Relaxed);
    ENCODER_SEEN.store(observed.is_some(), Ordering::Relaxed);
    live
}

pub fn mark_busy() {
    crate::slogread::invalidate_generation();
    ENCODER_LIVE.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The cluster MOST-sink feedback (DSIKOMOGfxStreamSink, the display-state publisher)
// ---------------------------------------------------------------------------

/// Sink graphics state, synchronization request and data rate: -1 is unknown.
pub static SINK_GFX: AtomicI32 = AtomicI32::new(-1);
pub static SINK_SYNC: AtomicI32 = AtomicI32::new(-1);
pub static SINK_RATE: AtomicI32 = AtomicI32::new(-1);
/// `s_sink_gfx_recovered`: the 0->1 edge, consumed by the focus-kick arm.
pub static SINK_GFX_RECOVERED: AtomicBool = AtomicBool::new(false);
static SINK_SEQ: AtomicU32 = AtomicU32::new(0);
static SINK_READ_MS: AtomicU64 = AtomicU64::new(0);

fn state_name(v: i32, names: &[(i32, &'static str)]) -> &'static str {
    names
        .iter()
        .find(|(k, _)| *k == v)
        .map_or("unknown", |(_, n)| n)
}

/// One parsed attribute value from the sink PPS object.
fn sink_attr(k: &[u8], v: &[u8], gfx: &mut i32, sync: &mut i32, rate: &mut i32, seq: &mut u32) {
    for (key, slot) in [
        (&b"gfx_state"[..], &mut *gfx),
        (&b"sync_needed"[..], &mut *sync),
        (&b"data_rate"[..], &mut *rate),
    ] {
        if k == key {
            if let Some(n) = parse_int(v) {
                *slot = n;
            }
            return;
        }
    }
    if k == b"seq" {
        if let Some(n) = parse_int(v) {
            #[allow(clippy::cast_sign_loss)] // %u of the parsed digits
            {
                *seq = n as u32;
            }
        }
    }
}

/// Current wall-clock milliseconds, for the optional PPS publication-age check.
pub fn sink_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| elapsed.as_millis().try_into().ok())
        .unwrap_or(0)
}

/// Read a new complete witness snapshot. Do not reuse a cached poll result.
pub fn sink_cycle_snapshot() -> Option<crate::sink_cycle::Snapshot> {
    let mut file = std::fs::File::open(CLUSTER_SINK.path_str()).ok()?;
    crate::sink_cycle::read_snapshot(&mut file)
}

/// `cluster_sink_poll`: the current `sync_needed` (-1, 0, 1).
pub fn sink_poll() -> i32 {
    let t = now_ms();
    let last = SINK_READ_MS.load(Ordering::Relaxed);
    if last != 0 && t.wrapping_sub(last) < 100 {
        return SINK_SYNC.load(Ordering::Relaxed);
    }
    SINK_READ_MS.store(t, Ordering::Relaxed);
    let mut b = [0u8; 512];
    let Some(text) = CLUSTER_SINK.read(&mut b) else {
        return SINK_SYNC.load(Ordering::Relaxed);
    };
    let (mut gfx, mut sync, mut rate) = (-1, -1, -1);
    let mut seq = SINK_SEQ.load(Ordering::Relaxed);
    for (k, v) in pps::attrs(text) {
        sink_attr(k, v, &mut gfx, &mut sync, &mut rate, &mut seq);
    }
    let old_gfx = SINK_GFX.load(Ordering::Relaxed);
    if gfx != old_gfx {
        cl_log!(
            "[GalCluster] sink gfx_state {}->{} ({})",
            old_gfx,
            gfx,
            state_name(gfx, &[(1, "AVAILABLE"), (0, "UNAVAILABLE")])
        );
        if old_gfx == 0 && gfx == 1 {
            SINK_GFX_RECOVERED.store(true, Ordering::Relaxed);
        }
        SINK_GFX.store(gfx, Ordering::Relaxed);
    }
    let old_sync = SINK_SYNC.load(Ordering::Relaxed);
    if sync != old_sync {
        cl_loge!(
            "[GalCluster] sink sync_needed {}->{} ({}) seq={}",
            old_sync,
            sync,
            state_name(sync, &[(1, "SYNCNEEDED"), (0, "SYNCSATISFIED")]),
            seq
        );
        SINK_SYNC.store(sync, Ordering::Relaxed);
    }
    let old_rate = SINK_RATE.load(Ordering::Relaxed);
    if rate != old_rate {
        cl_log!(
            "[GalCluster] sink data_rate {}->{} ({})",
            old_rate,
            rate,
            state_name(rate, &[(2, "FULL"), (1, "REDUCED"), (0, "OFF")])
        );
        SINK_RATE.store(rate, Ordering::Relaxed);
    }
    SINK_SEQ.store(seq, Ordering::Relaxed);
    SINK_SYNC.load(Ordering::Relaxed)
}

// HMI handover and boot-settle release.

/// The PPS root of the handover object. `GAL_PPS_ROOT` (environment,
/// harness only, read once in init) replaces it.
const HANDOVER_PPS_ROOT: &str = "/ramdisk/pps";
const HANDOVER_OBJ: &str = "/cluster/handover";
/// The HMI fades the map in after the handover.
pub const HANDOVER_FADE_MS: u64 = 3000;

/// The object path, NUL-terminated, built once in init (`s_handover_path`).
static HANDOVER_PATH: std::sync::OnceLock<([u8; 160], usize)> = std::sync::OnceLock::new();
pub(crate) static HANDOVER_SEEN_MS: AtomicU64 = AtomicU64::new(0);
static HANDOVER_LOGGED: AtomicBool = AtomicBool::new(false);

/// Build the handover path under the selected root. Limit it to 159 bytes plus the
/// terminating NUL.
pub fn handover_path_init() {
    let root = std::env::var_os("GAL_PPS_ROOT")
        .filter(|r| !r.is_empty())
        .and_then(|r| r.into_string().ok());
    let root = root.as_deref().unwrap_or(HANDOVER_PPS_ROOT);
    let mut b = [0u8; 160];
    let mut n = 0;
    for &c in root.as_bytes().iter().chain(HANDOVER_OBJ.as_bytes()) {
        if n < 159 && c != 0 {
            b[n] = c;
            n += 1;
        }
    }
    let _ = HANDOVER_PATH.set((b, n));
}

/// `pps_attr`: the value of `key` in a PPS line `key:<encoding>:value`.
fn pps_attr<'a>(line: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(key)?.strip_prefix(b":")?;
    let i = rest.iter().position(|&c| c == b':')?;
    Some(&rest[i + 1..])
}

/// `strtoul(v, NULL, 10)` on the 32-bit target: leading digits, saturating
/// (the C `ULONG_MAX` on overflow).
fn parse_ulong(v: &[u8]) -> u32 {
    let s = core::str::from_utf8(v).unwrap_or("").trim_start();
    let digits = s.bytes().take_while(u8::is_ascii_digit);
    digits.fold(0u32, |a, d| {
        a.checked_mul(10)
            .and_then(|x| x.checked_add(u32::from(d - b'0')))
            .unwrap_or(u32::MAX)
    })
}

/// What in the handover object released the gate (`GATE_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Release {
    /// `GATE_HELD`.
    Held,
    /// `GATE_BY_HMI`: `hmi_ctx` set and the fadeIn allowance has passed.
    Hmi,
    /// `GATE_BY_READY`: `ready::1` means the display-state publisher completed its early cluster
    /// start.
    Ready,
}

static READY_SEEN_MS: AtomicU64 = AtomicU64::new(0);

/// `base` from the attribute, or the first sight of the object at `up_ms`.
pub(crate) fn base_or_seen(have: bool, v: u32, seen: &AtomicU64, up_ms: u64) -> u64 {
    if have {
        return u64::from(v);
    }
    let s = seen.load(Ordering::Relaxed);
    if s == 0 {
        seen.store(up_ms, Ordering::Relaxed);
        up_ms
    } else {
        s
    }
}

/// The attributes of the handover object the gate reads.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per optional PPS attribute
pub(crate) struct Handover {
    ctx: i32,
    pub(crate) have_ctx: bool,
    pub(crate) ms: u32,
    pub(crate) have_ms: bool,
    ready: bool,
    ready_ms: u32,
    have_ready_ms: bool,
}

/// Read the handover object. None when it is absent or empty.
pub(crate) fn read_handover() -> Option<Handover> {
    let (pb, pn) = HANDOVER_PATH.get()?;
    let mut path = [0u8; 161];
    path[..*pn].copy_from_slice(&pb[..*pn]);
    let path = CStr::from_bytes_until_nul(&path).ok()?;
    let mut buf = [0u8; 255];
    let n = sys::read_file(path, &mut buf)?;
    if n == 0 {
        return None;
    }
    let mut h = Handover::default();
    // Empty lines are skipped. A NUL ends the text
    let text = buf[..n].split(|&c| c == 0).next().unwrap_or(&[]);
    for line in text.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        if let Some(v) = pps_attr(line, b"hmi_ctx") {
            h.ctx = parse_int(v).unwrap_or(0);
            h.have_ctx = true;
        } else if let Some(v) = pps_attr(line, b"hmi_ms") {
            h.ms = parse_ulong(v);
            h.have_ms = true;
        } else if let Some(v) = pps_attr(line, b"ready_ms") {
            h.ready_ms = parse_ulong(v);
            h.have_ready_ms = true;
        } else if let Some(v) = pps_attr(line, b"ready") {
            h.ready = parse_int(v).unwrap_or(0) == 1;
        }
    }
    Some(h)
}

/// The handover object fields that release the
/// gate at `up_ms`. When both `ready::1` and `hmi_ctx` (+ the fadeIn) have
/// released, the one whose release time came first wins.
pub fn hmi_handover_done(up_ms: u64) -> Release {
    let Some(h) = read_handover() else {
        return Release::Held;
    };
    let mut base = 0u64;
    let mut hmi_ok = false;
    if h.have_ctx {
        base = base_or_seen(h.have_ms, h.ms, &HANDOVER_SEEN_MS, up_ms);
        hmi_ok = up_ms >= base + HANDOVER_FADE_MS;
    }
    let mut ready_base = 0u64;
    if h.ready {
        ready_base = base_or_seen(h.have_ready_ms, h.ready_ms, &READY_SEEN_MS, up_ms);
    }
    let by = if h.ready && (!hmi_ok || ready_base <= base + HANDOVER_FADE_MS) {
        Release::Ready
    } else if hmi_ok {
        Release::Hmi
    } else {
        return Release::Held;
    };
    if !HANDOVER_LOGGED.swap(true, Ordering::AcqRel) {
        if by == Release::Ready {
            cl_log!(
                "[GalCluster] boot settle: gate released by ready at uptime {}ms (ready_ms={}{}: the HMI startup is complete) ahead of the {}s uptime fallback",
                up_ms & 0xFFFF_FFFF, // the low 32 bits of the uptime counter
                if h.have_ready_ms { "" } else { "unset, first seen at " },
                ready_base & 0xFFFF_FFFF,
                CFG.boot_settle_secs()
            );
        } else {
            cl_log!(
                "[GalCluster] boot settle: gate released by hmi_ctx at uptime {}ms (hmi_ctx={} hmi_ms={}{}: the HMI handed the cluster to nav, and the {}ms fadeIn allowance has passed) ahead of the {}s uptime fallback",
                up_ms & 0xFFFF_FFFF, // the low 32 bits of the uptime counter
                h.ctx,
                if h.have_ms { "" } else { "unset, first seen at " },
                base & 0xFFFF_FFFF,
                HANDOVER_FADE_MS,
                CFG.boot_settle_secs()
            );
        }
    }
    by
}

/// Whether the last release came from the handover object
/// (`ready::1` or `hmi_ctx`). then `cluster_handover_settle_ms` applies.
pub static GATE_BY_HANDOVER: AtomicBool = AtomicBool::new(false);

/// `cluster_boot_settling` remains true while uptime is below `cluster_boot_settle_secs`
/// and the handover object has no release. A release sets `taking::1` in the takeover
/// object.
pub fn boot_settling(mono: qnx_time::Timespec) -> bool {
    let settle = CFG.boot_settle_secs();
    if settle <= 0 {
        GATE_BY_HANDOVER.store(false, Ordering::Relaxed);
        return false;
    }
    let secs = crate::log::long(mono.tv_sec);
    let up_ms = u64::try_from(secs).unwrap_or(0) * 1000
        + u64::try_from(crate::log::long(mono.tv_nsec) / 1_000_000).unwrap_or(0);
    if secs >= i64::from(settle) {
        GATE_BY_HANDOVER.store(false, Ordering::Relaxed);
        if !HANDOVER_LOGGED.swap(true, Ordering::AcqRel) {
            cl_log!(
                "[GalCluster] boot settle: gate released by uptime at uptime {}ms (cluster_boot_settle_secs={}s)",
                up_ms & 0xFFFF_FFFF, // the low 32 bits of the uptime counter
                settle
            );
        }
        crate::takeover::taking();
        return false;
    }
    if hmi_handover_done(up_ms) == Release::Held {
        return true;
    }
    GATE_BY_HANDOVER.store(true, Ordering::Relaxed);
    crate::takeover::taking();
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pps_lines() {
        assert_eq!(pps_attr(b"hmi_ctx::70", b"hmi_ctx"), Some(&b"70"[..]));
        assert_eq!(pps_attr(b"hmi_ms:n:48123", b"hmi_ms"), Some(&b"48123"[..]));
        assert_eq!(pps_attr(b"hmi_ctxx::1", b"hmi_ctx"), None);
        assert_eq!(pps_attr(b"@handover", b"hmi_ctx"), None);
        assert_eq!(parse_ulong(b"48123"), 48123);
        assert_eq!(parse_ulong(b"99999999999"), u32::MAX);
    }

    #[test]
    fn decimal_integers_accept_leading_spaces() {
        assert_eq!(parse_int(b"1234\n"), Some(1234));
        assert_eq!(parse_int(b"  -3x"), Some(-3));
        assert_eq!(parse_int(b"x"), None);
    }

    #[test]
    fn sink_attributes() {
        let (mut g, mut s, mut r, mut q) = (-1, -1, -1, 0u32);
        let text = b"@cluster\nseq::17\nupdated_ms::1700000000000\ngfx_state::1\nsync_needed::0\ndata_rate::2\n";
        for (k, v) in pps::attrs(text) {
            sink_attr(k, v, &mut g, &mut s, &mut r, &mut q);
        }
        assert_eq!((g, s, r, q), (1, 0, 2, 17));
        // a key the writer deleted (invalid attribute) stays unknown
        let (mut g, mut s, mut r, mut q) = (-1, -1, -1, 0u32);
        for (k, v) in pps::attrs(b"@cluster\nseq::18\ngfx_state::0\n") {
            sink_attr(k, v, &mut g, &mut s, &mut r, &mut q);
        }
        assert_eq!((g, s, r, q), (0, -1, -1, 18));
    }
}
