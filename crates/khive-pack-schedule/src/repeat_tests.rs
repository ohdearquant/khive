use super::*;

fn at(text: &str) -> DateTime<Utc> {
    text.parse().expect("test timestamp")
}

#[test]
fn the_calendar_aliases_parse() {
    assert!(matches!(parse_repeat("daily"), Ok(Repeat::Daily)));
    assert!(matches!(parse_repeat("weekly"), Ok(Repeat::Weekly)));
    assert!(matches!(parse_repeat("monthly"), Ok(Repeat::Monthly)));
}

#[test]
fn intervals_parse_in_seconds_minutes_hours_and_days() {
    for (text, seconds) in [
        ("every:30s", 30),
        ("every:15m", 900),
        ("every:2h", 7_200),
        ("every:1d", 86_400),
    ] {
        match parse_repeat(text) {
            Ok(Repeat::Every(d)) => assert_eq!(d.num_seconds(), seconds, "{text}"),
            other => panic!("{text} must parse as an interval, got {other:?}"),
        }
    }
}

#[test]
fn malformed_intervals_are_refused_with_the_value_named() {
    for text in [
        "every:0s",
        "every:15",
        "every:x",
        "every:1w",
        "every:-5m",
        "every:1.5h",
        "every:",
        "every:99999999999999999d",
    ] {
        let err = parse_repeat(text).expect_err(text);
        assert!(
            err.starts_with(&format!("invalid repeat expression {text:?}")),
            "{text}: {err}"
        );
    }
}

#[test]
fn five_field_cron_parses_and_anything_else_is_refused() {
    assert!(matches!(parse_repeat("0 9 * * 1"), Ok(Repeat::Cron(_))));
    assert!(matches!(parse_repeat("*/15 * * * *"), Ok(Repeat::Cron(_))));
    assert!(matches!(
        parse_repeat("0 9-17 * * 1-5"),
        Ok(Repeat::Cron(_))
    ));
    for text in [
        "99 * * * *",
        "foo bar baz qux zap",
        "0 9 * * 1 2027",
        "* * * *",
        "@daily",
        "hourly",
    ] {
        let err = parse_repeat(text).expect_err(text);
        assert!(err.contains("invalid repeat expression"), "{text}: {err}");
    }
}

#[test]
fn cron_calendar_combinations_must_have_a_real_occurrence() {
    for text in ["0 9 30 2 *", "0 9 31 2 *", "0 9 31 4,6,9,11 *"] {
        let error = parse_repeat(text).expect_err(text);
        assert!(error.contains("no occurrence"), "{text}: {error}");
    }
    assert!(matches!(parse_repeat("0 9 29 2 *"), Ok(Repeat::Cron(_))));
    assert!(matches!(parse_repeat("0 9 L 2 *"), Ok(Repeat::Cron(_))));
}

#[test]
fn next_after_advances_each_form() {
    let base = at("2026-06-01T09:00:00Z");
    assert_eq!(
        parse_repeat("every:15m").unwrap().next_after(base),
        Some(base + Duration::minutes(15))
    );
    // 2026-06-01 is a Monday.
    assert_eq!(
        parse_repeat("0 9 * * 1").unwrap().next_after(base),
        Some(at("2026-06-08T09:00:00Z"))
    );
    assert_eq!(
        parse_repeat("monthly")
            .unwrap()
            .next_after(at("2026-01-31T09:00:00Z")),
        Some(at("2026-02-28T09:00:00Z"))
    );
}

#[test]
fn first_after_lands_strictly_after_now_without_a_burst() {
    let original = at("2026-06-01T09:00:00Z");
    let every = parse_repeat("every:15m").unwrap();
    assert_eq!(
        every.first_after(original, at("2026-06-15T09:07:00Z")),
        Some(at("2026-06-15T09:15:00Z"))
    );
    assert_eq!(
        every.first_after(original, at("2026-06-15T09:15:00Z")),
        Some(at("2026-06-15T09:30:00Z")),
        "an exact multiple still lands strictly after now"
    );
    let cron = parse_repeat("0 9 * * 1").unwrap();
    assert_eq!(
        cron.first_after(original, at("2026-06-17T10:00:00Z")),
        Some(at("2026-06-22T09:00:00Z"))
    );
    let daily = parse_repeat("daily").unwrap();
    assert_eq!(
        daily.first_after(original, at("2026-06-15T09:00:00Z")),
        Some(at("2026-06-16T09:00:00Z"))
    );
}

#[test]
fn no_representable_successor_returns_none_instead_of_panicking() {
    for alias in ["daily", "weekly"] {
        let repeat = parse_repeat(alias).unwrap();
        assert_eq!(repeat.next_after(DateTime::<Utc>::MAX_UTC), None);
        assert_eq!(
            repeat.first_after(DateTime::<Utc>::MAX_UTC, DateTime::<Utc>::MAX_UTC),
            None
        );
        let near_max = DateTime::<Utc>::MAX_UTC - Duration::hours(1);
        assert_eq!(repeat.first_after(near_max, near_max), None);
    }
}
