// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use core::ffi::{c_char, c_int, CStr};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use gal_feature_api::channel;
use gal_vendor_abi::endpoint::{self as ep_abi, FakeEndpoint, Vtable};
use gal_vendor_abi::offsets as off;
use gal_vendor_abi::symbols as sym;
use gal_vendor_abi::Ptr;
use qnx_slog::log::Line;

use crate::ring::{self, Mapped, OpenError, Pop, ABI, SLOTS, SLOT_DATA};
use crate::wire::{self, Bounds, RateLimit};

// ---------------------------------------------------------------------------
// Logging: "<message>\n" to slog, as `slogf(GENL, sev, "%s\n", buf)` with the
// message cut at 511 bytes (`char buf[512]` and vsnprintf). Fixed buffers.
// ---------------------------------------------------------------------------

pub fn log_at(sev: c_int, args: fmt::Arguments<'_>) {
    let mut msg: Line<512> = Line::new();
    let _ = msg.write_fmt(args);
    qnx_slog::log::raw(sev, format_args!("{}\n", msg.as_str()));
}

macro_rules! up_log {
    ($($a:tt)*) => {
        $crate::uplink::log_at(::qnx_slog::sys::SLOG_INFO, format_args!($($a)*))
    };
}
macro_rules! up_loge {
    ($($a:tt)*) => {
        $crate::uplink::log_at(::qnx_slog::sys::SLOG_ERROR, format_args!($($a)*))
    };
}
pub(crate) use {up_log, up_loge};

pub struct CPtr(pub usize);

impl fmt::Display for CPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// Bytes from the environment, printed as `%s` would (invalid UTF-8 as
/// U+FFFD. the names are ASCII in practice).
pub struct CBytes<'a>(pub &'a [u8]);

impl fmt::Display for CBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.utf8_chunks() {
            f.write_str(c.valid())?;
            if !c.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

extern "C" {
    fn getenv(name: *const c_char) -> *const c_char;
    fn strerror(e: c_int) -> *const c_char;
}

fn env_or(name: &CStr, def: &'static [u8]) -> &'static [u8] {
    let p = unsafe { getenv(name.as_ptr()) };
    if p.is_null() {
        return def;
    }
    // SAFETY: getenv returns a NUL-terminated string.
    let b = unsafe { CStr::from_ptr(p) }.to_bytes();
    if b.is_empty() {
        def
    } else {
        b
    }
}

#[must_use]
pub fn service_name() -> &'static [u8] {
    env_or(c"AAUP_SERVICE_NAME", wire::SERVICE_NAME_DEFAULT)
}

#[must_use]
pub fn package() -> &'static [u8] {
    env_or(c"AAUP_PACKAGE", wire::PACKAGE_DEFAULT)
}

/// The version-only banner, using the same shape as `GAL_BUILD_BANNER`.
pub struct Banner;

/// The version in the banner: the single copy. Bump per change.
pub const VERSION: &str = "11-rs";

impl fmt::Display for Banner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "aauplink v{VERSION}")
    }
}

#[cfg(test)]
mod banner_tests {
    use super::{Banner, VERSION};

    #[test]
    fn banner_contains_only_the_authoritative_version() {
        assert_eq!(Banner.to_string(), format!("aauplink v{VERSION}"));
    }
}

// ---------------------------------------------------------------------------
// Configuration, read once in init (gal_feature_api::config::get_int may touch the
// filesystem: never on the message path or under the lock).
// ---------------------------------------------------------------------------

pub struct Cfg {
    pub router_preflight: AtomicI32,
    pub shm_recheck_ms: AtomicI32,
    pub flush_on_open: AtomicI32,
    pub flush_on_close: AtomicI32,
}

impl Cfg {
    const fn defaults() -> Self {
        Cfg {
            router_preflight: AtomicI32::new(1),
            shm_recheck_ms: AtomicI32::new(5000),
            flush_on_open: AtomicI32::new(1),
            flush_on_close: AtomicI32::new(1),
        }
    }

    fn get(v: &AtomicI32) -> i32 {
        v.load(Ordering::Relaxed)
    }
}

pub const CONF_KEYS: [(&CStr, i32); 4] = [
    (c"aauplink_router_preflight", 1),
    (c"aauplink_shm_recheck_ms", 5000),
    (c"aauplink_flush_on_open", 1),
    (c"aauplink_flush_on_close", 1),
];

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The diagnostic counters (`aaup_stats_t`). in_* = phone -> external daemon,
/// out_* = external daemon -> phone.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stats {
    pub in_frames: u32,
    pub in_bytes: u32,
    pub in_empty: u32,
    pub in_bad_bounds: u32,
    pub in_ring_full: u32,
    pub in_no_ring: u32,
    pub out_frames: u32,
    pub out_bytes: u32,
    pub out_qerr: u32,
    pub out_no_send: u32,
    pub rx_corrupt: u32,
    pub stale_flushed: u32,
    pub shm_attach: u32,
    pub shm_reattach: u32,
    pub shm_fail: u32,
    pub reg_fail: u32,
}

impl Stats {
    const ZERO: Stats = Stats {
        in_frames: 0,
        in_bytes: 0,
        in_empty: 0,
        in_bad_bounds: 0,
        in_ring_full: 0,
        in_no_ring: 0,
        out_frames: 0,
        out_bytes: 0,
        out_qerr: 0,
        out_no_send: 0,
        rx_corrupt: 0,
        stale_flushed: 0,
        shm_attach: 0,
        shm_reattach: 0,
        shm_fail: 0,
        reg_fail: 0,
    };
}

