// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Real USBDI calls always pass through unchanged. The optional `wireless`
//! build feature adds accessory synthesis. default builds contain no synthetic
//! descriptors, handles or topology rewrite. The insertion ABI returns 0 when
//! that feature is absent. Struct layouts: sys/usbdi.h in the QNX 6.5 SDP.

use core::ffi::{c_int, c_void};

use crate::sys::Next;
#[cfg(feature = "wireless")]
use crate::{ldr_log, ldr_loge};

#[repr(C)]
pub struct DeviceIdent {
    pub vendor: u32,
    pub device: u32,
    pub dclass: u32,
    pub subclass: u32,
    pub protocol: u32,
}

#[repr(C)]
pub struct DeviceInstance {
    pub path: u8,
    pub devno: u8,
    pub generation: u16,
    pub ident: DeviceIdent,
    pub config: u32,
    pub iface: u32,
    pub alternate: u32,
}

#[repr(C)]
pub struct Funcs {
    pub nentries: u32,
    pub insertion: usize,
    pub removal: usize,
    pub event: usize,
}

/// `usbd_connect_parm_t` up to `funcs` (only read through a pointer).
#[repr(C)]
pub struct ConnectParm {
    pub path: *const c_void,
    pub vusb: u16,
    pub vusbd: u16,
    pub flags: u32,
    pub argc: i32,
    pub argv: *mut c_void,
    pub evtbufsz: u32,
    pub ident: *mut c_void,
    pub funcs: *mut Funcs,
}

#[repr(C)]
pub struct PortAttachment {
    pub upstream_devno: u8,
    pub upstream_port: u8,
    pub upstream_port_speed: u8,
    pub upstream_hc: u8,
    pub reserved: [u8; 4],
}

#[repr(C)]
pub struct BusTopology {
    pub ports: [PortAttachment; 64],
}

type P = *mut c_void;
#[cfg(feature = "wireless")]
mod synthetic {
    use super::{c_int, c_void, DeviceInstance, P};
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
    use std::cell::UnsafeCell;

    pub(super) const MAGIC_PATH: u8 = 0x7e;
    /// Indexes `usbd_bus_topology_t::ports[64]` in gal's high-speed check, so it
    /// must be <= 63. 62 avoids a real enumerated device.
    pub(super) const MAGIC_DEVNO: u8 = 0x3e;

    pub(super) type InsertionFn = unsafe extern "C" fn(P, *mut DeviceInstance);

    pub(super) static CONN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    pub(super) static INSERTION: AtomicUsize = AtomicUsize::new(0);
    /// Set once the synthetic insertion fires. gates `topology_ext`, which has no
    /// device argument to match on.
    pub(super) static SYNTH_ACTIVE: AtomicBool = AtomicBool::new(false);

    /// Opaque handles gal only stores and hands back. Oversized and zeroed so any
    /// scratch gal writes into them lands here.
    pub(super) struct Scratch<const N: usize>(UnsafeCell<[u8; N]>);
    // SAFETY: only the address is used here. gal owns the contents.
    unsafe impl<const N: usize> Sync for Scratch<N> {}
    impl<const N: usize> Scratch<N> {
        pub(super) fn ptr(&self) -> P {
            self.0.get().cast()
        }
    }
    pub(super) static FAKE_DEV: Scratch<512> = Scratch(UnsafeCell::new([0; 512]));
    pub(super) static FAKE_PIPE_IN: Scratch<128> = Scratch(UnsafeCell::new([0; 128]));
    pub(super) static FAKE_PIPE_OUT: Scratch<128> = Scratch(UnsafeCell::new([0; 128]));

    // AOAP accessory: device 18D1:2D00, one vendor-specific interface, bulk
    // 0x81 IN / 0x01 OUT at 512 bytes. Raw little-endian USB layouts.
    static DESC_DEVICE: [u8; 18] = [
        18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 64, 0xD1, 0x18, 0x00, 0x2D, 0x00, 0x01, 1, 2, 3, 1,
    ];
    static DESC_CONFIG: [u8; 9] = [9, 0x02, 32, 0x00, 1, 1, 0, 0x80, 0x32];
    static DESC_IFACE: [u8; 9] = [9, 0x04, 0, 0, 2, 0xFF, 0xFF, 0x00, 0];
    static DESC_EP_IN: [u8; 7] = [7, 0x05, 0x81, 0x02, 0x00, 0x02, 0];
    static DESC_EP_OUT: [u8; 7] = [7, 0x05, 0x01, 0x02, 0x00, 0x02, 0];

