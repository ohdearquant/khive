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

/// Validate that `at` is a valid RFC 3339 timestamp and lies in the future.
///
/// Accepts any RFC 3339 string that `chrono` can parse as a `DateTime<Utc>`
/// (e.g. "2027-01-01T00:00:00Z" or "2027-01-01T00:00:00+05:30").
///
/// Returns the parsed UTC instant so callers can use it for comparisons
/// without re-parsing. The original string is preserved by callers who want
/// to store it as-is (see H5 fix below).
///
/// Rejects:
/// - Unparseable strings (not RFC 3339).
/// - Timestamps that lie in the past relative to `Utc::now()`.
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

/// Validate a repeat spec: named aliases or a limited five-field form.
///
/// Accepts the literals `daily`, `weekly`, `monthly`, and a limited five-field
/// form `MIN HOUR DOM MON DOW`, where each field is exactly `*` or one
/// non-negative integer within the accepted range:
/// - MIN  0–59
/// - HOUR 0–23
/// - DOM  1–31
/// - MON  1–12
/// - DOW  0–7
///
/// Standard cron operators such as steps (`*/15`), ranges (`9-17`), and lists
/// (`0,30`) are NOT accepted (issue #481): `kkernel`'s pending-events runner
/// does not yet compute next-fire times for cron-form repeats (it fires them
/// one-shot), so advertising and accepting full cron syntax here would imply
/// recurrence semantics that do not exist yet. Use `daily` / `weekly` /
/// `monthly` for recurring runtime advancement until cron next-fire support
/// lands.
///
/// Malformed fields (non-numeric, out-of-range, or a cron operator) are
/// rejected with `RuntimeError::InvalidInput` rather than silently accepted.
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

/// Validate that `action` is parseable DSL via `khive_request::parse_request`.
///
/// This catches garbage like `"x"` or `"bogus-not-a-valid-verb()"` at write
/// time rather than at trigger time, when nobody is watching. Returns the
/// parsed request so callers can inspect the verb names without re-parsing.
fn validate_action(action: &str) -> Result<khive_request::ParsedRequest, RuntimeError> {
    khive_request::parse_request(action).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "schedule.action: invalid DSL ({e}); \
             provide a valid verb call (e.g. \"schedule.remind(content=\\\"hello\\\")\")"
        ))
    })
}

/// Validate that a parsed `schedule.schedule` action can be replayed exactly
/// as stored (issue #461).
///
/// The pending-events runner reparses the stored DSL at trigger time and
/// dispatches it through the normal request surface. For that replay to
/// succeed it must be a single op against an exactly-registered handler name
/// (not a bare shorthand resolved via a `schedule.{tool}` fallback), with
/// only literal argument values (no `$prev` references, which are only
/// meaningful inside a chain the replay path does not reconstruct) and all
/// required handler parameters present. Rejecting anything else here, at
/// write time, prevents storing an action that is guaranteed to fail (and be
/// silently marked "fired") when it comes due.
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

    // Reject any handler whose schema declares `namespace` as a business
    // param (issue #461/#462). `dispatch_action` in `pending_events.rs`
    // unconditionally injects the firing event's routing namespace into
    // every op's args, and the registry passes it through unchanged
    // whenever the handler declares `namespace` (`khive-runtime/src/pack.rs`).
    // For handlers that treat `namespace` as a business param (e.g.
    // `brain.bind`, `brain.resolve`), that silently changes the business
    // value on replay — even when the *stored* action omitted `namespace`
    // entirely (e.g. `brain.bind` defaults an omitted `namespace` to the
    // wildcard `"*"` at write time; replay would instead bind it to
    // whatever namespace the event happens to fire from). Replay cannot yet
    // carry routing-namespace and arg-namespace as separate concepts, so
    // reject at write time based on the handler's schema alone, regardless
    // of whether the stored args happen to include `namespace`.
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

