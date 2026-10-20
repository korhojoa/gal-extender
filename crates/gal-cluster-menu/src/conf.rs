// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use core::fmt::Write as _;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use crate::cbuf::CBuf;
use crate::settings::Setting;

const LINE_CAP: usize = 512;

pub fn c_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let max = rest.len().min(LINE_CAP - 1);
        let n = rest[..max]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(max, |i| i + 1);
        let (piece, tail) = rest.split_at(n);
        rest = tail;
        let end = piece.iter().position(|&b| b == 0).unwrap_or(piece.len());
        Some(&piece[..end])
    })
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// `parse_key`: a line `[blanks] [#] [blanks] IDENT [blanks] = ...`. The key
/// and whether a `#` came before it. None for any other line (blank, a bare
/// feature word, a plain comment) and for a key of 128 bytes or more.
#[must_use]
pub fn parse_key(line: &[u8]) -> Option<(&[u8], bool)> {
    let at = |i: usize| line.get(i).copied().unwrap_or(0);
    let mut p = 0;
    while is_blank(at(p)) {
        p += 1;
    }
    let mut commented = false;
    if at(p) == b'#' {
        commented = true;
        p += 1;
        while is_blank(at(p)) {
            p += 1;
        }
    }
    let k = p;
    while is_ident(at(p)) {
        p += 1;
    }
    let klen = p - k;
    if klen == 0 || klen >= 128 {
        return None;
    }
    let mut q = p;
    while is_blank(at(q)) {
        q += 1;
    }
    if at(q) != b'=' {
        return None;
    }
    Some((&line[k..p], commented))
}

/// `strtol(s, &end, 10)` with QNX 6.5's 32-bit `long` (out of range clamps
/// to its limits, as `strtol` does), then `(int)`: the value, or None when
/// no digit was converted.
fn strtol10(s: &[u8]) -> Option<i32> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut p = 0;
    while matches!(at(p), b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        p += 1;
    }
    let neg = at(p) == b'-';
    if matches!(at(p), b'+' | b'-') {
        p += 1;
    }
    let start = p;
    let mut v: i64 = 0;
    while at(p).is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add(i64::from(at(p) - b'0'));
        p += 1;
    }
    if p == start {
        return None;
    }
    let v = if neg { -v } else { v };
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).ok()
}

/// `parse_value`: the integer after the first `=`.
#[must_use]
pub fn parse_value(line: &[u8]) -> Option<i32> {
    let eq = line.iter().position(|&b| b == b'=')?;
    let mut v = &line[eq + 1..];
    while let [b, tail @ ..] = v {
        if !is_blank(*b) {
            break;
        }
        v = tail;
    }
    strtol10(v)
}

/// `find_key`: the first setting with this key.
fn find_key(items: &[Setting], key: &[u8]) -> Option<usize> {
    items.iter().position(|s| s.key.as_bytes() == key)
}

fn path_of(p: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(p))
}

/// `ui_conf_load`: set `value`, `file_value` and `present` of each setting
/// with an uncommented `key=value` line (a later line wins). -1 when the file
/// cannot be read (the settings keep their defaults).
pub fn load(path: &[u8], items: &mut [Setting]) -> i32 {
    let Ok(data) = std::fs::read(path_of(path)) else {
        return -1;
    };
    for line in c_lines(&data) {
        let Some((key, false)) = parse_key(line) else {
            continue;
        };
        if let (Some(i), Some(val)) = (find_key(items, key), parse_value(line)) {
            let s = &mut items[i];
            s.value = val;
            s.file_value = val;
            s.present = true;
        }
    }
    0
}

/// `copy_file`: the backup copy. False on any failure.
fn copy_file(src: &Path, dst: &Path) -> bool {
    let Ok(data) = std::fs::read(src) else {
        return false;
    };
    let Ok(mut out) = std::fs::File::create(dst) else {
        return false;
    };
    out.write_all(&data).is_ok()
}

