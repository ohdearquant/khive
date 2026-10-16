use khive_runtime::presentation::{present, present_with_policy, PresentationMode};
use khive_types::VerbPresentationPolicy;
use serde_json::json;

// 2000-01-02T00:00:00Z.
const NOW: i64 = 946_771_200;

fn assert_timestamp_passthrough(timestamp: &str) {
    let input = json!({
        "created_at": timestamp,
        "items": [{"created_at": timestamp}],
    });
    assert_eq!(
        present(input.clone(), PresentationMode::Agent, NOW),
        input,
        "malformed timestamp must stay exact and gain no relative label: {timestamp}",
    );
}

#[test]
fn signed_calendar_and_clock_fields_are_not_timestamps() {
    for timestamp in [
        "+000-01-01T00:00:00Z",
        "-001-01-01T00:00:00Z",
        "2000-+1-01T00:00:00Z",
        "2000-01-+1T00:00:00Z",
        "2000-01-01T+1:00:00Z",
        "2000-01-01T-0:00:00Z",
        "2000-01-01T00:+1:00Z",
        "2000-01-01T00:-0:00Z",
        "2000-01-01T00:00:+1Z",
        "2000-01-01T00:00:-0Z",
    ] {
        assert_timestamp_passthrough(timestamp);
    }
}

#[test]
fn extra_signs_in_offset_fields_are_not_timestamps() {
    for offset in [
        "++1:00", "+-1:00", "-+1:00", "--1:00", "+00:+1", "+00:-1", "-00:+1", "-00:-1", "++100",
        "+-100", "-+100", "--100", "+00+1", "+00-1", "-00+1", "-00-1",
    ] {
        assert_timestamp_passthrough(&format!("2000-01-01T00:00:00.123456{offset}"));
    }
}

#[test]
fn valid_offsets_keep_exact_utc_conversion_and_relative_labels() {
    for (timestamp, expected) in [
        ("2000-01-01T00:00:00Z", "2000-01-01T00:00:00Z"),
        ("2000-01-01T01:30:00+01:30", "2000-01-01T00:00:00Z"),
        ("2000-01-01T01:30:00+0130", "2000-01-01T00:00:00Z"),
        ("1999-12-31T22:30:00-01:30", "2000-01-01T00:00:00Z"),
        ("1999-12-31T22:30:00-0130", "2000-01-01T00:00:00Z"),
        (
            "2000-01-01T01:30:00.123456789123+01:30",
            "2000-01-01T00:00:00.123456789123Z",
        ),
    ] {
        assert_eq!(
            present(
                json!({"created_at": timestamp}),
                PresentationMode::Agent,
                NOW
            ),
            json!({"created_at": expected}),
        );
    }
    assert_eq!(
        present(
            json!([{"created_at": "2000-01-01T01:30:00+01:30"}]),
            PresentationMode::Agent,
            NOW,
        ),
        json!([{
            "created_at": "2000-01-01T00:00:00Z",
            "created_at_relative": "1d ago",
        }]),
    );
}

#[test]
fn canonical_modes_and_protected_payloads_keep_timestamp_bytes() {
    let timestamp = "2000-01-01T01:30:00.123456+01:30";
    let input = json!({"items": [{"created_at": timestamp}]});
    for mode in [PresentationMode::Verbose, PresentationMode::Human] {
        assert_eq!(present(input.clone(), mode, NOW), input);
    }
    assert_eq!(
        present_with_policy(
            input.clone(),
            PresentationMode::Agent,
            NOW,
            VerbPresentationPolicy::AlwaysVerbose,
        ),
        input,
    );
    let protected = json!({"properties": {"items": [{"created_at": timestamp}]}});
    assert_eq!(
        present(protected.clone(), PresentationMode::Agent, NOW),
        protected,
    );
}
