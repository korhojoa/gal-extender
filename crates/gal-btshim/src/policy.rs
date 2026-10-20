// SPDX-License-Identifier: GPL-3.0-only
use btsvc_core::wireless_policy::{Address, Policy};

pub const POLICY_PPS: &str = "/ramdisk/pps/aawg/policy";
pub const SESSION_PPS: &str = "/ramdisk/pps/aawg/session";

/// PPS stores one scalar per attribute. Reuse the authoritative policy parser
/// after translating its fixed fields. An allow list is comma separated.
#[must_use]
pub fn parse(text: &str, now: u64) -> Option<Policy> {
    Policy::parse(btsvc_core::pps::decode(text)?.as_str(), now)
}

#[must_use]
pub fn nudge(line: &[u8]) -> Option<Address> {
    let text = std::str::from_utf8(line).ok()?;
    btsvc_core::wireless_policy::address(text.strip_suffix('\n')?.strip_prefix("nudge ")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    const PPS: &str = "@policy\nversion::1\nrevision::2\nepoch::3\nupdated_ms::1000\nenabled::1\nselected::01:23:45:67:89:AB\nallow::01:23:45:67:89:AB,12:34:56:78:9A:BC\nend::1\n";
    #[test]
    fn admission_requires_selected_phone_and_live_lease() {
        let p = parse(PPS, 1001).unwrap();
        assert!(p.accepts_bluetooth(&[1, 35, 69, 103, 137, 171]));
        assert!(!p.accepts_bluetooth(&[18, 52, 86, 120, 154, 188]));
        assert!(parse(PPS, 11001).is_none());
        assert!(parse(PPS, 999).is_none());
        assert!(parse(&PPS.replace("enabled::1", "enabled::0"), 1001).is_none());
        assert!(parse(&format!("{PPS}epoch::4\n"), 1001).is_none());
    }
    #[test]
    fn trigger_is_one_complete_address_line() {
        assert_eq!(
            nudge(b"nudge 01:23:45:67:89:AB\n"),
            Some([1, 35, 69, 103, 137, 171])
        );
        assert!(nudge(b"nudge 01:23:45:67:89:AB").is_none());
        assert!(nudge(b"nudge 01:23:45:67:89:AB\nextra\n").is_none());
    }
}
