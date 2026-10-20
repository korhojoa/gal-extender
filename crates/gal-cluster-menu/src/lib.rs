// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

pub mod cbuf;
pub mod conf;
pub mod fb;
pub mod font;
pub mod h264;
pub mod menu;
pub mod settings;
mod sys;

use core::ffi::{c_int, c_uint, c_void, CStr};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use gal_feature_api::location::{self, LoaderFile};
use gal_feature_api::{config, FeatureModule};
use qnx_slog::log::Line;
use qnx_slog::sys::{SLOG_ERROR, SLOG_INFO};
use std::ffi::CString;

use crate::cbuf::{CBuf, Shown};
use crate::fb::Fb;
use crate::menu::Menu;

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule { name: "ui", init };

/// The version in the banner: the single copy. Bump per change.
const VERSION: &str = "9-rs";

/// `DEFAULT_CONF`: `gal_loader.conf` in the loader directory, when the
/// loader has no conf path.
static DEFAULT_CONF: LoaderFile = LoaderFile::new("gal_loader.conf");

/// `UI_COMBO_MS`: next and previous within this many ms open the menu.
const COMBO_MS: u64 = 350;

fn log_at(sev: c_int, args: fmt::Arguments<'_>) {
    let mut msg: Line<400> = Line::new();
    let _ = msg.write_fmt(args);
    qnx_slog::log::raw(sev, format_args!("[GalUI] {}\n", msg.as_str()));
}

macro_rules! ui_log {
    ($($a:tt)*) => { $crate::log_at(SLOG_INFO, format_args!($($a)*)) };
}
macro_rules! ui_loge {
    ($($a:tt)*) => { $crate::log_at(SLOG_ERROR, format_args!($($a)*)) };
}
/// `now_ms`: `CLOCK_MONOTONIC` in ms.
fn now_ms() -> u64 {
    qnx_time::mono_ms()
}

fn sleep_us(us: u64) {
    std::thread::sleep(Duration::from_micros(us));
}

// ---------------------------------------------------------------------------
// The cluster's UI sink
// ---------------------------------------------------------------------------

/// `int gal_cluster_ui_open(void)`.
type OpenFn = unsafe extern "C" fn() -> c_int;
/// `int gal_cluster_ui_submit(const void* au, unsigned len, int is_idr)`.
type SubmitFn = unsafe extern "C" fn(*const c_void, c_uint, c_int) -> c_int;
/// `void gal_cluster_ui_close(void)`.
type CloseFn = unsafe extern "C" fn();

extern "C" fn builtin_open() -> c_int {
    catch_unwind(gal_cluster_video::ui::ui_open).unwrap_or(-1)
}
extern "C" fn builtin_submit(au: *const c_void, len: c_uint, _is_idr: c_int) -> c_int {
    if au.is_null() {
        return -1;
    }
    let Ok(n) = usize::try_from(len) else {
        return -1;
    };
    // SAFETY: the pump's own frame buffer, `len` bytes.
    let au = unsafe { core::slice::from_raw_parts(au.cast::<u8>(), n) };
    catch_unwind(|| gal_cluster_video::ui::ui_submit(au)).unwrap_or(-1)
}
extern "C" fn builtin_close() {
    let _ = catch_unwind(gal_cluster_video::ui::ui_close);
}

fn cl_open() -> Option<OpenFn> {
    gal_cluster_video::ui::loaded().then_some(builtin_open as OpenFn)
}
fn cl_submit() -> Option<SubmitFn> {
    gal_cluster_video::ui::loaded().then_some(builtin_submit as SubmitFn)
}
fn cl_close() -> Option<CloseFn> {
    gal_cluster_video::ui::loaded().then_some(builtin_close as CloseFn)
}

/// Refuse the menu when its normal cluster sink is not loaded.
fn resolve_cluster_sink() -> bool {
    let loaded = gal_cluster_video::ui::loaded();
    if !loaded {
        ui_loge!("cluster UI sink unavailable - enable the cluster feature");
    }
    loaded
}

