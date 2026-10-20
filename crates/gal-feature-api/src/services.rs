// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Actions the loader performs on gal for a feature. Their bodies need the
//! loader's interposer state (the captured `VideoSink`, the USBDI connection),
//! so the loader installs them here at construction.

use std::sync::OnceLock;

/// The loader's implementations.
#[derive(Clone, Copy)]
pub struct LoaderServices {
    /// gal's real setVideoFocus on the captured sink. false when no sink yet.
    pub video_request_focus: fn(mode: i32, unsolicited: i32) -> bool,
    /// The synthetic accessory insertion returns the loader's result.
    pub usbdi_fire_insertion: fn(vendor: u32, product: u32) -> i32,
}

static SERVICES: OnceLock<LoaderServices> = OnceLock::new();

/// Install the loader's implementations (constructor, once).
pub fn install(s: LoaderServices) {
    let _ = SERVICES.set(s);
}

/// Notify the phone of a video focus mode.
#[must_use]
pub fn video_request_focus(mode: i32, unsolicited: i32) -> bool {
    let r = SERVICES
        .get()
        .is_some_and(|s| (s.video_request_focus)(mode, unsolicited));

    r
}

/// Fire a synthetic USB insertion.
#[must_use]
pub fn usbdi_fire_insertion(vendor: u32, product: u32) -> i32 {
    let r = SERVICES
        .get()
        .map_or(0, |s| (s.usbdi_fire_insertion)(vendor, product));

    r
}
