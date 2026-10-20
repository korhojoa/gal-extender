// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Reading fields out of a serialized message, and patching values in place
//! without moving a byte. See the crate docs for the malformed-input policy
//! every function here follows.

use crate::varint;

/// Wire type 0: base-128 varint.
pub const VARINT: u8 = 0;
/// Wire type 1: 8-byte little-endian fixed.
pub const FIXED64: u8 = 1;
/// Wire type 2: length-delimited (submessage, bytes, string, packed).
pub const LEN: u8 = 2;
/// Wire type 5: 4-byte little-endian fixed.
pub const FIXED32: u8 = 5;

/// Narrow a wire-derived `u64` (a length or an offset) to `usize`, or None if
/// it does not fit. Load-bearing on the 32-bit target -- see the crate docs.
#[must_use]
pub fn as_offset(v: u64) -> Option<usize> {
    usize::try_from(v).ok()
}

/// One field as found in a buffer: where its tag ends, where its value lives.
///
/// For `LEN` fields `value..value+len` is the payload with the length prefix
/// already consumed, so `value - tag_end` is the width of that prefix. For
/// scalar fields `len` is the encoded width of the value itself (the varint's
/// byte count, 4, or 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Field number from the tag. Never above `2^29-1` for a well-formed tag;
    /// anything wider is rejected by [`read_field`] rather than truncated.
    pub number: u32,
    /// Wire type from the tag: one of [`VARINT`], [`FIXED64`], [`LEN`],
    /// [`FIXED32`]. Groups never produce a `Field`.
    pub wire: u8,
    /// Offset just past the tag varint.
    pub tag_end: usize,
    /// Offset of the first value byte.
    pub value: usize,
    /// Byte length of the value.
    pub len: usize,
}

impl Field {
    /// Offset just past the value: where the next field's tag starts.
    /// Always in range, because [`read_field`] checked it against its bound.
    #[must_use]
    pub fn end(&self) -> usize {
        self.value.saturating_add(self.len)
    }
}

/// Read the field whose tag starts at `buf[p]`, bounded by `end`.
///
/// Returns None on: a truncated tag or length varint. a field number that does
/// not fit `u32`. a group or undefined wire type. a value that would run past
/// `end`. This is the single place "which field is this" and "how wide is it"
/// are decided, so a walker cannot disagree with itself about either.
#[must_use]
pub fn read_field(buf: &[u8], p: usize, end: usize) -> Option<Field> {
    let (tag, tag_w) = varint::read(buf.get(p..end)?)?;
    // A field number that does not fit u32 is not a protobuf tag (the encoding
    // caps field numbers at 2^29-1). treat it as corrupt rather than truncate
    // it into something that might match a wanted field.
    let number = u32::try_from(tag >> 3).ok()?;
    let wire = (tag & 7) as u8;
    let tag_end = p.checked_add(tag_w)?;
    if wire == LEN {
        let (l, lw) = varint::read(buf.get(tag_end..end)?)?;
        let value = tag_end.checked_add(lw)?;
        let len = as_offset(l)?;
        if value.checked_add(len)? > end {
            return None;
        }
        return Some(Field {
            number,
            wire,
            tag_end,
            value,
            len,
        });
    }
    let len: usize = match wire {
        VARINT => varint::read(buf.get(tag_end..end)?)?.1,
        FIXED32 => 4,
        FIXED64 => 8,
        _ => return None, // groups (3, 4) and undefined (6, 7): unsupported
    };
    if tag_end.checked_add(len)? > end {
        return None;
    }
    Some(Field {
        number,
        wire,
        tag_end,
        value: tag_end,
        len,
    })
}

/// Walk the fields of `buf[start..end]` in order, stopping at the first
/// malformed one. Each item is a [`Field`]. the iterator ends silently on
/// `None` from [`read_field`], which is the "stop the walk here" policy.
/// Callers that want "reject the whole message" instead call [`read_field`]
/// directly and propagate its `None`.
#[must_use]
pub fn fields(buf: &[u8], start: usize, end: usize) -> Fields<'_> {
    Fields { buf, p: start, end }
}

/// Iterator returned by [`fields`].
pub struct Fields<'a> {
    buf: &'a [u8],
    p: usize,
    end: usize,
}

impl Iterator for Fields<'_> {
    type Item = Field;
    fn next(&mut self) -> Option<Field> {
        if self.p >= self.end {
            return None;
        }
        let f = read_field(self.buf, self.p, self.end)?;
        self.p = f.end();
        Some(f)
    }
}

