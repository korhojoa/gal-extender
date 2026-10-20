// SPDX-License-Identifier: GPL-3.0-only
//! Existing native SDP/HSP trigger, using original Guard and owned storage.
//! Only the SDP parser runs synchronously in its callback: its response is
//! borrowed for that callback, as in native `SimClientSdpCallback`. All other
//! SDK operations run in the worker under Guard. The mailbox lock is never
//! held across any SDK call or parser invocation.
use super::{
    call1, log, put16, put32, read32, read8, remote_address, Event, Opaque, BT, ENVELOPE, EVENTS,
    RF_CLOSE, STOP,
};
use btsvc_core::wireless_policy::{Address, Policy};
use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

static QUERY: AtomicUsize = AtomicUsize::new(0);
static CHANNEL: AtomicUsize = AtomicUsize::new(0);
static REQUEST: [u8; 15] = [
    0x35, 6, 0x19, 0x11, 0x12, 0x19, 1, 0, 0, 100, 0x35, 3, 9, 0, 4,
];

pub(super) struct Nudge {
    query: &'static Opaque,
    channel: &'static Opaque,
    target: Option<(Address, u64)>,
    state: crate::nudge_state::State,
}
impl Nudge {
    pub(super) fn new() -> Self {
        let query = Box::leak(Box::new(Opaque::new()));
        let channel = Box::leak(Box::new(Opaque::new()));
        QUERY.store(query.pointer() as usize, Ordering::Release);
        CHANNEL.store(channel.pointer() as usize, Ordering::Release);
        Self {
            query,
            channel,
            target: None,
            state: crate::nudge_state::State::default(),
        }
    }

    // Caller holds original Guard and has checked current stack readiness.
    pub(super) unsafe fn start(&mut self, address: Address, policy: &Policy) {
        if !policy.accepts_bluetooth(&address) || !self.query.intact() || !self.channel.intact() {
            return;
        }
        let Some(remote) = resolve(address) else {
            log("nudge selected phone has no native service link");
            return;
        };
        if !self.state.begin() {
            return;
        }
        ptr::write_bytes(self.query.data.get().cast::<u8>(), 0, ENVELOPE);
        put32(self.query.pointer(), 8, remote);
        put16(self.query.pointer(), 20, 15);
        put32(self.query.pointer(), 16, REQUEST.as_ptr() as usize);
        put32(self.query.pointer(), 24, sdp_callback as *const () as usize);
        ptr::write(self.query.pointer().cast::<u8>().add(12), 6);
        self.target = Some((address, policy.epoch));
        let request: unsafe extern "C" fn(*mut c_void, u8) -> i32 =
            std::mem::transmute(0x1d_0c20_usize);
        let status = request(self.query.pointer(), 0);
        if status != 2 {
            self.state.query_failed();
        }
        log(&format!("nudge SDP status={status}"));
    }

    pub(super) unsafe fn ready(&mut self, number: u8, policy: Option<&Policy>) {
        if !self.state.query() {
            return;
        }
        self.state.query_failed();
        let Some((address, epoch)) = self.target else {
            return;
        };
        if !(1..=30).contains(&number)
            || !self.channel.intact()
            || !self.query.intact()
            || !policy.is_some_and(|p| p.epoch == epoch && p.accepts_bluetooth(&address))
        {
            return;
        }
        // Never consume a queued borrowed remote pointer. Re-resolve copied
        // identity against the currently connected pool under original Guard.
        let Some(remote) = resolve(address) else {
            return;
        };
        ptr::write_bytes(self.channel.data.get().cast::<u8>(), 0, ENVELOPE);
        put32(self.channel.pointer(), 8, rf_callback as *const () as usize);
        put16(self.channel.pointer(), 12, 512);
        // Move to Channel only after current policy/pool validation. Query
        // failure callbacks can no longer release this pending channel.
        let _ = self.state.begin();
        self.state.ready();
        let open: unsafe extern "C" fn(usize, u8, *mut c_void, u8) -> i32 =
            std::mem::transmute(0x27_74dc_usize);
        let status = open(remote, number, self.channel.pointer(), 7);
        if status != 2 {
            self.state.channel_closed();
        }
        log(&format!("nudge HSP AG channel={number} status={status}"));
    }

    pub(super) unsafe fn opened(&mut self) {
        if self.state.channel() && self.channel.intact() {
            call1(RF_CLOSE, self.channel.pointer());
        }
    }
    pub(super) fn query_failed(&mut self) {
        self.state.query_failed();
    }
    pub(super) fn channel_closed(&mut self) {
        self.state.channel_closed();
    }
    pub(super) unsafe fn cleanup(&mut self) {
        if self.state.channel() && self.channel.intact() {
            call1(RF_CLOSE, self.channel.pointer());
        }
        // Queries may still be native-owned. Do not clear, reuse or free them.
    }
}

unsafe fn resolve(address: Address) -> Option<usize> {
    (0..4).find_map(|index| {
        let remote = read32(BT + 0x1b_c6ec + index * 0xaf8);
        (remote_address(remote) == Some(address)
            && read8(remote + 12) == 1
            && read8(remote + 38) == 3)
            .then_some(remote)
    })
}

fn enqueue(event: &Event) {
    if STOP.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut queue) = EVENTS.lock() {
        if !queue.push(*event) {
            STOP.store(true, Ordering::Release);
        }
    } else {
        STOP.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn sdp_callback(event: *const u8) {
    if STOP.load(Ordering::Acquire) {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| {
        let query = QUERY.load(Ordering::Acquire);
        if event.is_null() || query == 0 || read32(event as usize + 8) != query {
            return;
        }
        match read8(event as usize) {
            70 => {
                // Exact native SimClient parse sequence, while borrowed result
                // lives. Copy only the one-byte result into the owned mailbox.
                put16(query as *mut c_void, 54, 4);
                put16(query as *mut c_void, 56, 3);
                let parse: unsafe extern "C" fn(usize) -> i32 =
                    std::mem::transmute(0x1d_0228_usize);
                let status = parse(query);
                let number = if status == 0
                    && read32(query + 60) == 1
                    && ptr::read_unaligned((query + 64) as *const u16) == 1
                {
                    let response = read32(query + 68);
                    if response == 0 {
                        0
                    } else {
                        read8(response)
                    }
                } else {
                    0
                };
                enqueue(&Event::NudgeReady(number));
            }
            71 | 72 => enqueue(&Event::NudgeQueryFailed),
            _ => {}
        }
    }))
    .is_err()
    {
        STOP.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn rf_callback(channel: usize, event: *const u8) {
    if STOP.load(Ordering::Acquire) {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| {
        if event.is_null() || channel != CHANNEL.load(Ordering::Acquire) {
            return;
        }
        match read8(event as usize) {
            2 => enqueue(&Event::NudgeOpen),
            // CLOSE_IND is not proof that native storage is reusable.
            4 => enqueue(&Event::NudgeChannelClosed),
            _ => {}
        }
    }))
    .is_err()
    {
        STOP.store(true, Ordering::Release);
    }
}
