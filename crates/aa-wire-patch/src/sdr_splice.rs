// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Advertise sensors in a serialized `ServiceDiscoveryResponse`. Add missing entries to
//! the first `sensor_source_service` and update enclosing lengths. Do not duplicate
//! existing types. For the energy model, include the electric fuel type and supported
//! connector types. Reject malformed fields and wire types other than varint or length-
//! delimited. the caller then leaves the response unchanged.

use protobuf_wire::varint;
use protobuf_wire::wire::{self, Field, LEN, VARINT};
use protobuf_wire::write;

/// Upper bound on appended bytes: at most 32 extra types * 4 bytes each, plus
/// the EV block (type 23 = 4 bytes, fuel + two connectors = 6 bytes).
const APP_MAX: usize = 32 * 4 + 4 + 6;

/// Outcome of a splice.
pub struct SpliceResult {
    /// Bytes written to `out`.
    pub out_len: usize,
    /// Bitmask (1<<type) of sensor types the stock SDR already advertised.
    pub present: u32,
    /// Bitmask (1<<type) of sensor types this call appended.
    pub spliced: u32,
}

/// Accept only varint and length-delimited fields. Any other wire type refuses the
/// complete splice.
fn accepted(f: Field) -> Option<Field> {
    if f.wire == VARINT || f.wire == LEN {
        Some(f)
    } else {
        None
    }
}

/// Splice sensor advertisements into `input`, writing the result to `out`.
///
/// `extra_types` is each sensor type (0..=31) to advertise if missing;
/// `do_ev` additionally advertises the EV energy-model + fuel/connector types.
/// Returns None if `input` has no `sensor_source_service`, is malformed, or `out`
/// is too small. When nothing needs appending, `input` is copied verbatim and
/// `spliced` is 0.
pub fn splice_sensors(
    input: &[u8],
    out: &mut [u8],
    do_ev: bool,
    extra_types: &[u8],
) -> Option<SpliceResult> {
    let mut i: usize = 0;
    while i < input.len() {
        let svc = accepted(wire::read_field(input, i, input.len())?)?;
        if svc.number == 1 && svc.wire == LEN {
            // Service submessage: look for field 2 = sensor_source_service.
            let mut j = svc.value;
            while j < svc.end() {
                let f2 = accepted(wire::read_field(input, j, svc.end())?)?;
                if f2.number == 2 && f2.wire == LEN {
                    return splice_here(input, out, do_ev, extra_types, i, svc, f2);
                }
                j = f2.end();
            }
        }
        i = svc.end();
    }
    None
}

