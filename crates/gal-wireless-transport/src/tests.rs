// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use super::*;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;

fn hex(bytes: Option<&[u8]>, len: u32) -> String {
    let mut l: Line<64> = Line::new();
    hex_preview(
        &mut l,
        bytes.map_or(core::ptr::null(), |b| b.as_ptr().cast()),
        len,
    );
    l.as_str().to_string()
}

#[test]
fn hex_preview_is_bounded_lowercase_text() {
    let b: Vec<u8> = (0u8..40).map(|i| i.wrapping_mul(37)).collect();
    assert_eq!(hex(Some(&b), 3), "00 25 4a");
    assert_eq!(
        hex(Some(&b), 40),
        "00 25 4a 6f 94 b9 de 03 28 4d 72 97 bc e1 06 2b"
    );
    assert_eq!(hex(Some(&b), 0), "");
    assert_eq!(hex(None, 16), "");
}

#[test]
fn pointers_print_as_qnx_percent_p() {
    assert_eq!(CPtr(0xB00B).to_string(), "b00b");
    assert_eq!(CPtr(0).to_string(), "0");
}

#[test]
fn stats_start_the_clock_then_report_every_two_seconds() {
    let s = Stats::new();
    for _ in 0..VERBOSE_FIRST {
        assert!(s.count(10));
    }
    assert!(!s.count(10));
    assert_eq!(s.due(100), None, "the first call starts the clock");
    assert!(!s.count(5));
    assert_eq!(s.due(101), None);
    assert_eq!(s.due(102), Some((95, 10, 2)));
    assert_eq!(s.due(103), None);
    assert!(!s.count(7));
    assert_eq!(s.due(107), Some((7, 1, 5)));
}

#[test]
fn atoi_is_strtol_base_10() {
    assert_eq!(atoi(b"5288"), 5288);
    assert_eq!(atoi(b"  +5290abc"), 5290);
    assert_eq!(atoi(b"\t-7"), -7);
    assert_eq!(atoi(b"abc"), 0);
    assert_eq!(atoi(b""), 0);
    assert_eq!(atoi(b"99999999999"), i32::MAX);
    assert_eq!(atoi(b"-99999999999"), i32::MIN);
}

#[test]
fn port_comes_from_the_conf_then_the_env() {
    assert_eq!(
        port_source(5288, Some(b"1")),
        (5288, "wireless_tcp_port(conf)")
    );
    assert_eq!(port_source(0, None), (0, "wireless_tcp_port(conf)"));
    assert_eq!(port_source(0, Some(b"")), (0, "wireless_tcp_port(conf)"));
    assert_eq!(port_source(-3, Some(b"")), (-3, "wireless_tcp_port(conf)"));
    assert_eq!(
        port_source(0, Some(b" 77x")),
        (77, "GAL_WIRELESS_TCP_PORT(env)")
    );
    assert_eq!(
        port_source(-1, Some(b"0")),
        (0, "GAL_WIRELESS_TCP_PORT(env)")
    );
}

fn wait_for(f: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The providers over a real socket: exact transfers both ways, a closed
/// phone fails the read and drops the client, and the listener takes the
/// next phone.
#[test]
fn providers_carry_bytes_over_tcp_and_reaccept() {
    const PORT: u16 = 45_288;
    std::thread::spawn(|| listen_thread(i32::from(PORT)));
    let connect = || {
        for _ in 0..200 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", PORT)) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no listener on {PORT}");
    };

    let mut phone = connect();
    assert!(wait_for(|| CLIENT_FD.load(Ordering::Acquire) >= 0));
    let first_fd = CLIENT_FD.load(Ordering::Acquire);
    phone.write_all(b"\x00\x03\x00\x06hello!").unwrap();
    let mut buf = [0u8; 10];
    assert_eq!(
        provider_read(core::ptr::null_mut(), buf.as_mut_ptr().cast(), 4),
        4
    );
    assert_eq!(&buf[..4], b"\x00\x03\x00\x06");
    assert_eq!(
        provider_read(core::ptr::null_mut(), buf.as_mut_ptr().cast(), 6),
        6
    );
    assert_eq!(&buf[..6], b"hello!");

    let out = b"from gal";
    assert_eq!(
        provider_write(core::ptr::null_mut(), out.as_ptr().cast(), 8),
        8
    );
    let mut got = [0u8; 8];
    phone.read_exact(&mut got).unwrap();
    assert_eq!(&got, out);

    // The phone goes: the next read fails and the client is dropped.
    drop(phone);
    assert_eq!(
        provider_read(core::ptr::null_mut(), buf.as_mut_ptr().cast(), 4),
        0
    );
    assert_eq!(CLIENT_FD.load(Ordering::Acquire), -1);
    // Dropping an fd that is no longer the client does nothing.
    drop_client(first_fd);

    // The listener takes the next phone.
    let mut phone = connect();
    assert!(wait_for(|| CLIENT_FD.load(Ordering::Acquire) >= 0));
    phone.write_all(b"ab").unwrap();
    assert_eq!(
        provider_read(core::ptr::null_mut(), buf.as_mut_ptr().cast(), 2),
        2
    );
    assert_eq!(&buf[..2], b"ab");
    drop(phone);
    assert_eq!(
        provider_read(core::ptr::null_mut(), buf.as_mut_ptr().cast(), 1),
        0
    );
}
