// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa
// Tests the actual Rust policy, not execution of the OEM protobuf parser.
// The enum expectations follow the retained native map in.
use super::{classify_focus, route_with, Decision, Focus, ForwardReason, Reject, MAX_PAYLOAD};
use std::cell::Cell;

fn run(
    enabled: bool,
    message_type: u16,
    payload: Option<&[u8]>,
    eligible: Option<bool>,
    original_result: i32,
) -> (i32, Decision, u32, u32) {
    let originals = Cell::new(0);
    let grants = Cell::new(0);
    let (result, decision) = route_with(
        enabled,
        message_type,
        payload,
        eligible,
        || {
            originals.set(originals.get() + 1);
            original_result
        },
        || grants.set(grants.get() + 1),
    );
    (result, decision, originals.get(), grants.get())
}

#[test]
fn disabled_preserves_even_unreadable_payload_and_original_result() {
    for payload in [None, Some(&[0x08][..]), Some(&[0x08, 0x02][..])] {
        assert_eq!(
            run(false, 13, payload, None, -253),
            (-253, Decision::Forward(ForwardReason::Disabled), 1, 0)
        );
    }
}

#[test]
fn unrelated_including_unit_originated_notification_is_unchanged() {
    // Message 14 is the HU-to-phone focus notification, not incoming request 13.
    for message_type in [0, 1, 12, 14, 15, 0x8003, 0x8004, 0x8005, u16::MAX] {
        for eligible in [None, Some(false), Some(true)] {
            assert_eq!(
                run(true, message_type, Some(&[0x08, 0x02]), eligible, 0x6a5b),
                (0x6a5b, Decision::Forward(ForwardReason::Unrelated), 1, 0)
            );
        }
    }
}

#[test]
fn supported_wire_classification_matches_mapped_optional_enum_rules() {
    // These are primary-map expectations, not a mocked native parser claiming
    // that arbitrary unsupported protobuf encodings are equivalent.
    let cases: &[(&[u8], Focus)] = &[
        (&[], Focus::Native),
        (&[0x08, 0x00], Focus::Native),
        (&[0x08, 0x03], Focus::Native),
        (&[0x08, 0x01], Focus::Native),
        (&[0x08, 0x02], Focus::Projected),
        (&[0x08, 0x02, 0x08, 0x01], Focus::Native),
        (&[0x08, 0x01, 0x08, 0x02], Focus::Projected),
        (&[0x08, 0x02, 0x08, 0x03], Focus::Projected),
        (&[0x08, 0x03, 0x08, 0x01], Focus::Native),
        // Unknown length-delimited field containing the PROJECTED byte pair.
        (&[0x12, 0x02, 0x08, 0x02], Focus::Native),
        (&[0x12, 0x02, 0x08, 0x01, 0x08, 0x02], Focus::Projected),
        // Unknown fixed32/fixed64 data must not be scanned for enum bytes.
        (&[0x15, 0x08, 0x02, 0x00, 0x00], Focus::Native),
        (&[0x11, 0x08, 0x02, 0, 0, 0, 0, 0, 0], Focus::Native),
    ];
    for &(payload, expected) in cases {
        assert_eq!(classify_focus(payload), Ok(expected), "{payload:02x?}");
    }
}

#[test]
fn eligible_projected_omits_original_and_attempts_exactly_one_grant() {
    assert_eq!(
        run(true, 13, Some(&[0x08, 0x02]), Some(true), -99),
        (0, Decision::GrantProjected, 0, 1)
    );
    // Repeated requests are separately handled. there is no invented latch,
    // retry, HMI flag reset or success receipt from the void grant operation.
    assert_eq!(
        run(true, 13, Some(&[0x08, 0x01, 0x08, 0x02]), Some(true), 42),
        (0, Decision::GrantProjected, 0, 1)
    );
}

