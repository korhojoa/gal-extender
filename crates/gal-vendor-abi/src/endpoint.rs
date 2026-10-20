// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Owned `ProtocolEndpointBase` layout and callbacks. The primary vptr begins at slot0.
//! The open byte is at offset4, wire channel at5, service index at12 and routing flag
//! at16. The storage has 64 bytes. this is allocated capacity, not a claim about the
//! native class size. Slot2 returns whether the channel was open so the receiver can
//! clear its routing map. Slot3 allows an eligible channel to open. Slot5 returns
//! nonzero for a handled routed message.

use crate::Ptr;

/// Byte offset of the vptr.
pub const VPTR: usize = 0;
/// Byte offset of `is_open` (u8).
pub const IS_OPEN: usize = 4;
/// Byte offset of the opened wire channel id (u8), set by `onChannelOpened`.
pub const OPEN_CHANNEL: usize = 5;
/// Byte offset of the service index (u8) `registerService` files it under.
pub const SERVICE_INDEX: usize = 12;
/// Byte offset of the flags byte: 0 routeMessage, non-zero handleRawMessage.
pub const FLAGS: usize = 16;

/// Slot 0/1: complete and deleting destructor, `void(void*)`.
pub type DtorFn = unsafe extern "C" fn(Ptr);
/// Slot 2: `int onChannelClosed(void* ep, uint8_t ch)` (see module docs).
pub type OnChannelClosedFn = unsafe extern "C" fn(Ptr, u8) -> i32;
/// Slot 3: `int mayOpenChannel(void* ep, uint8_t ch)`. 1 = yes.
pub type MayOpenChannelFn = unsafe extern "C" fn(Ptr, u8) -> i32;
/// Slot 4: onChannelOpened receives the endpoint and an eight-bit wire channel
/// identifier. It returns no value.
pub type OnChannelOpenedFn = unsafe extern "C" fn(Ptr, u8);
/// Route one message through endpoint slot5. The shared `IoBuffer` pointer can be null.
/// Return nonzero when handled.
pub type RouteMessageFn = unsafe extern "C" fn(Ptr, u8, u16, Ptr) -> i32;
/// Slot 6: handleRawMessage receives the endpoint, wire channel identifier and a shared
/// `IoBuffer` pointer. It returns no value.
pub type HandleRawMessageFn = unsafe extern "C" fn(Ptr, u8, Ptr);
/// Add discovery information through endpoint slot7.
pub type AddDiscoveryInfoFn = unsafe extern "C" fn(Ptr, Ptr);

/// The eight endpoint virtual slots in native order.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vtable {
    pub dtor_complete: DtorFn,
    pub dtor_deleting: DtorFn,
    pub on_channel_closed: OnChannelClosedFn,
    pub may_open_channel: MayOpenChannelFn,
    pub on_channel_opened: OnChannelOpenedFn,
    pub route_message: RouteMessageFn,
    pub handle_raw_message: HandleRawMessageFn,
    pub add_discovery_info: AddDiscoveryInfoFn,
}

/// The fake endpoint object as the extension libraries lay it out in 64 bytes.
#[repr(C, align(8))]
pub struct FakeEndpoint {
    pub vptr: *const Vtable,
    pub is_open: u8,
    pub open_channel: u8,
    _pad6: [u8; 6],
    pub service_index: u8,
    _pad13: [u8; 3],
    pub flags: u8,
    _tail: [u8; 64 - 17],
}

impl FakeEndpoint {
    /// Create a closed endpoint for the specified service index and routing flag.
    #[must_use]
    pub const fn new(vtable: *const Vtable, service_index: u8, flags: u8) -> Self {
        FakeEndpoint {
            vptr: vtable,
            is_open: 0,
            open_channel: 0,
            _pad6: [0; 6],
            service_index,
            _pad13: [0; 3],
            flags,
            _tail: [0; 64 - 17],
        }
    }

    /// The pointer `registerService` takes.
    #[must_use]
    pub fn as_ptr(&mut self) -> Ptr {
        core::ptr::from_mut(self).cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn endpoint_layout_matches_native_abi() {
        use core::mem::offset_of;
        assert_eq!(size_of::<FakeEndpoint>(), 64);
        assert_eq!(size_of::<Vtable>(), 8 * 4);
        assert_eq!(offset_of!(Vtable, on_channel_closed), 8);
        assert_eq!(offset_of!(Vtable, may_open_channel), 12);
        assert_eq!(offset_of!(Vtable, route_message), 20);
        assert_eq!(offset_of!(FakeEndpoint, vptr), VPTR);
        assert_eq!(offset_of!(FakeEndpoint, is_open), IS_OPEN);
        assert_eq!(offset_of!(FakeEndpoint, open_channel), OPEN_CHANNEL);
        assert_eq!(offset_of!(FakeEndpoint, service_index), SERVICE_INDEX);
        assert_eq!(offset_of!(FakeEndpoint, flags), FLAGS);
    }

    /// The host is 64-bit, so only the constructor is checked here. the byte
    /// layout is the 32-bit test above (run on the QNX ARM bench).
    #[test]
    fn constructor() {
        assert!(size_of::<FakeEndpoint>() >= 64);
        let ep = FakeEndpoint::new(core::ptr::null(), 9, 1);
        assert_eq!((ep.service_index, ep.flags, ep.is_open), (9, 1, 0));
    }
}