/// One rate limiter per recurring message.
#[derive(Default)]
pub struct Limits {
    pub bad_bounds: RateLimit,
    pub to_btsvcd_full: RateLimit,
    pub no_ring: RateLimit,
    pub shm_fail: RateLimit,
    pub qerr: RateLimit,
    pub to_phone_corrupt: RateLimit,
    pub to_phone_no_send: RateLimit,
    pub reg_fail: RateLimit,
    pub route: RateLimit,
}

impl Limits {
    const fn new() -> Self {
        Limits {
            bad_bounds: RateLimit::new(),
            to_btsvcd_full: RateLimit::new(),
            no_ring: RateLimit::new(),
            shm_fail: RateLimit::new(),
            qerr: RateLimit::new(),
            to_phone_corrupt: RateLimit::new(),
            to_phone_no_send: RateLimit::new(),
            reg_fail: RateLimit::new(),
            route: RateLimit::new(),
        }
    }
}

pub(crate) type QueueOutgoing = unsafe extern "C" fn(Ptr, u8, Ptr, u32) -> i32;
type RegisterService = unsafe extern "C" fn(Ptr, Ptr) -> i32;

/// The protobuf helpers on `ServiceDiscoveryResponse` the uplink calls.
#[derive(Clone, Copy)]
pub struct PbFns {
    dtor: unsafe extern "C" fn(Ptr),
    merge: unsafe extern "C" fn(Ptr, gal_vendor_abi::ConstPtr),
    parse: unsafe extern "C" fn(Ptr, gal_vendor_abi::ConstPtr, i32) -> bool,
    new: unsafe extern "C" fn(gal_vendor_abi::ConstPtr) -> Ptr,
    default: unsafe extern "C" fn() -> Ptr,
}

#[allow(clippy::struct_excessive_bools)]
pub struct State {
    pub ring: Option<Mapped>,
    pub ring_incompatible: bool,
    /// `MessageRouter*` (as an address).
    pub mr: usize,
    pub(crate) qout: Option<QueueOutgoing>,
    reg_svc: Option<RegisterService>,
    pub our_ch: u8,
    pub ch_open: bool,
    /// Local count of vendor-extension channel open/close edges. NOT an AA
    /// session id (nothing here parses `AVChannelStartIndication`).
    pub tunnel_gen: u32,
    /// Channel ids seen through `GalReceiver::registerService` and our own.
    pub ch_taken: [bool; 256],
    pub(crate) pump_run: bool,
    pump_started: bool,
    pump: Option<std::thread::JoinHandle<()>>,
    pub send_inflight: u32,
    pub attach_kick: bool,
    pub disabled: bool,
    pub ep_registered: bool,
    pub st: Stats,
    pub rl: Limits,
    pb: Option<PbFns>,
}

impl State {
    const fn new() -> Self {
        State {
            ring: None,
            ring_incompatible: false,
            mr: 0,
            qout: None,
            reg_svc: None,
            our_ch: 0xFF,
            ch_open: false,
            tunnel_gen: 0,
            ch_taken: [false; 256],
            pump_run: false,
            pump_started: false,
            pump: None,
            send_inflight: 0,
            attach_kick: false,
            disabled: false,
            ep_registered: false,
            st: Stats::ZERO,
            rl: Limits::new(),
            pb: None,
        }
    }

    /// `aaup_flush_to_phone_locked` with the counter.
    fn flush_to_phone(&mut self) -> u32 {
        let Some(m) = self.ring else { return 0 };
        // SAFETY: a published mapping. the lock is held (we are &mut State).
        let n = unsafe { m.flush_to_phone() };
        self.st.stale_flushed = self.st.stale_flushed.wrapping_add(n);
        n
    }
}

/// The uplink: its state, the in-flight condition, its configuration and
/// the shm name (`/aa-uplink`. tests use their own).
pub struct Uplink {
    state: Mutex<State>,
    idle: Condvar,
    pub cfg: Cfg,
    shm_name: &'static CStr,
}

