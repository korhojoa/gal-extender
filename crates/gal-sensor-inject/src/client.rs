// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! EXLAP transport through the published exlap crate. The protocol machine handles
//! initialization, capabilities, authentication, numbered requests and subscriptions.
//! Handshake timeout is three seconds. Receive timeout and heartbeat interval are two
//! seconds. A receive timeout keeps the session open. other receive failures close it.
//! Capture errno immediately after failure. Malformed replies can be rejected. protocol
//! tests do not prove target operation.

use core::ffi::{c_int, c_void};
use core::sync::atomic::Ordering;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd as _;
use std::time::Duration;

use exlap::{Machine, Rate, USERS};
use gal_feature_api::config;

use crate::log::{errno, s_loge, CBytesN, StrError};
use crate::parse::{self, cstr, find};
use crate::{ev, sensors};

const EXLAP_HOST: &str = "127.0.0.1";
const EXLAP_PORT: u16 = 25010;
const EXLAP_BUFSZ: usize = 8192;
const EXLAP_CREDS: i32 = 4;

/// The subscriptions, in order (`exlap_subs[]`).
pub const SUBS: [(&str, u32); 16] = [
    ("outsideTemperature", 1000),
    ("engineSpeed", 200),
    ("tankLevelPrimary", 2000),
    ("lateralAcceleration", 100),
    ("longitudinalAcceleration", 100),
    ("yawRate", 100),
    ("wheelAngle", 100),
    ("Nav_Heading", 500),
    ("doorState", 2000),
    ("lightState_front", 1000),
    ("blinkingState", 500),
    ("parkingBrake", 1000),
    ("accIsActive", 500),
    ("tyrePressures", 5000),
    ("recuperationLevel", 500),
    ("hevOperationMode", 1000),
];

#[cfg(target_os = "nto")]
const EAGAIN: i32 = 11;
#[cfg(target_os = "nto")]
const ETIMEDOUT: i32 = 260;
#[cfg(not(target_os = "nto"))]
const EAGAIN: i32 = 11;
#[cfg(not(target_os = "nto"))]
const ETIMEDOUT: i32 = 110;

extern "C" {
    fn send(fd: c_int, buf: *const c_void, n: usize, flags: c_int) -> isize;
    fn recv(fd: c_int, buf: *mut c_void, n: usize, flags: c_int) -> isize;
    fn getpid() -> c_int;
}

// ---------------------------------------------------------------------------
// The client nonce (rand_bytes + base64)
// ---------------------------------------------------------------------------

/// Read 16 nonce bytes from /dev/random. If that fails, use the clock, process ID and
/// bounded generator.
fn rand_bytes(out: &mut [u8; 16]) {
    use std::io::Read as _;

    if let Ok(mut f) = std::fs::File::open("/dev/random") {
        let mut got = 0;
        while got < out.len() {
            match f.read(&mut out[got..]) {
                Ok(r) if r > 0 => got += r,
                _ => break,
            }
        }
        if got == out.len() {
            return;
        }
    }
    let ts = qnx_slog::sys::wall_ms();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // C casts
    let mut s: u32 = ((ts % 1000) as u32 * 1_000_000) ^ ((ts / 1000) as u32)
        // SAFETY: getpid has no preconditions.
        ^ (unsafe { getpid() } as u32);
    for b in out.iter_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (s >> 24) as u8;
    }
}

/// Standard base64 with padding (`b64_encode`).
fn b64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    for c in input.chunks(3) {
        let v = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        o.push(char::from(A[(v >> 18) as usize & 63]));
        o.push(char::from(A[(v >> 12) as usize & 63]));
        o.push(if c.len() > 1 {
            char::from(A[(v >> 6) as usize & 63])
        } else {
            '='
        });
        o.push(if c.len() > 2 {
            char::from(A[v as usize & 63])
        } else {
            '='
        });
    }
    o
}

fn cnonce() -> String {
    let mut b = [0u8; 16];
    rand_bytes(&mut b);
    b64(&b)
}

// ---------------------------------------------------------------------------
// One connection
// ---------------------------------------------------------------------------

