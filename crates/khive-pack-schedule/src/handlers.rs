//! Verb handler implementations for the schedule pack.
//!
//! All four verbs (`remind`, `schedule`, `agenda`, `cancel`) store and query
//! `scheduled_event` notes. Trigger evaluation is NOT performed by the pack —
//! the pack only stores intent. See `docs/design.md` for execution modes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter, SortDir};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

/// Resolve a raw id string to a full UUID.
///
/// Accepts a 36-char hyphenated UUID or an 8+ hex-char short prefix.
/// The prefix is resolved via `runtime.resolve_prefix` (namespace-scoped).
async fn resolve_id(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
    verb: &str,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = raw.parse::<Uuid>() {
        return Ok(uuid);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, raw).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "{verb}: no record matches prefix: {raw:?}"
            ))),
        };
    }
    Err(RuntimeError::InvalidInput(format!(
        "{verb}: invalid id {raw:?}; expected full UUID or 8-char hex prefix"
    )))
}

fn note_to_event_json(note: &Note) -> Value {
    json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "kind": "scheduled_event",
        "content": note.content,
        "namespace": note.namespace,
        "properties": note.properties,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
    })
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Validates `at` is an RFC 3339 timestamp lying in the future; returns the
/// parsed instant. See `docs/api/replay-validation.md#validate_at` for accepted
/// formats and rationale.
fn validate_at(verb: &str, at: &str) -> Result<DateTime<Utc>, RuntimeError> {
    let parsed = at.parse::<DateTime<Utc>>().map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "{verb}.at: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), got {at:?}"
        ))
    })?;
    if parsed <= Utc::now() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}.at: cannot schedule in the past (got {at:?}); \
             use a future timestamp"
        )));
    }
    Ok(parsed)
}

/// Validates a repeat spec: `daily`/`weekly`/`monthly`, or a limited 5-field
/// `MIN HOUR DOM MON DOW` cron-lite form (no steps/ranges/lists — issue
/// #481). See `docs/api/replay-validation.md#validate_repeat` for field ranges
/// and rationale.
fn validate_repeat(repeat: &str) -> Result<(), RuntimeError> {
    match repeat {
        "daily" | "weekly" | "monthly" => return Ok(()),
        _ => {}
    }

    let fields: Vec<&str> = repeat.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid repeat expression {repeat:?}: must be \"daily\", \"weekly\", \
             \"monthly\", or a limited 5-field form (MIN HOUR DOM MON DOW) where each \
             field is '*' or one in-range integer; cron operators such as steps, \
             ranges, and lists are not accepted"
        )));
    }

    // (field_name, min_val, max_val)
    let ranges: [(&str, u64, u64); 5] = [
        ("minute", 0, 59),
        ("hour", 0, 23),
        ("day-of-month", 1, 31),
        ("month", 1, 12),
        ("day-of-week", 0, 7),
    ];
    for (field, (name, lo, hi)) in fields.iter().zip(ranges.iter()) {
        if *field == "*" {
            continue;
        }
        match field.parse::<u64>() {
            Ok(v) if v >= *lo && v <= *hi => {}
            Ok(v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid repeat expression {repeat:?}: cron {name} field {v} is out of \
                     range {lo}–{hi}"
                )));
            }
            Err(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "invalid repeat expression {repeat:?}: cron {name} field {field:?} is not \
                     \"*\" or a non-negative integer"
                )));
            }
        }
    }
    Ok(())
}

/// Validates `action` parses as DSL via `khive_request::parse_request`,
/// catching garbage at write time rather than trigger time. Returns the
/// parsed request so callers can inspect verb names without re-parsing.
fn validate_action(action: &str) -> Result<khive_request::ParsedRequest, RuntimeError> {
    khive_request::parse_request(action).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "schedule.action: invalid DSL ({e}); \
             provide a valid verb call (e.g. \"schedule.remind(content=\\\"hello\\\")\")"
        ))
    })
}

