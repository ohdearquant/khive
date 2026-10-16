//! Latest-due selection uses the production recurrence implementation.

use chrono::{DateTime, Duration, Utc};
use khive_pack_schedule::repeat::{parse_repeat, Repeat};

fn at(text: &str) -> DateTime<Utc> {
    text.parse().expect("timestamp")
}

#[test]
fn fixed_intervals_select_only_the_latest_due_and_preserve_trigger_precision() {
    let trigger = at("2020-01-01T09:00:00.123456789Z");
    for (repeat, period, steps) in [
        ("daily", 86_400, 2000),
        ("weekly", 604_800, 300),
        ("every:15m", 900, 100000),
    ] {
        let recurrence = parse_repeat(repeat).expect("repeat");
        let latest = trigger + Duration::seconds(period * steps);
        assert_eq!(
            recurrence.latest_at_or_before(trigger, latest + Duration::seconds(1), None),
            Some(latest)
        );
        assert_eq!(
            recurrence.latest_at_or_before(trigger, latest, None),
            Some(latest)
        );
        assert_eq!(
            recurrence.latest_at_or_before(trigger, latest - Duration::nanoseconds(1), None),
            Some(latest - Duration::seconds(period))
        );
    }
}

#[test]
fn monthly_selection_uses_original_anchor_after_february_clamping() {
    for (anchor, february) in [
        ("2025-01-31T09:00:00Z", "2025-02-28T09:00:00Z"),
        ("2024-01-31T09:00:00Z", "2024-02-29T09:00:00Z"),
    ] {
        let anchor = at(anchor);
        let february = at(february);
        let march = anchor
            .checked_add_months(chrono::Months::new(2))
            .expect("March");
        assert_eq!(
            Repeat::Monthly.latest_at_or_before(
                february,
                march - Duration::nanoseconds(1),
                Some(anchor)
            ),
            Some(february)
        );
        assert_eq!(
            Repeat::Monthly.latest_at_or_before(february, march, Some(anchor)),
            Some(march)
        );
    }
}

#[test]
fn cron_includes_off_pattern_armed_trigger_before_its_first_match() {
    let repeat = parse_repeat("0 9 * * 1").expect("Monday cron");
    let trigger = at("2026-10-13T12:34:56.123456789Z"); // Tuesday.
    assert_eq!(
        repeat.latest_at_or_before(trigger, at("2026-10-18T23:59:59Z"), None),
        Some(trigger)
    );
    assert_eq!(
        repeat.latest_at_or_before(trigger, at("2026-10-19T09:00:00Z"), None),
        Some(at("2026-10-19T09:00:00Z"))
    );
    assert_eq!(
        repeat.latest_at_or_before(trigger, at("2026-10-26T09:00:01Z"), None),
        Some(at("2026-10-26T09:00:00Z"))
    );
}

#[test]
fn future_triggers_and_invalid_internal_intervals_have_no_due_occurrence() {
    let trigger = at("2026-10-14T09:00:00Z");
    for repeat in [
        Repeat::Daily,
        Repeat::Weekly,
        Repeat::Monthly,
        Repeat::Every(Duration::seconds(1)),
    ] {
        assert_eq!(
            repeat.latest_at_or_before(trigger, trigger - Duration::nanoseconds(1), None),
            None
        );
        assert_eq!(
            repeat.latest_at_or_before(trigger, trigger, None),
            Some(trigger)
        );
    }
    for interval in [
        Duration::zero(),
        Duration::seconds(-1),
        Duration::milliseconds(1500),
    ] {
        assert_eq!(
            Repeat::Every(interval).latest_at_or_before(trigger, trigger, None),
            None
        );
    }
    assert_eq!(
        Repeat::Monthly.latest_at_or_before(trigger, trigger, Some(trigger + Duration::seconds(1))),
        None
    );
}
