// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Optional passive availability-cycle witness for initial focus admission.
//! The epoch identifies a witness generation, not native process liveness.

pub const WAIT_MS: u64 = 22_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    epoch: [u8; 32],
    serial: u64,
    available: bool,
    updated_wall_ms: Option<u64>,
}

/// One bounded complete PPS snapshot. Any I/O error refuses the whole result,
/// even if an implementation wrote a valid-looking prefix before failing.
pub fn read_snapshot(reader: &mut impl std::io::Read) -> Option<Snapshot> {
    let mut buffer = [0u8; 1024];
    let mut count = 0;
    loop {
        if count == buffer.len() {
            return None;
        }
        let read = reader.read(&mut buffer[count..]).ok()?;
        if read == 0 {
            return parse(&buffer[..count]);
        }
        count += read;
    }
}

/// Parse a complete PPS snapshot. Reject truncated input, unsupported fields, duplicate
/// attributes and partial numeric prefixes.
pub fn parse(text: &[u8]) -> Option<Snapshot> {
    if !text.ends_with(b"\n") || text.contains(&0) {
        return None;
    }
    let mut epoch = None;
    let mut serial = None;
    let mut available = None;
    let mut schema = false;
    let mut updated_wall_ms = None;
    for (index, line) in text.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() || line.starts_with(b"@") {
            continue;
        }
        let colon = line.iter().position(|b| *b == b':')?;
        let key = &line[..colon];
        let value = line[colon..].strip_prefix(b"::")?;
        if key.is_empty() || key.starts_with(b"-") || value.contains(&b':') {
            return None;
        }
        if text
            .split(|b| *b == b'\n')
            .take(index)
            .any(|previous| gal_feature_api::pps::attr(previous).is_some_and(|(old, _)| old == key))
        {
            return None;
        }
        match key {
            b"gfx_cycle_schema" if value == b"1" => schema = true,
            b"gfx_cycle_schema" => return None,
            b"gfx_observer_epoch" => {
                let bytes: [u8; 32] = value.try_into().ok()?;
                if !bytes
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                {
                    return None;
                }
                epoch = Some(bytes);
            }
            b"gfx_cycle_serial" => {
                if value.is_empty() || value[0] == b'0' || !value.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                let number = core::str::from_utf8(value).ok()?.parse::<u64>().ok()?;
                if number > 0x7fff_ffff_ffff_ffff {
                    return None;
                }
                serial = Some(number);
            }
            b"updated_ms" => {
                if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                updated_wall_ms = Some(core::str::from_utf8(value).ok()?.parse().ok()?);
            }
            b"gfx_state" => {
                available = Some(match value {
                    b"0" => false,
                    b"1" => true,
                    _ => return None,
                });
            }
            _ => {}
        }
    }
    schema.then_some(Snapshot {
        epoch: epoch?,
        serial: serial?,
        available: available?,
        updated_wall_ms,
    })
}

/// Select the witness only from a complete, recently published PPS object.
/// Wall-clock age selects the optional source. it does not prove native liveness.
impl Snapshot {
    pub fn fresh_at(&self, wall_ms: u64) -> bool {
        self.updated_wall_ms
            .and_then(|stamp| wall_ms.checked_sub(stamp))
            .is_some_and(|age| age <= WAIT_MS)
    }
}

/// The encoder, handover and settle checks apply to both admission paths.
/// Once a witness is selected, its loss or invalidation never enables fallback.
pub enum FirstSession {
    EncoderAndSettle,
    Witness(Admission),
}

impl FirstSession {
    pub fn new(snapshot: Option<Snapshot>, mono_ms: u64, wall_ms: u64) -> Self {
        match snapshot.filter(|snapshot| snapshot.fresh_at(wall_ms)) {
            Some(baseline) => Self::Witness(Admission::new(baseline, mono_ms)),
            None => Self::EncoderAndSettle,
        }
    }

    pub fn uses_witness(&self) -> bool {
        matches!(self, Self::Witness(_))
    }

    pub fn poll(&self, current: Option<Snapshot>, mono_ms: u64, wall_ms: u64) -> Decision {
        match self {
            Self::EncoderAndSettle => Decision::Ready,
            Self::Witness(admission) => admission.poll(
                current.filter(|snapshot| snapshot.fresh_at(wall_ms)),
                mono_ms,
            ),
        }
    }