/// The local time as `%Y%m%dT%H%M%S`.
fn stamp() -> CBuf<24> {
    let mut tm = qnx_time::Tm::zeroed();
    // SAFETY: time() with NULL only returns. localtime_r gets valid pointers.
    unsafe {
        let t = qnx_time::time(core::ptr::null_mut());
        qnx_time::localtime_r(&raw const t, &raw mut tm);
    }
    let mut ts: CBuf<24> = CBuf::new();
    let _ = write!(
        ts,
        "{}{:02}{:02}T{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    ts
}

/// `ui_conf_save`: write the changed settings into `path` (see the module
/// docs). The number of settings written (0: nothing changed, no file
/// touched), after which every setting is clean. or -1 with the reason in
/// `err` (`char err[96]`).
pub fn save<const E: usize>(path: &[u8], items: &mut [Setting], err: &mut CBuf<E>) -> i32 {
    if !items.iter().any(Setting::dirty) {
        return 0;
    }

    // The backup first: no rewrite without a copy to go back to.
    let mut bak: CBuf<600> = CBuf::new();
    bak.push_bytes(path);
    let _ = bak.write_str(".bak-");
    bak.push_bytes(stamp().as_bytes());
    if !copy_file(path_of(path), path_of(bak.as_bytes())) {
        err.clear();
        let _ = err.write_str("backup to ");
        err.push_bytes(bak.as_bytes());
        let _ = err.write_str(" failed");
        return -1;
    }

    let mut tmp: CBuf<600> = CBuf::new();
    tmp.push_bytes(path);
    let _ = tmp.write_str(".tmp");
    let input = std::fs::read(path_of(path)).ok();
    let Ok(mut out) = std::fs::File::create(path_of(tmp.as_bytes())) else {
        err.clear();
        let _ = err.write_str("open ");
        err.push_bytes(tmp.as_bytes());
        let _ = err.write_str(" failed");
        return -1;
    };

    let mut written = [false; 64];
    let cap = items.len().min(written.len());
    let mut text: Vec<u8> = Vec::with_capacity(input.as_ref().map_or(0, Vec::len) + 256);
    let mut vv: CBuf<32> = CBuf::new();
    if let Some(data) = &input {
        for line in c_lines(data) {
            if let Some((key, _)) = parse_key(line) {
                if let Some(i) = find_key(items, key).filter(|&i| items[i].dirty()) {
                    if i < cap && written[i] {
                        continue; // a later line of the same key: dropped
                    }
                    items[i].conf_value(&mut vv);
                    text.extend_from_slice(items[i].key.as_bytes());
                    text.push(b'=');
                    text.extend_from_slice(vv.as_bytes());
                    text.push(b'\n');
                    if i < cap {
                        written[i] = true;
                    }
                    continue;
                }
            }
            text.extend_from_slice(line);
        }
    }
    // The changed settings the file did not have.
    let mut count = 0;
    for (i, s) in items.iter().enumerate() {
        if !s.dirty() {
            continue;
        }
        let done = i < cap && written[i];
        if !done {
            s.conf_value(&mut vv);
            text.extend_from_slice(s.key.as_bytes());
            text.push(b'=');
            text.extend_from_slice(vv.as_bytes());
            text.push(b'\n');
        }
        count += 1;
    }
    if out.write_all(&text).is_err() || out.flush().is_err() {
        err.clear();
        let _ = err.write_str("write ");
        err.push_bytes(tmp.as_bytes());
        let _ = err.write_str(" failed");
        return -1;
    }
    drop(out);
    if std::fs::rename(path_of(tmp.as_bytes()), path_of(path)).is_err() {
        err.clear();
        let _ = err.write_str("rename ");
        err.push_bytes(tmp.as_bytes());
        let _ = err.write_str(" -> ");
        err.push_bytes(path);
        let _ = err.write_str(" failed");
        return -1;
    }
    for s in items.iter_mut() {
        if s.value != s.file_value {
            s.file_value = s.value;
            s.present = true;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::table;

    #[test]
    fn keys_and_values() {
        assert_eq!(
            parse_key(b"  cluster_density = 180\n"),
            Some((&b"cluster_density"[..], false))
        );
        assert_eq!(
            parse_key(b"#\tcluster_focus_kick=1"),
            Some((&b"cluster_focus_kick"[..], true))
        );
        assert_eq!(parse_key(b"cluster\n"), None);
        assert_eq!(parse_key(b"# a comment\n"), None);
        assert_eq!(parse_key(b"=5"), None);
        let long = [b'a'; 128];
        let mut l = long.to_vec();
        l.extend_from_slice(b"=1");
        assert_eq!(parse_key(&l), None);
        assert_eq!(parse_key(&l[1..]).map(|k| k.0.len()), Some(127));
        assert_eq!(parse_value(b"k= \t-42 # x"), Some(-42));
        assert_eq!(parse_value(b"k=+7"), Some(7));
        assert_eq!(parse_value(b"k=x"), None);
        assert_eq!(parse_value(b"k=\r\n"), None);
        assert_eq!(parse_value(b"k=99999999999"), Some(i32::MAX));
        assert_eq!(parse_value(b"k=-99999999999"), Some(i32::MIN));
        assert_eq!(parse_value(b"k=\n12"), Some(12)); // strtol skips the newline
    }

    #[test]
    fn fgets_pieces() {
        let mut d = vec![b'x'; 600];
        d.push(b'\n');
        d.extend_from_slice(b"a=1\nb\0=2\nc");
        let v: Vec<&[u8]> = c_lines(&d).collect();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].len(), 511);
        assert_eq!(v[1].len(), 90);
        assert_eq!(v[2], b"a=1\n");
        assert_eq!(v[3], b"b");
        assert_eq!(v[4], b"c");
    }

    /// A fresh directory for one test. QNX's /tmp is /dev/shmem (no
    /// directories and no `rename()`). If the root filesystem is read-only, the
    /// test uses /qnx. `GAL_TEST_DIR` selects another writable directory.
    fn scratch(name: &str) -> std::path::PathBuf {
        let base = std::env::var_os("GAL_TEST_DIR").map_or_else(
            || {
                if cfg!(target_os = "nto") {
                    std::path::PathBuf::from("/qnx")
                } else {
                    std::env::temp_dir()
                }
            },
            std::path::PathBuf::from,
        );
        let d = base.join(format!("gal-cluster-menu-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_rewrites_drops_dupes_appends_and_backs_up() {
        let d = scratch("save");
        let p = d.join("gal_loader.conf");
        std::fs::write(
            &p,
            "# head\ncluster\n# cluster_density=1\ncluster_density=7\ncluster_density=133\n\
             external_choice=0\nlong\0tail=9\n",
        )
        .unwrap();
        let pb = p.as_os_str().as_bytes();
        let mut t = table();
        assert_eq!(load(pb, &mut t), 0);
        assert_eq!((t[0].value, t[0].present), (133, true)); // the last line wins
        let mut err: CBuf<96> = CBuf::new();
        assert_eq!(save(pb, &mut t, &mut err), 0); // nothing changed
        t[0].adjust(1);
        t[2].adjust(1); // cluster_focus_kick, not in the file
        assert_eq!(save(pb, &mut t, &mut err), 2);
        assert!(t.iter().all(|s| !s.dirty()));
        assert!(t[2].present);
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            got,
            "# head\ncluster\ncluster_density=134\nexternal_choice=0\nlongcluster_focus_kick=1\n"
        );
        let baks: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("gal_loader.conf.bak-")
            })
            .collect();
        assert_eq!(baks.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_reports_a_failed_backup() {
        let d = scratch("nofile");
        let p = d.join("absent.conf");
        let pb = p.as_os_str().as_bytes();
        let mut t = table();
        assert_eq!(load(pb, &mut t), -1);
        t[0].adjust(1);
        let mut err: CBuf<96> = CBuf::new();
        assert_eq!(save(pb, &mut t, &mut err), -1);
        let e = String::from_utf8_lossy(err.as_bytes()).into_owned();
        assert!(e.starts_with("backup to "), "{e}");
        assert!(e.len() <= 95);
        let _ = std::fs::remove_dir_all(&d);
    }
}
