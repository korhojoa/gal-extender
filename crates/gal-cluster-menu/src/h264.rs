// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! A minimal H.264 baseline encoder that writes only `I_PCM` macroblocks
//!: raw 8-bit samples, no prediction, no transform, no
//! entropy coding of residuals, so the "encoder" is the bitstream framing.
//!
//! One call writes `[SPS][PPS][IDR]` in Annex-B form (start codes,
//! emulation prevention) for a full 4:2:0 frame whose width and height are
//! multiples of 16. Decoder support for this stream must be checked on the target.

/// MSB-first bit writer over a byte buffer (an RBSP before emulation
/// prevention), for the SPS and the PPS.
struct BitW<'a> {
    buf: &'a mut [u8],
    byte: usize,
    bit: u32,
    ovf: bool,
}

impl<'a> BitW<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        if let Some(b) = buf.first_mut() {
            *b = 0;
        }
        BitW {
            buf,
            byte: 0,
            bit: 0,
            ovf: false,
        }
    }

    fn put_bit(&mut self, v: bool) {
        if self.byte >= self.buf.len() {
            self.ovf = true;
            return;
        }
        if v {
            self.buf[self.byte] |= 1 << (7 - self.bit);
        }
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
            if self.byte < self.buf.len() {
                self.buf[self.byte] = 0;
            }
        }
    }

    fn put_bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((v >> i) & 1 != 0);
        }
    }

    /// Exp-Golomb unsigned.
    fn ue(&mut self, v: u32) {
        let x = v + 1;
        let nbits = 32 - x.leading_zeros();
        self.put_bits(0, nbits - 1);
        self.put_bits(x, nbits);
    }

    /// Exp-Golomb signed.
    fn se(&mut self, v: i32) {
        self.ue(se_code(v));
    }

    fn align_zero(&mut self) {
        while self.bit != 0 {
            self.put_bit(false);
        }
    }

    /// `rbsp_trailing_bits`.
    fn trailing(&mut self) {
        self.put_bit(true);
        self.align_zero();
    }

    fn size_bytes(&self) -> usize {
        self.byte + usize::from(self.bit != 0)
    }
}

/// The `se(v)` code number.
fn se_code(v: i32) -> u32 {
    if v <= 0 {
        v.unsigned_abs() * 2
    } else {
        v.unsigned_abs() * 2 - 1
    }
}

/// `emit_nal`: `00 00 00 01`, the header byte and the RBSP with emulation
/// prevention (a `03` after any `00 00` that is followed by a byte <= 3).
/// The new end. `ovf` set when `out` is full.
fn emit_nal(out: &mut [u8], mut pos: usize, hdr: u8, rbsp: &[u8], ovf: &mut bool) -> usize {
    for b in [0, 0, 0, 1, hdr] {
        if pos >= out.len() {
            *ovf = true;
            return pos;
        }
        out[pos] = b;
        pos += 1;
    }
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            if pos >= out.len() {
                *ovf = true;
                return pos;
            }
            out[pos] = 0x03;
            pos += 1;
            zeros = 0;
        }
        if pos >= out.len() {
            *ovf = true;
            return pos;
        }
        out[pos] = b;
        pos += 1;
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    pos
}

/// The slice writer: bits staged per byte, each byte through emulation
/// prevention straight into `out`.
struct SliceOut<'a> {
    out: &'a mut [u8],
    pos: usize,
    zeros: u32,
    ovf: bool,
    acc: u32,
    accn: u32,
}

impl SliceOut<'_> {
    fn emit(&mut self, b: u8) {
        if self.pos >= self.out.len() {
            self.ovf = true;
            return;
        }
        self.out[self.pos] = b;
        self.pos += 1;
    }

    fn raw_byte(&mut self, b: u8) {
        if self.zeros >= 2 && b <= 3 {
            self.emit(0x03);
            self.zeros = 0;
        }
        self.emit(b);
        if b == 0 {
            self.zeros += 1;
        } else {
            self.zeros = 0;
        }
    }

    fn bit(&mut self, v: u32) {
        self.acc = (self.acc << 1) | (v & 1);
        self.accn += 1;
        if self.accn == 8 {
            let [b, ..] = self.acc.to_le_bytes();
            self.raw_byte(b);
            self.acc = 0;
            self.accn = 0;
        }
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = v + 1;
        let nb = 32 - x.leading_zeros();
        self.bits(0, nb - 1);
        self.bits(x, nb);
    }

    fn se(&mut self, v: i32) {
        self.ue(se_code(v));
    }

    fn align0(&mut self) {
        while self.accn != 0 {
            self.bit(0);
        }
    }

    fn trailing(&mut self) {
        self.bit(1);
        self.align0();
    }
}

