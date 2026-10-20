// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! `EnergyModelControl` template patching for sensor type 23. The caller supplies a
//! vehicle-specific template that starts with the field 23 length tag. Patches preserve
//! each field width and the message length. The builder copies the template, patches
//! values and prepends the two-byte channel frame. Missing fields and values that do
//! not fit remain unchanged and are reported through `PatchResult`.

use crate::batch::FRAME_SENSOR_EVENT;
use protobuf_wire::wire::{self, FIXED32, VARINT};
use protobuf_wire::write;

/// `[23, 1, 3, 1]` -- `energy_model_control.u1.battery_level.value` (varint).
pub const PATH_LEVEL: [u32; 4] = [23, 1, 3, 1];
/// `[23, 1, 4, 1]` -- `energy_model_control.u1.battery_capacity.value` (varint).
pub const PATH_CAPACITY: [u32; 4] = [23, 1, 4, 1];
/// `[23, 1, 7]` -- `energy_model_control.u1.external_temp_celsius` (fixed32 float).
pub const PATH_TEMP: [u32; 3] = [23, 1, 7];

/// Report each requested field that was found and patched. An unrequested or absent
/// field reports false. The capacity and level overflow flags distinguish an absent
/// value from one that does not fit its encoded width.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent outcome flag per patched field; the caller reads each on its own to pick a log line, so an enum per field would only add a match at every read"
)]
pub struct PatchResult {
    pub out_len: usize,
    pub capacity_patched: bool,
    pub capacity_overflow: bool,
    pub level_patched: bool,
    pub level_overflow: bool,
    pub temp_patched: bool,
}

