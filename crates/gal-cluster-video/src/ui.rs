// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Encoded UI access-unit sink. It borrows the same MOST pipe and acknowledges but
//! drops phone video while it holds the pipe. These Rust functions are available only
//! after this cluster module starts. They do not enable a menu feature.

use core::sync::atomic::{AtomicBool, Ordering};

use most_isotx2 as mlb;

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge};
use crate::pipe::{self, lock, MOST};
use crate::route;

/// Set when the loader starts the cluster module.
static LOADED: AtomicBool = AtomicBool::new(false);

/// Return whether the loader started this cluster module.
#[must_use]
pub fn loaded() -> bool {
    LOADED.load(Ordering::Acquire)
}

pub(crate) fn mark_loaded() {
    LOADED.store(true, Ordering::Release);
}

/// `gal_cluster_ui_open`: take the pipe. 0, or `most_open`'s error.
pub fn ui_open() -> i32 {
    let mut most = lock(&MOST);
    let r = pipe::most_open(&mut most);
    if r != 0 {
        drop(most);
        cl_loge!("[GalCluster] UI sink OPEN failed (most_open r={})", r);
        return r;
    }
    let discarded = {
        let mut ts = pipe::ts();
        pipe::SINK_UI.store(true, Ordering::Release);
        most.ui_flushed = false;
        most.ui_frames = 0;
        ts.out.ui_next_block_us = 0;
        let mut ring = lock(&pipe::RING);
        let n = ring.count;
        ring.clear();
        n
    };
    drop(most);
    cl_log!(
        "[GalCluster] UI sink OPEN (owner=UI, most_open r={}, discarded_aa={}, pace={}kbps)",
        r,
        discarded,
        CFG.ui_pace_kbps()
    );
    r
}

/// `gal_cluster_ui_submit`: one self-contained H.264 AU. 1 written, 0 still
/// in the DM drain, -1 when the UI does not own an open pipe.
pub fn ui_submit(au: &[u8]) -> i32 {
    if au.is_empty() {
        return -1;
    }
    let mut most = lock(&MOST);
    let fd = pipe::MOST_FD.load(Ordering::Acquire);
    if !pipe::SINK_UI.load(Ordering::Acquire) || fd < 0 {
        return -1;
    }
    let now = qnx_time::now(qnx_time::CLOCK_REALTIME);
    let now_s = crate::log::long(now.tv_sec);
    if !most.ui_flushed {
        let r = mlb::flush_queue(fd);
        pipe::ts().reset_mux();
        most.ui_flushed = true;
        cl_log!(
            "[GalCluster] UI first frame: post-drain FLUSH r={}{}",
            r,
            if r != 0 { " (WARN)" } else { " ok" }
        );
    }
    let pts = u64::try_from(now.tv_sec).unwrap_or(0) * 90000
        + u64::try_from(now.tv_nsec / 1_000_000).unwrap_or(0) * 90;
    let trip = {
        let mut ts = pipe::ts();
        ts.out.trip = None;
        ts.op(false, |m, o| {
            m.send_pat(o);
            m.send_pmt(o);
            m.send_video(o, au, pts);
            m.flush_block(o);
        });
        ts.out.trip.take()
    };
    pipe::run_inline_trip(&mut most.drv, trip);
    if now_s - most.ui_last_start >= 1 {
        let mut num_max = crate::cfg::TX_QUEUE_PACKETS;
        let _ = mlb::start(pipe::MOST_FD.load(Ordering::Acquire), &mut num_max);
        most.ui_last_start = now_s;
    }
    most.ui_frames += 1;
    if most.ui_frames == 1 || most.ui_frames.is_multiple_of(30) {
        cl_log!("[GalCluster] UI frame #{} len={}", most.ui_frames, au.len());
    }
    1
}

/// `gal_cluster_ui_close`: hand back to AA (flush, fresh IDR) when its
/// session is live, else close the pipe and restore nav.
pub fn ui_close() {
    let mut most = lock(&MOST);
    if !pipe::SINK_UI.load(Ordering::Acquire) {
        return;
    }
    let ch = pipe::CLUSTER_CH.load(Ordering::Acquire);
    let fd = pipe::MOST_FD.load(Ordering::Acquire);
    {
        let mut ts = pipe::ts();
        pipe::SINK_UI.store(false, Ordering::Release);
        most.ui_flushed = false;
        ts.out.ui_next_block_us = 0;
        if route::started() && fd >= 0 && ch != 0xFF {
            let r = mlb::flush_queue(fd);
            ts.reset_mux();
            pipe::TABLES_SENT.store(false, Ordering::Relaxed);
            drop(ts);
            cl_log!(
                "[GalCluster] UI sink CLOSE -> hand back to AA (flush r={}, kicking fresh IDR)",
                r
            );
            route::kick_fresh_idr(&mut most, ch);
            return;
        }
    }
    pipe::most_close(&mut most);
    drop(most);
    cl_log!("[GalCluster] UI sink CLOSE -> nav restored");
}
