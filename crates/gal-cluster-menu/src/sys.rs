// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The libc calls the ui feature makes directly (the remount around a save).

use core::ffi::{c_char, c_int};

extern "C" {
    pub fn system(cmd: *const c_char) -> c_int;
    pub fn sync();
}
