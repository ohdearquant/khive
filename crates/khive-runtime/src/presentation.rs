//! Verb response presentation modes and transformation.
//!
//! Transforms canonical handler output into caller-appropriate form after dispatch
//! and before wire serialization. `Agent` mode abbreviates UUIDs and drops
//! empty fields; `Verbose` and `Human` pass through canonical JSON unchanged.
//!
//! This module also contains the `OutputFormat` axis (ADR-078) which governs how
//! the resulting `serde_json::Value` is serialized or rendered to an output string.
//! `PresentationMode` and `OutputFormat` compose independently.

use std::{cmp::Ordering, collections::HashSet};

use khive_types::VerbPresentationPolicy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Exact note-body locations selected by the request boundary for
/// `get/list(parse_content=true)`. No policy marker is inferred from user JSON.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoteContentScope {
    #[default]
    None,
    Record,
    Items,
    Notes,
}

impl NoteContentScope {
    /// Keep note bodies outside a metadata-only transform, including nulls,
    /// empty containers/strings, and fields that resemble IDs or timestamps.
    /// The transform must preserve record order and the get/list envelope.
    pub fn protect(self, mut value: Value, transform: impl FnOnce(Value) -> Value) -> Value {
        if self == Self::None {
            return transform(value);
        }
        fn take_content(record: &mut Value) -> Option<Value> {
            record.as_object_mut()?.remove("content")
        }
        fn restore_content(record: &mut Value, content: Option<Value>) {
            if let (Some(record), Some(content)) = (record.as_object_mut(), content) {
                record.insert("content".to_string(), content);
            }
        }
        if self == Self::Record {
            let content = take_content(&mut value);
            let mut value = transform(value);
            restore_content(&mut value, content);
            return value;
        }
        let key = if self == Self::Items {
            "items"
        } else {
            "notes"
        };
        let contents: Vec<_> = value
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .map(take_content)
            .collect();
        let mut value = transform(value);
        if let Some(records) = value.get_mut(key).and_then(Value::as_array_mut) {
            for (record, content) in records.iter_mut().zip(contents) {
                restore_content(record, content);
            }
        }
        value
    }

    fn stringify_table_content(self, value: &mut Value) {
        fn stringify(record: &mut Value) {
            if let Some(content) = record.get_mut("content") {
                if content.is_object() || content.is_array() {
                    *content = Value::String(content.to_string());
                }
            }
        }
        match self {
            Self::None => {}
            Self::Record => stringify(value),
            Self::Items | Self::Notes => {
                let key = if self == Self::Items {
                    "items"
                } else {
                    "notes"
                };
                if let Some(records) = value.get_mut(key).and_then(Value::as_array_mut) {
                    records.iter_mut().for_each(stringify);
                }
            }
        }
    }
}

// ── OutputFormat ─────────────────────────────────────────────────────────────

/// Output serialization format for verb results (ADR-078).
///
/// Orthogonal to [`PresentationMode`]: `PresentationMode` controls field-level
/// transforms (UUID shortening, exact UTC timestamps, empty-field dropping);
/// `OutputFormat` controls how the resulting `serde_json::Value` is serialized
/// or rendered to the wire string.
///
/// Default is [`OutputFormat::Json`] on every surface: compact and
/// machine-walkable. Verbose JSON is lossless; Agent JSON additionally elides
/// `namespace="local"` and duplicate `properties` entries (ADR-078 Amendment
/// 3, Machine scope) but keeps `full_id` — it is the caller's chaining
/// handle, not a redundant field.
///
/// Note: `Yaml` is a clean follow-up — implemented as a 3-variant enum
/// (`Json`, `Auto`, `Table`) per ADR-078 §"yaml" which permits omission when
/// the in-tree emitter would balloon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Compact JSON (`serde_json::to_string`). Default; lossless in Verbose
    /// presentation, Machine-scope redundancy-reduced in Agent presentation
    /// (`full_id` retained — see [`RedundancyScope`]).
    #[default]
    Json,
    /// Shape-aware: markdown table for homogeneous record arrays (with
    /// envelope siblings preserved as trailing `key: value` lines),
    /// compact-JSON fallback for every other shape.
    Auto,
    /// Force the markdown-table renderer regardless of detected shape.
    /// Ordinary results share Auto's dispatch. Opt-in parsed note bodies are
    /// stringified for table display; Auto preserves their JSON values.
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
/// Agent presentation applies the redundancy-reduction pre-pass (§7) for every
/// format, including JSON — at Machine scope for `json` (keeps `full_id`) and
/// View scope for `auto`/`table` (drops it too, ADR-078 Amendment 3). Auto/table
/// also apply View scope for Human presentation; Verbose remains canonical and
/// unreduced. The prepared value is then encoded as compact JSON or dispatched
/// to the shape-aware renderer. Verbose also disables cell truncation in the
/// table renderer (§3a).
pub fn render_format(value: Value, format: OutputFormat, presentation: PresentationMode) -> String {
    render_format_with_note_content(value, format, presentation, NoteContentScope::None)
}