/// Find the value of the field at the end of `path`, descending through
/// length-delimited submessages for every path element but the last, which
/// must have wire type `final_wire`. Returns `(value_offset, value_len)` into
/// `buf`, or None if any element along the path is absent, truncated, or
/// malformed. At each depth the FIRST field matching both number and wire
/// type is taken.
#[must_use]
pub fn locate(buf: &[u8], path: &[u32], final_wire: u8) -> Option<(usize, usize)> {
    let mut p: usize = 0;
    let mut end: usize = buf.len();
    for (d, &want) in path.iter().enumerate() {
        let last = d + 1 == path.len();
        let want_wire = if last { final_wire } else { LEN };
        let mut found: Option<Field> = None;
        while p < end {
            let f = read_field(buf, p, end)?;
            if f.number == want && f.wire == want_wire {
                found = Some(f);
                break;
            }
            p = f.end();
        }
        let f = found?;
        if last {
            return Some((f.value, f.len));
        }
        p = f.value;
        end = f.end();
    }
    None
}

/// Overwrite the varint at `buf[off..off+width]` with `v`, using EXACTLY
/// `width` bytes (a non-canonical zero-padded varint, so the enclosing
/// lengths never move). Returns `true` on success, `false` if `v` needs more
/// than `width` bytes or the range is out of bounds -- in which case nothing
/// is written.
pub fn patch_varint_fixed_width(buf: &mut [u8], off: usize, width: usize, v: u64) -> bool {
    if width == 0 || width > 10 || varint::width(v) > width {
        return false;
    }
    let Some(slot) = buf.get_mut(off..off.saturating_add(width)) else {
        return false;
    };
    let mut rest = v;
    for (i, b) in slot.iter_mut().enumerate() {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        *b = if i + 1 == width { byte } else { byte | 0x80 };
    }
    true
}

