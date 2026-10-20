// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Routing-thread video handling: `IoBuffer` bounds, PTS, SPS/PPS prepend,
//! writer delivery, focus recovery, MPE ownership and health counters.
//! Every frame is acknowledged after the MOST lock is released.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use gal_vendor_abi::offsets as off;
use most_isotx2 as mlb;
use qnx_slog::log::Line;

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge, now_ms, ul, wall_s};
use crate::pipe::{self, lock, Drv, Most, Route, MOST};
use crate::{dm, gate, h264, route};

// ---------------------------------------------------------------------------
// IoBuffer (gal-vendor-abi offsets. `iobuf_data` / `iobuf_len`)
// ---------------------------------------------------------------------------

/// `s_iobuf_bad`, `s_iobuf_mm_video`, `s_iobuf_mm_ctrl`, `s_iobuf_mm_max`.
pub static IOBUF_BAD: AtomicU32 = AtomicU32::new(0);
pub static IOBUF_MM_VIDEO: AtomicU32 = AtomicU32::new(0);
pub static IOBUF_MM_CTRL: AtomicU32 = AtomicU32::new(0);
pub static IOBUF_MM_MAX: AtomicU32 = AtomicU32::new(0);

/// Return message bytes from a shared `IoBuffer`: data plus start, for end minus start
/// bytes. Return None for a missing buffer or data pointer. Return an empty slice for
/// an empty or inconsistent range.
///
/// # Safety
///
/// sptr is null or a shared `IoBuffer` reference valid for this call.
pub unsafe fn iobuf<'a>(sptr: *const u8, is_video: bool) -> (Option<&'a [u8]>, u32) {
    if sptr.is_null() {
        return (None, 0);
    }
    // SAFETY: the caller's contract. the offsets of gal-vendor-abi.
    unsafe {
        let inner = sptr
            .add(off::SHARED_PTR_IOBUFFER)
            .cast::<*const u8>()
            .read_unaligned();
        if inner.is_null() {
            return (None, 0);
        }
        let base = inner
            .add(off::IOBUFFER_DATA)
            .cast::<*const u8>()
            .read_unaligned();
        let cap = inner
            .add(off::IOBUFFER_CAPACITY)
            .cast::<u32>()
            .read_unaligned();
        let start = inner
            .add(off::IOBUFFER_START)
            .cast::<u32>()
            .read_unaligned();
        let end = inner.add(off::IOBUFFER_END).cast::<u32>().read_unaligned();
        let len = iobuf_len(cap, start, end, is_video);
        if base.is_null() {
            return (None, len);
        }
        if len == 0 {
            return (
                Some(core::slice::from_raw_parts(base.add(start as usize), 0)),
                0,
            );
        }
        (off::frame_of(sptr), len)
    }
}

/// Return `end - start`, or zero for inconsistent metadata.
/// Log invalid metadata and unused capacity after the message.
pub fn iobuf_len(cap: u32, start: u32, end: u32, is_video: bool) -> u32 {
    let old_len = cap.saturating_sub(start);
    if end < start || end > cap {
        let bad = IOBUF_BAD.fetch_add(1, Ordering::Relaxed) + 1;
        if bad == 1 {
            cl_loge!(
                "[GalCluster] IoBuffer metadata INVALID (cap={} start={} end={}) \u{2014} dropping message. capacity_minus_start={} unused_or_invalid_bytes={}",
                cap,
                start,
                end,
                old_len,
                old_len
            );
        }
        return 0;
    }
    if end == start {
        return 0;
    }
    let len = end - start;
    if len != old_len {
        let trail = old_len - len;
        let first = if is_video {
            IOBUF_MM_VIDEO.fetch_add(1, Ordering::Relaxed) == 0
        } else {
            IOBUF_MM_CTRL.fetch_add(1, Ordering::Relaxed) == 0
        };
        IOBUF_MM_MAX.fetch_max(trail, Ordering::Relaxed);
        if first {
            cl_loge!(
                "[GalCluster] IoBuffer len ({}): message_bytes={} capacity_minus_start={} (cap={} start={} end={}) \u{2014} {} bytes after the message are excluded",
                if is_video { "VIDEO AU" } else { "control" },
                len,
                old_len,
                cap,
                start,
                end,
                trail
            );
        }
    }
    len
}

