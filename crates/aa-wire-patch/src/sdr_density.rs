// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Read and modify serialized `ServiceDiscoveryResponse` fields. The tests use a
//! constructed 217-byte fixture. A malformed field stops the current walk and keeps
//! earlier changes. The protobuf field iterator uses that same stop rule.

use protobuf_wire::varint;
use protobuf_wire::wire::{self, Field, LEN, VARINT};

/// Counts from a density patch: fields rewritten, and fields left alone because
/// the new value's varint width differs from the old (length-changing values are
/// refused rather than corrupt the enclosing length prefixes).
pub struct PatchResult {
    pub patched: u32,
    pub refused: u32,
}

/// Iterate buf[start..end]. Stop at the first malformed field.
fn walk(buf: &[u8], start: usize, end: usize) -> impl Iterator<Item = Field> + '_ {
    wire::fields(buf, start, end)
}

// -- density patch -------------------------------------------------------------

/// Rewrite MAIN display density in place. Walk each length-delimited service, its
/// `media_sink_service` field3, `display_id` zero, and `video_configs` field4. In codec
/// resolution2, replace density fields5 and9 only when their varint width stays
/// unchanged. Count refused writes. A malformed field stops the current walk.
pub fn patch_main_density(buf: &mut [u8], dpi: u32) -> PatchResult {
    let mut r = PatchResult {
        patched: 0,
        refused: 0,
    };
    let end = buf.len();
    // Treat every top-level length-delimited field as a candidate service. An identity
    // string has no applicable media sink field.
    let mut i = 0usize;
    while i < end {
        let Some(f) = wire::read_field(buf, i, end) else {
            break;
        };
        if f.wire == LEN {
            patch_service(buf, f.value, f.end(), dpi, &mut r);
        }
        i = f.end();
    }
    r
}

/// `Service.media_sink_service` is field 3.
fn patch_service(buf: &mut [u8], start: usize, end: usize, dpi: u32, r: &mut PatchResult) {
    let mut i = start;
    while i < end {
        let Some(f) = wire::read_field(buf, i, end) else {
            break;
        };
        if f.number == 3 && f.wire == LEN {
            patch_mediasink(buf, f.value, f.end(), dpi, r);
        }
        i = f.end();
    }
}

/// Return the last matching varint value, or zero when the field is absent. This is the
/// first pass of each two-pass walker.
fn last_varint(buf: &[u8], start: usize, end: usize, number: u32) -> u64 {
    let mut v = 0u64;
    for f in walk(buf, start, end) {
        if f.number == number && f.wire == VARINT {
            if let Some((x, _)) = varint::read(buf.get(f.value..f.end()).unwrap_or(&[])) {
                v = x;
            }
        }
    }
    v
}

/// One `MediaSinkService`: only the MAIN display (field 6 `display_id` absent or 0).
fn patch_mediasink(buf: &mut [u8], start: usize, end: usize, dpi: u32, r: &mut PatchResult) {
    // pass 1: whose display is this?
    if last_varint(buf, start, end, 6) != 0 {
        return; // a secondary display,not ours to touch
    }
    // pass 2: patch its video configs (field 4)
    let mut i = start;
    while i < end {
        let Some(f) = wire::read_field(buf, i, end) else {
            break;
        };
        if f.number == 4 && f.wire == LEN {
            patch_videoconfig(buf, f.value, f.end(), dpi, r);
        }
        i = f.end();
    }
}

/// One `VideoConfiguration`: patch density(5)/`real_density`(9), but ONLY the native
/// 1280x720 configs (`codec_res` field 1 == 2).
fn patch_videoconfig(buf: &mut [u8], start: usize, end: usize, dpi: u32, r: &mut PatchResult) {
    // pass 1: which resolution?
    if last_varint(buf, start, end, 1) != 2 {
        return; // not VIDEO_1280x720,leave alone
    }
    // pass 2: patch density / real_density
    let dpi_w = varint::width(u64::from(dpi));
    let mut i = start;
    while i < end {
        let Some(f) = wire::read_field(buf, i, end) else {
            break;
        };
        if (f.number == 5 || f.number == 9) && f.wire == VARINT {
            let old = varint::read(buf.get(f.value..f.end()).unwrap_or(&[])).map_or(0, |x| x.0);
            if dpi_w == f.len {
                if old != u64::from(dpi) {
                    // same width => the canonical encoding is exactly f.len
                    // bytes, written in place
                    if wire::patch_varint_fixed_width(buf, f.value, f.len, u64::from(dpi)) {
                        r.patched += 1;
                    }
                }
            } else {
                r.refused += 1; // length would change,leave it alone
            }
        }
        i = f.end();
    }
}

// -- frame-rate patch ---------------------------------------------------------

/// `VideoConfiguration.frame_rate` (field 2) enum values: `VIDEO_FPS_60` = 1,
/// `VIDEO_FPS_30` = 2 (the same enum `build_cluster_sdr_bytes` uses).
pub const VIDEO_FPS_60: u64 = 1;
pub const VIDEO_FPS_30: u64 = 2;

