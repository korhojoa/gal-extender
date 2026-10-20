// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use core::ffi::{c_int, CStr};
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

/// The object name (`/dev/shmem/aa-uplink` on QNX).
pub const SHM_NAME: &CStr = c"/aa-uplink";
/// "AAU1".
pub const MAGIC: u32 = 0x4141_5531;
/// The layout version.
pub const ABI: u32 = 1;
/// Payload bytes per slot.
pub const SLOT_DATA: u32 = 1536;
/// Slots per ring.
pub const SLOTS: u32 = 64;

/// One slot: `aaup_slot_t`, 1544 bytes.
#[repr(C)]
pub struct Slot {
    pub len: u32,
    pub flags: u32,
    pub data: [u8; SLOT_DATA as usize],
}

/// The shared header: `aaup_hdr_t`, 14 words padded to 128 bytes.
#[repr(C)]
pub struct Hdr {
    pub magic: u32,
    pub abi: u32,
    pub hdr_size: u32,
    pub slot_size: u32,
    pub slots: u32,
    pub tx_off: u32,
    pub rx_off: u32,
    pub total_size: u32,
    pub tx_head: u32,
    pub tx_tail: u32,
    pub tx_dropped: u32,
    pub rx_head: u32,
    pub rx_tail: u32,
    pub rx_dropped: u32,
    pub pad: [u8; 128 - 14 * 4],
}

/// `sizeof(aaup_hdr_t)`.
pub const HDR_SIZE: u32 = 128;
/// `sizeof(aaup_slot_t)`.
pub const SLOT_SIZE: u32 = 8 + SLOT_DATA;

// The contract, checked at compile time (btsvc-shm pins the same values).
const _: () = {
    assert!(core::mem::size_of::<Hdr>() == HDR_SIZE as usize);
    assert!(core::mem::size_of::<Slot>() == 1544);
    assert!(core::mem::offset_of!(Hdr, magic) == 0);
    assert!(core::mem::offset_of!(Hdr, slot_size) == 12);
    assert!(core::mem::offset_of!(Hdr, slots) == 16);
    assert!(core::mem::offset_of!(Hdr, tx_off) == 20);
    assert!(core::mem::offset_of!(Hdr, rx_off) == 24);
    assert!(core::mem::offset_of!(Hdr, tx_head) == 32);
    assert!(core::mem::offset_of!(Hdr, tx_tail) == 36);
    assert!(core::mem::offset_of!(Hdr, tx_dropped) == 40);
    assert!(core::mem::offset_of!(Hdr, rx_head) == 44);
    assert!(core::mem::offset_of!(Hdr, rx_tail) == 48);
    assert!(core::mem::offset_of!(Hdr, rx_dropped) == 52);
    assert!(core::mem::offset_of!(Slot, len) == 0);
    assert!(core::mem::offset_of!(Slot, flags) == 4);
    assert!(core::mem::offset_of!(Slot, data) == 8);
};

