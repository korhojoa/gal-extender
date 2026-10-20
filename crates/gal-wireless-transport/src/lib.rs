// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Optional transport observation and TCP transport module.
//!
//! The loader must enable the `wireless` build feature. Observation copies
//! transport diagnostics and does not change the original read/write data.
//! The first eight operations include a bounded byte preview. later logs
//! report throughput. TCP mode requires an explicit port configuration.
//! It accepts one phone connection and supplies the loader's transport
//! providers. A source build does not qualify a phone or radio session.

mod net;

use core::ffi::{c_int, c_void};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use gal_feature_api::{config, hooks, services, FeatureModule};
use qnx_slog::log::Line;
use qnx_slog::sys::{SLOG_ERROR, SLOG_INFO};

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule {
    name: "wireless",
    init,
};

/// The version in the banner: the single copy. Bump per change.
const VERSION: &str = "3-rs";

/// Log the first N reads / writes in full (`VERBOSE_FIRST`).
const VERBOSE_FIRST: u32 = 8;
/// Then a throughput line this often, in `time()` seconds (`REPORT_SECS`).
const REPORT_SECS: i64 = 2;
/// How long a provider call waits for a phone (`wl_wait_client`).
const WAIT_CLIENT_MS: u32 = 15_000;
const WAIT_STEP_MS: u32 = 50;
/// The AOAP accessory identity of the synthetic insertion.
const AOAP_VENDOR: u32 = 0x18D1;
const AOAP_PRODUCT: u32 = 0x2D00;

fn log_at(sev: c_int, args: fmt::Arguments<'_>) {
    let mut msg: Line<400> = Line::new();
    let _ = msg.write_fmt(args);
    qnx_slog::log::raw(sev, format_args!("[GalWireless] {}\n", msg.as_str()));
}

macro_rules! wl_log {
    ($($a:tt)*) => { log_at(SLOG_INFO, format_args!($($a)*)) };
}
macro_rules! wl_loge {
    ($($a:tt)*) => { log_at(SLOG_ERROR, format_args!($($a)*)) };
}

/// A pointer printed as `%p` prints it with QNX 6.5's libc: lower-case hex,
/// no `0x`, and NULL as `0`.
struct CPtr(usize);

impl fmt::Display for CPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// Run a hook or provider body. a panic is logged and `fallback` returned.
fn guarded<R>(site: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    if let Ok(r) = catch_unwind(AssertUnwindSafe(f)) {
        r
    } else {
        wl_loge!("PANIC contained in {}", site);
        fallback
    }
}

struct Stats {
    bytes: AtomicU64,
    calls: AtomicU32,
    logged: AtomicU32,
    /// `time()` of the last report. 0 = no call yet.
    last_report: AtomicI64,
}

impl Stats {
    const fn new() -> Self {
        Stats {
            bytes: AtomicU64::new(0),
            calls: AtomicU32::new(0),
            logged: AtomicU32::new(0),
            last_report: AtomicI64::new(0),
        }
    }

