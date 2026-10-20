// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Loader-owned channel ID allocation. One mutex protects the registry. Log after
//! unlocking. Native exports and Rust module calls use this same registry.

use core::fmt::Write;
use std::sync::Mutex;

use qnx_slog::log::Line;
use qnx_slog::{ldr_log, ldr_loge};

/// "No channel" / "no preference" / "do not scan".
pub const NONE: u8 = 0xFF;
/// Lowest allocatable id (0 is the AA control channel).
pub const MIN: u8 = 0x01;
/// Highest allocatable id.
pub const MAX: u8 = 0xFE;

/// The first few collisions diagnose the bug. the running total is still in
/// every report.
const COLLISION_LOG_MAX: u32 = 8;

/// Which ids are claimed. Claims are never released.
pub struct Registry {
    claimed: [bool; 256],
    collisions: u32,
}

impl Registry {
    #[must_use]
    pub const fn new() -> Self {
        Registry {
            claimed: [false; 256],
            collisions: 0,
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of one reservation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    Got(u8),
    /// `preferred` was 0 (255 means "no preference").
    PrefBad,
    PrefBusy,
    NoScan,
    Exhausted,
}

/// [`reserve_in`]'s full answer.
pub struct Reserve {
    pub outcome: Outcome,
    /// Set when `preferred` was refused, even if the scan then succeeded.
    pub pref_refused: Option<Outcome>,
    pub collisions: u32,
}

/// Find-and-take on an already locked registry. Pure: no logging.
pub fn reserve_in(reg: &mut Registry, preferred: u8, start: u8) -> Reserve {
    let mut refused = None;
    if preferred != NONE {
        if !(MIN..=MAX).contains(&preferred) {
            refused = Some(Outcome::PrefBad);
            reg.collisions = reg.collisions.wrapping_add(1);
        } else if reg.claimed[usize::from(preferred)] {
            refused = Some(Outcome::PrefBusy);
            reg.collisions = reg.collisions.wrapping_add(1);
        } else {
            reg.claimed[usize::from(preferred)] = true;
            return Reserve {
                outcome: Outcome::Got(preferred),
                pref_refused: None,
                collisions: reg.collisions,
            };
        }
    }
    let outcome = if start == NONE {
        None
    } else {
        (start.max(MIN)..=MAX)
            .find(|&i| !reg.claimed[usize::from(i)])
            .map(|i| {
                reg.claimed[usize::from(i)] = true;
                Outcome::Got(i)
            })
    };
    let outcome = match (outcome, &refused) {
        (Some(o), _) => o,
        (None, Some(Outcome::PrefBad)) => Outcome::PrefBad,
        (None, Some(_)) => Outcome::PrefBusy,
        (None, None) if start == NONE => Outcome::NoScan,
        (None, None) => Outcome::Exhausted,
    };
    Reserve {
        outcome,
        pref_refused: refused,
        collisions: reg.collisions,
    }
}

/// The log text for a failed outcome.
#[must_use]
pub fn why(o: Outcome) -> &'static str {
    match o {
        Outcome::Got(_) => "",
        Outcome::PrefBad => "id 0 is the AA control channel and is never allocatable",
        Outcome::PrefBusy => "requested id is ALREADY CLAIMED by another feature",
        Outcome::NoScan => "no preferred id given and scanning was not requested",
        Outcome::Exhausted => "no free channel id left (registry exhausted)",
    }
}

static REG: Mutex<Registry> = Mutex::new(Registry::new());

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    REG.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Returns the reserved id, or the reason it failed. Logs a refused
/// `preferred` at ERROR when an explicit claim collides with an existing one.
fn reserve_core(preferred: u8, start: u8) -> Result<u8, &'static str> {
    let r = reserve_in(&mut lock(), preferred, start);
    if let Some(ref_o) = r.pref_refused {
        if r.collisions <= COLLISION_LOG_MAX {
            ldr_loge!(
                "[GalLoader] channel {} REFUSED: {} (collision #{})",
                preferred,
                why(ref_o),
                r.collisions
            );
        }
    }
    match r.outcome {
        Outcome::Got(ch) => Ok(ch),
        o => Err(why(o)),
    }
}

