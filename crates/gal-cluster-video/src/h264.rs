// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! H.264 Annex-B scans: IDR detection, PPS length validation and inline
//! SPS/PPS harvesting. These bounds determine which bytes are prepended.

use crate::log::{cl_log, cl_loge};

/// `is_idr_h264`: any NAL of type 5 behind a 3- or 4-byte start code.
pub fn is_idr(h: &[u8]) -> bool {
    let len = h.len();
    let mut i = 0usize;
    while i + 3 < len {
        if h[i] == 0 && h[i + 1] == 0 {
            let ns = if h[i + 2] == 1 {
                Some(i + 3)
            } else if h[i + 2] == 0 && i + 4 < len && h[i + 3] == 1 {
                Some(i + 4)
            } else {
                None
            };
            if let Some(ns) = ns {
                if ns < len && h[ns] & 0x1F == 5 {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// A start code at offset 0 (the `sc@0=` / `start_code=` checks).
pub fn starts_with_start_code(h: &[u8]) -> bool {
    h.len() >= 3
        && h[0] == 0
        && h[1] == 0
        && (h[2] == 1 || (h.len() >= 4 && h[2] == 0 && h[3] == 1))
}

/// The bit reader of `pps_trimmed_len` (bits past the end read as 0).
struct Bits<'a> {
    b: &'a [u8],
    pos: u32,
}

impl Bits<'_> {
    fn u(&mut self, bits: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..bits {
            let byte = self.b.get((self.pos >> 3) as usize).copied().unwrap_or(0);
            v = (v << 1) | ((u32::from(byte) >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        v
    }

    #[allow(clippy::cast_possible_truncation)] // a NAL is far below 512 MB
    fn ue(&mut self) -> u32 {
        let mut z = 0u32;
        while self.pos < (self.b.len() as u32).saturating_mul(8) && self.u(1) == 0 {
            z += 1;
        }
        let low = if z != 0 { self.u(z) } else { 0 };
        1u32.checked_shl(z)
            .unwrap_or(0)
            .wrapping_sub(1)
            .wrapping_add(low)
    }
}

/// `pps_trimmed_len`: the minimum byte length of a baseline PPS NAL, or 0
/// when it cannot be parsed safely.
pub fn pps_trimmed_len(nal: &[u8]) -> u32 {
    if nal.len() < 2 || nal[0] & 0x1F != 8 {
        return 0;
    }
    let mut r = Bits { b: nal, pos: 8 };
    r.ue();
    r.ue();
    r.u(1);
    r.u(1);
    if r.ue() != 0 {
        return 0;
    }
    r.ue();
    r.ue();
    r.u(1);
    r.u(2);
    r.ue();
    r.ue();
    r.ue();
    r.u(1);
    r.u(1);
    r.u(1);
    let bytelen = (r.pos + 1).div_ceil(8);
    if bytelen == 0 || bytelen as usize > nal.len() {
        return 0;
    }
    bytelen
}

/// `check_codec_config`: logs (never trims) when the last parameter-set NAL
/// is longer than a baseline PPS parses.
pub fn check_codec_config(cfg: &[u8]) {
    let len = cfg.len();
    if len < 5 {
        return;
    }
    let mut last = 0usize;
    let mut found = false;
    let mut i = 0usize;
    while i + 3 < len {
        if cfg[i] == 0 && cfg[i + 1] == 0 {
            if cfg[i + 2] == 1 {
                last = i + 3;
                found = true;
            } else if cfg[i + 2] == 0 && i + 4 <= len && cfg[i + 3] == 1 {
                last = i + 4;
                found = true;
            }
        }
        i += 1;
    }
    if !found || last >= len {
        return;
    }
    let nal_len = u32::try_from(len - last).unwrap_or(u32::MAX);
    let trimmed = pps_trimmed_len(&cfg[last..]);
    if trimmed > 0 && trimmed < nal_len {
        cl_loge!(
            "[GalCluster] codec cfg: final PPS NAL is {} bytes but parses as {} ({} trailing) \u{2014} configuration length mismatch; configuration left intact.",
            nal_len,
            trimmed,
            nal_len - trimmed
        );
    }
}

/// `maybe_harvest_sps_pps`: the SPS..PPS run of a DATA access unit, when it
/// has both, as `(start, end)`. None otherwise.
pub fn harvest_range(h: &[u8]) -> Option<(usize, usize)> {
    let len = h.len();
    let (mut sps_start, mut cut) = (0usize, 0usize);
    let mut seen_sps = false;
    let mut pps_found = false;
    let mut i = 0usize;
    while i + 3 < len {
        if h[i] != 0 || h[i + 1] != 0 {
            i += 1;
            continue;
        }
        let (sc, ns) = if h[i + 2] == 1 {
            (i, i + 3)
        } else if h[i + 2] == 0 && i + 4 < len && h[i + 3] == 1 {
            (i, i + 4)
        } else {
            i += 1;
            continue;
        };
        if ns >= len {
            break;
        }
        let t = h[ns] & 0x1F;
        if t == 7 {
            if !seen_sps {
                sps_start = sc;
            }
            seen_sps = true;
            cut = 0;
        } else if t == 8 {
            pps_found = true;
            cut = 0;
        } else if seen_sps {
            cut = sc;
            break;
        }
        // Skip the complete start code before the next scan step.
        i = ns + 1;
    }
    if !seen_sps || !pps_found {
        return None;
    }
    if cut == 0 {
        cut = len;
    }
    Some((sps_start, cut))
}

/// The `harvested inline SPS/PPS` line.
pub fn log_harvested(len: usize) {
    cl_log!(
        "[GalCluster] harvested inline SPS/PPS len={} from DATA frame",
        len
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `CodecConfig` of the unit capture (29 bytes).
    const CFG: [u8; 29] = [
        0, 0, 0, 1, 0x67, 0x42, 0x80, 0x1f, 0xda, 0x03, 0x20, 0xf6, 0x9a, 0x80, 0x80, 0x80, 0x83,
        0x68, 0x50, 0x9a, 0x80, 0, 0, 0, 1, 0x68, 0xce, 0x06, 0xf2,
    ];

    #[test]
    fn idr_detection() {
        assert!(is_idr(&[0, 0, 0, 1, 0x65, 0xb8]));
        assert!(is_idr(&[0, 0, 1, 0x65, 0xb8]));
        assert!(!is_idr(&[0, 0, 0, 1, 0x41, 0x9a]));
        assert!(!is_idr(&CFG));
        // the 4-byte form needs a byte after the 1 (i + 4 < len)
        assert!(!is_idr(&[0, 0, 0, 1]));
    }

    #[test]
    fn the_car_pps_is_not_over_read() {
        // the final PPS NAL is 4 bytes and parses as 4: no log
        assert_eq!(pps_trimmed_len(&CFG[25..]), 4);
        let mut long = CFG.to_vec();
        long.extend_from_slice(&[0xde, 0xad]);
        assert_eq!(pps_trimmed_len(&long[25..]), 4);
    }

    #[test]
    fn harvest_takes_the_parameter_sets_only() {
        let mut au = CFG.to_vec();
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        assert_eq!(harvest_range(&au), Some((0, 29)));
        assert_eq!(harvest_range(&CFG), Some((0, 29)));
        assert_eq!(harvest_range(&[0, 0, 0, 1, 0x65, 0x88]), None);
    }

    #[test]
    fn start_code_at_zero() {
        assert!(starts_with_start_code(&[0, 0, 1]));
        assert!(starts_with_start_code(&[0, 0, 0, 1]));
        assert!(!starts_with_start_code(&[0, 0, 0]));
        assert!(!starts_with_start_code(&[1, 0, 0, 1]));
    }
}
