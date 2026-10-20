// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! MOST session, muxer, writer ring and circuit breaker. The MOST lock protects the
//! session. The TS lock protects mux state and writes. The ring lock protects bounded
//! copies. Lock order is MOST, then TS, then ring. The writer uses its scratch buffer
//! before ring or TS access. The VFN lock is a leaf below MOST. Replacing the muxer
//! clears continuity and partial-block state. A breaker trip requested while MOST is
//! held runs after the current mux call and drops its remaining blocks.

use core::ffi::c_int;
use core::fmt::Write as _;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use h264_ts_mux::ts::{Mux, MuxIo, PcrPlacement};
use most_isotx2 as mlb;
use qnx_slog::log::Line;

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge, long, mono_us, now_ms, ul, wall_s};
use crate::sys;
use crate::{dm, gate, route};

/// How often to emit periodic DATA/ACK stats and send PAT/PMT (frames).
pub const LOG_PERIOD_FRAMES: u32 = 150;
/// Maximum stored codec configuration size.
pub const CODEC_CFG_SIZE: usize = 512;
/// Largest access unit the writer drains.
pub const IDR_BUF_SIZE: usize = 256 * 1024;
/// Access-unit storage including codec configuration.
pub const AU_BUF_SIZE: usize = CODEC_CFG_SIZE + IDR_BUF_SIZE;
const TS_BLOCK_SIZE: usize = mlb::TS_BLOCK_SIZE;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// Atomic state shared across routing and writer threads.

/// MOST output file descriptor; -1 means closed.
pub static MOST_FD: AtomicI32 = AtomicI32::new(-1);
pub static CV_NEED_IDR: AtomicBool = AtomicBool::new(true);
pub static CV_LOSS: AtomicBool = AtomicBool::new(false);
/// Frame count incremented by the writer and reset by the routing thread.
pub static FRAME_COUNT: AtomicU32 = AtomicU32::new(0);
/// Whether stream tables have been sent.
pub static TABLES_SENT: AtomicBool = AtomicBool::new(false);
/// Total bytes written to the output.
pub static BYTES_WRITTEN: AtomicU32 = AtomicU32::new(0);
pub static MOST_WERR: AtomicU32 = AtomicU32::new(0);
pub static MOST_WSHORT: AtomicU32 = AtomicU32::new(0);
/// Consecutive write errors counted by the circuit breaker.
pub static CB_WERR_CONSEC: AtomicU32 = AtomicU32::new(0);
pub static CB_STALL_CONSEC: AtomicU32 = AtomicU32::new(0);
pub static CB_TRIPPED: AtomicBool = AtomicBool::new(false);
pub static CB_TRIP_TIME: AtomicI64 = AtomicI64::new(0);
pub static CB_TRIP_COUNT: AtomicI32 = AtomicI32::new(0);
pub static CB_LOCKED_OUT: AtomicBool = AtomicBool::new(false);
/// A circuit-breaker trip requested by the writer for the routing thread.
pub static CB_TRIP_PENDING: AtomicBool = AtomicBool::new(false);
static CB_TRIP_WHY: Mutex<Line<96>> = Mutex::new(Line::new());
/// Whether a write is in flight, for teardown diagnostics.
pub static WRITE_INFLIGHT: AtomicBool = AtomicBool::new(false);
pub static WRITE_ENTER_MS: AtomicU64 = AtomicU64::new(0);
pub static WRITE_EXIT_MS: AtomicU64 = AtomicU64::new(0);
/// Whether the owned UI currently uses the sink.
pub static SINK_UI: AtomicBool = AtomicBool::new(false);
/// Focus-kick restart expectation.
pub static FK_EXPECT_RESTART: AtomicBool = AtomicBool::new(false);
pub static FK_STREAM_MS: AtomicU64 = AtomicU64::new(0);
pub static FK_ARMED: AtomicBool = AtomicBool::new(false);
pub static FK_STATE: AtomicI32 = AtomicI32::new(0);
pub static FK_DRAINED_MS: AtomicU64 = AtomicU64::new(0);
pub static FK_LAST_MS: AtomicU64 = AtomicU64::new(0);
pub static FK_COUNT: AtomicU32 = AtomicU32::new(0);
/// Total frames paced by presentation timestamps.
pub static PACE_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static PACE_WAIT_MS: AtomicU32 = AtomicU32::new(0);
/// Cluster video channel; 0xFF means none.
pub static CLUSTER_CH: AtomicU8 = AtomicU8::new(0xFF);

// ---------------------------------------------------------------------------
// The session (MOST)
// ---------------------------------------------------------------------------

/// The driver-verbosity and open-time fields a breaker trip touches.
pub struct Drv {
    /// Output open time in wall-clock seconds; zero means closed.
    pub open_time: i64,
    pub driver_quieted: bool,
}

/// The routing thread's per-session data the AU emitter uses.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent session state flags"
)]
pub struct Route {
    pub codec_cfg: [u8; CODEC_CFG_SIZE],
    pub codec_cfg_len: usize,
    pub au_buf: [u8; AU_BUF_SIZE],
    pub es_bytes: u32,
    pub pts_base_us: u64,
    pub pts_base_set: bool,
}

/// Output state protected by one mutex.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent session state flags"
)]
pub struct Most {
    pub drv: Drv,
    pub rt: Route,
    pub last_fail: i64,
    pub ui_last_start: i64,
    pub boot_wait_last_log: i64,
    pub defer_open_log: i64,
    pub boot_deferred: bool,
    pub dm_drain_done: bool,
    pub need_first_idr: bool,
    pub ui_flushed: bool,
    pub ui_frames: u32,
    pub idr_seen: u32,
    pub natural_idr: u32,
    pub last_frame_ms: u64,
    pub first_frame_ms: u64,
    pub setup_ms: u64,
    pub last_idr_ms: u64,
    pub max_gap_ms: u32,
    pub health_last_ms: u64,
    pub health_last_frames: u32,
    pub health_last_bytes: u32,
    pub idr_kick_time: i64,
    pub oob_inbound: u32,
    pub oob_last_log_ms: u64,
}

