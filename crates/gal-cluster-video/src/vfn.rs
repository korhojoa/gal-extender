// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Generation-bound unsolicited `VideoFocusNotification`. Each accepted setup request
//! advances the generation. Workers check that generation at every step and pin a send
//! under the same lock as the check. Begin and invalidate replace the generation before
//! a bounded wait for pinned sends. The lock is a leaf: no callout occurs while held
//! except the condition wait, which releases it. Logging occurs after unlock. MOST may
//! precede this lock. never take MOST while holding it. The wait uses the monotonic
//! clock.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::log::{cl_log, cl_loge};

/// The bound on waiting for a pinned send (`VFN_SEND_DRAIN_MS`).
pub const SEND_DRAIN_MS: u64 = 1500;

struct State {
    generation: u32,
    pending: bool,
    ready: bool,
    restart_ch: Option<u8>,
    ch: u8,
    stale: u32,
    send_inflight: u32,
    drain_timeouts: u32,
}

static STATE: Mutex<State> = Mutex::new(State {
    generation: 0,
    pending: false,
    ready: false,
    restart_ch: None,
    ch: 0xFF,
    stale: 0,
    send_inflight: 0,
    drain_timeouts: 0,
});
static IDLE: Condvar = Condvar::new();

fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a worker was started for (`struct vfn_ctx`).
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub ch: u8,
    pub gen: u32,
}

/// `vfn_wait_sends_idle_locked`: true when no pinned send is left. false
/// on the timeout (counted).
fn wait_idle(mut g: MutexGuard<'static, State>) -> (MutexGuard<'static, State>, bool) {
    if g.send_inflight == 0 {
        return (g, true);
    }
    let (g2, r) = IDLE
        .wait_timeout_while(g, Duration::from_millis(SEND_DRAIN_MS), |s| {
            s.send_inflight > 0
        })
        .unwrap_or_else(PoisonError::into_inner);
    g = g2;
    if r.timed_out() && g.send_inflight > 0 {
        g.drain_timeouts += 1;
        return (g, false);
    }
    (g, true)
}

/// `vfn_send_unpin`.
pub fn unpin() {
    let mut g = lock();
    if g.send_inflight > 0 {
        g.send_inflight -= 1;
    }
    if g.send_inflight == 0 {
        IDLE.notify_all();
    }
}

/// `vfn_send_pin`: pin a send with no ownership check.
pub fn pin() {
    lock().send_inflight += 1;
}

fn current(g: &State, c: Ctx) -> bool {
    c.gen != 0 && c.gen == g.generation && c.ch == g.ch
}

/// `vfn_worker_still_current`.
pub fn still_current(c: Ctx) -> bool {
    current(&lock(), c)
}

/// `vfn_pending_pin_send`: check and pin in one critical section.
pub fn pin_if_current(c: Ctx) -> bool {
    let mut g = lock();
    let ok = current(&g, c);
    if ok {
        g.send_inflight += 1;
    }
    ok
}

fn next_generation(g: &mut State) {
    g.generation = g.generation.wrapping_add(1);
    if g.generation == 0 {
        g.generation = 1;
    }
}

/// `vfn_pending_begin`: a fresh non-zero generation for `ch`.
pub fn begin(ch: u8) -> u32 {
    let mut g = lock();
    let prev_gen = g.generation;
    let prev_ch = g.ch;
    let superseded = g.pending;
    next_generation(&mut g);
    g.ch = ch;
    g.pending = true;
    g.ready = false;
    g.restart_ch = None;
    let gen = g.generation;
    let (g, drained) = wait_idle(g);
    let timeouts = g.drain_timeouts;
    drop(g);
    if superseded {
        cl_log!(
            "[GalCluster] AVChannelSetupRequest ch={} supersedes the pending unsolicited VideoFocusNotification(PROJECTED) from generation {} (ch={}) \u{2014} new generation {}; only the newer worker may send",
            ch,
            prev_gen,
            prev_ch,
            gen
        );
    }
    if !drained {
        cl_loge!(
            "[GalCluster] AVChannelSetupRequest ch={}: a VideoFocusNotification send was still in flight after {}ms \u{2014} proceeding anyway (drain timeouts={}); gal's outbound path may be wedged",
            ch,
            SEND_DRAIN_MS,
            timeouts
        );
    }
    gen
}

/// Admit AA frames only after the current worker proves handover readiness.
pub fn admit(c: Ctx) -> bool {
    let mut g = lock();
    if !current(&g, c) {
        return false;
    }
    g.ready = true;
    true
}

