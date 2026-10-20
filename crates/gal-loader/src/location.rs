// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Find the loaded library directory. Use an absolute `GAL_LOADER_DIR` test override, the
//! dynamic linker path from dladdr, or the matching `LD_PRELOAD` entry. Read preload
//! before clearing it for child processes. If no absolute directory is found, load no
//! features. Configuration and feature data are relative to the selected directory.

use core::ffi::{c_char, c_int, c_void, CStr};

/// Which step of the order gave the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Override,
    Dladdr,
    LdPreload,
}

impl Found {
    pub fn describe(self) -> &'static str {
        match self {
            Found::Override => "GAL_LOADER_DIR override",
            Found::Dladdr => "dladdr of the loader",
            Found::LdPreload => "the LD_PRELOAD entry",
        }
    }
}

/// The directory part of an absolute file path, without the trailing `/`.
/// None for a relative path. A file directly in `/` resolves to `/`.
fn dir_of(path: &str) -> Option<&str> {
    if !path.starts_with('/') {
        return None;
    }
    let slash = path.rfind('/')?;
    let dir = path[..slash].trim_end_matches('/');
    Some(if dir.is_empty() { "/" } else { dir })
}

/// The loader's entry in an `LD_PRELOAD` list (entries separated by `:` or
/// white space): the first whose file name starts with `libgal_loader`, or
/// the only entry when there is one.
fn preload_entry(list: &str) -> Option<&str> {
    let all: Vec<&str> = list
        .split(|c: char| c == ':' || c.is_ascii_whitespace())
        .filter(|e| !e.is_empty())
        .collect();
    let named = all.iter().copied().find(|e| {
        e.rsplit('/')
            .next()
            .is_some_and(|f| f.starts_with("libgal_loader"))
    });
    named.or(if all.len() == 1 { Some(all[0]) } else { None })
}

/// The decision, from the three inputs (pure, for the unit tests).
pub fn resolve(
    override_dir: Option<&str>,
    dladdr_path: Option<&str>,
    ld_preload: Option<&str>,
) -> Option<(String, Found)> {
    if let Some(d) = override_dir.filter(|d| d.starts_with('/')) {
        return Some((d.to_string(), Found::Override));
    }
    if let Some(d) = dladdr_path.and_then(dir_of) {
        return Some((d.to_string(), Found::Dladdr));
    }
    if let Some(d) = ld_preload.and_then(preload_entry).and_then(dir_of) {
        return Some((d.to_string(), Found::LdPreload));
    }
    None
}

/// Configuration beside the library, using its resolved absolute directory.
pub fn config_path(dir: &str) -> String {
    std::path::Path::new(dir)
        .join("gal_loader.conf")
        .to_string_lossy()
        .into_owned()
}

/// dlfcn.h (QNX 6.5.0 SP1), `struct dl_info`.
#[repr(C)]
#[allow(clippy::struct_field_names)] // the native field names
struct DlInfo {
    dli_fname: *const c_char,
    dli_fbase: *mut c_void,
    dli_sname: *const c_char,
    dli_saddr: *mut c_void,
}

extern "C" {
    fn dladdr(addr: *mut c_void, info: *mut DlInfo) -> c_int;
}

/// The path the dynamic linker has for this object, if it gives one.
pub fn own_path() -> Option<String> {
    let mut info = DlInfo {
        dli_fname: core::ptr::null(),
        dli_fbase: core::ptr::null_mut(),
        dli_sname: core::ptr::null(),
        dli_saddr: core::ptr::null_mut(),
    };
    let addr = own_path as fn() -> Option<String> as *mut c_void;
    // SAFETY: an address inside this object and a writable Dl_info.
    if unsafe { dladdr(addr, &raw mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated name owned by the dynamic linker.
    let name = unsafe { CStr::from_ptr(info.dli_fname) };
    name.to_str().ok().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAR: &str = "/fs/sda0/gal-extender/libgal_loader.so";

    #[test]
    fn the_car_install_resolves_to_its_directory() {
        assert_eq!(
            resolve(None, Some(CAR), Some(CAR)),
            Some(("/fs/sda0/gal-extender".to_string(), Found::Dladdr))
        );
        assert_eq!(
            resolve(None, None, Some(CAR)),
            Some(("/fs/sda0/gal-extender".to_string(), Found::LdPreload))
        );
    }

    #[test]
    fn adjacent_configuration_at_three_install_locations() {
        for directory in [
            "/fs/sda0/gal-extender",
            "/fs/sdb0/gal-extender",
            "/internal/extensions/gal",
        ] {
            let object = format!("{directory}/libgal_loader.so");
            for (dladdr, preload, expected) in [
                (Some(object.as_str()), None, Found::Dladdr),
                (None, Some(object.as_str()), Found::LdPreload),
            ] {
                let (resolved, found) = resolve(None, dladdr, preload).unwrap();
                assert_eq!(found, expected);
                assert_eq!(resolved, directory);
                assert_eq!(
                    config_path(&resolved),
                    format!("{directory}/gal_loader.conf")
                );
            }
        }
        assert_eq!(config_path("/"), "/gal_loader.conf");
    }

    #[test]
    fn override_wins_unless_empty() {
        let d = "/qnx/bench";
        assert_eq!(
            resolve(Some(d), Some(CAR), Some(CAR)),
            Some((d.to_string(), Found::Override))
        );
        assert_eq!(resolve(Some(""), Some(CAR), None).unwrap().1, Found::Dladdr);
    }

    #[test]
    fn another_directory() {
        let p = "/hd/my stuff/gal/libgal_loader.so";
        assert_eq!(
            resolve(None, Some(p), Some(p)),
            Some(("/hd/my stuff/gal".to_string(), Found::Dladdr))
        );
        assert_eq!(
            resolve(None, Some("/mnt/x//libgal_loader.so"), None)
                .unwrap()
                .0,
            "/mnt/x"
        );
    }

    #[test]
    fn relative_or_missing_falls_through() {
        // dladdr gives a bare name: LD_PRELOAD is next.
        assert_eq!(
            resolve(
                None,
                Some("libgal_loader.so"),
                Some("/a/b/libgal_loader.so")
            ),
            Some(("/a/b".to_string(), Found::LdPreload))
        );
        assert_eq!(
            resolve(None, Some("./libgal_loader.so"), Some("libgal_loader.so")),
            None
        );
        assert_eq!(resolve(None, None, None), None);
        assert_eq!(resolve(Some("relative"), None, None), None);
        assert_eq!(
            resolve(None, Some("/libgal_loader.so"), None),
            Some(("/".to_string(), Found::Dladdr))
        );
    }

    #[test]
    fn preload_lists() {
        assert_eq!(
            preload_entry("/x/libfake.so:/y/libgal_loader.so"),
            Some("/y/libgal_loader.so")
        );
        assert_eq!(
            preload_entry("/y/libgal_loader_custom.so /x/libfake.so"),
            Some("/y/libgal_loader_custom.so")
        );
        assert_eq!(preload_entry("/z/renamed.so"), Some("/z/renamed.so"));
        assert_eq!(preload_entry("/x/a.so:/y/b.so"), None);
        assert_eq!(preload_entry(""), None);
        assert_eq!(preload_entry(" : "), None);
    }
}
