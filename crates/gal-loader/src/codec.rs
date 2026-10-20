// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The media audio sink's codec, chosen by `media_audio_codec` in
//! `gal_loader.conf` instead of gal.json. PCM stays the default and
//! the codec of the guidance and system sinks. AAC-LC is the allowed
//! alternative for media only, and needs far less of the link than 48 kHz
//! 16-bit stereo PCM (1536 kbit/s).
//!
//! HOW. gal keeps its gal.json settings in a table of 219 entries of 32
//! bytes (`0x0023_5978`, .bss), built by a static constructor. An entry is read
//! lazily: on the first getter call gal reads gal.json for that key, stores
//! the value and sets the entry's "resolved" byte. later calls return the
//! stored value. Entry 79 is `audio.sink.media.codecType`, read once by the
//! audio sink factory (0x1b2584). The constructor registers the table's
//! destructor (`0x0015_dcac`) with `__aeabi_atexit` as its last step, so that
//! call is the moment the table exists and nothing has read it yet. This
//! module interposes `__aeabi_atexit`, forwards every call unchanged, and on
//! that one call fills entry 79 as resolved with the configured codec.
//! Patching gal's code is not possible: the unit refuses mprotect on gal's
//! segments.

use core::ffi::{c_int, c_void, CStr};
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::{guard, ldr_log, ldr_loge};
use qnx_dl::NextFn;

type P = *mut c_void;

/// The configured media codec: 0 keeps gal.json's value (PCM). 2 AAC-LC,
/// 4 AAC-LC ADTS (AA `MediaCodecType`). Set by `loader_init`.
static MEDIA_CODEC: AtomicI32 = AtomicI32::new(0);
/// Set when gal has built its table.
static TABLE_SEEN: AtomicBool = AtomicBool::new(false);

/// gal's config table and the layout of entry 79 (gal 0x15e390, the static
/// constructor. 0x15dce8, the lazy reader. 0x185638, the int value).
const TABLE: usize = 0x0023_5978;
const TABLE_DTOR: usize = 0x0015_dcac;
const ENTRY_SIZE: usize = 32;
const MEDIA_CODEC_ID: usize = 79;
/// The entry's key path: 4 pointers to "audio", "sink", "media", "codecType".
const MEDIA_CODEC_PATH: usize = 0x0023_485c;
/// The int value object's type tag.
const VALUE_INT: u32 = 3;

fn codec_name(c: i32) -> &'static str {
    match c {
        1 => "AUDIO_PCM",
        2 => "AUDIO_AAC_LC",
        4 => "AUDIO_AAC_LC_ADTS",
        _ => "unknown",
    }
}

/// Take conf `media_audio_codec` (`loader_init`). Values other than 0, 2 and
/// 4 are refused and logged.
pub fn configure(v: i32) {
    if !matches!(v, 0 | 2 | 4) {
        ldr_loge!(
            "[GalLoader][AUDIO] media_audio_codec={} is not 0, 2 or 4 - gal.json value (PCM) kept",
            v
        );
        return;
    }
    MEDIA_CODEC.store(v, Ordering::Relaxed);
    if v != 0 && TABLE_SEEN.load(Ordering::Relaxed) {
        ldr_loge!(
            "[GalLoader][AUDIO] gal built its config table before the conf was read - \
             media_audio_codec={} NOT applied",
            v
        );
    }
}

/// The checks before the write: entry 79 is the media codecType key, and its
/// value is still the constructor's int. Returns the entry address.
///
/// # Safety
/// Reads gal's .data/.bss at fixed addresses. gal is non-PIE.
unsafe fn media_entry() -> Result<usize, &'static str> {
    let e = TABLE + MEDIA_CODEC_ID * ENTRY_SIZE;
    let rd = |a: usize| core::ptr::read_volatile(a as *const u32);
    if rd(e + 4) != 4 || rd(e + 8) as usize != MEDIA_CODEC_PATH {
        return Err("entry 79 is not the 4-part key at 0x23485c");
    }
    let key = |i: usize| CStr::from_ptr(rd(MEDIA_CODEC_PATH + 4 * i) as usize as *const _);
    if key(2).to_bytes() != b"media" || key(3).to_bytes() != b"codecType" {
        return Err("entry 79's key is not audio.sink.media.codecType");
    }
    if core::ptr::read_volatile(e as *const u8) != 0 {
        return Err("entry 79 already resolved");
    }
    if rd(e + 16) != VALUE_INT || rd(e + 20) != 1 {
        return Err("entry 79's value is not an int");
    }
    Ok(e)
}

fn apply() {
    TABLE_SEEN.store(true, Ordering::Relaxed);
    let codec = MEDIA_CODEC.load(Ordering::Relaxed);
    if codec == 0 {
        return;
    }
    // SAFETY: gal's own table, checked first. gal is still single-threaded
    // (static construction).
    match unsafe { media_entry() } {
        Ok(e) => {
            // SAFETY: as above. the int value word, then the "from gal.json"
            // and "resolved" bytes (0x15df14, 0x15deec).
            unsafe {
                core::ptr::write_volatile((e + 24) as *mut u32, codec.cast_unsigned());
                core::ptr::write_volatile((e + 1) as *mut u8, 1);
                core::ptr::write_volatile(e as *mut u8, 1);
            }
            ldr_log!(
                "[GalLoader][AUDIO] media sink codecType = {} ({}) from media_audio_codec; \
                 guidance and system audio keep gal.json's PCM",
                codec,
                codec_name(codec)
            );
        }
        Err(why) => ldr_loge!(
            "[GalLoader][AUDIO] media_audio_codec={} NOT applied: {}",
            codec,
            why
        ),
    }
}

static REAL_ATEXIT: NextFn<unsafe extern "C" fn(P, P, P) -> c_int> =
    // SAFETY: int __aeabi_atexit(void* obj, void (*dtor)(void*), void* dso).
    unsafe { NextFn::new(c"__aeabi_atexit") };
static REAL_CXA_ATEXIT: NextFn<unsafe extern "C" fn(P, P, P) -> c_int> =
    // SAFETY: int __cxa_atexit(void (*dtor)(void*), void* obj, void* dso).
    unsafe { NextFn::new(c"__cxa_atexit") };

// Exported by entry.rs as "__aeabi_atexit"
pub extern "C" fn aeabi_atexit(obj: P, dtor: P, dso: P) -> c_int {
    if dtor as usize == TABLE_DTOR {
        let _ = guard("media codec", apply);
    }
    match (REAL_ATEXIT.get(), REAL_CXA_ATEXIT.get()) {
        // SAFETY: the next definitions, with their C signatures. The ARM EABI
        // defines __aeabi_atexit(o, d, h) as __cxa_atexit(d, o, h).
        (Some(f), _) => unsafe { f(obj, dtor, dso) },
        (None, Some(f)) => unsafe { f(dtor, obj, dso) },
        // Not registered: the destructor would only run at process exit.
        (None, None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(codec_name(2), "AUDIO_AAC_LC");
        assert_eq!(codec_name(3), "unknown");
    }

    #[test]
    fn entry_79_offset() {
        // The constructor writes entry 79's path at table + 0x9e8.
        assert_eq!(TABLE + MEDIA_CODEC_ID * ENTRY_SIZE + 8, 0x0023_5978 + 0x9e8);
    }
}
