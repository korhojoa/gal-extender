// SPDX-License-Identifier: GPL-3.0-only
//! Startup requests never read native objects from an external callback.
use std::sync::atomic::{AtomicBool, Ordering};

#[must_use]
pub fn request(requested: &AtomicBool, verified: bool, stopped: bool, ready: bool) -> i32 {
    if !verified || stopped {
        return -1;
    }
    if requested.swap(true, Ordering::AcqRel) {
        2
    } else {
        i32::from(!ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn early_native_trigger_is_retained_for_worker_readiness_retry() {
        let requested = AtomicBool::new(false);
        assert_eq!(request(&requested, true, false, false), 1);
        assert!(requested.load(Ordering::Acquire));
        assert_eq!(request(&requested, true, false, true), 2);
    }
    #[test]
    fn disabled_identity_and_stop_never_arm_a_new_request() {
        let requested = AtomicBool::new(false);
        assert_eq!(request(&requested, false, false, true), -1);
        assert_eq!(request(&requested, true, true, true), -1);
        assert!(!requested.load(Ordering::Acquire));
        assert_eq!(request(&requested, true, false, true), 0);
    }
}