#[inline]
fn dmb() {
    #[cfg(target_arch = "arm")]
    // SAFETY: a barrier instruction. no operands, no stack use.
    unsafe {
        core::arch::asm!("dmb sy", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "arm"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Write barrier before publishing an index.
#[inline]
pub fn wmb() {
    dmb();
}

/// Read barrier after observing an index.
#[inline]
pub fn rmb() {
    dmb();
}

/// A validated mapping of the object (`aaup_map_t`).
#[derive(Clone, Copy, Debug)]
pub struct Mapped {
    pub base: *mut u8,
    pub size: u32,
    pub hdr: *mut Hdr,
    /// phone -> vendor-extension endpoint -> us -> external daemon. we produce.
    pub to_btsvcd: *mut Slot,
    /// external daemon -> us -> `queueOutgoing()` -> phone. we consume.
    pub to_phone: *mut Slot,
    pub dev: u64,
    pub ino: u64,
}

// SAFETY: the pointers are into a process-wide shared mapping. every access
// happens under the uplink's lock (see crate::uplink).
unsafe impl Send for Mapped {}

/// Why a map attempt failed.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// No object, or it cannot be stat'ed / mapped yet: retry later.
    Absent,
    Incompatible {
        magic: u32,
        abi: u32,
        slot_size: u32,
        slots: u32,
    },
    /// Offsets outside the mapping or overlapping: as `Incompatible`.
    BadLayout {
        size: u32,
        tx_off: u32,
        rx_off: u32,
        slots: u32,
        slot_size: u32,
    },
}

/// `MAP_FAILED`.
const MAP_FAILED: *mut core::ffi::c_void = usize::MAX as *mut core::ffi::c_void;

/// size, dev and ino of an open descriptor, through std's `fstat` (std knows
/// this target's `struct stat`. no allocation).
fn stat_fd(fd: c_int) -> Option<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::FromRawFd;
    let f = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    f.metadata().ok().map(|m| (m.size(), m.dev(), m.ino()))
}

/// `aaup_map_open`: open `name`, check its geometry and offsets, map it.
/// Syscalls only. the caller holds no lock.
///
/// # Errors
/// [`OpenError`]. on `Incompatible`/`BadLayout` the mapping is already
/// gone and the caller logs.
pub fn map_open(name: &CStr) -> Result<Mapped, OpenError> {
    // SAFETY: a NUL-terminated name.
    let fd = unsafe { qnx_shm::shm_open(name.as_ptr(), qnx_shm::O_RDWR, 0) };
    if fd < 0 {
        return Err(OpenError::Absent);
    }
    let close = |fd| {
        // SAFETY: our descriptor.
        unsafe { qnx_shm::close(fd) };
    };
    let Some((st_size, dev, ino)) = stat_fd(fd) else {
        close(fd);
        return Err(OpenError::Absent);
    };
    if st_size < u64::from(HDR_SIZE) {
        close(fd);
        return Err(OpenError::Absent);
    }
    #[allow(clippy::cast_possible_truncation)]
    let size = st_size as u32;
    // SAFETY: a new shared mapping of `size` bytes of the object.
    let mm = unsafe {
        qnx_shm::mmap(
            core::ptr::null_mut(),
            size as usize,
            qnx_shm::PROT_READ | qnx_shm::PROT_WRITE,
            qnx_shm::MAP_SHARED,
            fd,
            0,
        )
    };
    close(fd);
    if mm == MAP_FAILED {
        return Err(OpenError::Absent);
    }
    let base = mm.cast::<u8>();
    let h = mm.cast::<Hdr>();
    let unmap = || {
        // SAFETY: the mapping made above, unmapped once.
        unsafe { qnx_shm::munmap(mm, size as usize) };
    };
    // SAFETY: at least HDR_SIZE bytes are mapped (checked above).
    let (magic, abi, slot_size, slots, tx_off, rx_off) = unsafe {
        (
            read_volatile(addr_of!((*h).magic)),
            read_volatile(addr_of!((*h).abi)),
            read_volatile(addr_of!((*h).slot_size)),
            read_volatile(addr_of!((*h).slots)),
            read_volatile(addr_of!((*h).tx_off)),
            read_volatile(addr_of!((*h).rx_off)),
        )
    };
    if magic != MAGIC || abi != ABI || slot_size != SLOT_SIZE || slots != SLOTS {
        unmap();
        return Err(OpenError::Incompatible {
            magic,
            abi,
            slot_size,
            slots,
        });
    }
    // The offsets are peer-written words: check that both rings lie inside
    // the mapping, after the header, and do not overlap (64-bit math, a
    // hostile offset cannot wrap).
    let need = u64::from(slots) * u64::from(slot_size);
    let (tx, rx) = (u64::from(tx_off), u64::from(rx_off));
    let (tx_end, rx_end) = (tx + need, rx + need);
    let hdr = u64::from(HDR_SIZE);
    let geom_ok = tx >= hdr
        && rx >= hdr
        && tx_end <= u64::from(size)
        && rx_end <= u64::from(size)
        && (tx_end <= rx || rx_end <= tx);
    if !geom_ok {
        unmap();
        return Err(OpenError::BadLayout {
            size,
            tx_off,
            rx_off,
            slots,
            slot_size,
        });
    }
    #[allow(clippy::cast_ptr_alignment)]
    let (to_btsvcd, to_phone) = unsafe {
        (
            base.add(tx_off as usize).cast::<Slot>(),
            base.add(rx_off as usize).cast::<Slot>(),
        )
    };
    Ok(Mapped {
        base,
        size,
        hdr: h,
        to_btsvcd,
        to_phone,
        dev,
        ino,
    })
}

/// Has the external daemon recreated the object (device number or
/// ino differ)? No object at all, or a failed stat, is "not stale" (keep the
/// mapping). Syscalls only.
#[must_use]
pub fn is_stale(name: &CStr, dev: u64, ino: u64) -> bool {
    // SAFETY: a NUL-terminated name.
    let fd = unsafe { qnx_shm::shm_open(name.as_ptr(), qnx_shm::O_RDWR, 0) };
    if fd < 0 {
        return false;
    }
    let st = stat_fd(fd);
    // SAFETY: our descriptor.
    unsafe { qnx_shm::close(fd) };
    st.is_some_and(|(_, d, i)| d != dev || i != ino)
}

/// `munmap` a mapping made by [`map_open`] (or a test's equivalent).
///
/// # Safety
/// `m` is a live mapping no other code uses any more.
pub unsafe fn unmap(m: &Mapped) {
    qnx_shm::munmap(m.base.cast(), m.size as usize);
}

#[inline]
unsafe fn rd(p: *const u32) -> u32 {
    read_volatile(p)
}

#[inline]
unsafe fn wr(p: *mut u32, v: u32) {
    write_volatile(p, v);
}

/// The result of one pop from the ring to the phone.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Pop {
    Empty,
    /// A frame of this many bytes was copied out (1..=cap).
    Frame(u32),
    /// The slot's length was out of range. the slot was consumed.
    Corrupt(u32),
}

