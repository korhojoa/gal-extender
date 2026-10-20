// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! `cluster_sps_crop`: rewrite the `frame_cropping` of every SPS the phone
//! sends so that the cluster decodes only the content window (the coded frame
//! minus the per-side margins). For a 1010x376 map plane, offer 1280x720
//! with total margins of 270x344. Only the SPS changes; slice data stays intact.

use h264_ts_mux::h264::{sps_set_crop, Crop, CropError};

use crate::cfg::CFG;
use crate::log::{cl_log, cl_loge};

/// The most a rewrite can add to a buffer: the four offsets and the flag of
/// one SPS (at most ~80 bits) plus emulation-prevention bytes, with room for
/// a second SPS.
pub const GROWTH: usize = 64;

/// The rewrite is on (`cluster_sps_crop=1`).
pub fn active() -> bool {
    CFG.sps_crop() != 0
}

/// The window to crop to: the content window of the configured layout.
fn window() -> Option<Crop> {
    crate::route::layout().content_window()
}

/// The NAL units of an Annex-B buffer: `(start code offset, payload start,
/// payload end)`. A 4-byte start code's leading zero belongs to the start
/// code, not to the NAL before it.
fn nals(h: &[u8], mut f: impl FnMut(usize, usize, usize)) {
    let mut prev: Option<(usize, usize)> = None;
    let mut i = 0usize;
    while i + 3 <= h.len() {
        if h[i] == 0 && h[i + 1] == 0 && h[i + 2] == 1 {
            let sc = if i > 0 && h[i - 1] == 0 { i - 1 } else { i };
            if let Some((psc, pst)) = prev {
                f(psc, pst, sc);
            }
            prev = Some((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some((psc, pst)) = prev {
        f(psc, pst, h.len());
    }
}

/// Whether the buffer holds an SPS (NAL type 7).
pub fn has_sps(h: &[u8]) -> bool {
    let mut found = false;
    nals(h, |_, st, end| {
        if st < end && h[st] & 0x1F == 7 {
            found = true;
        }
    });
    found
}

/// Copy `src` to `dst` with every SPS cropped to `win`. `Ok(None)` when
/// `src` holds no SPS (nothing written).
fn rewrite(src: &[u8], dst: &mut [u8], win: Crop) -> Result<Option<usize>, CropError> {
    if !has_sps(src) {
        return Ok(None);
    }
    let mut o = 0usize;
    let mut err = None;
    let mut first = true;
    nals(src, |sc, st, end| {
        if err.is_some() {
            return;
        }
        // bytes before the first start code, then the start code itself
        let from = if first { 0 } else { sc };
        first = false;
        let head = &src[from..st];
        let Some(slot) = dst.get_mut(o..o + head.len()) else {
            err = Some(CropError::OutTooSmall);
            return;
        };
        slot.copy_from_slice(head);
        o += head.len();
        let nal = &src[st..end];
        if nal.first().is_some_and(|b| b & 0x1F == 7) {
            match sps_set_crop(nal, win, &mut dst[o..]) {
                Ok(done) => {
                    cl_log!(
                        "[GalCluster] SPS crop: coded {}x{} -> window {}x{} at {},{} (SPS {} -> {} bytes)",
                        done.coded_w,
                        done.coded_h,
                        done.crop.w,
                        done.crop.h,
                        done.crop.x,
                        done.crop.y,
                        nal.len(),
                        done.len
                    );
                    o += done.len;
                }
                Err(e) => err = Some(e),
            }
        } else {
            match dst.get_mut(o..o + nal.len()) {
                Some(slot) => {
                    slot.copy_from_slice(nal);
                    o += nal.len();
                }
                None => err = Some(CropError::OutTooSmall),
            }
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(Some(o)),
    }
}

/// Store `src` into `dst` (a codec config or an access unit), cropping its
/// SPS when `cluster_sps_crop` is on. Any failure stores `src` unchanged and
/// logs why. Returns the bytes stored (at most `dst.len()`).
pub fn store(dst: &mut [u8], src: &[u8]) -> usize {
    if active() {
        match window() {
            Some(win) => match rewrite(src, dst, win) {
                Ok(Some(n)) => return n,
                Ok(None) => {}
                Err(e) => cl_loge!(
                    "[GalCluster] SPS crop to {}x{} at {},{} refused ({:?}) \u{2014} SPS sent unchanged",
                    win.w,
                    win.h,
                    win.x,
                    win.y,
                    e
                ),
            },
            None => cl_loge!(
                "[GalCluster] SPS crop: the margins do not fit the codec frame \u{2014} SPS sent unchanged"
            ),
        }
    }
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone's `CodecConfig` of the capture: SPS + PPS.
    const CFG_BYTES: [u8; 29] = [
        0, 0, 0, 1, 0x67, 0x42, 0x80, 0x1f, 0xda, 0x03, 0x20, 0xf6, 0x9a, 0x80, 0x80, 0x80, 0x83,
        0x68, 0x50, 0x9a, 0x80, 0, 0, 0, 1, 0x68, 0xce, 0x06, 0xf2,
    ];

    #[test]
    fn nal_walk_keeps_start_codes_out_of_payloads() {
        let mut v = Vec::new();
        nals(&CFG_BYTES, |sc, st, end| v.push((sc, st, end)));
        assert_eq!(v, vec![(0, 4, 21), (21, 25, 29)]);
        assert!(has_sps(&CFG_BYTES));
        assert!(!has_sps(&CFG_BYTES[21..]));
    }

    #[test]
    fn rewrite_crops_the_sps_and_copies_the_pps() {
        let win = Crop {
            x: 48,
            y: 60,
            w: 530,
            h: 136,
        };
        let mut out = [0u8; 128];
        let n = rewrite(&CFG_BYTES, &mut out, win).unwrap().unwrap();
        // start code, cropped SPS, then the PPS NAL byte for byte
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(out[4], 0x67);
        assert_eq!(&out[n - 8..n], &CFG_BYTES[21..]);
        assert!(n > CFG_BYTES.len() && n <= CFG_BYTES.len() + GROWTH);
        // an AU with no SPS is left to the caller
        assert_eq!(rewrite(&CFG_BYTES[21..], &mut out, win), Ok(None));
        // a window bigger than the frame is refused, not clipped
        let big = Crop {
            x: 0,
            y: 0,
            w: 1010,
            h: 376,
        };
        assert_eq!(
            rewrite(&CFG_BYTES, &mut out, big),
            Err(CropError::OutOfFrame)
        );
    }

    #[test]
    fn store_without_the_key_is_a_copy() {
        let mut out = [0u8; 64];
        assert_eq!(store(&mut out, &CFG_BYTES), 29);
        assert_eq!(&out[..29], &CFG_BYTES[..]);
    }
}
