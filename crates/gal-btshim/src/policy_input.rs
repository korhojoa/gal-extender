// SPDX-License-Identifier: GPL-3.0-only
//! Owned bounded messages between PPS I/O and the native worker.
//! No native pointer or native operation crosses this boundary.
use crate::config::Config;
use aawg_creds::Creds;
use btsvc_core::wireless_policy::{Address, Policy};

pub const MAX_PPS: usize = 4096;

#[derive(Default)]
pub struct Snapshot {
    policy: Option<String>,
    credentials: Option<String>,
}

impl Snapshot {
    /// Missing, oversized or malformed input disables admission.
    #[must_use]
    pub fn new(policy: Option<String>, credentials: Option<String>) -> Self {
        Self {
            policy: policy.filter(|value| value.len() <= MAX_PPS),
            credentials: credentials.filter(|value| value.len() <= MAX_PPS),
        }
    }

    /// Reparse against the worker's CURRENT clock, so a stalled I/O producer
    /// cannot extend a previous selected-phone policy lease.
    #[must_use]
    pub fn admission(&self, now: u64) -> Option<Admission> {
        self.admit_credentials(now, self.read_credentials())
    }

    fn read_credentials(&self) -> Creds {
        self.credentials
            .as_deref()
            .and_then(Creds::parse_pps)
            .unwrap_or_default()
    }

    fn admit_credentials(&self, now: u64, creds: Creds) -> Option<Admission> {
        if !creds.complete() {
            return None;
        }
        let policy = crate::policy::parse(self.policy.as_deref()?, now)?;
        let candidate = policy.candidate()?;
        let config = Config {
            ip: creds.ip,
            port: u16::try_from(creds.port).ok()?,
            ssid: creds.ssid,
            key: creds.key,
            bssid: creds.bssid,
        };
        config.validate().ok()?;
        Some(Admission {
            policy,
            candidate,
            config,
        })
    }
}

/// Refuse incomplete credentials and log once per unchanged missing-field set.
#[derive(Default)]
pub struct AdmissionReader {
    missing: Vec<&'static str>,
}

impl AdmissionReader {
    pub fn read(
        &mut self,
        snapshot: &Snapshot,
        now: u64,
        log: &mut impl FnMut(&str),
    ) -> Option<Admission> {
        let creds = snapshot.read_credentials();
        let missing = creds.missing_fields();
        if !missing.is_empty() {
            if missing != self.missing {
                log(&format!(
                    "wireless bootstrap refused: missing connection fields: {}",
                    missing.join(", ")
                ));
            }
            self.missing = missing;
            return None;
        }
        self.missing.clear();
        snapshot.admit_credentials(now, creds)
    }
}

pub struct Admission {
    pub policy: Policy,
    pub candidate: Address,
    pub config: Config,
}

