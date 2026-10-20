// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa
//! Incoming phone focus policy. Unsupported wire forms keep the native route.

use core::ffi::c_int;
use protobuf_wire::{varint, wire};

pub const MAX_PAYLOAD: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Native,
    Projected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    TooLarge,
    Malformed,
    Unsupported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardReason {
    Disabled,
    Unrelated,
    Payload,
    Wire(Reject),
    Native,
    Eligibility,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Forward(ForwardReason),
    GrantProjected,
    SuppressLatched,
}

// Limit the shared reader to canonical uint32 encodings. In particular, never
// let its wide-varint truncation normalize an unsupported enum into PROJECTED.
fn scalar(bytes: &[u8]) -> Result<u64, Reject> {
    let (value, width) = varint::read(bytes).ok_or(Reject::Malformed)?;
    if width > 5 || value > u64::from(u32::MAX) || varint::width(value) != width {
        return Err(Reject::Unsupported);
    }
    Ok(value)
}

pub fn classify_focus(payload: &[u8]) -> Result<Focus, Reject> {
    if payload.len() > MAX_PAYLOAD {
        return Err(Reject::TooLarge);
    }
    let mut focus = Focus::Native;
    let mut pos = 0;
    while pos < payload.len() {
        let tag = scalar(&payload[pos..])?;
        let number = tag >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err(Reject::Malformed);
        }
        if !matches!(tag & 7, 0 | 1 | 2 | 5) {
            return Err(Reject::Unsupported);
        }
        let field = wire::read_field(payload, pos, payload.len()).ok_or(Reject::Malformed)?;
        match field.wire {
            wire::VARINT => {
                let value = scalar(&payload[field.value..field.end()])?;
                if field.number == 1 {
                    match value {
                        1 => focus = Focus::Native,
                        2 => focus = Focus::Projected,
                        _ => {} // Native proto2 preserves the last accepted enum.
                    }
                }
            }
            wire::LEN => {
                scalar(&payload[field.tag_end..field.value])?;
            }
            _ => {}
        }
        pos = field.end();
    }
    Ok(focus)
}

pub fn route_with(
    enabled: bool,
    message_type: u16,
    payload: Option<&[u8]>,
    eligible: Option<bool>,
    original: impl FnOnce() -> c_int,
    grant: impl FnOnce(),
) -> (c_int, Decision) {
    let reason = if !enabled {
        ForwardReason::Disabled
    } else if message_type != 13 {
        ForwardReason::Unrelated
    } else {
        match payload {
            None => ForwardReason::Payload,
            Some(bytes) => match classify_focus(bytes) {
                Err(error) => ForwardReason::Wire(error),
                Ok(Focus::Native) => ForwardReason::Native,
                Ok(Focus::Projected) => match eligible {
                    None => ForwardReason::Eligibility,
                    Some(false) => return (0, Decision::SuppressLatched),
                    Some(true) => {
                        grant();
                        return (0, Decision::GrantProjected);
                    }
                },
            },
        }
    };
    (original(), Decision::Forward(reason))
}

#[cfg(test)]
#[path = "nav_focus_tests.rs"]
mod tests;

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use gal_vendor_abi::offsets;
use qnx_dl::NextFn;

type P = *mut c_void;
static ENABLED: AtomicBool = AtomicBool::new(false);
// SAFETY: exact native signatures, established by Controller::routeMessage
// e43ac and setNavigationFocus e2d88.
static ROUTE: NextFn<unsafe extern "C" fn(P, u8, u16, P) -> c_int> =
    unsafe { NextFn::new(c"_ZN10Controller12routeMessageEhtRK10shared_ptrI8IoBufferE") };
static GRANT: NextFn<unsafe extern "C" fn(P, c_int)> =
    unsafe { NextFn::new(c"_ZN10Controller18setNavigationFocusE12NavFocusType") };

pub fn configure(value: c_int) {
    ENABLED.store(value != 0, Ordering::Relaxed);
    crate::ldr_log!("nav-focus: incoming PROJECTED suppression={}", value != 0);
}

/// A native frame includes the two-byte message type. Bound the body before
/// constructing its slice. unsupported layouts go through the original route.
fn body_range(start: usize, end: usize, capacity: usize) -> Option<(usize, usize)> {
    if start > end || end > capacity {
        return None;
    }
    let body = start.checked_add(offsets::CONTROLLER_MESSAGE_HEADER)?;
    let len = end.checked_sub(body)?;
    (len <= MAX_PAYLOAD).then_some((body, len))
}