/// Early or stale AA frames cannot issue the initial native hide.
pub fn ready_for(ch: u8) -> bool {
    let g = lock();
    g.ready && g.ch == ch
}

/// `vfn_pending_clear`: clear the flag only while `gen`/`ch` are current.
pub fn clear(gen: u32, ch: u8) -> bool {
    let mut g = lock();
    let owned = gen != 0 && gen == g.generation && ch == g.ch;
    if owned {
        g.pending = false;
    }
    owned
}

/// `vfn_pending_invalidate`.
pub fn invalidate(why: &str) {
    let _ = invalidate_with_restart(why, None);
}

/// Stop cancels workers and frame admission. Only an already admitted,
/// requested same-channel recovery may retain permission to retry the pipe.
pub fn stop(ch: u8, expected_restart: bool) -> bool {
    invalidate_with_restart(
        "phone sent AVChannelStopIndication",
        expected_restart.then_some(ch),
    )
}

/// Consume the retained permission once. The pipe still requires current
/// encoder-DOWN evidence before open, and again before the first write.
pub fn restart(ch: u8, expected_restart: bool) -> bool {
    let mut g = lock();
    let resume = g.restart_ch.take() == Some(ch) && expected_restart;
    if resume {
        g.ch = ch;
        g.ready = true;
    }
    resume
}

fn invalidate_with_restart(why: &str, restart: Option<u8>) -> bool {
    let mut g = lock();
    let gen = g.generation;
    let ch = g.ch;
    let was_pending = g.pending;
    g.restart_ch = restart.filter(|ch| g.ready && g.ch == *ch);
    let retained = g.restart_ch.is_some();
    next_generation(&mut g);
    g.ready = false;
    g.ch = 0xFF;
    g.pending = false;
    let (g, drained) = wait_idle(g);
    let timeouts = g.drain_timeouts;
    drop(g);
    if was_pending {
        cl_log!(
            "[GalCluster] generation {} (ch={}) superseded: {} \u{2014} the pending unsolicited VideoFocusNotification(PROJECTED) will not be sent",
            gen,
            ch,
            why
        );
    }
    if !drained {
        cl_loge!(
            "[GalCluster] teardown ({}): a VideoFocusNotification send was still in flight after {}ms \u{2014} returning anyway (drain timeouts={}); gal's outbound path may be wedged",
            why,
            SEND_DRAIN_MS,
            timeouts
        );
    }
    retained
}

/// `vfn_worker_cancel`: count and log a superseded worker.
pub fn worker_cancel(c: Ctx, at: &str) {
    let n = {
        let mut g = lock();
        g.stale += 1;
        g.stale
    };
    cl_log!(
        "[GalCluster] screen-ready worker: generation {} (ch={}) is no longer current at {} \u{2014} cancelled, NO VideoFocusNotification sent (stale_cancels={})",
        c.gen,
        c.ch,
        at,
        n
    );
}

/// `vfn_pending_current`: the generation and its channel.
pub fn current_generation() -> (u32, u8) {
    let g = lock();
    (g.generation, g.ch)
}

/// `vfn_pending_armed`.
pub fn armed() -> bool {
    lock().pending
}

/// `vfn_stale_count`.
#[cfg(test)]
pub fn stale_count() -> u32 {
    lock().stale
}