/// Copy `template` into `out`, patch `battery_capacity_wh` (always),
/// `battery_level_wh` (only when `level_wh` is `Some`, matching "only when
/// the state of charge is known" upstream), and `external_temp_celsius` (only
/// when `external_temp_c` is `Some`), then prepend the 2-byte channel frame.
///
/// Returns `None` only for the fatal case -- `out` too small to hold the
/// framed template -- never for an individual field miss, which is reported
/// through `PatchResult` instead so the caller can log it. A value that does
/// not fit its field's existing width leaves that field untouched.
pub fn patch_energy_model(
    template: &[u8],
    out: &mut [u8],
    capacity_wh: u64,
    level_wh: Option<u64>,
    external_temp_c: Option<f32>,
) -> Option<PatchResult> {
    let mut o: usize = 0;
    write::bytes(out, &mut o, &FRAME_SENSOR_EVENT)?;
    let body_start = o;
    write::bytes(out, &mut o, template)?;
    let body = out.get_mut(body_start..o)?;

    let mut r = PatchResult {
        out_len: o,
        ..Default::default()
    };

    if let Some((off, len)) = wire::locate(body, &PATH_CAPACITY, VARINT) {
        r.capacity_patched = wire::patch_varint_fixed_width(body, off, len, capacity_wh);
        r.capacity_overflow = !r.capacity_patched;
    }

    if let Some(level) = level_wh {
        if let Some((off, len)) = wire::locate(body, &PATH_LEVEL, VARINT) {
            r.level_patched = wire::patch_varint_fixed_width(body, off, len, level);
            r.level_overflow = !r.level_patched;
        }
    }

    if let Some(temp) = external_temp_c {
        if let Some((off, _len)) = wire::locate(body, &PATH_TEMP, FIXED32) {
            r.temp_patched = wire::patch_fixed32_le(body, off, temp);
        }
    }

    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf_wire::varint;

    // Constructed EnergyModelControl layout: field23 contains field1. that contains
    // field2, field3 with level in field1, field4 with capacity in field1, and
    // floating-point density and temperature in fields6 and7.
    fn synth_template(level: u64, cap: u64) -> Vec<u8> {
        let mut inner = Vec::new();
        // field 2 (untouched physics constant), 3-byte varint wrapper: 12 03 08 <v>
        // arbitrary value, but the wrapper len must be 3
        inner.extend_from_slice(&[0x12, 0x03, 0x08, 0x99, 0x01]);
        // field 3 battery_level: 1a 05 08 <4-byte-varint level>  (wrapper len =
        // 1 inner-tag byte + 4 varint bytes = 5, NOT 4 -- a mismatch here
        // silently truncates everything parsed after it, which is exactly the
        // bug this fixture had before the tests below caught it).
        let mut lvl = [0x88u8; 4];
        assert!(wire::patch_varint_fixed_width(&mut lvl, 0, 4, level));
        inner.extend_from_slice(&[0x1a, 0x05, 0x08]);
        inner.extend_from_slice(&lvl);
        // field 4 battery_capacity: 22 05 08 <4-byte-varint cap>
        let mut c = [0x88u8; 4];
        assert!(wire::patch_varint_fixed_width(&mut c, 0, 4, cap));
        inner.extend_from_slice(&[0x22, 0x05, 0x08]);
        inner.extend_from_slice(&c);
        // field 6 reference_air_density (f32): 35 <4 bytes>
        inner.push(0x35);
        inner.extend_from_slice(&1.225f32.to_le_bytes());
        // field 7 external_temp_celsius (f32): 3d <4 bytes>
        inner.push(0x3d);
        inner.extend_from_slice(&20.0f32.to_le_bytes());

        let mut msg = Vec::new();
        // field 1 (the container), LEN
        msg.push(0x0a);
        msg.push(u8::try_from(inner.len()).unwrap());
        msg.extend_from_slice(&inner);

        let mut out = Vec::new();
        out.push(0xba);
        out.push(0x01); // field 23, LEN
        out.push(u8::try_from(msg.len()).unwrap());
        out.extend_from_slice(&msg);
        out
    }

    #[test]
    fn patches_level_and_capacity() {
        // inner varint field is fixed-width at 4 bytes (matches synth_template's
        // wrapper), so this must fit within 4*7=28 bits.
        let tmpl = synth_template(0, 0);
        let mut out = [0u8; 128];
        let r = patch_energy_model(&tmpl, &mut out, 28500, Some(16245), None).unwrap();
        assert!(r.capacity_patched, "{r:?}");
        assert!(r.level_patched, "{r:?}");
        assert!(!r.capacity_overflow);
        assert!(!r.level_overflow);
        assert_eq!(&out[..2], &FRAME_SENSOR_EVENT);

        // Read back through the SAME locate() used to write, proving the
        // round trip (this is the property that matters -- an independent
        // decoder would need a full protobuf parser to check byte-for-byte).
        let body = &out[2..r.out_len];
        let (off, len) = wire::locate(body, &PATH_CAPACITY, VARINT).unwrap();
        let (v, _) = varint::read(&body[off..off + len]).unwrap();
        assert_eq!(v, 28500);
        let (off, len) = wire::locate(body, &PATH_LEVEL, VARINT).unwrap();
        let (v, _) = varint::read(&body[off..off + len]).unwrap();
        assert_eq!(v, 16245);
    }

    #[test]
    fn level_none_leaves_level_untouched() {
        let tmpl = synth_template(999, 0);
        let mut out = [0u8; 128];
        let r = patch_energy_model(&tmpl, &mut out, 28500, None, None).unwrap();
        assert!(!r.level_patched && !r.level_overflow, "{r:?}");
        let body = &out[2..r.out_len];
        let (off, len) = wire::locate(body, &PATH_LEVEL, VARINT).unwrap();
        let (v, _) = varint::read(&body[off..off + len]).unwrap();
        assert_eq!(v, 999, "untouched when level_wh is None");
    }

    #[test]
    fn capacity_too_wide_reports_overflow_not_panic() {
        let tmpl = synth_template(0, 0);
        let mut out = [0u8; 128];
        // 4-byte fixed-width varint holds at most 2^28-1
        let r = patch_energy_model(&tmpl, &mut out, 1 << 30, None, None).unwrap();
        assert!(r.capacity_overflow, "{r:?}");
        assert!(!r.capacity_patched);
        // and the refused field is left as the template had it
        assert_eq!(
            &out[2..r.out_len],
            &tmpl[..],
            "overflow leaves the template untouched"
        );
    }

    #[test]
    fn temp_patches_fixed32() {
        let tmpl = synth_template(0, 0);
        let mut out = [0u8; 128];
        let r = patch_energy_model(&tmpl, &mut out, 0, None, Some(-5.5)).unwrap();
        assert!(r.temp_patched, "{r:?}");
        let body = &out[2..r.out_len];
        let (off, _) = wire::locate(body, &PATH_TEMP, FIXED32).unwrap();
        let f = f32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        assert!((f - (-5.5)).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_field_is_none_not_panic() {
        // A template with no field 3/4/7 at all.
        let tmpl = [0xba, 0x01, 0x02, 0x0a, 0x00]; // field23{ field1{} }
        let mut out = [0u8; 32];
        let r = patch_energy_model(&tmpl, &mut out, 100, Some(50), Some(1.0)).unwrap();
        assert!(
            !r.capacity_patched && !r.level_patched && !r.temp_patched,
            "{r:?}"
        );
    }

    #[test]
    fn out_too_small_is_none() {
        let tmpl = synth_template(0, 0);
        let mut out = [0u8; 4];
        assert_eq!(patch_energy_model(&tmpl, &mut out, 0, None, None), None);
    }

    #[test]
    fn truncated_template_is_none_not_panic() {
        let mut out = [0u8; 32];
        assert!(patch_energy_model(&[0xba], &mut out, 0, None, None).is_some());
        // 0xba alone: an incomplete tag varint -> locate() should just find
        // nothing (Some with everything unpatched), never panic.
        let r = patch_energy_model(&[0xba], &mut out, 0, None, None).unwrap();
        assert!(!r.capacity_patched && !r.level_patched && !r.temp_patched);
    }
}
