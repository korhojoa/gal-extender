// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Constructed opaque-payload mux cases through the production mux path.
//! Expected hashes describe deterministic packet sequences.
//! These checks do not validate video decoding or receiver presentation.

use std::cell::Cell;

use h264_ts_mux::ts::{Mux, MuxIo, PcrPlacement};

use crate::pipe::mux_au;

struct VecSink(Vec<u8>);

impl MuxIo for VecSink {
    fn now90(&mut self) -> u64 {
        0
    }
    fn emit(&mut self, block: &[u8]) {
        self.0.extend_from_slice(block);
    }
}

/// Test matrix: name, frame rate, size, hold time, per-frame mode.
const CASES: [(&str, f64, usize, u64, bool); 5] = [
    ("static-hold400", 9.8, 1278, 400, false),
    ("static-hold100", 9.8, 1278, 100, false),
    ("static-perframe", 9.8, 1278, 400, true),
    ("startup-hold400", 28.0, 1170, 400, false),
    ("startup-hold100", 28.0, 1170, 100, false),
];

/// One `golden` run: 400 frames.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Test matrix values fit the destination types.
fn golden_run(au: &[u8], fps: f64, size: usize, hold: u64, per_frame: bool) -> Vec<u8> {
    let filler: Vec<u8> = (0..size).map(|i| au[i % au.len()]).collect();
    let mut mux = Mux::new();
    mux.pts_lead_ms = 0;
    mux.pcr_placement = PcrPlacement::PesStart;
    let mut out = VecSink(Vec::new());
    let mut pending = 0u64;
    let clock = Cell::new(0u64);
    let period_ms = 1000.0 / fps;
    for i in 0..400u32 {
        clock.set(1000 + (f64::from(i) * period_ms) as u64);
        let pts = ((f64::from(i) * period_ms) * 90.0) as u64;
        let tables = i == 0 || i % 300 == 0;
        let frame = if i == 0 { au } else { &filler[..] };
        mux_au(
            &mut mux,
            &mut out,
            &mut pending,
            frame,
            pts,
            tables,
            if per_frame { 0 } else { hold },
            &|| clock.get(),
        );
        // Exercise the padded-packet option separately from the production path.
        if per_frame {
            mux.flush_block(&mut out);
            pending = 0;
        }
    }
    mux.flush_block(&mut out);
    out.0
}

#[test]
fn constructed_cases_match_expected_packets() {
    let mut checked = 0;
    for line in include_str!("../../../.github/fixtures/mux-goldens.tsv").lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let length: usize = fields[1].parse().expect("constructed length");
        let au: Vec<u8> = (0..length)
            .map(|i| u8::try_from((i * 17 + 3) % 256).unwrap())
            .collect();
        let (_, fps, size, hold, per_frame) = CASES
            .iter()
            .copied()
            .find(|case| case.0 == fields[2])
            .expect("packet case");
        assert_eq!(
            sha256_hex(&golden_run(&au, fps, size, hold, per_frame)),
            fields[0]
        );
        checked += 1;
    }
    assert_eq!(checked, 15);
}

/// SHA-256 (FIPS 180-4), for the test only.
#[allow(
    clippy::too_many_lines,
    reason = "the round constants and one compression loop, kept together as the standard writes them"
)]
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().fold(String::new(), |mut s, x| {
        use core::fmt::Write as _;
        let _ = write!(s, "{x:08x}");
        s
    })
}

#[test]
fn sha256_known_answer() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