/// Reject scheduled actions known to fail a handler's *conditional* required
/// param even though `describe_verb` marks none of the alternatives
/// `required:true` (issue #461).
///
/// `validate_args_against_help` only enforces metadata-declared
/// `required:true` params. Some handlers accept one of several alternative
/// arg sets (e.g. `create` requires `kind` unless bulk `items` is given), so
/// neither alternative is marked required in metadata and both can be
/// omitted at write time — then fail at trigger-time replay. This function
/// hard-codes the known cases; it is not a general conditional-requirements
/// mechanism (there is no metadata surface for that yet), so it does not
/// guarantee every handler-internal semantic precondition is caught.
///
/// For `tool == "create"`, this mirrors the singleton branches of the KG
/// pack's own `handle_create` (`khive-pack-kg/src/handlers/create.rs`):
/// entity/granular-entity creates require `name`, note/granular-note creates
/// require `content`, and a bare `kind="entity"` requires an `entity_kind`
/// (or a granular entity kind) to resolve a concrete kind. It also validates
/// `entity_type` against the KG entity-type/subtype registry when present
/// (round-3 review gap 1) — see `validate_entity_type_for_replay`.
/// `khive-pack-schedule` does not depend on `khive-pack-kg` (only as a
/// dev-dependency for tests), so this reimplements the classification using
/// `VerbRegistry::all_entity_kinds` / `all_note_kinds` — the same data
/// `resolve_kind_spec` consults — rather than importing the KG pack's
/// private helpers.
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
            validate_entity_type_for_replay(&canonical, entity_type_arg).map_err(|e| {
                RuntimeError::InvalidInput(format!("schedule.action: verb \"create\": {e}"))
            })?;
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

/// Classify a `create(kind=...)` value the same way
/// `khive-pack-kg::handlers::common::resolve_kind_spec` does: literal
/// substrate keywords first, then base-8-kind aliases (`khive_types::EntityKind`,
/// e.g. `"paper"` -> `document` — a real, non-dev dependency already shared
/// with khive-pack-kg, so this is genuine reuse, not a hand-copy), then the
/// pack-local `resource`-kind alias set (`"atom"`, `"runbook"`, etc. ->
/// `resource`, ADR-048; hand-copied via `resource_alias_for_replay` since
/// `khive-pack-kg::vocab::EntityKind` is pack-private — see that function's
/// doc comment), then the registry's merged entity/note-kind vocabulary (the
/// same final fallback `resolve_kind_spec` uses). Returns an error for any
/// `kind` that is guaranteed to fail replay outright: `edge` (create edges
/// via `link`), `event` (immutable), `proposal` (create via `propose`), or
/// an unrecognized kind string.
///
/// Round-3 review (gap 2) found the pre-fix version skipped alias resolution
/// entirely, causing schedule-time false rejections (not a security hole)
/// for legitimate KG-accepted spellings like `"paper"` and `"atom"`.
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

/// Canonicalize a legacy `entity_kind` value the same way
/// `khive-pack-kg::handlers::common::canonical_entity_kind` does: try the
/// base `khive_types::EntityKind` parser (the 8 base kinds plus its common
/// aliases, e.g. `"paper"` -> `document`; a real, non-dev dependency already
/// shared with khive-pack-kg — genuine reuse, not a hand-copy), then the
/// pack-local `resource`-kind alias set (`"atom"`, `"runbook"`, etc. ->
/// `resource`, ADR-048; hand-copied via `resource_alias_for_replay` since
/// `khive-pack-kg::vocab::EntityKind` is pack-private and
/// `khive-pack-schedule` does not depend on `khive-pack-kg` in production —
/// dev-dependency only, for tests), then fall back to the registry's merged
/// entity-kind vocabulary (covers any further pack-declared additions).
///
/// Round-3 review (gap 2) found the pre-fix version resolved neither alias
/// set, causing schedule-time false rejections (not a security hole) for
/// legitimate KG-accepted spellings like `"paper"` and `"atom"`.
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

/// Canonicalize a legacy `note_kind` value the same way
/// `khive-pack-kg::handlers::common::canonical_note_kind` does. Note kinds
/// carry no alias set beyond their 5 canonical names (ADR-013), so this is
/// exactly the registry's merged note-kind vocabulary check.
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