// ---------------------------------------------------------------------------
// Config and state
// ---------------------------------------------------------------------------

static W: AtomicI32 = AtomicI32::new(400);
static H: AtomicI32 = AtomicI32::new(240);
static KEEPALIVE_MS: AtomicI32 = AtomicI32::new(1000);
static IDLE_TIMEOUT_S: AtomicI32 = AtomicI32::new(45);
static TRIGGER_KC: AtomicI32 = AtomicI32::new(84);
static REMOUNT: AtomicI32 = AtomicI32::new(1);
/// `char g_conf_path[128]`, with its NUL.
static CONF_PATH: OnceLock<CBuf<129>> = OnceLock::new();

/// `g_ok`: buffers allocated and the pump started.
static OK: AtomicBool = AtomicBool::new(false);
/// `g_run`.
static RUN: AtomicBool = AtomicBool::new(false);
static LOADED: AtomicBool = AtomicBool::new(false);
static LAST_SUBMIT_MS: AtomicU64 = AtomicU64::new(0);
/// `g_last_next_ms`, `g_last_prev_ms` (input thread only).
static LAST_NEXT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_PREV_MS: AtomicU64 = AtomicU64::new(0);

/// The state shared with gal's input thread (`g_mtx` and what it guards).
#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent flags protected by one lock"
)]
struct Shared {
    menu: Option<Menu>,
    active: bool,
    open_pending: bool,
    want_close: bool,
    /// Save, then close.
    want_save: bool,
    redraw: bool,
    last_input_ms: u64,
}

static MTX: Mutex<Shared> = Mutex::new(Shared {
    menu: None,
    active: false,
    open_pending: false,
    want_close: false,
    want_save: false,
    redraw: false,
    last_input_ms: 0,
});

fn lock() -> MutexGuard<'static, Shared> {
    MTX.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The conf path as bytes, no NUL.
fn conf_path() -> &'static [u8] {
    CONF_PATH
        .get()
        .map_or(DEFAULT_CONF.path().as_bytes(), CBuf::as_bytes)
}

/// The mount point to remount read-write around a save of `conf`, or None
/// when no remount is needed or none can be found (each logged). Asks the
/// file system that holds the conf (`gal_feature_api::location`). a file
/// system that is already read-write is not remounted.
fn remount_target(conf: &[u8]) -> Option<String> {
    let conf = core::str::from_utf8(conf).ok()?;
    let Some((mp, how)) = location::mount_point(conf) else {
        ui_loge!("save: no mount point found for {} - no remount", conf);
        return None;
    };
    match location::read_only(conf) {
        Some(false) => {
            ui_log!(
                "save: {} is on {} ({:?}), mounted read-write - no remount",
                conf,
                mp,
                how
            );
            None
        }
        ro => {
            ui_log!(
                "save: {} is on {} ({:?}), {} - remounting it read-write",
                conf,
                mp,
                how,
                if ro.is_some() {
                    "read-only"
                } else {
                    "mode unknown"
                }
            );
            Some(mp)
        }
    }
}

fn mount_cmd(flag: &str, mp: &str) -> Option<CString> {
    let plain = mp
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_.-+".contains(&b));
    let cmd = if plain {
        format!("mount {flag} {mp}")
    } else {
        format!("mount {flag} '{}'", mp.replace('\'', "'\\''"))
    };
    CString::new(cmd).ok()
}

// ---------------------------------------------------------------------------
// Render and submit (pump thread only)
// ---------------------------------------------------------------------------

/// The pump's buffers and the last encoded frame (`g_rgba`, `g_yuv`,
/// `g_out`, `g_frame_len`, `g_have_frame`).
struct Frames {
    rgba: Vec<u8>,
    yuv: Vec<u8>,
    out: Vec<u8>,
    len: usize,
    have: bool,
}

