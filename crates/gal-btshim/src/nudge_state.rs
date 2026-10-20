// SPDX-License-Identifier: GPL-3.0-only
//! Query completion cannot release a native RFCOMM channel still pending.
#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Query,
    Channel,
}
#[derive(Default)]
pub(crate) struct State {
    phase: Phase,
}
impl State {
    pub(crate) fn begin(&mut self) -> bool {
        if self.phase != Phase::Idle {
            return false;
        }
        self.phase = Phase::Query;
        true
    }
    pub(crate) fn query(&self) -> bool {
        self.phase == Phase::Query
    }
    pub(crate) fn channel(&self) -> bool {
        self.phase == Phase::Channel
    }
    pub(crate) fn ready(&mut self) {
        if self.phase == Phase::Query {
            self.phase = Phase::Channel;
        }
    }
    pub(crate) fn query_failed(&mut self) {
        if self.phase == Phase::Query {
            self.phase = Phase::Idle;
        }
    }
    pub(crate) fn channel_closed(&mut self) {
        if self.phase == Phase::Channel {
            self.phase = Phase::Idle;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Queue};
    #[test]
    fn queued_query_failure_after_ready_cannot_release_pending_hsp_channel() {
        let mut state = State::default();
        assert!(state.begin());
        let mut queue = Queue::new();
        assert!(queue.push(Event::NudgeReady(2)));
        assert!(queue.push(Event::NudgeQueryFailed));
        while let Some(event) = queue.pop() {
            match event {
                Event::NudgeReady(_) => state.ready(),
                Event::NudgeQueryFailed => state.query_failed(),
                _ => panic!("unexpected fixture event"),
            }
        }
        assert!(state.channel());
        assert!(!state.begin());
        state.channel_closed();
        assert!(state.begin());
    }
    #[test]
    fn channel_closed_cannot_release_a_new_query() {
        let mut state = State::default();
        assert!(state.begin());
        state.channel_closed();
        assert!(state.query());
        state.query_failed();
        assert!(state.begin());
    }
}
