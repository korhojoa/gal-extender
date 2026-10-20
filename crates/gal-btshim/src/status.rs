// SPDX-License-Identifier: GPL-3.0-only
//! Successful policy ACK requires a newly observed published native service.
use std::io;
#[derive(Clone, Copy)]
pub(crate) enum Readiness {
    MissingHook,
    Unready,
    Unregistered,
    Published,
}
pub(crate) fn session_due(passed: bool, id: u64, last_id: u64, periodic: bool) -> bool {
    passed && (id != last_id || periodic)
}
pub(crate) fn acknowledge(
    state: Readiness,
    revision: u64,
    now: u64,
    write: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    if !matches!(state, Readiness::Published) {
        return Ok(());
    }
    write(&format!(
        "revision::{revision}\nupdated_ms::{now}\nend::1\n"
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_native_registration_never_refreshes_old_ack() {
        let old = "revision::2\nupdated_ms::1000\nend::1\n".to_owned();
        let mut stored = old.clone();
        for state in [
            Readiness::MissingHook,
            Readiness::Unready,
            Readiness::Unregistered,
        ] {
            acknowledge(state, 3, 11001, |text| {
                stored = text.to_owned();
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(stored, old);
        acknowledge(Readiness::Published, 3, 11001, |text| {
            stored = text.to_owned();
            Ok(())
        })
        .unwrap();
        assert_eq!(stored, "revision::3\nupdated_ms::11001\nend::1\n");
    }
    #[test]
    fn successful_handshake_is_published_without_waiting_for_periodic_ack() {
        assert!(!session_due(false, 1001, 0, true));
        assert!(session_due(true, 1001, 0, false));
        assert!(!session_due(true, 1001, 1001, false));
        assert!(session_due(true, 1001, 1001, true));
    }
}