/// The SPS RBSP (before emulation prevention) for `mb_cols` x `mb_rows`
/// macroblocks into `rbsp`: its length, or None when it does not fit.
fn sps(rbsp: &mut [u8], mb_cols: u32, mb_rows: u32) -> Option<usize> {
    let mut s = BitW::new(rbsp);
    s.put_bits(66, 8); // profile_idc: baseline
    s.put_bits(0, 8); // constraint flags + reserved
    s.put_bits(30, 8); // level_idc 3.0
    s.ue(0); // seq_parameter_set_id
    s.ue(0); // log2_max_frame_num_minus4
    s.ue(2); // pic_order_cnt_type 2
    s.ue(1); // max_num_ref_frames
    s.put_bit(false); // gaps_in_frame_num_allowed
    s.ue(mb_cols - 1); // pic_width_in_mbs_minus1
    s.ue(mb_rows - 1); // pic_height_in_map_units_minus1
    s.put_bit(true); // frame_mbs_only_flag
    s.put_bit(false); // direct_8x8_inference_flag
    s.put_bit(false); // frame_cropping_flag
    s.put_bit(false); // vui_parameters_present_flag
    s.trailing();
    (!s.ovf).then(|| s.size_bytes())
}

/// The PPS RBSP into `rbsp`: its length, or None when it does not fit.
fn pps(rbsp: &mut [u8]) -> Option<usize> {
    let mut s = BitW::new(rbsp);
    s.ue(0); // pic_parameter_set_id
    s.ue(0); // seq_parameter_set_id
    s.put_bit(false); // entropy_coding_mode_flag: CAVLC
    s.put_bit(false); // bottom_field_pic_order_present
    s.ue(0); // num_slice_groups_minus1
    s.ue(0); // num_ref_idx_l0_default_active_minus1
    s.ue(0); // num_ref_idx_l1_default_active_minus1
    s.put_bit(false); // weighted_pred_flag
    s.put_bits(0, 2); // weighted_bipred_idc
    s.se(0); // pic_init_qp_minus26
    s.se(0); // pic_init_qs_minus26
    s.se(0); // chroma_qp_index_offset
    s.put_bit(false); // deblocking_filter_control_present
    s.put_bit(false); // constrained_intra_pred_flag
    s.put_bit(false); // redundant_pic_cnt_present_flag
    s.trailing();
    (!s.ovf).then(|| s.size_bytes())
}

/// `h264_ipcm_encode_idr`: one IDR picture from planar 4:2:0 (`luma`:
/// width * height, `cb` / `cr`: (width / 2) * (height / 2)) into `out`. The
/// bytes written, or 0 (bad size, planes too small, or `out` too small).
#[must_use]
pub fn encode_idr(
    out: &mut [u8],
    luma: &[u8],
    cb: &[u8],
    cr: &[u8],
    width: i32,
    height: i32,
) -> usize {
    if width <= 0 || height <= 0 || (width & 15) != 0 || (height & 15) != 0 {
        return 0;
    }
    let (Ok(wu), Ok(hu)) = (usize::try_from(width), usize::try_from(height)) else {
        return 0;
    };
    let (mbw, mbh) = (wu >> 4, hu >> 4);
    let cw = wu >> 1;
    if luma.len() < wu * hu || cb.len() < cw * (hu >> 1) || cr.len() < cw * (hu >> 1) {
        return 0;
    }
    let (Ok(mb_cols), Ok(mb_rows)) = (u32::try_from(mbw), u32::try_from(mbh)) else {
        return 0;
    };

    let mut rbsp = [0u8; 512];
    let mut ovf = false;
    // SPS (nal_ref_idc 3, type 7), PPS (nal_ref_idc 3, type 8).
    let Some(n) = sps(&mut rbsp, mb_cols, mb_rows) else {
        return 0;
    };
    let pos = emit_nal(out, 0, 0x67, &rbsp[..n], &mut ovf);
    let Some(n) = pps(&mut rbsp) else {
        return 0;
    };
    let mut pos = emit_nal(out, pos, 0x68, &rbsp[..n], &mut ovf);
    let _ = ovf;

    // IDR slice (nal_ref_idc 3, type 5): start code and header written
    // directly, then the RBSP streamed through the inline emulation
    // prevention (the PCM body is w*h*1.5 bytes).
    for b in [0u8, 0, 0, 1, 0x65] {
        if pos >= out.len() {
            return 0;
        }
        out[pos] = b;
        pos += 1;
    }
    let mut so = SliceOut {
        out,
        pos,
        zeros: 0,
        ovf: false,
        acc: 0,
        accn: 0,
    };
    // slice_header
    so.ue(0); // first_mb_in_slice
    so.ue(7); // slice_type I
    so.ue(0); // pps id
    so.bits(0, 4); // frame_num
    so.ue(0); // idr_pic_id
    so.bit(0); // no_output_of_prior_pics
    so.bit(0); // long_term_reference_flag
    so.se(0); // slice_qp_delta

    // slice_data: every macroblock I_PCM.
    for my in 0..mbh {
        for mx in 0..mbw {
            so.ue(25); // mb_type I_PCM
            so.align0(); // pcm_alignment_zero_bit
            let yo = my * 16 * wu + mx * 16;
            for r in 0..16 {
                for c in 0..16 {
                    so.raw_byte(luma[yo + r * wu + c]);
                }
            }
            let co = my * 8 * cw + mx * 8;
            for plane in [cb, cr] {
                for r in 0..8 {
                    for c in 0..8 {
                        so.raw_byte(plane[co + r * cw + c]);
                    }
                }
            }
        }
    }
    so.trailing();
    if so.ovf {
        return 0;
    }
    so.pos
}