// ---------------------------------------------------------------------------
// Runtime status
// ---------------------------------------------------------------------------

/// Publish health counters in the PPS object /`ramdisk/pps/gal/cluster_status` for
/// companion readers.
pub static CLUSTER_STATUS: gal_feature_api::pps::Object =
    gal_feature_api::pps::Object::new("cluster_status");

/// The numbers of one health tick.
pub struct Status {
    pub up_s: u32,
    pub frames: u32,
    pub dframes: u32,
    pub fps10: u32,
    pub idr: u32,
    pub nat_idr: u32,
    pub wq_cur: u32,
    pub wq_peak: u32,
    pub wq_drops: u32,
    pub werr: u32,
    pub wshort: u32,
    pub cb_trips: u32,
    pub iobad: u32,
    pub oobrx: u32,
    pub kbps: u32,
}

/// `write_cluster_status`: one attribute per counter, one write, best
/// effort (a fixed key set, so nothing is ever deleted).
pub fn write_status(s: &Status) {
    let mut b: Line<512> = Line::new();
    let _ = write!(
        b,
        "version::84\nupdated_ms::{}\nup_s::{}\nframes::{}\nframes_delta::{}\nfps_x10::{}\nidr::{}\nnat_idr::{}\nkbps::{}\nwq_cur::{}\nwq_peak::{}\nwq_drops::{}\nwerr::{}\nwshort::{}\ncb_trips::{}\niobad::{}\noobrx::{}\n",
        ul(now_ms()),
        s.up_s,
        s.frames,
        s.dframes,
        s.fps10,
        s.idr,
        s.nat_idr,
        s.kbps,
        s.wq_cur,
        s.wq_peak,
        s.wq_drops,
        s.werr,
        s.wshort,
        s.cb_trips,
        s.iobad,
        s.oobrx
    );
    let _ = CLUSTER_STATUS.write(b.as_str().as_bytes());
}

// ---------------------------------------------------------------------------
// AU emission
// ---------------------------------------------------------------------------

static RING_FULL_LOG_MS: AtomicU64 = AtomicU64::new(0);

