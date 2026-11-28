use chrono::Utc;
use serde_json::Value;

use crate::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use crate::time_anchor::anchor_date_to_earliest_instant;
use crate::{EventActorReadScope, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

pub(super) fn parse_handler_params<T: serde::de::DeserializeOwned>(
    params: Value,
) -> Result<T, String> {
    serde_json::from_value(params).map_err(|error| {
        mask_for_redaction_surface(RedactionSurface::GateProbe, &error.to_string()).into_owned()
    })
}

pub(super) fn parse_event_kind(raw: &str) -> Result<khive_types::EventKind, RuntimeError> {
    raw.parse::<khive_types::EventKind>().map_err(|mut error| {
        // EventKind normalizes case before constructing its error. Inspect the
        // original operand so that case-sensitive credentials remain detectable.
        let masked = mask_for_redaction_surface(RedactionSurface::GateProbe, raw);
        if masked.as_ref() != raw {
            error.value = masked.into_owned();
        }
        let message = format!("invalid `kind`: {error}");
        RuntimeError::InvalidInput(
            mask_for_redaction_surface(RedactionSurface::GateProbe, &message).into_owned(),
        )
    })
}

/// Parse an ISO-8601/RFC-3339 datetime string into a microsecond epoch,
/// naming the offending field/value in the error rather than a bare parse
/// failure (`brain.event_counts`, ADR-103 Stage 1).
///
/// A bare `YYYY-MM-DD` date (no time-of-day component) is accepted rather
/// than rejected — RFC-3339 requires a time component, but a date-only value
/// is unambiguous and a common caller shorthand; the alternative is silently
/// returning nothing until the caller happens to probe the full timestamp
/// form (#984). It anchors to the earliest instant of that calendar date in
/// `tz`, the configured display timezone (ADR-169 D1) — the same rule
/// `gtd`'s date-only `due` follows. The earlier form anchored to midnight
/// UTC, which put the window's edges up to a day off the calendar the caller
/// was naming.
///
/// `roll_to_next_day` handles the `until` bound: the window is applied as
/// half-open `[since, until)` (see `EventCounts` handler), so a date-only
/// `since` correctly means "that day's start" but a date-only `until`
/// must mean "the end of that day" — i.e. the *next* day's earliest
/// instant — otherwise the exclusive upper bound drops the entire named
/// day (#994).
pub(super) fn parse_rfc3339_micros(
    field: &'static str,
    value: &str,
    roll_to_next_day: bool,
    tz: chrono_tz::Tz,
) -> Result<i64, RuntimeError> {
    let trimmed = value.trim();
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let date = if roll_to_next_day {
            // Fallible at chrono's representable max (NaiveDate::MAX,
            // 262142-12-31): the roll would overflow. Return the named
            // validation error rather than panicking on caller input (#994).
            date.checked_add_days(chrono::Days::new(1)).ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "invalid `{field}`: {value:?} is past the maximum representable date"
                ))
            })?
        } else {
            date
        };
        return anchor_date_to_earliest_instant(date, tz)
            .map(|dt| dt.timestamp_micros())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "invalid `{field}`: {value:?} does not exist in the configured display \
                     timezone {tz} (no representable instant carries that local date)"
                ))
            });
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "invalid `{field}`: {value:?} is not a valid ISO-8601/RFC-3339 datetime ({e}); \
                 expected e.g. \"2026-07-01T00:00:00Z\""
            ))
        })
}

pub(super) fn event_actor_read_scope(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    actor: Option<&str>,
    all_actors: bool,
) -> RuntimeResult<EventActorReadScope> {
    crate::resolve_event_actor_read_scope(
        token,
        &runtime.config().brain.fleet_readers,
        actor,
        all_actors,
    )
}