impl Frames {
    /// `render_locked`: draw the menu and encode it. The caller holds the
    /// lock (it reads the menu).
    fn render(&mut self, sh: &mut Shared) {
        let (w, h) = (W.load(Ordering::Relaxed), H.load(Ordering::Relaxed));
        let Some(menu) = sh.menu.as_mut() else {
            return;
        };
        menu.render(&mut Fb {
            px: &mut self.rgba,
            w,
            h,
        });
        let sz = h264::encode_rgba(&mut self.out, &self.rgba, w, h, &mut self.yuv);
        if sz == 0 {
            ui_loge!("encode failed ({}x{})", w, h);
        } else {
            self.len = sz;
            self.have = true;
        }
    }

    /// Give the frame to the sink.
    fn submit(&self, f: SubmitFn) -> c_int {
        let len = c_uint::try_from(self.len).unwrap_or(c_uint::MAX);
        // SAFETY: the sink reads `len` bytes of our live buffer.
        unsafe { f(self.out.as_ptr().cast(), len, 1) }
    }

    /// `do_close`.
    fn close(&mut self) {
        {
            let mut sh = lock();
            sh.active = false;
            sh.want_close = false;
            sh.want_save = false;
            sh.open_pending = false;
        }
        if let Some(c) = cl_close() {
            // SAFETY: the sink's close, no arguments.
            unsafe { c() };
        }
        self.have = false;
        LAST_SUBMIT_MS.store(0, Ordering::Relaxed);
        ui_log!("UI closed");
    }

    /// `do_save_then_close`.
    fn save_then_close(&mut self) {
        let remount = REMOUNT.load(Ordering::Relaxed);
        ui_log!(
            "saving settings to {} (remount={})",
            Shown(conf_path()),
            remount
        );
        let target = if remount == 0 {
            None
        } else {
            remount_target(conf_path())
        };
        if let Some(cmd) = target.as_deref().and_then(|mp| mount_cmd("-uw", mp)) {
            // SAFETY: a NUL-terminated command.
            unsafe { sys::system(cmd.as_ptr()) };
        }
        let mut path: CBuf<130> = CBuf::new();
        path.push_bytes(conf_path());
        path.push_bytes(b"\0");
        let status = {
            let mut sh = lock();
            if let Some(m) = sh.menu.as_mut() {
                m.input(
                    menu::EV_SAVE,
                    CStr::from_bytes_until_nul(path.as_bytes()).ok(),
                );
            }
            self.render(&mut sh);
            sh.menu.as_ref().map(|m| m.status).unwrap_or_default()
        };
        if let Some(cmd) = target.as_deref().and_then(|mp| mount_cmd("-ur", mp)) {
            // SAFETY: plain libc calls. a NUL-terminated command.
            unsafe {
                sys::sync();
                sys::system(cmd.as_ptr());
            }
        }
        ui_log!("save: {}", Shown(status.as_bytes()));
        // Show the result for about 1.2 s, then close.
        for _ in 0..12 {
            if let Some(f) = cl_submit() {
                if self.have {
                    self.submit(f);
                }
            }
            sleep_us(100_000);
        }
        self.close();
    }
}