/// `cluster_emit_au`: prepend the cached SPS/PPS to an IDR, then hand the
/// AU to the writer ring (or emit inline).
pub fn cluster_emit_au(rt: &mut Route, drv: &mut Drv, h264: &[u8], pts_90k: u64) {
    let Route {
        codec_cfg,
        codec_cfg_len,
        au_buf,
        es_bytes,
        ..
    } = rt;
    let cfg_len = *codec_cfg_len;
    let is_idr_au = h264::is_idr(h264);
    let crop_on = crate::crop::active();
    let room = if crop_on { crate::crop::GROWTH } else { 0 };
    let fits = cfg_len + h264.len() + room <= pipe::AU_BUF_SIZE;
    let prepend = cfg_len > 0 && is_idr_au && fits;
    // cluster_sps_crop: an SPS inside the AU itself gets the same rewrite as
    // the cached one, or the decoder would take the phone's crop from it.
    let recrop = fits && crop_on && crate::crop::has_sps(h264);
    let mut total = 0;
    if prepend || recrop {
        let off = if prepend { cfg_len } else { 0 };
        au_buf[..off].copy_from_slice(&codec_cfg[..off]);
        total = off
            + if recrop {
                crate::crop::store(&mut au_buf[off..], h264)
            } else {
                au_buf[off..off + h264.len()].copy_from_slice(h264);
                h264.len()
            };
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        *es_bytes = es_bytes.wrapping_add(h264.len() as u32);
    }
    let au: &[u8] = if prepend || recrop {
        &au_buf[..total]
    } else {
        h264
    };
    if CFG.output() == 2 {
        if pipe::CV_NEED_IDR.load(Ordering::Acquire) && !is_idr_au {
            return;
        }
        if is_idr_au {
            pipe::CV_NEED_IDR.store(false, Ordering::Release);
        }
        if !pipe::writer_available() || !pipe::wq_push(au, pts_90k) {
            pipe::CV_NEED_IDR.store(true, Ordering::Release);
            pipe::CV_LOSS.store(true, Ordering::Release);
            pipe::lock(&pipe::RING).clear();
        }
        return;
    }
    if !pipe::writer_available() {
        let trip = {
            let mut ts = pipe::ts();
            ts.out.trip = None;
            pipe::emit_au_now(&mut ts, false, au, pts_90k);
            ts.out.trip.take()
        };
        pipe::run_inline_trip(drv, trip);
        return;
    }
    if !pipe::wq_push(au, pts_90k) {
        let n = now_ms();
        if n.wrapping_sub(RING_FULL_LOG_MS.load(Ordering::Relaxed)) > 1000 {
            RING_FULL_LOG_MS.store(n, Ordering::Relaxed);
            let (count, bytes) = {
                let r = lock(&pipe::RING);
                (r.count, r.bytes)
            };
            cl_loge!(
                "[GalCluster] writer ring FULL ({} frames / {}KB queued) \u{2014} dropped a frame; writer is not keeping up",
                count,
                bytes / 1024
            );
        }
    }
}

// ---------------------------------------------------------------------------

fn pts_of(most: &Most, ts_us: u64) -> u64 {
    ts_us.saturating_sub(most.rt.pts_base_us) * 9 / 100
}

/// Arrival counters are retained for stream health and teardown diagnostics.
static ARRIVAL_COUNT: AtomicU32 = AtomicU32::new(0);
pub static LAST_ARRIVAL_MS: AtomicU64 = AtomicU64::new(0);
static ARRIVAL_LOG_MS: AtomicU64 = AtomicU64::new(0);

/// What the first, pre-gate part of the handler found.
struct Frame {
    /// Offset and length of the H.264 in the message.
    hoff: usize,
    h264_len: usize,
    pts_90k: u64,
}

/// The part before the sink poll (MOST held): `IoBuffer`, header, PTS,
/// harvest, IDR and gap diagnostics. None: acked without a frame.
fn frame_prologue(most: &mut Most, ch: u8, data: Option<&[u8]>, len: u32) -> Option<Frame> {
    let Some(data) = data.filter(|_| len >= 10) else {
        cl_loge!(
            "[GalCluster] ch={} DATA: short payload len={} (need >=10) \u{2014} acking anyway",
            ch,
            len
        );
        return None;
    };
    let ts_us = data[2..10]
        .iter()
        .fold(0u64, |a, &b| (a << 8) | u64::from(b));
    let mut hoff = 8usize;
    let dl = data.len();
    let mut j = 8usize;
    while j + 3 <= dl {
        if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
            hoff = j;
            break;
        }
        if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 0 && j + 4 <= dl && data[j + 3] == 1 {
            hoff = j;
            break;
        }
        j += 1;
    }
    let h264_len = dl.saturating_sub(hoff);
    if h264_len == 0 {
        cl_loge!(
            "[GalCluster] ch={} DATA: zero H.264 payload after header (hoff={} len={}) \u{2014} acking",
            ch,
            hoff,
            len
        );
        return None;
    }
    let h = &data[hoff..];
    if !most.rt.pts_base_set {
        most.rt.pts_base_us = ts_us;
        most.rt.pts_base_set = true;
        cl_log!(
            "[GalCluster] ch={} PTS base set: {} ms (media clock)",
            ch,
            ul(ts_us / 1000)
        );
    }
    let pts_90k = pts_of(most, ts_us);
    if most.rt.codec_cfg_len == 0 {
        if let Some((s, e)) = h264::harvest_range(h) {
            let n = e - s;
            if n != 0 && n <= pipe::CODEC_CFG_SIZE {
                let n = crate::crop::store(&mut most.rt.codec_cfg, &h[s..e]);
                most.rt.codec_cfg_len = n;
                h264::check_codec_config(&most.rt.codec_cfg[..n]);
                h264::log_harvested(n);
            }
        }
    }
    idr_and_gap(most, ch, h);
    Some(Frame {
        hoff,
        h264_len,
        pts_90k,
    })
}

