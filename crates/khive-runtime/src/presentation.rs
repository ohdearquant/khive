//! Verb response presentation modes and transformation.
//!
//! Transforms canonical handler output into caller-appropriate form after dispatch
//! and before wire serialization. `Agent` mode abbreviates UUIDs/timestamps and drops
//! empty fields; `Verbose` and `Human` pass through canonical JSON unchanged.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Convert a microsecond epoch `i64` to an RFC 3339 / ISO-8601 string.
///
/// Entity and Note storage uses `i64` microseconds internally; this is the
/// single conversion point before any field reaches the MCP boundary.
///
/// Format: `YYYY-MM-DDTHH:MM:SS.ffffffZ` (SecondsFormat::Micros, UTC `Z`).
pub fn micros_to_iso(micros: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// How the response envelope is presented to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    /// Token-efficient. Default for MCP callers (agents).
    ///
    /// Short UUIDs (8-char), compact timestamps (minute granularity or
    /// relative), empty fields dropped, lifecycle nulls preserved, score
    /// fields truncated to 3 significant figures.
    #[default]
    Agent,
    /// Full canonical shape. Default for `kkernel call` and CI/scripted callers.
    ///
    /// No transformation — handler output passes through as-is.
    Verbose,
    /// Pretty-printed terminal output. Default for `khive` CLI.
    ///
    /// **At the MCP runtime level this is identical to `Verbose`** — the
    /// canonical JSON is returned unchanged. Terminal formatting (relative
    /// timestamps, glyph substitution, table layout) is applied by the CLI
    /// layer (`khive-cli::format::pretty`), not the MCP response pipeline.
    Human,
}

/// Lifecycle `null` fields that are PRESERVED in Agent mode even when null.
///
/// These fields carry lifecycle meaning (absent ≠ null) and must not be dropped.
const LIFECYCLE_NULL_PRESERVE: &[&str] = &[
    "completed_at",
    "deleted_at",
    "due_at",
    "read_at",
    "started_at",
    "superseded_at",
    "applied_at",
    "withdrawn_at",
    "reviewed_at",
    "parent_id",
    "superseded_by",
    "replaced_by",
];

/// Score field names that are truncated to 3 significant figures in Agent mode.
const SCORE_FIELDS: &[&str] = &[
    "score",
    "salience",
    "decay_factor",
    "rrf_score",
    "similarity",
    "cross_encoder_score",
    "graph_proximity_score",
];

/// UUID v4 canonical string length (8-4-4-4-12 = 32 hex + 4 dashes = 36).
const UUID_CANONICAL_LEN: usize = 36;

/// Return true for fields whose whole-string UUID values may be shortened in
/// Agent mode. Content-like fields are intentionally excluded even when their
/// value happens to be UUID-shaped.
///
/// `full_id` is explicitly excluded (P-C1): its purpose is to give callers a
/// stable chaining handle, so shortening it makes it identical to `id` and
/// defeats the field entirely.
fn should_shorten_uuid_field(key: &str) -> bool {
    if key == "full_id" {
        return false;
    }
    key == "id" || key.ends_with("_id") || matches!(key, "superseded_by" | "replaced_by")
}

/// Transform a successful verb result value according to the given
/// [`PresentationMode`].
///
/// - `Verbose` / `Human`: returns `value` unchanged.
/// - `Agent`: applies UUID shortening, timestamp compaction, empty-field
///   dropping, lifecycle-null preservation, and score truncation.
///
/// `now_unix_seconds` is sampled once per response and passed through so all
/// relative datetime renderings within a response use the same instant.
pub fn present(value: Value, mode: PresentationMode, now_unix_seconds: i64) -> Value {
    match mode {
        PresentationMode::Verbose | PresentationMode::Human => value,
        PresentationMode::Agent => {
            let lifecycle_preserve: HashSet<&str> =
                LIFECYCLE_NULL_PRESERVE.iter().copied().collect();
            let score_fields: HashSet<&str> = SCORE_FIELDS.iter().copied().collect();
            transform_agent(
                value,
                &lifecycle_preserve,
                &score_fields,
                now_unix_seconds,
                false,
            )
        }
    }
}