/// One turn of the pump loop. Returns the sleep before the next one, in us.
fn pump_once(fr: &mut Frames) -> u64 {
    let (active, open_pending, do_sv, do_cl) = {
        let mut sh = lock();
        let (active, open_pending, do_sv) = (sh.active, sh.open_pending, sh.want_save);
        let mut do_cl = sh.want_close;
        let now = now_ms();
        let idle = IDLE_TIMEOUT_S.load(Ordering::Relaxed);
        if active
            && !do_sv
            && !do_cl
            && idle > 0
            && now.wrapping_sub(sh.last_input_ms) > u64::from(idle.unsigned_abs()) * 1000
        {
            do_cl = true;
            sh.want_close = true;
            ui_log!("idle timeout ({}s) \u{2014} closing", idle);
        }
        if active && sh.redraw {
            fr.render(&mut sh);
            sh.redraw = false;
            LAST_SUBMIT_MS.store(0, Ordering::Relaxed);
        }
        (active, open_pending, do_sv, do_cl)
    };

    if open_pending {
        // SAFETY: the sink's open, no arguments.
        let r = cl_open().map_or(-1, |f| unsafe { f() });
        lock().open_pending = false;
        LAST_SUBMIT_MS.store(0, Ordering::Relaxed);
        if r < 0 {
            ui_loge!("cluster sink open failed r={} \u{2014} closing", r);
            fr.close();
            return 50_000;
        }
    }
    if do_sv {
        fr.save_then_close();
        return 50_000;
    }
    if do_cl {
        fr.close();
        return 50_000;
    }

    if active && fr.have {
        if let Some(f) = cl_submit() {
            let n2 = now_ms();
            let last = LAST_SUBMIT_MS.load(Ordering::Relaxed);
            // `(uint64_t)g_keepalive_ms`: sign-extended, so a negative value
            // is huge and the frame is never repeated.
            let keepalive = KEEPALIVE_MS.load(Ordering::Relaxed);
            let keep = u64::from_ne_bytes(i64::from(keepalive).to_ne_bytes());
            if last == 0 || n2.wrapping_sub(last) >= keep {
                let r = fr.submit(f);
                if r > 0 {
                    LAST_SUBMIT_MS.store(n2, Ordering::Relaxed);
                } else if r < 0 {
                    ui_loge!("cluster sink lost \u{2014} closing");
                    fr.close();
                }
                // r == 0: DM still draining. again on the next (short) turn.
            }
        }
    }
    if active {
        60_000
    } else {
        200_000
    }
}

fn pump_thread(mut fr: Frames) {
    while RUN.load(Ordering::Acquire) {
        let us = catch_unwind(AssertUnwindSafe(|| pump_once(&mut fr))).unwrap_or_else(|_| {
            ui_loge!("PANIC contained in the pump thread; it goes on");
            200_000
        });
        sleep_us(us);
    }
}

/// `gal_ui_is_active`.
#[must_use]
pub fn is_active() -> bool {
    lock().active
}

/// `gal_ui_open`.
pub fn open() {
    if !OK.load(Ordering::Acquire) || !resolve_cluster_sink() {
        return;
    }
    {
        let mut sh = lock();
        if sh.active {
            return;
        }
        sh.active = true;
        sh.open_pending = true;
        sh.redraw = true;
        sh.want_close = false;
        sh.want_save = false;
        sh.last_input_ms = now_ms();
        LAST_SUBMIT_MS.store(0, Ordering::Relaxed);
    }
    ui_log!("UI open requested");
}

/// `gal_ui_close`.
pub fn close() {
    let mut sh = lock();
    if sh.active {
        sh.want_close = true;
    }
}

pub fn handle_key(keycode: i32, down: bool, _is_long: bool) -> bool {
    if !OK.load(Ordering::Acquire) {
        return false;
    }
    if !is_active() {
        if down && (keycode == 87 || keycode == 88) {
            let now = now_ms();
            let other = if keycode == 87 {
                LAST_NEXT_MS.store(now, Ordering::Relaxed);
                LAST_PREV_MS.load(Ordering::Relaxed)
            } else {
                LAST_PREV_MS.store(now, Ordering::Relaxed);
                LAST_NEXT_MS.load(Ordering::Relaxed)
            };
            if other != 0 && now.wrapping_sub(other) <= COMBO_MS {
                LAST_NEXT_MS.store(0, Ordering::Relaxed);
                LAST_PREV_MS.store(0, Ordering::Relaxed);
                ui_log!("open gesture (next+prev)");
                open();
                return true;
            }
        }
        return false;
    }
    if down {
        let ev = if keycode == 88 {
            menu::EV_PREV
        } else if keycode == 87 {
            menu::EV_NEXT
        } else if keycode == TRIGGER_KC.load(Ordering::Relaxed) {
            menu::EV_SELECT
        } else {
            -1
        };
        if ev >= 0 {
            {
                let mut sh = lock();
                if let Some(m) = sh.menu.as_mut() {
                    m.input(ev, None);
                }
                sh.redraw = true;
                sh.last_input_ms = now_ms();
                if let Some(m) = sh.menu.as_mut() {
                    let (save, close) = (m.want_save, m.want_close);
                    m.want_save = false;
                    m.want_close = false;
                    sh.want_save |= save;
                    sh.want_close |= close;
                }
            }
        }
    }
    true
}

