// SPDX-License-Identifier: GPL-3.0-only
//! Stable storage for the recovered native SDP ABI. Mutate only while holding
//! the native stack lock. retain this object for the complete registration life.
use std::cell::UnsafeCell;
use std::ffi::c_void;

pub const UUID: [u8; 16] = [
    0x4d, 0xe1, 0x7a, 0x00, 0x52, 0xcb, 0x11, 0xe6, 0xbd, 0xf4, 0x08, 0x00, 0x20, 0x0c, 0x9a, 0x66,
];

#[repr(C)]
struct Attribute {
    id: u16,
    length: u16,
    value: *mut u8,
    state: u16,
    reserved: u16,
}

#[repr(C)]
struct NativeRecord {
    // Native intrusive-list links. deliberately left to the factory stack.
    links: [u32; 2],
    count: u8,
    padding: [u8; 3],
    attributes: *mut Attribute,
    opaque: [u32; 5],
}

#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(std::mem::size_of::<Attribute>() == 12);
    assert!(std::mem::offset_of!(Attribute, value) == 4);
    assert!(std::mem::offset_of!(Attribute, state) == 8);
    assert!(std::mem::size_of::<NativeRecord>() == 0x24);
    assert!(std::mem::offset_of!(NativeRecord, count) == 8);
    assert!(std::mem::offset_of!(NativeRecord, attributes) == 12);
};

/// The pointer-bearing ABI is native-width for host tests. it is only callable
/// on the audited 32-bit target. Each native-writable allocation is `UnsafeCell`,
/// so passing a pointer does not grant native code access to immutable Rust data.
pub struct Record {
    native: Box<UnsafeCell<NativeRecord>>,
    attributes: Box<UnsafeCell<[Attribute; 4]>>,
    encodings: [Box<UnsafeCell<[u8; 19]>>; 4],
}

impl Record {
    /// # Errors
    /// Rejects RFCOMM channel numbers outside the native server range.
    pub fn new(channel: u8) -> Result<Self, &'static str> {
        check_channel(channel)?;
        let mut encoded = [[0u8; 19]; 4];
        encoded[0][..3].copy_from_slice(&[0x35, 0x11, 0x1c]);
        encoded[0][3..].copy_from_slice(&UUID);
        encoded[1][..14].copy_from_slice(&[
            0x35, 0x0c, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00, 0x03, 0x08, channel,
        ]);
        encoded[2][..5].copy_from_slice(&[0x35, 0x03, 0x19, 0x10, 0x02]);
        encoded[3][..14].copy_from_slice(b"\x25\x0cAndroid Auto");
        let encodings = encoded.map(|bytes| Box::new(UnsafeCell::new(bytes)));
        let attributes = Box::new(UnsafeCell::new(std::array::from_fn(|i| Attribute {
            id: [1, 4, 5, 0x100][i],
            length: [19, 14, 5, 14][i],
            value: encodings[i].get().cast(),
            state: 0,
            reserved: 0,
        })));
        let native = Box::new(UnsafeCell::new(NativeRecord {
            links: [0; 2],
            count: 4,
            padding: [0; 3],
            attributes: attributes.get().cast(),
            opaque: [0; 5],
        }));
        Ok(Self {
            native,
            attributes,
            encodings,
        })
    }

    /// Set the assigned number before SDP publication, under native locking.
    /// Never change the published record while native code may read it.
    ///
    /// # Errors
    /// Rejects RFCOMM channel numbers outside the native server range.
    pub fn set_channel(&mut self, channel: u8) -> Result<(), &'static str> {
        check_channel(channel)?;
        self.encodings[1].get_mut()[13] = channel;
        Ok(())
    }

    /// Caller must keep `self` alive until native removal completes. Calling the
    /// native stack additionally requires its lock, verified identity and ABI.
    #[must_use]
    pub fn record_ptr(&self) -> *mut c_void {
        self.native.get().cast()
    }

    /// The arrays are owned even though native code accesses them via pointers.
    #[must_use]
    pub fn attribute_count(&self) -> usize {
        // Do not read native-owned mutable fields. this is the fixed allocation.
        let _ = &self.attributes;
        4
    }
}