struct Conn {
    sock: TcpStream,
    /// The errno of the last failed `recv` (0: the connection closed).
    recv_err: core::cell::Cell<i32>,
}

impl Conn {
    fn fd(&self) -> c_int {
        self.sock.as_raw_fd()
    }

    /// Send one EXLAP message and report failures.
    fn send(&self, msg: &str) {
        // SAFETY: a valid buffer of msg.len() bytes on our socket.
        let n = unsafe { send(self.fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if n < 0 {
            s_loge!("[GalSensor] exlap send failed: {}", StrError(errno()));
        }
    }

    /// Send everything `m` queued, in order.
    fn flush(&self, m: &mut Machine) {
        for msg in m.take_outbound() {
            self.send(&msg);
        }
    }

    /// `exlap_recv_msg(buf, cap)`: the length of one complete message (the
    /// close of its root element), the buffer length when full, or None on
    /// a failed or empty `recv`. `buf[len]` is 0.
    fn recv_msg(&self, buf: &mut [u8]) -> Option<usize> {
        let cap = buf.len();
        let (mut pos, mut depth, mut started, mut inq) = (0usize, 0i32, false, false);
        while pos < cap - 1 {
            // SAFETY: one byte into our buffer.
            let n = unsafe { recv(self.fd(), buf[pos..].as_mut_ptr().cast(), 1, 0) };
            if n <= 0 {
                // errno of the failed recv. 0 for a closed connection.
                self.recv_err.set(if n < 0 { errno() } else { 0 });
                return None;
            }
            let c = buf[pos];
            pos += 1;
            buf[pos] = 0;
            if c == b'"' {
                inq = !inq;
                continue;
            }
            if inq || c != b'>' {
                continue;
            }
            let Some(j) = buf[..pos].iter().rposition(|&b| b == b'<') else {
                continue;
            };
            let first = buf[j + 1];
            let prev = buf[pos - 2];
            if first == b'?' || first == b'!' {
                // <?xml?> / <!-- -->: not an element
            } else if first == b'/' {
                depth -= 1;
                started = true;
            } else if prev == b'/' {
                started = true;
            } else {
                depth += 1;
                started = true;
            }
            if started && depth <= 0 {
                return Some(pos);
            }
        }
        Some(pos)
    }
}

/// The message text for `Machine::feed`.
fn text(buf: &[u8]) -> String {
    String::from_utf8_lossy(cstr(buf)).into_owned()
}

fn user(cred: usize) -> &'static str {
    USERS[cred % USERS.len()]
}

/// Read one handshake message and log a timeout if none arrives.
fn expect(conn: &Conn, buf: &mut [u8], _what: &str, timeout_line: &str) -> bool {
    if conn.recv_msg(buf).is_none() {
        s_loge!("[GalSensor] EXLAP: {}", timeout_line);
        return false;
    }
    true
}

/// The handshake of `exlap_session`, driven through `Machine`: Init,
/// capabilities, challenge, authentication. False (logged) on failure.
fn handshake(conn: &Conn, mach: &mut Machine, buf: &mut [u8], cred: usize) -> bool {
    if !expect(
        conn,
        buf,
        "Init",
        "no Init within 3s \u{2014} server silent after connect",
    ) {
        return false;
    }
    if find(cstr(buf), b"Init").is_none() {
        s_loge!("[GalSensor] expected Init, got: {}", CBytesN(buf, 80));
        return false;
    }
    let _ = mach.feed(&text(buf));
    conn.flush(mach);

    if !expect(
        conn,
        buf,
        "capabilities Rsp",
        "no capabilities Rsp (timeout)",
    ) {
        return false;
    }
    let _ = mach.feed(&text(buf));
    conn.flush(mach);

    if !expect(conn, buf, "auth challenge", "no auth challenge (timeout)") {
        return false;
    }
    if parse::attr(buf, b"nonce", 128).is_none() || mach.feed(&text(buf)).is_err() {
        s_loge!(
            "[GalSensor] auth challenge: no nonce in: {}",
            CBytesN(buf, 80)
        );
        return false;
    }
    conn.flush(mach);

    if !expect(conn, buf, "auth result", "no auth result (timeout)") {
        return false;
    }
    if mach.feed(&text(buf)).is_err() {
        s_loge!("[GalSensor] auth failed for user {}", user(cred));
        return false;
    }
    conn.flush(mach);
    true
}

/// The receive loop of `exlap_session`: `<Dat>` messages and the 2 s
/// heartbeat, until the connection fails.
fn serve(conn: &Conn, mach: &mut Machine, buf: &mut [u8]) {
    // SAFETY: time(NULL).
    let now = || unsafe { qnx_slog::sys::time(core::ptr::null_mut()) };
    let mut last_hb = now();
    let _ = conn.sock.set_read_timeout(Some(Duration::from_secs(2)));

    loop {
        let got = conn.recv_msg(buf);
        let tick = now();
        if tick - last_hb >= 2 {
            mach.heartbeat();
            conn.flush(mach);
            if ev::PENDING.load(Ordering::Relaxed) {
                ev::push_ev_model();
            }
            if config::get_int(c"sensor_raw_ev_trip", 0) != 0 {
                ev::push_raw_ev_trip();
            }
            last_hb = tick;
        }
        if got.is_none() {
            let err = conn.recv_err.get();
            if err == EAGAIN || err == ETIMEDOUT {
                continue;
            }
            return;
        }
        let msg = cstr(buf);
        if find(msg, b"<Dat ").is_some() {
            if let Some(url) = parse::attr(msg, b"url", 64) {
                sensors::dispatch(&url, msg);
            }
        }
    }
}

/// Connect, authenticate, subscribe and serve vehicle data until the
/// connection fails.
fn session(cred: usize) {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, EXLAP_PORT);
    let sock = match TcpStream::connect(addr) {
        Ok(sock) => sock,
        Err(err) => {
            s_loge!(
                "[GalSensor] connect {}:{}: {}",
                EXLAP_HOST,
                EXLAP_PORT,
                StrError(err.raw_os_error().unwrap_or(0))
            );
            return;
        }
    };
    let conn = Conn {
        sock,
        recv_err: core::cell::Cell::new(0),
    };
    let _ = conn.sock.set_read_timeout(Some(Duration::from_secs(3)));

