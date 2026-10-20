// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Shared runtime services for built-in GAL features. Modules use one hook registry,
//! recursive send lock, channel allocator, settings reader, loaded-directory store and
//! session lifecycle. PPS objects carry state between processes. Each
//! `FeatureModule` supplies a name and an
//! initialization function called by the loader.

pub mod channel;
pub mod config;
pub mod hooks;
pub mod location;
pub mod pps;
pub mod send_lock;
pub mod services;
pub mod session;

/// A feature built into `libgal_loader.so`.
#[derive(Clone, Copy)]
pub struct FeatureModule {
    /// The feature name in `gal_loader.conf` (`cluster`, `input`, ...).
    pub name: &'static str,
    /// Called once from the loader's constructor, in the conf order, on the
    /// constructor thread. The loader contains a panic in it.
    pub init: fn(),
}
