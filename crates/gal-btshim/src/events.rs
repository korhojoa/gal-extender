// SPDX-License-Identifier: GPL-3.0-only
//! Allocation-free callback mailbox. Native callbacks never perform I/O.
pub const FRAME: usize = 512;
const CAPACITY: usize = 32;

// The queue below stores events by value in a fixed array, so the frame
// travels inline on purpose: no allocation on the callback path, and `Copy`
// stays available. Boxing the frame would defeat both.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
pub enum Event {
    Incoming(btsvc_core::wireless_policy::Address),
    NudgeReady(u8),
    NudgeOpen,
    NudgeQueryFailed,
    NudgeChannelClosed,
    Open,
    Closing,
    Closed,
    Data { len: usize, bytes: [u8; FRAME] },
    Handled { packet: usize, status: u16 },
}

pub struct Queue {
    entries: [Option<Event>; CAPACITY],
    head: usize,
    len: usize,
}

impl Queue {
    // The original fixed callback mailbox is static and allocation-free.
    #[allow(clippy::large_stack_arrays)]
    pub const fn new() -> Self {
        Self {
            entries: [None; CAPACITY],
            head: 0,
            len: 0,
        }
    }
    // Copying the inline frame preserves callback ownership without allocation.
    #[allow(clippy::large_types_passed_by_value)]
    pub fn push(&mut self, event: Event) -> bool {
        if self.len == CAPACITY {
            return false;
        }
        self.entries[(self.head + self.len) % CAPACITY] = Some(event);
        self.len += 1;
        true
    }
    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.entries[self.head].take();
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_mailbox_never_overwrites_completion_and_wraps() {
        let mut q = Queue::new();
        for packet in 0..CAPACITY {
            assert!(q.push(Event::Handled { packet, status: 0 }));
        }
        assert!(!q.push(Event::Closed));
        for expected in 0..CAPACITY {
            assert!(matches!(q.pop(), Some(Event::Handled {packet, ..}) if packet == expected));
        }
        assert!(q.pop().is_none());
        assert!(q.push(Event::Open));
        assert!(matches!(q.pop(), Some(Event::Open)));
    }
}