/// Render an opt-in parsed-note response without treating its body as metadata
/// or a top-level record table. Auto preserves parsed values; Table stringifies
/// non-scalar content for display only.
pub fn render_format_with_note_content(
    value: Value,
    format: OutputFormat,
    presentation: PresentationMode,
    content_scope: NoteContentScope,
) -> String {
    let value = prepare_format_value_with_note_content(value, format, presentation, content_scope);
    match format {
        OutputFormat::Json => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
        OutputFormat::Auto if content_scope != NoteContentScope::None => {
            serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
        }
        OutputFormat::Auto | OutputFormat::Table => {
            render_auto(value, presentation != PresentationMode::Verbose)
        }
    }
}

/// Prepare a successful result for its requested format at the wire boundary.
///
/// JSON remains a JSON value in compounded response envelopes, so the MCP seam
/// uses this helper directly instead of round-tripping through a serialized
/// string. Keeping the policy here prevents the single-value and compounded
/// paths from drifting.
pub fn prepare_format_value(
    value: Value,
    format: OutputFormat,
    presentation: PresentationMode,
) -> Value {
    let scope = match format {
        OutputFormat::Json if presentation == PresentationMode::Agent => {
            Some(RedundancyScope::Machine)
        }
        OutputFormat::Json => None,
        OutputFormat::Auto | OutputFormat::Table if presentation != PresentationMode::Verbose => {
            Some(RedundancyScope::View)
        }
        OutputFormat::Auto | OutputFormat::Table => None,
    };
    match scope {
        Some(scope) => apply_redundancy_drop(value, scope),
        None => value,
    }
}

/// Apply format reductions to record metadata while preserving parsed bodies.
pub fn prepare_format_value_with_note_content(
    value: Value,
    format: OutputFormat,
    presentation: PresentationMode,
    content_scope: NoteContentScope,
) -> Value {
    let mut value = content_scope.protect(value, |value| {
        prepare_format_value(value, format, presentation)
    });
    if format == OutputFormat::Table {
        content_scope.stringify_table_content(&mut value);
    }
    value
}

// ── Redundancy-reduction pre-pass (ADR-078 §7, Amendment 3) ────────────────

/// Which redundancy-reduction rules apply (ADR-078 §7, Amendment 3).
///
/// `View` is the rendered-view scope (`auto`/`table`): the caller has opted
/// into a display form that is never chained on programmatically, so
/// `full_id` — the caller's stable chaining handle — is safe to drop along
/// with the other reductions.
///
/// `Machine` is the machine-contract scope (Agent `json`): this is the
/// default MCP response shape, the one a caller chains `$prev` and strict
/// verb parameters (`memory.feedback(target_id=...)`, `gtd.assign
/// (context_entity_id=...)`, etc.) from. It applies the `namespace` elision
/// and the `properties` dedup, but never drops `full_id`, and the
/// `properties` dedup itself exempts `full_id`/`ROUND_TRIP_FULL_UUID_FIELDS`
/// keys — a caller reading a strict identifier out of `properties` must see
/// the same canonical value there as at the top level. Dropping any of
/// those would return a value the strict verbs that consume it reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedundancyScope {
    View,
    Machine,
}

