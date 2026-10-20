// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Runtime layout, timing and safety choices for cluster video.
//! Values are atomics because the loader initializes them before gal and
//! worker threads use them.

use core::ffi::CStr;
use core::sync::atomic::{AtomicI32, Ordering};

use gal_feature_api::config;

use crate::log::{cl_log, cl_loge};

macro_rules! cfg_struct {
    ($( $(#[$doc:meta])* $name:ident = $def:expr ),* $(,)?) => {
        /// The configuration (see the module docs).
        pub struct Cfg {
            $( $(#[$doc])* pub $name: AtomicI32, )*
        }
        impl Cfg {
            const fn defaults() -> Cfg {
                Cfg { $( $name: AtomicI32::new($def), )* }
            }
            $(
                #[inline]
                pub fn $name(&self) -> i32 {
                    self.$name.load(Ordering::Relaxed)
                }
            )*
        }
    };
}

cfg_struct! {
    /// 0: direct MOST, 2: explicitly selected cluster-video CVS1 sink.
    output = 0,
    /// `cluster_codec_resolution`: AA `VideoCodecResolutionType` offered for
    /// the cluster (1 = 800x480, 2 = 1280x720). touch follows it.
    codec_res = 1,
    /// `cluster_sps_crop`: rewrite the SPS crop to the content window.
    sps_crop = 0,
    width_margin = 270,
    height_margin = 344,
    density = 180,
    main_dpi = 0,
    main_fps = 0,
    cluster_max_unacked = 1,
    viewing_dist = 100,
    margin_top = 60,
    margin_bottom = 96,
    margin_left = 48,
    margin_right = 48,
    content_inset_top = 0,
    content_inset_bottom = 0,
    content_inset_left = 64,
    content_inset_right = 64,
    stable_inset_top = 0,
    stable_inset_bottom = 0,
    stable_inset_left = 0,
    stable_inset_right = 0,
    quiet_driver = 1,
    dm_display = 4,
    dm_table = 70,
    dm_hide_ctx = -9,
    ready_settle_ms = 5000,
    handover_settle_ms = 500,
    main_focus_wait_ms = 8000,
    main_focus_extra_ms = 750,
    boot_settle_secs = 90,
    flush_max_hold_ms = 100,
    focus_kick = 0,
    focus_kick_wq = 16,
    focus_kick_settle = 1500,
    focus_kick_grace = 15000,
    focus_kick_min_ms = 20000,
    focus_kick_max = 3,
    sink_feedback = 1,
    write_pace_pct = 150,
    write_pace_max_wait_ms = 1000,
    ui_pace_kbps = 9600,
    cb_enable = 1,
    cb_hard_stall_ms = 1000,
    cb_stall_ms = 400,
    cb_stall_count = 3,
    cb_max_werr = 6,
    cb_cooldown_secs = 60,
    cb_max_trips = 3,
    gate_enable = 1,
}

/// 3 x 64 packets: the START argument. The driver keeps its own depth
/// (64 packets, its `-P64`) independently of this value.
pub const TX_QUEUE_PACKETS: i32 = 64 * 3;

pub static CFG: Cfg = Cfg::defaults();

impl Cfg {
    fn set(v: &AtomicI32, x: i32) {
        v.store(x, Ordering::Relaxed);
    }

    /// The four content insets, top/bottom/left/right.
    pub fn content_insets(&self) -> [u32; 4] {
        [
            self.content_inset_top(),
            self.content_inset_bottom(),
            self.content_inset_left(),
            self.content_inset_right(),
        ]
        .map(as_u32)
    }

    /// The four stable insets, top/bottom/left/right.
    pub fn stable_insets(&self) -> [u32; 4] {
        [
            self.stable_inset_top(),
            self.stable_inset_bottom(),
            self.stable_inset_left(),
            self.stable_inset_right(),
        ]
        .map(as_u32)
    }
}

/// Read a stored unsigned 32-bit setting.
#[allow(clippy::cast_sign_loss)]
pub fn as_u32(v: i32) -> u32 {
    v as u32
}

/// `gal_feature_api::config::get_int(key, def)`.
pub fn get_int(key: &CStr, def: i32) -> i32 {
    config::get_int(key, def)
}

/// A negative value keeps the default. The default goes to the
/// reader as `(int)def`.
#[allow(clippy::cast_possible_wrap)]
fn cfg_u32(key: &CStr, def: u32) -> u32 {
    let v = get_int(key, def as i32);
    if v < 0 {
        def
    } else {
        as_u32(v)
    }
}

/// Store a nonnegative setting in an atomic value.
#[allow(clippy::cast_possible_wrap)]
fn load_u32(field: &AtomicI32, key: &CStr) {
    let cur = as_u32(field.load(Ordering::Relaxed));
    Cfg::set(field, cfg_u32(key, cur) as i32);
}

/// `gal_feature_api::config::get_int(key, <current value>)` into an atomic.
fn load_i32(field: &AtomicI32, key: &CStr) {
    let cur = field.load(Ordering::Relaxed);
    Cfg::set(field, get_int(key, cur));
}

/// `gal_feature_api::config::get_int(key, def)` into an atomic.
fn load_def(field: &AtomicI32, key: &CStr, def: i32) {
    Cfg::set(field, get_int(key, def));
}

/// Read cluster settings into the shared configuration.
#[allow(clippy::too_many_lines)]
pub fn load() {
    let c = &CFG;
    load_def(&c.codec_res, c"cluster_codec_resolution", 1);
    if !(1..=2).contains(&c.codec_res()) {
        cl_loge!(
            "[GalCluster] cluster_codec_resolution={} unsupported (1 = 800x480, 2 = 1280x720) - using 1",
            c.codec_res()
        );
        Cfg::set(&c.codec_res, 1);
    }
    load_def(&c.sps_crop, c"cluster_sps_crop", 0);
    load_u32(&c.width_margin, c"cluster_width_margin");
    load_u32(&c.height_margin, c"cluster_height_margin");
    load_u32(&c.density, c"cluster_density");
    load_u32(&c.main_dpi, c"main_dpi");
    load_u32(&c.main_fps, c"main_fps");
    let fps = as_u32(c.main_fps());
    if fps != 0 && fps != 30 {
        cl_loge!(
            "[GalCluster] main_fps={} unsupported (only 30 or 0) - ignored",
            fps
        );
        Cfg::set(&c.main_fps, 0);
    }
    load_u32(&c.cluster_max_unacked, c"cluster_max_unacked");
    let mu = as_u32(c.cluster_max_unacked());
    if !(1..=64).contains(&mu) {
        cl_loge!(
            "[GalCluster] cluster_max_unacked={} out of range 1..64 - using 1",
            mu
        );
        Cfg::set(&c.cluster_max_unacked, 1);
    }
    load_u32(&c.viewing_dist, c"cluster_viewing_distance");
    load_i32(&c.margin_top, c"cluster_margin_top");
    load_i32(&c.margin_bottom, c"cluster_margin_bottom");
    load_i32(&c.margin_left, c"cluster_margin_left");
    load_i32(&c.margin_right, c"cluster_margin_right");
    load_u32(&c.content_inset_top, c"cluster_content_inset_top");
    load_u32(&c.content_inset_bottom, c"cluster_content_inset_bottom");
    load_u32(&c.content_inset_left, c"cluster_content_inset_left");
    load_u32(&c.content_inset_right, c"cluster_content_inset_right");
    // cfg_u32(key, 0u): the default is 0, not the current value.
    for (f, k) in [
        (&c.stable_inset_top, c"cluster_stable_inset_top"),
        (&c.stable_inset_bottom, c"cluster_stable_inset_bottom"),
        (&c.stable_inset_left, c"cluster_stable_inset_left"),
        (&c.stable_inset_right, c"cluster_stable_inset_right"),
    ] {
        #[allow(clippy::cast_possible_wrap)]
        Cfg::set(f, cfg_u32(k, 0) as i32);
    }
    load_def(&c.quiet_driver, c"cluster_quiet_driver", 1);
    load_def(&c.output, c"cluster_output", 0);
    if c.output() != 0 && c.output() != 2 {
        cl_loge!("[GalCluster] invalid cluster_output; using direct MOST");
        Cfg::set(&c.output, 0);
    }
    load_def(&c.dm_display, c"cluster_dm_display", 4);
    load_def(&c.dm_table, c"cluster_dm_table", 70);
    load_def(&c.dm_hide_ctx, c"cluster_dm_hide_context", -9);
    load_def(&c.ready_settle_ms, c"cluster_ready_settle_ms", 5000);
    load_def(&c.handover_settle_ms, c"cluster_handover_settle_ms", 500);
    load_def(&c.main_focus_wait_ms, c"cluster_main_focus_wait_ms", 8000);
    load_def(&c.main_focus_extra_ms, c"cluster_main_focus_extra_ms", 750);
    load_def(&c.boot_settle_secs, c"cluster_boot_settle_secs", 90);
    load_def(&c.flush_max_hold_ms, c"cluster_flush_max_hold_ms", 100);
    load_def(&c.focus_kick, c"cluster_focus_kick", 0);
    load_def(&c.focus_kick_wq, c"cluster_focus_kick_wq", 16);
    load_def(&c.focus_kick_settle, c"cluster_focus_kick_settle_ms", 1500);
    load_def(&c.focus_kick_grace, c"cluster_focus_kick_grace_ms", 15000);
    load_def(&c.focus_kick_min_ms, c"cluster_focus_kick_min_ms", 20000);
    load_def(&c.focus_kick_max, c"cluster_focus_kick_max", 3);
    load_def(&c.sink_feedback, c"cluster_sink_feedback", 1);
    load_i32(&c.write_pace_pct, c"cluster_write_pace_pct");
    load_i32(&c.write_pace_max_wait_ms, c"cluster_write_pace_max_wait_ms");
    load_i32(&c.ui_pace_kbps, c"cluster_ui_pace_kbps");
    let pace = c.write_pace_pct();
    if pace != 0 && pace < 100 {
        cl_loge!(
            "[GalCluster] cluster_write_pace_pct={} invalid (need 0 or >=100) \u{2014} using 150",
            pace
        );
        Cfg::set(&c.write_pace_pct, 150);
    }
    let ui = c.ui_pace_kbps();
    if ui < 0 {
        cl_loge!(
            "[GalCluster] cluster_ui_pace_kbps={} invalid \u{2014} using 9600",
            ui
        );
        Cfg::set(&c.ui_pace_kbps, 9600);
    }
    load_def(&c.cb_enable, c"cluster_cb_enable", 1);
    load_def(&c.cb_hard_stall_ms, c"cluster_cb_hard_stall_ms", 1000);
    load_def(&c.cb_stall_ms, c"cluster_cb_stall_ms", 400);
    load_def(&c.cb_stall_count, c"cluster_cb_stall_count", 3);
    load_def(&c.cb_max_werr, c"cluster_cb_max_werr", 6);
    load_def(&c.cb_cooldown_secs, c"cluster_cb_cooldown_secs", 60);
    load_def(&c.cb_max_trips, c"cluster_cb_max_trips", 3);
    load_def(&c.gate_enable, c"cluster_gate_enable", 1);
    cl_log!(
        "[GalCluster] circuit-breaker: enable={} hard_stall_ms={} stall_ms={} stall_count={} max_werr={} cooldown={}s max_trips={}",
        c.cb_enable(),
        c.cb_hard_stall_ms(),
        c.cb_stall_ms(),
        c.cb_stall_count(),
        c.cb_max_werr(),
        c.cb_cooldown_secs(),
        c.cb_max_trips()
    );
    cl_log!(
        "[GalCluster] MPE encoder gate: enable={} (quiet_driver forced on when gated)",
        c.gate_enable()
    );
    if c.gate_enable() != 0 && c.quiet_driver() == 0 {
        cl_loge!("[GalCluster] gate on but quiet_driver=0 \u{2014} forcing quiet_driver=1 (churn would flood the slogger)");
        Cfg::set(&c.quiet_driver, 1);
    }
    cl_log!(
        "[GalCluster] dm: display={} table={} hide_ctx={}",
        c.dm_display(),
        c.dm_table(),
        c.dm_hide_ctx()
    );
    let ci = c.content_insets();
    let si = c.stable_insets();
    cl_log!(
        "[GalCluster] config: codec_res={} sps_crop={} margins={}x{} pos t/b/l/r={}/{}/{}/{} (-1=auto) density={} view_dist={} content_insets={}/{}/{}/{} stable={}/{}/{}/{} quiet_driver={}",
        c.codec_res(),
        c.sps_crop(),
        as_u32(c.width_margin()),
        as_u32(c.height_margin()),
        c.margin_top(),
        c.margin_bottom(),
        c.margin_left(),
        c.margin_right(),
        as_u32(c.density()),
        as_u32(c.viewing_dist()),
        ci[0],
        ci[1],
        ci[2],
        ci[3],
        si[0],
        si[1],
        si[2],
        si[3],
        c.quiet_driver()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_u32_keeps_the_default_for_a_negative_value() {
        // No reader installed: every lookup returns its default.
        assert_eq!(cfg_u32(c"cluster_density", 180), 180);
        assert_eq!(as_u32(-1), u32::MAX);
    }
}
