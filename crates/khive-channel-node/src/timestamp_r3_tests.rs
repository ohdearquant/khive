use crate::wire::UtcTimestamp;

#[test]
fn utc_timestamp_accepts_z_and_positive_zero_with_z_serialization() {
    for (input, expected) in [
        ("2026-09-23T20:00:00Z", "2026-09-23T20:00:00Z"),
        ("2026-09-23T20:00:00+00:00", "2026-09-23T20:00:00Z"),
        ("2026-09-23T20:00:59.1Z", "2026-09-23T20:00:59.100Z"),
        (
            "2026-09-23T20:00:59.123456789+00:00",
            "2026-09-23T20:00:59.123456789Z",
        ),
    ] {
        let parsed = UtcTimestamp::parse(input).expect(input);
        let from_json: UtcTimestamp =
            serde_json::from_value(serde_json::json!(input)).expect(input);
        assert_eq!(parsed, from_json);
        assert_eq!(parsed, UtcTimestamp::parse(expected).unwrap(), "{input}");
        assert_eq!(serde_json::to_value(&parsed).unwrap(), expected, "{input}");
        assert_eq!(UtcTimestamp::from_utc(parsed.as_utc()), parsed);
    }
}

#[test]
fn utc_timestamp_refuses_every_non_profile_form() {
    for input in [
        "2026-09-23 20:00:00Z",
        "2026-09-23t20:00:00Z",
        "2026-09-23T20:00:00z",
        "2026-09-23T20:00:00-00:00",
        "2026-09-23T23:59:60Z",
        "2026-09-23T20:00:00.1234567890Z",
        "2026-09-23T20:00:00.Z",
        "2026-09-23T20:00:00+24:00",
        "2026-09-23T20:00:00+05:60",
        "2026-02-30T20:00:00Z",
        "2026-09-23T24:00:00Z",
        "2026-09-23T20:00:00Z ",
    ] {
        assert!(UtcTimestamp::parse(input).is_err(), "{input}");
        assert!(
            serde_json::from_value::<UtcTimestamp>(serde_json::json!(input)).is_err(),
            "{input}"
        );
    }
}

#[test]
fn utc_timestamp_refuses_nonzero_offsets() {
    for input in [
        "2026-09-23T21:30:00+05:30",
        "2026-09-23T16:00:00-04:00",
        "2026-09-23T20:00:00+00:01",
        "2026-09-23T20:00:00-00:01",
    ] {
        assert!(UtcTimestamp::parse(input).is_err(), "{input}");
        assert!(
            serde_json::from_value::<UtcTimestamp>(serde_json::json!(input)).is_err(),
            "{input}"
        );
    }
}
