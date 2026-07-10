//! Verb response presentation modes and transformation.
//!
//! Transforms canonical handler output into caller-appropriate form after dispatch
//! and before wire serialization. `Agent` mode abbreviates UUIDs/timestamps and drops
//! empty fields; `Verbose` and `Human` pass through canonical JSON unchanged.
//!
//! This module also contains the `OutputFormat` axis (ADR-078) which governs how
//! the resulting `serde_json::Value` is serialized or rendered to an output string.
//! `PresentationMode` and `OutputFormat` compose independently.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ── OutputFormat ─────────────────────────────────────────────────────────────

/// Output serialization format for verb results (ADR-078).
///
/// Orthogonal to [`PresentationMode`]: `PresentationMode` controls field-level
/// transforms (UUID shortening, timestamp compaction, empty-field dropping);
/// `OutputFormat` controls how the resulting `serde_json::Value` is serialized
/// or rendered to the wire string.
///
/// Default is [`OutputFormat::Json`] on every surface: compact, lossless,
/// shape-stable machine contract.
///
/// Note: `Yaml` is a clean follow-up — implemented as a 3-variant enum
/// (`Json`, `Auto`, `Table`) per ADR-078 §"yaml" which permits omission when
/// the in-tree emitter would balloon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Compact JSON (`serde_json::to_string`). Lossless machine contract. Default.
    #[default]
    Json,
    /// Shape-aware: markdown table for homogeneous record arrays,
    /// flat key-value block for single records, compact-JSON fallback.
    Auto,
    /// Force the markdown-table renderer regardless of detected shape.
    Table,
}

/// Cell truncation limit for markdown-table rendering (ADR-078 §3a).
const CELL_TRUNCATE: usize = 120;

// ── Public render entry point ────────────────────────────────────────────────

/// Render a successful verb result value to a wire string using the given format.
///
/// Called at the single serialization seam (ADR-078 §9) AFTER all `$prev` chain
/// resolution and AFTER the [`PresentationMode`] transform.
///
/// Error envelopes (`ok=false`) are never passed here — the caller must handle
/// them as compact JSON directly (ADR-078 §8.2).
///
/// When `format` is [`OutputFormat::Json`], returns compact JSON (`serde_json::to_string`).
/// When `format` is [`OutputFormat::Auto`] or [`OutputFormat::Table`], applies the
/// redundancy-reduction pre-pass (§7) — unless `presentation` is [`PresentationMode::Verbose`]
/// — then dispatches to the shape-aware renderer.
pub fn render_format(value: Value, format: OutputFormat, presentation: PresentationMode) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
        OutputFormat::Auto | OutputFormat::Table => {
            // Redundancy-reduction pre-pass (§7): skipped in Verbose mode.
            let reduced = if presentation == PresentationMode::Verbose {
                value
            } else {
                apply_redundancy_drop(value)
            };
            match format {
                OutputFormat::Auto => render_auto(reduced),
                OutputFormat::Table => render_table_forced(reduced),
                OutputFormat::Json => unreachable!(),
            }
        }
    }
}

// ── Redundancy-reduction pre-pass (ADR-078 §7) ──────────────────────────────

/// Apply the view-only redundancy-reduction pre-pass (ADR-078 §7) to a value.
///
/// Applies at most ONE pass over the value. This function is the canonical
/// entry for the pre-pass; the per-record logic lives in `drop_record`.
///
/// Applied only when `format` ∈ {`auto`, `table`} AND `presentation` ≠ `Verbose`.
/// Callers are responsible for checking those conditions; this function applies
/// unconditionally.
pub fn apply_redundancy_drop(value: Value) -> Value {
    match value {
        Value::Object(_) => drop_record(value),
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|v| if v.is_object() { drop_record(v) } else { v })
                .collect(),
        ),
        other => other,
    }
}

