use super::*;
use chrono::TimeZone;

// America/New_York (used for the west-of-UTC and general DST coverage in
// tests/assign.rs) never transitions at local midnight, so it cannot
// exercise LocalResult::None or ::Ambiguous — a "DST test" against it
// passes without touching the rule ADR-169 D1 actually specifies. The
// zones and dates below are ADR-169's own worked table (measured against
// the IANA database) of zones that DO transition at 00:00.

#[test]
fn date_only_due_resolves_to_first_instant_when_local_midnight_does_not_exist_havana() {
    let tz: Tz = "America/Havana".parse().expect("known IANA zone");
    let result = parse_due("2021-03-14", tz).expect(
        "a gap date must resolve to the first instant of that date, not error (ADR-169 D1)",
    );
    let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
    assert_eq!(
        parsed.date_naive(),
        chrono::NaiveDate::from_ymd_opt(2021, 3, 14).unwrap(),
        "got {result}"
    );
    assert!(
        parsed.time() > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        "local midnight does not exist on this date, so the resolved instant must be \
             strictly after 00:00 — proving the gap was bridged forward, not unwrapped to UTC; \
             got {result}"
    );
}

#[test]
fn date_only_due_resolves_to_first_instant_when_local_midnight_does_not_exist_santiago() {
    let tz: Tz = "America/Santiago".parse().expect("known IANA zone");
    let result = parse_due("2021-09-05", tz).expect(
        "a gap date must resolve to the first instant of that date, not error (ADR-169 D1)",
    );
    let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
    assert_eq!(
        parsed.date_naive(),
        chrono::NaiveDate::from_ymd_opt(2021, 9, 5).unwrap(),
        "got {result}"
    );
    assert!(
        parsed.time() > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        "got {result}"
    );
}

// The two tests above assert only that the resolved time is after 00:00.
// That is too weak to separate "the first instant on this date" from "some
// instant on this date": both zones' gaps begin exactly at local midnight,
// so a projection under the previous offset and a true least-instant search
// agree there and either implementation passes. The test below uses a zone
// whose gap begins BEFORE midnight, where the two rules give different
// answers, and asserts the least-instant property itself.
#[test]
fn date_only_due_resolves_to_the_least_instant_when_the_gap_begins_before_midnight_toronto() {
    let tz: Tz = "America/Toronto".parse().expect("known IANA zone");
    // On 1919-03-31 the clock jumped from 23:30 on the previous date to
    // 00:30, so local midnight never occurred and the first instant of the
    // date is 00:30 -04:00 = 04:30Z. Reading the previous day's offset
    // (-05:00) and projecting midnight forward gives 05:00Z instead, which
    // is on the right date and thirty minutes late.
    let result = parse_due("1919-03-31", tz)
        .expect("a gap date must resolve to the first instant of that date (ADR-169 D1)");
    let parsed = DateTime::parse_from_rfc3339(&result).unwrap();

    assert_eq!(
        parsed.to_utc(),
        chrono::NaiveDate::from_ymd_opt(1919, 3, 31)
            .unwrap()
            .and_hms_opt(4, 30, 0)
            .unwrap()
            .and_utc(),
        "must be the FIRST instant of the date (04:30Z), not the instant local midnight \
             would have had under the previous offset (05:00Z); got {result}"
    );

    // The least-instant property, asserted directly rather than by
    // comparing against a constant: one second earlier must already be a
    // different local date. This is what fails for the projected answer,
    // since 04:59:59Z is still 1919-03-31 locally.
    let one_second_earlier = parsed.to_utc() - chrono::Duration::seconds(1);
    assert_ne!(
        tz.from_utc_datetime(&one_second_earlier.naive_utc())
            .date_naive(),
        chrono::NaiveDate::from_ymd_opt(1919, 3, 31).unwrap(),
        "the instant one second before the resolved one must belong to a different local \
             date, or the resolved instant was not the least one; got {result}"
    );
}

#[test]
fn date_only_due_picks_the_earlier_instant_when_local_midnight_occurs_twice_havana() {
    let tz: Tz = "America/Havana".parse().expect("known IANA zone");
    let date = chrono::NaiveDate::from_ymd_opt(2020, 11, 1).unwrap();
    let midnight = date.and_hms_opt(0, 0, 0).unwrap();
    // Derived directly from chrono_tz, independent of parse_due, so this
    // is a cross-check against the library's own contract rather than a
    // restatement of parse_due's internals.
    let (earliest, latest) = match tz.from_local_datetime(&midnight) {
        chrono::LocalResult::Ambiguous(a, b) => (a, b),
        other => panic!(
            "2020-11-01 in America/Havana must be ambiguous per ADR-169's worked table; \
                 got {other:?}"
        ),
    };
    assert!(
        earliest < latest,
        "sanity: chrono's `earliest` must be chronologically first"
    );

    let result = parse_due("2020-11-01", tz).expect("an ambiguous date must resolve, not error");
    let parsed = DateTime::parse_from_rfc3339(&result).unwrap();
    assert_eq!(
        parsed, earliest,
        "parse_due must pick the earlier of the two midnight instants (ADR-169 D1); got {result}"
    );
}

// Pacific/Apia jumped from UTC-11 to UTC+13 on 2011-12-30, skipping that
// calendar date entirely in local time — not a sub-day gap but a
// whole-day one, which ADR-169 D1's worked examples do not name. No
// instant anywhere maps to this date in this zone, so parse_due errors
// rather than silently resolving to a neighboring date under a
// mislabeled due-date.
#[test]
fn date_only_due_in_a_skipped_local_day_is_rejected() {
    let tz: Tz = "Pacific/Apia".parse().expect("known IANA zone");
    let err = parse_due("2011-12-30", tz).expect_err("2011-12-30 never existed in Apia");
    let msg = err.to_string();
    assert!(
        msg.contains("2011-12-30"),
        "error must name the unparseable due date; got: {msg}"
    );
}

