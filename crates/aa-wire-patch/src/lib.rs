// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! aa-wire-patch -- the byte-exact Android Auto wire code gal-extender applies to the
//! head unit's `ServiceDiscoveryResponse` and sends on the sensor channel.
//!
//! - [`sdr_splice`]: advertise extra sensor types (and the EV energy model)
//!   in the SDR's `sensor_source_service`.
//! - [`sdr_density`]: rewrite the MAIN display's density in place, and find
//!   the highest `display_id` in use.
//! - [`ev_model`]: patch live values into the `EnergyModelControl` template.
//! - [`batch`]: build `SensorEventIndication` messages for the sensor types
//!   the vendor library has no `report*` call for.
//!
//! All protobuf wire mechanics come from `protobuf-wire`. nothing here reads a
//! varint by hand. THE RULE THAT SHAPES THE API: this crate decides *what the
//! bytes are*. the consumer decides *what happens to them*. Inputs come in as
//! slices, results go out into caller-provided buffers, and outcomes are
//! return values, never prints.
//!
//! `#![no_std]`, no `alloc`, `#![forbid(unsafe_code)]`, no reachable panic
//! path from slice and offset operations. Not shared with
//! dio-extender: the AA wire format has no `CarPlay` equivalent.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod batch;
pub mod ev_model;
pub mod nav;
pub mod sdr_density;
pub mod sdr_splice;

pub use protobuf_wire::{varint, wire, write};
