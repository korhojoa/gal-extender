// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Session-end notifications for built-in Rust modules.
//!
//! A module must release session pointers before GAL frees the session.
//! Both read and write failures can end it. a writer can fail before any read
//! fails. The loader dispatches end notifications for failed transport writes
//! and before `GalReceiver::prepareShutdown()` and `shutdown()` run.
//!
//! The executable's calls at 0x1ce85c through 0x1ce8a8 support the teardown
//! ordering. That session-lifetime interpretation is inferred from code,
//! not measured. Dispatch takes no lock and does not allocate.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Why the session is ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// `Transport::writeOrFail` returned a failure for a non-empty write.
    WriteFailed,
    /// gal called `GalReceiver::prepareShutdown()`.
    PrepareShutdown,
    /// gal called `GalReceiver::shutdown()`.
    Shutdown,
}

impl End {
    /// A short name for log lines.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            End::WriteFailed => "writeOrFail failed",
            End::PrepareShutdown => "GalReceiver::prepareShutdown",
            End::Shutdown => "GalReceiver::shutdown",
        }
    }
}

/// A listener: called on the thread that ends the session, before gal's own
/// teardown code (for the two `GalReceiver` calls) or right after the failed
/// write. It may block briefly (the sensor waits for a report in flight).
pub type EndHook = fn(why: End);

const MAX: usize = 8;

static HOOKS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static N: AtomicUsize = AtomicUsize::new(0);
static REGISTER: Mutex<()> = Mutex::new(());

/// Add a listener (a module's init). A full list ignores the call.
pub fn on_end(f: EndHook) {
    let _g = REGISTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let n = N.load(Ordering::Relaxed);
    if n < MAX {
        HOOKS[n].store(f as usize, Ordering::Relaxed);
        N.store(n + 1, Ordering::Release);
    }
}

/// Tell every listener (the loader's interposers).
pub fn dispatch(why: End) {
    let n = N.load(Ordering::Acquire).min(MAX);
    for h in &HOOKS[..n] {
        let p = h.load(Ordering::Relaxed);
        if p != 0 {
            // SAFETY: stored by `on_end` from an EndHook.
            let f = unsafe { core::mem::transmute::<usize, EndHook>(p) };
            f(why);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU32;

    static SEEN: AtomicU32 = AtomicU32::new(0);

    fn listener(why: End) {
        let bit = match why {
            End::WriteFailed => 1,
            End::PrepareShutdown => 2,
            End::Shutdown => 4,
        };
        SEEN.fetch_or(bit, Ordering::Relaxed);
    }

    #[test]
    fn every_listener_hears_every_end() {
        on_end(listener);
        dispatch(End::WriteFailed);
        dispatch(End::PrepareShutdown);
        dispatch(End::Shutdown);
        assert_eq!(SEEN.load(Ordering::Relaxed), 7);
        assert_eq!(End::Shutdown.name(), "GalReceiver::shutdown");
    }
}
