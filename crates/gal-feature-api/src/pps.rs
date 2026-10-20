// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! QNX PPS objects: the IPC between gal's features and the other processes
//! on the unit. One object per channel under
//! `<root>/gal/`, the root `/ramdisk/pps` (the vendor's PPS mount) or, off
//! the unit, `GAL_PPS_ROOT`.
//!
//! - A publish is ONE `write()` of `attr::value\n` lines on a descriptor
//!   opened `O_WRONLY|O_CREAT` without `O_TRUNC` (a truncating open deletes
//!   every attribute first, in a separate step). No temp file, no rename:
//!   `/dev/shmem` has no `rename()`.
//! - PPS merges: an attribute stays until a `-attr` line deletes it. The
//!   [`Publisher`] deletes, in the same write, every key of its previous
//!   publish (the first time: of the object as found) that is absent now, so
//!   a reader sees the complete current key set.
//! - A read is one `open` + `read` of the whole object: `@<name>` first,
//!   then `name:<encoding>:value` lines. [`attrs`] skips the `@` and `-`
//!   lines and the encoding. Readers run on gal's threads through caller
//!   buffers: nothing here allocates on the read path.

use core::ffi::CStr;
use std::ffi::CString;
use std::io::{Read as _, Write as _};
use std::sync::OnceLock;

/// The PPS mount on the unit.
pub const DEFAULT_ROOT: &str = "/ramdisk/pps";
/// gal's directory under the root.
pub const DIR: &str = "gal";

static ROOT: OnceLock<String> = OnceLock::new();

/// The PPS root: `GAL_PPS_ROOT`, else [`DEFAULT_ROOT`]. Read once.
pub fn root() -> &'static str {
    ROOT.get_or_init(|| {
        std::env::var_os("GAL_PPS_ROOT")
            .filter(|v| !v.is_empty())
            .and_then(|v| v.into_string().ok())
            .unwrap_or_else(|| DEFAULT_ROOT.to_string())
    })
}

/// The directory of gal's objects, `<root>/gal`.
#[must_use]
pub fn dir() -> String {
    format!("{}/{}", root(), DIR)
}

/// `<root>/gal/<name>`.
#[must_use]
pub fn path(name: &str) -> String {
    format!("{}/{}/{}", root(), DIR, name)
}

/// One `name::value` (or `name:enc:value`) line: the name and the value.
/// None for the `@object` line, a `-name` deletion, or anything else.
#[must_use]
pub fn attr(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let first = line.iter().position(|&c| c == b':')?;
    if first == 0 || matches!(line[0], b'@' | b'-') {
        return None;
    }
    let rest = &line[first + 1..];
    let second = rest.iter().position(|&c| c == b':')?;
    Some((&line[..first], &rest[second + 1..]))
}

/// The attributes of an object's text, in order.
pub fn attrs(text: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let text = text.split(|&c| c == 0).next().unwrap_or(&[]);
    text.split(|&c| c == b'\n').filter_map(attr)
}

/// The value of `name` in `text`.
#[must_use]
pub fn get<'a>(text: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    attrs(text).find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// A PPS object in gal's directory, its path built once.
pub struct Object {
    name: &'static str,
    path: OnceLock<CString>,
}

impl Object {
    /// The object `<root>/gal/<name>`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Object {
            name,
            path: OnceLock::new(),
        }
    }

    /// The object's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The path, NUL-terminated.
    #[must_use]
    pub fn path(&self) -> &CStr {
        self.path
            .get_or_init(|| CString::new(path(self.name)).unwrap_or_default())
    }

    /// The path as text.
    #[must_use]
    pub fn path_str(&self) -> &str {
        self.path().to_str().unwrap_or("?")
    }

    /// Whether the object exists (`access(F_OK)`).
    #[must_use]
    pub fn exists(&self) -> bool {
        std::fs::metadata(self.path_str()).is_ok()
    }

    /// Delete the object (`unlink`). a missing object is not an error.
    pub fn remove(&self) {
        let _ = std::fs::remove_file(self.path_str());
    }

    /// Read up to `buf.len()` bytes of the object. None when it cannot be
    /// opened.
    #[must_use]
    pub fn read<'a>(&self, buf: &'a mut [u8]) -> Option<&'a [u8]> {
        let mut f = std::fs::File::open(self.path_str()).ok()?;
        let mut n = 0usize;
        while n < buf.len() {
            match f.read(&mut buf[n..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
        Some(&buf[..n])
    }

    /// Publish `text` (`attr::value\n` and `-attr\n` lines): the directory
    /// is created, the object opened `O_WRONLY|O_CREAT` (never truncated),
    /// one `write()`.
    ///
    /// # Errors
    /// The open or the write failed, or the write was short.
    pub fn write(&self, text: &[u8]) -> std::io::Result<()> {
        write_object(self.path_str(), text)
    }
}

/// One write of `text` to the PPS object at `path` (see [`Object::write`]).
///
/// # Errors
/// The open or the write failed, or the write was short.
pub fn write_object(path: &str, text: &[u8]) -> std::io::Result<()> {
    if let Some(i) = path.rfind('/') {
        let _ = std::fs::create_dir_all(&path[..i]);
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let n = f.write(text)?;
    if n == text.len() {
        Ok(())
    } else {
        Err(std::io::Error::other("short PPS write"))
    }
}

/// The text of one publish: `key=value` lines (a feature's own render
/// format) as `key::value`, plus a `-key` for every key of `previous` that
/// is not in `kv`. The keys of `kv` come back for the next call.
#[must_use]
pub fn render(kv: &[u8], previous: &[Vec<u8>]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut text = Vec::with_capacity(kv.len() + 16);
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for line in kv.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(k) = line.strip_prefix(b"-") {
            if !k.is_empty() && !keys.iter().any(|x| x == k) {
                text.extend_from_slice(line);
                text.push(b'\n');
            }
            continue;
        }
        let Some(eq) = line.iter().position(|&c| c == b'=') else {
            continue;
        };
        let (k, v) = (&line[..eq], &line[eq + 1..]);
        if k.is_empty() || k.contains(&b':') {
            continue;
        }
        text.extend_from_slice(k);
        text.extend_from_slice(b"::");
        text.extend_from_slice(v);
        text.push(b'\n');
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_vec());
        }
    }
    for k in previous {
        if !keys.iter().any(|x| x == k) {
            text.push(b'-');
            text.extend_from_slice(k);
            text.push(b'\n');
        }
    }
    (text, keys)
}