    /// Count one call. true when it is one of the first [`VERBOSE_FIRST`]
    /// (the caller logs it).
    fn count(&self, bytes: u32) -> bool {
        self.bytes.fetch_add(u64::from(bytes), Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.logged.load(Ordering::Relaxed) < VERBOSE_FIRST {
            self.logged.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// `maybe_report_rx` / `maybe_report_tx`: the window, bytes and calls to
    /// report (then reset) when [`REPORT_SECS`] have passed since the last
    /// report. the first call only starts the clock.
    fn due(&self, now: i64) -> Option<(u64, u32, i64)> {
        let last = self.last_report.load(Ordering::Relaxed);
        if last == 0 {
            self.last_report.store(now, Ordering::Relaxed);
            return None;
        }
        if now - last < REPORT_SECS {
            return None;
        }
        let r = (
            self.bytes.swap(0, Ordering::Relaxed),
            self.calls.swap(0, Ordering::Relaxed),
            now - last,
        );
        self.last_report.store(now, Ordering::Relaxed);
        Some(r)
    }
}

static RX: Stats = Stats::new();
static TX: Stats = Stats::new();

#[allow(clippy::useless_conversion)] // c_long: 32 bits on the target, 64 on the host
fn now_secs() -> i64 {
    // SAFETY: time(NULL).
    i64::from(unsafe { qnx_time::time(core::ptr::null_mut()) })
}

/// `hex_preview`: up to 16 leading bytes as `"xx xx .."` (no trailing
/// space). empty for NULL or no bytes.
fn hex_preview(out: &mut Line<64>, buf: *const c_void, len: u32) {
    if buf.is_null() {
        return;
    }
    let n = len.min(16) as usize;
    // SAFETY: gal's buffer holds `len` (>= n) bytes.
    let bytes = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), n) };
    for (i, b) in bytes.iter().enumerate() {
        let _ = if i == 0 {
            write!(out, "{b:02x}")
        } else {
            write!(out, " {b:02x}")
        };
    }
}

/// The read tap. `ret` is readOrFail's success flag (1 ok, <= 0 failed):
/// on success exactly `len` bytes were read into `buf`.
fn on_read(this: *mut c_void, buf: *const c_void, len: u32, ret: i32) {
    guarded("the readOrFail tap", (), || {
        let got = if ret > 0 { len } else { 0 };
        if RX.count(got) {
            let mut hx: Line<64> = Line::new();
            hex_preview(&mut hx, buf, got);
            wl_log!(
                "readOrFail(self={}, len={}) -> {}  [{}]",
                CPtr(this as usize),
                len,
                if ret > 0 { "ok" } else { "fail" },
                hx.as_str()
            );
        }
        if let Some((bytes, calls, window)) = RX.due(now_secs()) {
            wl_log!("rx stats: {bytes} B / {calls} calls  ({window} s window)");
        }
    });
}

/// The write tap (called before the write happens).
fn on_write(this: *mut c_void, buf: *const c_void, len: u32) {
    guarded("the writeOrFail tap", (), || {
        if TX.count(len) {
            let mut hx: Line<64> = Line::new();
            hex_preview(&mut hx, buf, len);
            wl_log!(
                "writeOrFail(self={}, len={})  [{}]",
                CPtr(this as usize),
                len,
                hx.as_str()
            );
        }
        if let Some((bytes, calls, window)) = TX.due(now_secs()) {
            wl_log!("tx stats: {bytes} B / {calls} calls  ({window} s window)");
        }
    });
}

// ---------------------------------------------------------------------------
// TCP transport provider (the wireless data plane)
// ---------------------------------------------------------------------------

/// The connected phone's socket, -1 when none (`s_client_fd`).
static CLIENT_FD: AtomicI32 = AtomicI32::new(-1);
/// Taken to set or drop the client (`s_fd_mtx`).
static FD_MTX: Mutex<()> = Mutex::new(());

fn fd_lock() -> std::sync::MutexGuard<'static, ()> {
    FD_MTX.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `wl_drop_client`: close `fd` when it is still the client.
fn drop_client(fd: c_int) {
    if fd < 0 {
        return;
    }
    let _g = fd_lock();
    if CLIENT_FD.load(Ordering::Acquire) == fd {
        CLIENT_FD.store(-1, Ordering::Release);
        // SAFETY: our socket. shutdown wakes a peer thread blocked on it.
        unsafe {
            net::shutdown(fd, net::SHUT_RDWR);
            net::close(fd);
        }
        wl_log!("phone TCP connection closed (fd={})", fd);
    }
}

/// `wl_wait_client`: the client fd, waiting up to 15 s for one. -1 when
/// none came.
fn wait_client() -> c_int {
    let mut waited = 0;
    while CLIENT_FD.load(Ordering::Acquire) < 0 && waited < WAIT_CLIENT_MS {
        std::thread::sleep(Duration::from_millis(u64::from(WAIT_STEP_MS)));
        waited += WAIT_STEP_MS;
    }
    CLIENT_FD.load(Ordering::Acquire)
}