/// Apply per-record redundancy rules (§7.1, §7.2, §7.3) to a single record object.
fn drop_record(value: Value) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };

    // §7.1: suppress `full_id`.
    map.remove("full_id");

    // §7.3: elide `namespace` when its value is `"local"`.
    if map.get("namespace").and_then(Value::as_str) == Some("local") {
        map.remove("namespace");
    }

    // §7.2: properties dedup — remove key-value pairs from `properties` that
    // have an identical counterpart at the top level of this record.
    let props_val = map.remove("properties");
    if let Some(Value::Object(props)) = props_val {
        let mut new_props = Map::new();
        for (k, v) in props {
            if map.get(&k) != Some(&v) {
                new_props.insert(k, v);
            }
        }
        if !new_props.is_empty() {
            map.insert("properties".to_string(), Value::Object(new_props));
        }
    } else if let Some(other) = props_val {
        map.insert("properties".to_string(), other);
    }

    // Recurse into array values so nested record arrays are also reduced.
    let out: Map<String, Value> = map
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                Value::Array(arr) => Value::Array(
                    arr.into_iter()
                        .map(|item| {
                            if item.is_object() {
                                drop_record(item)
                            } else {
                                item
                            }
                        })
                        .collect(),
                ),
                other => other,
            };
            (k, v)
        })
        .collect();
    Value::Object(out)
}

// ── Shape-aware rendering (`auto`) ──────────────────────────────────────────

/// Render a value using shape-aware dispatch (ADR-078 §3).
fn render_auto(value: Value) -> String {
    // Shape (a): homogeneous record array.
    if let Some((records, keys)) = find_record_array(&value) {
        return render_table(&records, &keys);
    }
    // Shape (b): single record / heterogeneous object.
    if value.is_object() {
        return render_kv_block(&value, 0);
    }
    // Shape (c): compact-JSON fallback.
    serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
}

/// Force the markdown-table renderer regardless of detected shape (ADR-078 §6).
fn render_table_forced(value: Value) -> String {
    if let Some((records, keys)) = find_record_array(&value) {
        return render_table(&records, &keys);
    }
    // No record array detected — fallback to compact JSON per §8.3.
    serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
}