/// Pack-local `resource`-kind aliases (ADR-048), mirroring
/// `khive-pack-kg::vocab::EntityKind`'s `FromStr` arm `"resource" | "atom" |
/// "runbook" | "template" | "prompt" | "skill" | "tool"`
/// (`khive-pack-kg/src/vocab.rs`). That type is `pub(crate)` to
/// `khive-pack-kg` and `khive-pack-schedule` does not depend on
/// `khive-pack-kg` in production (dev-dependency only, for tests), so this
/// hand-copies just the alias set — six short strings — rather than the
/// type. `normalized` must already be trimmed + lowercased (callers already
/// compute this for the registry-vocabulary fallback). Kept in sync by
/// `entity_kind_resource_aliases_match_real_vocab` in `create_validation.rs`,
/// which asserts this list against the live `khive-pack-kg` vocab (via the
/// dev-dependency) so drift is caught in CI rather than silently reproducing
/// a round-3-style false rejection.
fn resource_alias_for_replay(normalized: &str) -> bool {
    matches!(
        normalized,
        "resource" | "atom" | "runbook" | "template" | "prompt" | "skill" | "tool"
    )
}

/// Mirror of `khive-pack-kg::entity_type_registry::BUILTIN_DEFS` — one row
/// per `(kind, canonical type_name, aliases)` triple, used only to reject at
/// schedule-write-time any `entity_type` that the real KG `create` handler's
/// `validate_entity_type` (`khive-pack-kg/src/handlers/common.rs`) would
/// reject at trigger-time replay (round-3 review gap 1). Only the base 8
/// `khive_types::EntityKind` kinds ever reach this table — see
/// `validate_entity_type_for_replay` for why `resource` (and any other
/// pack-owned kind) short-circuits before a subtype is even considered,
/// mirroring the real handler exactly.
///
/// `khive-pack-schedule` does not depend on `khive-pack-kg` in production
/// (dev-dependency only, for tests), so this hand-copies the definitions
/// rather than importing `EntityTypeRegistry` directly. Keep in sync with
/// `BUILTIN_DEFS`:
/// `gap1_entity_type_exhaustive_parity_with_real_registry` in
/// `create_validation.rs` compares every entry here against the live
/// registry (via the dev-dependency, `khive_pack_kg::EntityTypeRegistry::global()`)
/// and fails if the two tables diverge, so future additions to the real
/// table that aren't mirrored here fail in CI instead of drifting silently.
const ENTITY_TYPE_DEFS_FOR_REPLAY: &[(khive_types::EntityKind, &str, &[&str])] = &[
    // ── Document ──────────────────────────────────────────────────────────
    (
        khive_types::EntityKind::Document,
        "paper",
        &["preprint", "article"],
    ),
    (khive_types::EntityKind::Document, "report", &[]),
    (khive_types::EntityKind::Document, "blog_post", &["blog"]),
    (khive_types::EntityKind::Document, "book", &[]),
    (
        khive_types::EntityKind::Document,
        "specification",
        &["spec"],
    ),
    (
        khive_types::EntityKind::Document,
        "documentation",
        &["docs"],
    ),
    (khive_types::EntityKind::Document, "thesis", &[]),
    // ── Concept ───────────────────────────────────────────────────────────
    (khive_types::EntityKind::Concept, "algorithm", &["algo"]),
    (
        khive_types::EntityKind::Concept,
        "theorem",
        &["lemma", "proposition", "corollary"],
    ),
    (khive_types::EntityKind::Concept, "definition", &["def"]),
    (
        khive_types::EntityKind::Concept,
        "structure",
        &["inductive", "struct", "class"],
    ),
    (khive_types::EntityKind::Concept, "instance", &[]),
    (khive_types::EntityKind::Concept, "axiom", &[]),
    (khive_types::EntityKind::Concept, "goal", &["proof_goal"]),
    (khive_types::EntityKind::Concept, "technique", &[]),
    (khive_types::EntityKind::Concept, "architecture", &["arch"]),
    (khive_types::EntityKind::Concept, "model_family", &["model"]),
    (khive_types::EntityKind::Concept, "theory", &[]),
    (khive_types::EntityKind::Concept, "research_gap", &["gap"]),
    (
        khive_types::EntityKind::Concept,
        "design_pattern",
        &["pattern"],
    ),
    (
        khive_types::EntityKind::Concept,
        "mathematical_operation",
        &["math_op"],
    ),
    (khive_types::EntityKind::Concept, "metric", &[]),
    (khive_types::EntityKind::Concept, "objective", &["loss"]),
    // ── Dataset ───────────────────────────────────────────────────────────
    (khive_types::EntityKind::Dataset, "benchmark", &[]),
    (khive_types::EntityKind::Dataset, "corpus", &[]),
    (
        khive_types::EntityKind::Dataset,
        "training_set",
        &["train_set"],
    ),
    (
        khive_types::EntityKind::Dataset,
        "evaluation_set",
        &["eval_set"],
    ),
    (khive_types::EntityKind::Dataset, "test_set", &[]),
    (
        khive_types::EntityKind::Dataset,
        "synthetic_dataset",
        &["synthetic"],
    ),
    // ── Project ───────────────────────────────────────────────────────────
    (khive_types::EntityKind::Project, "library", &["lib"]),
    (khive_types::EntityKind::Project, "framework", &[]),
    (khive_types::EntityKind::Project, "tool", &[]),
    (khive_types::EntityKind::Project, "application", &["app"]),
    (khive_types::EntityKind::Project, "repository", &["repo"]),
    // ── Org ───────────────────────────────────────────────────────────────
    (
        khive_types::EntityKind::Org,
        "academic_institution",
        &["university", "uni"],
    ),
    (khive_types::EntityKind::Org, "company", &[]),
    (khive_types::EntityKind::Org, "research_lab", &["lab"]),
    (khive_types::EntityKind::Org, "nonprofit", &[]),
    (
        khive_types::EntityKind::Org,
        "government_agency",
        &["gov_agency"],
    ),
    (khive_types::EntityKind::Org, "consortium", &[]),
    (khive_types::EntityKind::Org, "standards_body", &[]),
    // ── Artifact ──────────────────────────────────────────────────────────
    (khive_types::EntityKind::Artifact, "checkpoint", &["ckpt"]),
    (khive_types::EntityKind::Artifact, "snapshot", &[]),
    (khive_types::EntityKind::Artifact, "export", &[]),
    (
        khive_types::EntityKind::Artifact,
        "embedding_index",
        &["embed_index"],
    ),
    (khive_types::EntityKind::Artifact, "state_bundle", &[]),
    (khive_types::EntityKind::Artifact, "profile", &[]),
    // ── Service ───────────────────────────────────────────────────────────
    (khive_types::EntityKind::Service, "inference_engine", &[]),
    (khive_types::EntityKind::Service, "retrieval_engine", &[]),
    (khive_types::EntityKind::Service, "embedding_engine", &[]),
    (khive_types::EntityKind::Service, "api", &["endpoint"]),
    (khive_types::EntityKind::Service, "database", &["db"]),
    (khive_types::EntityKind::Service, "search_engine", &[]),
    (khive_types::EntityKind::Service, "mcp_server", &["mcp"]),
    // Person — no standard subtypes (roles are metadata, not subtypes).
];