/// Rewrite every MAIN-display `VideoConfiguration` offered at 60 fps to 30 fps,
/// in place. The phone picks the config (its Start message carries the chosen
/// index), so once no 60 fps entry is offered it cannot choose one. Both enum
/// values are one-byte varints, so the SDR length and the config order (the
/// indices) never change. Secondary displays (`display_id` != 0) are left
/// alone. `refused` counts a 60 value not encoded in one byte (not seen on
/// a real SDR. left untouched rather than change a length).
pub fn patch_main_fps30(buf: &mut [u8]) -> PatchResult {
    let mut r = PatchResult {
        patched: 0,
        refused: 0,
    };
    let end = buf.len();
    let mut i = 0usize;
    while i < end {
        let Some(svc) = wire::read_field(buf, i, end) else {
            break;
        };
        if svc.wire == LEN {
            let mut j = svc.value;
            while j < svc.end() {
                let Some(sink) = wire::read_field(buf, j, svc.end()) else {
                    break;
                };
                if sink.number == 3
                    && sink.wire == LEN
                    && last_varint(buf, sink.value, sink.end(), 6) == 0
                {
                    let mut k = sink.value;
                    while k < sink.end() {
                        let Some(vc) = wire::read_field(buf, k, sink.end()) else {
                            break;
                        };
                        if vc.number == 4 && vc.wire == LEN {
                            patch_fps_in_config(buf, vc.value, vc.end(), &mut r);
                        }
                        k = vc.end();
                    }
                }
                j = sink.end();
            }
        }
        i = svc.end();
    }
    r
}

/// One `VideoConfiguration`: `frame_rate(2)` 60 -> 30.
fn patch_fps_in_config(buf: &mut [u8], start: usize, end: usize, r: &mut PatchResult) {
    let mut i = start;
    while i < end {
        let Some(f) = wire::read_field(buf, i, end) else {
            break;
        };
        if f.number == 2 && f.wire == VARINT {
            let old = varint::read(buf.get(f.value..f.end()).unwrap_or(&[])).map_or(0, |x| x.0);
            if old == VIDEO_FPS_60 {
                if f.len == 1 && wire::patch_varint_fixed_width(buf, f.value, 1, VIDEO_FPS_30) {
                    r.patched += 1;
                } else {
                    r.refused += 1;
                }
            }
        }
        i = f.end();
    }
}

// -- display-id scan -----------------------------------------------------------