#[cfg(test)]
pub fn reset_for_test() {
    let mut g = lock();
    *g = State {
        generation: 0,
        pending: false,
        ready: false,
        restart_ch: None,
        ch: 0xFF,
        stale: 0,
        send_inflight: 0,
        drain_timeouts: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "nto"))]
    fn transport_closes_owned_output() {
        // A transport failure can arrive without AV_STOP or channel close.
        // Host libc stubs make this a synthetic descriptor, not a real device.
        use core::sync::atomic::Ordering;
        let gen = begin(6);
        assert!(admit(Ctx { ch: 6, gen }));
        assert!(clear(gen, 6));
        crate::route::STARTED.store(true, Ordering::Release);
        crate::pipe::FK_EXPECT_RESTART.store(true, Ordering::Release);
        crate::pipe::MOST_FD.store(42, Ordering::Release);
        crate::route::hook_transport_read(core::ptr::null_mut(), core::ptr::null(), 64, 0);
        assert_eq!(
            crate::pipe::MOST_FD.load(Ordering::Acquire),
            -1,
            "transport teardown must close the owned raw output before native return"
        );
        assert!(!crate::route::started());
        assert!(!crate::pipe::FK_EXPECT_RESTART.load(Ordering::Acquire));
        assert!(!ready_for(6));
        // A repeated failed read is harmless after the pipe closed.
        crate::route::hook_transport_read(core::ptr::null_mut(), core::ptr::null(), 64, -1);
        assert_eq!(crate::pipe::MOST_FD.load(Ordering::Acquire), -1);

        // Teardown must wait for an already active writer before closing
        // its descriptor and reaching most_close's native restore call.
        let gen = begin(6);
        assert!(admit(Ctx { ch: 6, gen }));
        assert!(clear(gen, 6));
        crate::pipe::MOST_FD.store(43, Ordering::Release);
        let release_writer = crate::pipe::hold_writer_for_test();
        let (finished, result) = std::sync::mpsc::channel();
        let teardown = std::thread::spawn(move || {
            crate::route::hook_transport_read(core::ptr::null_mut(), core::ptr::null(), 64, 0);
            let _ = finished.send(());
        });
        let deadline = std::time::Instant::now();
        while ready_for(6) && deadline.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!ready_for(6), "teardown must revoke frame admission first");
        assert_eq!(crate::pipe::MOST_FD.load(Ordering::Acquire), 43);
        assert!(matches!(
            result.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_writer.send(()).unwrap();
        result.recv_timeout(Duration::from_secs(1)).unwrap();
        teardown.join().unwrap();
        assert_eq!(crate::pipe::MOST_FD.load(Ordering::Acquire), -1);
        assert!(!crate::pipe::WQ_RUNNING.load(Ordering::Acquire));
    }

    /// The rules, in one test (the state is process-wide).
    #[test]
    fn supersede_pin_and_drain() {
        reset_for_test();
        let g1 = begin(6);
        assert_ne!(g1, 0);
        assert!(armed());
        let c1 = Ctx { ch: 6, gen: g1 };
        assert!(still_current(c1));
        assert!(!ready_for(6));
        assert!(admit(c1));
        assert!(ready_for(6));
        // a repeated setup request supersedes
        let g2 = begin(6);
        assert_ne!(g1, g2);
        assert!(!still_current(c1));
        assert!(!ready_for(6));
        assert!(!admit(c1));
        assert!(!clear(g1, 6), "an old worker cannot clear the new flag");
        let c2 = Ctx { ch: 6, gen: g2 };
        assert!(pin_if_current(c2));
        // a teardown waits for the pinned send: unpin from another thread
        let t = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            unpin();
        });
        let t0 = std::time::Instant::now();
        invalidate("test");
        assert!(t0.elapsed() >= Duration::from_millis(40));
        t.join().unwrap();
        assert!(!armed());
        assert!(!ready_for(6));
        assert!(!admit(c2));
        assert!(!pin_if_current(c2));
        worker_cancel(c2, "test");
        assert_eq!(stale_count(), 1);
        assert_eq!(current_generation().1, 0xFF);
        // Exercise the production route handlers. A stopped channel cannot
        // submit frames, and an admitted requested restart can retry opening.
        let gen = begin(6);
        let admitted = Ctx { ch: 6, gen };
        assert!(admit(admitted));
        assert!(clear(gen, 6));
        crate::pipe::FK_EXPECT_RESTART.store(true, core::sync::atomic::Ordering::Release);
        crate::route::test_channel_message(6, crate::sdr::AV_STOP_INDICATION);
        assert!(!ready_for(6));
        assert!(!admit(admitted), "Stop cancels old handover workers");
        crate::route::test_channel_message(6, crate::sdr::AV_START_INDICATION);
        assert!(ready_for(6), "restarted channel must reach most_open");
        assert!(!restart(6, true), "restart permission is consumed once");

        // Wrong-channel starts, transport teardown and new setup requests
        // cannot reuse a previous handover. Early recovery cannot admit it.
        for action in 0..5 {
            let gen = begin(6);
            if action != 4 {
                assert!(admit(Ctx { ch: 6, gen }));
                assert!(clear(gen, 6));
            }
            crate::pipe::FK_EXPECT_RESTART
                .store(action != 3, core::sync::atomic::Ordering::Release);
            crate::route::test_channel_message(6, crate::sdr::AV_STOP_INDICATION);
            match action {
                0 => crate::route::test_channel_message(7, crate::sdr::AV_START_INDICATION),
                1 => invalidate("AA transport read ended"),
                2 => {
                    begin(6);
                }
                _ => {}
            }
            crate::route::test_channel_message(6, crate::sdr::AV_START_INDICATION);
            assert!(!ready_for(6), "invalid recovery case {action}");
        }
        #[cfg(not(target_os = "nto"))]
        transport_closes_owned_output();
        invalidate("test cleanup");
    }
}