/// Normalize a raw `entity_type` string to canonical snake_case, mirroring
/// `khive-pack-kg::entity_type_registry::to_snake_case` exactly (ADR-001:106
/// write-time normalization step that precedes alias resolution): trim ->
/// lowercase -> runs of separators (space, hyphen, underscore) collapsed to
/// a single `_` -> leading/trailing `_` stripped.
fn to_snake_case_for_replay(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = true;
    for ch in s.chars() {
        if ch == ' ' || ch == '-' || ch == '_' {
            if !prev_sep && !out.is_empty() {
                out.push('_');
                prev_sep = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    if out.ends_with('_') {
        out.pop();
    }
    out
}

/// Comma-separated list of canonical type names valid for `kind`, mirroring
/// `EntityTypeRegistry::valid_types_for` for error messages.
fn valid_entity_types_for_replay(kind: khive_types::EntityKind) -> String {
    let mut names: Vec<&str> = ENTITY_TYPE_DEFS_FOR_REPLAY
        .iter()
        .filter(|(k, _, _)| *k == kind)
        .map(|(_, type_name, _)| *type_name)
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        "(none registered)".to_string()
    } else {
        names.join(" | ")
    }
}

/// Resolve a `(kind, entity_type)` pair against `ENTITY_TYPE_DEFS_FOR_REPLAY`,
/// mirroring `EntityTypeRegistry::resolve` exactly: kind-qualified lookup
/// first (a name or alias registered under this specific kind), then a bare
/// lookup across all kinds — if the name/alias exists under a *different*
/// kind, the cross-kind rejection error names the correct kind (matching
/// the real registry's ambiguity-safe bare-name path); otherwise the type is
/// entirely unknown.
fn resolve_entity_type_for_replay(
    kind: khive_types::EntityKind,
    raw_type: &str,
) -> Result<Option<String>, RuntimeError> {
    let normalized = to_snake_case_for_replay(raw_type.trim());

    for (def_kind, type_name, aliases) in ENTITY_TYPE_DEFS_FOR_REPLAY.iter().copied() {
        if def_kind == kind
            && (type_name == normalized.as_str() || aliases.contains(&normalized.as_str()))
        {
            return Ok(Some(type_name.to_string()));
        }
    }

    for (def_kind, type_name, aliases) in ENTITY_TYPE_DEFS_FOR_REPLAY.iter().copied() {
        if type_name == normalized.as_str() || aliases.contains(&normalized.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "entity_type {raw_type:?} belongs to {:?}, not {:?}; valid types for {:?}: {}",
                def_kind.name(),
                kind.name(),
                kind.name(),
                valid_entity_types_for_replay(kind),
            )));
        }
    }

    Err(RuntimeError::InvalidInput(format!(
        "unknown entity_type {raw_type:?} for {:?}; valid: {}",
        kind.name(),
        valid_entity_types_for_replay(kind),
    )))
}