impl Most {
    #[allow(
        clippy::large_stack_arrays,
        reason = "evaluated at compile time for the MOST static only"
    )]
    const fn new() -> Most {
        Most {
            drv: Drv {
                open_time: 0,
                driver_quieted: false,
            },
            rt: Route {
                codec_cfg: [0; CODEC_CFG_SIZE],
                codec_cfg_len: 0,
                au_buf: [0; AU_BUF_SIZE],
                es_bytes: 0,
                pts_base_us: 0,
                pts_base_set: false,
            },
            last_fail: 0,
            ui_last_start: 0,
            boot_wait_last_log: 0,
            defer_open_log: 0,
            boot_deferred: false,
            dm_drain_done: false,
            need_first_idr: false,
            ui_flushed: false,
            ui_frames: 0,
            idr_seen: 0,
            natural_idr: 0,
            last_frame_ms: 0,
            first_frame_ms: 0,
            setup_ms: 0,
            last_idr_ms: 0,
            max_gap_ms: 0,
            health_last_ms: 0,
            health_last_frames: 0,
            health_last_bytes: 0,
            idr_kick_time: 0,
            oob_inbound: 0,
            oob_last_log_ms: 0,
        }
    }
}

/// Mutex protecting output state.
pub static MOST: Mutex<Most> = Mutex::new(Most::new());

// ---------------------------------------------------------------------------
// The muxer and the writes (TS)
// ---------------------------------------------------------------------------

/// The write side of the muxer: `most_write_block` per finished block.
pub struct Out {
    /// Whether the current access unit was abandoned on EAGAIN.
    pub write_abort: bool,
    /// Earliest time for the next UI block.
    pub ui_next_block_us: u64,
    /// Time used to limit write-error logs to one per second.
    pub werr_last_log: i64,
    /// True while the calling thread is the writer thread.
    on_writer: bool,
    /// A breaker trip requested inline (see the module docs). the rest of
    /// the call's blocks are dropped.
    pub trip: Option<Line<96>>,
}

/// Transport-stream state protected by one mutex.
pub struct Ts {
    pub mux: Mux,
    pub out: Out,
    /// Time when the current partial block received its first
    /// packet.
    pub block_pending_ms: u64,
}

fn new_mux() -> Mux {
    let mut m = Mux::new();
    m.pts_lead_ms = 0;
    m.pcr_placement = PcrPlacement::PesStart;
    m
}

/// A sink that counts the blocks it passes on.
struct Counted<'a, O: MuxIo> {
    inner: &'a mut O,
    n: u32,
}

impl<O: MuxIo> MuxIo for Counted<'_, O> {
    fn now90(&mut self) -> u64 {
        self.inner.now90()
    }
    fn emit(&mut self, block: &[u8]) {
        self.n += 1;
        self.inner.emit(block);
    }
}

/// One muxer call on any sink, keeping `pending_ms` as the time
/// when the current partial block received its first
/// packet (`now` when it was empty before, or when a block filled during
/// the call and packets were left over).
pub fn mux_op<O: MuxIo>(
    mux: &mut Mux,
    out: &mut O,
    pending_ms: &mut u64,
    now: u64,
    f: impl FnOnce(&mut Mux, &mut dyn MuxIo),
) {
    let was_pending = mux.block_pending();
    let mut c = Counted { inner: out, n: 0 };
    f(mux, &mut c);
    if mux.block_pending() && (!was_pending || c.n > 0) {
        *pending_ms = now;
    }
}

/// The muxing half of `emit_au_now` on any sink: PAT/PMT when asked, the
/// PES, then a partial block held at least `hold_ms`. `clock` is the millisecond clock (`now_ms`. a virtual
/// one in the gate test).
#[allow(clippy::too_many_arguments)]
pub fn mux_au<O: MuxIo>(
    mux: &mut Mux,
    out: &mut O,
    pending_ms: &mut u64,
    au: &[u8],
    pts_90k: u64,
    send_tables: bool,
    hold_ms: u64,
    clock: &dyn Fn() -> u64,
) {
    mux_op(mux, out, pending_ms, clock(), |m, o| {
        if send_tables {
            m.send_pat(o);
            m.send_pmt(o);
        }
        m.send_video(o, au, pts_90k);
    });
    if mux.block_pending()
        && hold_ms > 0
        && *pending_ms != 0
        && clock().wrapping_sub(*pending_ms) >= hold_ms
    {
        mux_op(mux, out, pending_ms, clock(), |m, o| m.flush_block(o));
    }
}

impl Ts {
    /// Run one muxer call and retain the partial block start time.
    pub fn op(&mut self, on_writer: bool, f: impl FnOnce(&mut Mux, &mut dyn MuxIo)) {
        self.out.on_writer = on_writer;
        mux_op(
            &mut self.mux,
            &mut self.out,
            &mut self.block_pending_ms,
            now_ms(),
            f,
        );
    }

    /// Reset continuity counters and discard the unsent partial block.
    pub fn reset_mux(&mut self) {
        self.mux = new_mux();
    }
}

static TS: OnceLock<Mutex<Ts>> = OnceLock::new();

/// The TS lock (created on first use. [`warm_up`] creates it in init).
pub fn ts() -> MutexGuard<'static, Ts> {
    lock(TS.get_or_init(|| {
        Mutex::new(Ts {
            mux: new_mux(),
            out: Out {
                write_abort: false,
                ui_next_block_us: 0,
                werr_last_log: 0,
                on_writer: false,
                trip: None,
            },
            block_pending_ms: 0,
        })
    }))
}

/// Writer-thread identity.
pub static WQ_RUNNING: AtomicBool = AtomicBool::new(false);
static WQ_HANDLE: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn timespec_diff(t0: qnx_time::Timespec, t1: qnx_time::Timespec) -> (i64, i64) {
    let ds = long(t1.tv_sec) - long(t0.tv_sec);
    let dn = long(t1.tv_nsec) - long(t0.tv_nsec);
    // Compute signed microsecond difference before converting to milliseconds.
    (ds * 1000 + dn / 1_000_000, ds * 1_000_000 + dn / 1000)
}