/// A value clamped to 0..=255.
fn sample(v: i32) -> u8 {
    u8::try_from(v.clamp(0, 255)).unwrap_or(u8::MAX)
}

/// The BT.601 limited-range luma of one RGB pixel.
fn luma_of(px: [u8; 4]) -> u8 {
    let (red, green, blue) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
    sample((66 * red + 129 * green + 25 * blue + 128) / 256 + 16)
}

/// `h264_ipcm_encode_rgba`: RGBA (4 bytes a pixel) to BT.601 limited-range
/// 4:2:0 in `yuv` (`w*h + 2*(w/2)*(h/2)` bytes or more. chroma from the 2x2
/// average), then [`encode_idr`]. 0 on a bad size or a short buffer.
#[must_use]
pub fn encode_rgba(out: &mut [u8], rgba: &[u8], width: i32, height: i32, yuv: &mut [u8]) -> usize {
    if width <= 0 || height <= 0 || (width & 15) != 0 || (height & 15) != 0 {
        return 0;
    }
    let (Ok(wu), Ok(hu)) = (usize::try_from(width), usize::try_from(height)) else {
        return 0;
    };
    let (cw, chh) = (wu >> 1, hu >> 1);
    let (ysz, csz) = (wu * hu, cw * chh);
    if yuv.len() < ysz + 2 * csz || rgba.len() < ysz * 4 {
        return 0;
    }
    let (y_plane, rest) = yuv.split_at_mut(ysz);
    let (blue_diff, rest) = rest.split_at_mut(csz);
    let red_diff = &mut rest[..csz];

    for (dst, px) in y_plane.iter_mut().zip(rgba.as_chunks::<4>().0) {
        *dst = luma_of(*px);
    }
    for row in 0..chh {
        for col in 0..cw {
            let (mut red, mut green, mut blue) = (0i32, 0i32, 0i32);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let at = ((row * 2 + dy) * wu + col * 2 + dx) * 4;
                red += i32::from(rgba[at]);
                green += i32::from(rgba[at + 1]);
                blue += i32::from(rgba[at + 2]);
            }
            red >>= 2;
            green >>= 2;
            blue >>= 2;
            // Signed division truncates toward zero.
            let cb = (-38 * red - 74 * green + 112 * blue + 128) / 256 + 128;
            let cr = (112 * red - 94 * green - 18 * blue + 128) / 256 + 128;
            blue_diff[row * cw + col] = sample(cb);
            red_diff[row * cw + col] = sample(cr);
        }
    }
    encode_idr(out, y_plane, blue_diff, red_diff, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_of_a_16x16_black_frame() {
        let rgba = [0u8, 0, 0, 255].repeat(256);
        let mut yuv = vec![0u8; 256 + 128];
        let mut out = vec![0u8; 2048];
        let n = encode_rgba(&mut out, &rgba, 16, 16, &mut yuv);
        assert!(n > 384);
        assert_eq!(&out[..5], &[0, 0, 0, 1, 0x67]);
        assert_eq!(&out[5..8], &[66, 0, 30]);
        // Y of black is 16, chroma 128.
        assert_eq!(yuv[0], 16);
        assert_eq!(yuv[256], 128);
        assert_eq!(encode_rgba(&mut out, &rgba, 15, 16, &mut yuv), 0);
        assert_eq!(encode_rgba(&mut out[..40], &rgba, 16, 16, &mut yuv), 0);
    }

    #[test]
    fn emulation_prevention() {
        let mut out = [0u8; 16];
        let mut ovf = false;
        let n = emit_nal(&mut out, 0, 0x67, &[0, 0, 1, 0, 0, 0], &mut ovf);
        assert_eq!(&out[..n], &[0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0]);
        assert!(!ovf);
        assert_eq!(se_code(0), 0);
        assert_eq!(se_code(1), 1);
        assert_eq!(se_code(-1), 2);
    }
}
