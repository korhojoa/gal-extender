// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Additive Android Auto vendor-extension channel.
//!
//! Discovery reserves a free channel and registers an owned endpoint. Only
//! a successful registration adds `org.korho.gal.tunnel` to discovery.
//! The stock EXLAP channel remains untouched.
//!
//! Incoming frames are bounds-checked and copied atomically to the shared
//! `/aa-uplink` ring. A pump forwards outgoing ring frames through the native
//! message router. The companion daemon creates the ring and handles relay.
//! The endpoint returns its open state on close. slots three and five return
//! one. Ring barriers and message-router offsets are part of this contract.
//!
//! Callbacks and the pump contain panics. Shutdown stops and joins the pump
//! before dropping session-owned objects. Construction and transport status
//! do not prove a working phone connection.

pub mod ring;
pub mod uplink;
pub mod wire;

use core::cell::UnsafeCell;
use core::ffi::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};

use gal_feature_api::{config, hooks, FeatureModule};
use gal_vendor_abi::endpoint::{FakeEndpoint, Vtable};
use gal_vendor_abi::Ptr;

use uplink::{up_log, up_loge, Banner, CBytes, Uplink};

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule {
    name: "aauplink",
    init,
};

/// The one uplink of the process.
static UPLINK: Uplink = Uplink::new(ring::SHM_NAME);

/// Run a callback body. a panic is logged and `fallback` returned.
fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    if let Ok(r) = catch_unwind(AssertUnwindSafe(f)) {
        r
    } else {
        up_loge!("[GalAAUp] PANIC contained in {}; gal continues", site);
        fallback
    }
}

// ---------------------------------------------------------------------------
// The fake endpoint and its vtable (flags = 1: gal routes the channel's
// traffic to slot 6 handleRawMessage)
// ---------------------------------------------------------------------------

struct EpCell(UnsafeCell<FakeEndpoint>);
// SAFETY: written in on_video_discovery before registerService publishes it
// to gal. after that only gal's thread touches it through the slots.
unsafe impl Sync for EpCell {}

static EP: EpCell = EpCell(UnsafeCell::new(FakeEndpoint::new(&raw const VTABLE, 0, 0)));

/// Slots 0, 1: the destructors (the object is static, nothing to do).
unsafe extern "C" fn ep_dtor(_: Ptr) {}

unsafe extern "C" fn ep_add_discovery_info(_: Ptr, _: Ptr) {}

unsafe extern "C" fn ep_on_channel_closed(ep: Ptr, ch: u8) -> i32 {
    // was_open is read first, so a panic after it still reports it.
    let was_open = !ep.is_null() && *ep.cast::<u8>().add(gal_vendor_abi::endpoint::IS_OPEN) != 0;
    guarded("onChannelClosed", i32::from(was_open), || {
        // SAFETY: gal passes our endpoint.
        unsafe { UPLINK.on_channel_closed(ep.cast(), ch) }
    })
}

unsafe extern "C" fn ep_may_open_channel(_: Ptr, ch: u8) -> i32 {
    guarded("mayOpenChannel", 1, || UPLINK.may_open_channel(ch))
}

unsafe extern "C" fn ep_on_channel_opened(ep: Ptr, ch: u8) {
    guarded("onChannelOpened", (), || {
        // SAFETY: gal passes our endpoint.
        unsafe { UPLINK.on_channel_opened(ep.cast(), ch) }
    });
}

unsafe extern "C" fn ep_route_message(_: Ptr, ch: u8, t: u16, _: Ptr) -> i32 {
    guarded("routeMessage", 1, || UPLINK.route_message(ch, t))
}

unsafe extern "C" fn ep_handle_raw_message(_: Ptr, ch: u8, sptr: Ptr) {
    guarded("handleRawMessage", (), || {
        // SAFETY: gal's shared_ptr<IoBuffer>*.
        unsafe { UPLINK.handle_raw_message(ch, sptr.cast()) }
    });
}

static VTABLE: Vtable = Vtable {
    dtor_complete: ep_dtor,
    dtor_deleting: ep_dtor,
    on_channel_closed: ep_on_channel_closed,
    may_open_channel: ep_may_open_channel,
    on_channel_opened: ep_on_channel_opened,
    route_message: ep_route_message,
    handle_raw_message: ep_handle_raw_message,
    add_discovery_info: ep_add_discovery_info,
};

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

fn on_register_service(this: Ptr, ep: Ptr) {
    guarded("registerService hook", (), || {
        // SAFETY: gal's ProtocolEndpointBase* (or NULL).
        unsafe { UPLINK.on_register_service(this, ep) }
    });
}

fn on_channel_opened(mr: Ptr, ch: u8, _req: Ptr) {
    guarded("channel opened hook", (), || {
        UPLINK.on_hook_channel_opened(mr, ch);
    });
}

fn on_video_discovery(this: Ptr, resp: Ptr) {
    guarded("video discovery hook", (), || {
        // SAFETY: gal's VideoSink* and ServiceDiscoveryResponse*. EP is ours.
        unsafe { UPLINK.on_video_discovery(this, resp, EP.0.get(), &raw const VTABLE) }
    });
}

extern "C" {
    fn atexit(f: extern "C" fn()) -> c_int;
}

extern "C" fn at_exit() {
    guarded("shutdown", (), || UPLINK.shutdown());
}

// ---------------------------------------------------------------------------
// init (the loader calls it once, on the constructor thread, panic-guarded)
// ---------------------------------------------------------------------------

fn init() {
    UPLINK.reset();
    UPLINK.read_config(config::get_int);
    UPLINK.warm_up();

    up_log!(
        "[GalAAUp] {} ring='{}' abi={} magic={:08x} slots={} slot={} svc='{}' pkg='{}' ch=<pending ServiceDiscoveryResponse>",
        Banner,
        ring::SHM_NAME.to_str().unwrap_or("?"),
        ring::ABI,
        ring::MAGIC,
        ring::SLOTS,
        ring::SLOT_DATA,
        CBytes(uplink::service_name()),
        CBytes(uplink::package())
    );

    hooks::REGISTER_SERVICE.register(on_register_service);
    hooks::CHANNEL_OPENED.register(on_channel_opened);
    hooks::VIDEO_DISCOVERY.register(on_video_discovery);

    // SAFETY: a plain extern "C" fn with no arguments.
    unsafe { atexit(at_exit) };
}

#[cfg(test)]
mod tests;
