// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! In-process `DisplayManager` navigation handover and restoration.
//! Initialization must succeed before cluster output opens. The retired
//! shell-command implementation is removed. there is no fork fallback.
//! The MPE gate still proves encoder shutdown before isoTX2 is opened.

use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::OnceLock;

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge, wall_s};
use crate::sys::{self, sig};

/// The native navigation handover is active until restoration.
pub static HIDDEN: AtomicBool = AtomicBool::new(false);
static LAST_REASSERT_HIDE: AtomicI64 = AtomicI64::new(0);

/// The native control's one-time init result (`s_dmn.tried/ok`).
static NATIVE: OnceLock<bool> = OnceLock::new();

/// `dm_native_init`: lazy, once. logs the outcome once.
pub fn ready() -> bool {
    *NATIVE.get_or_init(|| match display_manager_control::init() {
        Ok(()) => {
            cl_loge!("[GalCluster] dm-native: in-process DisplayManager control ready");
            true
        }
        Err(error) => {
            cl_loge!(
                "[GalCluster] DisplayManager unavailable: {} - cluster output disabled",
                error
            );
            false
        }
    })
}

/// Send the native request. The returned register is not a status code.
fn native_switch(display: i32, ctx: i32) {
    let r = display_manager_control::switch(display, ctx);
    cl_loge!(
        "[GalCluster] dm-native: switchContext(display={}, ctx={}) -> {}",
        display,
        ctx,
        r
    );
}

/// `dmdt_issue_hide`: unconditional, so also the re-assert.
pub fn issue_hide() {
    if !ready() {
        return;
    }
    let c = &CFG;
    // Once per session, before the first hide.
    crate::takeover::taking();
    // A fresh hide. see gate::mpe_mark.
    if !HIDDEN.load(Ordering::Acquire) {
        crate::gate::mpe_mark();
    }
    if ready() {
        native_switch(c.dm_display(), c.dm_hide_ctx());
    }
}

/// `dmdt_hide_nav`: true when a fresh hide was issued.
pub fn hide_nav() -> bool {
    if HIDDEN.load(Ordering::Acquire) || !ready() {
        return false;
    }
    issue_hide();
    HIDDEN.store(true, Ordering::Release);
    cl_log!("[GalCluster] dmdt: cluster nav hide issued");
    true
}

/// `dmdt_restore_nav`.
pub fn restore_nav() {
    if !HIDDEN.load(Ordering::Acquire) {
        return;
    }
    let c = &CFG;
    if ready() {
        native_switch(c.dm_display(), c.dm_table());
        HIDDEN.store(false, Ordering::Release);
        cl_log!("[GalCluster] native nav restore issued");
        crate::takeover::clear();
    }
}

/// `cluster_reassert_hide`: at most once per wall-clock second, ignoring
/// [`HIDDEN`].
pub fn reassert_hide() {
    let now = wall_s();
    if LAST_REASSERT_HIDE.load(Ordering::Relaxed) == now {
        return;
    }
    LAST_REASSERT_HIDE.store(now, Ordering::Relaxed);
    issue_hide();
    HIDDEN.store(true, Ordering::Release);
}

/// Restore the cluster at process exit. Initialization registers this handler with
/// atexit only when the feature is enabled.
pub fn exit_restore() {
    crate::vfn::invalidate("process exiting");
    if HIDDEN.load(Ordering::Acquire) {
        cl_loge!("[GalCluster] process exiting \u{2014} restoring native nav to cluster");
        restore_nav();
    }
}

// ---------------------------------------------------------------------------
// Fatal-signal restore
// ---------------------------------------------------------------------------

struct PrevActions(UnsafeCell<[sig::SigAction; sig::NSIG]>);
// SAFETY: written once by `install_fatal_restore` (loader constructor,
// before any signal can reach the handler), then only read.
unsafe impl Sync for PrevActions {}
static PREV: PrevActions = PrevActions(UnsafeCell::new([sig::SigAction::ZERO; sig::NSIG]));
static IN_FATAL: AtomicBool = AtomicBool::new(false);

/// `cluster_fatal_restore`: give the panel back once, then re-raise through
/// the disposition installed before ours.
extern "C" fn fatal_restore(signo: c_int) {
    if !IN_FATAL.swap(true, Ordering::AcqRel) && HIDDEN.load(Ordering::Acquire) {
        let _ = std::panic::catch_unwind(restore_nav);
    }
    if let Ok(i) = usize::try_from(signo) {
        if i < sig::NSIG {
            // SAFETY: PREV is read only here. Restore the saved action before raising
            // the signal again.
            unsafe {
                let prev = &(*PREV.0.get())[i];
                sig::sigaction(signo, prev, core::ptr::null_mut());
                sys::raise(signo);
            }
        }
    }
}

/// `cluster_install_fatal_restore`.
pub fn install_fatal_restore() {
    let sigs = [
        sig::SIGSEGV,
        sig::SIGABRT,
        sig::SIGBUS,
        sig::SIGILL,
        sig::SIGFPE,
        sig::SIGTERM,
        sig::SIGINT,
        sig::SIGHUP,
        sig::SIGQUIT,
    ];
    let handler: extern "C" fn(c_int) = fatal_restore;
    let sa = sig::SigAction {
        handler: handler as usize,
        flags: sig::SA_NODEFER,
        mask: [0; 2],
    };
    for s in sigs {
        if let Ok(i) = usize::try_from(s) {
            if i < sig::NSIG {
                // SAFETY: constructor thread, before the handler can run;
                // the slot is this signal's own.
                unsafe {
                    let prev = &raw mut (*PREV.0.get())[i];
                    sig::sigaction(s, &raw const sa, prev);
                }
            }
        }
    }
    cl_loge!("[GalCluster] fatal-signal nav restore armed (crash/SIGTERM give the panel back; SIGKILL still needs an external watchdog)");
}
