// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Construct and read cluster protocol messages in fixed buffers. This includes video
//! and input discovery entries, access-unit channel messages and bounded field readers.
//! No allocation is required.

use h264_ts_mux::h264::Crop;

/// Write protobuf data into a bounded caller buffer. Drop any write that exceeds the
/// buffer.
pub struct Pb<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl<'a> Pb<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Pb { buf, n: 0 }
    }

    fn byte(&mut self, b: u8) {
        if let Some(s) = self.buf.get_mut(self.n) {
            *s = b;
        }
        self.n += 1;
    }

    /// `pb_varint32`.
    #[allow(clippy::cast_possible_truncation)] // the low 7 bits, by design
    pub fn varint(&mut self, mut v: u32) {
        while v >= 0x80 {
            self.byte(((v & 0x7f) | 0x80) as u8);
            v >>= 7;
        }
        self.byte(v as u8);
    }

    /// `pb_field_varint`.
    pub fn field_varint(&mut self, field: u32, value: u32) {
        self.varint(field << 3);
        self.varint(value);
    }

    /// `pb_field_embed`.
    pub fn field_embed(&mut self, field: u32, data: &[u8]) {
        self.varint((field << 3) | 2);
        self.varint(u32::try_from(data.len()).unwrap_or(u32::MAX));
        for &b in data {
            self.byte(b);
        }
    }

    /// Raw bytes (a message id).
    pub fn bytes(&mut self, data: &[u8]) {
        for &b in data {
            self.byte(b);
        }
    }

    /// Bytes written (capped at the buffer).
    pub fn len(&self) -> usize {
        self.n.min(self.buf.len())
    }
}