/// The IDR cadence and frame-gap diagnostics of the prologue.
fn idr_and_gap(most: &mut Most, ch: u8, h: &[u8]) {
    let tnow = now_ms();
    if h264::is_idr(h) {
        most.idr_seen += 1;
        let frames = pipe::FRAME_COUNT.load(Ordering::Relaxed);
        if most.dm_drain_done && frames > 0 {
            most.natural_idr += 1;
            cl_log!(
                "[GalCluster] ch={} natural IDR #{} at frame={} ({}ms since prev IDR)",
                ch,
                most.natural_idr,
                frames,
                ul(if most.last_idr_ms != 0 {
                    tnow.wrapping_sub(most.last_idr_ms)
                } else {
                    0
                })
            );
        }
        most.last_idr_ms = tnow;
    }
    if most.last_frame_ms != 0 {
        let gap = tnow.wrapping_sub(most.last_frame_ms);
        let gap32 = ul(gap);
        if gap32 > most.max_gap_ms {
            most.max_gap_ms = gap32;
        }
    }
    most.last_frame_ms = tnow;
}

/// The focus kick arm (sink feedback) and fire (both notifications back to
/// back), and a pending writer trip. True when the frame is done (acked).
fn kick_and_trip(most: &mut Most, ch: u8) -> bool {
    // IDR kicks apply only while GAL streams.
    crate::kick::check(
        ch,
        pipe::MOST_FD.load(Ordering::Acquire) >= 0 && most.first_frame_ms != 0,
    );
    let c = &CFG;
    let fk_stream = pipe::FK_STREAM_MS.load(Ordering::Relaxed);
    #[allow(clippy::cast_sign_loss)]
    if c.sink_feedback() != 0
        && c.focus_kick() != 0
        && (gate::SINK_GFX_RECOVERED.load(Ordering::Relaxed)
            || gate::SINK_SYNC.load(Ordering::Relaxed) == 1)
        && !pipe::FK_ARMED.load(Ordering::Relaxed)
        && pipe::FK_STATE.load(Ordering::Relaxed) == 0
        && fk_stream != 0
        && now_ms().wrapping_sub(fk_stream) >= c.focus_kick_grace() as u64
    {
        let by_gfx = gate::SINK_GFX_RECOVERED.swap(false, Ordering::Relaxed);
        pipe::FK_ARMED.store(true, Ordering::Relaxed);
        pipe::FK_DRAINED_MS.store(0, Ordering::Relaxed);
        cl_loge!(
            "[GalCluster] sink {} -> arming focus kick (budget {})",
            if by_gfx {
                "gfx_state 0->1 (fresh decoder)"
            } else {
                "REQUESTSYNC=1 (level)"
            },
            c.focus_kick_max()
        );
    }
    #[allow(clippy::cast_sign_loss)]
    let settle = c.focus_kick_settle() as u64;
    if c.focus_kick() != 0 && pipe::FK_STATE.load(Ordering::Relaxed) != 0 {
        let nowk = now_ms();
        if nowk.wrapping_sub(pipe::FK_DRAINED_MS.load(Ordering::Relaxed)) >= settle {
            pipe::FK_EXPECT_RESTART.store(true, Ordering::Release);
            route::send_video_focus_notification(
                ch,
                crate::sdr::FOCUS_NATIVE,
                crate::sdr::VFN_UNSOLICITED,
            );
            route::send_video_focus_notification(
                ch,
                crate::sdr::FOCUS_PROJECTED,
                crate::sdr::VFN_UNSOLICITED,
            );
            pipe::FK_STATE.store(0, Ordering::Relaxed);
            pipe::FK_ARMED.store(false, Ordering::Relaxed);
            pipe::FK_LAST_MS.store(nowk, Ordering::Relaxed);
            let n = pipe::FK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            cl_loge!(
                "[GalCluster] focus kick {}/{}: toggled focus NATIVE->PROJECTED back to back \u{2014} expect the phone to restart the stream with a fresh CodecConfig + IDR; cluster context held across it",
                n,
                c.focus_kick_max()
            );
        }
    } else if c.focus_kick() != 0
        && pipe::FK_ARMED.load(Ordering::Relaxed)
        && pipe::FK_STATE.load(Ordering::Relaxed) == 0
    {
        let nowk = now_ms();
        let depth = lock(&pipe::RING).count;
        let drained = pipe::FK_DRAINED_MS.load(Ordering::Relaxed);
        #[allow(clippy::cast_sign_loss)]
        if depth > 1 {
            pipe::FK_DRAINED_MS.store(0, Ordering::Relaxed);
        } else if drained == 0 {
            pipe::FK_DRAINED_MS.store(nowk, Ordering::Relaxed);
        } else if nowk.wrapping_sub(drained) >= settle {
            let last = pipe::FK_LAST_MS.load(Ordering::Relaxed);
            if pipe::FK_COUNT.load(Ordering::Relaxed) >= c.focus_kick_max() as u32 {
                pipe::FK_ARMED.store(false, Ordering::Relaxed);
                cl_loge!(
                    "[GalCluster] focus kick: budget spent ({}) \u{2014} not kicking again this session",
                    c.focus_kick_max()
                );
            } else if last != 0 && nowk.wrapping_sub(last) < c.focus_kick_min_ms() as u64 {
                // too soon after the last one. keep waiting
            } else {
                pipe::FK_STATE.store(1, Ordering::Relaxed);
                pipe::FK_DRAINED_MS.store(nowk.wrapping_sub(settle), Ordering::Relaxed);
            }
        }
    }
    if pipe::CB_TRIP_PENDING.swap(false, Ordering::AcqRel) {
        let why = pipe::take_pending_trip();
        pipe::cb_trip(&mut most.drv, why.as_str());
        return true;
    }
    false
}