/// Validates a parsed `schedule.schedule` action can be replayed exactly as
/// stored (issue #461): single op, exactly-registered handler, literal args
/// only, all required params present, and no handler that treats
/// `namespace` as a business arg (issue #461/#462). See
/// `docs/api/replay-validation.md#validate_replayable_single_action`.
fn validate_replayable_single_action(
    parsed: &khive_request::ParsedRequest,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    if parsed.mode != khive_request::ExecutionMode::Single {
        return Err(RuntimeError::InvalidInput(
            "schedule.action: chains and $prev references are not supported for scheduled \
             replay; provide a single verb call"
                .into(),
        ));
    }

    let op = parsed
        .ops
        .first()
        .ok_or_else(|| RuntimeError::InvalidInput("schedule.action: missing verb".into()))?;

    let help = registry.describe_verb(&op.tool).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "schedule.action: verb {:?} is not registered; use the exact pack-prefixed name \
             (e.g. \"schedule.remind(...)\")",
            op.tool
        ))
    })?;

    for value in op.args.values() {
        if !matches!(value, khive_request::ArgValue::Value(_)) {
            return Err(RuntimeError::InvalidInput(
                "schedule.action: $prev references are not replayable in a scheduled action".into(),
            ));
        }
    }

    // Reject handlers whose schema declares `namespace` as a business param
    // (issue #461/#462) — see docs/api/replay-validation.md#validate_replayable_single_action.
    let handler_accepts_namespace =
        help.get("params")
            .and_then(Value::as_array)
            .is_some_and(|params| {
                params
                    .iter()
                    .any(|p| p.get("name").and_then(Value::as_str) == Some("namespace"))
            });
    if handler_accepts_namespace {
        return Err(RuntimeError::InvalidInput(format!(
            "schedule.action: verb {:?} treats `namespace` as a business argument; scheduled \
             replay would overwrite it with the event's routing namespace, silently changing \
             the business value on trigger regardless of whether `namespace` was stored. This \
             verb cannot be scheduled",
            op.tool
        )));
    }

    validate_args_against_help(&op.tool, &op.args, &help)?;
    validate_conditional_requirements(&op.tool, &op.args, registry)
}

/// Rejects scheduled actions known to fail a handler's *conditional*
/// required param even though `describe_verb` marks none of the
/// alternatives `required:true` (issue #461) — hard-codes the `create`
/// kind/name/content cases. See
/// `docs/api/replay-validation.md#validate_conditional_requirements`.
fn validate_conditional_requirements(
    tool: &str,
    args: &std::collections::BTreeMap<String, khive_request::ArgValue>,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    if tool != "create" {
        return Ok(());
    }

    let has_kind = args.contains_key("kind");
    let has_items = args.contains_key("items");

    if !has_kind && !has_items {
        return Err(RuntimeError::InvalidInput(
            "schedule.action: verb \"create\" requires either `kind` (singleton) or `items` \
             (bulk); neither is present"
                .into(),
        ));
    }

    // Bulk path takes priority over `kind`, mirroring `handle_create`'s
    // early-exit on `items` before the singleton `kind` requirement is even
    // checked.
    if has_items {
        let items_value = args
            .get("items")
            .and_then(khive_request::ArgValue::as_value)
            .cloned()
            .unwrap_or(Value::Null);
        return validate_create_bulk_items(&items_value, registry);
    }

    let kind_str = args
        .get("kind")
        .and_then(khive_request::ArgValue::as_value)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let entity_kind_arg = args
        .get("entity_kind")
        .and_then(khive_request::ArgValue::as_value)
        .and_then(Value::as_str);
    let note_kind_arg = args
        .get("note_kind")
        .and_then(khive_request::ArgValue::as_value)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    match classify_create_kind(kind_str, registry)? {
        CreateKindClass::Entity { specific } => {
            let canonical = reconcile_specific_for_replay(
                "",
                specific,
                entity_kind_arg,
                |s| canonical_entity_kind_for_replay(s, registry),
                "entity_kind",
            )?;
            let Some(canonical) = canonical else {
                return Err(RuntimeError::InvalidInput(
                    "schedule.action: verb \"create\": kind=\"entity\" requires a specific \
                     kind — use kind=<concept|document|dataset|project|person|org|artifact|\
                     service|resource> directly, or kind=entity + entity_kind=<…>"
                        .into(),
                ));
            };
            let name = args
                .get("name")
                .and_then(khive_request::ArgValue::as_value)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if name.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "schedule.action: verb \"create\": entity creation requires `name`".into(),
                ));
            }
            let entity_type_arg = args
                .get("entity_type")
                .and_then(khive_request::ArgValue::as_value)
                .and_then(Value::as_str);
            validate_entity_type_for_replay(&canonical, entity_type_arg, registry).map_err(
                |e| RuntimeError::InvalidInput(format!("schedule.action: verb \"create\": {e}")),
            )?;
        }
        CreateKindClass::Note { specific } => {
            reconcile_specific_for_replay(
                "",
                specific,
                note_kind_arg,
                |s| canonical_note_kind_for_replay(s, registry),
                "note_kind",
            )?;
            let content = args
                .get("content")
                .and_then(khive_request::ArgValue::as_value)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if content.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "schedule.action: verb \"create\": note creation requires `content`".into(),
                ));
            }
        }
    }

    Ok(())
}