/// Apply the Agent-mode transform to an arbitrary JSON value.
///
/// `inside_properties` is `true` when recursing inside a `"properties"` object.
/// Caller-supplied payload timestamps (e.g. `trigger_at`) must not be compacted
/// because they encode domain semantics the agent may need to round-trip (#546).
fn transform_agent(
    value: Value,
    lifecycle: &HashSet<&str>,
    scores: &HashSet<&str>,
    now: i64,
    inside_properties: bool,
) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let child_inside_properties = inside_properties || k == "properties";
                let transformed =
                    transform_field_agent(&k, v, lifecycle, scores, now, child_inside_properties);
                match transformed {
                    None => {} // drop
                    Some(tv) => {
                        out.insert(k, tv);
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            let items: Vec<Value> = arr
                .into_iter()
                .map(|v| transform_agent(v, lifecycle, scores, now, inside_properties))
                .collect();
            Value::Array(items)
        }
        other => other,
    }
}

/// Transform a single named field value under Agent mode.
///
/// Returns `None` if the field should be dropped.
///
/// `inside_properties` suppresses timestamp compaction for caller-submitted
/// payload values (e.g. `trigger_at` stored under `"properties"`). Metadata
/// timestamps at the top level (`created_at`, `updated_at`) are still compacted.
fn transform_field_agent(
    key: &str,
    value: Value,
    lifecycle: &HashSet<&str>,
    scores: &HashSet<&str>,
    now: i64,
    inside_properties: bool,
) -> Option<Value> {
    match &value {
        // Preserve lifecycle nulls; drop other nulls.
        Value::Null => {
            if lifecycle.contains(key) {
                Some(value)
            } else {
                None
            }
        }
        // Drop empty strings, arrays, objects.
        Value::String(s) if s.is_empty() => None,
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        // Truncate score fields.
        Value::Number(_) if scores.contains(key) => {
            if let Some(f) = value.as_f64() {
                Some(truncate_to_3_sig_figs(f))
            } else {
                Some(value)
            }
        }
        // Shorten UUIDs only in fields whose names carry ID semantics.
        Value::String(s) if is_canonical_uuid(s) && should_shorten_uuid_field(key) => {
            Some(Value::String(s[..8].to_string()))
        }
        // Compact ISO-8601 timestamps only outside caller-supplied payload objects.
        Value::String(s) if !inside_properties && looks_like_iso8601(s) => {
            Some(Value::String(compact_timestamp(s, now)))
        }
        // Recurse into objects and arrays.
        Value::Object(_) | Value::Array(_) => Some(transform_agent(
            value,
            lifecycle,
            scores,
            now,
            inside_properties,
        )),
        // Everything else passes through.
        _ => Some(value),
    }
}

/// Returns `true` if `s` looks like a canonical UUID (36 chars, standard form).
fn is_canonical_uuid(s: &str) -> bool {
    if s.len() != UUID_CANONICAL_LEN {
        return false;
    }
    let b = s.as_bytes();
    // Pattern: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    b[8] == b'-'
        && b[13] == b'-'
        && b[18] == b'-'
        && b[23] == b'-'
        && b[..8].iter().all(|c| c.is_ascii_hexdigit())
        && b[9..13].iter().all(|c| c.is_ascii_hexdigit())
        && b[14..18].iter().all(|c| c.is_ascii_hexdigit())
        && b[19..23].iter().all(|c| c.is_ascii_hexdigit())
        && b[24..].iter().all(|c| c.is_ascii_hexdigit())
}

/// Returns `true` if `s` looks like an ISO-8601 datetime string.
///
/// Heuristic: starts with `YYYY-MM-DDTHH:` (16 chars, proper digit positions).
fn looks_like_iso8601(s: &str) -> bool {
    if s.len() < 16 {
        return false;
    }
    let b = s.as_bytes();
    b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..10].iter().all(|c| c.is_ascii_digit())
        && b[11..13].iter().all(|c| c.is_ascii_digit())
}

/// Compact an ISO-8601 timestamp for Agent mode.
///
/// - Within the last 24 hours: relative form (e.g. `"3m ago"`, `"2h ago"`).
/// - Older: minute-granularity absolute form `"YYYY-MM-DDTHH:MM"`.
fn compact_timestamp(s: &str, now: i64) -> String {
    // Parse Unix seconds from the timestamp if possible; fall back to truncation.
    if let Some(unix) = parse_iso8601_unix(s) {
        let diff = now - unix;
        if (0..86400).contains(&diff) {
            return relative_time(diff);
        }
    }
    // Minute granularity: take the first 16 chars.
    s.chars().take(16).collect()
}