impl MuxIo for Out {
    fn now90(&mut self) -> u64 {
        0 // PesStart never reads the transmission clock
    }

    /// `most_write_block`.
    fn emit(&mut self, block: &[u8]) {
        let fd = MOST_FD.load(Ordering::Acquire);
        if fd < 0 || self.trip.is_some() {
            return;
        }
        if self.write_abort {
            return;
        }
        let ui_kbps = CFG.ui_pace_kbps();
        if SINK_UI.load(Ordering::Acquire) && ui_kbps > 0 {
            let mut now = mono_us();
            if self.ui_next_block_us > now {
                let mut wait = self.ui_next_block_us - now;
                while wait > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(wait.min(20_000)));
                    now = mono_us();
                    wait = self.ui_next_block_us.saturating_sub(now);
                }
            }
            now = mono_us();
            #[allow(clippy::cast_sign_loss)]
            let kbps = ui_kbps as u64;
            let spacing = (TS_BLOCK_SIZE as u64 * 8000).div_ceil(kbps);
            self.ui_next_block_us = now + spacing;
        }
        let t0 = qnx_time::now(qnx_time::CLOCK_MONOTONIC);
        WRITE_ENTER_MS.store(now_ms(), Ordering::Relaxed);
        WRITE_INFLIGHT.store(true, Ordering::Release);
        let r = mlb::write(fd, block);
        WRITE_INFLIGHT.store(false, Ordering::Release);
        WRITE_EXIT_MS.store(now_ms(), Ordering::Relaxed);
        let t1 = qnx_time::now(qnx_time::CLOCK_MONOTONIC);
        let (write_ms, _) = timespec_diff(t0, t1);
        match r {
            Err(e) => {
                let werr = MOST_WERR.fetch_add(1, Ordering::Relaxed) + 1;
                let consec = CB_WERR_CONSEC.fetch_add(1, Ordering::Relaxed) + 1;
                let now = wall_s();
                if werr == 1 || now != self.werr_last_log {
                    cl_loge!(
                        "[GalCluster] MLB write FAILED errno={} ({}) werr={} consec={} \u{2014} stream stalled?",
                        e,
                        sys::strerror_str(e),
                        werr,
                        consec
                    );
                    self.werr_last_log = now;
                }
            }
            Ok(n) => {
                #[allow(clippy::cast_possible_truncation)] // at most one block
                BYTES_WRITTEN.fetch_add(n as u32, Ordering::Relaxed);
                CB_WERR_CONSEC.store(0, Ordering::Relaxed);
                if n > 0 {
                    // Once per session.
                    crate::takeover::streaming();
                }
                if n != TS_BLOCK_SIZE {
                    let ws = MOST_WSHORT.fetch_add(1, Ordering::Relaxed) + 1;
                    let now = wall_s();
                    if ws == 1 || now != self.werr_last_log {
                        cl_loge!(
                            "[GalCluster] MLB SHORT write {} of {} wshort={}",
                            n,
                            TS_BLOCK_SIZE,
                            ws
                        );
                        self.werr_last_log = now;
                    }
                }
            }
        }
        self.breaker(write_ms);
    }
}

impl Out {
    /// Evaluate the circuit breaker after a write returns.
    fn breaker(&mut self, write_ms: i64) {
        if CFG.cb_enable() == 0 || CB_TRIPPED.load(Ordering::Acquire) {
            return;
        }
        let hard_ms = i64::from(CFG.cb_hard_stall_ms());
        let stall_ms = i64::from(CFG.cb_stall_ms());
        let hard = CFG.cb_hard_stall_ms() > 0 && write_ms >= hard_ms;
        if write_ms >= stall_ms {
            let c = CB_STALL_CONSEC.fetch_add(1, Ordering::Relaxed) + 1;
            let now = wall_s();
            if now != self.werr_last_log {
                #[allow(clippy::cast_sign_loss)]
                let count = CFG.cb_stall_count() as u32;
                cl_loge!(
                    "[GalCluster] MLB write STALLED {}ms (soft>={}ms hard>={}ms) count={}/{}{}",
                    write_ms,
                    CFG.cb_stall_ms(),
                    CFG.cb_hard_stall_ms(),
                    c,
                    count,
                    if hard { " HARD" } else { "" }
                );
                self.werr_last_log = now;
            }
        } else if CB_STALL_CONSEC.load(Ordering::Relaxed) > 0 {
            CB_STALL_CONSEC.fetch_sub(1, Ordering::Relaxed);
        }
        let mut why: Line<96> = Line::new();
        #[allow(clippy::cast_sign_loss)]
        if hard {
            let _ = write!(
                why,
                "single write {}ms >= hard {}ms",
                write_ms,
                CFG.cb_hard_stall_ms()
            );
        } else if CB_STALL_CONSEC.load(Ordering::Relaxed) >= CFG.cb_stall_count() as u32 {
            let _ = write!(
                why,
                "{} stalls >= {}ms (decaying count)",
                CB_STALL_CONSEC.load(Ordering::Relaxed),
                CFG.cb_stall_ms()
            );
        } else if CB_WERR_CONSEC.load(Ordering::Relaxed) >= CFG.cb_max_werr() as u32 {
            let _ = write!(
                why,
                "{} consecutive write() failures",
                CB_WERR_CONSEC.load(Ordering::Relaxed)
            );
        } else {
            return;
        }
        self.request_trip(why);
    }

    /// `cb_request_trip`: deferred to the routing thread from the writer;
    /// performed by the caller right after this mux call otherwise.
    fn request_trip(&mut self, why: Line<96>) {
        if self.on_writer {
            if !CB_TRIP_PENDING.load(Ordering::Acquire) {
                *lock(&CB_TRIP_WHY) = why;
                CB_TRIP_PENDING.store(true, Ordering::Release);
                cl_loge!(
                    "[GalCluster] breaker: trip REQUESTED from the writer thread ({}) \u{2014} deferred to the routing thread",
                    lock(&CB_TRIP_WHY).as_str()
                );
            }
            return;
        }
        self.trip = Some(why);
    }
}