impl Mapped {
    /// `tx_head`/`tx_tail` (ring to external daemon).
    ///
    /// # Safety
    /// The mapping is live.
    #[must_use]
    pub unsafe fn tx_used(&self) -> u32 {
        rd(addr_of!((*self.hdr).tx_head)).wrapping_sub(rd(addr_of!((*self.hdr).tx_tail)))
    }

    /// `rx_head - rx_tail` (ring to phone).
    ///
    /// # Safety
    /// The mapping is live.
    #[must_use]
    pub unsafe fn rx_queued(&self) -> u32 {
        rd(addr_of!((*self.hdr).rx_head)).wrapping_sub(rd(addr_of!((*self.hdr).rx_tail)))
    }

    /// Bump the shared `tx_dropped` counter (external daemon can read it).
    ///
    /// # Safety
    /// The mapping is live.
    pub unsafe fn bump_tx_dropped(&self) {
        let p = addr_of_mut!((*self.hdr).tx_dropped);
        wr(p, rd(p).wrapping_add(1));
    }

    /// Discard everything the external daemon queued for the
    /// phone (advance `rx_tail` to `rx_head`). Returns the count.
    ///
    /// # Safety
    /// The mapping is live. the caller holds the uplink lock.
    #[must_use]
    pub unsafe fn flush_to_phone(&self) -> u32 {
        let head = rd(addr_of!((*self.hdr).rx_head));
        let tail = rd(addr_of!((*self.hdr).rx_tail));
        let n = head.wrapping_sub(tail);
        if n == 0 {
            return 0;
        }
        rmb();
        wr(addr_of_mut!((*self.hdr).rx_tail), head);
        wmb();
        n
    }