extern "C" fn handle_key_c(keycode: c_int, down: c_int, is_long: c_int) -> c_int {
    if let Ok(used) = catch_unwind(|| handle_key(keycode, down != 0, is_long != 0)) {
        c_int::from(used)
    } else {
        ui_loge!("PANIC contained in the key handler; the key goes on to gal");
        0
    }
}

/// The key handler for the input feature: Some once the loader started this
/// module. No external-library handler is substituted.
#[must_use]
pub fn key_handler() -> Option<unsafe extern "C" fn(c_int, c_int, c_int) -> c_int> {
    LOADED
        .load(Ordering::Acquire)
        .then_some(handle_key_c as unsafe extern "C" fn(c_int, c_int, c_int) -> c_int)
}

// ---------------------------------------------------------------------------
// init (the loader calls it once, on the constructor thread, panic-guarded)
// ---------------------------------------------------------------------------

/// `round16`: down to a multiple of 16 (the encoder needs it). `def` for a
/// value that is not positive or rounds to 0.
fn round16(v: i32, def: i32) -> i32 {
    if v <= 0 {
        return def;
    }
    let v = v & !15;
    if v > 0 {
        v
    } else {
        def
    }
}

fn alloc(n: Option<usize>) -> Option<Vec<u8>> {
    let n = n?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, 0);
    Some(v)
}

fn buffer_sizes(w: i32, h: i32) -> Option<(usize, usize, usize)> {
    let (w, h) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?);
    let px = w.checked_mul(h)?;
    let rgba = px.checked_mul(4)?;
    let yuv = px.checked_add((w / 2).checked_mul(h / 2)?.checked_mul(2)?)?;
    let out = px.checked_mul(2)?.checked_add(4096)?;
    Some((rgba, yuv, out))
}

