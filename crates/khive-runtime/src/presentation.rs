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
    /// Shape-aware: markdown table for homogeneous record arrays (with
    /// envelope siblings preserved as trailing `key: value` lines),
    /// compact-JSON fallback for every other shape.
    Auto,
    /// Force the markdown-table renderer regardless of detected shape.
    /// Since the kv-block renderer was removed (ADR-078 §3 amendment),
    /// `Table` and `Auto` share the same dispatch.
    Table,
}

/// Cell truncation limit for markdown-table rendering (ADR-078 §3a).
const CELL_TRUNCATE: usize = 120;

/// Scalar payload fields hoisted from `properties` to the top level in the
/// auto/table pre-pass, when no top-level sibling of that name exists.
/// View-only (never applied to `json`): without the hoist, a table row for a
/// scheduled event cannot say when it fires or whether it is pending —
/// those fields exist only inside the nested `properties` bag.
const PROPERTY_HOIST_FIELDS: &[&str] = &["trigger_at", "due", "status"];

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
/// — then dispatches to the shape-aware renderer. Verbose also disables cell
/// truncation in the table renderer (§3a).
pub fn render_format(value: Value, format: OutputFormat, presentation: PresentationMode) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
        OutputFormat::Auto | OutputFormat::Table => {
            // Skip the redundancy-reduction pre-pass in Verbose mode: full
            // canonical shape must pass through unchanged.
            let verbose = presentation == PresentationMode::Verbose;
            let reduced = if verbose {
                value
            } else {
                apply_redundancy_drop(value)
            };
            render_auto(reduced, !verbose)
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

    // `full_id` is a chaining handle, not needed in view-only output.
    map.remove("full_id");

    // `"local"` is the common case, so only surface `namespace` when it isn't.
    if map.get("namespace").and_then(Value::as_str) == Some("local") {
        map.remove("namespace");
    }

    // Properties dedup: drop key-value pairs from `properties` that
    // duplicate an identical top-level sibling. The scalar hoist for table
    // columns lives in `hoist_table_scalars`, applied only on the table
    // path: reshaping the compact-JSON fallback would move fields with no
    // column to gain.
    let props_val = map.remove("properties");
    if let Some(Value::Object(props)) = props_val {
        let mut new_props = Map::new();
        for (k, v) in props {
            if map.get(&k) == Some(&v) {
                continue;
            }
            new_props.insert(k, v);
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

/// A record array located inside a value: the records, their ordered column
/// keys, and — when the array sat under an object key — that key's name, so
/// the caller can enumerate the remaining (sibling) keys.
struct RecordArray {
    key: Option<String>,
    records: Vec<Value>,
    columns: Vec<String>,
}

/// Render a value using shape-aware dispatch (ADR-078 §3, as amended).
///
/// Shape (a): homogeneous record array → markdown table, with any envelope
/// siblings preserved as trailing `key: value` lines.
/// Every other shape → compact JSON, lossless by construction. The former
/// kv-block renderer for single records is removed: its truncation destroyed
/// single-record payloads (e.g. a compose briefing's `markdown` field).
fn render_auto(value: Value, truncate: bool) -> String {
    match locate_record_array(&value) {
        Some(found) => render_table_with_siblings(&value, &found, truncate),
        None => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
    }
}

/// Find the first homogeneous record array in `value`.
///
/// Checks:
/// 1. `value` itself is an array of 2+ objects.
/// 2. `value` is an object with a key whose value is an array of 2+ objects.
fn locate_record_array(value: &Value) -> Option<RecordArray> {
    let build = |key: Option<String>, arr: &[Value]| {
        let records: Vec<Value> = arr.iter().cloned().map(hoist_table_scalars).collect();
        let columns = collect_keys(&records);
        RecordArray {
            key,
            records,
            columns,
        }
    };
    match value {
        Value::Array(arr) if is_record_array(arr) => Some(build(None, arr)),
        Value::Object(map) => map.iter().find_map(|(k, v)| match v {
            Value::Array(arr) if is_record_array(arr) => Some(build(Some(k.clone()), arr)),
            _ => None,
        }),
        _ => None,
    }
}

/// Hoist `PROPERTY_HOIST_FIELDS` scalars out of a record's `properties` bag
/// to the top level (never overwriting a top-level sibling), so the table
/// renderer can surface them as columns — a scheduled event's
/// `trigger_at`/`status` live only inside `properties`, and a nested cell
/// renders as `{…}`. Table path only (ADR-078 Amendment 2): the compact-JSON
/// fallback keeps its shape.
fn hoist_table_scalars(record: Value) -> Value {
    let Value::Object(mut map) = record else {
        return record;
    };
    let mut props = match map.remove("properties") {
        Some(Value::Object(props)) => props,
        Some(other) => {
            // Non-object `properties` is not a bag to hoist from — restore it.
            map.insert("properties".to_string(), other);
            return Value::Object(map);
        }
        None => return Value::Object(map),
    };
    for field in PROPERTY_HOIST_FIELDS {
        if map.contains_key(*field) {
            continue;
        }
        if props
            .get(*field)
            .is_some_and(|v| !v.is_object() && !v.is_array())
        {
            let v = props.remove(*field).expect("checked above");
            map.insert((*field).to_string(), v);
        }
    }
    if !props.is_empty() {
        map.insert("properties".to_string(), Value::Object(props));
    }
    Value::Object(map)
}

/// An array of 2+ objects qualifies as a record array.
fn is_record_array(arr: &[Value]) -> bool {
    arr.len() >= 2 && arr.iter().all(Value::is_object)
}

/// Render the located record array as a table, then append one `key: value`
/// line per remaining top-level key. Envelope fields (`has_more`, `offset`,
/// counts) must survive rendering: dropping them made a truncated `query`
/// page read as complete.
fn render_table_with_siblings(value: &Value, found: &RecordArray, truncate: bool) -> String {
    let mut out = render_table(&found.records, &found.columns, truncate);
    let (Value::Object(map), Some(array_key)) = (value, &found.key) else {
        return out;
    };
    for (k, v) in map {
        if k == array_key {
            continue;
        }
        let text = match v {
            // A newline-bearing string renders as its JSON literal (one line,
            // lossless) so a stored value cannot fabricate a sibling line or
            // table row (ADR-078 Amendment 2 escaping contract).
            Value::String(s) if !s.contains(['\n', '\r']) => s.clone(),
            // Non-string scalars and nested values: compact JSON, untruncated
            // — sibling fidelity is the point of this path.
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        out.push_str(&format!("{k}: {text}\n"));
    }
    out
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
fn render_table(records: &[Value], keys: &[String], truncate: bool) -> String {
    let mut out = String::new();

    out.push('|');
    for k in keys {
        out.push(' ');
        out.push_str(k);
        out.push_str(" |");
    }
    out.push('\n');

    out.push('|');
    for _ in keys {
        out.push_str("---|");
    }
    out.push('\n');

    for record in records {
        out.push('|');
        for k in keys {
            let cell = record.get(k).unwrap_or(&Value::Null);
            let text = cell_text(k, cell, truncate);
            out.push(' ');
            out.push_str(&text);
            out.push_str(" |");
        }
        out.push('\n');
    }

    out
}

/// Column names exempt from cell truncation: identity and decision fields
/// must arrive whole — a truncated id cannot be resolved and a truncated
/// title cannot be selected on.
fn truncation_exempt(key: &str) -> bool {
    matches!(
        key,
        "id" | "kind"
            | "status"
            | "priority"
            | "relation"
            | "title"
            | "name"
            | "signature"
            | "slug"
            | "assignee"
            | "from"
            | "to"
            | "due"
    ) || key.ends_with("_id")
        || key.ends_with("_at")
        || key.starts_with("due")
}

/// Format a cell value: escape `|`, collapse newlines, truncate to ~120 chars
/// unless `truncate` is off or the column is identity/decision-bearing.
///
/// Nested values are elided to a constant marker (`{…}` / `[…]`) rather than
/// stringified and cut mid-JSON: truncated pseudo-JSON reads as data while
/// silently missing fields. Full content is one `format=json` or `get` away.
fn cell_text(key: &str, value: &Value, truncate: bool) -> String {
    let raw = match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Object(_) => return "{…}".to_string(),
        Value::Array(arr) => {
            if arr.iter().any(|v| v.is_object() || v.is_array()) {
                return "[…]".to_string();
            }
            // Arrays of scalars (tags, ids) stay compact JSON.
            serde_json::to_string(value).unwrap_or_default()
        }
    };

    // Escape literal `|` and collapse embedded newlines to a space.
    let escaped = raw.replace('|', "\\|").replace(['\n', '\r'], " ");

    if !truncate || truncation_exempt(key) {
        return escaped;
    }

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

/// Parse an RFC 3339 timestamp (offset required) into microsecond epoch
/// `i64` — the inverse of [`micros_to_iso`], and the single parse point for
/// caller-supplied instants entering storage comparisons.
///
/// Leading/trailing whitespace is tolerated. Date-only and offset-less forms
/// are rejected; callers own the verb-specific error context around the
/// returned `ParseError`.
pub fn rfc3339_to_utc_micros(raw: &str) -> Result<i64, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(raw.trim()).map(|dt| dt.timestamp_micros())
}

/// How the response envelope is presented to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    /// Token-efficient. Default for MCP callers (agents).
    ///
    /// Short UUIDs (8-char), compact timestamps (minute granularity or
    /// relative), empty fields dropped, structural nulls preserved, score
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

/// Lifecycle/operational `null` fields that are PRESERVED in Agent mode.
///
/// These fields carry state meaning (absent ≠ known-unknown) and must not be
/// dropped. The channel-health fields distinguish a quarantine-only identity
/// from a heartbeat row whose liveness facts were actually observed.
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
    "poll_interval_secs",
    "stalled",
    "last_success_at",
    "last_poll_attempt_at",
    "last_failure_at",
    "last_error",
    "consecutive_failures",
];

/// Empty collection fields that define a stable response envelope and must
/// survive Agent-mode compaction. Dropping these turns an empty page into a
/// different response type and leaves callers unable to distinguish an empty
/// result from a missing/unsupported field.
const EMPTY_ARRAY_PRESERVE: &[&str] = &["items", "entities", "notes", "edges"];

fn is_stable_list_envelope(map: &Map<String, Value>) -> bool {
    map.contains_key("requested_limit")
        && map.contains_key("effective_limit")
        && map.contains_key("limit_clamped")
        && EMPTY_ARRAY_PRESERVE
            .iter()
            .any(|field| map.contains_key(*field))
}

/// Field names carrying caller-supplied payload timestamps that must never be
/// compacted (relative-time or minute-truncated), regardless of nesting.
///
/// These encode domain semantics the caller needs to round-trip verbatim —
/// e.g. `trigger_at` on a `schedule.remind`/`schedule.schedule` create
/// response, returned as a top-level convenience field alongside `id` and
/// `full_id`, not nested under `"properties"`. The `inside_properties` guard
/// alone only protects fields nested under a literal `"properties"` key
/// (as returned by `agenda`/`get`); it does not cover this top-level case,
/// which `compact_timestamp` rewrote into either a relative string or a
/// minute-truncated absolute form — either way discarding the seconds and
/// offset the caller needs to round-trip the exact submitted value (#871).
///
/// `due` on `gtd.assign`/`gtd.tasks`/`gtd.next` responses is the same shape:
/// a top-level convenience field mirroring `properties.due`, which
/// `parse_due` already normalizes to full RFC 3339.
const PAYLOAD_TIMESTAMP_FIELDS: &[&str] = &["trigger_at", "due"];

/// UUID fields whose canonical value is itself a strict-verb input.
///
/// Shortening these would make a successful response fail when submitted back
/// to the verb that produced or consumes it. `context_entity_id` and
/// `thread_id` are explicit stable references rather than prefix searches;
/// `outbound_ref` is the exact correlation key consumed by `comm.delivered`;
/// `parent_id` is the explicit ancestry reference consumed by `propose`;
/// `session_id` is an exact event-list filter; and `project_id` is the exact
/// provenance anchor required by git issue and pull-request creation.
const ROUND_TRIP_FULL_UUID_FIELDS: &[&str] = &[
    "context_entity_id",
    "thread_id",
    "outbound_ref",
    "parent_id",
    "session_id",
    "project_id",
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
/// `full_id` and strict round-trip fields are explicitly excluded: their
/// purpose is to give callers a stable chaining handle, so shortening them
/// would produce a value that the corresponding strict verb rejects.
fn should_shorten_uuid_field(key: &str) -> bool {
    if key == "full_id" || ROUND_TRIP_FULL_UUID_FIELDS.contains(&key) {
        return false;
    }
    key == "id" || key.ends_with("_id") || matches!(key, "superseded_by" | "replaced_by")
}

/// Transform a successful verb result value according to the given
/// [`PresentationMode`].
///
/// - `Verbose` / `Human`: returns `value` unchanged.
/// - `Agent`: applies UUID shortening, timestamp compaction, empty-field
///   dropping, structural-null preservation, and score truncation.
///
/// `now_unix_seconds` is sampled once per response and passed through so all
/// relative datetime renderings within a response use the same instant.
pub fn present(value: Value, mode: PresentationMode, now_unix_seconds: i64) -> Value {
    match mode {
        PresentationMode::Verbose | PresentationMode::Human => value,
        PresentationMode::Agent => {
            let preserved_nulls: HashSet<&str> = LIFECYCLE_NULL_PRESERVE.iter().copied().collect();
            let score_fields: HashSet<&str> = SCORE_FIELDS.iter().copied().collect();
            let payload_timestamps: HashSet<&str> =
                PAYLOAD_TIMESTAMP_FIELDS.iter().copied().collect();
            transform_agent(
                value,
                &preserved_nulls,
                &score_fields,
                &payload_timestamps,
                now_unix_seconds,
                false,
            )
        }
    }
}

/// Apply the Agent-mode transform to an arbitrary JSON value.
///
/// `inside_properties` is `true` when recursing inside a `"properties"` object.
/// Caller-supplied empty strings and payload timestamps (e.g. `trigger_at`)
/// must not be compacted because they encode domain semantics the agent may
/// need to round-trip.
fn transform_agent(
    value: Value,
    preserved_nulls: &HashSet<&str>,
    scores: &HashSet<&str>,
    payload_timestamps: &HashSet<&str>,
    now: i64,
    inside_properties: bool,
) -> Value {
    match value {
        Value::Object(map) => {
            let preserve_list_envelope = is_stable_list_envelope(&map);
            let mut out = Map::new();
            for (k, v) in map {
                let child_inside_properties = inside_properties || k == "properties";
                let transformed = transform_field_agent(
                    &k,
                    v,
                    preserved_nulls,
                    scores,
                    payload_timestamps,
                    now,
                    AgentFieldContext {
                        inside_properties: child_inside_properties,
                        preserve_list_envelope,
                    },
                );
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
                .map(|v| {
                    transform_agent(
                        v,
                        preserved_nulls,
                        scores,
                        payload_timestamps,
                        now,
                        inside_properties,
                    )
                })
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
/// `inside_properties` preserves empty strings and suppresses timestamp
/// compaction for caller-submitted payload values nested under a literal
/// `"properties"` key (e.g. `trigger_at` as returned by `agenda`/`get`).
/// `payload_timestamps` suppresses compaction by field name regardless of
/// nesting, covering top-level convenience fields such as the `trigger_at`
/// returned directly in a `schedule.remind`/`schedule.schedule` create
/// response (#871). Metadata timestamps at the top level (`created_at`,
/// `updated_at`) are still compacted.
#[derive(Clone, Copy)]
struct AgentFieldContext {
    inside_properties: bool,
    preserve_list_envelope: bool,
}

fn transform_field_agent(
    key: &str,
    value: Value,
    preserved_nulls: &HashSet<&str>,
    scores: &HashSet<&str>,
    payload_timestamps: &HashSet<&str>,
    now: i64,
    context: AgentFieldContext,
) -> Option<Value> {
    match &value {
        // Preserve lifecycle and stable-envelope nulls; drop other nulls.
        Value::Null => {
            if preserved_nulls.contains(key)
                || (context.preserve_list_envelope && key == "next_after")
            {
                Some(value)
            } else {
                None
            }
        }
        // Stable page-envelope arrays remain present even when empty.
        Value::Array(a)
            if context.preserve_list_envelope
                && a.is_empty()
                && EMPTY_ARRAY_PRESERVE.contains(&key) =>
        {
            Some(value)
        }
        // Caller-owned property strings are data; drop empty strings elsewhere.
        Value::String(s) if s.is_empty() && !context.inside_properties => None,
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
        // Compact ISO-8601 timestamps unless inside a caller-supplied payload
        // object, or the field is a named payload timestamp at any nesting.
        Value::String(s)
            if !context.inside_properties
                && !payload_timestamps.contains(key)
                && looks_like_iso8601(s) =>
        {
            Some(Value::String(compact_timestamp(s, now)))
        }
        // Recurse into objects and arrays.
        Value::Object(_) | Value::Array(_) => Some(transform_agent(
            value,
            preserved_nulls,
            scores,
            payload_timestamps,
            now,
            context.inside_properties,
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

    #[test]
    fn rfc3339_to_utc_micros_round_trips_and_rejects_partial_forms() {
        let micros = 1_748_016_480_000_000_i64;
        assert_eq!(rfc3339_to_utc_micros(&micros_to_iso(micros)), Ok(micros));
        // Offset spellings resolve to the same instant; whitespace tolerated.
        // (NOW's epoch value is 2025-05-23T16:08:00Z despite its comment.)
        assert_eq!(
            rfc3339_to_utc_micros(" 2025-05-23T12:08:00-04:00 "),
            Ok(micros)
        );
        assert!(rfc3339_to_utc_micros("2026-05-23").is_err());
        assert!(rfc3339_to_utc_micros("2026-05-23T16:18:00").is_err());
    }

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
    fn agent_preserves_empty_list_page_arrays() {
        let v = json!({
            "items": [],
            "entities": [],
            "notes": [],
            "edges": [],
            "next_after": null,
            "requested_limit": 10,
            "effective_limit": 10,
            "limit_clamped": false,
        });
        let out = agent(v);
        for key in EMPTY_ARRAY_PRESERVE {
            assert_eq!(out[*key], json!([]), "missing structural key {key}");
        }
        assert_eq!(out["next_after"], json!(null));
    }

    #[test]
    fn agent_still_drops_empty_arrays_outside_list_envelopes() {
        let out = agent(json!({"items": [], "entities": [], "title": "ordinary response"}));
        assert!(out.get("items").is_none());
        assert!(out.get("entities").is_none());
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
    fn agent_preserves_unknown_channel_heartbeat_nulls() {
        let v = json!({
            "channels": [{
                "poll_interval_secs": null,
                "stalled": null,
                "last_success_at": null,
                "last_poll_attempt_at": null,
                "last_failure_at": null,
                "last_error": null,
                "consecutive_failures": null,
                "quarantined_count": 1,
            }]
        });
        let out = agent(v);
        let channel = out["channels"][0].as_object().expect("channel object");
        for field in [
            "poll_interval_secs",
            "stalled",
            "last_success_at",
            "last_poll_attempt_at",
            "last_failure_at",
            "last_error",
            "consecutive_failures",
        ] {
            assert_eq!(
                channel.get(field),
                Some(&Value::Null),
                "Agent presentation must preserve unknown heartbeat fact `{field}`"
            );
        }
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
    fn agent_does_not_compact_top_level_trigger_at_field() {
        // Regression for #871: `schedule.remind`'s create response returns
        // `trigger_at` as a top-level convenience field (sibling to `id`,
        // `full_id`), not nested under `"properties"`. The pre-existing
        // `inside_properties` guard alone did not protect it, so Agent-mode
        // compaction rewrote it: `at` here is far outside the 24h relative
        // window, so pre-fix it would have been minute-truncated to
        // "2026-07-11T19:00", discarding the seconds and the "-04:00"
        // offset the caller needs to round-trip the exact value verbatim.
        let at = "2026-07-11T19:00:00-04:00";
        let v = json!({
            "id": "a1b2c3d4",
            "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "event_type": "remind",
            "trigger_at": at,
            "repeat": null,
            "status": "pending",
        });
        let out = agent(v);
        assert_eq!(out["trigger_at"], json!(at));
    }

    #[test]
    fn agent_does_not_compact_top_level_trigger_at_utc() {
        let at = "2026-07-11T23:00:00Z";
        let v = json!({"trigger_at": at});
        let out = agent(v);
        assert_eq!(out["trigger_at"], json!(at));
    }

    #[test]
    fn agent_does_not_compact_top_level_trigger_at_offset_less() {
        let at = "2026-07-11T23:00:00";
        let v = json!({"trigger_at": at});
        let out = agent(v);
        assert_eq!(out["trigger_at"], json!(at));
    }

    #[test]
    fn agent_still_compacts_other_top_level_timestamps_alongside_trigger_at() {
        // The `trigger_at` exemption is scoped to that field name only — a
        // sibling generic timestamp field must still be compacted, so the
        // fix does not blanket-disable Agent-mode compaction.
        let v = json!({
            "trigger_at": "2026-07-11T19:00:00-04:00",
            "created_at": "2020-01-01T10:30:45.123456Z",
        });
        let out = agent(v);
        assert_eq!(out["trigger_at"], json!("2026-07-11T19:00:00-04:00"));
        assert_eq!(out["created_at"], json!("2020-01-01T10:30"));
    }

    #[test]
    fn agent_does_not_compact_top_level_due() {
        // gtd.assign/gtd.tasks/gtd.next return the caller-supplied `due` as
        // a top-level convenience field mirroring `properties.due`; it must
        // round-trip verbatim through Agent-mode presentation, the same
        // guarantee already given to `trigger_at`.
        let due = "2026-08-01T09:30:15-04:00";
        let v = json!({"due": due});
        let out = agent(v);
        assert_eq!(out["due"], json!(due));
    }

    #[test]
    fn agent_still_protects_nested_trigger_at_under_properties() {
        // Pre-existing protection (agenda/get responses nest trigger_at
        // under "properties") must remain intact alongside the new
        // top-level, field-name-based guard.
        let at = "2026-07-11T19:00:00-04:00";
        let v = json!({
            "id": "a1b2c3d4",
            "properties": {"trigger_at": at, "status": "pending"},
        });
        let out = agent(v);
        assert_eq!(out["properties"]["trigger_at"], json!(at));
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

    // full_id must never be shortened in Agent mode: it's the caller's
    // stable chaining handle.
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

    #[test]
    fn agent_preserves_strict_round_trip_uuid_fields() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let out = agent(json!({
            "context_entity_id": uuid,
            "thread_id": uuid,
            "session_id": uuid,
            "project_id": uuid,
            "properties": {
                "context_entity_id": uuid,
                "thread_id": uuid,
                "outbound_ref": uuid,
                "parent_id": uuid,
                "session_id": uuid,
                "project_id": uuid,
            },
            "parent_id": uuid,
        }));

        assert_eq!(out["context_entity_id"], json!(uuid));
        assert_eq!(out["thread_id"], json!(uuid));
        assert_eq!(out["session_id"], json!(uuid));
        assert_eq!(out["project_id"], json!(uuid));
        assert_eq!(out["properties"]["context_entity_id"], json!(uuid));
        assert_eq!(out["properties"]["thread_id"], json!(uuid));
        assert_eq!(out["properties"]["outbound_ref"], json!(uuid));
        assert_eq!(out["properties"]["parent_id"], json!(uuid));
        assert_eq!(out["properties"]["session_id"], json!(uuid));
        assert_eq!(out["properties"]["project_id"], json!(uuid));
        assert_eq!(out["parent_id"], json!(uuid));
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
        // full_id must not be dropped in json mode.
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
        // Auto mode elides namespace=local and drops full_id.
        // The value itself is a single record → rendered as compact JSON.
        assert!(!auto_rendered.contains("full_id"), "auto must drop full_id");
        assert!(
            !auto_rendered.contains("namespace"),
            "auto must elide namespace=local"
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
        assert!(rendered.starts_with('|'), "must start with |");
        assert!(
            rendered.contains("| id |") || rendered.contains("| id"),
            "must have id column"
        );
        assert!(rendered.contains("title"), "must have title column");
        assert!(rendered.contains("|---|"), "must have separator row");
        assert!(rendered.contains("abc"), "must have first row data");
        assert!(rendered.contains("Second"), "must have second row data");
    }

    /// (b2) single record → compact JSON, lossless (kv-block renderer removed).
    #[test]
    fn format_auto_single_record_renders_compact_json() {
        let v = json!({"id": "abc", "title": "Hello World"});
        let rendered = render_format(v.clone(), OutputFormat::Auto, PresentationMode::Agent);
        let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
        assert_eq!(parsed, v, "single record must round-trip losslessly");
        assert!(
            !rendered.starts_with('|'),
            "single record must not be a markdown table"
        );
    }

    /// (b2-lossless) a single record with a large payload field (a compose
    /// briefing's `markdown`) arrives whole — the former kv-block renderer
    /// truncated it.
    #[test]
    fn format_auto_single_record_large_payload_survives_whole() {
        let briefing = "line one\nline two\n".repeat(500); // ~9KB, newlines included
        let v = json!({"id": "abc", "markdown": briefing.clone()});
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
        assert_eq!(
            parsed.get("markdown").and_then(Value::as_str),
            Some(briefing.as_str()),
            "large payload field must survive untruncated"
        );
    }

    /// Envelope siblings survive table rendering: a `query`-style page whose
    /// `has_more` is dropped reads as complete — fail-open.
    #[test]
    fn format_auto_table_preserves_sibling_scalars() {
        let v = json!({
            "results": [
                {"id": "abc", "title": "First"},
                {"id": "def", "title": "Second"}
            ],
            "has_more": true,
            "offset": 20,
            "page_size": 2,
            "truncated": false
        });
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(rendered.contains("| id"), "records must render as a table");
        assert!(
            rendered.contains("has_more: true"),
            "has_more sibling must survive: {rendered}"
        );
        assert!(
            rendered.contains("offset: 20"),
            "offset sibling must survive"
        );
        assert!(
            rendered.contains("page_size: 2"),
            "page_size sibling must survive"
        );
        assert!(
            rendered.contains("truncated: false"),
            "truncated sibling must survive"
        );
    }

    /// Nested table cells render a constant elision marker, never
    /// truncated pseudo-JSON that reads as data while missing fields.
    #[test]
    fn format_auto_nested_cell_renders_elision_marker() {
        let big_nested: Value = json!({"k": "v".repeat(300), "other": {"deep": true}});
        let v = json!([
            {"id": "abc", "properties": big_nested},
            {"id": "def", "properties": {"x": 1}}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            rendered.contains("{…}"),
            "nested object cell must render the elision marker: {rendered}"
        );
        assert!(
            !rendered.contains("..."),
            "no truncated pseudo-JSON in nested cells"
        );
    }

    /// Arrays of scalars (tags) still render as compact JSON in cells.
    #[test]
    fn format_auto_scalar_array_cell_renders_compact_json() {
        let v = json!([
            {"id": "abc", "tags": ["lesson", "khive"]},
            {"id": "def", "tags": ["adr"]}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            rendered.contains(r#"["lesson","khive"]"#),
            "scalar array cell must be compact JSON: {rendered}"
        );
    }

    /// Identity/decision columns are exempt from truncation: a truncated
    /// title cannot be selected on, a truncated signature is unusable.
    #[test]
    fn format_auto_identity_columns_not_truncated() {
        let long_title = "T".repeat(300);
        let long_note = "N".repeat(300);
        let v = json!([
            {"id": "abc", "title": long_title.clone(), "note": long_note.clone()},
            {"id": "def", "title": "short", "note": "short"}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            rendered.contains(&long_title),
            "title column must not be truncated"
        );
        assert!(
            !rendered.contains(&long_note),
            "non-exempt column must still truncate"
        );
    }

    /// Verbose presentation disables cell truncation entirely (ADR-078 §3a).
    #[test]
    fn format_auto_verbose_disables_truncation() {
        let long_note = "N".repeat(300);
        let v = json!([
            {"id": "abc", "note": long_note.clone()},
            {"id": "def", "note": "short"}
        ]);
        let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Verbose);
        assert!(
            rendered.contains(&long_note),
            "verbose must render full cell content"
        );
    }

    /// The pre-pass hoists named scalar payload fields (`trigger_at`, `due`,
    /// `status`) out of `properties` so they can surface as table columns —
    /// a scheduled event carries them only inside `properties`.
    #[test]
    fn table_path_hoists_payload_scalars_to_columns() {
        // The hoist is table-path only (ADR-078 Amendment 2): two scheduled
        // events whose trigger_at/status live inside `properties` must gain
        // top-level columns in the rendered table.
        let record = |n: u32| {
            json!({
                "id": format!("evt-{n}"),
                "properties": {
                    "trigger_at": "2026-09-01T14:00:00-04:00",
                    "status": "pending",
                    "dispatch_receipt": {"state": "succeeded"}
                }
            })
        };
        let out = render_format(
            json!([record(1), record(2)]),
            OutputFormat::Auto,
            PresentationMode::Agent,
        );
        let header = out.lines().next().expect("table header");
        assert!(
            header.contains("trigger_at") && header.contains("status"),
            "hoisted scalars must appear as table columns, got header: {header}"
        );
        assert!(
            out.contains("2026-09-01T14:00:00-04:00") && out.contains("pending"),
            "hoisted values must render in cells:\n{out}"
        );
    }

    #[test]
    fn redundancy_drop_no_longer_hoists_outside_tables() {
        // Rule-separating control for the table-scoped hoist: the §7 pre-pass
        // alone must leave the properties bag in place, so a single record's
        // compact-JSON fallback keeps its shape.
        let v = json!({
            "id": "abc",
            "properties": {
                "trigger_at": "2026-09-01T14:00:00-04:00",
                "status": "pending"
            }
        });
        let reduced = apply_redundancy_drop(v);
        assert!(
            reduced.get("trigger_at").is_none() && reduced.get("status").is_none(),
            "the pre-pass must not hoist; the hoist is table-path only"
        );
        let props = reduced.get("properties").expect("properties must remain");
        assert_eq!(
            props.get("status").and_then(Value::as_str),
            Some("pending"),
            "fallback shape keeps payload fields inside properties"
        );
    }

    #[test]
    fn sibling_string_with_newline_renders_as_json_literal() {
        // Escaping contract: a newline-bearing sibling string must not be able
        // to fabricate an additional `key: value` line.
        let v = json!({
            "items": [{"id": "a", "kind": "x"}, {"id": "b", "kind": "y"}],
            "note": "line one\nforged_key: forged_value",
            "has_more": true
        });
        let out = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            !out.contains("\nforged_key: forged_value"),
            "raw newline from a sibling string must not start a new line:\n{out}"
        );
        assert!(
            out.contains(r#"note: "line one\nforged_key: forged_value""#),
            "newline-bearing sibling renders as its JSON literal:\n{out}"
        );
        assert!(out.contains("has_more: true"), "siblings still preserved");
    }

    #[test]
    fn carriage_return_in_cell_collapses_like_newline() {
        // Escaping contract pin: cells collapse \r exactly like \n, so a
        // \r- or \r\n-bearing value cannot smuggle a raw line break into
        // the table body and forge row structure.
        let v = json!([
            {"id": "a", "note": "before\rafter"},
            {"id": "b", "note": "one\r\ntwo"}
        ]);
        let out = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
        assert!(
            !out.contains('\r'),
            "no raw carriage return may survive into rendered output:\n{out:?}"
        );
        assert!(
            out.contains("before after") && out.contains("one  two"),
            "\\r and \\r\\n collapse to spaces inside cells:\n{out}"
        );
    }

    /// The hoist never overwrites an existing top-level sibling.
    #[test]
    fn redundancy_drop_hoist_does_not_overwrite_top_level() {
        let v = json!({
            "id": "abc",
            "status": "active",
            "properties": {"status": "pending"}
        });
        let reduced = apply_redundancy_drop(v);
        assert_eq!(
            reduced.get("status").and_then(Value::as_str),
            Some("active"),
            "existing top-level status must win"
        );
        assert_eq!(
            reduced
                .get("properties")
                .and_then(|p| p.get("status"))
                .and_then(Value::as_str),
            Some("pending"),
            "conflicting properties value must stay where it was"
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
        // The value is a single object → compact JSON; full_id and namespace stay.
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

    /// Indirect check that the redundancy pre-pass doesn't corrupt an error
    /// envelope's shape; the actual ok=false bypass is enforced by
    /// `render_result`, not by this pre-pass.
    #[test]
    fn redundancy_drop_does_not_corrupt_error_shape() {
        let v = json!({"ok": false, "error": "something failed", "namespace": "local"});
        // apply_redundancy_drop is a pure value transform with no knowledge of
        // `ok`: bypassing it for error envelopes is the caller's job
        // (render_result in server.rs). This only checks the pre-pass itself
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

    /// Cell truncation must not panic on multi-byte UTF-8 characters.
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
        // A wall-clock-identical-to-NOW timestamp carrying a "-02:00" offset
        // is actually 2h in the future; an offset-naive parser would misread
        // the wall-clock digits as UTC and report "0s ago".
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
