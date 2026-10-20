// SPDX-License-Identifier: GPL-3.0-only
//! Nonblocking, single-operation PPS snapshots. No retry or wait option.
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

// Verified QNX 6.5 fcntl.h value.
#[cfg(target_os = "nto")]
const NONBLOCK: i32 = 0o200;
#[cfg(not(target_os = "nto"))]
const NONBLOCK: i32 = 0o4000;
const LIMIT: usize = crate::policy_input::MAX_PPS;

#[must_use]
pub fn read(path: &str) -> Option<String> {
    if path.contains('?') {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCK)
        .open(path)
        .ok()?;
    snapshot(file)
}

fn snapshot(mut input: impl Read) -> Option<String> {
    let mut bytes = [0; LIMIT + 1];
    let count = input.read(&mut bytes).ok()?;
    if count == 0 || count > LIMIT {
        return None;
    }
    std::str::from_utf8(&bytes[..count]).ok().map(str::to_owned)
}

/// # Errors
/// Reports an unavailable object, oversized update, short write or I/O error.
pub fn write(path: &str, text: &str) -> io::Result<()> {
    if path.contains('?') || text.len() > LIMIT {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(NONBLOCK)
        .open(path)?;
    update(file, text.as_bytes())
}

fn update(mut output: impl Write, bytes: &[u8]) -> io::Result<()> {
    if output.write(bytes)? == bytes.len() {
        Ok(())
    } else {
        Err(io::ErrorKind::WriteZero.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct WouldBlock;
    impl Read for WouldBlock {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
    impl Write for WouldBlock {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("must not flush or retry")
        }
    }
    struct Short;
    impl Write for Short {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len() - 1)
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("must not flush or retry")
        }
    }

    #[test]
    fn unavailable_and_wait_paths_fail_without_retry_or_stale_values() {
        assert!(read("/nonexistent-native-aa-pps-fixture/object").is_none());
        assert!(read("/unused?wait,delta").is_none());
        assert!(snapshot(WouldBlock).is_none());
        assert!(snapshot(&b""[..]).is_none());
        assert!(snapshot(&[0xff][..]).is_none());
        assert!(snapshot(&vec![b'x'; LIMIT + 1][..]).is_none());
        assert_eq!(snapshot(&b"epoch::2\n"[..]).as_deref(), Some("epoch::2\n"));
    }
    #[test]
    fn atomic_update_never_retries_partial_or_blocked_writes() {
        assert_eq!(
            update(Short, b"epoch::2\n").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        assert_eq!(
            update(WouldBlock, b"epoch::2\n").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(write("/unused?wait", "x").is_err());
        assert!(write("/unused", &"x".repeat(LIMIT + 1)).is_err());
    }
}