    /// Add one chunk to the ring for the external daemon.
    /// `Err(true)` when the ring was full (`tx_dropped` bumped), `Err(false)`
    /// for a length the slot cannot hold (ring untouched).
    ///
    /// # Errors
    /// As above.
    ///
    /// # Safety
    /// The mapping is live. the caller holds the uplink lock.
    pub unsafe fn push_to_btsvcd(&self, data: &[u8]) -> Result<(), bool> {
        let len = data.len();
        if len == 0 || len > SLOT_DATA as usize {
            return Err(false);
        }
        let head = rd(addr_of!((*self.hdr).tx_head));
        let tail = rd(addr_of!((*self.hdr).tx_tail));
        if head.wrapping_sub(tail) >= SLOTS {
            self.bump_tx_dropped();
            return Err(true);
        }
        let s = self.to_btsvcd.add((head % SLOTS) as usize);
        core::ptr::copy_nonoverlapping(data.as_ptr(), addr_of_mut!((*s).data).cast::<u8>(), len);
        #[allow(clippy::cast_possible_truncation)] // len <= SLOT_DATA
        wr(addr_of_mut!((*s).len), len as u32);
        wr(addr_of_mut!((*s).flags), 0);
        wmb();
        wr(addr_of_mut!((*self.hdr).tx_head), head.wrapping_add(1));
        Ok(())
    }

    /// `aaup_pop_to_phone_locked`: one frame out of the ring to the phone
    /// into `out`. A length above `out.len()` or [`SLOT_DATA`] is corrupt;
    /// the slot is consumed either way (a bad length must not stall the ring).
    ///
    /// # Safety
    /// The mapping is live. the caller holds the uplink lock.
    pub unsafe fn pop_to_phone(&self, out: &mut [u8]) -> Pop {
        let head = rd(addr_of!((*self.hdr).rx_head));
        let tail = rd(addr_of!((*self.hdr).rx_tail));
        if head == tail {
            return Pop::Empty;
        }
        rmb();
        let s = self.to_phone.add((tail % SLOTS) as usize);
        let len = rd(addr_of!((*s).len));
        let r = if len as usize > out.len() || len > SLOT_DATA {
            Pop::Corrupt(len)
        } else {
            if len != 0 {
                core::ptr::copy_nonoverlapping(
                    addr_of!((*s).data).cast::<u8>(),
                    out.as_mut_ptr(),
                    len as usize,
                );
            }
            Pop::Frame(len)
        };
        wmb();
        wr(addr_of_mut!((*self.hdr).rx_tail), tail.wrapping_add(1));
        r
    }
}

#[cfg(test)]
pub mod testing {

    use super::*;

    #[cfg(target_os = "nto")]
    const MAP_PRIVATE_ANON: c_int = 0x2 | 0x80000;
    #[cfg(not(target_os = "nto"))]
    const MAP_PRIVATE_ANON: c_int = 0x2 | 0x20;

    /// A formatted, zeroed object in an anonymous mapping (so [`unmap`]
    /// releases it legally).
    ///
    /// # Panics
    /// When the mapping fails.
    // The mapping is page-aligned and both ring offsets are 4-aligned.
    #[allow(clippy::cast_ptr_alignment)]
    #[must_use]
    pub fn anon() -> Mapped {
        let total = HDR_SIZE + 2 * SLOTS * SLOT_SIZE;
        // SAFETY: a new private anonymous mapping.
        let mm = unsafe {
            qnx_shm::mmap(
                core::ptr::null_mut(),
                total as usize,
                qnx_shm::PROT_READ | qnx_shm::PROT_WRITE,
                MAP_PRIVATE_ANON,
                -1,
                0,
            )
        };
        assert!(mm != MAP_FAILED && !mm.is_null(), "anonymous mmap failed");
        let base = mm.cast::<u8>();
        let h = base.cast::<Hdr>();
        let tx_off = HDR_SIZE;
        let rx_off = HDR_SIZE + SLOTS * SLOT_SIZE;
        // SAFETY: inside the fresh mapping (zero-filled by the kernel).
        unsafe {
            (*h).magic = MAGIC;
            (*h).abi = ABI;
            (*h).hdr_size = HDR_SIZE;
            (*h).slot_size = SLOT_SIZE;
            (*h).slots = SLOTS;
            (*h).tx_off = tx_off;
            (*h).rx_off = rx_off;
            (*h).total_size = total;
            Mapped {
                base,
                size: total,
                hdr: h,
                to_btsvcd: base.add(tx_off as usize).cast(),
                to_phone: base.add(rx_off as usize).cast(),
                dev: 0,
                ino: 0,
            }
        }
    }