/// `%Y` accepts chrono's full signed year range, so a caller can name the
/// minimum date, whose ±48h anchor window runs off the end of
/// `NaiveDateTime`. chrono's `-` PANICS there, and the input arrives
/// through a public verb, so that was reachable availability denial.
///
/// The right outcome is not a rejection. In a zone east of UTC the minimum
/// date has no representable local midnight, but later local times on it
/// do exist, so the date HAS a least representable instant and the
/// least-instant contract can be honoured. Assert that instant, which also
/// keeps this a panic regression test: bare arithmetic here still aborts.
#[test]
fn the_minimum_date_resolves_to_its_least_representable_instant_east_of_utc() {
    let tz: Tz = "Pacific/Apia".parse().expect("known IANA zone");
    let out = parse_due("-262143-01-01", tz)
        .expect("the minimum date has representable instants in a positive-offset zone");
    assert!(
        out.starts_with("-262143-01-01T12:33:04"),
        "must be the least representable instant carrying that local date, which is \
             chrono's minimum instant seen through Apia's +12:33:04 offset; got: {out}"
    );
}

/// The returned instant is not merely valid, it is chrono's minimum. That
/// is what makes it the LEAST instant carrying the date: the usual
/// one-second-earlier check cannot be written here, because one second
/// earlier does not exist.
#[test]
fn the_minimum_date_anchors_exactly_at_the_minimum_instant_east_of_utc() {
    let tz: Tz = "Pacific/Apia".parse().expect("known IANA zone");
    let resolved = super::anchor_date_to_earliest_instant(chrono::NaiveDate::MIN, tz)
        .expect("the minimum date has representable instants east of UTC");
    assert_eq!(
        resolved.naive_utc(),
        chrono::NaiveDateTime::MIN,
        "no representable instant precedes this one, so it is the least"
    );
}

/// The same boundary from the other side, and it takes a DIFFERENT path.
/// Zone names mislead here: `America/Adak` reads as west of UTC but its
/// earliest LMT is +12:13:22, because it crossed the date line. Zones that
/// are genuinely negative at the minimum instant have a representable
/// local midnight (UTC = midnight + |offset|), so they resolve through the
/// direct branch and never reach the lower-bound short-circuit. Offsets
/// measured at `NaiveDateTime::MIN`: New York -4:56:02, Los Angeles
/// -7:52:58, Honolulu -10:31:26.
#[test]
fn the_minimum_date_resolves_in_genuinely_negative_offset_zones() {
    for name in [
        "America/New_York",
        "America/Los_Angeles",
        "Pacific/Honolulu",
    ] {
        let tz: Tz = name.parse().expect("known IANA zone");
        let out = parse_due("-262143-01-01", tz)
            .unwrap_or_else(|e| panic!("{name}: minimum date must resolve, got {e}"));
        assert!(
            out.starts_with("-262143-01-01T00:00:00"),
            "{name}: a representable local midnight anchors the date directly; got: {out}"
        );
    }
}

/// The guard this replaced asked "is the +/-48h window representable" of
/// EVERY date, but only the skipped-midnight branch searches that window.
/// So it rejected the extremes of the representable range even where local
/// midnight resolves directly to a single instant.
///
/// Both ends, and zones on both sides of UTC: the offset's SIGN decides
/// which end is at risk, so a single-zone test would confirm one direction
/// and infer the other. `Pacific/Apia` is east of UTC, `America/Adak` west.
#[test]
fn the_extremes_of_the_representable_range_resolve_when_local_midnight_exists() {
    for name in ["UTC", "Pacific/Apia", "Pacific/Midway", "America/Adak"] {
        let tz: Tz = name.parse().expect("known IANA zone");
        let out = parse_due("+262142-12-31", tz)
            .unwrap_or_else(|e| panic!("{name}: the maximum date must resolve, got {e}"));
        assert!(
            out.starts_with("+262142-12-31T00:00:00"),
            "{name}: the maximum date anchors to its own local midnight; got: {out}"
        );
    }

    // The minimum date takes the SEARCHING branch in zones east of UTC:
    // converting its local midnight to UTC would run off the end of the
    // range, and chrono-tz reports that as no valid local time. That case
    // is asserted above, where it resolves to the least representable
    // instant. The case here is UTC, where local midnight exists, so the
    // direct branch must still succeed at the bottom of the range.
    let utc: Tz = "UTC".parse().expect("known IANA zone");
    let out = parse_due("-262143-01-01", utc)
        .expect("the minimum date must resolve in UTC, where its midnight exists");
    assert!(
        out.starts_with("-262143-01-01T00:00:00"),
        "the minimum date anchors to its own local midnight in UTC; got: {out}"
    );
}

/// The must-not-fire half. Without it the assertion above could pass
/// against a resolver that rejected every date.
#[test]
fn an_ordinary_date_still_resolves() {
    let tz: Tz = "Pacific/Apia".parse().expect("known IANA zone");
    let out = parse_due("2026-08-23", tz).expect("an ordinary date must still resolve");
    assert!(
        out.starts_with("2026-08-23T00:00:00"),
        "ordinary dates keep anchoring to local midnight; got: {out}"
    );
}