/// The reason of a pending writer trip (taken with the flag cleared).
pub fn take_pending_trip() -> Line<96> {
    let g = lock(&CB_TRIP_WHY);
    let mut l: Line<96> = Line::new();
    let _ = l.write_str(g.as_str());
    l
}

/// Run a trip an inline mux call requested (see the module docs).
pub fn run_inline_trip(drv: &mut Drv, trip: Option<Line<96>>) {
    if let Some(why) = trip {
        cb_trip(drv, why.as_str());
    }
}

/// `mlb_set_verbose`.
pub fn set_verbose(fd: c_int, level: c_int) {
    let r = mlb::set_verbose(fd, level);
    cl_log!(
        "[GalCluster] driver verbose -> {} devctl r={}{}",
        level,
        r,
        if r != 0 {
            " (WARN \u{2014} spam not silenced)"
        } else {
            " ok"
        }
    );
}

/// `cb_trip`: hard-close isoTX2, park the phone's cluster stream, schedule
/// the resume. Called with [`MOST`] held.
pub fn cb_trip(drv: &mut Drv, why: &str) {
    let now = wall_s();
    let count = CB_TRIP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    CB_TRIPPED.store(true, Ordering::Release);
    CB_TRIP_TIME.store(now, Ordering::Relaxed);
    if count >= CFG.cb_max_trips() {
        CB_LOCKED_OUT.store(true, Ordering::Release);
    }
    let locked = CB_LOCKED_OUT.load(Ordering::Acquire);
    cl_loge!(
        "[GalCluster] *** isoTX2 CIRCUIT BREAKER TRIPPED (#{}/{}): {} \u{2014} hard-closing isoTX2 to protect the HU (staying on our context, NOT restoring nav){}",
        count,
        CFG.cb_max_trips(),
        why,
        if locked {
            " (session LOCKED OUT \u{2014} reconnect to retry)"
        } else {
            ""
        }
    );
    let fd = MOST_FD.load(Ordering::Acquire);
    if fd >= 0 {
        if drv.driver_quieted {
            set_verbose(fd, mlb::VERBOSE_STOCK);
            drv.driver_quieted = false;
        }
        let fr = mlb::flush_queue(fd);
        cl_loge!(
            "[GalCluster] cb_trip: FLUSH_QUEUE devctl r={} before close",
            fr
        );
        mlb::close(fd);
        MOST_FD.store(-1, Ordering::Release);
        drv.open_time = 0;
        SINK_UI.store(false, Ordering::Release);
    }
    CB_WERR_CONSEC.store(0, Ordering::Relaxed);
    CB_STALL_CONSEC.store(0, Ordering::Relaxed);
    let ch = CLUSTER_CH.load(Ordering::Acquire);
    if ch != 0xFF {
        route::send_video_focus_notification(
            ch,
            crate::sdr::FOCUS_NATIVE,
            crate::sdr::VFN_UNSOLICITED,
        );
        cl_log!(
            "[GalCluster] cb_trip: parked cluster stream (unsolicited VideoFocusNotification(NATIVE)){}",
            if locked {
                " \u{2014} locked out, no resume"
            } else {
                ""
            }
        );
        if !locked {
            route::cb_schedule_resume(ch);
        }
    }
}

// ---------------------------------------------------------------------------
// The writer ring and thread
// ---------------------------------------------------------------------------

pub const WQ_MAX_ITEMS: usize = 256;
pub const WQ_POOL_SIZE: usize = 2 * 1024 * 1024;

/// Access-unit ring shared with the writer.
pub struct Ring {
    pool: [u8; WQ_POOL_SIZE],
    off: [u32; WQ_MAX_ITEMS],
    len: [u32; WQ_MAX_ITEMS],
    pts: [u64; WQ_MAX_ITEMS],
    head: u32,
    tail: u32,
    pub count: u32,
    pub bytes: u32,
    wr: u32,
    pub drops: u32,
    pub peak: u32,
    pub oversize: u32,
    stop: bool,
}

pub static RING: Mutex<Ring> = Mutex::new(Ring {
    pool: [0; WQ_POOL_SIZE],
    off: [0; WQ_MAX_ITEMS],
    len: [0; WQ_MAX_ITEMS],
    pts: [0; WQ_MAX_ITEMS],
    head: 0,
    tail: 0,
    count: 0,
    bytes: 0,
    wr: 0,
    drops: 0,
    peak: 0,
    oversize: 0,
    stop: false,
});
static RING_COND: Condvar = Condvar::new();

impl Ring {
    /// Empty the ring (head = tail = count = bytes = wr = 0).
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.bytes = 0;
        self.wr = 0;
    }

    /// `wq_copy_out`.
    fn copy_out(&self, dst: &mut [u8], off: u32, len: u32) {
        let (off, len) = (off as usize, len as usize);
        let first = WQ_POOL_SIZE - off;
        if first >= len {
            dst[..len].copy_from_slice(&self.pool[off..off + len]);
        } else {
            dst[..first].copy_from_slice(&self.pool[off..]);
            dst[first..len].copy_from_slice(&self.pool[..len - first]);
        }
    }
}

/// Drain buffer used only by the writer.
static SCRATCH: Mutex<[u8; AU_BUF_SIZE]> = Mutex::new([0; AU_BUF_SIZE]);
static OVERSIZE_LOG_MS: AtomicU64 = AtomicU64::new(0);

