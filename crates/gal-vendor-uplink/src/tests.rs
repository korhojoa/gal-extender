// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use std::sync::Mutex;

use crate::ring::testing::{anon, hdr, rx_publish, set_tx_tail, tx_consume};
use crate::ring::{self, Mapped, Pop, SLOTS, SLOT_DATA};
use crate::uplink::Uplink;
use crate::wire::{check_iobuf_bounds, Bounds};

fn leak(name: &'static core::ffi::CStr) -> &'static Uplink {
    Box::leak(Box::new(Uplink::new(name)))
}

fn with_ring() -> (&'static Uplink, Mapped) {
    let u = leak(c"/gal-vendor-uplink-test-unused");
    let m = anon();
    u.install_ring(m);
    (u, m)
}

fn detach(u: &Uplink) {
    u.drop_map("test");
}

// ---------------------------------------------------------------------------
// TARGET 1: the bounds decision
// ---------------------------------------------------------------------------

fn b(cap: u32, start: u32, end: u32) -> Bounds {
    check_iobuf_bounds(true, cap, start, end)
}

#[test]
fn t1_1_ok_is_exactly_end_minus_start() {
    assert_eq!(b(4096, 0, 100), Bounds::Ok(100));
    assert_eq!(b(4096, 10, 100), Bounds::Ok(90));
    assert_eq!(b(4096, 4095, 4096), Bounds::Ok(1));
    assert_eq!(b(1, 0, 1), Bounds::Ok(1));
    // The unit frame: 38 bytes in a 67-byte pooled buffer.
    assert_eq!(b(67, 0, 38), Bounds::Ok(38));
}

#[test]
fn t1_2_start_eq_end_is_empty() {
    for (c, s, e) in [
        (4096, 0, 0),
        (4096, 1, 1),
        (4096, 4095, 4095),
        (4096, 4096, 4096),
        (0, 0, 0),
    ] {
        assert_eq!(b(c, s, e), Bounds::Empty);
    }
}

#[test]
fn t1_3_end_past_cap_is_invalid_before_empty() {
    for (c, s, e) in [
        (4096, 0, 4097),
        (4096, 100, 5000),
        (0, 0, 1),
        (10, 0, 11),
        (10, 11, 11),
    ] {
        assert_eq!(b(c, s, e), Bounds::Invalid, "{c} {s} {e}");
    }
}

#[test]
fn t1_4_start_past_end_is_invalid() {
    for (c, s, e) in [
        (4096, 100, 10),
        (4096, 1, 0),
        (4, 4000, 40),
        (u32::MAX, u32::MAX, 0),
    ] {
        assert_eq!(b(c, s, e), Bounds::Invalid);
    }
}

#[test]
fn t1_5_null_data_is_always_invalid() {
    let v = [0u32, 1, 100, 0xFFFF_FFFE, u32::MAX];
    let caps = [0u32, 1, 1536, 4096, u32::MAX];
    for &c in &caps {
        for &s in &v {
            for &e in &v {
                assert_eq!(check_iobuf_bounds(false, c, s, e), Bounds::Invalid);
            }
        }
    }
}

#[test]
fn t1_6_end_eq_cap_is_ok() {
    assert_eq!(b(1536, 0, 1536), Bounds::Ok(1536));
    assert_eq!(b(1536, 1535, 1536), Bounds::Ok(1));
    assert_eq!(b(38, 0, 38), Bounds::Ok(38));
}

#[test]
fn t1_7_cap_zero() {
    assert_eq!(b(0, 0, 0), Bounds::Empty);
    for (s, e) in [(0, 1), (1, 1), (1, 0), (u32::MAX, u32::MAX)] {
        assert_eq!(b(0, s, e), Bounds::Invalid);
    }
}

#[test]
fn t1_8_near_u32_max_no_wrap() {
    const M: u32 = u32::MAX;
    assert_eq!(b(M, 0, M), Bounds::Ok(M));
    assert_eq!(b(M, M - 1, M), Bounds::Ok(1));
    assert_eq!(b(M, M, M), Bounds::Empty);
    for (c, s, e) in [
        (M - 1, 0, M),
        (M, M, 0),
        (M, M, M - 1),
        (1, M, M),
        (M, M - 1, 1),
    ] {
        assert_eq!(b(c, s, e), Bounds::Invalid);
    }
}

/// The contract for one triple: an Ok length is end - start, non-zero, and
/// never more than cap - start (what is copied out of gal's heap into a
/// ring another process reads).
fn invariant(cap: u32, start: u32, end: u32) -> Result<(), &'static str> {
    match b(cap, start, end) {
        Bounds::Ok(len) => {
            if start >= end || end > cap {
                return Err("OK outside the buffer");
            }
            if len != end - start || len == 0 {
                return Err("OK with the wrong length");
            }
            if len > cap - start {
                return Err("OK with len > cap - start (DISCLOSURE)");
            }
        }
        Bounds::Empty => {
            if start != end || end > cap {
                return Err("EMPTY with a range");
            }
        }
        Bounds::Invalid => {}
    }
    if check_iobuf_bounds(false, cap, start, end) != Bounds::Invalid {
        return Err("NULL data not INVALID");
    }
    Ok(())
}