fn splice_here(
    input: &[u8],
    out: &mut [u8],
    do_ev: bool,
    extra_types: &[u8],
    svc_tag_pos: usize,
    svc: Field,
    ss: Field,
) -> Option<SpliceResult> {
    // Scan the sensor_source_service content: field 1 = sensors submsg (with
    // sensor_type as inner field 1 varint, 0x08 N), field 3 = a
    // supported_fuel_types entry.
    let mut present: u32 = 0;
    let mut have_fuel_types = false;
    let mut k = ss.value;
    while k < ss.end() {
        let f3 = accepted(wire::read_field(input, k, ss.end())?)?;
        if f3.wire == LEN && f3.number == 1 {
            if f3.len == 0 {
                present |= 1; // empty sensors msg => sensor_type 0
            } else {
                let inner = input.get(f3.value..f3.end())?;
                if inner.first() == Some(&0x08) {
                    if let Some((st, _)) = varint::read(inner.get(1..)?) {
                        if st < 32 {
                            present |= 1u32 << st;
                        }
                    }
                }
            }
        } else if f3.wire == VARINT && f3.number == 3 {
            have_fuel_types = true;
        }
        k = f3.end();
    }

    // Build the append (only what is missing).
    let mut app = [0u8; APP_MAX];
    let mut alen: usize = 0;
    let mut spliced: u32 = 0;
    for &st in extra_types {
        if st >= 32 || present & (1u32 << st) != 0 {
            continue;
        }
        write::bytes(&mut app, &mut alen, &[0x0A, 0x02, 0x08, st])?; // sensors{sensor_type}
        spliced |= 1u32 << st;
    }
    if do_ev {
        if present & (1u32 << 23) == 0 {
            write::bytes(&mut app, &mut alen, &[0x0A, 0x02, 0x08, 23])?;
            spliced |= 1u32 << 23;
        }
        if !have_fuel_types {
            // supported_fuel_types=10 (ELECTRIC), connectors=2 (MENNEKES),5 (COMBO_2)
            write::bytes(&mut app, &mut alen, &[0x18, 0x0A, 0x20, 0x02, 0x20, 0x05])?;
        }
    }

    if alen == 0 {
        // Nothing to add: copy verbatim.
        let mut o: usize = 0;
        write::bytes(out, &mut o, input)?;
        return Some(SpliceResult {
            out_len: o,
            present,
            spliced: 0,
        });
    }

    // The Service grows by the append plus any width increase of the
    // sensor_source_service length varint (e.g. 1->2 bytes when its content
    // crosses 127). Undercounting it makes the ss submsg overrun the Service
    // boundary -> parse failure.
    let ss_len_w = ss.value.checked_sub(ss.tag_end)?; // width of the old ss length prefix
    let new_ss_len = ss.len.checked_add(alen)?;
    let new_ss_w = varint::width(new_ss_len as u64);
    let svc_delta = alen.checked_add(new_ss_w)?.checked_sub(ss_len_w)?;
    let new_svc_len = svc.len.checked_add(svc_delta)?;

    let mut o: usize = 0;
    write::bytes(out, &mut o, input.get(..svc_tag_pos)?)?; // everything before the Service tag
    write::bytes(out, &mut o, input.get(svc_tag_pos..svc.tag_end)?)?; // the Service tag (0x0A)
    write::uvarint(out, &mut o, new_svc_len as u64)?; // new Service len
    write::bytes(out, &mut o, input.get(svc.value..ss.tag_end)?)?; // up to & incl the ss tag
    write::uvarint(out, &mut o, new_ss_len as u64)?; // new ss len
    write::bytes(out, &mut o, input.get(ss.value..ss.end())?)?; // original ss content
    write::bytes(out, &mut o, app.get(..alen)?)?; // appended sensors
    write::bytes(out, &mut o, input.get(ss.end()..)?)?; // tail after the ss

    Some(SpliceResult {
        out_len: o,
        present,
        spliced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf_wire::varint;
    use protobuf_wire::wire::as_offset;

    // --- tiny protobuf builders for synthetic SDRs (test-only, std) ---------
    fn put_tag(b: &mut Vec<u8>, field: u32, wt: u32) {
        let mut t = [0u8; 10];
        let n = varint::write(&mut t, u64::from((field << 3) | wt)).unwrap();
        b.extend_from_slice(&t[..n]);
    }
    fn put_varint(b: &mut Vec<u8>, v: u64) {
        let mut t = [0u8; 10];
        let n = varint::write(&mut t, v).unwrap();
        b.extend_from_slice(&t[..n]);
    }
    fn put_len(b: &mut Vec<u8>, field: u32, msg: &[u8]) {
        put_tag(b, field, 2);
        put_varint(b, msg.len() as u64);
        b.extend_from_slice(msg);
    }

    fn make_ss(present: &[u32], with_fuel: bool) -> Vec<u8> {
        let mut ss = Vec::new();
        for &t in present {
            let mut sensor = Vec::new();
            put_tag(&mut sensor, 1, 0); // sensor_type = inner field 1 varint
            put_varint(&mut sensor, u64::from(t));
            put_len(&mut ss, 1, &sensor); // sensors (field 1)
        }
        if with_fuel {
            put_tag(&mut ss, 3, 0);
            put_varint(&mut ss, 7); // some non-electric fuel type
        }
        ss
    }

    fn make_sdr(present: &[u32], with_fuel: bool) -> Vec<u8> {
        let mut sdr = Vec::new();
        // Service #1: a media placeholder (field 1 submsg with a scalar).
        let mut svc1 = Vec::new();
        let mut mss = Vec::new();
        put_tag(&mut mss, 6, 0);
        put_varint(&mut mss, 0);
        put_len(&mut svc1, 1, &mss);
        put_len(&mut sdr, 1, &svc1);
        // Service #2: sensor_source_service = field 2.
        let mut svc2 = Vec::new();
        put_len(&mut svc2, 2, &make_ss(present, with_fuel));
        put_len(&mut sdr, 1, &svc2);
        // A trailing top-level string, like a real SDR's make/model.
        put_tag(&mut sdr, 2, 2);
        put_varint(&mut sdr, 10);
        sdr.extend_from_slice(b"Volkswagen");
        sdr
    }

    // A validating walker: descends the SDR and collects sensor types found
    // under any field-2 sensor_source_service. Returns None on any overrun.
    fn collect(input: &[u8]) -> Option<u32> {
        fn walk(b: &[u8], sensors: &mut u32, in_ss: bool, depth: u32) -> Option<()> {
            let mut i = 0;
            while i < b.len() {
                let (tag, tl) = varint::read(b.get(i..)?)?;
                i += tl;
                let f = tag >> 3;
                let wt = tag & 7;
                match wt {
                    0 => {
                        let (v, vl) = varint::read(b.get(i..)?)?;
                        if in_ss && f == 1 && v < 32 {
                            *sensors |= 1u32 << v;
                        }
                        i += vl;
                    }
                    2 => {
                        let (l, ll) = varint::read(b.get(i..)?)?;
                        i += ll;
                        let end = i.checked_add(as_offset(l)?)?;
                        if end > b.len() {
                            return None;
                        }
                        let seg = b.get(i..end)?;
                        // Structure: depth0 field1 = Service. depth1 field2 = ss;
                        // inside ss, field1 = sensors submsg (has sensor_type).
                        let child_ss = (depth == 1 && f == 2) || (in_ss && f == 1);
                        // Skip the trailing string (depth0 field2).
                        let looks_msg = !(depth == 0 && f == 2);
                        if looks_msg {
                            walk(seg, sensors, child_ss, depth + 1)?;
                        }
                        i = end;
                    }
                    _ => return None,
                }
            }
            Some(())
        }
        let mut s = 0u32;
        walk(input, &mut s, false, 0)?;
        Some(s)
    }

    const EXTRA: &[u8] = &[2, 4, 11, 12, 14, 15, 16, 17, 18, 19, 20];

    fn run(present: &[u32], with_fuel: bool, do_ev: bool, do_extra: bool) -> SpliceResult {
        let sdr = make_sdr(present, with_fuel);
        // Base SDR must itself be well-formed for the test to mean anything.
        assert!(collect(&sdr).is_some(), "base SDR invalid");
        let extra: &[u8] = if do_extra { EXTRA } else { &[] };
        let mut out = [0u8; 4096];
        let r = splice_sensors(&sdr, &mut out, do_ev, extra).expect("splice failed");
        let got = collect(&out[..r.out_len]).expect("spliced SDR does not validate");
        assert_eq!(r.spliced & r.present, 0, "spliced overlaps present");
        assert_eq!(got, r.present | r.spliced, "final sensor set mismatch");
        r
    }

    #[test]
    fn empty_ss_extra_and_ev() {
        let r = run(&[], false, true, true);
        assert_eq!(r.present, 0);
        // all EXTRA + 23
        assert_eq!(
            r.spliced,
            (1 << 23) | EXTRA.iter().fold(0u32, |m, &t| m | 1 << t)
        );
    }

    #[test]
    fn dedup_against_present() {
        let r = run(&[4, 16], false, true, true);
        assert_eq!(r.present, (1 << 4) | (1 << 16));
        assert_eq!(
            r.spliced & ((1 << 4) | (1 << 16)),
            0,
            "did not re-add present"
        );
    }

    #[test]
    fn ev_already_present() {
        let r = run(&[23], true, true, true);
        assert_eq!(r.spliced & (1 << 23), 0, "did not re-add type 23");
        // fuel types already present => EV adds no fuel/connector bytes either
    }

    #[test]
    fn extra_only_and_ev_only() {
        let e = run(&[], false, false, true);
        assert_eq!(e.spliced & (1 << 23), 0, "extra-only must not add EV");
        let v = run(&[], false, true, false);
        assert_eq!(v.spliced, 1 << 23, "ev-only adds exactly type 23");
    }

    #[test]
    fn all_present_copies_verbatim() {
        let all: Vec<u32> = EXTRA.iter().copied().map(u32::from).collect();
        let sdr = make_sdr(&all, false);
        let mut out = [0u8; 4096];
        let r = splice_sensors(&sdr, &mut out, false, EXTRA).unwrap();
        assert_eq!(r.spliced, 0);
        assert_eq!(&out[..r.out_len], &sdr[..], "verbatim copy expected");
    }

    #[test]
    fn crosses_127_byte_ss_length_boundary() {
        // Pad the ss past 127 bytes so its length varint widens 1->2 bytes when
        // we append, including the length prefix.
        let present: Vec<u32> = (1..=31).collect(); // 31 sensors ~ 124 bytes of ss
        let sdr = make_sdr(&present, false);
        let mut out = [0u8; 4096];
        // add type 0 (missing: present starts at 1) to force an append
        let r = splice_sensors(&sdr, &mut out, false, &[0]).unwrap();
        assert_eq!(r.spliced, 1, "type 0 appended");
        assert!(
            collect(&out[..r.out_len]).is_some(),
            "widened SDR still parses"
        );
    }

    #[test]
    fn no_sensor_service_is_none() {
        // An SDR with only a media Service (no field-2 ss) -> None.
        let mut sdr = Vec::new();
        let mut svc = Vec::new();
        let mut mss = Vec::new();
        put_tag(&mut mss, 6, 0);
        put_varint(&mut mss, 0);
        put_len(&mut svc, 1, &mss);
        put_len(&mut sdr, 1, &svc);
        let mut out = [0u8; 256];
        assert!(splice_sensors(&sdr, &mut out, true, EXTRA).is_none());
    }

    #[test]
    fn output_buffer_too_small_is_none() {
        let sdr = make_sdr(&[], false);
        let mut tiny = [0u8; 8];
        assert!(splice_sensors(&sdr, &mut tiny, true, EXTRA).is_none());
    }

    #[test]
    fn fixed_width_field_on_the_walk_is_refused() {
        // A fixed32 field refuses the complete splice even when the field reader can
        // skip it.
        let mut sdr = make_sdr(&[], false);
        sdr.splice(0..0, [0x1D, 0, 0, 0, 0]); // field 3, fixed32, before the Services
        let mut out = [0u8; 4096];
        assert!(splice_sensors(&sdr, &mut out, true, EXTRA).is_none());
    }

    #[test]
    fn exact_bytes_of_a_known_splice() {
        // Pin the wire bytes themselves, not just the validating walker.
        // Service{ ss{ field 2 (empty) } } -- the ss holds one empty
        // length-delimited field 2, which is NOT a sensors entry.
        let sdr = [0x0A, 0x04, 0x12, 0x02, 0x12, 0x00];
        let mut out = [0u8; 64];
        let r = splice_sensors(&sdr, &mut out, false, &[22, 26]).unwrap();
        assert_eq!(
            &out[..r.out_len],
            &[
                0x0A, 0x0C, // Service, len 4 + 8
                0x12, 0x0A, // ss, len 2 + 8
                0x12, 0x00, // the original ss content
                0x0A, 0x02, 0x08, 22, // sensors{22}
                0x0A, 0x02, 0x08, 26, // sensors{26}
            ]
        );
        assert_eq!(r.present, 0);
        assert_eq!(r.spliced, (1 << 22) | (1 << 26));
    }
}