#[allow(clippy::too_many_lines)]
fn init() {
    LOADED.store(true, Ordering::Release);
    ui_loge!("ui v{}", VERSION);

    // The FULL cluster panel (800x480), so the frame maps 1:1 onto it. the
    // menu is drawn inside the visible window (below).
    let enc_w = round16(config::get_int(c"ui_encode_w", 800), 800);
    let enc_h = round16(config::get_int(c"ui_encode_h", 480), 480);
    W.store(enc_w, Ordering::Relaxed);
    H.store(enc_h, Ordering::Relaxed);
    let keepalive = config::get_int(c"ui_keepalive_ms", 1500);
    KEEPALIVE_MS.store(keepalive, Ordering::Relaxed);
    let idle = config::get_int(c"ui_idle_timeout_s", 45);
    IDLE_TIMEOUT_S.store(idle, Ordering::Relaxed);
    let trigger = config::get_int(c"ui_trigger_keycode", 84);
    TRIGGER_KC.store(trigger, Ordering::Relaxed);
    REMOUNT.store(config::get_int(c"ui_remount", 1), Ordering::Relaxed);

    // snprintf(g_conf_path, 128, "%s", p) when the path is not empty.
    let p = config::path().to_bytes();
    let mut path: CBuf<128> = CBuf::new();
    path.push_bytes(if p.is_empty() {
        DEFAULT_CONF.path().as_bytes()
    } else {
        p
    });
    let mut stored: CBuf<129> = CBuf::new();
    stored.push_bytes(path.as_bytes());
    let _ = CONF_PATH.set(stored);

    // The settings and their values in the conf.
    let mut items = settings::table();
    let nset = items.len();
    conf::load(conf_path(), &mut items);
    let mut menu = Menu::new(items);

    // The content box: the visible cluster window, from the same margins
    // and insets the cluster advertises (the defaults are the cluster's).
    {
        let mt = config::get_int(c"cluster_margin_top", 60);
        let mb = config::get_int(c"cluster_margin_bottom", 96);
        let ml = config::get_int(c"cluster_margin_left", 48);
        let mr = config::get_int(c"cluster_margin_right", 48);
        let cit = config::get_int(c"cluster_content_inset_top", 0);
        let cib = config::get_int(c"cluster_content_inset_bottom", 0);
        let cil = config::get_int(c"cluster_content_inset_left", 64);
        let cir = config::get_int(c"cluster_content_inset_right", 64);
        let left = ml.wrapping_add(cil);
        let top = mt.wrapping_add(cit);
        let bw = enc_w
            .wrapping_sub(ml.wrapping_add(cil))
            .wrapping_sub(mr.wrapping_add(cir));
        let bh = enc_h
            .wrapping_sub(mt.wrapping_add(cit))
            .wrapping_sub(mb.wrapping_add(cib));
        if bw > 0 && bh > 0 {
            (menu.cx, menu.cy, menu.cw, menu.ch) = (left, top, bw, bh);
        }
        ui_log!(
            "content window: x={} y={} w={} h={} (of {}x{})",
            menu.cx,
            menu.cy,
            menu.cw,
            menu.ch,
            enc_w,
            enc_h
        );
    }
    {
        let mut sh = lock();
        sh.menu = Some(menu);
    }

    // The frame buffers (heap).
    let sizes = buffer_sizes(enc_w, enc_h);
    let bufs = (
        alloc(sizes.map(|s| s.0)),
        alloc(sizes.map(|s| s.1)),
        alloc(sizes.map(|s| s.2)),
    );
    let (Some(rgba), Some(yuv), Some(out)) = bufs else {
        ui_loge!(
            "buffer alloc failed ({}x{}) \u{2014} UI disabled",
            enc_w,
            enc_h
        );
        return;
    };
    let frames = Frames {
        rgba,
        yuv,
        out,
        len: 0,
        have: false,
    };

    OK.store(true, Ordering::Release);
    RUN.store(true, Ordering::Release);
    let spawned = std::thread::Builder::new()
        .name("gal-ui-pump".into())
        .stack_size(128 * 1024)
        .spawn(move || pump_thread(frames));
    if spawned.is_err() {
        ui_loge!("pump thread create failed \u{2014} UI disabled");
        OK.store(false, Ordering::Release);
        RUN.store(false, Ordering::Release);
        return;
    }

    ui_log!(
        "ui ready: {}x{} conf={} trigger_kc={} keepalive={}ms idle={}s settings={}",
        enc_w,
        enc_h,
        Shown(conf_path()),
        trigger,
        keepalive,
        idle,
        nset
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_is_named_as_the_conf_word() {
        assert_eq!(MODULE.name, "ui");
    }

    #[test]
    fn round16_uses_positive_aligned_dimensions() {
        assert_eq!(round16(800, 1), 800);
        assert_eq!(round16(810, 1), 800);
        assert_eq!(round16(15, 7), 7);
        assert_eq!(round16(0, 7), 7);
        assert_eq!(round16(-32, 7), 7);
    }

    #[test]
    fn buffer_sizes_for_the_panel() {
        assert_eq!(buffer_sizes(800, 480), Some((1_536_000, 576_000, 772_096)));
        #[cfg(target_pointer_width = "32")]
        assert_eq!(buffer_sizes(65536, 65536), None);
    }

    #[test]
    fn menu_refuses_a_disabled_cluster_sink() {
        assert!(!gal_cluster_video::ui::loaded());
        assert!(!resolve_cluster_sink());
        assert!(cl_open().is_none());
        assert!(cl_submit().is_none());
        assert!(cl_close().is_none());
    }

    #[test]
    fn closed_menu_passes_keys_before_init() {
        // OK is false until init ran: every key goes on to gal.
        assert!(!handle_key(87, true, false));
        assert!(key_handler().is_none() || LOADED.load(Ordering::Acquire));
    }
}
