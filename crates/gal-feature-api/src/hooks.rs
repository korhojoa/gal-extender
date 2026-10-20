// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Subscribe to and dispatch interposed receiver methods. Each list holds at most
//! `MAX_HOOKS` typed Rust callbacks in registration order. Initialization writes
//! a slot before publishing its count. Dispatch has no allocation or lock.

use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Maximum callbacks for each hook.
pub const MAX_HOOKS: usize = 8;

type P = *mut c_void;
type CP = *const c_void;

/// A key from `InputSource::sendKey` or `reportKeyLongPress`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// gal's 64-bit timestamp.
    pub ts: u64,
    /// The high timestamp word for sendKey. zero for a long press.
    pub seq: u32,
    /// Android key code (84 search, 85 play/pause, 87 next, 88 previous).
    pub keycode: i32,
    /// sendKey: key down. long press: true.
    pub down: bool,
    /// sendKey: gal's second bool (0/1). long press: the duration.
    pub extra: u32,
    /// True for reportKeyLongPress.
    pub long_press: bool,
}

/// A touch report (stored, never dispatched. see [`INPUT_TOUCH`]).
#[derive(Clone, Copy, Debug)]
pub struct TouchEvent {
    pub ts: u64,
    pub count: i32,
    pub ids: *const u32,
    pub x: *const u32,
    pub y: *const u32,
    pub action: i32,
    pub action_index: i32,
}

/// A rotary report (stored, never dispatched. see [`INPUT_ROTARY`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RotaryEvent {
    pub ts: u64,
    pub code: u32,
    pub value: i32,
    pub relative: bool,
}

/// reportFuelData's values. a hook may rewrite them before gal sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuelData {
    pub has_level: i32,
    pub level: i32,
    pub has_range: i32,
    pub range_m: i32,
    pub has_low: i32,
    pub low_fuel: i32,
}

/// reportSpeedData's values. a hook may rewrite them before gal sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedData {
    pub speed_e3: i32,
    pub has_cruise: i32,
    pub cruise_engaged: i32,
    pub has_cruise_speed: i32,
    pub cruise_speed_e3: i32,
}

pub type DiscoveryHook = fn(this: P, arg: P);
pub type ChannelOpenedHook = fn(router: P, ch: u8, req: P);
pub type UnexpectedMsgHook = fn(router: P, ch: u8);
/// Returns true to suppress gal's own handler.
pub type SensorRequestHook = fn(this: P, req: CP) -> bool;
/// Returns true to suppress gal's forward to the phone.
pub type InputKeyHook = fn(this: P, ev: &KeyEvent) -> bool;
pub type InputTouchHook = fn(this: P, ev: &TouchEvent) -> bool;
pub type InputRotaryHook = fn(this: P, ev: &RotaryEvent) -> bool;
pub type FuelHook = fn(this: P, d: &mut FuelData);
pub type SpeedHook = fn(this: P, d: &mut SpeedData);
pub type VideoFocusHook = fn(this: P, mode: i32, reason: i32, req: CP);
pub type VideoFocusGrantHook = fn(this: P, mode: i32, unsolicited: i32);
/// `ret` is gal's readOrFail result (1 = success, not a byte count).
pub type TransportReadHook = fn(this: P, buf: CP, len: u32, ret: i32);
pub type TransportWriteHook = fn(this: P, buf: CP, len: u32);
/// Replaces gal's readOrFail. returns its result.
pub type ReadProvider = fn(this: P, buf: P, len: u32) -> i32;
/// Replaces gal's writeOrFail. returns its result.
pub type WriteProvider = fn(this: P, buf: CP, len: u32) -> i32;

// ---------------------------------------------------------------------------
// The lists
// ---------------------------------------------------------------------------

static REGISTER: Mutex<()> = Mutex::new(());

/// Up to [`MAX_HOOKS`] typed callbacks.
pub struct HookList<R> {
    f: [AtomicUsize; MAX_HOOKS],
    n: AtomicUsize,
    _t: PhantomData<R>,
}

// SAFETY: the stored values are function addresses. R is a type marker.
unsafe impl<R> Sync for HookList<R> {}

fn to_addr<F: Copy>(f: F) -> usize {
    const { assert!(core::mem::size_of::<F>() == core::mem::size_of::<usize>()) };
    // SAFETY: F is a function pointer type of pointer size (asserted).
    unsafe { core::mem::transmute_copy::<F, usize>(&f) }
}

/// # Safety
/// `p` must have come from `to_addr::<F>`.
unsafe fn from_addr<F: Copy>(p: usize) -> F {
    const { assert!(core::mem::size_of::<F>() == core::mem::size_of::<usize>()) };
    core::mem::transmute_copy::<usize, F>(&p)
}

impl<R: Copy> HookList<R> {
    /// An empty list.
    #[must_use]
    const fn new() -> Self {
        HookList {
            f: [const { AtomicUsize::new(0) }; MAX_HOOKS],
            n: AtomicUsize::new(0),
            _t: PhantomData,
        }
    }