/// The MPE gate and the drain on the way to the first write. True when the
/// frame is done (acked).
fn gate_first_write(most: &mut Most, ch: u8, h264: &[u8]) -> bool {
    let c = &CFG;
    if c.gate_enable() != 0 && most.dm_drain_done && gate::encoder_live() {
        cl_loge!(
            "[GalCluster] ch={} MPE re-enabled mid-session \u{2014} HMI reclaimed cluster; re-evicting + pausing writes until Disabled",
            ch
        );
        most.dm_drain_done = false;
        most.need_first_idr = true;
        dm::reassert_hide();
    }
    if !most.dm_drain_done {
        let now = wall_s();
        let gated = if c.gate_enable() != 0 {
            gate::encoder_live()
        } else {
            false
        };
        if gated {
            if c.gate_enable() != 0 {
                dm::reassert_hide();
            }
            return true;
        }
        {
            let fd = pipe::MOST_FD.load(Ordering::Acquire);
            let mut ts = pipe::ts();
            let r = mlb::flush_queue(fd);
            cl_log!(
                "[GalCluster] ch={} {} ({}s) \u{2014} queue FLUSH r={}{}",
                ch,
                if c.gate_enable() != 0 {
                    "MPE gate clear: encoder DOWN, sole writer"
                } else {
                    "DM drain complete"
                },
                now - most.drv.open_time,
                r,
                if r != 0 { " (WARN)" } else { " ok" }
            );
            let mut num_max = crate::cfg::TX_QUEUE_PACKETS;
            let requested = num_max;
            let r = mlb::start(fd, &mut num_max);
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let (bp, ps) = (mlb::TS_BLOCK_PACKETS as i32, mlb::TS_PACKET_SIZE as i32);
            cl_log!(
                "[GalCluster] START devctl r={}{} \u{2014} requested {} packets ({} blocks, {} bytes), driver returned {}{} [issued AFTER encoder-down, not at open]",
                r,
                if r != 0 { " (WARN)" } else { " ok" },
                requested,
                requested / bp,
                requested * ps,
                num_max,
                if num_max == requested {
                    " (accepted verbatim)"
                } else {
                    " (DIFFERS)"
                }
            );
            ts.reset_mux();
            pipe::TABLES_SENT.store(false, Ordering::Relaxed);
        }
        most.dm_drain_done = true;
        if !h264::is_idr(h264) {
            cl_log!(
                "[GalCluster] ch={} sole writer \u{2014} waiting for the first IDR",
                ch
            );
            most.need_first_idr = true;
            return true;
        }
        most.need_first_idr = false;
    }
    if most.need_first_idr {
        if !h264::is_idr(h264) {
            return true;
        }
        cl_log!(
            "[GalCluster] ch={} first IDR arrived \u{2014} starting stream",
            ch
        );
        most.need_first_idr = false;
    }
    false
}