/// Validate an `entity_type` value the same way
/// `khive-pack-kg::handlers::common::validate_entity_type` does: parse
/// `canonical_kind_name` into the base `khive_types::EntityKind` first, then
/// resolve the subtype against `ENTITY_TYPE_DEFS_FOR_REPLAY`.
///
/// The kind-parse step is exactly what makes a pack-owned kind like
/// `"resource"` reject *any* non-`None` `entity_type` outright: `resource`
/// has no variant in the base 8-kind enum, so parsing `canonical_kind_name`
/// fails before the subtype table is even consulted — the real handler has
/// this same short-circuit (`khive-pack-kg/src/handlers/common.rs`,
/// `validate_entity_type`), verified live via `kkernel exec` against a
/// scratch DB. This mirrors that behavior rather than "fixing" it: the
/// contract here is bit-for-bit replay parity with the real handler, not
/// what the real handler arguably should do.
fn validate_entity_type_for_replay(
    canonical_kind_name: &str,
    entity_type: Option<&str>,
) -> Result<Option<String>, RuntimeError> {
    let Some(raw) = entity_type else {
        return Ok(None);
    };
    let kind = canonical_kind_name
        .parse::<khive_types::EntityKind>()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!("unknown entity kind {canonical_kind_name:?}"))
        })?;
    resolve_entity_type_for_replay(kind, raw)
}

/// Reconcile a granular `kind`'s resolved `specific` value against a legacy
/// `entity_kind`/`note_kind` argument, mirroring
/// `khive-pack-kg::handlers::common::reconcile_specific` exactly (including
/// the contradiction error shape) so a scheduled action that the real KG
/// `create` handler would reject for a kind/legacy-kind contradiction is
/// rejected at schedule time too, not only discovered at trigger-time replay.
/// `context` prefixes error messages (e.g. `"items[3] "` for a bulk entry,
/// `""` for the singleton path).
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
                validate_entity_type_for_replay(&canonical, entry.entity_type.as_deref()).map_err(
                    |e| {
                        RuntimeError::InvalidInput(format!(
                            "schedule.action: verb \"create\": items[{idx}] {e}"
                        ))
                    },
                )?;
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
    params: Value,
) -> Result<Value, RuntimeError> {
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