#[test]
fn t1_11_invariant_dense() {
    for cap in 0..256 {
        for start in 0..256 {
            for end in 0..256 {
                assert_eq!(invariant(cap, start, end), Ok(()), "{cap} {start} {end}");
            }
        }
    }
}

#[test]
fn t1_12_invariant_random() {
    let mut x: u32 = 0x0123_4567;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let anchors = [
        0u32,
        1,
        2,
        1535,
        1536,
        1537,
        4095,
        4096,
        65535,
        65536,
        0x7FFF_FFFF,
        0x8000_0000,
        0xFFFF_FFFE,
        u32::MAX,
    ];
    let na = u32::try_from(anchors.len()).unwrap();
    for _ in 0..2_000_000 {
        let mut v = [0u32; 3];
        for k in &mut v {
            *k = match rnd() & 3 {
                0 => rnd(),
                1 => anchors[(rnd() % na) as usize],
                2 => anchors[(rnd() % na) as usize]
                    .wrapping_add(rnd() % 5)
                    .wrapping_sub(2),
                _ => rnd() % 8192,
            };
        }
        assert_eq!(invariant(v[0], v[1], v[2]), Ok(()), "{v:?}");
    }
}

// ---------------------------------------------------------------------------
// TARGET 2: multi-slot split through the inbound path
// ---------------------------------------------------------------------------

fn pattern(i: u32) -> u8 {
    #[allow(clippy::cast_possible_truncation)] // the low byte, by design
    let v = (i.wrapping_mul(31) + (i >> 8).wrapping_mul(17) + 7) as u8;
    v
}

/// An arena poisoned with 0x5A and the pattern at `start..start + n`.
fn arena(start: u32, n: u32) -> Vec<u8> {
    let mut a = vec![0x5Au8; (start + n) as usize + 64];
    for i in 0..n {
        a[(start + i) as usize] = pattern(i);
    }
    a
}

fn split_one(n: u32, start: u32) {
    let (up, ring) = with_ring();
    let arena = arena(start, n);
    let cap = u32::try_from(arena.len()).unwrap();
    // SAFETY: the arena holds start + n bytes.
    unsafe { up.inbound(9, arena.as_ptr(), cap, start, start + n) };
    let want_chunks = n.div_ceil(SLOT_DATA);
    let words = hdr(&ring);
    assert_eq!(words[0].wrapping_sub(words[1]), want_chunks, "n={n}");
    assert_eq!(words[2], 0, "tx_dropped");
    let mut got = Vec::new();
    for chunk_no in 0..want_chunks {
        let chunk = tx_consume(&ring).unwrap();
        let expect = (n - chunk_no * SLOT_DATA).min(SLOT_DATA);
        assert_eq!(u32::try_from(chunk.len()).unwrap(), expect);
        got.extend_from_slice(&chunk);
    }
    assert!(tx_consume(&ring).is_none(), "a duplicated chunk is queued");
    let want: Vec<u8> = (0..n).map(pattern).collect();
    assert_eq!(got, want, "n={n} start={start}");
    let st = up.stats();
    if n == 0 {
        assert_eq!((st.in_empty, st.in_frames, st.in_bytes), (1, 0, 0));
    } else {
        assert_eq!((st.in_frames, st.in_bytes, st.in_empty), (1, n, 0));
    }
    assert_eq!(st.in_bad_bounds, 0);
    detach(up);
}

