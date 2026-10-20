// SPDX-License-Identifier: GPL-3.0-only
//! Fail-closed identity checks for the two audited factory executables.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub const EXPECTED_SHA256: &str =
    "77d99b88b924f1776ec78b14ef447cc01b03b894dd4b39f0f15c38dafbbfa708";
pub const K4525_SHA256: &str = "dd80ce8f6d95589cc4db6674150a595b8412d132e2d9e689fbfe9f72e5789251";
const SUPPORTED_SHA256: [&str; 2] = [EXPECTED_SHA256, K4525_SHA256];

pub struct Entry {
    pub name: &'static str,
    pub address: usize,
    pub fingerprint: &'static str,
}
pub const ENTRIES: [Entry; 11] = [
    Entry {
        name: "rf_register",
        address: 0x27_7a30,
        fingerprint: "c72c9e321e9d5210cade78169a0beb12e79daf39573c805c610e89b373b3a778",
    },
    Entry {
        name: "rf_deregister",
        address: 0x27_79cc,
        fingerprint: "8f3cc4625928f29485354bb2682062c1d29d55e3cee99afa0c35e7b067da336f",
    },
    Entry {
        name: "rf_accept",
        address: 0x27_7354,
        fingerprint: "f3e653967b335724ece380569c94ed6c3db187f68a2a7555143ac1bf3dbc5d16",
    },
    Entry {
        name: "rf_close",
        address: 0x27_7410,
        fingerprint: "d662075b0bd23d424bbad4d81405d2488dc73e569dba3c263a53a7a29d1762a9",
    },
    Entry {
        name: "rf_give_credit",
        address: 0x27_7174,
        fingerprint: "65905340b3773a971789b2cc9d19135fae0a0f524ba143ce6bbdf034048e9530",
    },
    Entry {
        name: "rf_send",
        address: 0x27_7210,
        fingerprint: "6ab0696c380a995ebc29ccaeffacfe8ec2c2a0f22c7deb500cc7644b0f628fb7",
    },
    Entry {
        name: "rf_frame_size",
        address: 0x27_708c,
        fingerprint: "40d2454ad7cb84cdb4ae7c62ce3334a75ef1cde84c9c036ae989d020ea4c7c09",
    },
    Entry {
        name: "stack_lock",
        address: 0x27_a144,
        fingerprint: "b47eb0e3d2bc9b5c0e29bac0b45a6f879bca29e4f1eb50ff40af3e16ce8c78f6",
    },
    Entry {
        name: "stack_unlock",
        address: 0x27_a0b8,
        fingerprint: "f718b0bc316abc91e348dac3b29ec518d03d8291c4021a01f4d86e8b781428c9",
    },
    Entry {
        name: "sdp_add",
        address: 0x20_725c,
        fingerprint: "2d255050688011f4b6c55b78b59c1b9e38562d9b6d163d4c950379cabec9e6cc",
    },
    Entry {
        name: "sdp_remove",
        address: 0x20_71dc,
        fingerprint: "6885271022612bfd6a339b5565d870def7b8d0c2d233f6699bce51c0c0a95cd6",
    },
];

/// Compare exactly one sixteen-byte entry without storing vendor instructions.
#[must_use]
pub fn matches_fingerprint(bytes: &[u8], expected: &str) -> bool {
    if bytes.len() != 16 {
        return false;
    }
    let mut hash = Sha256::new();
    hash.update(bytes);
    hex(&hash.finish()) == expected
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn u16le(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}
fn u32le(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

/// Checks disk identity only. the loader must separately verify the running
/// process symbols and instruction words before dereferencing native addresses.
///
/// # Errors
/// Returns I/O errors or a failed whole-file identity/ELF/instruction check.
pub fn verify_file(path: impl AsRef<Path>) -> io::Result<()> {
    let mut file = File::open(path)?;
    verify_reader(&mut file)
}

fn verify_reader(file: &mut (impl Read + Seek)) -> io::Result<()> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let actual = hex(&hash.finish());
    if !SUPPORTED_SHA256.contains(&actual.as_str()) {
        return Err(invalid("factory btstack SHA256 mismatch"));
    }
    verify_elf(file)
}

fn verify_elf(file: &mut (impl Read + Seek)) -> io::Result<()> {
    verify_elf_entries(file, &ENTRIES)
}

fn verify_elf_entries(file: &mut (impl Read + Seek), entries: &[Entry]) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0; 52];
    file.read_exact(&mut header)?;
    if &header[..7] != b"\x7fELF\x01\x01\x01"
        || u16le(&header, 16) != 2
        || u16le(&header, 18) != 40
        || u16le(&header, 42) != 32
    {
        return Err(invalid("expected little-endian ARM ELF32 ET_EXEC"));
    }
    let phoff = u64::from(u32le(&header, 28));
    let mut segments = Vec::new();
    for i in 0..u16le(&header, 44) {
        file.seek(SeekFrom::Start(phoff + u64::from(i) * 32))?;
        let mut ph = [0; 32];
        file.read_exact(&mut ph)?;
        if u32le(&ph, 0) == 1 && u32le(&ph, 24) & 1 != 0 {
            segments.push((
                u64::from(u32le(&ph, 4)),
                u64::from(u32le(&ph, 8)),
                u64::from(u32le(&ph, 16)),
            ));
        }
    }
    for entry in entries {
        let address = entry.address as u64;
        let &(offset, base, _) = segments
            .iter()
            .find(|&&(_, base, size)| address >= base && address + 16 <= base + size)
            .ok_or_else(|| invalid(format!("{} outside executable segment", entry.name)))?;
        file.seek(SeekFrom::Start(offset + address - base))?;
        let mut words = [0; 16];
        file.read_exact(&mut words)?;
        if !matches_fingerprint(&words, entry.fingerprint) {
            return Err(invalid(format!("{} instruction mismatch", entry.name)));
        }
    }
    Ok(())
}