/// Resolved shape of a `create(kind=...)` discriminator, mirroring
/// `khive-pack-kg`'s `KindSpec` for the two branches that carry a
/// conditional requirement (`entity` needs `name`, `note` needs `content`).
/// Both variants carry the resolved granular `specific` kind (`None` for a
/// bare `"entity"`/`"note"`) so callers can run the same kind/legacy-kind
/// reconciliation `khive-pack-kg::handlers::create` runs.
enum CreateKindClass {
    Entity { specific: Option<String> },
    Note { specific: Option<String> },
}

/// Classifies a `create(kind=...)` value mirroring
/// `khive-pack-kg::handlers::common::resolve_kind_spec`; errors on any kind
/// guaranteed to fail replay (`edge`, `event`, `proposal`, unrecognized). See
/// `docs/api/replay-validation.md#classify_create_kind`.
fn classify_create_kind(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<CreateKindClass, RuntimeError> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "entity" => return Ok(CreateKindClass::Entity { specific: None }),
        "note" => return Ok(CreateKindClass::Note { specific: None }),
        "edge" => {
            return Err(RuntimeError::InvalidInput(
                "schedule.action: verb \"create\" with kind=\"edge\" always fails at replay; \
                 edges are created via `link`, not `create`"
                    .into(),
            ));
        }
        "event" => {
            return Err(RuntimeError::InvalidInput(
                "schedule.action: verb \"create\" with kind=\"event\" always fails at replay; \
                 events are immutable and not creatable via `create`"
                    .into(),
            ));
        }
        "proposal" => {
            return Err(RuntimeError::InvalidInput(
                "schedule.action: verb \"create\" with kind=\"proposal\" always fails at \
                 replay; use `propose` to create a proposal"
                    .into(),
            ));
        }
        _ => {}
    }
    if let Ok(k) = raw.parse::<khive_types::EntityKind>() {
        return Ok(CreateKindClass::Entity {
            specific: Some(k.name().to_string()),
        });
    }
    if resource_alias_for_replay(&normalized) {
        return Ok(CreateKindClass::Entity {
            specific: Some("resource".to_string()),
        });
    }
    if registry.all_entity_kinds().contains(&normalized.as_str()) {
        return Ok(CreateKindClass::Entity {
            specific: Some(normalized),
        });
    }
    if registry.all_note_kinds().contains(&normalized.as_str()) {
        return Ok(CreateKindClass::Note {
            specific: Some(normalized),
        });
    }
    Err(RuntimeError::InvalidInput(format!(
        "schedule.action: verb \"create\" with kind={raw:?} always fails at replay (unknown \
         kind; valid: entity | note | edge | event | proposal | {} | {})",
        registry.all_entity_kinds().join(" | "),
        registry.all_note_kinds().join(" | "),
    )))
}