/// Overwrite the little-endian `f32` at `buf[off..off+4]`. Returns `false`
/// (and writes nothing) if the range is out of bounds.
///
/// Deliberately NOT `slice::copy_from_slice`: that asserts (panics on
/// mismatch) that both slices have equal length at runtime, and the no-panic
/// gate checks for the PRESENCE of that panic path in the compiled artifact,
/// not whether it is provably unreachable. `zip` never compares lengths.
pub fn patch_fixed32_le(buf: &mut [u8], off: usize, f: f32) -> bool {
    let Some(slot) = buf.get_mut(off..off.saturating_add(4)) else {
        return false;
    };
    if slot.len() != 4 {
        return false;
    }
    for (dst, src) in slot.iter_mut().zip(f.to_le_bytes()) {
        *dst = src;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // field 1 varint 300 | field 2 LEN [aa bb] | field 3 fixed32 | field 4 fixed64
    const MSG: &[u8] = &[
        0x08, 0xAC, 0x02, // 1: varint 300 (2 bytes)
        0x12, 0x02, 0xAA, 0xBB, // 2: LEN 2
        0x1D, 1, 2, 3, 4, // 3: fixed32
        0x21, 1, 2, 3, 4, 5, 6, 7, 8, // 4: fixed64
    ];

    #[test]
    fn read_field_reports_every_wire_type() {
        let f = read_field(MSG, 0, MSG.len()).unwrap();
        assert_eq!(
            (f.number, f.wire, f.tag_end, f.value, f.len),
            (1, VARINT, 1, 1, 2)
        );
        let f = read_field(MSG, f.end(), MSG.len()).unwrap();
        assert_eq!(
            (f.number, f.wire, f.tag_end, f.value, f.len),
            (2, LEN, 4, 5, 2)
        );
        assert_eq!(&MSG[f.value..f.end()], &[0xAA, 0xBB]);
        let f = read_field(MSG, f.end(), MSG.len()).unwrap();
        assert_eq!((f.number, f.wire, f.len), (3, FIXED32, 4));
        let f = read_field(MSG, f.end(), MSG.len()).unwrap();
        assert_eq!((f.number, f.wire, f.len), (4, FIXED64, 8));
        assert_eq!(f.end(), MSG.len());
    }

    #[test]
    fn fields_iterates_in_order_and_stops_at_garbage() {
        let nums: Vec<u32> = fields(MSG, 0, MSG.len()).map(|f| f.number).collect();
        assert_eq!(nums, [1, 2, 3, 4]);
        // a group tag (wire 3) in the middle ends the walk there
        let bad = [0x08, 0x01, 0x13, 0x08, 0x02];
        let nums: Vec<u32> = fields(&bad, 0, bad.len()).map(|f| f.number).collect();
        assert_eq!(nums, [1]);
    }

    #[test]
    fn overruns_and_groups_are_none() {
        // LEN claims 5 bytes, only 2 follow
        assert_eq!(read_field(&[0x12, 0x05, 0xAA, 0xBB], 0, 4), None);
        // fixed32 with 3 bytes left
        assert_eq!(read_field(&[0x1D, 1, 2, 3], 0, 4), None);
        // truncated tag varint
        assert_eq!(read_field(&[0x80], 0, 1), None);
        // group start / end / undefined
        assert_eq!(read_field(&[0x0B], 0, 1), None);
        assert_eq!(read_field(&[0x0C], 0, 1), None);
        assert_eq!(read_field(&[0x0E, 0], 0, 2), None);
        assert_eq!(read_field(&[0x0F, 0], 0, 2), None);
        // bound tighter than the buffer is honoured
        assert_eq!(read_field(MSG, 0, 2), None);
        // field number wider than u32
        let mut huge = [0x80u8; 10];
        huge[9] = 0x01; // 2^63 -> number 2^60
        assert_eq!(read_field(&huge, 0, 10), None);
    }

    #[test]
    fn locate_descends_by_path() {
        // outer{ 1: inner{ 2: varint 7, 3: fixed32 } }
        let inner = [0x10, 0x07, 0x1D, 0, 0, 0x80, 0x3F]; // 7, then 1.0f
        let mut msg = vec![0x0A, u8::try_from(inner.len()).unwrap()];
        msg.extend_from_slice(&inner);
        let (off, len) = locate(&msg, &[1, 2], VARINT).unwrap();
        assert_eq!((off, len), (3, 1));
        assert_eq!(msg[off], 7);
        let (off, len) = locate(&msg, &[1, 3], FIXED32).unwrap();
        assert_eq!(len, 4);
        assert_eq!(
            f32::from_le_bytes(msg[off..off + 4].try_into().unwrap()).to_bits(),
            1.0f32.to_bits()
        );
        // wrong final wire type, absent field, absent path element
        assert_eq!(locate(&msg, &[1, 2], FIXED32), None);
        assert_eq!(locate(&msg, &[1, 9], VARINT), None);
        assert_eq!(locate(&msg, &[5, 2], VARINT), None);
        // empty path is nothing
        assert_eq!(locate(&msg, &[], VARINT), None);
    }

    #[test]
    fn fixed_width_patch_keeps_width_or_refuses() {
        let mut b = [0xFFu8; 6];
        assert!(patch_varint_fixed_width(&mut b, 1, 4, 300));
        assert_eq!(b, [0xFF, 0xAC, 0x82, 0x80, 0x00, 0xFF]);
        assert_eq!(varint::read(&b[1..]).unwrap(), (300, 4));
        // needs 5 bytes, only 4 wide: refused, untouched
        let before = b;
        assert!(!patch_varint_fixed_width(&mut b, 1, 4, 1 << 28));
        assert_eq!(b, before);
        // out of range, zero width, absurd width
        assert!(!patch_varint_fixed_width(&mut b, 4, 4, 1));
        assert!(!patch_varint_fixed_width(&mut b, 0, 0, 0));
        assert!(!patch_varint_fixed_width(&mut b, 0, 11, 0));
        // exact-fit canonical value at width 1
        assert!(patch_varint_fixed_width(&mut b, 0, 1, 0x7F));
        assert_eq!(b[0], 0x7F);
    }

    #[test]
    fn fixed32_patch_bounds() {
        let mut b = [0u8; 5];
        assert!(patch_fixed32_le(&mut b, 1, -5.5));
        assert_eq!(&b[1..], &(-5.5f32).to_le_bytes());
        assert!(!patch_fixed32_le(&mut b, 2, 1.0));
        assert!(!patch_fixed32_le(&mut b, usize::MAX, 1.0));
    }

    #[test]
    fn as_offset_is_try_from() {
        assert_eq!(as_offset(0), Some(0));
        assert_eq!(as_offset(u64::from(u32::MAX)), Some(u32::MAX as usize));
        #[cfg(target_pointer_width = "32")]
        assert_eq!(as_offset(1 << 32), None);
    }
}
