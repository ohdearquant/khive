// --- parse_iso8601_unix / relative-time offset handling ---

#[test]
fn parse_iso8601_unix_negative_offset_matches_equivalent_utc() {
    // "-04:00" is 4 hours behind UTC, so 11:55 local == 15:55Z.
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00-04:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_positive_offset_matches_equivalent_utc() {
    // "+04:00" is 4 hours ahead of UTC, so 20:15 local == 16:15Z.
    assert_eq!(
        parse_iso8601_unix("2026-05-23T20:15:00+04:00"),
        parse_iso8601_unix("2026-05-23T16:15:00Z")
    );
}

#[test]
fn parse_iso8601_unix_zero_offset_matches_z() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T15:55:00+00:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_compact_offset_form_matches_colon_form() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00-0400"),
        parse_iso8601_unix("2026-07-09T11:55:00-04:00")
    );
}

#[test]
fn parse_iso8601_unix_fractional_seconds_with_offset() {
    // Fractional seconds are dropped (whole-second precision only) but
    // must not prevent the trailing offset from being applied.
    assert_eq!(
        parse_iso8601_unix("2026-07-09T11:55:00.123-04:00"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_fractional_seconds_with_z() {
    assert_eq!(
        parse_iso8601_unix("2026-07-09T15:55:00.999Z"),
        parse_iso8601_unix("2026-07-09T15:55:00Z")
    );
}

#[test]
fn parse_iso8601_unix_rejects_bare_form() {
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00"), None);
}

#[test]
fn parse_iso8601_unix_malformed_tail_returns_none() {
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00X"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+04"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00."), None);
}

#[test]
fn parse_iso8601_unix_out_of_range_offset_returns_none() {
    // Hour out of range (>23), colon and compact forms.
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+24:00"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+2400"), None);
    // Minute out of range (>59), colon and compact forms.
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+01:60"), None);
    assert_eq!(parse_iso8601_unix("2026-07-09T15:55:00+0160"), None);
}

#[test]
fn parse_iso8601_unix_max_valid_offset_boundary_is_accepted() {
    // +23:59 / -23:59 are the largest valid offsets and must still parse.
    assert!(parse_iso8601_unix("2026-07-09T15:55:00+23:59").is_some());
    assert!(parse_iso8601_unix("2026-07-09T15:55:00-23:59").is_some());
    assert!(parse_iso8601_unix("2026-07-09T15:55:00+2359").is_some());
}

#[test]
fn exact_timestamp_converts_future_offset_without_relative_form() {
    let out = exact_timestamp("2025-05-23T16:08:00-02:00").unwrap();
    assert_eq!(out.0, "2025-05-23T18:08:00Z");
    assert!(out.1 > NOW);
}

#[test]
fn exact_timestamp_converts_past_offset_and_retains_fraction() {
    let out = exact_timestamp("2025-05-23T20:05:00.123+04:00").unwrap();
    assert_eq!(out.0, "2025-05-23T16:05:00.123Z");
    assert_eq!(out.1, NOW - 180);
}