// SAFETY: only called for the native route's live const shared_ptr<IoBuffer>&.
unsafe fn payload_of<'a>(shared: P) -> Option<&'a [u8]> {
    if shared.is_null() {
        return None;
    }
    // SAFETY: shared is the original reference. all layout offsets are native
    // ARM32 fields. No pointer is retained beyond this invocation.
    unsafe {
        let io = shared
            .cast::<u8>()
            .add(offsets::SHARED_PTR_IOBUFFER)
            .cast::<*const u8>()
            .read_unaligned();
        if io.is_null() {
            return None;
        }
        let data = io
            .add(offsets::IOBUFFER_DATA)
            .cast::<*const u8>()
            .read_unaligned();
        let start = io
            .add(offsets::IOBUFFER_START)
            .cast::<u32>()
            .read_unaligned() as usize;
        let end = io.add(offsets::IOBUFFER_END).cast::<u32>().read_unaligned() as usize;
        let capacity = io
            .add(offsets::IOBUFFER_CAPACITY)
            .cast::<u32>()
            .read_unaligned() as usize;
        body_range(start, end, capacity)?;
        if data.is_null() || (data as usize).checked_add(end).is_none() {
            return None;
        }
        offsets::frame_of(shared.cast())?.get(offsets::CONTROLLER_MESSAGE_HEADER..)
    }
}

fn callback_table_matches(vptr: *const u8, table: *const u8) -> bool {
    !vptr.is_null() && !table.is_null() && (table as usize).checked_add(8) == Some(vptr as usize)
}

fn reply_eligibility(eligibility: Option<bool>, reply_available: bool) -> Option<bool> {
    match eligibility {
        Some(true) if !reply_available => None,
        other => other,
    }
}

// SAFETY: this is the native Controller passed to its original route method.
unsafe fn eligible(this: P) -> Option<bool> {
    if this.is_null() {
        return None;
    }
    let table = qnx_dl::sym_default(c"_ZTVN3gal26CControllerCallbackHandlerE");
    if table.is_null() {
        return None;
    }
    // SAFETY: native Controller e4624 loads a direct callback pointer at256.
    // Require the concrete GAL vtable address point before reading its byte4.
    unsafe {
        let callback = this
            .cast::<u8>()
            .add(offsets::CONTROLLER_CALLBACK)
            .cast::<*const u8>()
            .read_unaligned();
        if callback.is_null() {
            return None;
        }
        let vptr = callback.cast::<*const u8>().read_unaligned();
        if !callback_table_matches(vptr, table.cast()) {
            return None;
        }
        Some(callback.add(offsets::CONTROLLER_CALLBACK_ERROR).read() == 0)
    }
}

// Exported only through entry.rs's stack-realigning ARM trampoline.
pub extern "C" fn controller_route_message(this: P, channel: u8, kind: u16, shared: P) -> c_int {
    // Contain only Rust panics. Never retry an original operation after a possible
    // reply side effect. foreign exceptions are outside this guard contract.
    crate::guard("nav-focus route", || {
        route_native(this, channel, kind, shared)
    })
    .unwrap_or(-253)
}

fn route_native(this: P, channel: u8, kind: u16, shared: P) -> c_int {
    let Some(original) = ROUTE.get() else {
        crate::ldr_loge!("nav-focus: original Controller route missing; refusing");
        return -253;
    };
    let enabled = ENABLED.load(Ordering::Relaxed);
    // Disabled and unrelated traffic is forwarded without reading private data.
    if !enabled || kind != 13 {
        // SAFETY: original receives every unchanged native argument exactly once.
        return unsafe { original(this, channel, kind, shared) };
    }
    let grant = GRANT.get();
    // SAFETY: these are the original Controller and shared buffer references.
    let (payload, eligibility) = unsafe { (payload_of(shared), eligible(this)) };
    // A missing reply keeps the original route for eligible traffic, but must
    // not override the native error latch's no-callback/no-grant decision.
    let eligibility = reply_eligibility(eligibility, grant.is_some());
    let (result, decision) = route_with(
        true,
        kind,
        payload,
        eligibility,
        // SAFETY: original references and ABI, forwarded unchanged once.
        || unsafe { original(this, channel, kind, shared) },
        // SAFETY: established native setter ABI and enum PROJECTED=2. It has
        // baseline native allocation behavior, not a Rust exception boundary.
        || {
            // reply_eligibility permits this branch only with a resolved setter.
            if let Some(grant) = grant {
                unsafe { grant(this, 2) };
            }
        },
    );
    match decision {
        Decision::GrantProjected => crate::ldr_log!("nav-focus: phone PROJECTED withheld; native PROJECTED grant attempted; Exbox/native-guidance HMI actions skipped"),
        Decision::SuppressLatched => crate::ldr_log!("nav-focus: phone PROJECTED withheld; native error latch set; no grant; Exbox/native-guidance HMI actions skipped"),
        Decision::Forward(reason) => crate::ldr_log!("nav-focus: incoming request forwarded ({reason:?})"),
    }
    result
}