/// Canonicalizes a legacy `entity_kind` value mirroring
/// `khive-pack-kg::handlers::common::canonical_entity_kind`. See
/// `docs/api/replay-validation.md#canonical_entity_kind_for_replay-canonical_note_kind_for_replay`.
fn canonical_entity_kind_for_replay(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<String, RuntimeError> {
    if let Ok(k) = raw.parse::<khive_types::EntityKind>() {
        return Ok(k.name().to_string());
    }
    let normalized = raw.trim().to_ascii_lowercase();
    if resource_alias_for_replay(&normalized) {
        return Ok("resource".to_string());
    }
    if registry.all_entity_kinds().contains(&normalized.as_str()) {
        return Ok(normalized);
    }
    let mut all: Vec<&'static str> = registry.all_entity_kinds();
    all.sort_unstable();
    Err(RuntimeError::InvalidInput(format!(
        "unknown entity_kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

/// Canonicalizes a legacy `note_kind` value mirroring
/// `khive-pack-kg::handlers::common::canonical_note_kind`; note kinds carry
/// no alias set beyond their 5 canonical names (ADR-013).
fn canonical_note_kind_for_replay(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<String, RuntimeError> {
    let normalized = raw.trim().to_ascii_lowercase();
    if registry.all_note_kinds().contains(&normalized.as_str()) {
        return Ok(normalized);
    }
    let mut all: Vec<&'static str> = registry.all_note_kinds();
    all.sort_unstable();
    Err(RuntimeError::InvalidInput(format!(
        "unknown note_kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

/// Hand-copied ADR-048 `resource`-kind alias set mirroring
/// `khive-pack-kg::vocab::EntityKind`'s `FromStr` arm (that type is
/// pack-private). `normalized` must already be trimmed + lowercased. Kept in
/// sync with the CI-checked `entity_kind_resource_aliases_match_real_vocab`
/// test. See `docs/api/replay-validation.md#resource_alias_for_replay`.
fn resource_alias_for_replay(normalized: &str) -> bool {
    matches!(
        normalized,
        "resource" | "atom" | "runbook" | "template" | "prompt" | "skill" | "tool"
    )
}

/// Validates an `entity_type` value mirroring bit-for-bit
/// `khive-pack-kg::handlers::common::validate_entity_type`'s replay parity,
/// resolving subtypes against the boot-time composed registry (builtin +
/// every loaded pack's `ENTITY_TYPES`), not the builtin-only
/// `EntityTypeRegistry::global()`. See
/// `docs/api/replay-validation.md#validate_entity_type_for_replay`.
fn validate_entity_type_for_replay(
    canonical_kind_name: &str,
    entity_type: Option<&str>,
    registry: &VerbRegistry,
) -> Result<Option<String>, RuntimeError> {
    let Some(raw) = entity_type else {
        return Ok(None);
    };
    let kind = canonical_kind_name
        .parse::<khive_types::EntityKind>()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!("unknown entity kind {canonical_kind_name:?}"))
        })?;
    khive_types::EntityTypeRegistry::with_extra(registry.all_entity_types())
        .resolve(kind, Some(raw))
        .map(|resolved| resolved.entity_type)
        .map_err(RuntimeError::from)
}

/// Reconciles a granular `kind`'s resolved `specific` value against a legacy
/// `entity_kind`/`note_kind` argument, mirroring
/// `khive-pack-kg::handlers::common::reconcile_specific` exactly. `context`
/// prefixes error messages (e.g. `"items[3] "` for a bulk entry). See
/// `docs/api/replay-validation.md#reconcile_specific_for_replay`.
fn reconcile_specific_for_replay(
    context: &str,
    spec_specific: Option<String>,
    legacy_raw: Option<&str>,
    canonicalize: impl Fn(&str) -> Result<String, RuntimeError>,
    legacy_field: &str,
) -> Result<Option<String>, RuntimeError> {
    let legacy_canonical = match legacy_raw {
        Some(s) => Some(canonicalize(s).map_err(|e| {
            RuntimeError::InvalidInput(format!("schedule.action: verb \"create\": {context}{e}"))
        })?),
        None => None,
    };
    match (spec_specific, legacy_canonical) {
        (Some(a), Some(b)) if a != b => Err(RuntimeError::InvalidInput(format!(
            "schedule.action: verb \"create\": {context}kind={a:?} contradicts \
             {legacy_field}={b:?}; pick one"
        ))),
        (Some(a), _) => Ok(Some(a)),
        (None, b) => Ok(b),
    }
}

/// A single entry in a bulk `create(items=[...])` action, mirroring
/// `khive-pack-kg::handlers::params::BulkCreateEntry`'s exact field set
/// (including `#[serde(deny_unknown_fields)]`) so schedule-time validation
/// rejects the same malformed entries the real bulk handler would.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // fields exist only to mirror BulkCreateEntry's deserialize shape
struct ScheduleBulkCreateEntryCheck {
    kind: String,
    name: String,
    entity_kind: Option<String>,
    entity_type: Option<String>,
    description: Option<String>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
}

/// Validate a `create(items=[...])` bulk payload the way `handle_create`'s
/// bulk path would: `items` must parse into the same shape as
/// `BulkCreateEntry` (required `kind` + `name`, deny-unknown-fields), and
/// bulk create only supports entity kinds (never note kinds).
fn validate_create_bulk_items(
    items_value: &Value,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    let entries: Vec<ScheduleBulkCreateEntryCheck> = serde_json::from_value(items_value.clone())
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "schedule.action: verb \"create\": malformed `items` — could not parse bulk \
             entries: {e}"
            ))
        })?;
    if entries.len() > 1000 {
        return Err(RuntimeError::InvalidInput(
            "schedule.action: verb \"create\": bulk create limited to 1000 entries per request"
                .into(),
        ));
    }
    for (idx, entry) in entries.iter().enumerate() {
        match classify_create_kind(&entry.kind, registry)? {
            CreateKindClass::Entity { specific } => {
                let canonical = reconcile_specific_for_replay(
                    &format!("items[{idx}] "),
                    specific,
                    entry.entity_kind.as_deref(),
                    |s| canonical_entity_kind_for_replay(s, registry),
                    "entity_kind",
                )?;
                let Some(canonical) = canonical else {
                    return Err(RuntimeError::InvalidInput(format!(
                        "schedule.action: verb \"create\": items[{idx}] kind=\"entity\" \
                         requires a specific kind — use kind=<concept|…> or kind=entity + \
                         entity_kind=<…>"
                    )));
                };
                validate_entity_type_for_replay(&canonical, entry.entity_type.as_deref(), registry)
                    .map_err(|e| {
                        RuntimeError::InvalidInput(format!(
                            "schedule.action: verb \"create\": items[{idx}] {e}"
                        ))
                    })?;
            }
            CreateKindClass::Note { .. } => {
                return Err(RuntimeError::InvalidInput(format!(
                    "schedule.action: verb \"create\": items[{idx}] bulk create only supports \
                     entity kinds; got kind={:?}",
                    entry.kind
                )));
            }
        }
    }
    Ok(())
}

/// Validate `args` against a verb's `describe_verb` help schema: reject
/// unknown argument names and ensure every required parameter is present.
fn validate_args_against_help(
    tool: &str,
    args: &std::collections::BTreeMap<String, khive_request::ArgValue>,
    help: &Value,
) -> Result<(), RuntimeError> {
    let params: &[Value] = help
        .get("params")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    for name in args.keys() {
        let known = params
            .iter()
            .any(|p| p.get("name").and_then(Value::as_str) == Some(name.as_str()));
        if !known {
            return Err(RuntimeError::InvalidInput(format!(
                "schedule.action: verb {tool:?} does not accept argument {name:?}"
            )));
        }
    }

    for p in params {
        let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
        if !required {
            continue;
        }
        let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
        if !args.contains_key(name) {
            return Err(RuntimeError::InvalidInput(format!(
                "schedule.action: verb {tool:?} is missing required argument {name:?}"
            )));
        }
    }

    Ok(())
}

// ── param structs ────────────────────────────────────────────────────────────

// ue-errors C1 (cross-pack): deny_unknown_fields so typo kwargs are rejected
// at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemindParams {
    pub content: String,
    pub at: String,
    #[serde(default)]
    pub repeat: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleParams {
    pub action: String,
    pub at: String,
    #[serde(default)]
    pub repeat: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgendaParams {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelParams {
    pub id: String,
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// `remind` — create a time-triggered reminder.
pub(crate) async fn handle_remind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    params: Value,
) -> Result<Value, RuntimeError> {
    if registry.describe_verb("comm.send").is_err() {
        return Err(RuntimeError::InvalidInput(
            "schedule.remind requires the comm delivery capability `comm.send`; load the `comm` pack"
                .into(),
        ));
    }

    let p: RemindParams = deser(params)?;
    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "remind: `content` must not be empty".into(),
        ));
    }
    if p.at.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "remind: `at` must not be empty".into(),
        ));
    }
    // Validate RFC 3339 and reject past timestamps (C3).
    // Preserve the caller's original string as `trigger_at` so the
    // submitted wall time and offset are round-tripped faithfully (H5).
    // The UTC instant is used only for comparison/ordering.
    let trigger_at_original = p.at.trim().to_string();
    let _trigger_utc = validate_at("remind", &trigger_at_original)?;

    if let Some(ref r) = p.repeat {
        validate_repeat(r)?;
    }

    let properties = json!({
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
        "event_type": "remind",
        "created_by_actor": token.actor().id.clone(),
        "payload": null,
        "fired_at": null,
        "cancelled_at": null,
    });

    let note = runtime
        .create_note(
            token,
            "scheduled_event",
            None,
            &p.content,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "event_type": "remind",
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
    }))
}