    /// The descriptor for (type, index) of the synthetic device.
    pub(super) fn descriptor(kind: u8, index: c_int) -> Option<&'static [u8]> {
        match (kind, index) {
            (0x01, _) => Some(&DESC_DEVICE),
            (0x02, _) => Some(&DESC_CONFIG),
            (0x04, _) => Some(&DESC_IFACE),
            (0x05, 0) => Some(&DESC_EP_IN),
            (0x05, 1) => Some(&DESC_EP_OUT),
            _ => None,
        }
    }
}
#[cfg(feature = "wireless")]
use core::sync::atomic::Ordering;
#[cfg(feature = "wireless")]
use synthetic::{
    descriptor, InsertionFn, CONN, FAKE_DEV, FAKE_PIPE_IN, FAKE_PIPE_OUT, INSERTION, MAGIC_DEVNO,
    MAGIC_PATH, SYNTH_ACTIVE,
};

static USBD_CONNECT: Next = Next::new(c"usbd_connect");
static USBD_ATTACH: Next = Next::new(c"usbd_attach");
static USBD_PARSE: Next = Next::new(c"usbd_parse_descriptors");
static USBD_OPEN_PIPE: Next = Next::new(c"usbd_open_pipe");
static USBD_TOPOLOGY: Next = Next::new(c"usbd_topology_ext");

pub extern "C" fn usbd_connect(parm: *mut ConnectParm, connection: *mut P) -> c_int {
    let p = USBD_CONNECT.get();
    let rc = if p.is_null() {
        -1
    } else {
        // SAFETY: libusbdi's usbd_connect(parm, connection**).
        unsafe {
            core::mem::transmute::<P, unsafe extern "C" fn(*mut ConnectParm, *mut P) -> c_int>(p)(
                parm, connection,
            )
        }
    };
    // SAFETY: parm and connection are gal's valid pointers when rc == 0.
    #[cfg(feature = "wireless")]
    unsafe {
        if rc == 0 && !parm.is_null() && !(*parm).funcs.is_null() && !connection.is_null() {
            let conn = *connection;
            let ins = (*(*parm).funcs).insertion;
            CONN.store(conn, Ordering::Release);
            INSERTION.store(ins, Ordering::Release);
            ldr_log!(
                "[GalLoader] usbd_connect captured: conn={:p} insertion={:#x}",
                conn,
                ins
            );
        }
    }
    rc
}

/// Fire the synthetic accessory insertion into gal's USBDI client (installed
/// as `LoaderServices::usbdi_fire_insertion`). 1 when fired, 0 before gal
/// has called `usbd_connect`.
#[cfg(feature = "wireless")]
pub fn fire_insertion(vendor: u32, product: u32) -> c_int {
    let conn = CONN.load(Ordering::Acquire);
    let ins = INSERTION.load(Ordering::Acquire);
    if ins == 0 || conn.is_null() {
        ldr_loge!(
            "[GalLoader] fire_insertion: USBDI not captured yet (gal has not called usbd_connect)"
        );
        return 0;
    }
    let mut inst = DeviceInstance {
        path: MAGIC_PATH,
        devno: MAGIC_DEVNO,
        generation: 0,
        ident: DeviceIdent {
            vendor,
            device: product,
            dclass: 0,
            subclass: 0,
            protocol: 0,
        },
        config: 0,
        iface: 0,
        alternate: 0,
    };
    ldr_log!(
        "[GalLoader] firing synthetic USB insertion (vendor={:#06x} product={:#06x} path={} devno={})",
        vendor,
        product,
        inst.path,
        inst.devno
    );
    SYNTH_ACTIVE.store(true, Ordering::Release);
    // SAFETY: gal's insertion callback captured from usbd_connect.
    unsafe { core::mem::transmute::<usize, InsertionFn>(ins)(conn, &raw mut inst) };
    1
}

/// Retained loader ABI: wireless insertion is unavailable in this build.
#[cfg(not(feature = "wireless"))]
pub fn fire_insertion(_vendor: u32, _product: u32) -> c_int {
    0
}

#[cfg(feature = "wireless")]
fn is_synth(device: P) -> bool {
    device == FAKE_DEV.ptr()
}

pub extern "C" fn usbd_attach(
    conn: P,
    inst: *mut DeviceInstance,
    extra: usize,
    device: *mut P,
) -> c_int {
    #[cfg(feature = "wireless")]
    {
        // SAFETY: gal passes a valid instance or NULL.
        let magic = !inst.is_null()
            && unsafe { (*inst).path == MAGIC_PATH && (*inst).devno == MAGIC_DEVNO };
        if magic {
            if !device.is_null() {
                // SAFETY: gal's out-pointer.
                unsafe { *device = FAKE_DEV.ptr() };
            }
            ldr_log!(
                "[GalLoader] synth usbd_attach(extra={}) -> fake device {:p} (EOK)",
                extra,
                FAKE_DEV.ptr()
            );
            return 0;
        }
    }
    let p = USBD_ATTACH.get();
    if p.is_null() {
        return -1;
    }
    // SAFETY: libusbdi's usbd_attach.
    unsafe {
        core::mem::transmute::<
            P,
            unsafe extern "C" fn(P, *mut DeviceInstance, usize, *mut P) -> c_int,
        >(p)(conn, inst, extra, device)
    }
}