/// The value readOrFail / writeOrFail return for a full transfer: `(int)len`.
#[allow(clippy::cast_possible_wrap)]
fn full(len: u32) -> i32 {
    len as i32
}

/// Move exactly `len` bytes with `io` (recv or send), retrying on EINTR.
/// False on EOF or an error: the client is dropped.
fn transfer(fd: c_int, len: u32, mut io: impl FnMut(usize, usize) -> isize) -> bool {
    let len = len as usize;
    let mut done = 0usize;
    while done < len {
        let n = io(done, len - done);
        if n > 0 {
            done += n.unsigned_abs();
            continue;
        }
        if n < 0 && qnx_errno::errno() == net::EINTR {
            continue;
        }
        drop_client(fd);
        return false;
    }
    true
}

/// The read provider: readOrFail semantics, exactly `len` bytes or 0.
#[allow(clippy::not_unsafe_ptr_arg_deref)] // gal's buffer of `len` bytes
fn provider_read(_this: *mut c_void, buf: *mut c_void, len: u32) -> i32 {
    guarded("the read provider", 0, || {
        let fd = wait_client();
        if fd < 0 {
            wl_loge!("provider_read: no phone connected");
            return 0;
        }
        let p = buf.cast::<u8>();
        // SAFETY: gal's buffer holds `len` bytes. `at + n <= len`.
        let ok = transfer(fd, len, |at, n| unsafe {
            net::recv(fd, p.add(at).cast(), n, 0)
        });
        if ok {
            full(len)
        } else {
            0
        }
    })
}

/// The write provider: writeOrFail semantics, exactly `len` bytes or 0.
/// Use `send` with flags zero. A disconnected phone can raise SIGPIPE.
#[allow(clippy::not_unsafe_ptr_arg_deref)] // gal's buffer of `len` bytes
fn provider_write(_this: *mut c_void, buf: *const c_void, len: u32) -> i32 {
    guarded("the write provider", 0, || {
        let fd = wait_client();
        if fd < 0 {
            wl_loge!("provider_write: no phone connected");
            return 0;
        }
        let p = buf.cast::<u8>();
        // SAFETY: gal's buffer holds `len` bytes. `at + n <= len`.
        let ok = transfer(fd, len, |at, n| unsafe {
            net::send(fd, p.add(at).cast(), n, 0)
        });
        if ok {
            full(len)
        } else {
            0
        }
    })
}