/// Find the first homogeneous record array in `value`.
///
/// Checks:
/// 1. `value` itself is an array of 2+ objects.
/// 2. `value` is an object with a key whose value is an array of 2+ objects.
///
/// Returns `(records_cloned, ordered_column_keys)` when found.
fn find_record_array(value: &Value) -> Option<(Vec<Value>, Vec<String>)> {
    match value {
        Value::Array(arr) if arr.len() >= 2 && arr.iter().all(Value::is_object) => {
            let keys = collect_keys(arr);
            Some((arr.clone(), keys))
        }
        Value::Object(map) => {
            for v in map.values() {
                if let Value::Array(arr) = v {
                    if arr.len() >= 2 && arr.iter().all(Value::is_object) {
                        let keys = collect_keys(arr);
                        return Some((arr.clone(), keys));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Collect column names in first-seen order across all records.
fn collect_keys(records: &[Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for record in records {
        if let Value::Object(map) = record {
            for k in map.keys() {
                if seen.insert(k.clone()) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys
}

// ── Markdown table renderer (ADR-078 §3a) ───────────────────────────────────

/// Render a record array as a GitHub-Flavored Markdown table.
fn render_table(records: &[Value], keys: &[String]) -> String {
    let mut out = String::new();

    // Header row.
    out.push('|');
    for k in keys {
        out.push(' ');
        out.push_str(k);
        out.push_str(" |");
    }
    out.push('\n');

    // Separator row.
    out.push('|');
    for _ in keys {
        out.push_str("---|");
    }
    out.push('\n');

    // Data rows.
    for record in records {
        out.push('|');
        for k in keys {
            let cell = record.get(k).unwrap_or(&Value::Null);
            let text = cell_text(cell);
            out.push(' ');
            out.push_str(&text);
            out.push_str(" |");
        }
        out.push('\n');
    }

    out
}

/// Format a cell value: escape `|`, collapse newlines, truncate to ~120 chars.
fn cell_text(value: &Value) -> String {
    let raw = match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Arrays and nested objects use compact JSON in the cell.
        other => serde_json::to_string(other).unwrap_or_default(),
    };

    // Escape literal `|` and collapse embedded newlines to a space.
    let escaped = raw.replace('|', "\\|").replace(['\n', '\r'], " ");

    // Truncate to approximately CELL_TRUNCATE *characters* (char boundary,
    // not byte index — slicing on a byte offset can panic on multi-byte chars).
    let char_count = escaped.chars().count();
    if char_count > CELL_TRUNCATE {
        let truncated: String = escaped.chars().take(CELL_TRUNCATE).collect();
        format!("{truncated}...")
    } else {
        escaped
    }
}

// ── Flat key-value block renderer (ADR-078 §3b) ─────────────────────────────

/// Render a single record or heterogeneous object as a flat key-value block.
fn render_kv_block(value: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match value {
        Value::Object(map) => {
            let mut out = String::new();
            for (k, v) in map {
                match v {
                    Value::Object(_) => {
                        out.push_str(&format!("{}{}:\n", indent, k));
                        out.push_str(&render_kv_block(v, depth + 1));
                    }
                    Value::Array(arr) if arr.iter().any(Value::is_object) => {
                        out.push_str(&format!("{}{}:\n", indent, k));
                        for item in arr {
                            if item.is_object() {
                                out.push_str(&render_kv_block(item, depth + 1));
                            } else {
                                out.push_str(&format!("{}  - {}\n", indent, cell_text(item)));
                            }
                        }
                    }
                    _ => {
                        out.push_str(&format!("{}{}: {}\n", indent, k, cell_text(v)));
                    }
                }
            }
            out
        }
        other => format!("{}{}\n", indent, cell_text(other)),
    }
}

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
    /// Full canonical shape. Default for `kkernel exec` and CI/scripted callers.
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
/// `YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM|±HHMM]`. Returns `None` for anything
/// we can't parse (graceful degradation — the timestamp is still compacted
/// by truncation).
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

    // Simple Gregorian → local-wall-clock Unix seconds, then adjust for any
    // trailing timezone offset (see `parse_tz_offset_secs`) to get the
    // actual UTC instant.
    let days_since_epoch = days_from_civil(year, month, day);
    let local = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second;
    let offset_secs = parse_tz_offset_secs(&s[19..])?;
    Some(local - offset_secs)
}

/// Parse the tail of an ISO-8601 timestamp (everything from byte index 19
/// onward, i.e. after the whole-seconds field) into a UTC offset in seconds.
///
/// Handles, in order: optional fractional seconds (`.nnn`, skipped — this
/// parser only has whole-second precision), then one of:
/// - empty string or `"Z"` → offset 0
/// - `±HH:MM` or the compact `±HHMM` form → `sign * (hh*3600 + mm*60)`
///
/// Returns `None` for anything else (malformed tail).
fn parse_tz_offset_secs(tail: &str) -> Option<i64> {
    let mut rest = tail;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if frac_len == 0 {
            return None;
        }
        rest = &after_dot[frac_len..];
    }

    if rest.is_empty() || rest == "Z" {
        return Some(0);
    }

    let sign: i64 = match rest.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &rest[1..];
    let (hh, mm) = match digits.len() {
        // "HH:MM"
        5 if digits.as_bytes()[2] == b':' => (
            parse_digits(&digits.as_bytes()[0..2])?,
            parse_digits(&digits.as_bytes()[3..5])?,
        ),
        // "HHMM"
        4 => (
            parse_digits(&digits.as_bytes()[0..2])?,
            parse_digits(&digits.as_bytes()[2..4])?,
        ),
        _ => return None,
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60))
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

    // ── ADR-078: OutputFormat tests ───────────────────────────────────────────

    /// (a) json format preserves full shape (no field dropped, no transformation).
    #[test]
    fn format_json_preserves_full_shape() {
        let v = json!({
            "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "namespace": "local",
            "properties": {"k": "v"},
            "title": "test"
        });
        let rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Agent);
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        // full_id must NOT be dropped in json mode (§P-C1, §4).
        assert!(
            parsed.get("full_id").is_some(),
            "json mode must keep full_id"
        );
        // namespace must NOT be elided in json mode.
        assert_eq!(
            parsed.get("namespace").and_then(Value::as_str),
            Some("local")
        );
        // properties must NOT be deduped in json mode.
        assert!(parsed.get("properties").is_some());
    }

    /// (a-vs-auto) auto mode drops redundant fields that json mode preserves.
    #[test]
    fn format_auto_drops_versus_json_keeps() {
        let v = json!({
            "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "namespace": "local",
            "title": "test"
        });
        let json_rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Agent);
        let auto_rendered = render_format(v.clone(), OutputFormat::Auto, PresentationMode::Agent);
        // json keeps both; auto drops namespace="local" and full_id.
        let json_parsed: Value = serde_json::from_str(&json_rendered).unwrap();
        assert!(
            json_parsed.get("full_id").is_some(),
            "json must keep full_id"
        );
        assert_eq!(
            json_parsed.get("namespace").and_then(Value::as_str),
            Some("local")
        );
        // Auto mode: namespace should be elided (redundancy §7.3), full_id dropped (§7.1).
        // The value itself is a single record → rendered as kv block.
        assert!(
            !auto_rendered.contains("full_id"),
            "auto kv block must drop full_id"
        );
        assert!(
            !auto_rendered.contains("namespace"),
            "auto kv block must elide namespace=local"
        );
    }

    /// (b1) homogeneous record array → markdown table with header + separator + rows.
    #[test]
    fn format_auto_homogeneous_array_renders_markdown_table() {
        let v = json!([
            {"id": "abc", "title": "First"},
            {"id": "def", "title": "Second"}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        // Header row.
        assert!(rendered.starts_with('|'), "must start with |");
        assert!(
            rendered.contains("| id |") || rendered.contains("| id"),
            "must have id column"
        );
        assert!(rendered.contains("title"), "must have title column");
        // Separator row.
        assert!(rendered.contains("|---|"), "must have separator row");
        // Data rows.
        assert!(rendered.contains("abc"), "must have first row data");
        assert!(rendered.contains("Second"), "must have second row data");
    }

    /// (b2) single record → flat kv block.
    #[test]
    fn format_auto_single_record_renders_kv_block() {
        let v = json!({"id": "abc", "title": "Hello World"});
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        // kv block uses "key: value\n" format.
        assert!(rendered.contains("id: abc"), "must have id: abc");
        assert!(
            rendered.contains("title: Hello World"),
            "must have title line"
        );
        // Must NOT contain markdown table markers.
        assert!(
            !rendered.starts_with('|'),
            "single record must not be a markdown table"
        );
    }

    /// (b3) fallback: auto on heterogeneous/scalar value falls back to compact json.
    #[test]
    fn format_auto_scalar_fallback_compact_json() {
        let v = json!(42);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert_eq!(rendered, "42");
    }

    /// (c) table format forces markdown table even when shape would normally be kv.
    #[test]
    fn format_table_forces_markdown_when_array() {
        let v = json!({
            "items": [
                {"name": "A", "score": 1},
                {"name": "B", "score": 2}
            ]
        });
        let rendered = render_format(v, OutputFormat::Table, PresentationMode::Agent);
        assert!(
            rendered.contains("|"),
            "table format must produce markdown table"
        );
        assert!(rendered.contains("name"), "must have name column");
        assert!(rendered.contains("score"), "must have score column");
    }

    /// (c-fallback) table format falls back to compact json when no record array found.
    #[test]
    fn format_table_falls_back_to_json_when_no_array() {
        let v = json!({"single": "value"});
        let rendered = render_format(v, OutputFormat::Table, PresentationMode::Agent);
        // No record array → compact JSON fallback.
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["single"], json!("value"));
    }

    /// (d) redundancy-drop: auto/table skipped in Verbose mode (§7).
    #[test]
    fn format_auto_verbose_skips_redundancy_drop() {
        let v = json!({
            "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "namespace": "local",
            "title": "test"
        });
        // In Verbose mode, redundancy drop must be skipped.
        // The value is a single object → kv block, but full_id and namespace stay.
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Verbose);
        assert!(
            rendered.contains("full_id"),
            "verbose must preserve full_id"
        );
        assert!(
            rendered.contains("namespace"),
            "verbose must preserve namespace"
        );
    }

    /// (e) error envelope invariant: ok=false entries must stay compact json under auto.
    ///
    /// This tests the render_result logic via the redundancy-drop + render path.
    /// The server-level render_result function is tested indirectly: we verify that
    /// apply_redundancy_drop does NOT touch "ok: false" envelopes (the actual
    /// invariant is enforced by render_result checking the ok flag before dispatching).
    #[test]
    fn redundancy_drop_does_not_corrupt_error_shape() {
        // An error envelope that might be passed to the pre-pass in a batch.
        let v = json!({"ok": false, "error": "something failed", "namespace": "local"});
        // apply_redundancy_drop is a pure value transform; it doesn't know about ok.
        // The caller (render_result in server.rs) is responsible for bypassing
        // auto-render on ok=false entries. Here we just verify the pre-pass
        // doesn't lose the error field.
        let reduced = apply_redundancy_drop(v.clone());
        assert!(
            reduced.get("error").is_some(),
            "redundancy drop must preserve error field"
        );
        assert_eq!(
            reduced.get("ok").and_then(Value::as_bool),
            Some(false),
            "redundancy drop must preserve ok=false"
        );
    }

    /// Properties dedup removes only keys that match a top-level sibling exactly.
    #[test]
    fn redundancy_drop_properties_dedup() {
        let v = json!({
            "id": "abc",
            "title": "Same",
            "properties": {
                "title": "Same",  // duplicate → removed
                "extra": "unique" // not at top level → kept
            }
        });
        let reduced = apply_redundancy_drop(v);
        let props = reduced.get("properties").expect("properties must remain");
        assert!(props.get("extra").is_some(), "unique property must be kept");
        assert!(
            props.get("title").is_none(),
            "duplicate top-level property must be removed"
        );
    }

    /// Cell truncation: text > 120 chars gets `...` appended.
    #[test]
    fn cell_text_truncates_long_values() {
        let long = "X".repeat(200);
        let v = json!([
            {"col": long.clone()},
            {"col": "short"}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        // Cell must be truncated to ~120 chars + "..."
        assert!(
            rendered.contains("..."),
            "long cell must be truncated with ..."
        );
        assert!(
            !rendered.contains(&long),
            "full long string must not appear in table"
        );
    }

    /// Cell truncation must not panic on multi-byte UTF-8 characters (High 3).
    ///
    /// A string of 119 ASCII bytes followed by a 3-byte CJK character and more
    /// text has `len() > 120` but byte index 120 falls inside the CJK char.
    /// The old byte-slice truncation would panic; char-boundary truncation is safe.
    #[test]
    fn cell_text_truncation_is_utf8_safe() {
        // 119 ASCII 'a' bytes, then CJK char U+4E2D (3 bytes each), then more text.
        // Total byte length: 119 + 3 * 10 + 5 > 120, but byte 120 is inside a CJK char.
        let prefix = "a".repeat(119);
        let suffix = "中".repeat(10); // each '中' is 3 bytes
        let long_multibyte = format!("{prefix}{suffix}trailing");
        let v = json!([
            {"col": long_multibyte.clone()},
            {"col": "ok"}
        ]);
        // Must not panic — this was the bug.
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            rendered.contains("..."),
            "multibyte cell must be truncated with ..."
        );
        // The rendered string must be valid UTF-8 (no partial char slicing).
        assert!(
            std::str::from_utf8(rendered.as_bytes()).is_ok(),
            "rendered output must be valid UTF-8"
        );
    }

    // --- parse_iso8601_unix / relative-time offset handling (#754) ---

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
    fn parse_iso8601_unix_bare_form_unchanged() {
        // No trailing Z/offset at all: existing "no offset" behavior preserved.
        assert_eq!(
            parse_iso8601_unix("2026-07-09T15:55:00"),
            parse_iso8601_unix("2026-07-09T15:55:00Z")
        );
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
    fn compact_timestamp_offset_bearing_future_time_not_shown_as_ago() {
        // Regression for #754: a wall-clock-identical-to-NOW timestamp that
        // carries a "-02:00" offset is actually 2h in the future relative to
        // NOW (2025-05-23T16:08:00Z). The old offset-naive parser treated
        // the wall-clock digits as UTC and reported "0s ago"; the fixed
        // parser must not.
        let out = compact_timestamp("2025-05-23T16:08:00-02:00", NOW);
        assert_ne!(out, "0s ago");
        assert_eq!(out, "2025-05-23T16:08");
    }

    #[test]
    fn compact_timestamp_offset_bearing_past_time_renders_relative() {
        // "20:05+04:00" == "16:05Z", which is 3 minutes before NOW
        // (2025-05-23T16:08:00Z). Correct offset handling must produce
        // "3m ago"; the old offset-naive parser would compare wall-clock
        // 20:05 against NOW directly, landing outside the 24h window and
        // falling back to truncated absolute form instead.
        let out = compact_timestamp("2025-05-23T20:05:00+04:00", NOW);
        assert_eq!(out, "3m ago");
    }
}