/// Apply the redundancy-reduction pre-pass (ADR-078 §7, Amendment 3) to a
/// value at the given [`RedundancyScope`].
///
/// Applies at most ONE pass over the value. This function is the canonical
/// entry for the pre-pass; the per-record logic lives in `drop_record`.
///
/// Applied for Agent presentation in every format (`Machine` scope for
/// `json`, `View` scope for non-Verbose `auto`/`table`). Callers are
/// responsible for checking those conditions; this function applies
/// unconditionally.
pub fn apply_redundancy_drop(value: Value, scope: RedundancyScope) -> Value {
    match value {
        Value::Object(_) => drop_record(value, scope),
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|v| {
                    if v.is_object() {
                        drop_record(v, scope)
                    } else {
                        v
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

/// Apply per-record redundancy rules (§7.1, §7.2, §7.3) to a single record object.
fn drop_record(value: Value, scope: RedundancyScope) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };

    // `full_id` is a chaining handle. Dropping it is safe only in the View
    // scope (a rendered auto/table view the caller opted into); the Machine
    // scope (Agent JSON, the default machine contract) must keep it.
    if scope == RedundancyScope::View {
        map.remove("full_id");
    }

    // `"local"` is the common case, so only surface `namespace` when it isn't.
    if map.get("namespace").and_then(Value::as_str) == Some("local") {
        map.remove("namespace");
    }

    // Properties dedup: drop key-value pairs from `properties` that
    // duplicate an identical top-level sibling. The scalar hoist for table
    // columns lives in `hoist_table_scalars`, applied only on the table
    // path: reshaping the compact-JSON fallback would move fields with no
    // column to gain.
    //
    // Machine scope exempts `full_id`/`ROUND_TRIP_FULL_UUID_FIELDS`: a caller
    // that reads the strict identifier out of `properties` (rather than the
    // top-level convenience mirror) must keep getting the same canonical
    // value there too. View scope is unaffected — those callers already
    // opted into a display form, not the strict round-trip contract.
    let props_val = map.remove("properties");
    if let Some(Value::Object(props)) = props_val {
        let mut new_props = Map::new();
        for (k, v) in props {
            let strict_round_trip = scope == RedundancyScope::Machine
                && (k == "full_id" || ROUND_TRIP_FULL_UUID_FIELDS.contains(&k.as_str()));
            if !strict_round_trip && map.get(&k) == Some(&v) {
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

    // A stream entry's record is opaque caller JSON, including arrays of
    // objects whose fields happen to look like metadata.
    let stream_entry = is_stream_entry(&map);
    // Recurse into array values so nested record arrays are also reduced.
    let out: Map<String, Value> = map
        .into_iter()
        .map(|(k, v)| {
            if stream_entry && k == "record" {
                return (k, v);
            }
            let v = match v {
                Value::Array(arr) => Value::Array(
                    arr.into_iter()
                        .map(|item| {
                            if item.is_object() {
                                drop_record(item, scope)
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

/// Format an instant in UTC using RFC3339 whole seconds and a `Z` suffix.
///
/// Fractional seconds are omitted. Chrono's supported years and signed
/// expanded-year rendering are inherited; no additional range validation is done.
/// Parsing and error policy remain with callers.
pub fn format_utc_rfc3339_seconds<Tz: chrono::TimeZone>(value: &chrono::DateTime<Tz>) -> String {
    value
        .with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Convert a microsecond epoch `i64` to an exact UTC ISO-8601 string.
///
/// Output always carries six fractional digits and a `Z` suffix. Years
/// 0000–9999 use the RFC 3339 spelling; other years use Chrono's signed
/// expanded-year convention. The full `i64` microsecond range is supported.
pub fn micros_to_iso(micros: i64) -> String {
    if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros) {
        return dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    }

    // Gregorian leap positions repeat every 400 years. Euclidean reduction
    // keeps the remainder in 1970–2369, including for negative timestamps.
    const CYCLE_MICROS: i64 = 146_097 * 86_400 * 1_000_000;
    let cycles = micros.div_euclid(CYCLE_MICROS);
    let reduced =
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros.rem_euclid(CYCLE_MICROS))
            .expect("400-year remainder is within Chrono's range");
    let year = i64::from(chrono::Datelike::year(&reduced)) + cycles * 400;
    let rendered = reduced.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    format!("{year:+05}{}", &rendered[4..])
}

/// Parse an RFC 3339 timestamp (offset required) into microsecond epoch
/// `i64` — the inverse of [`micros_to_iso`] within the RFC 3339 four-digit
/// year range. Expanded ISO years are not accepted. This is the single parse
/// point for caller-supplied instants entering storage comparisons.
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
    /// Short UUIDs (8-char), exact UTC timestamps with relative labels on
    /// list rows, empty fields dropped, structural nulls preserved, and
    /// score fields truncated to 3 significant figures.
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
    "consecutive_held_polls",
    "held_messages",
    "last_held_at",
    "last_quarantined_after_hold_at",
    "originals_not_retained",
    "last_original_not_retained_at",
    "last_original_not_retained_reason",
    "original_retention_state",
    "notices_failed",
    "last_notice_failed_at",
    "notice_config_error",
];

/// Empty collection fields that define a stable response envelope and must
/// survive Agent-mode compaction. Dropping these turns an empty page into a
/// different response type and leaves callers unable to distinguish an empty
/// result from a missing/unsupported field.
const EMPTY_ARRAY_PRESERVE: &[&str] = &[
    "items",
    "entities",
    "notes",
    "edges",
    "results",
    "entries",
    "neighbors",
];

fn is_stable_list_envelope(map: &Map<String, Value>) -> bool {
    map.contains_key("requested_limit")
        && map.contains_key("effective_limit")
        && map.contains_key("limit_clamped")
        && EMPTY_ARRAY_PRESERVE
            .iter()
            .any(|field| map.contains_key(*field))
}

/// Keyset cursor pages outside the ADR-023 envelope — `knowledge.list(after=…)`
/// returns `{results, limit, order, next_after}` — are structural in the same
/// way (ADR-045 Amendment 4): an empty `results` page and a terminal
/// `next_after: null` are the walk's completion signals, so a caller that
/// cannot see them cannot stop. A `results` array without a sibling
/// `next_after` key is an ordinary response and gets the generic transform.
fn is_keyset_cursor_envelope(map: &Map<String, Value>) -> bool {
    map.contains_key("next_after")
        && (map.get("results").is_some_and(Value::is_array)
            || (map.get("head_seq").is_some_and(Value::is_number)
                && map.get("entries").is_some_and(Value::is_array)))
}

fn is_stream_entry(map: &Map<String, Value>) -> bool {
    map.get("seq").is_some_and(Value::is_number)
        && map.contains_key("id")
        && map.contains_key("created_at")
        && map.contains_key("record")
}

/// Field names carrying caller-supplied payload timestamps that must never be
/// transformed, regardless of nesting.
///
/// These encode domain semantics the caller needs to round-trip verbatim —
/// e.g. `trigger_at` on a `schedule.remind`/`schedule.schedule` create
/// response, returned as a top-level convenience field alongside `id` and
/// `full_id`, not nested under `"properties"`. The `inside_properties` guard
/// alone only protects fields nested under a literal `"properties"` key
/// (as returned by `agenda`/`get`); it does not cover this top-level case,
/// which older Agent presentation rewrote into a relative or minute-truncated
/// form, discarding the seconds and offset needed for round trips (#871).
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
    "rank_score",
    "vector_similarity",
    "keyword_score",
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
    if key == "full_id" || key == "bridge_instance_id" || ROUND_TRIP_FULL_UUID_FIELDS.contains(&key)
    {
        return false;
    }
    key == "id" || key.ends_with("_id") || matches!(key, "superseded_by" | "replaced_by")
}

/// Transform a successful verb result value according to the given
/// [`PresentationMode`].
///
/// - `Verbose` / `Human`: returns `value` unchanged.
/// - `Agent`: applies UUID shortening, exact UTC timestamp rendering, empty-field
///   dropping, structural-null preservation, and score truncation.
///
/// `now_unix_seconds` is a whole-second clock for callers without a finer
/// sample. All relative list-row labels within a response use this instant.
pub fn present(value: Value, mode: PresentationMode, now_unix_seconds: i64) -> Value {
    present_with_policy(
        value,
        mode,
        now_unix_seconds,
        VerbPresentationPolicy::Standard,
    )
}

/// Present a successful result using its trusted registered verb policy.
/// Receipt policies preserve only their closed string paths, not descendants.
pub fn present_with_policy(
    value: Value,
    mode: PresentationMode,
    now_unix_seconds: i64,
    policy: VerbPresentationPolicy,
) -> Value {
    present_with_policy_at(value, mode, now_unix_seconds.into(), policy)
}

/// A response-wide clock sample. Fractional precision distinguishes a newly
/// stored row from a truly future row within the same Unix second.
#[derive(Clone, Copy)]
pub struct PresentationNow {
    seconds: i64,
    nanoseconds: u32,
}

impl From<i64> for PresentationNow {
    fn from(seconds: i64) -> Self {
        Self {
            seconds,
            nanoseconds: 0,
        }
    }
}

impl From<i32> for PresentationNow {
    fn from(seconds: i32) -> Self {
        i64::from(seconds).into()
    }
}

impl From<chrono::DateTime<chrono::Utc>> for PresentationNow {
    fn from(now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            seconds: now.timestamp(),
            nanoseconds: now.timestamp_subsec_nanos(),
        }
    }
}

/// Present with the response's single sampled instant, including its fraction.
pub fn present_with_policy_at(
    value: Value,
    mode: PresentationMode,
    now: PresentationNow,
    policy: VerbPresentationPolicy,
) -> Value {
    if policy == VerbPresentationPolicy::AlwaysVerbose {
        return value;
    }
    match mode {
        PresentationMode::Verbose | PresentationMode::Human => value,
        PresentationMode::Agent => {
            // These root paths are selected by the registered agenda handler,
            // never by a payload marker or a nested lookalike. The cursor's
            // timestamp spelling and full UUID jointly define its seek position.
            let mut value = value;
            let agenda_next = (policy == VerbPresentationPolicy::AgendaContinuation)
                .then(|| value.as_object_mut().and_then(|map| map.remove("next")))
                .flatten();
            let agenda_empty = policy == VerbPresentationPolicy::AgendaContinuation
                && value
                    .get("events")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty);
            let config = AgentTransformConfig {
                preserved_nulls: LIFECYCLE_NULL_PRESERVE.iter().copied().collect(),
                scores: SCORE_FIELDS.iter().copied().collect(),
                payload_timestamps: PAYLOAD_TIMESTAMP_FIELDS.iter().copied().collect(),
                now,
            };
            let mut value = transform_agent(
                value,
                &config,
                AgentTreeContext {
                    inside_properties: false,
                    object_properties: false,
                    array_member: false,
                    receipt_context: match policy {
                        VerbPresentationPolicy::StreamAppendReceipt => ReceiptContext::AppendRoot,
                        VerbPresentationPolicy::StreamBatchReceipts => ReceiptContext::BatchRoot,
                        _ => ReceiptContext::None,
                    },
                },
            );
            if let Some(map) = value.as_object_mut() {
                if let Some(next) = agenda_next {
                    map.insert("next".into(), next);
                }
                if agenda_empty {
                    map.insert("events".into(), Value::Array(Vec::new()));
                }
            }
            value
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiptContext {
    None,
    AppendRoot,
    BatchRoot,
    BatchResults,
    BatchMember,
}

struct AgentTransformConfig {
    preserved_nulls: HashSet<&'static str>,
    scores: HashSet<&'static str>,
    payload_timestamps: HashSet<&'static str>,
    now: PresentationNow,
}

#[derive(Clone, Copy)]
struct AgentTreeContext {
    inside_properties: bool,
    object_properties: bool,
    array_member: bool,
    receipt_context: ReceiptContext,
}

/// Apply the Agent-mode transform to an arbitrary JSON value.
///
/// `inside_properties` is `true` when recursing inside a `"properties"` value.
/// Caller-supplied payload timestamps (e.g. `trigger_at`) must not be normalized
/// because they encode domain semantics the agent may need to round-trip.
fn transform_agent(
    value: Value,
    config: &AgentTransformConfig,
    context: AgentTreeContext,
) -> Value {
    match value {
        Value::Object(map) => {
            let preserve_list_envelope =
                is_stable_list_envelope(&map) || is_keyset_cursor_envelope(&map);
            let stream_entry = is_stream_entry(&map);
            let relative_created_at = (context.array_member
                && !context.inside_properties
                && !map.contains_key("created_at_relative")
                && !matches!(
                    context.receipt_context,
                    ReceiptContext::AppendRoot | ReceiptContext::BatchMember
                ))
            .then(|| map.get("created_at").and_then(Value::as_str))
            .flatten()
            .and_then(|raw| {
                let (_, created_at) = exact_timestamp(raw)?;
                let age = config.now.seconds.checked_sub(created_at)?;
                // Floor the full fractional age. The lexicographic comparison
                // also handles canonical fractions longer than nanoseconds.
                age.checked_sub(i64::from(
                    timestamp_fraction_cmp(raw, config.now.nanoseconds) == Ordering::Greater,
                ))
            })
            .filter(|difference| *difference >= 0)
            .map(relative_time);
            let mut out = Map::new();
            for (k, v) in map {
                if stream_entry && k == "record" {
                    out.insert(k, v);
                    continue;
                }
                if v.is_string()
                    && ((context.receipt_context == ReceiptContext::AppendRoot
                        && k == "created_at")
                        || (context.receipt_context == ReceiptContext::BatchMember
                            && matches!(k.as_str(), "created_at" | "updated_at")))
                {
                    out.insert(k, v);
                    continue;
                }
                let child_receipt_context = if context.receipt_context == ReceiptContext::BatchRoot
                    && k == "results"
                    && v.is_array()
                {
                    ReceiptContext::BatchResults
                } else {
                    ReceiptContext::None
                };
                // ADR-045 Amendment 3 scopes the empty-string carve-out to
                // strings nested under an object-valued `properties`; a
                // scalar or array `properties` value gets no carve-out.
                let child_inside_properties = context.inside_properties || k == "properties";
                let child_object_properties =
                    context.object_properties || (k == "properties" && v.is_object());
                let transformed = transform_field_agent(
                    &k,
                    v,
                    config,
                    AgentFieldContext {
                        inside_properties: child_inside_properties,
                        object_properties: child_object_properties,
                        preserve_list_envelope,
                        receipt_context: child_receipt_context,
                    },
                );
                match transformed {
                    None => {} // drop
                    Some(tv) => {
                        out.insert(k, tv);
                    }
                }
            }
            if let Some(relative) = relative_created_at {
                out.insert("created_at_relative".to_string(), Value::String(relative));
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            let items: Vec<Value> = arr
                .into_iter()
                .map(|v| {
                    transform_agent(
                        v,
                        config,
                        AgentTreeContext {
                            inside_properties: context.inside_properties,
                            object_properties: context.object_properties,
                            array_member: true,
                            receipt_context: if context.receipt_context
                                == ReceiptContext::BatchResults
                            {
                                ReceiptContext::BatchMember
                            } else {
                                ReceiptContext::None
                            },
                        },
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
/// `inside_properties` suppresses timestamp normalization for caller-submitted
/// payload values nested under a literal `"properties"` key (e.g. `trigger_at`
/// as returned by `agenda`/`get`). `payload_timestamps` suppresses normalization
/// by field name regardless of nesting, covering top-level convenience fields
/// such as the `trigger_at` returned directly in a `schedule.remind`/
/// `schedule.schedule` create response (#871). Metadata timestamps at the top
/// level (`created_at`, `updated_at`) still render as exact UTC instants.
#[derive(Clone, Copy)]
struct AgentFieldContext {
    inside_properties: bool,
    object_properties: bool,
    preserve_list_envelope: bool,
    receipt_context: ReceiptContext,
}

fn transform_field_agent(
    key: &str,
    value: Value,
    config: &AgentTransformConfig,
    context: AgentFieldContext,
) -> Option<Value> {
    match &value {
        // Preserve lifecycle and stable-envelope nulls; drop other nulls.
        Value::Null => {
            if config.preserved_nulls.contains(key)
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
        // Preserve empty strings under a record's `properties` object: those
        // keys exist only because a caller wrote them, so `""` there is data
        // (set-to-empty vs absent, ADR-045 Amendment 3, issue #1995). Empty
        // arrays and objects under `properties` are still dropped per the
        // amendment's scope note.
        Value::String(s) if s.is_empty() && context.object_properties => Some(value),
        // Drop other empty strings, arrays, objects.
        Value::String(s) if s.is_empty() => None,
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        // Truncate score fields.
        Value::Number(_) if config.scores.contains(key) => {
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
        // Render whole, offset-bearing timestamps in exact UTC form unless
        // this is caller-supplied or otherwise protected payload data.
        Value::String(s)
            if !context.inside_properties
                && !config.payload_timestamps.contains(key)
                && looks_like_iso8601(s) =>
        {
            Some(Value::String(
                exact_timestamp(s).map_or_else(|| s.clone(), |(rendered, _)| rendered),
            ))
        }
        // Recurse into objects and arrays.
        Value::Object(_) | Value::Array(_) => Some(transform_agent(
            value,
            config,
            AgentTreeContext {
                inside_properties: context.inside_properties,
                object_properties: context.object_properties,
                array_member: false,
                receipt_context: context.receipt_context,
            },
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

/// Render a whole timestamp in UTC while retaining its original fraction digits.
fn exact_timestamp(s: &str) -> Option<(String, i64)> {
    let unix = parse_iso8601_unix(s)?;
    let tail = s.get(19..)?;
    let fraction = if let Some(after_dot) = tail.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        tail.get(..digits + 1)?
    } else {
        ""
    };
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)?;
    Some((
        format!("{}{fraction}Z", utc.format("%Y-%m-%dT%H:%M:%S")),
        unix,
    ))
}

/// Compare the timestamp's decimal fraction with a sampled nanosecond value.
/// Digits beyond nanoseconds matter when the leading nine digits tie.
fn timestamp_fraction_cmp(timestamp: &str, now_nanoseconds: u32) -> Ordering {
    let fraction = if timestamp.as_bytes().get(19) == Some(&b'.') {
        timestamp.as_bytes()[20..]
            .split(|byte| !byte.is_ascii_digit())
            .next()
            .unwrap_or_default()
    } else {
        &[]
    };
    let now_fraction = format!("{now_nanoseconds:09}");
    for index in 0..fraction.len().max(9) {
        let created_digit = fraction.get(index).copied().unwrap_or(b'0');
        let now_digit = now_fraction.as_bytes().get(index).copied().unwrap_or(b'0');
        match created_digit.cmp(&now_digit) {
            Ordering::Equal => {}
            difference => return difference,
        }
    }
    Ordering::Equal
}

/// Parse a whole ISO-8601 datetime with an explicit offset to Unix seconds.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM|±HHMM)`. An offset-less,
/// invalid or trailing-text value stays byte-exact in Agent presentation.
fn parse_iso8601_unix(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = parse_digits(&b[0..4])?;
    let month: i64 = parse_digits(&b[5..7])?;
    let day: i64 = parse_digits(&b[8..10])?;
    let hour: i64 = parse_digits(&b[11..13])?;
    let minute: i64 = parse_digits(&b[14..16])?;
    let second: i64 = parse_digits(&b[17..19])?;

    let local = chrono::NaiveDate::from_ymd_opt(
        year.try_into().ok()?,
        month.try_into().ok()?,
        day.try_into().ok()?,
    )?
    .and_hms_opt(
        hour.try_into().ok()?,
        minute.try_into().ok()?,
        second.try_into().ok()?,
    )?
    .and_utc()
    .timestamp();
    let offset_secs = parse_tz_offset_secs(s.get(19..)?)?;
    local.checked_sub(offset_secs)
}

/// Parse the tail of an ISO-8601 timestamp (everything from byte index 19
/// onward, i.e. after the whole-seconds field) into a UTC offset in seconds.
///
/// Handles, in order: optional fractional seconds (`.nnn`, skipped — this
/// parser only has whole-second precision), then one of:
/// - `"Z"` → offset 0
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

    if rest == "Z" {
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
    if !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let s = std::str::from_utf8(b).ok()?;
    s.parse().ok()
}

/// Format a duration in seconds as a relative time string (e.g. `"3m ago"`).
fn relative_time(diff_secs: i64) -> String {
    if diff_secs < 60 {
        format!("{diff_secs}s ago")
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
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
    let rounded = if rounded.is_finite() {
        rounded
    } else {
        // Decimal formatting handles subnormals without an overflowing scale.
        // If rounding would exceed f64's range, retain the finite input.
        format!("{f:.2e}")
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .unwrap_or(f)
    };
    // Re-serialize through serde_json to avoid floating-point noise.
    serde_json::Number::from_f64(rounded)
        .map(Value::Number)
        .unwrap_or(Value::from(f))
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "presentation_stream_presentation_tests.rs"]
mod stream_presentation_tests;

#[cfg(test)]
#[path = "presentation_issue_2537_standard_tests.rs"]
mod issue_2537_standard_tests;

#[cfg(test)]
#[path = "presentation_issue_2537_receipt_policy_tests.rs"]
mod issue_2537_receipt_policy_tests;

#[cfg(test)]
#[path = "presentation_parsed_note_content_tests.rs"]
mod parsed_note_content_tests;

#[cfg(test)]
#[path = "presentation_adr045_amendment9_tests.rs"]
mod adr045_amendment9_tests;