/// Queue an access unit without blocking on device I/O.
pub fn wq_push(au: &[u8], pts: u64) -> bool {
    if au.is_empty() {
        return false;
    }
    let len = u32::try_from(au.len()).unwrap_or(u32::MAX);
    if au.len() > AU_BUF_SIZE {
        lock(&RING).oversize += 1;
        let n = now_ms();
        if n.wrapping_sub(OVERSIZE_LOG_MS.load(Ordering::Relaxed)) > 1000 {
            OVERSIZE_LOG_MS.store(n, Ordering::Relaxed);
            cl_loge!(
                "[GalCluster] access unit {} bytes > {}-byte writer scratch \u{2014} dropped; raise IDR_BUF_SIZE if the stream really is this big",
                len,
                AU_BUF_SIZE
            );
        }
        return false;
    }
    let mut r = lock(&RING);
    #[allow(clippy::cast_possible_truncation)]
    if r.count >= WQ_MAX_ITEMS as u32 || r.bytes as usize + au.len() > WQ_POOL_SIZE {
        r.drops += 1;
        return false;
    }
    let wr = r.wr as usize;
    let first = WQ_POOL_SIZE - wr;
    if first >= au.len() {
        r.pool[wr..wr + au.len()].copy_from_slice(au);
    } else {
        r.pool[wr..].copy_from_slice(&au[..first]);
        r.pool[..au.len() - first].copy_from_slice(&au[first..]);
    }
    let t = r.tail as usize;
    r.off[t] = r.wr;
    r.len[t] = len;
    r.pts[t] = pts;
    #[allow(clippy::cast_possible_truncation)]
    {
        r.tail = (r.tail + 1) % WQ_MAX_ITEMS as u32;
        r.wr = ((r.wr as usize + au.len()) % WQ_POOL_SIZE) as u32;
    }
    r.bytes += len;
    r.count += 1;
    if r.count > r.peak {
        r.peak = r.count;
    }
    // Arm the focus kick on a real backlog, after the grace window.
    let c = &CFG;
    #[allow(clippy::cast_sign_loss)]
    if c.focus_kick() != 0
        && c.focus_kick_wq() > 0
        && r.count >= c.focus_kick_wq() as u32
        && !FK_ARMED.load(Ordering::Relaxed)
    {
        let stream = FK_STREAM_MS.load(Ordering::Relaxed);
        if stream != 0 && now_ms().wrapping_sub(stream) >= c.focus_kick_grace() as u64 {
            FK_ARMED.store(true, Ordering::Relaxed);
        }
    }
    RING_COND.notify_one();
    true
}

/// `emit_au_now`: mux and write one AU. Caller holds the TS lock.
pub fn emit_au_now(ts: &mut Ts, on_writer: bool, au: &[u8], pts_90k: u64) {
    ts.out.write_abort = false;
    let frame_count = FRAME_COUNT.load(Ordering::Relaxed);
    let send_tables = !TABLES_SENT.load(Ordering::Relaxed)
        || crate::h264::is_idr(au)
        || frame_count.is_multiple_of(LOG_PERIOD_FRAMES);
    // Decide table emission before the access unit. Record completion after its table
    // packets and before later writes.
    let hold = u64::try_from(CFG.flush_max_hold_ms()).unwrap_or(0);
    ts.out.on_writer = on_writer;
    let Ts {
        mux,
        out,
        block_pending_ms,
    } = ts;
    mux_au(
        mux,
        out,
        block_pending_ms,
        au,
        pts_90k,
        send_tables,
        hold,
        &now_ms,
    );
    if send_tables {
        TABLES_SENT.store(true, Ordering::Relaxed);
    }
    FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Block every signal on a thread of ours: gal's USB reads are
/// EINTR-sensitive.
fn block_signals() {
    let all = [u32::MAX; 2];
    // SAFETY: valid set pointers.
    unsafe {
        sys::sig::pthread_sigmask(sys::sig::SIG_BLOCK, &raw const all, core::ptr::null_mut())
    };
}

/// One drained frame: pace it, then mux and write it (unless the UI took
/// the pipe meanwhile).
fn writer_one(
    scratch: &mut [u8; AU_BUF_SIZE],
    pace: &mut (u64, u64, bool),
    cv: &mut crate::cvs::Writer,
) -> bool {
    let (len, pts, depth) = {
        let mut r = lock(&RING);
        while r.count == 0 && !r.stop {
            r = RING_COND.wait(r).unwrap_or_else(PoisonError::into_inner);
        }
        if r.count == 0 && r.stop {
            return false;
        }
        let h = r.head as usize;
        let (len, pts, off) = (r.len[h], r.pts[h], r.off[h]);
        let depth = r.count;
        r.copy_out(&mut scratch[..], off, len);
        #[allow(clippy::cast_possible_truncation)]
        {
            r.head = (r.head + 1) % WQ_MAX_ITEMS as u32;
        }
        r.bytes -= len;
        r.count -= 1;
        (len, pts, depth)
    };
    if CFG.output() == 2 {
        if CV_LOSS.swap(false, Ordering::AcqRel) {
            cv.invalidate();
        }
        let au = &scratch[..len as usize];
        match cv.send(
            au,
            crate::h264::is_idr(au),
            || lock(&RING).stop,
            |port| {
                let helper = crate::cvs::helper_path(std::path::Path::new(
                    gal_feature_api::location::dir(),
                ))?;
                crate::cvs::start(&helper, port).map(|_| ())
            },
        ) {
            Ok(true) => {
                FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(error) => {
                CV_NEED_IDR.store(true, Ordering::Release);
                lock(&RING).clear();
                cl_loge!("[GalCluster] CVS1 write failed: {}; waiting for IDR", error);
            }
        }
        return true;
    }
    let pct = CFG.write_pace_pct();
    if pct > 0 {
        let (prev_line, prev_pts, valid) = *pace;
        let tnow = now_ms();
        let mut line = tnow;
        if valid && pts >= prev_pts {
            #[allow(clippy::cast_sign_loss)]
            let budget = ((pts - prev_pts) / 90) * 100 / pct as u64;
            line = prev_line + budget;
            if line > tnow {
                let mut wait = line - tnow;
                #[allow(clippy::cast_sign_loss)]
                let max_wait = CFG.write_pace_max_wait_ms() as u64;
                if wait > max_wait {
                    cl_log!(
                        "[GalCluster] write pace: {}ms wait > max {}ms (pts jump?) \u{2014} emitting now",
                        ul(wait),
                        CFG.write_pace_max_wait_ms()
                    );
                    line = tnow;
                } else {
                    static PACE_LOG_LAST: AtomicU64 = AtomicU64::new(0);
                    if depth >= 8 && tnow.wrapping_sub(PACE_LOG_LAST.load(Ordering::Relaxed)) > 1000
                    {
                        PACE_LOG_LAST.store(tnow, Ordering::Relaxed);
                        cl_log!(
                            "[GalCluster] write pace: wq={} behind \u{2014} holding {}ms (pace={}%)",
                            depth,
                            ul(wait),
                            pct
                        );
                    }
                    PACE_FRAMES.fetch_add(1, Ordering::Relaxed);
                    PACE_WAIT_MS.fetch_add(ul(wait), Ordering::Relaxed);
                    while wait > 0 && !lock(&RING).stop {
                        let slice = wait.min(20);
                        std::thread::sleep(std::time::Duration::from_millis(slice));
                        wait -= slice;
                    }
                }
            } else {
                line = tnow;
            }
        }
        *pace = (line, pts, true);
    }
    let mut ts = ts();
    if !SINK_UI.load(Ordering::Acquire) {
        emit_au_now(&mut ts, true, &scratch[..len as usize], pts);
    }
    true
}

/// `wq_writer_main`.
fn writer_main() {
    block_signals();
    let mut scratch = lock(&SCRATCH);
    let mut pace = (0u64, 0u64, false);
    let mut cv = crate::cvs::Writer::new(crate::cvs::PORT);
    loop {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer_one(&mut scratch, &mut pace, &mut cv)
        }));
        match r {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => {
                cl_loge!("[GalCluster] PANIC contained in the writer thread; the frame is dropped");
            }
        }
    }
}