/// Return a varint value and consumed byte count. Refuse end-of-buffer and more than
/// five continuation bytes. Discard bits above 32.
pub fn read_varint(b: &[u8]) -> Option<(u32, usize)> {
    let mut v = 0u32;
    let mut shift = 0u32;
    for (i, &x) in b.iter().enumerate() {
        v |= u32::from(x & 0x7f).checked_shl(shift).unwrap_or(0);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
    None
}

/// `pb_skip_value`: the bytes a value of wire type `wire` takes.
pub fn skip_value(b: &[u8], wire: u32) -> Option<usize> {
    match wire {
        0 => read_varint(b).map(|(_, n)| n),
        1 => (b.len() >= 8).then_some(8),
        2 => {
            let (len, n) = read_varint(b)?;
            let end = n.checked_add(len as usize)?;
            (end <= b.len()).then_some(end)
        }
        5 => (b.len() >= 4).then_some(4),
        _ => None,
    }
}

/// Call f(field, value) for each varint field. Skip other supported wire types and stop
/// at the first malformed field.
pub fn for_each_varint(body: &[u8], mut f: impl FnMut(u32, u32)) {
    let mut p = 0usize;
    while p < body.len() {
        let Some((tag, n)) = read_varint(&body[p..]) else {
            break;
        };
        p += n;
        let (field, wire) = (tag >> 3, tag & 7);
        if wire != 0 {
            match skip_value(&body[p..], wire) {
                Some(k) => p += k,
                None => break,
            }
            continue;
        }
        let Some((val, k)) = read_varint(&body[p..]) else {
            break;
        };
        p += k;
        f(field, val);
    }
}

/// Read `AVChannelSetupRequest` codec type only when the first field is field1 with
/// varint wire type.
pub fn setup_codec_type(body: &[u8]) -> u32 {
    let Some((tag, n)) = read_varint(body) else {
        return 0;
    };
    let (field, wire) = (tag >> 3, tag & 7);
    if field == 1 && wire == 0 {
        if let Some((v, _)) = read_varint(&body[n..]) {
            return v;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Messages the cluster sends
// ---------------------------------------------------------------------------

/// `AA_AV_MSG_*` ids used on the cluster video channel.
pub const AV_MEDIA_DATA: u16 = 0x0000;
pub const AV_CODEC_CONFIG: u16 = 0x0001;
pub const AV_SETUP_REQUEST: u16 = 0x8000;
pub const AV_START_INDICATION: u16 = 0x8001;
pub const AV_STOP_INDICATION: u16 = 0x8002;
pub const AV_SETUP_RESPONSE: u16 = 0x8003;
pub const AV_MEDIA_ACK: u16 = 0x8004;
pub const AV_VIDEO_FOCUS_REQUEST: u16 = 0x8007;
pub const AV_VIDEO_FOCUS_NOTIFICATION: u16 = 0x8008;
pub const AV_UPDATE_UI_CONFIG_OUT: u16 = 0x8009;
pub const AV_UPDATE_UI_CONFIG_IN: u16 = 0x800A;
/// This protocol value is unverified.
pub const AV_UPDATE_UI_CONFIG_REPLY: u16 = 0x800B;
/// `InstrumentClusterStart`, on the NAVIGATION channel.
pub const NAV_INSTRUMENT_CLUSTER_START: u16 = 0x8001;

pub const VFN_FIELD_FOCUS: u32 = 1;
pub const VFN_FIELD_UNSOLICITED: u32 = 2;
pub const VFR_FIELD_MODE: u32 = 2;
pub const VFR_FIELD_REASON: u32 = 3;

pub const FOCUS_PROJECTED: u32 = 1;
pub const FOCUS_NATIVE: u32 = 2;
pub const FOCUS_REASON_UNKNOWN: u32 = 0;
pub const VFN_UNSOLICITED: u32 = 1;
pub const VFN_SOLICITED: u32 = 0;

/// `VideoFocusNotification` (0x8008): `msg[10]`. returns the length.
pub fn video_focus_notification(out: &mut [u8; 10], focus: u32, unsolicited: u32) -> usize {
    let mut p = Pb::new(out);
    p.bytes(&AV_VIDEO_FOCUS_NOTIFICATION.to_be_bytes());
    p.field_varint(VFN_FIELD_FOCUS, focus);
    p.field_varint(VFN_FIELD_UNSOLICITED, unsolicited);
    p.len()
}

/// `AVChannelSetupResponse` (0x8003): status READY, `max_unacked`, config
/// index 0. Returns the length.
pub fn setup_response(out: &mut [u8; 24], max_unacked: u32) -> usize {
    let mut body = [0u8; 16];
    let mut b = Pb::new(&mut body);
    b.field_varint(1, 2);
    b.field_varint(2, max_unacked);
    b.field_varint(3, 0);
    let n = b.len();
    let mut p = Pb::new(out);
    p.bytes(&AV_SETUP_RESPONSE.to_be_bytes());
    p.bytes(&body[..n]);
    p.len()
}

/// `AVMediaAckIndication` (0x8004): session id, ack count 1. Returns the
/// length.
#[allow(clippy::cast_sign_loss)] // the session identifier uses an unsigned 32-bit wire value
pub fn media_ack(out: &mut [u8; 16], session_id: i32) -> usize {
    let mut p = Pb::new(out);
    p.bytes(&AV_MEDIA_ACK.to_be_bytes());
    p.field_varint(1, session_id as u32);
    p.field_varint(2, 1);
    p.len()
}

// ---------------------------------------------------------------------------
// The cluster ServiceDiscoveryResponse pieces
// ---------------------------------------------------------------------------

/// The layout values the SDR pieces are built from (`gal_loader.conf`).
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// `cluster_codec_resolution`: the AA `VideoCodecResolutionType` offered
    /// (1 = 800x480, 2 = 1280x720).
    pub codec_res: u32,
    pub width_margin: u32,
    pub height_margin: u32,
    /// `cluster_margin_*` (-1 = half of the width/height margin).
    pub margin_top: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    pub margin_right: i32,
    pub content_insets: [u32; 4],
    pub stable_insets: [u32; 4],
    pub viewing_dist: u32,
}

/// `build_insets`: all zero is an empty message.
fn insets(out: &mut [u8; 24], v: [u32; 4]) -> usize {
    let mut p = Pb::new(out);
    if v.iter().any(|&x| x != 0) {
        for (i, &x) in (1u32..).zip(v.iter()) {
            p.field_varint(i, x);
        }
    }
    p.len()
}

#[allow(clippy::cast_sign_loss)]
fn side(m: i32, half_of: u32) -> u32 {
    if m >= 0 {
        m as u32
    } else {
        half_of >> 1
    }
}

/// The frame size of an AA `VideoCodecResolutionType` this sink offers.
pub fn codec_size(res: u32) -> (u32, u32) {
    match res {
        2 => (1280, 720),
        _ => (800, 480),
    }
}

impl Layout {
    /// The margin per side, top/bottom/left/right (`UiConfig.margins`).
    pub fn sides(&self) -> [u32; 4] {
        [
            side(self.margin_top, self.height_margin),
            side(self.margin_bottom, self.height_margin),
            side(self.margin_left, self.width_margin),
            side(self.margin_right, self.width_margin),
        ]
    }

    /// The content window inside the coded frame: the frame minus the
    /// per-side margins. None when the margins do not fit the frame.
    pub fn content_window(&self) -> Option<Crop> {
        let (width, height) = codec_size(self.codec_res);
        let [top, bottom, left, right] = self.sides();
        Some(Crop {
            x: left,
            y: top,
            w: width.checked_sub(left.checked_add(right)?)?,
            h: height.checked_sub(top.checked_add(bottom)?)?,
        })
    }
}

/// `build_video_config` into `out[128]`.
fn video_config(
    out: &mut [u8; 128],
    layout: &Layout,
    res: u32,
    fps: u32,
    dpi: u32,
    par: u32,
) -> usize {
    let mut ui = [0u8; 96];
    let mut uw = Pb::new(&mut ui);
    let mut margins = [0u8; 24];
    let len = insets(&mut margins, layout.sides());
    uw.field_embed(1, &margins[..len]);
    let mut inset = [0u8; 24];
    let len = insets(&mut inset, layout.content_insets);
    uw.field_embed(2, &inset[..len]);
    let len = insets(&mut inset, layout.stable_insets);
    uw.field_embed(3, &inset[..len]);
    uw.field_varint(4, 0);
    let ui_len = uw.len();

    let mut w = Pb::new(out);
    w.field_varint(1, res);
    w.field_varint(2, fps);
    w.field_varint(3, layout.width_margin);
    w.field_varint(4, layout.height_margin);
    w.field_varint(5, dpi);
    w.field_varint(6, 0);
    w.field_varint(7, layout.viewing_dist);
    w.field_varint(8, par);
    w.field_varint(9, dpi);
    w.field_varint(10, 3);
    w.field_embed(11, &ui[..ui_len]);
    w.len()
}

/// `build_cluster_sdr_bytes`: `SDR { services: Service { id, media_sink {
/// H264_BP, one l.codec_res@30 config, display_id, CLUSTER } } }` into
/// `out[512]`. Returns the length.
pub fn cluster_video_sdr(
    out: &mut [u8; 512],
    l: &Layout,
    service_id: u32,
    display_id: u32,
    dpi: u32,
    par: u32,
) -> usize {
    let mut vc = [0u8; 128];
    let vn = video_config(&mut vc, l, l.codec_res, 2, dpi, par);
    let mut mss = [0u8; 256];
    let mut m = Pb::new(&mut mss);
    m.field_varint(1, 3);
    m.field_embed(4, &vc[..vn]);
    m.field_varint(6, display_id);
    m.field_varint(7, 1);
    let mn = m.len();
    let mut svc = [0u8; 512];
    let mut s = Pb::new(&mut svc);
    s.field_varint(1, service_id);
    s.field_embed(3, &mss[..mn]);
    let sn = s.len();
    let mut p = Pb::new(out);
    p.field_embed(1, &svc[..sn]);
    p.len()
}

/// `build_cluster_input_sdr_bytes`: `SDR { channels: { id 1, input {
/// touch (w, h) CLUSTER, display_id } } }` into `out[64]`. Returns the
/// length. The caller patches byte 3 (the channel id) afterwards.
pub fn cluster_input_sdr(out: &mut [u8; 64], display_id: u32, touch: (u32, u32)) -> usize {
    let (touch_w, touch_h) = touch;
    let mut touch = [0u8; 16];
    let mut tw = Pb::new(&mut touch);
    tw.field_varint(1, touch_w);
    tw.field_varint(2, touch_h);
    tw.field_varint(3, 1);
    let touch_len = tw.len();
    let mut input = [0u8; 32];
    let mut iw = Pb::new(&mut input);
    iw.field_embed(2, &touch[..touch_len]);
    iw.field_varint(5, display_id);
    let input_len = iw.len();
    let mut chan = [0u8; 64];
    let mut cw = Pb::new(&mut chan);
    cw.field_varint(1, 1);
    cw.field_embed(4, &input[..input_len]);
    let chan_len = cw.len();
    let mut out_w = Pb::new(out);
    out_w.field_embed(1, &chan[..chan_len]);
    out_w.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default layout: margins 270x344, top/bottom/left/right
    /// 60/48/32/32, density 133, viewing distance 550, content insets
    /// 0/0/80/80, stable zero.
    fn car_layout() -> Layout {
        Layout {
            codec_res: 1,
            width_margin: 270,
            height_margin: 344,
            margin_top: 60,
            margin_bottom: 48,
            margin_left: 32,
            margin_right: 32,
            content_insets: [0, 0, 80, 80],
            stable_insets: [0; 4],
            viewing_dist: 550,
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect()
    }

    /// Constructed discovery layout with channel6, `display_id1`, density133 and pixel
    /// aspect ratio10000.
    #[test]
    fn video_sdr_is_the_logged_bytes() {
        let want = hex(
            "0a 40 08 06 1a 3c 08 03 22 34 08 01 10 02 18 8e 02 20 d8 02 28 85 01 30 00 38 a6 04 \
             40 90 4e 48 85 01 50 03 5a 18 0a 08 08 3c 10 30 18 20 20 20 \
             12 08 08 00 10 00 18 50 20 50 1a 00 20 00 30 01 38 01",
        );
        let mut out = [0u8; 512];
        let n = cluster_video_sdr(&mut out, &car_layout(), 6, 1, 133, 10000);
        assert_eq!(n, 66);
        assert_eq!(&out[..n], &want[..]);
    }

    /// `input_sdr (18 bytes, ch=7 display_id=1)` of the same capture, after
    /// the channel-id patch.
    #[test]
    fn input_sdr_is_the_logged_bytes() {
        let want = hex("0a 10 08 07 22 0c 12 08 08 a0 06 10 e0 03 18 01 28 01");
        let mut out = [0u8; 64];
        let n = cluster_input_sdr(&mut out, 1, (800, 480));
        assert_eq!((n, out[2]), (18, 0x08));
        out[3] = 7;
        assert_eq!(&out[..n], &want[..]);
    }

    /// A cropped map layout: 1280x720 with even margins 270x344
    /// leaves the 1010x376 kombi map plane. `codec_res` 2 is in field 1.
    #[test]
    fn cropped_layout_is_the_1010x376_window() {
        let l = Layout {
            codec_res: 2,
            margin_top: -1,
            margin_bottom: -1,
            margin_left: -1,
            margin_right: -1,
            ..car_layout()
        };
        assert_eq!(
            l.content_window(),
            Some(Crop {
                x: 135,
                y: 172,
                w: 1010,
                h: 376
            })
        );
        let mut out = [0u8; 512];
        let n = cluster_video_sdr(&mut out, &l, 6, 1, 133, 10000);
        // ... 22 len { 08 02 (codec_res 2) 10 02 (30 fps) ...
        let vc = out[..n]
            .windows(4)
            .position(|w| w == [0x08, 0x02, 0x10, 0x02]);
        assert!(vc.is_some(), "codec_res 2 not in the SDR");
        // Example content area: 800 - 32 - 32 by 480 - 60 - 48
        assert_eq!(
            car_layout().content_window().map(|c| (c.w, c.h)),
            Some((736, 372))
        );
        let mut wide = car_layout();
        wide.margin_left = 900;
        assert_eq!(wide.content_window(), None);
    }

    /// The defaults (no conf): centered margins via -1 and the default
    /// content insets 0/0/64/64.
    #[test]
    fn auto_margins_split_evenly() {
        let mut l = car_layout();
        l.margin_top = -1;
        l.margin_left = -1;
        let mut out = [0u8; 128];
        let n = video_config(&mut out, &l, 1, 2, 180, 10000);
        let ui_start = out[..n].iter().rposition(|&b| b == 0x5a).unwrap();
        // margins Insets: top 344/2=172, bottom 48, left 270/2=135, right 32
        assert_eq!(
            &out[ui_start + 2..ui_start + 2 + 12],
            &[0x0a, 0x0a, 0x08, 0xac, 0x01, 0x10, 0x30, 0x18, 0x87, 0x01, 0x20, 0x20]
        );
    }

    /// `SETUP_RESPONSE bytes[8]: 80 03 08 02 10 08 18 00` (`max_unacked=8`).
    #[test]
    fn av_messages() {
        let mut m = [0u8; 24];
        let n = setup_response(&mut m, 8);
        assert_eq!(&m[..n], &[0x80, 0x03, 0x08, 0x02, 0x10, 0x08, 0x18, 0x00]);
        let mut v = [0u8; 10];
        let n = video_focus_notification(&mut v, FOCUS_PROJECTED, VFN_UNSOLICITED);
        assert_eq!(&v[..n], &[0x80, 0x08, 0x08, 0x01, 0x10, 0x01]);
        let mut a = [0u8; 16];
        let n = media_ack(&mut a, 0);
        assert_eq!(&a[..n], &[0x80, 0x04, 0x08, 0x00, 0x10, 0x01]);
        let n = media_ack(&mut a, -1);
        assert_eq!(
            &a[..n],
            &[0x80, 0x04, 0x08, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x10, 0x01]
        );
    }

    #[test]
    fn readers_match_pb_read_varint() {
        assert_eq!(read_varint(&[0x96, 0x01]), Some((150, 2)));
        assert_eq!(read_varint(&[0x80]), None);
        assert_eq!(read_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01]), None);
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x7f]),
            Some((u32::MAX, 5))
        );
        assert_eq!(setup_codec_type(&[0x08, 0x03]), 3);
        assert_eq!(setup_codec_type(&[0x10, 0x03]), 0);
        let mut seen = Vec::new();
        // field 1 = 5, a length-delimited field 3 skipped, field 2 = 7
        for_each_varint(&[0x08, 0x05, 0x1a, 0x02, 0xaa, 0xbb, 0x10, 0x07], |f, v| {
            seen.push((f, v));
        });
        assert_eq!(seen, [(1, 5), (2, 7)]);
    }
}
