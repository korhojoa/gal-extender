// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 korhojoa

//! Protobuf field readers and writers for the GAL message paths.
//!
//! [`varint`] reads the base-128 integer primitive. [`wire`] reads fields and
//! patches values in place. [`write`] emits tags, varints and length-delimited
//! fields into a caller-provided buffer.
//!
//! # The malformed-input policy, defined once
//!
//! Nothing here panics and nothing here guesses. Every reader returns `None`
//! the moment the bytes stop making sense:
//!
//! - a varint that is truncated or wider than 10 bytes;
//! - a field whose value would run past the bound it was read under;
//! - a group (wire types 3 and 4) or an undefined wire type (6, 7);
//! - a length or field number that does not fit the target's `usize`/`u32`.
//!
//! What a *caller* does with that `None` is the caller's policy: the splice
//! treats it as "leave the SDR untouched", the density patch as "stop the
//! walk here, keep what was patched so far", the EV patch as "this field is
//! absent". The primitives never decide that for them, which is what lets one
//! reader serve all three without changing any of their bytes.
//!
//! # 32-bit targets
//!
//! `usize` is 64 bits on the host the tests run on and **32 bits** on the
//! `ARMv7` QNX target. A wire length is a `u64` varint, so
//! `as usize` behaves differently on the two: a length above 4 `GiB` is rejected
//! on the host by the bounds check, but on target it truncates to a small,
//! plausible, in-bounds number and is accepted. [`wire::as_offset`] folds the
//! overlong length into `None` so both targets agree. Every length in this
//! crate goes through it. do not bypass it with a cast.
//!
//! # Discipline
//!
//! `#![no_std]`, no `alloc`, no dependencies, `#![forbid(unsafe_code)]`.
//! Every slice access is `get`/`get_mut`, every offset computation is
//! `checked_*` or `saturating_*`, so the compiled object has no reachable
//! panic path from those operations.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod varint;
pub mod wire;
pub mod write;