fn check_channel(channel: u8) -> Result<(), &'static str> {
    if (1..=6).contains(&channel) {
        Ok(())
    } else {
        Err("assigned RFCOMM channel must be 1..6")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Independent strict DE decoder catches length, UUID and PDL tag errors.
    #[derive(Debug, PartialEq)]
    enum Element {
        Scalar(u8, Vec<u8>),
        Sequence(Vec<Element>),
    }
    fn decode(bytes: &[u8]) -> Result<(Element, usize), &'static str> {
        let tag = *bytes.first().ok_or("missing element")?;
        let (start, len) = match tag {
            0x25 | 0x35 => (2, *bytes.get(1).ok_or("missing length")? as usize),
            0x08 => (1, 1),
            0x19 => (1, 2),
            0x1c => (1, 16),
            _ => return Err("unsupported tag"),
        };
        let data = bytes.get(start..start + len).ok_or("truncated element")?;
        let element = if tag == 0x35 {
            let mut children = Vec::new();
            let mut remaining = data;
            while !remaining.is_empty() {
                let (child, n) = decode(remaining)?;
                children.push(child);
                remaining = &remaining[n..];
            }
            Element::Sequence(children)
        } else {
            Element::Scalar(tag, data.to_vec())
        };
        Ok((element, start + len))
    }
    #[test]
    fn all_assigned_channels_have_valid_canonical_records() {
        use Element::{Scalar as S, Sequence as Q};
        for channel in 1..=6 {
            let record = Record::new(channel).unwrap();
            let expected = [
                Q(vec![S(0x1c, UUID.to_vec())]),
                Q(vec![
                    Q(vec![S(0x19, vec![1, 0])]),
                    Q(vec![S(0x19, vec![0, 3]), S(8, vec![channel])]),
                ]),
                Q(vec![S(0x19, vec![0x10, 2])]),
                S(0x25, b"Android Auto".to_vec()),
            ];
            // No native access runs in these host tests.
            let attrs = unsafe { &*record.attributes.get() };
            for (i, attr) in attrs.iter().enumerate() {
                let bytes = unsafe { std::slice::from_raw_parts(attr.value, attr.length as usize) };
                let (actual, n) = decode(bytes).unwrap();
                assert_eq!(n, bytes.len());
                assert_eq!(actual, expected[i]);
            }
            assert_eq!(
                std::array::from_fn::<_, 4, _>(|i| attrs[i].id),
                [1, 4, 5, 0x100]
            );
        }
    }
    #[test]
    fn rejects_invalid_channels_and_legacy_descriptor_corruption() {
        for channel in [0, 7, 8, 255] {
            assert!(Record::new(channel).is_err());
        }
        let mut record = Record::new(1).unwrap();
        assert!(record.set_channel(7).is_err());
        let mut bytes = *record.encodings[1].get_mut();
        bytes[12] = 3;
        assert!(decode(&bytes[..14]).is_err());
        assert!(decode(&[0x35, 5, 0x19, 1]).is_err());
    }
    #[test]
    fn moving_owner_preserves_every_pointer_and_channel_tag() {
        let mut record = Record::new(1).unwrap();
        let ptr = record.record_ptr();
        let attrs = record.attributes.get();
        let value = record.encodings[1].get();
        record.set_channel(6).unwrap();
        let moved = Box::new(record);
        assert_eq!(ptr, moved.record_ptr());
        assert_eq!(attrs, moved.attributes.get());
        assert_eq!(value, moved.encodings[1].get());
        unsafe {
            assert_eq!((*moved.native.get()).attributes, attrs.cast());
            assert_eq!((*attrs)[1].value, value.cast());
            assert_eq!((*value)[12], 8);
            assert_eq!((*value)[13], 6);
        }
    }
}