/// `schedule` — schedule a future verb dispatch.
pub(crate) async fn handle_schedule(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ScheduleParams = deser(params)?;
    if p.action.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "schedule: `action` must not be empty".into(),
        ));
    }
    if p.at.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "schedule: `at` must not be empty".into(),
        ));
    }
    // Validate DSL parseability at write time (C4). Garbage like "x" or
    // "bogus-not-a-valid-verb()" is rejected before it enters storage.
    let parsed = validate_action(p.action.trim())?;
    // Validate that the action is a single, exactly-registered verb call with
    // only literal args and all required params present, so the pending-events
    // runner can replay it exactly at trigger time (issue #461). Bare
    // shorthand (e.g. "remind(...)") is rejected: it is not the verb name
    // that gets stored and replayed, so accepting it here would let a
    // trigger-time replay fail as an unknown verb.
    validate_replayable_single_action(&parsed, registry)?;

    // Validate RFC 3339 and reject past timestamps (C3).
    // Preserve the caller's original string as `trigger_at` so the
    // submitted wall time and offset are round-tripped faithfully (H5).
    // The UTC instant is used only for comparison/ordering.
    let trigger_at_original = p.at.trim().to_string();
    let _trigger_utc = validate_at("schedule", &trigger_at_original)?;

    if let Some(ref r) = p.repeat {
        validate_repeat(r)?;
    }

    let properties = json!({
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
        "event_type": "schedule",
        "payload": p.action,
        "fired_at": null,
        "cancelled_at": null,
    });

    let note = runtime
        .create_note(
            token,
            "scheduled_event",
            None,
            &p.action,
            None,
            Some(properties),
            Vec::new(),
        )
        .await?;

    Ok(json!({
        "id": short_id(note.id),
        "full_id": note.id.as_hyphenated().to_string(),
        "event_type": "schedule",
        "trigger_at": trigger_at_original,
        "repeat": p.repeat,
        "status": "pending",
    }))
}