#[test]
fn t2_split_every_size() {
    for n in [0, 1, 1535, 1536, 1537, 3072, 4615, 20000] {
        split_one(n, 0);
    }
    // Copy from the declared start offset, not from the buffer base.
    split_one(4615, 7);
}

// ---------------------------------------------------------------------------
// TARGET 4: the SPSC ring and the channel-edge flush
// ---------------------------------------------------------------------------

#[test]
fn t4_1_ring_full_refuses_and_counts() {
    let m = anon();
    let p = [0xC3u8; 100];
    // SAFETY: a live test mapping, one thread.
    unsafe {
        for _ in 0..SLOTS {
            assert_eq!(m.push_to_btsvcd(&p), Ok(()));
        }
        assert_eq!(hdr(&m)[0], SLOTS);
        for _ in 0..5 {
            assert_eq!(m.push_to_btsvcd(&p), Err(true));
        }
        assert_eq!(hdr(&m)[2], 5, "tx_dropped counts each refusal");
        assert_eq!(hdr(&m)[0], SLOTS, "head did not move");
        set_tx_tail(&m, 1); // btsvcd drained one
        assert_eq!(m.push_to_btsvcd(&p), Ok(()));
        assert_eq!(hdr(&m)[0], SLOTS + 1);
        assert_eq!(hdr(&m)[2], 5);
        assert_eq!(m.push_to_btsvcd(&p[..0]), Err(false));
        assert_eq!(m.push_to_btsvcd(&[0; SLOT_DATA as usize + 1]), Err(false));
        assert_eq!(hdr(&m)[0], SLOTS + 1);
        ring::unmap(&m);
    }
}

fn fill_tx(m: &Mapped, slots: u32) {
    for _ in 0..slots {
        // SAFETY: a live test mapping, one thread.
        unsafe { m.push_to_btsvcd(&[0x11; 16]).unwrap() };
    }
}

#[test]
fn t4_2_full_ring_drops_the_whole_frame() {
    let (u, m) = with_ring();
    fill_tx(&m, SLOTS);
    let a = arena(0, 4615);
    // SAFETY: the arena holds 4615 bytes.
    unsafe { u.inbound(9, a.as_ptr(), 4615, 0, 4615) };
    let st = u.stats();
    assert_eq!(hdr(&m)[2], 1, "one frame, counted once");
    assert_eq!((st.in_ring_full, st.in_frames, st.in_bytes), (1, 1, 0));
    assert_eq!(hdr(&m)[0], SLOTS, "no partial publish");
    detach(u);
}

#[test]
fn t4_2b_partial_fit_is_still_all_or_nothing() {
    let (u, m) = with_ring();
    let need = 4615u32.div_ceil(SLOT_DATA);
    let leave_free = 2;
    assert!(need > leave_free);
    fill_tx(&m, SLOTS - leave_free);
    let before = hdr(&m)[0];
    let a = arena(0, 4615);
    // SAFETY: the arena holds 4615 bytes.
    unsafe { u.inbound(9, a.as_ptr(), 4615, 0, 4615) };
    assert_eq!(hdr(&m)[0], before, "a truncated prefix was published");
    assert_eq!(u.stats().in_bytes, 0);
    assert_eq!(hdr(&m)[2], 1);
    detach(u);
}

#[test]
fn t4_3_corrupt_slot_is_consumed() {
    let m = anon();
    let good: Vec<u8> = (0..64u8).map(|i| i ^ 0x3C).collect();
    let mut out = [0u8; 2048];
    assert!(rx_publish(&m, 9999, &good));
    assert!(rx_publish(&m, 64, &good));
    // SAFETY: a live test mapping, one thread.
    unsafe {
        assert_eq!(m.pop_to_phone(&mut out), Pop::Corrupt(9999));
        assert_eq!(hdr(&m)[4], 1, "the corrupt slot was consumed");
        assert_eq!(m.pop_to_phone(&mut out), Pop::Frame(64));
        assert_eq!(&out[..64], &good[..]);
        // Fits the slot but not the caller's buffer: corrupt too.
        assert!(rx_publish(&m, 1000, &good));
        assert_eq!(m.pop_to_phone(&mut out[..100]), Pop::Corrupt(1000));
        assert_eq!(hdr(&m)[4], 3);
        assert_eq!(m.pop_to_phone(&mut out), Pop::Empty);
        assert_eq!((hdr(&m)[3], hdr(&m)[4]), (3, 3));
        ring::unmap(&m);
    }
}