impl Admission {
    #[must_use]
    pub fn accepts(&self, address: Address) -> bool {
        self.policy.accepts_bluetooth(&address)
    }
    #[must_use]
    pub fn retains(&self, address: Address, epoch: u64) -> bool {
        self.policy.epoch == epoch && self.accepts(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const POLICY: &str = "@policy\nversion::1\nrevision::2\nepoch::3\nupdated_ms::1000\nenabled::1\nselected::01:23:45:67:89:AB\nallow::01:23:45:67:89:AB,12:34:56:78:9A:BC\nend::1\n";
    const CREDS: &str = "@creds\nip::198.51.100.1\nport:n:5288\nssid::test-ap\nkey::key with:colon\nbssid::02:00:00:00:00:01\n";
    const SELECTED: Address = [1, 35, 69, 103, 137, 171];

    fn snapshot() -> Snapshot {
        Snapshot::new(Some(POLICY.into()), Some(CREDS.into()))
    }

    #[test]
    fn each_missing_field_refuses_both_messages_and_logs_once() {
        use crate::{engine::EngineError, session::Session};
        for field in ["ip", "port", "ssid", "key", "bssid"] {
            for remove in [false, true] {
                let text = CREDS
                    .lines()
                    .filter_map(|line| {
                        if line.starts_with(&format!("{field}:")) {
                            if remove {
                                None
                            } else {
                                Some(format!(
                                    "{field}::{}",
                                    if field == "port" { "0" } else { "" }
                                ))
                            }
                        } else {
                            Some(line.to_owned())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let partial = Snapshot::new(Some(POLICY.into()), Some(text));
                let mut reader = AdmissionReader::default();
                let mut logs = Vec::new();
                let mut session = Session::default();
                for _ in 0..3 {
                    if let Some(grant) =
                        reader.read(&partial, 1001, &mut |s| logs.push(s.to_owned()))
                    {
                        session.admit(grant, SELECTED, 1001);
                    }
                    assert_eq!(session.on_open(1), Err(EngineError::NotOpen));
                    assert_eq!(session.next_tx(1, 512), Err(EngineError::NotOpen));
                    assert_eq!(session.on_data(1, &[0, 0, 0, 2]), Err(EngineError::NotOpen));
                }
                assert_eq!(
                    logs,
                    [format!(
                        "wireless bootstrap refused: missing connection fields: {field}"
                    )]
                );
            }
        }
    }

    #[test]
    fn missing_field_log_resets_after_recovery_and_never_contains_values() {
        let mut reader = AdmissionReader::default();
        let partial = Snapshot::new(
            Some(POLICY.into()),
            Some(CREDS.replace("ip::198.51.100.1", "ip::")),
        );
        let mut logs = Vec::new();
        assert!(reader
            .read(&partial, 1001, &mut |s| logs.push(s.to_owned()))
            .is_none());
        let grant = reader
            .read(&snapshot(), 1001, &mut |s| logs.push(s.to_owned()))
            .unwrap();
        let mut session = crate::session::Session::default();
        assert!(session.admit(grant, SELECTED, 1001));
        session.on_open(1).unwrap();
        assert!(session.next_tx(1, 512).unwrap().is_some());
        assert!(reader
            .read(&partial, 1001, &mut |s| logs.push(s.to_owned()))
            .is_none());
        assert_eq!(
            logs,
            ["wireless bootstrap refused: missing connection fields: ip"; 2]
        );
        let mut absent = AdmissionReader::default();
        logs.clear();
        for _ in 0..2 {
            assert!(absent
                .read(&Snapshot::default(), 1001, &mut |s| logs.push(s.to_owned()))
                .is_none());
        }
        assert_eq!(
            logs,
            ["wireless bootstrap refused: missing connection fields: ip, port, ssid, key, bssid"]
        );
    }

    #[test]
    fn selected_peer_only_and_native_config_reuses_literal_credentials() {
        let admission = snapshot().admission(1001).unwrap();
        assert!(admission.accepts(SELECTED));
        assert!(!admission.accepts([18, 52, 86, 120, 154, 188]));
        assert_eq!(admission.config.key, "key with:colon");
        assert_eq!(admission.config.port, 5288);
        assert!(admission.retains(SELECTED, 3));
        assert!(!admission.retains(SELECTED, 2));
    }

    #[test]
    fn io_stall_cannot_extend_a_policy_lease() {
        let held = snapshot();
        assert!(held.admission(1001).is_some());
        assert!(held.admission(11001).is_none());
        assert!(held.admission(999).is_none());
    }

    #[test]
    fn failed_or_partial_io_snapshot_never_keeps_an_old_grant() {
        assert!(Snapshot::default().admission(1001).is_none());
        assert!(Snapshot::new(Some(POLICY.into()), None)
            .admission(1001)
            .is_none());
        assert!(Snapshot::new(None, Some(CREDS.into()))
            .admission(1001)
            .is_none());
        assert!(
            Snapshot::new(Some(POLICY.into()), Some(CREDS.replace("5288", "0")))
                .admission(1001)
                .is_none()
        );
        assert!(
            Snapshot::new(Some("x".repeat(MAX_PPS + 1)), Some(CREDS.into()))
                .admission(1001)
                .is_none()
        );
    }

    #[test]
    fn policy_revocation_and_epoch_change_invalidate_the_session() {
        let changed = Snapshot::new(
            Some(POLICY.replace("epoch::3", "epoch::4")),
            Some(CREDS.into()),
        );
        assert!(!changed.admission(1001).unwrap().retains(SELECTED, 3));
        let disabled = Snapshot::new(
            Some(POLICY.replace("enabled::1", "enabled::0")),
            Some(CREDS.into()),
        );
        assert!(disabled.admission(1001).is_none());
    }
}