/// `wq_start`: bring up the writer (once per stream).
pub fn writer_available() -> bool {
    !WRITER_FAILED.load(Ordering::Acquire)
}

static WRITER_FAILED: AtomicBool = AtomicBool::new(false);

pub fn wq_start() {
    if !writer_available() || WQ_RUNNING.load(Ordering::Acquire) {
        return;
    }
    {
        let mut r = lock(&RING);
        r.clear();
        r.drops = 0;
        r.peak = 0;
        r.oversize = 0;
        r.stop = false;
    }
    PACE_FRAMES.store(0, Ordering::Relaxed);
    PACE_WAIT_MS.store(0, Ordering::Relaxed);
    match std::thread::Builder::new()
        .name("cluster-writer".into())
        .stack_size(128 * 1024)
        .spawn(writer_main)
    {
        Ok(h) => {
            *lock(&WQ_HANDLE) = Some(h);
        }
        Err(e) => {
            cl_loge!(
                "[GalCluster] writer thread: pthread_create FAILED errno={} \u{2014} falling back to inline writes",
                e.raw_os_error().unwrap_or(0)
            );
            WRITER_FAILED.store(true, Ordering::Release);
            return;
        }
    }
    WQ_RUNNING.store(true, Ordering::Release);
    cl_log!(
        "[GalCluster] writer thread started (ring {} items / {}KB, pace={}% max_wait={}ms) \u{2014} the routing thread no longer blocks on isoTX2 writes",
        WQ_MAX_ITEMS,
        WQ_POOL_SIZE / 1024,
        CFG.write_pace_pct(),
        CFG.write_pace_max_wait_ms()
    );
}

/// `wq_stop`: drain and join. Before the fd is closed.
pub fn wq_stop() {
    if !WQ_RUNNING.load(Ordering::Acquire) {
        return;
    }
    {
        let mut r = lock(&RING);
        r.stop = true;
        if CFG.output() == 2 {
            r.clear();
        }
        RING_COND.notify_one();
    }
    if let Some(h) = lock(&WQ_HANDLE).take() {
        let _ = h.join();
    }
    WQ_RUNNING.store(false, Ordering::Release);
    let (peak, drops) = {
        let r = lock(&RING);
        (r.peak, r.drops)
    };
    cl_log!(
        "[GalCluster] writer thread stopped (peak depth {} frames, {} dropped on a full ring, paced {} frames / {}ms total)",
        peak,
        drops,
        PACE_FRAMES.load(Ordering::Relaxed),
        PACE_WAIT_MS.load(Ordering::Relaxed)
    );
}

// ---------------------------------------------------------------------------
// Open and close
// ---------------------------------------------------------------------------