/// Publish stream health after the access unit went out.
fn after_emit(most: &mut Most) {
    let tnow = now_ms();
    let dt = tnow.wrapping_sub(most.health_last_ms);
    if dt >= 5000 {
        let frames = pipe::FRAME_COUNT.load(Ordering::Relaxed);
        let bytes = pipe::BYTES_WRITTEN.load(Ordering::Relaxed);
        let dframes = frames.wrapping_sub(most.health_last_frames);
        let dbytes = bytes.wrapping_sub(most.health_last_bytes);
        let dtn = if dt != 0 { dt } else { 1 };
        let fps10 = ul(u64::from(dframes) * 10000 / dtn);
        let kbps = ul(u64::from(dbytes) * 8 / dtn);
        let up = ul(tnow.wrapping_sub(if most.first_frame_ms != 0 {
            most.first_frame_ms
        } else {
            tnow
        }) / 1000);
        let (wq_cur, wq_peak, wq_drops) = {
            let r = lock(&pipe::RING);
            (r.count, r.peak, r.drops)
        };
        let trips = pipe::CB_TRIP_COUNT.load(Ordering::Relaxed);
        write_status(&Status {
            up_s: up,
            frames,
            dframes,
            fps10,
            idr: most.idr_seen,
            nat_idr: most.natural_idr,
            wq_cur,
            wq_peak,
            wq_drops,
            werr: pipe::MOST_WERR.load(Ordering::Relaxed),
            wshort: pipe::MOST_WSHORT.load(Ordering::Relaxed),
            cb_trips: trips.cast_unsigned(),
            iobad: IOBUF_BAD.load(Ordering::Relaxed),
            oobrx: most.oob_inbound,
            kbps,
        });
        most.health_last_ms = tnow;
        most.health_last_frames = frames;
        most.health_last_bytes = bytes;
        most.max_gap_ms = 0;
    }
}