    pub fn still_valid(&self, current: Option<Snapshot>, wall_ms: u64) -> bool {
        match self {
            Self::EncoderAndSettle => true,
            Self::Witness(admission) => {
                admission.still_valid(current.filter(|snapshot| snapshot.fresh_at(wall_ms)))
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Wait,
    Ready,
    Park,
}

pub struct Admission {
    baseline: Snapshot,
    deadline: u64,
}

impl Admission {
    pub fn new(baseline: Snapshot, now: u64) -> Self {
        Self {
            baseline,
            deadline: now.saturating_add(WAIT_MS),
        }
    }

    /// Revalidate an already observed in-time cycle after panel settle.
    /// The acquisition deadline does not limit that separate existing settle.
    pub fn still_valid(&self, current: Option<Snapshot>) -> bool {
        current.is_some_and(|current| {
            current.epoch == self.baseline.epoch
                && current.serial > self.baseline.serial
                && current.available
        })
    }

    /// No automatic baseline reset after a restart or invalid callback.
    pub fn poll(&self, current: Option<Snapshot>, now: u64) -> Decision {
        if now >= self.deadline {
            return Decision::Park;
        }
        let Some(current) = current else {
            return Decision::Park;
        };
        if current.epoch != self.baseline.epoch || current.serial < self.baseline.serial {
            return Decision::Park;
        }
        if current.available && current.serial > self.baseline.serial {
            Decision::Ready
        } else {
            Decision::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(epoch: &str, serial: &str, state: &str) -> Vec<u8> {
        format!("@cluster\ngfx_cycle_schema::1\ngfx_observer_epoch::{epoch}\ngfx_cycle_serial::{serial}\ngfx_state::{state}\n").into_bytes()
    }
    const EPOCH: &str = "0123456789abcdef0123456789abcdef";
    fn published(serial: &str, state: &str, stamp: u64) -> Option<Snapshot> {
        parse(
            &[
                snapshot(EPOCH, serial, state),
                format!("updated_ms::{stamp}\n").into_bytes(),
            ]
            .concat(),
        )
    }

    #[test]
    fn absent_incomplete_stale_or_future_witness_uses_encoder_and_settle() {
        for baseline in [
            None,
            parse(&snapshot(EPOCH, "1", "1")),
            published("1", "1", 1),
            published("1", "1", 30_001),
        ] {
            let path = FirstSession::new(baseline, 100, 30_000);
            assert!(!path.uses_witness());
            assert_eq!(path.poll(None, 200, 30_100), Decision::Ready);
            assert!(path.still_valid(None, 35_000));
        }
        assert!(parse(b"gfx_state::1\nupdated_ms::30000\n").is_none());
    }

    #[test]
    fn fresh_witness_requires_a_new_cycle_and_never_downgrades_on_loss() {
        let path = FirstSession::new(published("1", "1", 30_000), 100, 30_000);
        assert!(path.uses_witness());
        assert_eq!(
            path.poll(published("1", "1", 30_000), 101, 30_001),
            Decision::Wait
        );
        assert_eq!(
            path.poll(published("2", "1", 30_001), 102, 30_002),
            Decision::Ready
        );
        assert!(path.still_valid(published("2", "1", 30_001), 35_002));
        assert_eq!(path.poll(None, 102, 30_002), Decision::Park);
        assert!(!path.still_valid(None, 35_002));
        assert!(!path.still_valid(published("2", "1", 1), 35_002));
        assert_eq!(
            path.poll(published("2", "1", 30_000), 103, 52_001),
            Decision::Park
        );
        assert!(published("1", "1", 30_000).unwrap().fresh_at(52_000));
    }

    #[test]
    fn strict_complete_snapshot_required() {
        let valid = snapshot(EPOCH, "1", "1");
        assert!(parse(&valid).is_some());
        for serial in [
            "0",
            "01",
            "+1",
            "1junk",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            assert!(parse(&snapshot(EPOCH, serial, "1")).is_none());
        }
        for bad in [
            String::from_utf8(valid.clone())
                .unwrap()
                .replace("gfx_state::1", "gfx_state:bogus:1")
                .into_bytes(),
            [valid.as_slice(), b"-gfx_state\n"].concat(),
            b"gfx_state::1\n".to_vec(),
            valid[..valid.len() - 1].to_vec(),
            [valid.as_slice(), b"gfx_state::1\n"].concat(),
            snapshot("junk", "1", "1"),
            snapshot(EPOCH, "1", "-1"),
        ] {
            assert!(parse(&bad).is_none());
        }
    }
    #[test]
    fn coalesced_cycle_qualifies_but_invalid_generation_never_does() {
        let baseline = parse(&snapshot(EPOCH, "1", "1")).unwrap();
        let admission = Admission::new(baseline, 100);
        assert_eq!(admission.poll(Some(baseline), 101), Decision::Wait);
        assert_eq!(
            admission.poll(parse(&snapshot(EPOCH, "2", "1")), 102),
            Decision::Ready
        );
        assert_eq!(
            admission.poll(
                parse(&snapshot("1123456789abcdef0123456789abcdef", "2", "1")),
                102
            ),
            Decision::Park
        );
        assert_eq!(admission.poll(None, 102), Decision::Park);
        assert_eq!(admission.poll(Some(baseline), 22_100), Decision::Park);
        assert_eq!(
            admission.poll(parse(&snapshot(EPOCH, "2", "0")), 103),
            Decision::Wait
        );
    }
    #[test]
    fn acquisition_deadline_and_final_revalidation_are_distinct() {
        let baseline = parse(&snapshot(EPOCH, "1", "1")).unwrap();
        let ready = parse(&snapshot(EPOCH, "2", "1"));
        let admission = Admission::new(baseline, 0);
        assert_eq!(admission.poll(ready, 21_999), Decision::Ready);
        assert_eq!(admission.poll(ready, 22_000), Decision::Park);
        assert_eq!(admission.poll(ready, 22_001), Decision::Park);
        assert!(admission.still_valid(ready));
        assert!(!admission.still_valid(parse(&snapshot(EPOCH, "2", "0"))));
        assert!(!admission.still_valid(parse(&snapshot(
            "1123456789abcdef0123456789abcdef",
            "2",
            "1"
        ))));
        assert!(!admission.still_valid(None));
    }

    #[test]
    fn checked_read_rejects_error_after_valid_looking_prefix_and_capacity() {
        struct Failed(Vec<u8>);
        impl std::io::Read for Failed {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Err(std::io::Error::other("injected PPS read failure"));
                }
                let count = self.0.len();
                output[..count].copy_from_slice(&self.0);
                self.0.clear();
                Ok(count)
            }
        }
        let valid = snapshot(EPOCH, "2", "1");
        assert!(read_snapshot(&mut Failed(valid.clone())).is_none());
        assert_eq!(read_snapshot(&mut valid.as_slice()), parse(&valid));
        let mut full = valid.clone();
        full.resize(1024, b'\n');
        assert!(read_snapshot(&mut full.as_slice()).is_none());
    }
}