#[test]
fn t4_4_channel_edges_flush_and_return_was_open() {
    let (u, m) = with_ring();
    let mut ep = [0u8; 64];
    let frame = [0x77u8; 32];
    let gen0 = u.lock().tunnel_gen;
    for _ in 0..5 {
        assert!(rx_publish(&m, 32, &frame));
    }
    // SAFETY: ep is a 64-byte endpoint stand-in.
    unsafe { u.on_channel_opened(ep.as_mut_ptr(), 9) };
    assert_eq!(hdr(&m)[4], 5, "open flushed rx_tail to rx_head");
    assert_eq!(u.stats().stale_flushed, 5);
    assert_eq!((ep[4], ep[5]), (1, 9), "is_open and the wire channel");
    assert_eq!(u.lock().tunnel_gen, gen0 + 1);
    for _ in 0..3 {
        assert!(rx_publish(&m, 32, &frame));
    }
    // SAFETY: as above.
    let was_open = unsafe { u.on_channel_closed(ep.as_mut_ptr(), 9) };
    assert_eq!(was_open, 1, "gal clears its channel map only on non-zero");
    assert_eq!(hdr(&m)[4], 8);
    assert_eq!(u.stats().stale_flushed, 8);
    assert_eq!(u.lock().tunnel_gen, gen0 + 2);
    // A second close with the endpoint closed: 0.
    // SAFETY: as above.
    assert_eq!(unsafe { u.on_channel_closed(ep.as_mut_ptr(), 9) }, 0);
    // NULL endpoint: 0, no crash.
    // SAFETY: NULL is allowed.
    assert_eq!(unsafe { u.on_channel_closed(core::ptr::null_mut(), 9) }, 0);
    detach(u);
}

#[test]
fn t4_5_inbound_without_a_ring_is_counted() {
    let u = leak(c"/gal-vendor-uplink-test-unused");
    let a = arena(0, 200);
    // SAFETY: the arena holds 200 bytes.
    unsafe { u.inbound(9, a.as_ptr(), 200, 0, 200) };
    let st = u.stats();
    assert_eq!((st.in_no_ring, st.in_frames, st.in_bytes), (1, 0, 0));
    assert!(u.lock().attach_kick, "a frame kicks the attach");
}

#[test]
fn t4_6_invalid_iobuffer_at_the_seam() {
    let (u, m) = with_ring();
    let a = arena(0, 100);
    let cap = u32::try_from(a.len()).unwrap();
    // SAFETY: nothing is read for an invalid or empty buffer.
    unsafe {
        u.inbound(9, a.as_ptr(), cap, 10, 5);
        u.inbound(9, a.as_ptr(), cap, 0, cap + 1);
        u.inbound(9, core::ptr::null(), 0, 0, 0);
    }
    let st = u.stats();
    assert_eq!((st.in_bad_bounds, st.in_frames, st.in_bytes), (3, 0, 0));
    assert_eq!(hdr(&m)[0], 0, "not one byte forwarded");
    // SAFETY: as above.
    unsafe { u.inbound(9, a.as_ptr(), cap, 42, 42) };
    assert_eq!(u.stats().in_empty, 1);
    assert_eq!(hdr(&m)[0], 0);
    detach(u);
}

#[test]
fn disabled_uplink_ignores_frames_but_counts_bad_ones() {
    let (u, m) = with_ring();
    u.lock().disabled = true;
    let a = arena(0, 10);
    // SAFETY: the arena holds 10 bytes.
    unsafe {
        u.inbound(9, a.as_ptr(), 10, 0, 10);
        u.inbound(9, a.as_ptr(), 10, 5, 1);
    }
    assert_eq!(hdr(&m)[0], 0);
    assert_eq!((u.stats().in_frames, u.stats().in_bad_bounds), (0, 1));
    detach(u);
}

// ---------------------------------------------------------------------------
// The IoBuffer read using the 32-bit native layout
// ---------------------------------------------------------------------------