/// Inbound `IoBuffer` as gal passes it: `shared_ptr<IoBuffer>*`.
///
/// # Safety
/// `sptr` is NULL or gal's `const shared_ptr<IoBuffer>&` (32-bit layout,
/// gal-vendor-abi `offsets`).
#[must_use]
pub unsafe fn read_iobuf(sptr: *const u8) -> Option<(*const u8, u32, u32, u32)> {
    if sptr.is_null() {
        return None;
    }
    let io = sptr
        .add(off::SHARED_PTR_IOBUFFER)
        .cast::<*const u8>()
        .read_unaligned();
    if io.is_null() {
        return None;
    }
    let data = io
        .add(off::IOBUFFER_DATA)
        .cast::<*const u8>()
        .read_unaligned();
    let cap = io
        .add(off::IOBUFFER_CAPACITY)
        .cast::<u32>()
        .read_unaligned();
    let start = io.add(off::IOBUFFER_START).cast::<u32>().read_unaligned();
    let end = io.add(off::IOBUFFER_END).cast::<u32>().read_unaligned();
    Some((data, cap, start, end))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl Uplink {
    #[must_use]
    pub const fn new(shm_name: &'static CStr) -> Self {
        Uplink {
            state: Mutex::new(State::new()),
            idle: Condvar::new(),
            cfg: Cfg::defaults(),
            shm_name,
        }
    }

    /// The state, under the lock (a poisoned lock is used as it is: every
    /// update in it is a plain field store).
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Create the lock and the condition variable now, on the constructor
    /// thread: std may set its pthread objects up lazily on first use, and
    /// that first use must not be an allocation on gal's message thread.
    pub fn warm_up(&self) {
        let g = self.lock();
        let (g, _) = self
            .idle
            .wait_timeout(g, std::time::Duration::ZERO)
            .unwrap_or_else(PoisonError::into_inner);
        self.idle.notify_all();
        drop(g);
    }

    /// Reset channel identifiers and counters to zero.
    pub fn reset(&self) {
        let mut s = self.lock();
        s.ch_taken = [false; 256];
        s.st = Stats::ZERO;
    }

    /// Read the operational conf keys (init only).
    pub fn read_config(&self, get: impl Fn(&CStr, i32) -> i32) {
        let c = &self.cfg;
        let slots = [
            &c.router_preflight,
            &c.shm_recheck_ms,
            &c.flush_on_open,
            &c.flush_on_close,
        ];
        for ((key, def), slot) in CONF_KEYS.iter().zip(slots) {
            slot.store(get(key, *def), Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------------
    // shm attach: syscalls with the lock dropped, publication atomic
    // -----------------------------------------------------------------------

    /// Install a complete mapping and discard queued data for a previous process.
    pub fn publish(&self, m: Mapped, reattach: bool) {
        let mut s = self.lock();
        if s.ring.is_some() {
            drop(s);
            // SAFETY: our own unused mapping.
            unsafe { ring::unmap(&m) };
            return;
        }
        s.ring = Some(m);
        let flushed = s.flush_to_phone();
        if reattach {
            s.st.shm_reattach = s.st.shm_reattach.wrapping_add(1);
        } else {
            s.st.shm_attach = s.st.shm_attach.wrapping_add(1);
        }
        let size = m.size;
        drop(s);
        up_log!(
            "[GalAAUp] /aa-uplink {} ({} bytes, {} slots, abi={}); {} stale external daemon->phone entries discarded",
            if reattach { "RE-attached" } else { "mapped" },
            size,
            SLOTS,
            ABI,
            flushed
        );
    }

    /// `aaup_map_drop`: detach. unmap with the lock dropped.
    pub fn drop_map(&self, why: &str) {
        let m = self.lock().ring.take();
        if let Some(m) = m {
            // SAFETY: unpublished under the lock above. no ring pointer
            // escapes a critical section, so nothing else uses it.
            unsafe { ring::unmap(&m) };
            up_log!("[GalAAUp] /aa-uplink detached ({})", why);
        }
    }

    fn router_slot_free(&self, mr: usize, idx: u8) -> bool {
        if mr == 0 || Cfg::get(&self.cfg.router_preflight) == 0 {
            return true; // unknown -> do not veto
        }
        let p = (mr + off::ROUTER_ENDPOINT_TABLE + usize::from(idx) * 4) as *const u32;
        // SAFETY: MessageRouter+0x100+idx*4, inside the router object
        // Register the endpoint at the receiver service entry.
        unsafe { p.read_unaligned() == 0 }
    }

    fn router_alive(&self, mr: usize) -> bool {
        if mr == 0 {
            return false;
        }
        if Cfg::get(&self.cfg.router_preflight) == 0 {
            return true;
        }
        let p = (mr + off::ROUTER_CHANNEL_MANAGER) as *const u32;
        // SAFETY: MessageRouter+0x4fc, the ChannelManager* (shutdown() @0xecc38
        // NULLs it).
        unsafe { p.read_unaligned() != 0 }
    }

    // -----------------------------------------------------------------------
    // Endpoint slots
    // -----------------------------------------------------------------------

    /// Slot 2 `onChannelClosed`: 1 if the channel was open, else 0. gal
    /// (`handleChannelCloseNotif` @0xeccbc) clears its channel map only on
    /// non-zero. a 0 would refuse every later open of this channel (-5).
    ///
    /// # Safety
    /// `ep` is NULL or our fake endpoint.
    pub unsafe fn on_channel_closed(&self, ep: *mut u8, ch: u8) -> i32 {
        let mut was_open = false;
        if !ep.is_null() {
            was_open = *ep.add(ep_abi::IS_OPEN) != 0;
            *ep.add(ep_abi::IS_OPEN) = 0;
        }
        let mut s = self.lock();
        s.ch_open = false;
        s.tunnel_gen = s.tunnel_gen.wrapping_add(1);
        let gen = s.tunnel_gen;
        let flushed = if Cfg::get(&self.cfg.flush_on_close) != 0 {
            s.flush_to_phone()
        } else {
            0
        };
        // No return while a queueOutgoing is in flight on this channel.
        let s = self
            .idle
            .wait_while(s, |s| s.send_inflight > 0)
            .unwrap_or_else(PoisonError::into_inner);
        drop(s);
        up_log!(
            "[GalAAUp] onChannelClosed ch={} (was_open={}, tunnel_gen->{}, {} stale external daemon->phone entries dropped)",
            ch,
            i32::from(was_open),
            gen,
            flushed
        );
        i32::from(was_open)
    }

    /// Slot 3 `mayOpenChannel`: 1, deliberately (gal refuses a duplicate open
    /// one level up, -5 in handleChannelOpenReq).
    pub fn may_open_channel(&self, ch: u8) -> i32 {
        up_log!("[GalAAUp] mayOpenChannel ch={} -> 1", ch);
        1
    }

    /// Slot 4 `onChannelOpened`. `ch` is the WIRE channel id.
    ///
    /// # Safety
    /// `ep` is NULL or our fake endpoint.
    pub unsafe fn on_channel_opened(&self, ep: *mut u8, ch: u8) {
        if !ep.is_null() {
            *ep.add(ep_abi::IS_OPEN) = 1;
            *ep.add(ep_abi::OPEN_CHANNEL) = ch;
        }
        let mut s = self.lock();
        s.ch_open = true;
        s.tunnel_gen = s.tunnel_gen.wrapping_add(1);
        let gen = s.tunnel_gen;
        let flushed = if Cfg::get(&self.cfg.flush_on_open) != 0 {
            s.flush_to_phone()
        } else {
            0
        };
        s.attach_kick = true;
        drop(s);
        up_log!(
            "[GalAAUp] onChannelOpened ch={} (tunnel_gen={}, {} stale external daemon->phone entries dropped)",
            ch,
            gen,
            flushed
        );
    }

    /// Slot 5 `routeMessage`: not expected on a flags=1 endpoint. 1 =
    /// handled (0 would make gal reset the slot).
    pub fn route_message(&self, ch: u8, t: u16) -> i32 {
        let say = self.lock().rl.route.allow();
        if say {
            up_log!(
                "[GalAAUp] routeMessage ch={} type=0x{:04x} (unexpected on flags=1)",
                ch,
                t
            );
        }
        1
    }

    /// Slot 6 `handleRawMessage`: a frame from the phone.
    ///
    /// # Safety
    /// `sptr` is NULL or gal's `shared_ptr<IoBuffer>*`.
    pub unsafe fn handle_raw_message(&self, ch: u8, sptr: *const u8) {
        if let Some((data, cap, start, end)) = read_iobuf(sptr) {
            self.inbound(ch, data, cap, start, end);
        }
    }

    #[allow(clippy::too_many_lines)]
    /// Forward a checked buffer into the shared-memory ring.
    ///
    /// # Safety
    /// When bounds are valid, `data` must point to `cap` readable bytes.
    pub unsafe fn inbound(&self, ch: u8, data: *const u8, cap: u32, start: u32, end: u32) {
        let plen = match wire::check_iobuf_bounds(!data.is_null(), cap, start, end) {
            Bounds::Invalid => {
                let (n, say) = {
                    let mut s = self.lock();
                    s.st.in_bad_bounds = s.st.in_bad_bounds.wrapping_add(1);
                    (s.st.in_bad_bounds, s.rl.bad_bounds.allow())
                };
                if say {
                    up_loge!(
                        "[GalAAUp] from phone ch={} INVALID IoBuffer bounds (data={} cap={} start={} end={}) \u{2014} dropped, {} so far",
                        ch,
                        CPtr(data as usize),
                        cap,
                        start,
                        end,
                        n
                    );
                }
                return;
            }
            Bounds::Empty => {
                let mut s = self.lock();
                s.st.in_empty = s.st.in_empty.wrapping_add(1);
                return;
            }
            Bounds::Ok(n) => n,
        };

        let mut s = self.lock();
        if s.disabled {
            return;
        }
        let have_ring = s.ring.is_some();
        let (mut pushed, mut dropped) = (0u32, 0u32);
        match s.ring {
            None => {
                s.attach_kick = true;
                s.st.in_no_ring = s.st.in_no_ring.wrapping_add(1);
            }
            Some(m) => {
                // All or nothing: reserve every slot the frame needs, or drop
                // the whole frame. Half a message desyncs external daemon's mux.
                let need = plen.div_ceil(SLOT_DATA);
                let used = m.tx_used();
                let free = SLOTS.saturating_sub(used);
                if need > free {
                    m.bump_tx_dropped();
                    s.st.in_ring_full = s.st.in_ring_full.wrapping_add(1);
                    dropped = plen;
                } else {
                    let frame =
                        core::slice::from_raw_parts(data.add(start as usize), plen as usize);
                    for chunk in frame.chunks(SLOT_DATA as usize) {
                        #[allow(clippy::cast_possible_truncation)] // <= SLOT_DATA
                        let n = chunk.len() as u32;
                        match m.push_to_btsvcd(chunk) {
                            Ok(()) => pushed += n,
                            // Unreachable: the slots were reserved under the
                            // lock. Counted, never partially published.
                            Err(full) => {
                                if full {
                                    s.st.in_ring_full = s.st.in_ring_full.wrapping_add(1);
                                }
                                dropped += n;
                            }
                        }
                    }
                }
                s.st.in_frames = s.st.in_frames.wrapping_add(1);
                s.st.in_bytes = s.st.in_bytes.wrapping_add(pushed);
            }
        }
        let frames = s.st.in_frames.wrapping_add(s.st.in_no_ring);
        let first = frames == 1;
        let say_full = dropped != 0 && s.rl.to_btsvcd_full.allow();
        let say_noring = !have_ring && s.rl.no_ring.allow();
        let (full_n, noring_n) = (s.st.in_ring_full, s.st.in_no_ring);
        drop(s);

        if first {
            up_log!(
                "[GalAAUp] FIRST frame from phone on ch={}: {} bytes (cap={} start={} end={}) (ring {})",
                ch,
                plen,
                cap,
                start,
                end,
                if have_ring {
                    "MAPPED"
                } else {
                    "NOT mapped \u{2014} bytes dropped"
                }
            );
        }
        if say_noring {
            up_loge!(
                "[GalAAUp] frame from phone with no ring mapping \u{2014} {} frames dropped so far (external daemon not up?)",
                noring_n
            );
        }
        if say_full {
            up_loge!(
                "[GalAAUp] ring to external daemon FULL \u{2014} dropped {} of {} bytes this frame, {} full events so far (external daemon not draining)",
                dropped,
                plen,
                full_n
            );
        }
    }

    // -----------------------------------------------------------------------
    // Hooks
    // -----------------------------------------------------------------------

    /// Interposed `GalReceiver::registerService` (after gal's own): the
    /// router pointer, and the service index gal registered.
    ///
    /// # Safety
    /// `ep` is NULL or a `ProtocolEndpointBase*`.
    pub unsafe fn on_register_service(&self, this: Ptr, ep: Ptr) {
        let mut s = self.lock();
        s.mr = this as usize;
        if !ep.is_null() {
            let ch = *ep.cast::<u8>().add(ep_abi::SERVICE_INDEX);
            s.ch_taken[usize::from(ch)] = true;
        }
    }

    /// Interposed `MessageRouter::notifyChannelOpened` (after gal's own).
    pub fn on_hook_channel_opened(&self, mr: Ptr, ch: u8) {
        let ours = {
            let mut s = self.lock();
            let ours = ch == s.our_ch && s.our_ch != 0xFF;
            if ours {
                s.mr = mr as usize;
            }
            ours
        };
        if ours {
            up_log!(
                "[GalAAUp] ChannelOpenRequest accepted for our vendor-extension channel {}",
                ch
            );
        }
    }

    fn resolve_logged<F: Copy>(s: &gal_vendor_abi::Symbol<F>) -> Option<F> {
        let f = s.resolve();
        if f.is_none() {
            up_loge!(
                "[GalAAUp] dlsym FAILED: {}",
                s.name().to_str().unwrap_or("?")
            );
        }
        f
    }

    /// `init_pb_ptrs`: resolve the seven protobuf symbols until all five
    /// the uplink calls are found (each miss logged, every time).
    fn pb_fns(&self) -> Option<PbFns> {
        if let Some(p) = self.lock().pb {
            return Some(p);
        }
        let dtor = Self::resolve_logged(&sym::SDR_DELETING_DTOR);
        let merge = Self::resolve_logged(&sym::SDR_MERGE_FROM);
        let parse = Self::resolve_logged(&sym::MSG_PARSE_FROM_ARRAY);
        let new = Self::resolve_logged(&sym::SDR_NEW);
        let default = Self::resolve_logged(&sym::SDR_DEFAULT_INSTANCE);
        let _ = Self::resolve_logged(&sym::SDR_BYTE_SIZE);
        let _ = Self::resolve_logged(&sym::MSG_SERIALIZE_TO_ARRAY);
        let p = PbFns {
            dtor: dtor?,
            merge: merge?,
            parse: parse?,
            new: new?,
            default: default?,
        };
        self.lock().pb = Some(p);
        Some(p)
    }

    /// `alloc_temp_sdr`.
    fn alloc_temp_sdr(pb: &PbFns) -> Ptr {
        // SAFETY: ServiceDiscoveryResponse::default_instance() and New().
        unsafe {
            let def = (pb.default)();
            if def.is_null() {
                up_loge!("[GalAAUp] default_instance NULL");
                return core::ptr::null_mut();
            }
            (pb.new)(def)
        }
    }

    /// Interposed `VideoSink::addDiscoveryInfo` (after gal's own): advertise
    /// the service and register the endpoint. Complete all fallible steps before
    /// merging the response. Any failure disables only the uplink. Projection
    /// is untouched.
    ///
    /// # Safety
    /// `resp` is gal's `ServiceDiscoveryResponse*`. `ep` our endpoint object.
    #[allow(clippy::too_many_lines)]
    pub unsafe fn on_video_discovery(
        &'static self,
        this: Ptr,
        resp: Ptr,
        ep: *mut FakeEndpoint,
        vtable: *const Vtable,
    ) {
        if this.is_null() || resp.is_null() {
            return;
        }
        let (disabled, registered, known_ch) = {
            let s = self.lock();
            (s.disabled, s.ep_registered, s.our_ch)
        };
        if disabled {
            return;
        }

        // A second AA session in the same gal process: re-advertise the
        // channel we already own (there is no unregisterService).
        if registered {
            if known_ch == 0xFF {
                return;
            }
            let Some(pb) = self.pb_fns() else { return };
            let mut rb = [0u8; 512];
            let rl = wire::build_vendor_sdr(&mut rb, known_ch, service_name(), package());
            if rl == 0 {
                return;
            }
            let rt = Self::alloc_temp_sdr(&pb);
            if rt.is_null() {
                return;
            }
            #[allow(clippy::cast_possible_wrap)] // rl <= 512
            if (pb.parse)(rt, rb.as_ptr().cast(), rl as i32) {
                (pb.merge)(resp, rt.cast_const());
                up_log!(
                    "[GalAAUp] re-advertised VendorExtensionService on ch={} (endpoint already registered)",
                    known_ch
                );
            }
            (pb.dtor)(rt);
            return;
        }

        let Some(pb) = self.pb_fns() else {
            self.lock().disabled = true;
            up_loge!(
                "[GalAAUp] pb symbols missing \u{2014} uplink disabled (projection unaffected)"
            );
            return;
        };

        // The router entry points before anything is decided.
        let (mr, mut qout, mut reg) = {
            let s = self.lock();
            (s.mr, s.qout, s.reg_svc)
        };
        if qout.is_none() {
            qout = sym::MR_QUEUE_OUTGOING.resolve();
        }
        if reg.is_none() {
            reg = sym::MR_REGISTER_SERVICE.resolve();
        }
        {
            let mut s = self.lock();
            s.qout = qout;
            s.reg_svc = reg;
        }
        let (Some(reg), Some(_), true) = (reg, qout, mr != 0) else {
            let (n, say) = {
                let mut s = self.lock();
                s.st.reg_fail = s.st.reg_fail.wrapping_add(1);
                (s.st.reg_fail, s.rl.reg_fail.allow())
            };
            if say {
                up_loge!(
                    "[GalAAUp] preflight FAILED (mr={} reg={} qout={}) \u{2014} NOT advertising the VendorExtensionService ({}), projection unaffected",
                    CPtr(mr),
                    CPtr(reg.map_or(0, |f| f as usize)),
                    CPtr(qout.map_or(0, |f| f as usize)),
                    n
                );
            }
            return;
        };

        // The lowest free channel id: the loader's registry allocates it
        // atomically. our two local filters (ids gal registered, the
        // router's own endpoint table) may still reject it, and then it stays
        // burned and the scan moves on. The snapshot is taken and the scan
        // runs with our lock dropped (the allocator has its own).
        let taken = self.lock().ch_taken;
        let mut our_ch = 0xFFu8;
        let mut scan_at: u32 = 1;
        let mut burned: u32 = 0;
        while scan_at <= 254 {
            #[allow(clippy::cast_possible_truncation)] // <= 254
            let Some(cand) = channel::reserve_lowest(scan_at as u8) else {
                break; // registry exhausted
            };
            if !taken[usize::from(cand)] && self.router_slot_free(mr, cand) {
                our_ch = cand;
                break;
            }
            burned += 1;
            scan_at = u32::from(cand) + 1;
        }
        if our_ch == 0xFF {
            up_loge!(
                "[GalAAUp] no free channel_id ({} reserved ids rejected by the local filters) \u{2014} uplink not advertised",
                burned
            );
            return;
        }
        if burned != 0 {
            up_log!(
                "[GalAAUp] channel {} reserved after skipping {} id(s) gal or the router already held",
                our_ch,
                burned
            );
        }

        let mut bytes = [0u8; 512];
        let len = wire::build_vendor_sdr(&mut bytes, our_ch, service_name(), package());
        if len == 0 {
            up_loge!("[GalAAUp] service/package name too long \u{2014} not advertised");
            return;
        }
        let tmp = Self::alloc_temp_sdr(&pb);
        if tmp.is_null() {
            up_loge!("[GalAAUp] alloc_temp_sdr failed \u{2014} not advertised");
            return;
        }
        #[allow(clippy::cast_possible_wrap)] // len <= 512
        if !(pb.parse)(tmp, bytes.as_ptr().cast(), len as i32) {
            up_loge!(
                "[GalAAUp] parse_array FAILED ({} bytes) \u{2014} not advertised",
                len
            );
            (pb.dtor)(tmp);
            return;
        }

        // Register FIRST (registerService @0xecd20: 1 installed, 0 refused);
        // a refusal means we must not advertise.
        ep.write(FakeEndpoint::new(vtable, our_ch, 1));
        let ok = reg(mr as Ptr, ep.cast());
        if ok == 0 {
            let (n, say) = {
                let mut s = self.lock();
                s.st.reg_fail = s.st.reg_fail.wrapping_add(1);
                (s.st.reg_fail, s.rl.reg_fail.allow())
            };
            (pb.dtor)(tmp);
            if say {
                up_loge!(
                    "[GalAAUp] registerService REFUSED ch={} (table slot in use) \u{2014} VendorExtensionService NOT advertised ({}), projection unaffected",
                    our_ch,
                    n
                );
            }
            return;
        }

        // Committed: the endpoint exists, so advertising is safe.
        (pb.merge)(resp, tmp.cast_const());
        (pb.dtor)(tmp);
        {
            let mut s = self.lock();
            s.our_ch = our_ch;
            s.ch_taken[usize::from(our_ch)] = true;
            s.ep_registered = true;
        }
        // No channel::claim: reserve_lowest already took the id.
        up_log!(
            "[GalAAUp] {} ready: VendorExtensionService '{}' pkg '{}' ch={} (SDR {} bytes, registerService={}, ring abi={} slots={} slot={})",
            Banner,
            CBytes(service_name()),
            CBytes(package()),
            our_ch,
            len,
            ok,
            ABI,
            SLOTS,
            SLOT_DATA
        );

        self.start_pump();
    }

    /// Start the external daemon -> phone pump once. A thread, never a fork (a child's
    /// SIGCHLD interrupts gal's USB reads).
    fn start_pump(&'static self) {
        let start = {
            let mut s = self.lock();
            if s.pump_started {
                false
            } else {
                s.pump_started = true;
                s.pump_run = true;
                true
            }
        };
        if !start {
            return;
        }
        // 64 KiB: the loop's frame buffer and log lines need a few KiB. an
        // explicit size also keeps std from reading RUST_MIN_STACK.
        let spawned = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || self.pump_thread());
        match spawned {
            Ok(h) => self.lock().pump = Some(h),
            Err(e) => {
                {
                    let mut s = self.lock();
                    s.pump_run = false;
                    s.pump_started = false;
                }
                let code = e.raw_os_error().unwrap_or(0);
                // SAFETY: strerror returns a NUL-terminated static string.
                let msg = unsafe { CStr::from_ptr(strerror(code)) };
                up_loge!(
                    "[GalAAUp] pump thread create failed: {} \u{2014} external daemon->phone is dead, phone->external daemon still works",
                    msg.to_str().unwrap_or("?")
                );
            }
        }
    }

    /// The pump thread: the loop under a panic guard. A panic ends the
    /// thread (its pinned send, if any, is released by the pin's drop).
    fn pump_thread(&self) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pump())).is_err() {
            up_loge!(
                "[GalAAUp] PANIC contained in the pump thread; external daemon->phone stopped"
            );
        }
    }

    /// The external daemon -> phone pump: pop the ring to the phone and hand each
    /// frame to `MessageRouter::queueOutgoing` on our channel. attach and
    /// re-attach the ring. the periodic stats. Polls with a 2 ms sleep.
    #[allow(clippy::too_many_lines)]
    pub fn pump(&self) {
        let mut frame = [0u8; SLOT_DATA as usize];
        // Attach backoff in ticks: a failed attach always costs one 2 ms
        // sleep, so 250 ticks are about 500 ms.
        let mut retry_ticks: u32 = 0;
        let mut ever_attached = false;
        // Recheck and stats run on time(), not ticks (under load the loop
        // does not sleep, and a tick interval would fire far too often).
        let mut next_recheck: i64 = 0;

        loop {
            // ---- one critical section decides everything about this pass
            let mut pin: Option<SendPin<'_>> = None;
            let mut send: Option<(usize, QueueOutgoing, u8, u32)> = None;
            let (mut say_corrupt, mut bad_len, mut ncorrupt) = (false, 0u32, 0u32);
            let mut no_send_log: Option<(u32, u32)> = None;
            let (run, have_ring, kick, dev, ino) = {
                let mut s = self.lock();
                let run = s.pump_run;
                let have_ring = s.ring.is_some();
                let kick = s.attach_kick;
                s.attach_kick = false;
                let (dev, ino) = s.ring.map_or((0, 0), |m| (m.dev, m.ino));
                let ring = s.ring;
                if let (true, Some(m), true, true, Some(qout), false) =
                    (run, ring, s.ch_open, s.mr != 0, s.qout, s.disabled)
                {
                    if self.router_alive(s.mr) {
                        // SAFETY: a published mapping. lock held.
                        match unsafe { m.pop_to_phone(&mut frame) } {
                            Pop::Frame(n) if n > 0 => {
                                send = Some((s.mr, qout, s.our_ch, n));
                                s.send_inflight += 1;
                                pin = Some(SendPin(self));
                            }
                            Pop::Corrupt(len) => {
                                s.st.rx_corrupt = s.st.rx_corrupt.wrapping_add(1);
                                bad_len = len;
                                say_corrupt = s.rl.to_phone_corrupt.allow();
                            }
                            Pop::Frame(_) | Pop::Empty => {}
                        }
                        ncorrupt = s.st.rx_corrupt;
                    } else {
                        Self::check_no_send(&mut s, m, &mut no_send_log);
                    }
                } else if let (true, Some(m), false) = (run, ring, s.disabled) {
                    Self::check_no_send(&mut s, m, &mut no_send_log);
                }
                (run, have_ring, kick, dev, ino)
            };
            if let Some((q, c)) = no_send_log {
                up_loge!(
                    "[GalAAUp] {} external daemon->phone entries queued but channel not sendable ({} occurrences)",
                    q,
                    c
                );
            }

            if !run {
                break;
            }

            // A rejected length is > SLOT_DATA: a slot was consumed, keep
            // draining instead of sleeping.
            let mut did = bad_len != 0;
            if say_corrupt {
                up_loge!(
                    "[GalAAUp] external daemon->phone slot rejected: len={} out of range (max {}) \u{2014} slot consumed, {} corrupt slots so far",
                    bad_len,
                    SLOT_DATA,
                    ncorrupt
                );
            }

            // ---- send, with the lock dropped
            if let Some((mr, qout, ch, n)) = send {
                // SAFETY: queueOutgoing(router, ch, data, len). the router is
                // pinned (send_inflight) so close/shutdown wait for us.
                let ret = unsafe { qout(mr as Ptr, ch, frame.as_mut_ptr().cast(), n) };
                let (say, nerr) = {
                    let mut s = self.lock();
                    if let Some(p) = pin.take() {
                        p.release(&mut s);
                    }
                    let mut say = 0i32;
                    let mut nerr = 0u32;
                    if ret != 0 {
                        s.st.out_qerr = s.st.out_qerr.wrapping_add(1);
                        nerr = s.st.out_qerr;
                        say = i32::from(s.rl.qerr.allow());
                    } else {
                        s.st.out_frames = s.st.out_frames.wrapping_add(1);
                        s.st.out_bytes = s.st.out_bytes.wrapping_add(n);
                        if s.st.out_frames == 1 {
                            say = -1;
                        }
                    }
                    (say, nerr)
                };
                // gal ignores queueOutgoing's result at every call site;
                // non-zero is counted and logged, never acted on.
                match say {
                    1 => up_loge!(
                        "[GalAAUp] queueOutgoing returned {} (non-zero) \u{2014} {} occurrences; {} bytes were handed over",
                        ret,
                        nerr,
                        n
                    ),
                    -1 => up_log!("[GalAAUp] FIRST frame to phone on ch={}: {} bytes", ch, n),
                    _ => {}
                }
                did = true;
            }

            // ---- attach, with the lock dropped
            if !have_ring {
                if kick {
                    retry_ticks = 0;
                }
                if retry_ticks == 0 {
                    match ring::map_open(self.shm_name) {
                        Ok(m) => {
                            self.publish(m, ever_attached);
                            ever_attached = true;
                            did = true;
                        }
                        Err(OpenError::Absent) => {
                            let (say, nfail) = {
                                let mut s = self.lock();
                                s.st.shm_fail = s.st.shm_fail.wrapping_add(1);
                                (s.rl.shm_fail.allow(), s.st.shm_fail)
                            };
                            if say {
                                up_log!(
                                    "[GalAAUp] /aa-uplink not available yet ({} attempts) \u{2014} external daemon not running?",
                                    nfail
                                );
                            }
                        }
                        Err(e) => {
                            Self::log_incompatible(&e);
                            {
                                let mut s = self.lock();
                                s.ring_incompatible = true;
                                s.disabled = true;
                            }
                            up_loge!(
                                "[GalAAUp] uplink DISABLED (incompatible ring); projection is unaffected"
                            );
                        }
                    }
                    retry_ticks = 250;
                } else {
                    retry_ticks -= 1;
                }
            } else if Cfg::get(&self.cfg.shm_recheck_ms) > 0 && !did {
                // Peer restart detection, idle only (never a syscall storm).
                let now = now_secs();
                if now >= next_recheck {
                    let secs = i64::from(Cfg::get(&self.cfg.shm_recheck_ms) / 1000).max(1);
                    next_recheck = now + secs;
                    if ring::is_stale(self.shm_name, dev, ino) {
                        self.drop_map("peer recreated /aa-uplink");
                        self.lock().attach_kick = true;
                        did = true;
                    }
                }
            }

            if !did {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    /// external daemon data waiting with no open channel to send it on: count and
    /// (rate-limited) report. it stays queued (the open/close flush drops it).
    fn check_no_send(s: &mut State, m: Mapped, out: &mut Option<(u32, u32)>) {
        // SAFETY: a published mapping. lock held.
        let q = unsafe { m.rx_queued() };
        if q != 0 {
            s.st.out_no_send = s.st.out_no_send.wrapping_add(1);
            if s.rl.to_phone_no_send.allow() {
                *out = Some((q, s.st.out_no_send));
            }
        }
    }

    fn log_incompatible(e: &OpenError) {
        match *e {
            OpenError::Incompatible {
                magic,
                abi,
                slot_size,
                slots,
            } => up_loge!(
                "[GalAAUp] /aa-uplink INCOMPATIBLE: magic={:08x}(want {:08x}) abi={}(want {}) slot={}(want {}) slots={}(want {}) \u{2014} uplink disabled for this gal process",
                magic,
                ring::MAGIC,
                abi,
                ABI,
                slot_size,
                ring::SLOT_SIZE,
                slots,
                SLOTS
            ),
            OpenError::BadLayout {
                size,
                tx_off,
                rx_off,
                slots,
                slot_size,
            } => up_loge!(
                "[GalAAUp] /aa-uplink BAD LAYOUT: size={} tx_off={} rx_off={} slots={} slot={} \u{2014} uplink disabled for this gal process",
                size,
                tx_off,
                rx_off,
                slots,
                slot_size
            ),
            OpenError::Absent => {}
        }
    }

    /// `aaup_feature_shutdown`: no new send can be pinned, wait for the one in
    /// flight, stop and join the pump, drop the mapping. Idempotent.
    pub fn shutdown(&self) {
        let join = {
            let mut s = self.lock();
            s.ch_open = false;
            s.mr = 0;
            let mut s = self
                .idle
                .wait_while(s, |s| s.send_inflight > 0)
                .unwrap_or_else(PoisonError::into_inner);
            if s.pump_started {
                s.pump_run = false;
                s.pump_started = false;
                s.pump.take()
            } else {
                None
            }
        };
        if let Some(h) = join {
            let _ = h.join();
        }
        self.drop_map("feature shutdown");
    }
}

/// A pinned send: `send_inflight` was raised. released exactly once, under
/// the lock, by [`SendPin::release`] or (on a panic) by its drop.
struct SendPin<'a>(&'a Uplink);

impl SendPin<'_> {
    fn release(self, s: &mut State) {
        s.send_inflight -= 1;
        if s.send_inflight == 0 {
            self.0.idle.notify_all();
        }
        core::mem::forget(self);
    }
}

impl Drop for SendPin<'_> {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.send_inflight = s.send_inflight.saturating_sub(1);
        if s.send_inflight == 0 {
            self.0.idle.notify_all();
        }
    }
}

/// Test access.
#[cfg(test)]
impl Uplink {
    pub fn install_ring(&self, m: Mapped) {
        self.publish(m, false);
    }

    pub fn stats(&self) -> Stats {
        self.lock().st
    }
}
