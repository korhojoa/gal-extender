// SPDX-License-Identifier: GPL-3.0-only
//! Selected-phone lease and protocol state, with no native pointer or I/O.
use crate::{
    engine::{Engine, EngineError},
    policy_input::Admission,
};
use btsvc_core::{
    aawg::Outcome,
    wireless_policy::{Address, Policy},
};

#[derive(Default)]
pub struct Session {
    engine: Option<Engine>,
    grant: Option<(Address, u64)>,
    pub passed: bool,
    pub id: u64,
    clear_pending: bool,
}

impl Session {
    #[must_use]
    pub fn grant(&self) -> Option<(Address, u64)> {
        self.grant
    }

    #[must_use]
    pub fn retains(&self, policy: Option<&Policy>) -> bool {
        self.grant.is_some_and(|(address, epoch)| {
            policy.is_some_and(|p| p.epoch == epoch && p.accepts_bluetooth(&address))
        })
    }

    pub fn admit(&mut self, admission: Admission, address: Address, now: u64) -> bool {
        if !admission.accepts(address) {
            return false;
        }
        self.engine = Some(Engine::new(admission.config));
        self.grant = Some((address, admission.policy.epoch));
        self.id = now.max(1);
        self.passed = false;
        self.clear_pending = true;
        true
    }

    pub fn revoke(&mut self) {
        self.engine = None;
        self.grant = None;
        self.passed = false;
        self.clear_pending = true;
    }

    pub fn take_clear(&mut self) -> bool {
        std::mem::take(&mut self.clear_pending)
    }

    /// # Errors
    /// Rejects ungranted peers and invalid protocol generations/configuration.
    pub fn on_open(&mut self, generation: u64) -> Result<(), EngineError> {
        self.engine
            .as_mut()
            .ok_or(EngineError::NotOpen)?
            .on_open(generation)
    }
    /// # Errors
    /// Rejects ungranted or stale protocol generations.
    pub fn on_close(&mut self, generation: u64) -> Result<(), EngineError> {
        let result = self
            .engine
            .as_mut()
            .ok_or(EngineError::NotOpen)?
            .on_close(generation);
        if !self.passed {
            self.revoke();
        }
        result
    }
    /// # Errors
    /// Rejects ungranted/stale sessions and invalid or oversized protocol input.
    pub fn on_data(&mut self, generation: u64, bytes: &[u8]) -> Result<Outcome, EngineError> {
        let outcome = self
            .engine
            .as_mut()
            .ok_or(EngineError::NotOpen)?
            .on_data(generation, bytes)?;
        if outcome == Outcome::Pass {
            self.passed = true;
        } else if outcome == Outcome::Fail {
            self.revoke();
        }
        Ok(outcome)
    }
    /// # Errors
    /// Rejects ungranted/stale sessions and invalid native frame sizes.
    pub fn next_tx(
        &mut self,
        generation: u64,
        size: usize,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        self.engine
            .as_mut()
            .ok_or(EngineError::NotOpen)?
            .next_tx(generation, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_input::Snapshot;
    const ADDRESS: Address = [1, 35, 69, 103, 137, 171];
    const POLICY: &str = "version::1\nrevision::2\nepoch::3\nupdated_ms::1000\nenabled::1\nselected::01:23:45:67:89:AB\nallow::01:23:45:67:89:AB\nend::1\n";
    fn admission() -> Admission {
        Snapshot::new(
            Some(POLICY.into()),
            Some(
                "@creds\nip::198.51.100.1\nport:n:5288\nssid::ap\nkey::password\nbssid::02:00:00:00:00:01\n"
                    .into(),
            ),
        )
        .admission(1001)
        .unwrap()
    }
    #[test]
    fn denied_peer_does_not_install_credentials_or_grant() {
        let mut session = Session::default();
        assert!(!session.admit(admission(), [0; 6], 1001));
        assert!(session.grant().is_none());
        assert_eq!(session.on_open(1), Err(EngineError::NotOpen));
    }
    #[test]
    fn current_epoch_is_required_and_revocation_discards_protocol() {
        let mut session = Session::default();
        assert!(session.admit(admission(), ADDRESS, 1001));
        assert!(session.retains(Some(&admission().policy)));
        session.on_open(1).unwrap();
        assert!(session.next_tx(1, 512).unwrap().is_some());
        let changed = crate::policy::parse(&POLICY.replace("epoch::3", "epoch::4"), 1001).unwrap();
        assert!(!session.retains(Some(&changed)));
        assert!(!session.retains(None));
        session.revoke();
        assert!(session.grant().is_none());
        assert_eq!(session.next_tx(1, 512), Err(EngineError::NotOpen));
    }
    #[test]
    fn unsuccessful_rfcomm_close_revokes_but_successful_close_retains_live_grant() {
        let mut failed = Session::default();
        assert!(failed.admit(admission(), ADDRESS, 1001));
        assert!(failed.take_clear());
        failed.on_open(1).unwrap();
        failed.on_close(1).unwrap();
        assert!(failed.grant().is_none());
        assert!(failed.take_clear());
        let mut passed = Session::default();
        assert!(passed.admit(admission(), ADDRESS, 1001));
        passed.on_open(1).unwrap();
        // The protocol PASS path sets this. this test isolates close policy.
        passed.passed = true;
        let _ = passed.take_clear();
        passed.on_close(1).unwrap();
        assert!(passed.retains(Some(&admission().policy)));
        assert!(!passed.take_clear());
    }
}