/// `handle_video_data`: one AV media DATA message.
///
/// # Safety
/// `sptr` is gal's `shared_ptr<IoBuffer>`, valid for the call.
pub unsafe fn handle_video_data(ch: u8, sptr: *const u8) {
    if CFG.output() == 2 && !route::started() {
        route::send_media_ack(ch);
        return;
    }
    // FIRST, before any validation, lock or write: record the arrival (the
    // only signal that survives a wedged write path).
    let t = now_ms();
    let n = ARRIVAL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    LAST_ARRIVAL_MS.store(t, Ordering::Relaxed);
    let log_ms = ARRIVAL_LOG_MS.load(Ordering::Relaxed);
    if log_ms == 0 {
        ARRIVAL_LOG_MS.store(t, Ordering::Relaxed);
    } else if t.wrapping_sub(log_ms) >= 5000 {
        let win = ul(t.wrapping_sub(log_ms));
        cl_log!(
            "[GalCluster] arrivals: {} frames in {}ms ({} per 10s)",
            n,
            win,
            ul(u64::from(n) * 10000 / u64::from(if win != 0 { win } else { 1 }))
        );
        ARRIVAL_COUNT.store(0, Ordering::Relaxed);
        ARRIVAL_LOG_MS.store(t, Ordering::Relaxed);
    }

    let mut most = lock(&MOST);
    // SAFETY: the caller's contract.
    let (data, len) = unsafe { iobuf(sptr, true) };
    let f = frame_prologue(&mut most, ch, data, len);
    drop(most);
    let (Some(data), Some(f)) = (data, f) else {
        route::send_media_ack(ch);
        return;
    };
    let h264 = &data[f.hoff..f.hoff + f.h264_len];

    if CFG.output() == 2 {
        let mut most = lock(&MOST);
        if pipe::CV_NEED_IDR.load(Ordering::Acquire) && !h264::is_idr(h264) {
            route::kick_fresh_idr(&mut most, ch);
        }
        let m = &mut *most;
        cluster_emit_au(&mut m.rt, &mut m.drv, h264, f.pts_90k);
        drop(most);
        route::send_media_ack(ch);
        return;
    }

    if CFG.sink_feedback() != 0 {
        gate::sink_poll();
    }

    let mut most = lock(&MOST);
    if (CFG.gate_enable() != 0 && !crate::vfn::ready_for(ch))
        || kick_and_trip(&mut most, ch)
        || pipe::SINK_UI.load(Ordering::Acquire)
        || pipe::most_open(&mut most) < 0
    {
        drop(most);
        route::send_media_ack(ch);
        return;
    }
    if most.boot_deferred {
        most.boot_deferred = false;
        cl_log!("[GalCluster] boot settle complete \u{2014} cluster taken (no kick)");
    }
    if gate_first_write(&mut most, ch, h264) {
        drop(most);
        route::send_media_ack(ch);
        return;
    }
    if pipe::FRAME_COUNT.load(Ordering::Relaxed) == 0 {
        most.first_frame_ms = now_ms();
        // The hmi_ctx kick compares against this value.
        crate::kick::STREAM_UP_MS.store(crate::kick::uptime_ms32(), Ordering::Relaxed);
        cl_log!(
            "[GalCluster] ch={} first frame to MOST: pts_90k={} h264_len={} start_code={} ({}ms after SETUP)",
            ch,
            ul(f.pts_90k),
            h264.len(),
            if h264::starts_with_start_code(h264) {
                "ok"
            } else {
                "MISSING"
            },
            ul(if most.setup_ms != 0 {
                most.first_frame_ms.wrapping_sub(most.setup_ms)
            } else {
                0
            })
        );
    }
    {
        let m = &mut *most;
        cluster_emit_au(&mut m.rt, &mut m.drv, h264, f.pts_90k);
    }
    after_emit(&mut most);
    drop(most);
    route::send_media_ack(ch);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iobuf_len_is_end_minus_start() {
        assert_eq!(iobuf_len(37, 4, 8, false), 4);
        assert_eq!(IOBUF_MM_CTRL.load(Ordering::Relaxed), 1);
        assert_eq!(IOBUF_MM_MAX.load(Ordering::Relaxed), 29);
        assert_eq!(iobuf_len(2236, 4, 2236, true), 2232);
        assert_eq!(iobuf_len(10, 4, 4, true), 0);
        assert_eq!(iobuf_len(10, 5, 4, true), 0);
        assert_eq!(iobuf_len(10, 0, 11, true), 0);
        assert_eq!(IOBUF_BAD.load(Ordering::Relaxed), 2);
    }
}
