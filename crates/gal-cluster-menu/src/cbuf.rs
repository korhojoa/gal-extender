// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use core::fmt;

/// A fixed buffer of at most `N - 1` bytes of text.
#[derive(Clone, Copy)]
pub struct CBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> CBuf<N> {
    #[must_use]
    pub const fn new() -> Self {
        CBuf {
            buf: [0; N],
            len: 0,
        }
    }

    /// The text (no NUL).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// True when empty (`buf[0] == '\0'`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empty it.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// `snprintf(buf, N, ...)`: replace the text.
    pub fn set(&mut self, args: fmt::Arguments<'_>) {
        self.len = 0;
        let _ = fmt::Write::write_fmt(self, args);
    }

    /// Append raw bytes, cut at `N - 1` (the `%s` of a C string).
    pub fn push_bytes(&mut self, b: &[u8]) {
        let room = (N - 1).saturating_sub(self.len);
        let n = b.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
    }
}

impl<const N: usize> Default for CBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for CBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_bytes(s.as_bytes());
        Ok(())
    }
}

pub struct Shown<'a>(pub &'a [u8]);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            for _ in chunk.invalid() {
                f.write_str("?")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_at_n_minus_one_bytes() {
        let mut b: CBuf<8> = CBuf::new();
        b.set(format_args!("{}-{}", "abcdef", 12));
        assert_eq!(b.as_bytes(), b"abcdef-");
        b.set(format_args!("x"));
        assert_eq!(b.as_bytes(), b"x");
        assert!(!b.is_empty());
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn shown_replaces_invalid_bytes() {
        assert_eq!(format!("{}", Shown(b"a\xffb")), "a?b");
    }
}