#[cfg(target_pointer_width = "32")]
#[test]
fn handle_raw_message_reads_start_and_end_not_capacity() {
    #[repr(C)]
    struct Io {
        data: *const u8,
        cap: u32,
        start: u32,
        end: u32,
    }
    let (u, m) = with_ring();
    let a = arena(0, 67);
    let io = Io {
        data: a.as_ptr(),
        cap: 67,
        start: 0,
        end: 38,
    };
    let sp: [*const Io; 2] = [core::ptr::null(), &raw const io];
    // SAFETY: laid out as gal's shared_ptr<IoBuffer> on a 32-bit target.
    unsafe { u.handle_raw_message(9, sp.as_ptr().cast()) };
    assert_eq!(tx_consume(&m).unwrap().len(), 38, "38, never 67");
    // SAFETY: NULL shared_ptr and NULL IoBuffer are ignored.
    unsafe {
        u.handle_raw_message(9, core::ptr::null());
        let none: [*const Io; 2] = [core::ptr::null(), core::ptr::null()];
        u.handle_raw_message(9, none.as_ptr().cast());
    }
    assert_eq!(u.stats().in_frames, 1);
    detach(u);
}

// ---------------------------------------------------------------------------
// The pump
// ---------------------------------------------------------------------------

static SENT: Mutex<Vec<(u8, Vec<u8>)>> = Mutex::new(Vec::new());

/// queueOutgoing stand-in: records. a frame starting 0xEE returns -5.
unsafe extern "C" fn qout(
    _mr: gal_vendor_abi::Ptr,
    ch: u8,
    data: gal_vendor_abi::Ptr,
    len: u32,
) -> i32 {
    // SAFETY: the pump passes its frame buffer of len bytes.
    let b = unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len as usize) }.to_vec();
    let r = if b.first() == Some(&0xEE) { -5 } else { 0 };
    SENT.lock().unwrap().push((ch, b));
    r
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed().as_secs() < 10, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn pump_sends_frames_skips_corrupt_and_counts_errors() {
    let (u, m) = with_ring();
    // A MessageRouter stand-in: ChannelManager* at +0x4fc non-NULL.
    let router: &'static mut [u32] = Box::leak(vec![0u32; 0x500 / 4 + 1].into_boxed_slice());
    router[0x4fc / 4] = 1;
    {
        let mut s = u.lock();
        s.mr = router.as_ptr() as usize;
        s.qout = Some(qout);
        s.our_ch = 42;
        s.ch_open = true;
        s.pump_run = true;
    }
    let ten: Vec<u8> = (0..10).collect();
    let full = vec![0x5Au8; SLOT_DATA as usize];
    let ee = [0xEEu8, 1, 2];
    assert!(rx_publish(&m, 10, &ten));
    assert!(rx_publish(&m, 9999, &ten));
    assert!(rx_publish(&m, 3, &ee));
    assert!(rx_publish(&m, SLOT_DATA, &full));
    let h = std::thread::spawn(move || u.pump());
    wait_until("the ring drained", || {
        hdr(&m)[4] == 4 && u.lock().send_inflight == 0
    });
    wait_until("the counters", || u.stats().out_frames == 2);
    let sent: Vec<(u8, Vec<u8>)> = SENT
        .lock()
        .unwrap()
        .iter()
        .filter(|(c, _)| *c == 42)
        .cloned()
        .collect();
    assert_eq!(sent.len(), 3, "10 bytes, the 0xEE frame, the full slot");
    assert_eq!(sent[0].1, ten);
    assert_eq!(sent[1].1, ee);
    assert_eq!(sent[2].1, full);
    let st = u.stats();
    assert_eq!(
        (st.out_frames, st.out_bytes, st.out_qerr, st.rx_corrupt),
        (2, 10 + SLOT_DATA, 1, 1)
    );
    // Closing the channel stops sends: a frame stays queued.
    let mut ep = [1u8; 64];
    // SAFETY: a 64-byte endpoint stand-in.
    assert_eq!(unsafe { u.on_channel_closed(ep.as_mut_ptr(), 42) }, 1);
    assert!(rx_publish(&m, 4, b"late"));
    wait_until("the no-send count", || u.stats().out_no_send > 0);
    assert_eq!(hdr(&m)[4], 4, "nothing sent while closed");
    u.lock().pump_run = false;
    h.join().unwrap();
    detach(u);
}