pub extern "C" fn usbd_parse_descriptors(
    device: P,
    root: P,
    kind: u8,
    index: c_int,
    node: *mut P,
) -> P {
    #[cfg(feature = "wireless")]
    if is_synth(device) {
        let d = descriptor(kind, index);
        ldr_log!(
            "[GalLoader] synth parse_descriptors(type={:#04x} index={}) -> {}",
            kind,
            index,
            if d.is_some() { "ok" } else { "MISS" }
        );
        let dp = d.map_or(core::ptr::null_mut(), |d| d.as_ptr() as P);
        if !node.is_null() {
            // SAFETY: gal's out-pointer. the node is an opaque continuation token.
            unsafe { *node = dp };
        }
        return dp;
    }
    let p = USBD_PARSE.get();
    if p.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: libusbdi's usbd_parse_descriptors.
    unsafe {
        core::mem::transmute::<P, unsafe extern "C" fn(P, P, u8, c_int, *mut P) -> P>(p)(
            device, root, kind, index, node,
        )
    }
}

pub extern "C" fn usbd_open_pipe(device: P, desc: P, pipe: *mut P) -> c_int {
    #[cfg(feature = "wireless")]
    if is_synth(device) {
        // desc is one of our endpoint descriptors. byte 2 = bEndpointAddress.
        // SAFETY: at least 3 bytes when non-null (DESC_EP_*).
        let ep = if desc.is_null() {
            0
        } else {
            unsafe { *desc.cast::<u8>().add(2) }
        };
        if !pipe.is_null() {
            // SAFETY: gal's out-pointer.
            unsafe {
                *pipe = if ep & 0x80 != 0 {
                    FAKE_PIPE_IN.ptr()
                } else {
                    FAKE_PIPE_OUT.ptr()
                };
            }
        }
        ldr_log!(
            "[GalLoader] synth usbd_open_pipe(ep={:#04x}) -> fake pipe (EOK)",
            ep
        );
        return 0;
    }
    let p = USBD_OPEN_PIPE.get();
    if p.is_null() {
        return -1;
    }
    // SAFETY: libusbdi's usbd_open_pipe.
    unsafe {
        core::mem::transmute::<P, unsafe extern "C" fn(P, P, *mut P) -> c_int>(p)(
            device, desc, pipe,
        )
    }
}

pub extern "C" fn usbd_topology_ext(conn: P, busno: u8, tp: *mut BusTopology) -> c_int {
    let p = USBD_TOPOLOGY.get();
    let rc = if p.is_null() {
        -1
    } else {
        // SAFETY: libusbdi's usbd_topology_ext.
        unsafe {
            core::mem::transmute::<P, unsafe extern "C" fn(P, u8, *mut BusTopology) -> c_int>(p)(
                conn, busno, tp,
            )
        }
    };
    #[cfg(feature = "wireless")]
    if SYNTH_ACTIVE.load(Ordering::Acquire) && !tp.is_null() {
        // gal reads ports[devno].upstream_devno and rejects 0xFF.
        // SAFETY: gal's topology buffer (64 x 8 bytes).
        unsafe {
            if rc != 0 {
                core::ptr::write_bytes(tp.cast::<u8>(), 0xFF, core::mem::size_of::<BusTopology>());
            }
            let port = &mut (*tp).ports[MAGIC_DEVNO as usize];
            port.upstream_devno = 0;
            port.upstream_port = 1;
            port.upstream_port_speed = 2; // best-effort high speed
            port.upstream_hc = 0;
        }
        ldr_log!(
            "[GalLoader] synth topology_ext(bus={}): ports[{}]=present,hs (EOK)",
            busno,
            MAGIC_DEVNO
        );
        return 0;
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_sdp_header() {
        assert_eq!(core::mem::size_of::<DeviceInstance>(), 36);
        assert_eq!(core::mem::offset_of!(DeviceInstance, ident), 4);
        assert_eq!(core::mem::size_of::<BusTopology>(), 512);
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn connect_parm_funcs_offset() {
        assert_eq!(core::mem::offset_of!(ConnectParm, funcs), 28);
        assert_eq!(core::mem::offset_of!(Funcs, insertion), 4);
    }

    #[cfg(not(feature = "wireless"))]
    #[test]
    fn insertion_is_unavailable_without_wireless() {
        assert_eq!(fire_insertion(0x18d1, 0x2d00), 0);
        assert_eq!(fire_insertion(0, 0), 0);
    }

    #[cfg(feature = "wireless")]
    #[test]
    fn descriptor_select() {
        assert_eq!(descriptor(0x05, 0).unwrap()[2], 0x81);
        assert_eq!(descriptor(0x05, 1).unwrap()[2], 0x01);
        assert!(descriptor(0x05, 2).is_none());
        assert!(descriptor(0x03, 0).is_none());
    }
}
