// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! `SensorEventIndication` batch builders. Message 0x8003 uses sensor type numbers as
//! field numbers, including `toll_card` at field 22 and `raw_ev_trip_settings` at field 26.
//! Each builder writes the two-byte header 0x80 0x03 followed by protobuf bytes. These
//! sensor types have no dedicated report function in the declared `SensorSource`
//! interface.

use protobuf_wire::write;

/// The sensor-channel message-id frame for `SensorEventIndication` (0x8003).
pub const FRAME_SENSOR_EVENT: [u8; 2] = [0x80, 0x03];

/// `SensorEventIndication.toll_card` (sensor type 22).
const FIELD_TOLL_CARD: u32 = 22;
/// `SensorEventIndication.raw_ev_trip_settings` (sensor type 26).
const FIELD_RAW_EV_TRIP: u32 = 26;

/// Build a framed `SensorEventIndication` carrying one `TollCardData`
/// (sensor type 22): `toll_card { toll_card_present = present }`.
///
/// Wire: `80 03` | `B2 01`(field 22, wt2) `02`(len) `08`(inner field 1, wt0)
/// `<0|1>`. Writes 7 bytes into `out`. returns the length, or None if `out` is
/// too small.
pub fn toll_card(out: &mut [u8], present: bool) -> Option<usize> {
    let mut o: usize = 0;
    write::bytes(out, &mut o, &FRAME_SENSOR_EVENT)?;
    // TollCardData { toll_card_present (field 1, varint) }
    write::len_delimited(out, &mut o, FIELD_TOLL_CARD, &[0x08, u8::from(present)])?;
    Some(o)
}

/// Build a framed `SensorEventIndication` carrying one `RawEvTripSettings`
/// (sensor type 26): `raw_ev_trip_settings { raw_data = <raw> }`.
///
/// Wire: `80 03` | `D2 01`(field 26, wt2) `<outer_len>` `0A`(`raw_data` field 1,
/// wt2) `<raw_len>` `<raw>`. Returns the length, or None if `out` is too small.
/// `raw` is opaque -- its internal structure is TBD from wire captures, so the
/// crate treats it as bytes and the caller supplies the blob.
pub fn raw_ev_trip(out: &mut [u8], raw: &[u8]) -> Option<usize> {
    // inner RawEvTripSettings = 0x0A <varint raw_len> <raw>
    let inner_len = 1usize
        .checked_add(protobuf_wire::varint::width(raw.len() as u64))?
        .checked_add(raw.len())?;
    let mut o: usize = 0;
    write::bytes(out, &mut o, &FRAME_SENSOR_EVENT)?;
    write::tag(out, &mut o, FIELD_RAW_EV_TRIP, protobuf_wire::wire::LEN)?;
    write::uvarint(out, &mut o, inner_len as u64)?; // outer (field 26) length
    write::len_delimited(out, &mut o, 1, raw)?; // raw_data, field 1
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf_wire::varint;

    // Decode SensorEventIndication -> (field_number, payload) of its single
    // entry, after stripping the 0x8003 frame. Proves the wire is well-formed
    // and lands on the intended sensor-type field.
    fn one_entry(framed: &[u8]) -> (u64, Vec<u8>) {
        assert_eq!(&framed[..2], &FRAME_SENSOR_EVENT, "frame header");
        let body = &framed[2..];
        let (tag, tl) = varint::read(body).unwrap();
        let field = tag >> 3;
        assert_eq!(tag & 7, 2, "outer must be length-delimited");
        let (len, ll) = varint::read(&body[tl..]).unwrap();
        let start = tl + ll;
        let end = start + usize::try_from(len).unwrap();
        assert_eq!(end, body.len(), "no trailing bytes");
        (field, body[start..end].to_vec())
    }

    #[test]
    fn toll_card_wire() {
        let mut out = [0u8; 16];
        let n = toll_card(&mut out, true).unwrap();
        assert_eq!(&out[..n], &[0x80, 0x03, 0xB2, 0x01, 0x02, 0x08, 0x01]);
        let (field, payload) = one_entry(&out[..n]);
        assert_eq!(field, 22, "lands on toll_card");
        assert_eq!(payload, [0x08, 0x01], "TollCardData{{present=1}}");

        let n0 = toll_card(&mut out, false).unwrap();
        assert_eq!(&out[..n0], &[0x80, 0x03, 0xB2, 0x01, 0x02, 0x08, 0x00]);
    }

    #[test]
    fn raw_ev_trip_wire_short() {
        let raw = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut out = [0u8; 32];
        let n = raw_ev_trip(&mut out, &raw).unwrap();
        // 80 03 | D2 01 | 06 | 0A 04 DE AD BE EF
        assert_eq!(
            &out[..n],
            &[0x80, 0x03, 0xD2, 0x01, 0x06, 0x0A, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]
        );
        let (field, payload) = one_entry(&out[..n]);
        assert_eq!(field, 26, "lands on raw_ev_trip_settings");
        assert_eq!(payload, [0x0A, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn raw_ev_trip_wire_long_crosses_varint() {
        // 200-byte blob: both the outer and inner lengths become 2-byte varints.
        let raw = [0x5A; 200];
        let mut out = [0u8; 256];
        let n = raw_ev_trip(&mut out, &raw).unwrap();
        let (field, payload) = one_entry(&out[..n]);
        assert_eq!(field, 26);
        // payload = 0A <varint 200> <200 bytes>
        assert_eq!(payload[0], 0x0A);
        let (len, ll) = varint::read(&payload[1..]).unwrap();
        assert_eq!(len, 200);
        assert_eq!(payload.len(), 1 + ll + 200);
    }

    #[test]
    fn short_buffers_are_none_not_panic() {
        let mut tiny = [0u8; 4];
        assert_eq!(toll_card(&mut tiny, true), None);
        assert_eq!(raw_ev_trip(&mut tiny, &[1, 2, 3, 4, 5]), None);
        // empty blob still frames cleanly
        let mut out = [0u8; 16];
        let n = raw_ev_trip(&mut out, &[]).unwrap();
        assert_eq!(&out[..n], &[0x80, 0x03, 0xD2, 0x01, 0x02, 0x0A, 0x00]);
    }
}
