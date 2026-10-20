// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Emitting protobuf into a caller-provided buffer. Every function appends at
//! `*n` and advances it. None means "the buffer is too small", and by then
//! `out` may be partially written -- callers discard the whole result on
//! None, which is why nothing here tries to roll back.

use crate::varint;

/// Append `bytes` at `*n`. The one building block every writer here uses, so
/// the `get_mut`/checked discipline lives in a single place.
pub fn bytes(out: &mut [u8], n: &mut usize, bytes: &[u8]) -> Option<()> {
    for &b in bytes {
        *out.get_mut(*n)? = b;
        *n = n.checked_add(1)?;
    }
    Some(())
}

/// Append a varint.
pub fn uvarint(out: &mut [u8], n: &mut usize, v: u64) -> Option<()> {
    let w = varint::write(out.get_mut(*n..)?, v)?;
    *n = n.checked_add(w)?;
    Some(())
}

/// Append a tag (`field << 3 | wire`).
pub fn tag(out: &mut [u8], n: &mut usize, field: u32, wire: u8) -> Option<()> {
    let t = (u64::from(field) << 3) | u64::from(wire & 7);
    uvarint(out, n, t)
}

/// Append a length-delimited field: tag, length, payload.
pub fn len_delimited(out: &mut [u8], n: &mut usize, field: u32, payload: &[u8]) -> Option<()> {
    tag(out, n, field, crate::wire::LEN)?;
    uvarint(out, n, payload.len() as u64)?;
    bytes(out, n, payload)
}

/// Append a varint field: tag and value.
pub fn varint_field(out: &mut [u8], n: &mut usize, field: u32, v: u64) -> Option<()> {
    tag(out, n, field, crate::wire::VARINT)?;
    uvarint(out, n, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire;

    #[test]
    fn builds_a_message_that_reads_back() {
        let mut out = [0u8; 32];
        let mut n = 0;
        varint_field(&mut out, &mut n, 1, 300).unwrap();
        len_delimited(&mut out, &mut n, 2, &[0xAA, 0xBB]).unwrap();
        tag(&mut out, &mut n, 3, wire::FIXED32).unwrap();
        bytes(&mut out, &mut n, &1.0f32.to_le_bytes()).unwrap();
        assert_eq!(
            &out[..n],
            &[0x08, 0xAC, 0x02, 0x12, 0x02, 0xAA, 0xBB, 0x1D, 0, 0, 0x80, 0x3F]
        );
        let nums: Vec<(u32, u8)> = wire::fields(&out, 0, n)
            .map(|f| (f.number, f.wire))
            .collect();
        assert_eq!(
            nums,
            [(1, wire::VARINT), (2, wire::LEN), (3, wire::FIXED32)]
        );
    }

    #[test]
    fn wide_field_numbers_take_a_wide_tag() {
        let mut out = [0u8; 8];
        let mut n = 0;
        tag(&mut out, &mut n, 26, wire::LEN).unwrap();
        assert_eq!(&out[..n], &[0xD2, 0x01]);
    }

    #[test]
    fn short_buffer_is_none() {
        let mut out = [0u8; 3];
        let mut n = 0;
        assert_eq!(len_delimited(&mut out, &mut n, 2, &[1, 2, 3]), None);
        let mut n = 0;
        assert_eq!(uvarint(&mut [0u8; 1], &mut n, 300), None);
    }
}
