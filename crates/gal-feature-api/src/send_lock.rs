// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Shared recursive lock for outbound receiver messages.
//!
//! One pthread mutex serializes frames from GAL and extension threads. The
//! loader holds it across enqueue and the complete ping send. Features hold
//! it across construction and submission of each outbound frame. This keeps
//! a concurrent sender from changing the receiver's shared message state.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use qnx_slog::sys;

struct SendLock {
    m: UnsafeCell<[u64; 4]>, // pthread_mutex_t is 8 bytes on QNX 6.5
    ready: AtomicBool,
}
// SAFETY: the buffer is only touched through pthread calls after init.
unsafe impl Sync for SendLock {}

static SEND_LOCK: SendLock = SendLock {
    m: UnsafeCell::new([0; 4]),
    ready: AtomicBool::new(false),
};

/// Create the mutex (loader constructor, before any feature or gal thread).
pub fn init() {
    let mut attr = [0u64; 8]; // pthread_mutexattr_t is 32 bytes
                              // SAFETY: both buffers are larger than the QNX types and 8-aligned. this
                              // runs once, in the constructor, before any other thread uses the lock.
    unsafe {
        let a = attr.as_mut_ptr().cast::<c_void>();
        sys::pthread_mutexattr_init(a);
        sys::pthread_mutexattr_settype(a, sys::PTHREAD_MUTEX_RECURSIVE);
        sys::pthread_mutex_init(SEND_LOCK.m.get().cast(), a);
        sys::pthread_mutexattr_destroy(a);
    }
    SEND_LOCK.ready.store(true, Ordering::Release);
}

/// True once [`init`] has run.
#[must_use]
pub fn ready() -> bool {
    SEND_LOCK.ready.load(Ordering::Acquire)
}

/// Take the lock (recursive).
pub fn lock() {
    if ready() {
        // SAFETY: initialised recursive mutex.
        unsafe { sys::pthread_mutex_lock(SEND_LOCK.m.get().cast()) };
    }
}

/// Release one level of the lock.
pub fn unlock() {
    if ready() {
        // SAFETY: initialised recursive mutex, held by this thread.
        unsafe { sys::pthread_mutex_unlock(SEND_LOCK.m.get().cast()) };
    }
}

/// Holds the send lock until dropped.
pub struct SendGuard(());

impl Drop for SendGuard {
    fn drop(&mut self) {
        unlock();
    }
}

/// Take the lock for the life of the returned guard.
#[must_use]
pub fn hold() -> SendGuard {
    lock();
    SendGuard(())
}
