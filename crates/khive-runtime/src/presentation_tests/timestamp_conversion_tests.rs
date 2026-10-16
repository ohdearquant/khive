#[test]
fn utc_rfc3339_seconds_normalizes_offsets_and_omits_fractions() {
    for (raw, expected) in [
        (
            "2026-01-01T00:15:30.987654321+01:00",
            "2025-12-31T23:15:30Z",
        ),
        ("1969-12-31T23:59:59.999999999Z", "1969-12-31T23:59:59Z"),
    ] {
        let instant = chrono::DateTime::parse_from_rfc3339(raw).unwrap();
        assert_eq!(format_utc_rfc3339_seconds(&instant), expected, "{raw}");
    }
}

#[test]
fn micros_to_iso_preserves_chrono_range_spelling() {
    for (micros, expected) in [
        (0, "1970-01-01T00:00:00.000000Z"),
        (-1, "1969-12-31T23:59:59.999999Z"),
        (1_748_016_480_123_456, "2025-05-23T16:08:00.123456Z"),
        (-62_198_755_200_000_000, "-0001-01-01T00:00:00.000000Z"),
        (253_402_300_800_000_000, "+10000-01-01T00:00:00.000000Z"),
        (-8_334_601_228_800_000_000, "-262143-01-01T00:00:00.000000Z"),
        (8_210_266_876_799_999_999, "+262142-12-31T23:59:59.999999Z"),
    ] {
        assert!(
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros).is_some(),
            "{micros}"
        );
        assert_eq!(micros_to_iso(micros), expected, "{micros}");
    }
}

#[test]
fn micros_to_iso_formats_outside_chrono_range_exactly() {
    for (micros, expected) in [
        (i64::MIN, "-290308-12-21T19:59:05.224192Z"),
        (i64::MAX, "+294247-01-10T04:00:54.775807Z"),
        (-8_334_601_228_800_000_001, "-262144-12-31T23:59:59.999999Z"),
        (8_210_266_876_800_000_000, "+262143-01-01T00:00:00.000000Z"),
        (-8_898_108_636_303_876_544, "-280000-02-29T12:34:56.123456Z"),
        (8_776_940_198_400_000_001, "+280100-03-01T00:00:00.000001Z"),
    ] {
        assert!(
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros).is_none(),
            "{micros}"
        );
        assert_eq!(micros_to_iso(micros), expected, "{micros}");
    }
}

#[test]
fn expanded_micros_timestamps_pass_through_agent_without_relative_labels() {
    for (micros, expected) in [
        (i64::MIN, "-290308-12-21T19:59:05.224192Z"),
        (i64::MAX, "+294247-01-10T04:00:54.775807Z"),
    ] {
        let shown = present(
            json!({"items": [{"created_at": micros_to_iso(micros)}]}),
            PresentationMode::Agent,
            NOW,
        );
        assert_eq!(shown, json!({"items": [{"created_at": expected}]}));
        assert!(rfc3339_to_utc_micros(expected).is_err());
    }
}

#[test]
fn rfc3339_to_utc_micros_round_trips_and_rejects_partial_forms() {
    let micros = 1_748_016_480_000_000_i64;
    assert_eq!(rfc3339_to_utc_micros(&micros_to_iso(micros)), Ok(micros));
    // Offset spellings resolve to the same instant; whitespace tolerated.
    // The offset spelling names the same instant as NOW.
    assert_eq!(
        rfc3339_to_utc_micros(" 2025-05-23T12:08:00-04:00 "),
        Ok(micros)
    );
    assert!(rfc3339_to_utc_micros("2026-05-23").is_err());
    assert!(rfc3339_to_utc_micros("2026-05-23T16:18:00").is_err());
}
