// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Byte offsets in gal's objects that the extension libraries read or write (32-bit ARM).
//! Each constant names its proof. The fake endpoint's own fields are in
//! [`crate::endpoint`].

/// The shared `IoBuffer` data pointer is at offset4. This module does not read offset0.
pub const SHARED_PTR_IOBUFFER: usize = 4;

/// `IoBuffer` data allocation base pointer.
pub const IOBUFFER_DATA: usize = 0;
/// `IoBuffer` allocation capacity. This is not the number of message bytes.
pub const IOBUFFER_CAPACITY: usize = 4;
/// `IoBuffer` start index. The message range is data plus start through data plus end.
pub const IOBUFFER_START: usize = 8;
/// `IoBuffer` end index.
pub const IOBUFFER_END: usize = 12;

/// `Controller::routeMessage` skips the two-byte type before protobuf parsing.
pub const CONTROLLER_MESSAGE_HEADER: usize = 2;
/// Controller callback handler pointer within the native object.
pub const CONTROLLER_CALLBACK: usize = 256;
/// Callback error latch: initialized to zero, set to one on unrecoverable error.
/// Check the callback virtual table before reading this field.
pub const CONTROLLER_CALLBACK_ERROR: usize = 4;

/// `MessageRouter` endpoint pointer table, indexed by service index.
pub const ROUTER_ENDPOINT_TABLE: usize = 0x100;
/// `MessageRouter` `ChannelManager` pointer. shutdown clears it. queueOutgoing requires a
/// non-null pointer.
pub const ROUTER_CHANNEL_MANAGER: usize = 0x4fc;

/// `SensorSource` started byte. Send only when nonzero.
pub const SENSOR_SOURCE_STARTED: usize = 4;
/// `SensorSource` sensor channel identifier byte.
pub const SENSOR_SOURCE_CHANNEL: usize = 5;
/// `SensorSource` `MessageRouter` pointer.
pub const SENSOR_SOURCE_ROUTER: usize = 8;

/// `SensorRequest` requested sensor type as a signed integer.
pub const SENSOR_REQUEST_TYPE: usize = 0x30;

/// `ChannelOpenRequest` service-index byte read by the native channel-open handler.
pub const CHANNEL_OPEN_REQUEST_SERVICE: usize = 0x2c;

/// `VideoSink` wire channel byte stored as the service ID.
pub const VIDEO_SINK_CHANNEL: usize = 12;

/// `VideoFocusRequestNotification`: has-bits word. mode is valid when bit
/// 0x4 is set. RE-confirmed (crates/gal-loader/src/gal.rs `decode_focus`).
pub const VIDEO_FOCUS_REQUEST_HASBITS: usize = 0x20;
/// `VideoFocusRequestNotification`: reason.
pub const VIDEO_FOCUS_REQUEST_REASON: usize = 0x2c;
/// `VideoFocusRequestNotification`: mode.
pub const VIDEO_FOCUS_REQUEST_MODE: usize = 0x30;

/// The frame bytes of a `const shared_ptr<IoBuffer>&`, by the offsets above.
/// None for a NULL pointer, a NULL `IoBuffer` or data, or `end <= start`.
///
/// # Safety
/// `sp` is NULL or gal's `shared_ptr<IoBuffer>` reference, valid for the
/// returned lifetime.
#[must_use]
pub unsafe fn frame_of<'a>(sp: *const u8) -> Option<&'a [u8]> {
    if sp.is_null() {
        return None;
    }
    let io = sp
        .add(SHARED_PTR_IOBUFFER)
        .cast::<*const u8>()
        .read_unaligned();
    if io.is_null() {
        return None;
    }
    let data = io.add(IOBUFFER_DATA).cast::<*const u8>().read_unaligned();
    let start = io.add(IOBUFFER_START).cast::<u32>().read_unaligned() as usize;
    let end = io.add(IOBUFFER_END).cast::<u32>().read_unaligned() as usize;
    if data.is_null() || end <= start {
        return None;
    }
    Some(core::slice::from_raw_parts(data.add(start), end - start))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 32-bit layout. runs on the target (the QNX ARM bench).
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn frame_of_reads_start_and_end() {
        #[repr(C)]
        struct Io {
            data: *const u8,
            cap: u32,
            start: u32,
            end: u32,
        }
        let data = [0x80u8, 0x06, 0xaa, 0xbb, 0xcc];
        let io = Io {
            data: data.as_ptr(),
            cap: 64,
            start: 1,
            end: 5,
        };
        let sp: [*const Io; 2] = [core::ptr::null(), &raw const io];
        // SAFETY: laid out as frame_of expects on a 32-bit target.
        let f = unsafe { frame_of(sp.as_ptr().cast()) };
        assert_eq!(f, Some(&data[1..5]));
        assert_eq!(io.cap, 64);
    }

    #[test]
    fn null_is_none() {
        // SAFETY: NULL is allowed.
        assert!(unsafe { frame_of(core::ptr::null()) }.is_none());
    }
}