/// Return the highest `MediaSinkService` `display_id` in the response. An absent ID is
/// zero. Scan every top-level length-delimited field as a candidate service.
#[must_use]
pub fn max_display_id(data: &[u8]) -> u32 {
    let mut max = 0u32;
    for svc in walk(data, 0, data.len()).filter(|f| f.wire == LEN) {
        for sink in walk(data, svc.value, svc.end()).filter(|f| f.number == 3 && f.wire == LEN) {
            for f in walk(data, sink.value, sink.end()) {
                if f.number == 6 && f.wire == VARINT {
                    let Some((v, _)) = varint::read(data.get(f.value..f.end()).unwrap_or(&[]))
                    else {
                        break;
                    };
                    // Narrow the wire value to 32 bits with wrapping. The result feeds
                    // the highest display-index calculation. saturation would change
                    // that calculation.
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "the display identifier uses a wrapping 32-bit wire value"
                    )]
                    let v32 = v as u32;
                    if v32 > max {
                        max = v32;
                    }
                }
            }
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    // Constructed video-service fixture: four configurations,
    // primary density 213, secondary density 133, inert padding.
    // No captured vehicle or person identifier is present.
    const fn fixture_bytes() -> [u8; 217] {
        const fn nibble(byte: u8) -> u8 {
            match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("invalid fixture hex"),
            }
        }
        let hex = include_str!("../tests/fixtures/sdr-main-217B.hex").as_bytes();
        assert!(hex.len() == 435 && hex[434] == b'\n');
        let mut bytes = [0; 217];
        let mut i = 0;
        while i < bytes.len() {
            bytes[i] = (nibble(hex[i * 2]) << 4) | nibble(hex[i * 2 + 1]);
            i += 1;
        }
        bytes
    }
    const SDR_BYTES: [u8; 217] = fixture_bytes();
    const SDR: &[u8] = &SDR_BYTES;

    #[test]
    fn fixture_is_the_expected_shape() {
        assert_eq!(SDR.len(), 217);
        // display-id scan: primary display, so 0
        assert_eq!(max_display_id(SDR), 0);
    }

    #[test]
    fn patches_only_the_1280x720_density_to_160() {
        let mut buf = SDR.to_vec();
        let r = patch_main_density(&mut buf, 160);
        // Two 1280x720 configs, each with density(5)=213 -> patched. No
        // real_density(9) in this SDR, so exactly 2 fields patched, 0 refused.
        assert_eq!(r.patched, 2, "both 1280x720 density fields");
        assert_eq!(r.refused, 0);
        // length preserved (213 and 160 are both 2-byte varints)
        assert_eq!(buf.len(), 217);
        // the 800x480 density (133) must be untouched: 0x85 0x01 still present
        assert!(
            buf.windows(3).any(|w| w == [0x28, 0x85, 0x01]),
            "800x480 density 133 left alone"
        );
        // no 213 (0xd5 0x01) density left. the two became 160 (0x28 0xa0 0x01)
        assert!(
            !buf.windows(3).any(|w| w == [0x28, 0xd5, 0x01]),
            "no 1280x720 density 213 remains"
        );
        assert_eq!(
            buf.windows(3).filter(|w| *w == [0x28, 0xa0, 0x01]).count(),
            2,
            "two density fields now encode 160"
        );
    }

    #[test]
    fn same_value_is_a_noop() {
        let mut buf = SDR.to_vec();
        let r = patch_main_density(&mut buf, 213); // already 213
        assert_eq!(r.patched, 0, "no change when value already matches");
        assert_eq!(r.refused, 0);
        assert_eq!(&buf[..], SDR, "buffer byte-identical after no-op patch");
    }

    #[test]
    fn length_changing_value_is_refused_not_corrupted() {
        let mut buf = SDR.to_vec();
        // 213 is a 2-byte varint. 100 is 1-byte -> width differs -> refuse both.
        let r = patch_main_density(&mut buf, 100);
        assert_eq!(r.patched, 0);
        assert_eq!(r.refused, 2, "both fields refused for width mismatch");
        assert_eq!(&buf[..], SDR, "refused patch leaves SDR untouched");
    }

    #[test]
    fn malformed_tail_does_not_panic() {
        // Truncated copies must never panic, just stop early.
        for n in 0..SDR.len() {
            let mut buf = SDR[..n].to_vec();
            let _ = patch_main_density(&mut buf, 160);
            let _ = max_display_id(&buf);
        }
    }

    /// `frame_rate` values of every `VideoConfiguration`, in SDR order.
    fn fps_list(buf: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        for svc in walk(buf, 0, buf.len()).filter(|f| f.wire == LEN) {
            for sink in walk(buf, svc.value, svc.end()).filter(|f| f.number == 3 && f.wire == LEN) {
                for vc in
                    walk(buf, sink.value, sink.end()).filter(|f| f.number == 4 && f.wire == LEN)
                {
                    out.push(last_varint(buf, vc.value, vc.end(), 2));
                }
            }
        }
        out
    }

    #[test]
    fn fps30_rewrites_both_60_entries_and_keeps_order() {
        assert_eq!(
            fps_list(SDR),
            vec![1, 2, 1, 2],
            "fixture: 720p@60,@30, 480p@60,@30"
        );
        let mut buf = SDR.to_vec();
        let r = patch_main_fps30(&mut buf);
        assert_eq!((r.patched, r.refused), (2, 0));
        assert_eq!(buf.len(), SDR.len(), "length preserved");
        assert_eq!(fps_list(&buf), vec![2, 2, 2, 2], "no 60 fps entry offered");
        // only the two frame_rate bytes differ
        let diff = buf.iter().zip(SDR).filter(|(a, b)| a != b).count();
        assert_eq!(diff, 2);
        // idempotent
        let again = patch_main_fps30(&mut buf);
        assert_eq!((again.patched, again.refused), (0, 0));
    }

    #[test]
    fn fps30_leaves_secondary_display_alone() {
        // Service{ media_sink_service{ display_id=1, video_configs{codec_res=1, frame_rate=1} } }
        let vc = [0x08, 0x01, 0x10, 0x01];
        let mut sink = vec![0x30, 0x01, 0x22, 4];
        sink.extend_from_slice(&vc);
        let mut svc = vec![0x1A, 8];
        svc.extend_from_slice(&sink);
        let mut sdr = vec![0x0A, 10];
        sdr.extend_from_slice(&svc);
        let before = sdr.clone();
        let r = patch_main_fps30(&mut sdr);
        assert_eq!((r.patched, r.refused), (0, 0));
        assert_eq!(sdr, before);
    }

    #[test]
    fn fps30_malformed_tail_does_not_panic() {
        for n in 0..SDR.len() {
            let mut buf = SDR[..n].to_vec();
            let _ = patch_main_fps30(&mut buf);
        }
    }

    #[test]
    fn secondary_display_ids_are_found_and_left_alone() {
        // Service{ media_sink_service{ display_id=3, video_configs{codec_res=2, density=213} } }
        fn len8(v: &[u8]) -> u8 {
            u8::try_from(v.len()).unwrap()
        }
        let vc = [0x08, 0x02, 0x28, 0xd5, 0x01];
        let mut sink = vec![0x30, 0x03, 0x22, len8(&vc)];
        sink.extend_from_slice(&vc);
        let mut svc = vec![0x1A, len8(&sink)];
        svc.extend_from_slice(&sink);
        let mut sdr = vec![0x0A, len8(&svc)];
        sdr.extend_from_slice(&svc);
        assert_eq!(max_display_id(&sdr), 3);
        let before = sdr.clone();
        let r = patch_main_density(&mut sdr, 160);
        assert_eq!(
            (r.patched, r.refused),
            (0, 0),
            "display 3 is not the main display"
        );
        assert_eq!(sdr, before);
    }
}