/// `agenda` — list upcoming scheduled events.
pub(crate) async fn handle_agenda(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: AgendaParams = deser(params)?;
    let limit: u32 = match p.limit {
        None => 20,
        Some(0) => {
            return Err(RuntimeError::InvalidInput(
                "agenda: `limit` must be between 1 and 200 (inclusive); got 0".into(),
            ));
        }
        Some(n) if n > 200 => {
            return Err(RuntimeError::InvalidInput(format!(
                "agenda: `limit` must be between 1 and 200 (inclusive); got {n}"
            )));
        }
        Some(n) => n,
    };

    // Parse from/to bounds as instants so comparison is correct regardless of
    // timezone offset or DST. Reject non-RFC-3339 filter values (H1).
    let from_instant: Option<DateTime<Utc>> = match p.from {
        Some(ref s) => {
            let ts = s.parse::<DateTime<Utc>>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "agenda.from: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), \
                     got {s:?}"
                ))
            })?;
            Some(ts)
        }
        None => None,
    };
    let to_instant: Option<DateTime<Utc>> = match p.to {
        Some(ref s) => {
            let ts = s.parse::<DateTime<Utc>>().map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "agenda.to: must be an RFC 3339 timestamp (e.g. \"2027-01-01T00:00:00Z\"), \
                     got {s:?}"
                ))
            })?;
            Some(ts)
        }
        None => None,
    };

    // Push kind + status filter into SQL so SQLite can use idx_schedule_trigger
    // (declared in lib.rs on json_extract(properties,'$.trigger_at')).
    // The RFC3339 from/to window comparison and the Rust sort by parsed DateTime<Utc>
    // are kept in Rust to preserve timezone-correct ordering and handle corrupt legacy rows.
    let store = runtime.notes(token)?;
    let namespace = token.namespace().as_str();
    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
        ..Default::default()
    };

    const PAGE_SIZE: u32 = 200;
    // Use u64 for offset so it cannot overflow for very large stores (SCH-AUD-006).
    let mut offset: u64 = 0;
    // Bounded top-k: keep only the `limit` earliest events while scanning
    // so we avoid full allocation + sort of an unbounded set (SCH-AUD-004).
    // BinaryHeap requires Ord on the element; serde_json::Value does not
    // implement Ord, so we maintain a max-heap over just the timestamp and
    // pair it with a separate Vec for the serialized payloads.
    use std::collections::BinaryHeap;
    // Max-heap over timestamps: the root is always the latest (worst) entry.
    let mut ts_heap: BinaryHeap<DateTime<Utc>> = BinaryHeap::new();
    // Parallel vec of serialized events, kept in the same insertion order.
    // After scanning we zip ts_heap (drained) with this vec and sort.
    let mut ts_vec: Vec<DateTime<Utc>> = Vec::new();
    let mut ev_vec: Vec<Value> = Vec::new();

    loop {
        let page = store
            .query_notes_filtered(
                namespace,
                &filter,
                PageRequest {
                    limit: PAGE_SIZE,
                    offset,
                },
            )
            .await?;
        let page_len = page.items.len() as u32;

        for n in &page.items {
            // Parse trigger_at as an instant. Skip rows with unparseable
            // trigger_at — these are legacy corrupt rows (H1, H2).
            let trigger_at_str = n
                .properties
                .as_ref()
                .and_then(|p| p.get("trigger_at"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let instant = match trigger_at_str.parse::<DateTime<Utc>>() {
                Ok(ts) => ts,
                Err(_) => continue,
            };

            // Apply from/to window using parsed instants (H1).
            if let Some(from) = from_instant {
                if instant < from {
                    continue;
                }
            }
            if let Some(to) = to_instant {
                if instant > to {
                    continue;
                }
            }

            // Maintain bounded top-k (SCH-AUD-004):
            // if we already have `limit` items and this one is not earlier
            // than the current worst (maximum), skip it entirely.
            if ts_heap.len() < limit as usize {
                ts_heap.push(instant);
                ts_vec.push(instant);
                ev_vec.push(note_to_event_json(n));
            } else if let Some(&max_ts) = ts_heap.peek() {
                if instant < max_ts {
                    // Evict the worst entry and insert the better one.
                    // We need to remove max_ts from ts_vec/ev_vec too; find
                    // its last occurrence (insertion order, LIFO for ties).
                    ts_heap.pop();
                    if let Some(pos) = ts_vec.iter().rposition(|t| *t == max_ts) {
                        ts_vec.remove(pos);
                        ev_vec.remove(pos);
                    }
                    ts_heap.push(instant);
                    ts_vec.push(instant);
                    ev_vec.push(note_to_event_json(n));
                }
            }
        }

        // Stop when the storage page is exhausted.
        if page_len < PAGE_SIZE {
            break;
        }
        // Checked addition — extremely unlikely to overflow u64 for personal
        // schedule data, but the standard coding policy requires it (SCH-AUD-006).
        offset = offset
            .checked_add(u64::from(PAGE_SIZE))
            .ok_or_else(|| RuntimeError::Internal("agenda: pagination offset overflow".into()))?;
    }

    // Sort ascending by parsed timestamp (sort only the selected ≤ limit items).
    let mut selected: Vec<(DateTime<Utc>, Value)> = ts_vec.into_iter().zip(ev_vec).collect();
    selected.sort_by_key(|(ts, _)| *ts);

    let events: Vec<Value> = selected.into_iter().map(|(_, v)| v).collect();
    let count = events.len();

    Ok(json!({ "events": events, "count": count }))
}

/// `cancel` — cancel a scheduled event.
pub(crate) async fn handle_cancel(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: CancelParams = deser(params)?;
    let id = resolve_id(runtime, token, &p.id, "cancel").await?;

    let store = runtime.notes(token)?;
    let note = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("cancel: event {id} not found")))?;

    if note.namespace != token.namespace().as_str() {
        return Err(RuntimeError::NotFound(format!(
            "cancel: event {id} not found"
        )));
    }
    if note.kind != "scheduled_event" {
        return Err(RuntimeError::InvalidInput(format!(
            "cancel: note {id} is kind {:?}, expected \"scheduled_event\"",
            note.kind
        )));
    }

    // Require properties to be a JSON object (or absent — treated as `{}`).
    // Mutable string-key indexing on a non-object value panics in serde_json;
    // reject corrupt notes here with a clear error instead (SCH-AUD-001).
    let raw_props = note.properties.clone().unwrap_or_else(|| json!({}));
    let props = match raw_props {
        Value::Object(_) => raw_props,
        ref other => {
            let type_name = match other {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => unreachable!(),
            };
            return Err(RuntimeError::InvalidInput(format!(
                "cancel: event {id} has malformed properties (expected JSON object, got \
                 {type_name}); cannot mutate"
            )));
        }
    };
    // Scheduled events are a state machine: cancel only transitions
    // pending -> cancelled. Any other current status (already "cancelled",
    // "fired" by the pending-events runner, or anything else) is rejected
    // outright rather than unconditionally overwritten (issue #462).
    let status = props
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string);
    if status.as_deref() != Some("pending") {
        return Err(RuntimeError::InvalidInput(format!(
            "cancel: event {id} is not pending (current status: {status:?}); \
             only pending events can be cancelled"
        )));
    }

    let cancelled_at = Utc::now().to_rfc3339();
    // Persist the transition with a conditional (CAS) update on
    // (id, namespace, kind, current status) instead of a full-row
    // `upsert_note`/`INSERT OR REPLACE`. This closes the race where a
    // pending-events dispatch fires the event (writing status="fired" and
    // fired_at) between our read above and the write below: the CAS only
    // succeeds if the row is still "pending" at write time, so a concurrent
    // fire can never be clobbered by a stale cancel (issue #462).
    let updated = cancel_pending_event(runtime, token.namespace().as_str(), id, &cancelled_at)
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: conditional update: {e}")))?;
    if !updated {
        return Err(RuntimeError::InvalidInput(format!(
            "cancel: event {id} is no longer pending; it was cancelled or fired concurrently"
        )));
    }

    let note = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("cancel: event {id} not found")))?;
    let props = note.properties.unwrap_or_else(|| json!({}));

    Ok(json!({
        "id": short_id(id),
        "full_id": id.as_hyphenated().to_string(),
        "status": "cancelled",
        "cancelled_at": cancelled_at,
        "properties": props,
    }))
}

/// Conditionally transition a `scheduled_event` note from `pending` to
/// `cancelled`, returning `true` iff the transition was applied.
///
/// Uses a `json_set`-on-`properties` UPDATE gated by
/// `json_extract(properties,'$.status') = 'pending'` so the write only lands
/// if the row is still pending at the moment the statement executes — a
/// concurrent fire (or a second cancel) that already changed the status
/// causes this to affect zero rows instead of overwriting the newer state.
async fn cancel_pending_event(
    runtime: &KhiveRuntime,
    namespace: &str,
    id: Uuid,
    cancelled_at: &str,
) -> Result<bool, RuntimeError> {
    let updated_at = Utc::now().timestamp_micros();
    let mut writer = runtime
        .sql()
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: open SQL writer: {e}")))?;

    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set(COALESCE(properties, '{}'), \
                      '$.status', 'cancelled', '$.cancelled_at', ?1), \
                      updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending'"
                .to_string(),
            params: vec![
                SqlValue::Text(cancelled_at.to_string()),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("schedule_cancel_pending".into()),
        })
        .await
        .map_err(|e| RuntimeError::Internal(format!("cancel: conditional update: {e}")))?;

    Ok(rows == 1)
}