    /// Append a callback. Ignore the call when the list is full.
    fn push(&self, p: usize) {
        let _g = REGISTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = self.n.load(Ordering::Relaxed);
        if n < MAX_HOOKS {
            self.f[n].store(p, Ordering::Relaxed);
            self.n.store(n + 1, Ordering::Release);
        }
    }

    /// Register a Rust callback.
    pub fn register(&self, f: R) {
        self.push(to_addr(f));
    }

    /// Number of registered callbacks.
    pub fn count(&self) -> usize {
        self.n.load(Ordering::Acquire).min(MAX_HOOKS)
    }

    /// Each callback, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = R> + '_ {
        (0..self.count()).map(move |i| {
            let p = self.f[i].load(Ordering::Relaxed);
            // SAFETY: slot i was written by register from an R.
            unsafe { from_addr::<R>(p) }
        })
    }
}

/// At most one replacement for the transport call. The last set wins.
pub struct Provider<R> {
    rust: AtomicUsize,
    _t: PhantomData<R>,
}

// SAFETY: as HookList.
unsafe impl<R> Sync for Provider<R> {}

impl<R: Copy> Provider<R> {
    #[must_use]
    const fn new() -> Self {
        Provider {
            rust: AtomicUsize::new(0),
            _t: PhantomData,
        }
    }

    /// Set (or clear with None) the Rust provider.
    pub fn set(&self, f: Option<R>) {
        self.rust.store(f.map_or(0, to_addr), Ordering::Release);
    }

    /// The current provider.
    pub fn get(&self) -> Option<R> {
        let r = self.rust.load(Ordering::Acquire);
        // SAFETY: nonzero values are stored by set from an R.
        (r != 0).then(|| unsafe { from_addr::<R>(r) })
    }
}

pub static VIDEO_DISCOVERY: HookList<DiscoveryHook> = HookList::new();
pub static SENSOR_DISCOVERY: HookList<DiscoveryHook> = HookList::new();
pub static INPUT_DISCOVERY: HookList<DiscoveryHook> = HookList::new();
pub static REGISTER_SERVICE: HookList<DiscoveryHook> = HookList::new();
pub static CHANNEL_OPENED: HookList<ChannelOpenedHook> = HookList::new();
pub static UNEXPECTED_MSG: HookList<UnexpectedMsgHook> = HookList::new();
pub static SENSOR_REQUEST: HookList<SensorRequestHook> = HookList::new();
pub static INPUT_KEY: HookList<InputKeyHook> = HookList::new();
/// Registered but not dispatched: touch and rotary interposers are disabled.
pub static INPUT_TOUCH: HookList<InputTouchHook> = HookList::new();
/// Registered but never dispatched (as [`INPUT_TOUCH`]).
pub static INPUT_ROTARY: HookList<InputRotaryHook> = HookList::new();
pub static FUEL: HookList<FuelHook> = HookList::new();
pub static SPEED: HookList<SpeedHook> = HookList::new();
pub static VIDEO_FOCUS: HookList<VideoFocusHook> = HookList::new();
pub static VIDEO_FOCUS_GRANT: HookList<VideoFocusGrantHook> = HookList::new();
pub static TRANSPORT_READ: HookList<TransportReadHook> = HookList::new();
pub static TRANSPORT_WRITE: HookList<TransportWriteHook> = HookList::new();
pub static READ_PROVIDER: Provider<ReadProvider> = Provider::new();
pub static WRITE_PROVIDER: Provider<WriteProvider> = Provider::new();

pub use dispatch::*;

