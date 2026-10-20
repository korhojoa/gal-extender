// SPDX-License-Identifier: GPL-3.0-only
//! Monotonic admission for the first native cluster hide.

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Wait,
    Hide,
    Park,
}

pub struct MainHold {
    deadline: u64,
    extra_ms: u64,
    seen: Option<u64>,
}

impl MainHold {
    pub fn new(now: u64, wait_ms: u64, extra_ms: u64) -> Self {
        Self {
            deadline: now.saturating_add(wait_ms),
            extra_ms,
            seen: None,
        }
    }

    pub fn poll(&mut self, now: u64, projected: bool) -> Decision {
        if projected {
            let seen = *self.seen.get_or_insert(now);
            if now.saturating_sub(seen) >= self.extra_ms {
                Decision::Hide
            } else {
                Decision::Wait
            }
        } else if now >= self.deadline {
            Decision::Park
        } else {
            self.seen = None;
            Decision::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_restarts_before_delayed_first_hide() {
        // Main display becomes projected before the native encoder starts.
        for (main, up) in [(209_090, 209_756), (273_130, 274_149), (348_497, 349_444)] {
            let mut hold = MainHold::new(main, 8000, 1500);
            assert_eq!(hold.poll(main, true), Decision::Wait);
            assert_eq!(hold.poll(up, true), Decision::Wait);
            assert_eq!(hold.poll(main + 1500, true), Decision::Hide);
        }
    }

    #[test]
    fn missing_main_focus_parks_at_monotonic_deadline() {
        let mut hold = MainHold::new(100, 8000, 750);
        assert_eq!(hold.poll(8099, false), Decision::Wait);
        assert_eq!(hold.poll(8100, false), Decision::Park);
        assert_eq!(MainHold::new(100, 0, 750).poll(100, false), Decision::Park);
    }

    #[test]
    fn focus_loss_restarts_the_hold() {
        let mut hold = MainHold::new(0, 8000, 750);
        assert_eq!(hold.poll(100, true), Decision::Wait);
        assert_eq!(hold.poll(800, false), Decision::Wait);
        assert_eq!(hold.poll(900, true), Decision::Wait);
        assert_eq!(hold.poll(1649, true), Decision::Wait);
        assert_eq!(hold.poll(1650, true), Decision::Hide);
    }
}