// Reuse the linked credential library's pinned SHA-256 implementation.
struct Sha256(sha2::Sha256);
impl Sha256 {
    fn new() -> Self {
        Self(sha2::Digest::new())
    }
    fn update(&mut self, bytes: &[u8]) {
        sha2::Digest::update(&mut self.0, bytes);
    }
    fn finish(self) -> [u8; 32] {
        sha2::Digest::finalize(self.0).into()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("String formatting");
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires both locally extracted firmware references"]
    fn extracted_firmware_identities() {
        for variable in ["BTSHIM_REFERENCE_EXECUTABLE", "BTSHIM_K4525_EXECUTABLE"] {
            let path = std::env::var_os(variable).expect("set the extracted firmware path");
            verify_file(&path).unwrap_or_else(|error| panic!("{variable}: {error}"));
            let mut bytes = std::fs::read(path).unwrap();
            // A one-byte change must invalidate an otherwise accepted executable.
            let last = bytes.last_mut().unwrap();
            *last ^= 1;
            assert_eq!(
                verify_reader(&mut io::Cursor::new(bytes))
                    .unwrap_err()
                    .to_string(),
                "factory btstack SHA256 mismatch"
            );
        }
    }
    #[test]
    fn sha256_known_vectors_and_chunking() {
        for (input, expected) in [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            for size in [1, 7, 64, 8192] {
                let mut hash = Sha256::new();
                for chunk in input.as_bytes().chunks(size) {
                    hash.update(chunk);
                }
                let actual = hex(&hash.finish());
                assert_eq!(actual, expected);
            }
        }
        let mut hash = Sha256::new();
        for _ in 0..1000 {
            hash.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&hash.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
    #[test]
    fn fingerprint_requires_exact_length_and_all_bytes() {
        let fixture = [0x5a; 16];
        let expected = "1c712ecc21e27e374111d5a1beeaf75a4e343b3814c1847cba14013420809873";
        assert!(matches_fingerprint(&fixture, expected));
        assert!(!matches_fingerprint(&fixture[..15], expected));
        assert!(!matches_fingerprint(&[0x5a; 17], expected));
        for at in 0..fixture.len() {
            let mut changed = fixture;
            changed[at] ^= 1;
            assert!(!matches_fingerprint(&changed, expected));
        }
    }

    #[test]
    fn elf_rejects_malformed_header() {
        assert!(verify_elf(&mut io::Cursor::new(vec![0; 52])).is_err());
        assert!(verify_elf(&mut io::Cursor::new(vec![0; 12])).is_err());
    }
    #[test]
    fn elf_checks_file_backed_executable_words() {
        let base = 0x0020_0000_usize;
        let mut bytes = vec![0u8; 0x100 + 0x8_0000];
        bytes[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        for (at, value) in [(16, 2u16), (18, 40), (42, 32), (44, 1)] {
            bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (at, value) in [
            (28, 52u32),
            (52, 1),
            (56, 0x100),
            (60, u32::try_from(base).unwrap()),
            (68, 0x8_0000),
            (76, 5),
        ] {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        let entries = [Entry {
            name: "synthetic",
            address: base + 64,
            fingerprint: "1c712ecc21e27e374111d5a1beeaf75a4e343b3814c1847cba14013420809873",
        }];
        let at = 0x100 + entries[0].address - base;
        bytes[at..at + 16].fill(0x5a);
        assert!(verify_elf_entries(&mut io::Cursor::new(&bytes), &entries).is_ok());
        // Matching ELF entry words alone do not authorize an unknown file.
        assert_eq!(
            verify_reader(&mut io::Cursor::new(&bytes))
                .unwrap_err()
                .to_string(),
            "factory btstack SHA256 mismatch"
        );
        bytes[at] ^= 1;
        assert!(verify_elf_entries(&mut io::Cursor::new(&bytes), &entries)
            .unwrap_err()
            .to_string()
            .contains("synthetic"));
        bytes[76] = 4;
        assert!(verify_elf_entries(&mut io::Cursor::new(&bytes), &entries)
            .unwrap_err()
            .to_string()
            .contains("executable segment"));
    }
}