    /// Header words, for assertions.
    #[must_use]
    pub fn hdr(m: &Mapped) -> [u32; 6] {
        // SAFETY: a live test mapping.
        unsafe {
            let h = m.hdr;
            [
                (*h).tx_head,
                (*h).tx_tail,
                (*h).tx_dropped,
                (*h).rx_head,
                (*h).rx_tail,
                (*h).rx_dropped,
            ]
        }
    }

    /// Set `tx_tail` as the external daemon would after draining.
    pub fn set_tx_tail(m: &Mapped, v: u32) {
        // SAFETY: a live test mapping.
        unsafe { (*m.hdr).tx_tail = v };
    }

    /// The external daemon publishes a slot to the phone with `len` verbatim (may be
    /// deliberately corrupt) and `data` as payload. False when full.
    #[must_use]
    pub fn rx_publish(m: &Mapped, len: u32, data: &[u8]) -> bool {
        // SAFETY: a live test mapping.
        unsafe {
            let hp = m.hdr;
            let (head, tail) = ((*hp).rx_head, (*hp).rx_tail);
            if head.wrapping_sub(tail) >= SLOTS {
                return false;
            }
            let slot = m.to_phone.add((head % SLOTS) as usize);
            let count = data.len().min(SLOT_DATA as usize);
            core::ptr::copy_nonoverlapping(data.as_ptr(), (*slot).data.as_mut_ptr(), count);
            (*slot).len = len;
            (*slot).flags = 0;
            (*hp).rx_head = head.wrapping_add(1);
        }
        true
    }