/// Dispatch, called by the loader's interposers.
// The pointer arguments are gal's own, passed through unchanged to the
// callbacks, which own their validity as gal's methods do. nothing here
// dereferences them. Hence the allow, scoped to exactly these functions.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub mod dispatch {
    use super::{
        DiscoveryHook, FuelData, HookList, KeyEvent, SpeedData, CHANNEL_OPENED, CP, FUEL,
        INPUT_KEY, P, READ_PROVIDER, SENSOR_REQUEST, SPEED, TRANSPORT_READ, TRANSPORT_WRITE,
        UNEXPECTED_MSG, VIDEO_FOCUS, VIDEO_FOCUS_GRANT, WRITE_PROVIDER,
    };

    /// Call every discovery-type callback of `list` with (this, arg).
    pub fn discovery(list: &HookList<DiscoveryHook>, this: P, arg: P) {
        for h in list.iter() {
            h(this, arg);
        }
    }

    pub fn channel_opened(router: P, ch: u8, req: P) {
        let list = &CHANNEL_OPENED;

        for h in list.iter() {
            h(router, ch, req);
        }
    }

    pub fn unexpected_msg(router: P, ch: u8) {
        let list = &UNEXPECTED_MSG;

        for h in list.iter() {
            h(router, ch);
        }
    }

    /// True when a callback handled the request (gal's handler is skipped).
    /// Every callback runs.
    pub fn sensor_request(this: P, req: CP) -> bool {
        let list = &SENSOR_REQUEST;
        let mut handled = false;
        for h in list.iter() {
            let r = h(this, req);
            handled |= r;
        }

        handled
    }

    /// True when a callback handled the key (gal's forward is skipped). Every
    /// callback runs.
    pub fn input_key(this: P, ev: &KeyEvent) -> bool {
        let list = &INPUT_KEY;
        let mut handled = false;
        for h in list.iter() {
            let r = h(this, ev);
            handled |= r;
        }

        handled
    }

    /// Let every callback rewrite the fuel values.
    pub fn fuel(this: P, d: &mut FuelData) {
        let list = &FUEL;
        for h in list.iter() {
            h(this, d);
        }
    }

    /// Let every callback rewrite the speed values.
    pub fn speed(this: P, d: &mut SpeedData) {
        let list = &SPEED;
        for h in list.iter() {
            h(this, d);
        }
    }

    pub fn video_focus(this: P, mode: i32, reason: i32, req: CP) {
        let list = &VIDEO_FOCUS;

        for h in list.iter() {
            h(this, mode, reason, req);
        }
    }

    pub fn video_focus_grant(this: P, mode: i32, unsolicited: i32) {
        let list = &VIDEO_FOCUS_GRANT;

        for h in list.iter() {
            h(this, mode, unsolicited);
        }
    }

    pub fn transport_read(this: P, buf: CP, len: u32, ret: i32) {
        let list = &TRANSPORT_READ;

        for h in list.iter() {
            h(this, buf, len, ret);
        }
    }

    pub fn transport_write(this: P, buf: CP, len: u32) {
        let list = &TRANSPORT_WRITE;

        for h in list.iter() {
            h(this, buf, len);
        }
    }

    /// The read provider's result, or None when no provider is set.
    pub fn read_provider(this: P, buf: P, len: u32) -> Option<i32> {
        let r = (READ_PROVIDER.get()?)(this, buf, len);

        Some(r)
    }

    /// The write provider's result, or None when no provider is set.
    pub fn write_provider(this: P, buf: CP, len: u32) -> Option<i32> {
        let r = (WRITE_PROVIDER.get()?)(this, buf, len);

        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static SEEN: AtomicU32 = AtomicU32::new(0);

    fn rust_key(_: P, ev: &KeyEvent) -> bool {
        SEEN.store(u32::try_from(ev.keycode).unwrap_or(0), Ordering::Relaxed);
        ev.keycode == 87
    }

    fn next_key(_: P, ev: &KeyEvent) -> bool {
        ev.keycode == 88
    }

    #[test]
    fn list_keeps_order_and_capacity() {
        let l: HookList<InputKeyHook> = HookList::new();
        l.register(rust_key);
        l.register(next_key);
        assert_eq!(l.count(), 2);
        let ev = KeyEvent {
            ts: 0,
            seq: 0,
            keycode: 87,
            down: true,
            extra: 0,
            long_press: false,
        };
        let results: Vec<bool> = l.iter().map(|f| f(core::ptr::null_mut(), &ev)).collect();
        assert_eq!(results, [true, false]);
        for _ in 0..10 {
            l.register(rust_key);
        }
        assert_eq!(l.count(), MAX_HOOKS);
    }

    #[test]
    fn input_key_reaches_rust_callback() {
        INPUT_KEY.register(rust_key);
        let mut ev = KeyEvent {
            ts: 0x0102_0304_0506_0708,
            seq: 0x0102_0304,
            keycode: 87,
            down: true,
            extra: 0,
            long_press: false,
        };
        assert!(input_key(core::ptr::null_mut(), &ev));
        assert_eq!(SEEN.load(Ordering::Relaxed), 87);
        ev.keycode = 88;
        assert!(!input_key(core::ptr::null_mut(), &ev));
        ev.keycode = 85;
        assert!(!input_key(core::ptr::null_mut(), &ev));
    }

    fn rust_read(_: P, _: P, len: u32) -> i32 {
        i32::try_from(len).unwrap_or(-1)
    }

    #[test]
    fn provider_last_set_wins() {
        let p: Provider<ReadProvider> = Provider::new();
        assert!(p.get().is_none());
        p.set(Some(rust_read));
        assert_eq!(
            p.get().unwrap()(core::ptr::null_mut(), core::ptr::null_mut(), 7),
            7
        );
        p.set(None);
        assert!(p.get().is_none());
    }

    fn rust_fuel(_: P, d: &mut FuelData) {
        d.level = 77;
    }

    #[test]
    fn fuel_hook_rewrites() {
        FUEL.register(rust_fuel);
        let mut d = FuelData {
            has_level: 1,
            level: 50,
            has_range: 1,
            range_m: 100_000,
            has_low: 0,
            low_fuel: 1,
        };
        fuel(core::ptr::null_mut(), &mut d);
        assert_eq!(d.level, 77);
    }
}