/// A writer whose key set varies between publishes.
pub struct Publisher {
    obj: &'static Object,
    keys: Option<Vec<Vec<u8>>>,
}

impl Publisher {
    #[must_use]
    pub const fn new(obj: &'static Object) -> Self {
        Publisher { obj, keys: None }
    }

    /// The keys the object holds now (the first publish of a process
    /// deletes the ones it does not write).
    fn prime(&mut self) {
        if self.keys.is_some() {
            return;
        }
        let mut buf = [0u8; 4096];
        let found = self
            .obj
            .read(&mut buf)
            .map(|t| attrs(t).map(|(k, _)| k.to_vec()).collect())
            .unwrap_or_default();
        self.keys = Some(found);
    }

    /// Publish `kv` (`key=value` lines, `-key` lines passed through) as one
    /// write, deleting the keys of the previous publish that are absent.
    ///
    /// # Errors
    /// The open or the write failed (the key set is then kept for a retry).
    pub fn publish_kv(&mut self, kv: &[u8]) -> std::io::Result<()> {
        self.prime();
        let previous = self.keys.take().unwrap_or_default();
        let (text, keys) = render(kv, &previous);
        let r = self.obj.write(&text);
        self.keys = Some(if r.is_ok() { keys } else { previous });
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_as_pps_attributes() {
        assert_eq!(attr(b"seq::5"), Some((&b"seq"[..], &b"5"[..])));
        assert_eq!(
            attr(b"hmi_ms:n:48123"),
            Some((&b"hmi_ms"[..], &b"48123"[..]))
        );
        assert_eq!(
            attr(b"dest_address::Main St, Town"),
            Some((&b"dest_address"[..], &b"Main St, Town"[..]))
        );
        assert_eq!(
            attr(b"url::http://x:1"),
            Some((&b"url"[..], &b"http://x:1"[..]))
        );
        assert_eq!(attr(b"@nav"), None);
        assert_eq!(attr(b"-soc_pct"), None);
        assert_eq!(attr(b"seq=5"), None);
        assert_eq!(attr(b""), None);
        assert_eq!(attr(b"::x"), None);
        let text = b"@ev\nseq::7\nupdated_ms::42\n-soc_pct\nrange_m::1000\r\n\0junk::1\n";
        let got: Vec<_> = attrs(text).collect();
        assert_eq!(
            got,
            vec![
                (&b"seq"[..], &b"7"[..]),
                (&b"updated_ms"[..], &b"42"[..]),
                (&b"range_m"[..], &b"1000"[..]),
            ]
        );
        assert_eq!(get(text, b"range_m"), Some(&b"1000"[..]));
        assert_eq!(get(text, b"soc_pct"), None);
    }

    #[test]
    fn render_converts_and_deletes_the_missing_keys() {
        let (t, keys) = render(b"seq=1\nstate=routing\nstep_road=Main St\n", &[]);
        assert_eq!(t, b"seq::1\nstate::routing\nstep_road::Main St\n");
        assert_eq!(
            keys,
            vec![b"seq".to_vec(), b"state".to_vec(), b"step_road".to_vec()]
        );
        let (t, keys2) = render(b"seq=2\nstate=routing\n-dest_dist_m\n", &keys);
        assert_eq!(t, b"seq::2\nstate::routing\n-dest_dist_m\n-step_road\n");
        assert_eq!(keys2, vec![b"seq".to_vec(), b"state".to_vec()]);
        // round trip: what a reader sees after the second publish
        let seen: Vec<_> = attrs(&t).map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        assert_eq!(
            seen,
            vec![
                (b"seq".to_vec(), b"2".to_vec()),
                (b"state".to_vec(), b"routing".to_vec())
            ]
        );
        // malformed lines are dropped, keys with a colon too
        let (t, _) = render(b"junk\n=v\na:b=1\nok=1\n", &[]);
        assert_eq!(t, b"ok::1\n");
    }

    #[test]
    fn objects_write_once_and_read_back() {
        let dir = std::env::temp_dir().join(format!("gal-pps-test-{}", std::process::id()));
        let p = dir.join("gal/obj");
        let ps = p.to_str().unwrap();
        write_object(ps, b"a::1\nb::two\n").unwrap();
        let text = std::fs::read(&p).unwrap();
        assert_eq!(get(&text, b"a"), Some(&b"1"[..]));
        assert_eq!(get(&text, b"b"), Some(&b"two"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