    /// Consume one slot for the external daemon. Return None when empty.
    #[must_use]
    pub fn tx_consume(m: &Mapped) -> Option<Vec<u8>> {
        // SAFETY: a live test mapping.
        unsafe {
            let hp = m.hdr;
            let (head, tail) = ((*hp).tx_head, (*hp).tx_tail);
            if head == tail {
                return None;
            }
            let slot = m.to_btsvcd.add((tail % SLOTS) as usize);
            let count = ((*slot).len as usize).min(SLOT_DATA as usize);
            let bytes =
                core::slice::from_raw_parts(addr_of!((*slot).data).cast::<u8>(), count).to_vec();
            (*hp).tx_tail = tail.wrapping_add(1);
            Some(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn layout_matches_ring_contract() {
        assert_eq!(HDR_SIZE + 2 * SLOTS * SLOT_SIZE, 197_760);
        assert_eq!(MAGIC.to_be_bytes(), *b"AAU1");
    }

    #[test]
    fn push_pop_flush_on_the_raw_ring() {
        let m = anon();
        // SAFETY: a live test mapping, one thread.
        unsafe {
            assert_eq!(m.push_to_btsvcd(b"abc"), Ok(()));
            assert_eq!(m.push_to_btsvcd(b""), Err(false));
            assert_eq!(m.push_to_btsvcd(&[0; 1537]), Err(false));
            assert_eq!(tx_consume(&m).unwrap(), b"abc");
            assert!(rx_publish(&m, 2, b"hi"));
            let mut out = [0u8; 1536];
            assert_eq!(m.pop_to_phone(&mut out), Pop::Frame(2));
            assert_eq!(&out[..2], b"hi");
            assert_eq!(m.pop_to_phone(&mut out), Pop::Empty);
            assert!(rx_publish(&m, 1, b"x"));
            assert!(rx_publish(&m, 1, b"y"));
            assert_eq!(m.flush_to_phone(), 2);
            assert_eq!(m.flush_to_phone(), 0);
            unmap(&m);
        }
    }

    #[test]
    fn map_open_validates_the_object() {
        let name = c"/gal-vendor-uplink-test-map-open";
        let total = (HDR_SIZE + 2 * SLOTS * SLOT_SIZE) as usize;
        let _ = qnx_shm::unlink(name);
        assert_eq!(map_open(name).err(), Some(OpenError::Absent));

        let mk = |f: &dyn Fn(*mut Hdr)| {
            let w = qnx_shm::Mapping::create(name, total, qnx_shm::MODE_ALL_RW).unwrap();
            let h = w.as_ptr().cast::<core::ffi::c_void>().cast::<Hdr>();
            // SAFETY: inside the new mapping.
            unsafe {
                (*h).magic = MAGIC;
                (*h).abi = ABI;
                (*h).hdr_size = HDR_SIZE;
                (*h).slot_size = SLOT_SIZE;
                (*h).slots = SLOTS;
                (*h).tx_off = HDR_SIZE;
                (*h).rx_off = HDR_SIZE + SLOTS * SLOT_SIZE;
            }
            f(h);
            w
        };

        let w = mk(&|_| {});
        let m = map_open(name).expect("a good object maps");
        assert_eq!(m.size as usize, total);
        assert!(!is_stale(name, m.dev, m.ino));
        // SAFETY: the mapping just made.
        unsafe { unmap(&m) };
        drop(w);

        // SAFETY (closures below): inside the 128-byte header.
        let w = mk(&|h| unsafe { (*h).magic = 0xdead_beef });
        assert!(matches!(
            map_open(name),
            Err(OpenError::Incompatible {
                magic: 0xdead_beef,
                ..
            })
        ));
        drop(w);
        let w = mk(&|h| unsafe { (*h).rx_off = HDR_SIZE });
        assert!(matches!(map_open(name), Err(OpenError::BadLayout { .. })));
        drop(w);
        let w = mk(&|h| unsafe { (*h).tx_off = 0 });
        assert!(matches!(map_open(name), Err(OpenError::BadLayout { .. })));
        drop(w);
        let w = mk(&|h| unsafe { (*h).rx_off = u32::MAX - 10 });
        assert!(matches!(map_open(name), Err(OpenError::BadLayout { .. })));
        drop(w);
        qnx_shm::unlink(name).unwrap();
        assert!(!is_stale(name, 0, 0));
    }

    #[test]
    fn a_recreated_object_is_stale() {
        let name = c"/gal-vendor-uplink-test-stale";
        let _ = qnx_shm::unlink(name);
        let a = qnx_shm::Mapping::create(name, 4096, qnx_shm::MODE_ALL_RW).unwrap();
        // SAFETY: a NUL-terminated name.
        let fd = unsafe { qnx_shm::shm_open(name.as_ptr(), qnx_shm::O_RDWR, 0) };
        let (_, dev, ino) = stat_fd(fd).unwrap();
        // SAFETY: our descriptor.
        unsafe { qnx_shm::close(fd) };
        assert!(!is_stale(name, dev, ino));
        qnx_shm::unlink(name).unwrap();
        // Keep the old object mapped while the new one exists, so the
        // filesystem cannot hand the same inode straight back.
        let b = qnx_shm::Mapping::create(name, 4096, qnx_shm::MODE_ALL_RW).unwrap();
        assert!(is_stale(name, dev, ino));
        drop((a, b));
        qnx_shm::unlink(name).unwrap();
    }
}