/// Reserve `preferred` if free, else the lowest free id from `start` (255 =
/// do not scan).
#[must_use]
pub fn reserve(preferred: u8, start: u8) -> Option<u8> {
    let r = match reserve_core(preferred, start) {
        Ok(ch) => {
            ldr_log!(
                "[GalLoader] channel {} RESERVED (preferred={} scan_from={})",
                ch,
                preferred,
                start
            );
            Some(ch)
        }
        Err(why) => {
            ldr_loge!(
                "[GalLoader] channel reserve FAILED (preferred={} scan_from={}): {}",
                preferred,
                start,
                why
            );
            None
        }
    };

    r
}

/// Reserve exactly `ch`.
#[must_use]
pub fn reserve_exact(ch: u8) -> bool {
    let ok = match reserve_core(ch, NONE) {
        Ok(_) => {
            ldr_log!("[GalLoader] channel {} RESERVED (exact)", ch);
            true
        }
        Err(why) => {
            ldr_loge!("[GalLoader] channel {} reserve FAILED (exact): {}", ch, why);
            false
        }
    };

    ok
}

/// Reserve the lowest free id from `start`.
#[must_use]
pub fn reserve_lowest(start: u8) -> Option<u8> {
    let r = match reserve_core(NONE, start) {
        Ok(ch) => {
            ldr_log!(
                "[GalLoader] channel {} RESERVED (lowest free from {})",
                ch,
                start
            );
            Some(ch)
        }
        Err(why) => {
            ldr_loge!(
                "[GalLoader] channel reserve FAILED (lowest free from {}): {}",
                start,
                why
            );
            None
        }
    };

    r
}

/// True when `ch` is claimed.
#[must_use]
pub fn claimed(ch: u8) -> bool {
    lock().claimed[usize::from(ch)]
}

/// "CHANNELS at <what>: reserved=[...]" into the loader log.
pub fn report(what: &str) {
    let mut list: Line<300> = Line::new();
    let (count, collisions) = {
        let reg = lock();
        let mut count = 0;
        for (i, _) in reg.claimed.iter().enumerate().filter(|(_, &c)| c) {
            count += 1;
            let _ = write!(list, "{}{}", if count > 1 { "," } else { "" }, i);
        }
        (count, reg.collisions)
    };
    ldr_log!(
        "[GalLoader] CHANNELS at {}: reserved=[{}] ({}) collisions={} \
         (allocatable {}-{}; 0=control, 255='none'; claims are never released)",
        what,
        list.as_str(),
        count,
        collisions,
        MIN,
        MAX
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_then_busy_then_scan() {
        let mut r = Registry::new();
        assert_eq!(reserve_in(&mut r, 6, NONE).outcome, Outcome::Got(6));
        let x = reserve_in(&mut r, 6, NONE);
        assert_eq!(x.outcome, Outcome::PrefBusy);
        assert_eq!(x.collisions, 1);
        let y = reserve_in(&mut r, 6, 6);
        assert_eq!(y.outcome, Outcome::Got(7));
        assert_eq!(y.pref_refused, Some(Outcome::PrefBusy));
        assert_eq!(y.collisions, 2);
    }

    #[test]
    fn zero_is_refused_and_scan_clamps() {
        let mut r = Registry::new();
        assert_eq!(reserve_in(&mut r, 0, NONE).outcome, Outcome::PrefBad);
        assert_eq!(reserve_in(&mut r, NONE, 0).outcome, Outcome::Got(1));
        assert_eq!(reserve_in(&mut r, NONE, NONE).outcome, Outcome::NoScan);
    }

    #[test]
    fn exhaustion() {
        let mut r = Registry::new();
        for i in MIN..=MAX {
            assert_eq!(reserve_in(&mut r, NONE, 1).outcome, Outcome::Got(i));
        }
        assert_eq!(reserve_in(&mut r, NONE, 1).outcome, Outcome::Exhausted);
        assert!(!r.claimed[0] && !r.claimed[255]);
    }
}