#[test]
fn fatal_error_latch_suppresses_projected_without_original_or_grant() {
    assert_eq!(
        run(true, 13, Some(&[0x08, 0x02]), Some(false), 45),
        (0, Decision::SuppressLatched, 0, 0)
    );
}

#[test]
fn native_and_default_enum_requests_keep_original_latch_handling() {
    for payload in [&[][..], &[0x08, 1], &[0x08, 3], &[0x08, 2, 0x08, 1]] {
        for eligible in [None, Some(false), Some(true)] {
            assert_eq!(
                run(true, 13, Some(payload), eligible, -17),
                (-17, Decision::Forward(ForwardReason::Native), 1, 0)
            );
        }
    }
}

#[test]
fn unverified_callback_identity_keeps_projected_original() {
    assert_eq!(
        run(true, 13, Some(&[0x08, 0x02]), None, 77),
        (77, Decision::Forward(ForwardReason::Eligibility), 1, 0)
    );
    assert_eq!(
        run(true, 13, None, Some(true), -253),
        (-253, Decision::Forward(ForwardReason::Payload), 1, 0)
    );
}

#[test]
fn malformed_suffix_is_not_hidden_by_an_earlier_projected_field() {
    let cases: &[&[u8]] = &[
        &[0x08],
        &[0x08, 0x80],
        &[0x08, 2, 0x10],
        &[0x08, 2, 0x12, 3, 1],
        &[0x08, 2, 0x15, 1],
        &[0x08, 2, 0x11, 1],
    ];
    for &payload in cases {
        assert!(classify_focus(payload).is_err(), "{payload:02x?}");
        for eligible in [None, Some(false), Some(true)] {
            let (result, decision, originals, grants) =
                run(true, 13, Some(payload), eligible, -253);
            assert_eq!((result, originals, grants), (-253, 1, 0));
            assert!(matches!(
                decision,
                Decision::Forward(ForwardReason::Wire(_))
            ));
        }
    }
}

#[test]
fn unsupported_encodings_forward_instead_of_guessing_native_parse() {
    let cases: &[&[u8]] = &[
        &[0x08, 0x82, 0x00],                      // Noncanonical scalar.
        &[0x88, 0x00, 0x02],                      // Noncanonical tag.
        &[0x12, 0x80, 0x00],                      // Noncanonical length.
        &[0x13, 0x08, 0x02, 0x14],                // Unknown group, accepted by some protobufs.
        &[0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 1], // Wide enum.
        &[0x00, 0x02],                            // Illegal field zero.
        &[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
        ], // Ten-byte overflow.
    ];
    for &payload in cases {
        assert!(classify_focus(payload).is_err(), "{payload:02x?}");
        let (result, decision, originals, grants) = run(true, 13, Some(payload), Some(false), 0x33);
        assert_eq!((result, originals, grants), (0x33, 1, 0));
        assert!(matches!(
            decision,
            Decision::Forward(ForwardReason::Wire(_))
        ));
    }
}

#[test]
fn payload_cap_refuses_before_any_projected_decision() {
    let mut boundary = [0x10, 0].repeat(MAX_PAYLOAD / 2);
    assert_eq!(boundary.len(), MAX_PAYLOAD);
    boundary[MAX_PAYLOAD - 2..].copy_from_slice(&[0x08, 2]);
    assert_eq!(classify_focus(&boundary), Ok(Focus::Projected));
    assert_eq!(
        run(true, 13, Some(&boundary), Some(true), -1),
        (0, Decision::GrantProjected, 0, 1)
    );
    let payload = vec![0; MAX_PAYLOAD + 1];
    assert_eq!(classify_focus(&payload), Err(Reject::TooLarge));
    assert_eq!(
        run(true, 13, Some(&payload), Some(true), -253),
        (
            -253,
            Decision::Forward(ForwardReason::Wire(Reject::TooLarge)),
            1,
            0
        )
    );
}

