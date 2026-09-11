//! The recurrence contract, shared by schedule creation and the executor.
//!
//! One parser decides what `repeat` means, so nothing is accepted at write
//! time that the pending-events drain cannot advance, and a stored recurrence
//! never degrades silently to one-shot delivery.

use chrono::{DateTime, Duration, Months, Utc};
use std::str::FromStr;

/// A recurrence the executor can advance.
#[derive(Debug, Clone)]
pub enum Repeat {
    /// The previous trigger plus one day.
    Daily,
    /// The previous trigger plus seven days.
    Weekly,
    /// The previous trigger plus one calendar month.
    Monthly,
    /// A fixed interval from the previous trigger: `every:<N><s|m|h|d>`.
    Every(Duration),
    /// A five-field cron expression, evaluated in UTC.
    Cron(Box<croner::Cron>),
}

/// The forms `repeat` accepts, for error text and documentation.
pub const REPEAT_FORMS: &str = "\"daily\", \"weekly\", \"monthly\", \
     \"every:<N><s|m|h|d>\" (an interval from the previous trigger, N >= 1, \
     e.g. \"every:15m\"), or a five-field cron expression in UTC (e.g. \"0 9 * * 1\")";

/// Parse a `repeat` value into a recurrence the executor can advance.
///
/// The error text names the rejected value and the accepted forms; callers
/// surface it verbatim as their invalid-input message.
pub fn parse_repeat(text: &str) -> Result<Repeat, String> {
    let text = text.trim();
    match text {
        "daily" => return Ok(Repeat::Daily),
        "weekly" => return Ok(Repeat::Weekly),
        "monthly" => return Ok(Repeat::Monthly),
        _ => {}
    }
    if let Some(spec) = text.strip_prefix("every:") {
        return parse_every(spec)
            .map(Repeat::Every)
            .map_err(|why| format!("invalid repeat expression {text:?}: {why}"));
    }
    if text.split_whitespace().count() == 5 {
        return croner::Cron::from_str(text)
            .map(|cron| Repeat::Cron(Box::new(cron)))
            .map_err(|e| format!("invalid repeat expression {text:?}: cron did not parse ({e})"));
    }
    Err(format!(
        "invalid repeat expression {text:?}: supported forms are {REPEAT_FORMS}"
    ))
}

fn parse_every(spec: &str) -> Result<Duration, String> {
    let digits_len = spec
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .len();
    let (digits, unit) = spec.split_at(digits_len);
    let n: i64 = digits.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
        format!("an interval is <N><s|m|h|d> with N >= 1, e.g. \"every:15m\"; got {spec:?}")
    })?;
    let per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        other => {
            return Err(format!(
                "unknown interval unit {other:?}; use s, m, h or d (got {spec:?})"
            ))
        }
    };
    n.checked_mul(per_unit)
        .and_then(Duration::try_seconds)
        .ok_or_else(|| format!("interval {spec:?} is out of range"))
}

impl Repeat {
    /// The occurrence after `current`.
    pub fn next_after(&self, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Repeat::Daily => Some(current + Duration::days(1)),
            Repeat::Weekly => Some(current + Duration::weeks(1)),
            // chrono::Months handles month-boundary arithmetic (Jan 31 + 1
            // month = Feb 28/29).
            Repeat::Monthly => current.checked_add_months(Months::new(1)),
            Repeat::Every(interval) => current.checked_add_signed(*interval),
            Repeat::Cron(cron) => cron.find_next_occurrence(&current, false).ok(),
        }
    }

    /// The first occurrence strictly after `now`, given the missed occurrence
    /// `current`: the missed-event advance without a catch-up burst. An
    /// interval jumps arithmetically and stays phase-locked to the original
    /// trigger; cron asks the pattern from `now`; the calendar aliases step
    /// one occurrence at a time, each strictly later than the last.
    pub fn first_after(&self, current: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Repeat::Every(interval) => {
                let step = interval.num_milliseconds().max(1);
                let elapsed = (now - current).num_milliseconds();
                if elapsed < 0 {
                    return self.next_after(current);
                }
                let skipped = elapsed / step + 1;
                let advance = Duration::milliseconds(step.checked_mul(skipped)?);
                current.checked_add_signed(advance)
            }
            Repeat::Cron(cron) => cron.find_next_occurrence(&now, false).ok(),
            Repeat::Daily | Repeat::Weekly | Repeat::Monthly => {
                let mut current = current;
                loop {
                    let next = self.next_after(current)?;
                    if next > now {
                        return Some(next);
                    }
                    current = next;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
}