/// `wl_listen_thread`: listen on 0.0.0.0:`port`, one phone at a time.
fn listen_thread(port: i32) {
    // SAFETY: plain socket call.
    let lfd = unsafe { net::socket(net::AF_INET, net::SOCK_STREAM, 0) };
    if lfd < 0 {
        wl_loge!("listen socket() failed: {}", net::strerror_errno());
        return;
    }
    net::set_int_opt(lfd, net::SOL_SOCKET, net::SO_REUSEADDR);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sa = net::SockaddrIn::any(port as u16);
    let sa_len = net::SOCKADDR_IN_LEN;
    // SAFETY: a valid sockaddr_in and its size.
    if unsafe { net::bind(lfd, &raw const sa, sa_len) } < 0 {
        wl_loge!("bind(:{}) failed: {}", port, net::strerror_errno());
        // SAFETY: our socket.
        unsafe { net::close(lfd) };
        return;
    }
    // SAFETY: our socket.
    if unsafe { net::listen(lfd, 1) } < 0 {
        wl_loge!("listen(:{}) failed: {}", port, net::strerror_errno());
        // SAFETY: our socket.
        unsafe { net::close(lfd) };
        return;
    }
    wl_log!("TCP transport server listening on 0.0.0.0:{}", port);
    loop {
        let mut ca = net::SockaddrIn::default();
        let mut cl = sa_len;
        // SAFETY: valid out-pointers.
        let cfd = unsafe { net::accept(lfd, &raw mut ca, &raw mut cl) };
        if cfd < 0 {
            if qnx_errno::errno() == net::EINTR {
                continue;
            }
            wl_loge!("accept failed: {}", net::strerror_errno());
            break;
        }
        net::set_int_opt(cfd, net::IPPROTO_TCP, net::TCP_NODELAY);
        let [a, b, c, d] = ca.ip();
        wl_log!(
            "phone connected from {}.{}.{}.{}:{} (fd={})",
            a,
            b,
            c,
            d,
            ca.port(),
            cfd
        );
        {
            let _g = fd_lock();
            CLIENT_FD.store(cfd, Ordering::Release);
        }
        // The phone is on the socket: make gal start a transport, which then
        // drives the providers.
        if services::usbdi_fire_insertion(AOAP_VENDOR, AOAP_PRODUCT) != 0 {
            wl_log!("USB insertion fired -> gal should start a transport");
        } else {
            wl_loge!("USB insertion NOT fired (USBDI not captured yet)");
        }
        // One phone at a time: wait until this connection is dropped.
        while CLIENT_FD.load(Ordering::Acquire) == cfd {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // SAFETY: our socket.
    unsafe { net::close(lfd) };
}

/// `wl_start_tcp`: the listener thread, then the providers.
fn start_tcp(port: i32) {
    // Set the fd mutex up here, not on gal's reader thread.
    drop(fd_lock());
    let spawned = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| listen_thread(port))).is_err() {
                wl_loge!("PANIC contained in the listener thread; no more phone connections");
            }
        });
    if spawned.is_err() {
        wl_loge!("pthread_create(listener) failed: {}", net::strerror_errno());
    }
    hooks::READ_PROVIDER.set(Some(provider_read));
    hooks::WRITE_PROVIDER.set(Some(provider_write));
    wl_log!("TCP provider installed (readOrFail/writeOrFail -> socket)");
}

/// `atoi`: QNX's is `(int)strtol(s, NULL, 10)`: leading white space, a sign,
/// digits up to the first non-digit, clamped to the int range (long is 32
/// bits on the target).
fn atoi(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut v: i64 = 0;
    while let Some(d) = s.get(i).filter(|c| c.is_ascii_digit()) {
        v = (v * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX) + 1);
        i += 1;
    }
    let v = if neg { -v } else { v };
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

/// The port and where it came from: the conf key first, else a non-empty
/// env `GAL_WIRELESS_TCP_PORT`.
fn port_source(conf: i32, env: Option<&[u8]>) -> (i32, &'static str) {
    if conf > 0 {
        return (conf, "wireless_tcp_port(conf)");
    }
    match env {
        Some(e) if !e.is_empty() => (atoi(e), "GAL_WIRELESS_TCP_PORT(env)"),
        _ => (conf, "wireless_tcp_port(conf)"),
    }
}

// ---------------------------------------------------------------------------
// init (the loader calls it once, on the constructor thread, panic-guarded)
// ---------------------------------------------------------------------------

fn init() {
    wl_log!("wireless v{}", VERSION);
    hooks::TRANSPORT_READ.register(on_read);
    hooks::TRANSPORT_WRITE.register(on_write);
    wl_log!("registered Transport::readOrFail / writeOrFail taps");

    let conf = config::get_int(c"wireless_tcp_port", 0);
    let env = if conf > 0 {
        None
    } else {
        net::env(c"GAL_WIRELESS_TCP_PORT")
    };
    let (port, src) = port_source(conf, env);
    if port > 0 {
        wl_log!("{}={} -> enabling TCP provider mode", src, port);
        start_tcp(port);
    } else {
        wl_log!(
            "tap-only (set wireless_tcp_port in conf or GAL_WIRELESS_TCP_PORT env to route AA over TCP)"
        );
    }
}

#[cfg(test)]
mod tests;