#[test]
fn original_closure_preserves_captured_native_arguments_and_result() {
    let controller = 0x1234_usize;
    let channel = 0xa5_u8;
    let message = 14_u16;
    let shared_pointer = 0x5678_usize;
    let observed = Cell::new(None);
    let (result, decision) = route_with(
        true,
        message,
        None,
        Some(false),
        || {
            observed.set(Some((controller, channel, message, shared_pointer)));
            -0x1234
        },
        || panic!("outgoing notification must never trigger another grant"),
    );
    assert_eq!(observed.get(), Some((0x1234, 0xa5, 14, 0x5678)));
    assert_eq!(result, -0x1234);
    assert_eq!(decision, Decision::Forward(ForwardReason::Unrelated));
}

#[test]
fn native_body_range_rejects_bad_geometry_before_slice_creation() {
    use super::body_range;
    assert_eq!(body_range(5, 4, 8), None);
    assert_eq!(body_range(0, 9, 8), None);
    assert_eq!(body_range(0, 0, 0), None);
    assert_eq!(body_range(0, 1, 1), None);
    assert_eq!(body_range(0, 2, 2), Some((2, 0)));
    assert_eq!(body_range(3, 9, 12), Some((5, 4)));
    assert_eq!(
        body_range(3, 5 + MAX_PAYLOAD, 5 + MAX_PAYLOAD),
        Some((5, MAX_PAYLOAD))
    );
    assert_eq!(body_range(3, 6 + MAX_PAYLOAD, 6 + MAX_PAYLOAD), None);
    assert_eq!(body_range(usize::MAX, usize::MAX, usize::MAX), None);
}

#[test]
fn rust_panic_after_grant_attempt_never_retries_original_route() {
    let originals = Cell::new(0);
    let grants = Cell::new(0);
    let result = crate::guard("nav-focus-test", || {
        route_with(
            true,
            13,
            Some(&[0x08, 2]),
            Some(true),
            || {
                originals.set(originals.get() + 1);
                -253
            },
            || {
                grants.set(grants.get() + 1);
                panic!("deliberate Rust grant closure panic");
            },
        )
    });
    assert!(result.is_none());
    assert_eq!(originals.get(), 0);
    assert_eq!(grants.get(), 1);
    // This test proves the Rust unwind policy only. It does not test foreign exception
    // propagation.
}

#[test]
fn callback_identity_requires_exact_nonnull_native_address_point() {
    use super::callback_table_matches;
    use core::ptr::{null, without_provenance};
    let table = without_provenance::<u8>(0x1000);
    assert!(callback_table_matches(without_provenance(0x1008), table));
    assert!(!callback_table_matches(table, table));
    assert!(!callback_table_matches(without_provenance(0x1010), table));
    assert!(!callback_table_matches(null(), table));
    assert!(!callback_table_matches(without_provenance(8), null()));
    assert!(!callback_table_matches(null(), null()));
    assert!(!callback_table_matches(
        without_provenance(3),
        without_provenance(usize::MAX - 4)
    ));
}

#[test]
fn missing_reply_preserves_latched_ignore_and_only_falls_back_when_eligible() {
    use super::reply_eligibility;
    for reply_available in [false, true] {
        assert_eq!(reply_eligibility(None, reply_available), None);
        assert_eq!(reply_eligibility(Some(false), reply_available), Some(false));
        assert_eq!(
            run(
                true,
                13,
                Some(&[0x08, 2]),
                reply_eligibility(Some(false), reply_available),
                73
            ),
            (0, Decision::SuppressLatched, 0, 0)
        );
    }
    assert_eq!(reply_eligibility(Some(true), true), Some(true));
    assert_eq!(reply_eligibility(Some(true), false), None);
    assert_eq!(
        run(
            true,
            13,
            Some(&[0x08, 2]),
            reply_eligibility(Some(true), false),
            73
        ),
        (73, Decision::Forward(ForwardReason::Eligibility), 1, 0)
    );
}
