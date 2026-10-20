// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! `MessageRouter` forwarding. Protocol negotiation stays with the unit.

use core::ffi::{c_int, c_uint, c_void};

use qnx_dl::NextFn;

type P = *mut c_void;

// gal's real MessageRouter methods, found through RTLD_NEXT on first use.
static MR_UNEXPECTED: NextFn<unsafe extern "C" fn(P, u8) -> c_int> =
    // SAFETY: MessageRouter::sendUnexpectedMessage(this, uint8_t).
    unsafe { NextFn::new(c"_ZN13MessageRouter21sendUnexpectedMessageEh") };
static MR_QUEUE_UNENC: NextFn<unsafe extern "C" fn(P, u8, P, c_uint) -> c_int> =
    // SAFETY: MessageRouter::queueOutgoingUnencrypted(this, u8, void*, unsigned).
    unsafe { NextFn::new(c"_ZN13MessageRouter24queueOutgoingUnencryptedEhPvj") };
static MR_ROUTE: NextFn<unsafe extern "C" fn(P, u8, P) -> c_int> =
    // SAFETY: MessageRouter::routeMessage(this, u8, const shared_ptr<IoBuffer>&).
    unsafe { NextFn::new(c"_ZN13MessageRouter12routeMessageEhRK10shared_ptrI8IoBufferE") };

// Exported by entry.rs as "_ZN13MessageRouter21sendUnexpectedMessageEh".
pub extern "C" fn message_router_send_unexpected_message(mr: P, ch: u8) -> c_int {
    let rc = match MR_UNEXPECTED.get() {
        None => 0,
        // SAFETY: gal's original MessageRouter method and arguments.
        Some(f) => unsafe { f(mr, ch) },
    };
    gal_feature_api::hooks::dispatch::unexpected_msg(mr, ch);
    rc
}

// Exported by entry.rs as "_ZN13MessageRouter24queueOutgoingUnencryptedEhPvj".
pub extern "C" fn message_router_queue_outgoing_unencrypted(
    mr: P,
    ch: u8,
    buf: P,
    len: c_uint,
) -> c_int {
    let Some(f) = MR_QUEUE_UNENC.get() else {
        return 0;
    };
    // SAFETY: gal's original method receives its unchanged buffer and length.
    unsafe { f(mr, ch, buf, len) }
}

// Exported by entry.rs as "_ZN13MessageRouter12routeMessageEhRK10shared_ptrI8IoBufferE"
pub extern "C" fn message_router_route_message(mr: P, ch: u8, iobuf_sp: P) -> c_int {
    let Some(f) = MR_ROUTE.get() else {
        return 0;
    };
    // SAFETY: gal's MessageRouter::routeMessage(this, u8, const shared_ptr<IoBuffer>&).
    unsafe { f(mr, ch, iobuf_sp) }
}