    let mut mach = Machine::new(cred).with_cnonce(cnonce);
    let mut buf = vec![0u8; EXLAP_BUFSZ];
    if !handshake(&conn, &mut mach, &mut buf, cred) {
        s_loge!("[GalSensor] EXLAP session ended (cred={})", cred);
        return;
    }

    let rates: Vec<Rate> = SUBS.iter().map(|&(u, i)| Rate::new(u, i)).collect();
    let _ = mach.subscribe(&rates, None, None);
    conn.flush(&mut mach);

    sensors::resolve_all();
    serve(&conn, &mut mach, &mut buf);
    s_loge!("[GalSensor] EXLAP session ended (cred={})", cred);
}

/// One session, a panic in it contained and logged (the thread goes on).
fn guarded_session(cred: usize) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session(cred);
    }));
    if r.is_err() {
        s_loge!("[GalSensor] PANIC contained in the EXLAP session; it is restarted");
    }
}

/// `exlap_thread_func`.
pub fn thread_main() {
    let mut cred = config::get_int(c"sensor_cred", 2);
    if !(0..EXLAP_CREDS).contains(&cred) {
        cred = 2;
    }
    loop {
        guarded_session(usize::try_from(cred).unwrap_or(2));
        cred = (cred + 1) % EXLAP_CREDS;
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_known_inputs() {
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"abc"), "YWJj");
        let fixed: Vec<u8> = (0u8..16).collect();
        assert_eq!(b64(&fixed), "AAECAwQFBgcICQoLDA0ODw==");
    }

    #[test]
    fn subscriptions_match_expected_entries() {
        assert_eq!(SUBS.len(), 16);
        assert_eq!(SUBS[0], ("outsideTemperature", 1000));
    }
}