#[test]
fn pump_attaches_reattaches_and_latches_an_incompatible_ring() {
    use crate::ring::{Hdr, HDR_SIZE, SLOT_SIZE};
    let name = c"/gal-vendor-uplink-test-pump";
    let total = (HDR_SIZE + 2 * SLOTS * SLOT_SIZE) as usize;
    let make = |magic: u32, stale: u32| {
        let w = qnx_shm::Mapping::create(name, total, qnx_shm::MODE_ALL_RW).unwrap();
        let h = w.as_ptr().cast::<core::ffi::c_void>().cast::<Hdr>();
        // SAFETY: inside the new mapping.
        unsafe {
            (*h).magic = magic;
            (*h).abi = ring::ABI;
            (*h).hdr_size = HDR_SIZE;
            (*h).slot_size = SLOT_SIZE;
            (*h).slots = SLOTS;
            (*h).tx_off = HDR_SIZE;
            (*h).rx_off = HDR_SIZE + SLOTS * SLOT_SIZE;
            (*h).rx_head = stale;
        }
        w
    };
    let _ = qnx_shm::unlink(name);
    let u = leak(name);
    u.cfg
        .shm_recheck_ms
        .store(1000, core::sync::atomic::Ordering::Relaxed);
    u.lock().pump_run = true;
    let h = std::thread::spawn(move || u.pump());
    wait_until("a failed attach", || u.stats().shm_fail >= 1);
    let a = make(ring::MAGIC, 2);
    u.lock().attach_kick = true;
    wait_until("the attach", || u.stats().shm_attach == 1);
    assert_eq!(u.stats().stale_flushed, 2, "the attach edge flush");
    // external daemon restarts: a new object under the same name.
    qnx_shm::unlink(name).unwrap();
    let b2 = make(ring::MAGIC, 1);
    wait_until("the re-attach", || u.stats().shm_reattach == 1);
    assert_eq!(u.stats().stale_flushed, 3);
    u.lock().pump_run = false;
    h.join().unwrap();
    u.drop_map("test");
    drop((a, b2));
    qnx_shm::unlink(name).unwrap();

    let bad = make(0xdead_beef, 0);
    let v = leak(name);
    v.lock().pump_run = true;
    let h = std::thread::spawn(move || v.pump());
    wait_until("the latch", || v.lock().disabled);
    assert!(v.lock().ring_incompatible);
    assert!(v.lock().ring.is_none());
    v.lock().pump_run = false;
    h.join().unwrap();
    drop(bad);
    qnx_shm::unlink(name).unwrap();
}

#[test]
fn shutdown_joins_the_pump_and_detaches() {
    let (u, _m) = with_ring();
    u.lock().pump_run = true;
    // The pump is started by discovery in production. here by hand, as the
    // shutdown path only needs pump_started and the handle.
    let h = std::thread::spawn(move || u.pump());
    u.lock().pump_run = false;
    h.join().unwrap();
    u.shutdown();
    assert!(u.lock().ring.is_none());
    assert_eq!(u.lock().mr, 0);
    u.shutdown(); // idempotent
}

// ---------------------------------------------------------------------------
// The vtable slots of the real endpoint (the module's global uplink)
// ---------------------------------------------------------------------------

#[test]
fn vtable_slot_return_values() {
    let vt = &super::VTABLE;
    let mut ep = [0u8; 64];
    let p = ep.as_mut_ptr().cast();
    // SAFETY: the slots with a 64-byte endpoint stand-in.
    unsafe {
        assert_eq!((vt.may_open_channel)(p, 7), 1);
        assert_eq!((vt.route_message)(p, 7, 0x8001, core::ptr::null_mut()), 1);
        assert_eq!((vt.on_channel_closed)(p, 7), 0, "not open: 0");
        (vt.on_channel_opened)(p, 7);
        assert_eq!(ep[4], 1);
        assert_eq!(
            (vt.on_channel_closed)(ep.as_mut_ptr().cast(), 7),
            1,
            "open: 1"
        );
        (vt.handle_raw_message)(ep.as_mut_ptr().cast(), 7, core::ptr::null_mut());
        (vt.add_discovery_info)(ep.as_mut_ptr().cast(), core::ptr::null_mut());
        (vt.dtor_complete)(ep.as_mut_ptr().cast());
    }
}

#[test]
fn module_is_named_aauplink() {
    assert_eq!(super::MODULE.name, "aauplink");
}