/// Attempt to parse an ISO-8601 datetime string to Unix seconds.
///
/// Only handles the subset produced by khive handlers:
/// `YYYY-MM-DDTHH:MM:SS[.frac][Z]`. Returns `None` for anything we can't parse
/// (graceful degradation — the timestamp is still compacted by truncation).
fn parse_iso8601_unix(s: &str) -> Option<i64> {
    // Minimum parseable: "YYYY-MM-DDTHH:MM:SS"
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    let year: i64 = parse_digits(&b[0..4])?;
    let month: i64 = parse_digits(&b[5..7])?;
    let day: i64 = parse_digits(&b[8..10])?;
    let hour: i64 = parse_digits(&b[11..13])?;
    let minute: i64 = parse_digits(&b[14..16])?;
    let second: i64 = parse_digits(&b[17..19])?;

    // Simple Gregorian → Unix seconds (no timezone offsets other than 'Z').
    // Close enough for relative-time comparisons; not for calendar correctness.
    let days_since_epoch = days_from_civil(year, month, day);
    Some(days_since_epoch * 86400 + hour * 3600 + minute * 60 + second)
}

fn parse_digits(b: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(b).ok()?;
    s.parse().ok()
}

/// Gregorian date → days since 1970-01-01. Algorithm: Howard Hinnant's civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Format a duration in seconds as a relative time string (e.g. `"3m ago"`).
fn relative_time(diff_secs: i64) -> String {
    if diff_secs < 60 {
        format!("{diff_secs}s ago")
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else {
        format!("{}h ago", diff_secs / 3600)
    }
}

/// Truncate a float to 3 significant figures, returning a `serde_json::Value`.
fn truncate_to_3_sig_figs(f: f64) -> Value {
    if f == 0.0 || !f.is_finite() {
        return Value::from(f);
    }
    let magnitude = f.abs().log10().floor() as i32;
    let factor = 10f64.powi(2 - magnitude);
    let rounded = (f * factor).round() / factor;
    // Re-serialize through serde_json to avoid floating-point noise.
    serde_json::Number::from_f64(rounded)
        .map(Value::Number)
        .unwrap_or(Value::from(rounded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A fixed "now" for deterministic tests: 2026-05-23T16:18:00Z ≈ 1748016480.
    const NOW: i64 = 1_748_016_480;

    fn agent(v: Value) -> Value {
        present(v, PresentationMode::Agent, NOW)
    }

    #[test]
    fn verbose_passthrough() {
        let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "title": "X"});
        let out = present(v.clone(), PresentationMode::Verbose, NOW);
        assert_eq!(out, v);
    }

    #[test]
    fn agent_shortens_uuid() {
        let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"});
        let out = agent(v);
        assert_eq!(out["id"], json!("a1b2c3d4"));
    }

    #[test]
    fn agent_drops_empty_string() {
        let v = json!({"title": "ok", "description": ""});
        let out = agent(v);
        assert!(out.get("description").is_none());
        assert_eq!(out["title"], json!("ok"));
    }

    #[test]
    fn agent_drops_empty_array() {
        let v = json!({"tags": [], "title": "ok"});
        let out = agent(v);
        assert!(out.get("tags").is_none());
    }

    #[test]
    fn agent_drops_empty_object() {
        let v = json!({"properties": {}, "title": "ok"});
        let out = agent(v);
        assert!(out.get("properties").is_none());
    }

    #[test]
    fn agent_drops_non_lifecycle_null() {
        let v = json!({"result": null, "title": "ok"});
        let out = agent(v);
        assert!(out.get("result").is_none());
    }

    #[test]
    fn agent_preserves_lifecycle_null() {
        let v = json!({"completed_at": null, "due_at": null, "title": "ok"});
        let out = agent(v);
        assert_eq!(out["completed_at"], json!(null));
        assert_eq!(out["due_at"], json!(null));
    }

    #[test]
    fn agent_preserves_relationship_null() {
        let v = json!({"parent_id": null, "superseded_by": null});
        let out = agent(v);
        assert_eq!(out["parent_id"], json!(null));
        assert_eq!(out["superseded_by"], json!(null));
    }

    #[test]
    fn agent_truncates_score_field() {
        let v = json!({"score": 0.12345678});
        let out = agent(v);
        let s = out["score"].as_f64().unwrap();
        assert!((s - 0.123).abs() < 1e-9, "expected ~0.123, got {s}");
    }

    #[test]
    fn agent_compacts_old_timestamp_to_minutes() {
        // Far past — not within 24h of NOW. Should be truncated to 16 chars.
        let v = json!({"created_at": "2020-01-01T10:30:45.123456Z"});
        let out = agent(v);
        assert_eq!(out["created_at"], json!("2020-01-01T10:30"));
    }

    #[test]
    fn agent_compacts_recent_timestamp_to_relative() {
        // 3 minutes before NOW: diff = 180s.
        let ts_unix = NOW - 180;
        // Format as ISO-8601.
        let ts = unix_to_iso8601(ts_unix);
        let v = json!({"updated_at": ts});
        let out = agent(v);
        assert_eq!(out["updated_at"], json!("3m ago"));
    }

    #[test]
    fn agent_recurses_into_nested_objects() {
        let v = json!({
            "items": [
                {
                    "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
                    "tags": [],
                    "score": 0.9999
                }
            ]
        });
        let out = agent(v);
        let item = &out["items"][0];
        assert_eq!(item["id"], json!("a1b2c3d4"));
        assert!(item.get("tags").is_none());
        let s = item["score"].as_f64().unwrap();
        assert!((s - 1.0).abs() < 1e-9);
    }

    // P-C1 regression: full_id must never be shortened in Agent mode.
    #[test]
    fn agent_preserves_full_id_as_36_chars() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let v = json!({"id": uuid, "full_id": uuid, "title": "X"});
        let out = agent(v);
        // `id` is shortened to 8 chars
        assert_eq!(
            out["id"],
            json!("a1b2c3d4"),
            "id should be 8-char short form"
        );
        // `full_id` must remain the full 36-char UUID
        assert_eq!(
            out["full_id"].as_str().unwrap().len(),
            36,
            "full_id must be 36 chars in agent mode"
        );
        assert_eq!(
            out["full_id"],
            json!(uuid),
            "full_id must equal the original UUID"
        );
        // Verify the invariant: full_id starts with the short id prefix
        assert!(
            out["full_id"]
                .as_str()
                .unwrap()
                .starts_with(out["id"].as_str().unwrap()),
            "full_id must start with the short id prefix"
        );
    }

    #[test]
    fn is_canonical_uuid_recognizes_valid() {
        assert!(is_canonical_uuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert!(!is_canonical_uuid("a1b2c3d4"));
        assert!(!is_canonical_uuid("not-a-uuid-at-all-here---------"));
    }

    #[test]
    fn looks_like_iso8601_recognizes_valid() {
        assert!(looks_like_iso8601("2026-05-23T16:18:15.234567Z"));
        assert!(!looks_like_iso8601("not a timestamp"));
        assert!(!looks_like_iso8601("2026-05-23"));
    }

    /// Format Unix seconds as ISO-8601 for test construction.
    fn unix_to_iso8601(unix: i64) -> String {
        let (y, mo, d, h, mi, s) = unix_to_civil(unix);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }

    fn unix_to_civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
        let s = unix % 86400;
        let days = unix / 86400;
        let h = s / 3600;
        let m = (s % 3600) / 60;
        let sec = s % 60;
        // Howard Hinnant civil_from_days
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        (y, mo, d, h, m, sec)
    }

    #[test]
    fn agent_does_not_shorten_uuid_shaped_content_fields() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let out = agent(json!({
            "id": uuid,
            "full_id": uuid,
            "content": uuid,
            "description": uuid,
            "title": uuid,
            "query": uuid,
        }));

        assert_eq!(out["id"], json!("a1b2c3d4"));
        assert_eq!(out["full_id"], json!(uuid));
        assert_eq!(out["content"], json!(uuid));
        assert_eq!(out["description"], json!(uuid));
        assert_eq!(out["title"], json!(uuid));
        assert_eq!(out["query"], json!(uuid));
    }

    #[test]
    fn agent_shortens_suffix_id_fields() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let out = agent(json!({
            "note_id": uuid,
            "source_id": uuid,
            "target_id": uuid,
        }));

        assert_eq!(out["note_id"], json!("a1b2c3d4"));
        assert_eq!(out["source_id"], json!("a1b2c3d4"));
        assert_eq!(out["target_id"], json!("a1b2c3d4"));
    }
}
