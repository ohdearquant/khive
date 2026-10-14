//! The recurrence contract, shared by schedule creation and the executor.
//!
//! One parser decides what `repeat` means, so nothing is accepted at write
//! time that the pending-events drain cannot advance, and a stored recurrence
//! never degrades silently to one-shot delivery.

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc};
use std::str::FromStr;

/// A recurrence the executor can advance.
#[derive(Debug, Clone)]
pub enum Repeat {
    /// The previous trigger plus one day.
    Daily,
    /// The previous trigger plus seven days.
    Weekly,
    /// A calendar month from the row's fixed anchor, clamped independently.
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
        let cron = croner::Cron::from_str(text)
            .map_err(|e| format!("invalid repeat expression {text:?}: cron did not parse ({e})"))?;
        if !cron_has_occurrence(&cron).map_err(|e| {
            format!("invalid repeat expression {text:?}: cron could not be evaluated ({e})")
        })? {
            return Err(format!(
                "invalid repeat expression {text:?}: cron has no occurrence in a Gregorian 400-year cycle"
            ));
        }
        return Ok(Repeat::Cron(Box::new(cron)));
    }
    Err(format!(
        "invalid repeat expression {text:?}: supported forms are {REPEAT_FORMS}"
    ))
}

/// Five-field cron has no year selector. Gregorian dates (including weekday,
/// leap-day and month-end patterns) repeat every 400 years, so one fixed cycle
/// proves whether the expression can ever fire. Check one permitted time on
/// each real date rather than asking croner to search toward its year-5000
/// limit for an impossible combination such as February 30.
fn cron_has_occurrence(cron: &croner::Cron) -> Result<bool, croner::errors::CronError> {
    let first_matching =
        |limit, matches: &dyn Fn(u32) -> Result<bool, croner::errors::CronError>| {
            (0..limit)
                .map(|value| matches(value).map(|yes| yes.then_some(value)))
                .find_map(|result| match result {
                    Ok(Some(value)) => Some(Ok(value)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                })
                .transpose()
        };
    let Some(hour) = first_matching(24, &|value| cron.pattern.hour_match(value))? else {
        return Ok(false);
    };
    let Some(minute) = first_matching(60, &|value| cron.pattern.minute_match(value))? else {
        return Ok(false);
    };
    let Some(second) = first_matching(60, &|value| cron.pattern.second_match(value))? else {
        return Ok(false);
    };

    for year in 2000..2400 {
        if !cron.pattern.year_match(year)? {
            continue;
        }
        for month in 1..=12 {
            if !cron.pattern.month_match(month)? {
                continue;
            }
            for day in 1..=31 {
                let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
                    break;
                };
                let at = date
                    .and_hms_opt(hour, minute, second)
                    .expect("validated time fields are in range")
                    .and_utc();
                if cron.is_time_matching(&at)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
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
    /// Latest occurrence at or before `now`, with the stored trigger included.
    /// Monthly series use their original anchor; cron preserves an off-pattern
    /// initial trigger. This does not change the legacy advancement methods.
    pub fn latest_at_or_before(
        &self,
        current: DateTime<Utc>,
        now: DateTime<Utc>,
        monthly_anchor: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        if now < current {
            return None;
        }
        match self {
            Repeat::Daily | Repeat::Weekly | Repeat::Every(_) => {
                let seconds = match self {
                    Repeat::Daily => 86_400,
                    Repeat::Weekly => 604_800,
                    Repeat::Every(interval) => {
                        let seconds = interval.num_seconds();
                        if Duration::try_seconds(seconds) != Some(*interval) {
                            return None;
                        }
                        seconds
                    }
                    _ => unreachable!(),
                };
                if seconds <= 0 {
                    return None;
                }
                let steps = now.signed_duration_since(current).num_seconds() / seconds;
                current.checked_add_signed(Duration::try_seconds(seconds.checked_mul(steps)?)?)
            }
            Repeat::Monthly => {
                let anchor = monthly_anchor.unwrap_or(current);
                if anchor > current {
                    return None;
                }
                let months = i64::from(now.year() - anchor.year()) * 12 + i64::from(now.month())
                    - i64::from(anchor.month());
                let months = u32::try_from(months).ok()?;
                let candidate = anchor.checked_add_months(Months::new(months))?;
                let latest = if candidate > now {
                    anchor.checked_add_months(Months::new(months.checked_sub(1)?))?
                } else {
                    candidate
                };
                Some(latest.max(current))
            }
            Repeat::Cron(cron) => {
                let previous = cron.find_previous_occurrence(&now, true).ok()?;
                Some(previous.max(current))
            }
        }
    }

    /// The occurrence after `current`. For a stored monthly series, use
    /// `first_after` with its fixed anchor once the first month has clamped.
    pub fn next_after(&self, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Repeat::Daily => current.checked_add_signed(Duration::days(1)),
            Repeat::Weekly => current.checked_add_signed(Duration::weeks(1)),
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
    /// trigger; cron asks the pattern from `now`; daily and weekly step one
    /// occurrence at a time. For monthly rows, `current` is the original
    /// anchor, and each candidate is that anchor plus n calendar months.
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
            Repeat::Monthly => {
                let mut months = 1u32;
                loop {
                    let next = current.checked_add_months(Months::new(months))?;
                    if next > now {
                        return Some(next);
                    }
                    months = months.checked_add(1)?;
                }
            }
            Repeat::Daily | Repeat::Weekly => {
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
#[path = "repeat_tests.rs"]
mod tests;
