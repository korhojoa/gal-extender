// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Base-128 varint read/write/width, the protobuf wire primitive. No panics:
//! `read` returns None on truncation, `write` None if the buffer is too small.

/// Read a varint from the front of `buf`. Returns `(value, bytes_consumed)`,
/// or None on truncation / a varint wider than 10 bytes (malformed).
#[must_use]
pub fn read(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    let mut i: usize = 0;
    while i < 10 {
        let b = *buf.get(i)?;
        v |= u64::from(b & 0x7f) << shift; // shift <= 63 here (i <= 9)
        i += 1;
        if b & 0x80 == 0 {
            return Some((v, i));
        }
        shift += 7;
    }
    None
}

/// Write `v` as a canonical varint into the front of `out`. Returns the byte
/// count, or None if `out` cannot hold it.
pub fn write(out: &mut [u8], mut v: u64) -> Option<usize> {
    let mut i: usize = 0;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        *out.get_mut(i)? = if v != 0 { byte | 0x80 } else { byte };
        i += 1;
        if v == 0 {
            return Some(i);
        }
    }
}

/// Byte width a canonical varint of `v` would occupy (1..=10).
#[must_use]
pub fn width(mut v: u64) -> usize {
    let mut n: usize = 1;
    v >>= 7;
    while v != 0 {
        n += 1;
        v >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_width() {
        for &v in &[0u64, 1, 127, 128, 255, 300, 16383, 16384, 1 << 35, u64::MAX] {
            let mut buf = [0u8; 10];
            let n = write(&mut buf, v).unwrap();
            assert_eq!(n, width(v), "width disagrees with write for {v}");
            let (got, consumed) = read(&buf).unwrap();
            assert_eq!(got, v);
            assert_eq!(consumed, n);
        }
    }

    #[test]
    fn truncation_is_none_not_panic() {
        assert_eq!(read(&[]), None);
        assert_eq!(read(&[0x80]), None); // continuation bit set, no next byte
                                         // 11 continuation bytes: wider than a u64 varint can be -> None
        assert_eq!(read(&[0x80; 11]), None);
    }

    #[test]
    fn write_into_short_buffer_is_none() {
        let mut one = [0u8; 1];
        assert_eq!(write(&mut one, 300), None); // needs 2 bytes
        assert_eq!(write(&mut one, 5), Some(1));
    }
}