/// `most_open`: 0 when isoTX2 is open (now or already), -1 when not yet.
/// Called with [`MOST`] held.
#[allow(clippy::too_many_lines)]
pub fn most_open(most: &mut Most) -> i32 {
    let c = &CFG;
    if c.output() == 2 {
        return -1;
    }
    let fd0 = MOST_FD.load(Ordering::Acquire);
    if fd0 >= 0 {
        return 0;
    }
    if c.cb_enable() != 0
        && (CB_LOCKED_OUT.load(Ordering::Acquire) || CB_TRIPPED.load(Ordering::Acquire))
    {
        let now = wall_s();
        if !CB_LOCKED_OUT.load(Ordering::Acquire)
            && now - CB_TRIP_TIME.load(Ordering::Relaxed) >= i64::from(c.cb_cooldown_secs())
        {
            CB_TRIPPED.store(false, Ordering::Release);
            cl_log!(
                "[GalCluster] circuit-breaker cooldown elapsed ({}s) \u{2014} re-arming, will retry isoTX2",
                c.cb_cooldown_secs()
            );
        } else {
            most.last_fail = now;
            if now != most.boot_wait_last_log {
                most.boot_wait_last_log = now;
                cl_log!(
                    "[GalCluster] circuit-breaker {} \u{2014} nav stays on cluster, dropping AA frames",
                    if CB_LOCKED_OUT.load(Ordering::Acquire) {
                        "LOCKED OUT (session)"
                    } else {
                        "cooling down"
                    }
                );
            }
            return -1;
        }
    }
    if most.last_fail != 0 && wall_s() == most.last_fail {
        return -1;
    }
    if c.boot_settle_secs() > 0 {
        let mono = qnx_time::now(qnx_time::CLOCK_MONOTONIC);
        let up = long(mono.tv_sec);
        // the uptime gate, or the HMI handover (gate::boot_settling)
        if gate::boot_settling(mono) {
            let now = wall_s();
            most.last_fail = now;
            most.boot_deferred = true;
            if now != most.boot_wait_last_log {
                most.boot_wait_last_log = now;
                cl_log!(
                    "[GalCluster] deferring cluster start \u{2014} HU still booting (uptime {}s < settle {}s); nav stays until then",
                    up,
                    c.boot_settle_secs()
                );
            }
            return -1;
        }
    }
    // Resolve control only after boot settling, as the normal handover did.
    // Never open a writer when navigation cannot be handed over.
    if !dm::ready() {
        return -1;
    }
    if c.gate_enable() != 0 {
        gate::helper_launch();
        let fresh = dm::hide_nav();
        if fresh {
            gate::ENCODER_LIVE.store(true, Ordering::Relaxed);
            gate::ENCODER_SEEN.store(false, Ordering::Relaxed);
            cl_log!("[GalCluster] nav hidden \u{2014} holding the isoTX2 open until DM's encoder reports DOWN (deferred-open)");
        }
        if gate::encoder_live() {
            dm::reassert_hide();
            let nw = wall_s();
            if nw != most.defer_open_log {
                most.defer_open_log = nw;
                cl_log!(
                    "[GalCluster] deferred-open: MPE encoder still UP \u{2014} not opening isoTX2 yet (seen={})",
                    i32::from(gate::ENCODER_SEEN.load(Ordering::Relaxed))
                );
            }
            return -1;
        }
        cl_log!("[GalCluster] deferred-open: MPE encoder DOWN \u{2014} opening isoTX2 now");
    }
    let nonblock = false;
    let opened = mlb::open(nonblock);
    let fd = match opened {
        Ok(fd) => fd,
        Err(e) => {
            most.last_fail = wall_s();
            cl_loge!(
                "[GalCluster] open({}) O_WRONLY failed errno={} ({}) \u{2014} retry 1s",
                mlb::DEVICE.to_str().unwrap_or("?"),
                e,
                sys::strerror_str(e)
            );
            return -1;
        }
    };
    if c.quiet_driver() != 0 {
        set_verbose(fd, mlb::VERBOSE_QUIET);
        most.drv.driver_quieted = true;
    }
    gate::helper_launch();
    let fresh_hide = dm::hide_nav();
    if gate::encoder_live() {
        gate::mark_busy();
        gate::ENCODER_LIVE.store(true, Ordering::Relaxed);
        gate::ENCODER_SEEN.store(true, Ordering::Relaxed);
        cl_log!(
            "[GalCluster] MPE encoder is UP \u{2014} marked BUSY, waiting for {} close",
            if fresh_hide {
                "the hide's"
            } else {
                "an in-flight"
            }
        );
    } else {
        cl_log!(
            "[GalCluster] MPE encoder already DOWN at open ({}) \u{2014} not marking BUSY; no encoder-close is coming to clear it",
            if fresh_hide {
                "hide issued anyway"
            } else {
                "nav already hidden"
            }
        );
    }
    most.last_fail = 0;
    MOST_FD.store(fd, Ordering::Release);
    let now = wall_s();
    most.drv.open_time = now;
    most.ui_last_start = now;
    FK_STREAM_MS.store(now_ms(), Ordering::Relaxed);
    most.dm_drain_done = false;
    most.rt.es_bytes = 0;
    most.need_first_idr = CB_TRIP_COUNT.load(Ordering::Relaxed) > 0;
    if most.need_first_idr {
        cl_log!("[GalCluster] breaker recovery: holding writes until a fresh IDR (mid-stream resume, next frames are P-frames)");
    }
    ts().reset_mux();
    TABLES_SENT.store(false, Ordering::Relaxed);
    FRAME_COUNT.store(0, Ordering::Relaxed);
    CB_WERR_CONSEC.store(0, Ordering::Relaxed);
    CB_STALL_CONSEC.store(0, Ordering::Relaxed);
    BYTES_WRITTEN.store(0, Ordering::Relaxed);
    MOST_WERR.store(0, Ordering::Relaxed);
    MOST_WSHORT.store(0, Ordering::Relaxed);
    most.idr_seen = 0;
    most.natural_idr = 0;
    most.last_frame_ms = 0;
    most.first_frame_ms = 0;
    most.last_idr_ms = 0;
    most.max_gap_ms = 0;
    most.health_last_ms = now_ms();
    most.health_last_frames = 0;
    most.health_last_bytes = 0;
    cl_log!(
        "[GalCluster] MLB {} opened fd={} (O_WRONLY write-fd + FLUSH+START init-fd)",
        mlb::DEVICE.to_str().unwrap_or("?"),
        fd
    );
    CB_TRIP_PENDING.store(false, Ordering::Release);
    FK_ARMED.store(false, Ordering::Relaxed);
    FK_STATE.store(0, Ordering::Relaxed);
    FK_COUNT.store(0, Ordering::Relaxed);
    FK_EXPECT_RESTART.store(false, Ordering::Relaxed);
    FK_DRAINED_MS.store(0, Ordering::Relaxed);
    FK_LAST_MS.store(0, Ordering::Relaxed);
    FK_STREAM_MS.store(0, Ordering::Relaxed);
    wq_start();
    0
}

/// `most_close`. Called with [`MOST`] held.
pub fn most_close(most: &mut Most) {
    wq_stop();
    if CFG.output() == 2 {
        CV_NEED_IDR.store(true, Ordering::Release);
        CV_LOSS.store(false, Ordering::Release);
        most.rt.codec_cfg_len = 0;
        most.rt.pts_base_set = false;
        FRAME_COUNT.store(0, Ordering::Relaxed);
        return;
    }
    let fd = MOST_FD.load(Ordering::Acquire);
    if fd < 0 {
        // A gated startup may hide navigation without ever opening isoTX2.
        // Channel teardown must discharge that hide too.
        if !FK_EXPECT_RESTART.load(Ordering::Acquire) {
            dm::restore_nav();
        }
        return;
    }
    let up = if most.drv.open_time != 0 {
        let first = if most.first_frame_ms != 0 {
            most.first_frame_ms
        } else {
            now_ms()
        };
        now_ms().wrapping_sub(first)
    } else {
        0
    };
    cl_log!(
        "[GalCluster] MLB closing session summary: frames={} idr={}(natural={}) bytes={}KB werr={} wshort={} maxgap={}ms streamed={}s fd={}",
        FRAME_COUNT.load(Ordering::Relaxed),
        most.idr_seen,
        most.natural_idr,
        BYTES_WRITTEN.load(Ordering::Relaxed) / 1024,
        MOST_WERR.load(Ordering::Relaxed),
        MOST_WSHORT.load(Ordering::Relaxed),
        most.max_gap_ms,
        ul(up / 1000),
        fd
    );
    let trip = {
        let mut t = ts();
        t.out.trip = None;
        t.op(false, |m, o| m.flush_block(o));
        t.out.trip.take()
    };
    run_inline_trip(&mut most.drv, trip);
    let fd = MOST_FD.load(Ordering::Acquire);
    if most.drv.driver_quieted {
        set_verbose(fd, mlb::VERBOSE_STOCK);
        most.drv.driver_quieted = false;
    }
    mlb::close(fd);
    MOST_FD.store(-1, Ordering::Release);
    TABLES_SENT.store(false, Ordering::Relaxed);
    FRAME_COUNT.store(0, Ordering::Relaxed);
    most.drv.open_time = 0;
    most.rt.pts_base_set = false;
    most.dm_drain_done = false;
    most.need_first_idr = false;
    SINK_UI.store(false, Ordering::Release);
    most.ui_flushed = false;
    if FK_EXPECT_RESTART.load(Ordering::Acquire) {
        cl_log!("[GalCluster] MLB closed mid-recovery \u{2014} KEEPING the cluster context (no nav restore); waiting for the phone to re-START");
        return;
    }
    cl_log!("[GalCluster] MLB closed, restoring nav encoder");
    dm::restore_nav();
}

/// Create the TS lock and its muxer on the init thread (not on gal's).
pub fn warm_up() {
    drop(ts());
}

#[cfg(all(test, not(target_os = "nto")))]
pub(crate) fn hold_writer_for_test() -> std::sync::mpsc::Sender<()> {
    let (release, held) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        // An owned host fixture stands in for a write that has not returned.
        // A dropped sender or timeout also releases it on test failure.
        let _ = held.recv_timeout(std::time::Duration::from_secs(2));
    });
    *lock(&WQ_HANDLE) = Some(worker);
    WQ_RUNNING.store(true, Ordering::Release);
    release
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects every block the muxer emits (no fd).
    struct Collect(Vec<u8>);
    impl MuxIo for Collect {
        fn now90(&mut self) -> u64 {
            0
        }
        fn emit(&mut self, b: &[u8]) {
            self.0.extend_from_slice(b);
        }
    }

    #[test]
    fn writer_rejects_empty_and_oversize_units_before_copying() {
        let before = {
            let ring = lock(&RING);
            (ring.count, ring.bytes, ring.wr, ring.oversize)
        };
        assert!(!wq_push(&[], 0));
        assert!(!wq_push(&vec![0; AU_BUF_SIZE + 1], 0));
        let ring = lock(&RING);
        assert_eq!(
            (ring.count, ring.bytes, ring.wr),
            (before.0, before.1, before.2)
        );
        assert_eq!(ring.oversize, before.3 + 1);
    }

    #[test]
    fn the_mux_is_configured_as_gals_stream() {
        let mut m = new_mux();
        let mut c = Collect(Vec::new());
        m.send_pat(&mut c);
        m.send_pmt(&mut c);
        m.send_video(&mut c, &[0, 0, 0, 1, 0x65, 0x88], 0x1234);
        m.flush_block(&mut c);
        assert_eq!(c.0.len(), TS_BLOCK_SIZE);
        // packet 2: the PES start with the PCR in its adaptation field
        let p = &c.0[2 * 188..3 * 188];
        assert_eq!(&p[..6], &[0x47, 0x41, 0x01, 0x30, 7 + 176 - 20, 0x10]);
        // PTS == PCR (pts_lead_ms 0): PTS 0x1234 in the PES header
        let pes = &p[4 + 1 + p[4] as usize..];
        assert_eq!(&pes[..4], &[0, 0, 1, 0xE0]);
        assert_eq!(pes[13], 0x01 | ((0x1234u64 << 1) & 0xFE) as u8);
        // the rest of the block is null packets
        assert_eq!(&c.0[3 * 188..3 * 188 + 4], &[0x47, 0x1F, 0xFF, 0x10]);
    }

    #[test]
    fn the_pending_time_follows_the_first_packet() {
        let mut ts = Ts {
            mux: new_mux(),
            out: Out {
                write_abort: false,
                ui_next_block_us: 0,
                werr_last_log: 0,
                on_writer: false,
                trip: None,
            },
            block_pending_ms: 0,
        };
        ts.op(false, |m, o| m.send_pat(o));
        assert!(ts.mux.block_pending());
        assert_ne!(ts.block_pending_ms, 0);
        ts.block_pending_ms = 1;
        ts.op(false, |m, o| m.send_pmt(o));
        assert_eq!(ts.block_pending_ms, 1, "same block: unchanged");
        ts.reset_mux();
        assert!(!ts.mux.block_pending());
    }

    #[test]
    fn write_ms_subtracts_elapsed_time() {
        let t0 = qnx_time::Timespec {
            tv_sec: 10,
            tv_nsec: 900_000_000,
        };
        let t1 = qnx_time::Timespec {
            tv_sec: 11,
            tv_nsec: 100_000_000,
        };
        assert_eq!(timespec_diff(t0, t1), (200, 200_000));
    }
}
