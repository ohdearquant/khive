//! Shared types and helper functions for KG verb handlers.
//!
//! Param structs (deserialization types) live in `super::params` and are
//! re-exported here so existing `use super::common::*` imports keep working.

use std::str::FromStr;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    micros_to_iso, ContentMergeStrategy, EntityDedupMergePolicy, KhiveRuntime, NamespaceToken,
    QueryResult, RuntimeError, VerbRegistry,
};
use khive_storage::types::{Direction, SqlValue};
use khive_storage::{EdgeRelation, EntityFilter, EventFilter, EventOutcome, SubstrateKind};

use khive_types::{EntityKind, EventKind};

use crate::entity_type_registry::EntityTypeRegistry;
use crate::vocab::NoteKind;

pub(crate) use super::params::{
    ContextParams, CreateParams, DeleteParams, GetParams, LinkParams, ListParams,
    ListProposalsParams, MergeParams, NeighborsParams, ProposeParams, QueryParams, ReviewParams,
    SearchParams, StatsParams, TraverseParams, UpdateParams, WithdrawParams, HARD_CAP,
};

// ---- Kind canonicalization ----

pub(crate) fn canonical_entity_kind(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<String, RuntimeError> {
    if let Ok(k) = EntityKind::from_str(raw) {
        return Ok(k.name().to_string());
    }
    if let Ok(k) = crate::vocab::EntityKind::from_str(raw) {
        return Ok(k.name().to_string());
    }
    let normalized = raw.trim().to_ascii_lowercase();
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

pub(crate) fn canonical_note_kind(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<String, RuntimeError> {
    if let Ok(k) = NoteKind::from_str(raw) {
        return Ok(k.name().to_string());
    }
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

// ---- Entity-type validation ----

pub(crate) fn validate_entity_type(
    kind_name: &str,
    entity_type: Option<&str>,
) -> Result<Option<String>, RuntimeError> {
    let Some(raw) = entity_type else {
        return Ok(None);
    };
    let kind = kind_name
        .parse::<khive_types::EntityKind>()
        .map_err(|_| RuntimeError::InvalidInput(format!("unknown entity kind {kind_name:?}")))?;
    let resolved = EntityTypeRegistry::global().resolve(kind, Some(raw))?;
    Ok(resolved.entity_type)
}

// ---- Granular `kind` discriminator ----

/// Resolved shape of a `kind` discriminator string.
///
/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 1): the
/// `--atomic` seam in `kkernel` reuses [`resolve_kind_spec`] and this type to
/// resolve a caller-supplied `delete(kind=...)` the SAME way `handle_delete`
/// does, rather than re-deriving kind-vocabulary resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindSpec {
    Entity { specific: Option<String> },
    Note { specific: Option<String> },
    Edge,
    Event,
    Proposal,
}

impl KindSpec {
    pub(crate) fn substrate_label(&self) -> &'static str {
        match self {
            KindSpec::Entity { .. } => "entity",
            KindSpec::Note { .. } => "note",
            KindSpec::Edge => "edge",
            KindSpec::Event => "event",
            KindSpec::Proposal => "proposal",
        }
    }
}

/// Resolve a wire-level `kind` value into a [`KindSpec`].
///
/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 1) —
/// see [`KindSpec`]'s doc comment.
pub fn resolve_kind_spec(raw: &str, registry: &VerbRegistry) -> Result<KindSpec, RuntimeError> {
    let normalized = raw.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "entity" => return Ok(KindSpec::Entity { specific: None }),
        "note" => return Ok(KindSpec::Note { specific: None }),
        "edge" => return Ok(KindSpec::Edge),
        "event" => return Ok(KindSpec::Event),
        "proposal" => return Ok(KindSpec::Proposal),
        _ => {}
    }

    if let Ok(k) = EntityKind::from_str(raw) {
        return Ok(KindSpec::Entity {
            specific: Some(k.name().to_string()),
        });
    }
    if let Ok(k) = crate::vocab::EntityKind::from_str(raw) {
        return Ok(KindSpec::Entity {
            specific: Some(k.name().to_string()),
        });
    }
    if let Ok(k) = NoteKind::from_str(raw) {
        return Ok(KindSpec::Note {
            specific: Some(k.name().to_string()),
        });
    }

    if registry.all_entity_kinds().contains(&normalized.as_str()) {
        return Ok(KindSpec::Entity {
            specific: Some(normalized),
        });
    }
    if registry.all_note_kinds().contains(&normalized.as_str()) {
        return Ok(KindSpec::Note {
            specific: Some(normalized),
        });
    }

    let mut all: Vec<String> = vec![
        "entity".into(),
        "note".into(),
        "edge".into(),
        "event".into(),
        "proposal".into(),
    ];
    all.extend(registry.all_entity_kinds().iter().map(|s| (*s).to_string()));
    all.extend(registry.all_note_kinds().iter().map(|s| (*s).to_string()));
    all.sort();
    all.dedup();
    Err(RuntimeError::InvalidInput(format!(
        "unknown kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

/// Reconcile a granular `kind` with a legacy `entity_kind`/`note_kind` subfield.
pub(crate) fn reconcile_specific(
    spec_specific: Option<String>,
    legacy_raw: Option<&str>,
    canonicalize: impl Fn(&str) -> Result<String, RuntimeError>,
    legacy_field: &str,
) -> Result<Option<String>, RuntimeError> {
    let legacy_canonical = match legacy_raw {
        Some(s) => Some(canonicalize(s)?),
        None => None,
    };
    match (spec_specific, legacy_canonical) {
        (Some(a), Some(b)) if a != b => Err(RuntimeError::InvalidInput(format!(
            "kind={a:?} contradicts {legacy_field}={b:?}; pick one"
        ))),
        (Some(a), _) => Ok(Some(a)),
        (None, b) => Ok(b),
    }
}

// ---- Helpers ----

async fn resolve_name_async(
    name: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    let filter = EntityFilter {
        name_prefix: Some(name.to_string()),
        ..Default::default()
    };
    let page = runtime
        .entities(token)?
        .query_entities(
            token.namespace().as_str(),
            filter,
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .map_err(RuntimeError::Storage)?;

    let name_lower = name.to_ascii_lowercase();
    let exact: Vec<_> = page
        .items
        .into_iter()
        .filter(|e| e.name.to_ascii_lowercase() == name_lower && e.deleted_at.is_none())
        .collect();

    match exact.len() {
        0 => Err(RuntimeError::NotFound(format!(
            "entity not found: {name:?}"
        ))),
        1 => Ok(exact[0].id),
        n => {
            let ids: Vec<String> = exact
                .iter()
                .map(|e| e.id.to_string()[..8].to_string())
                .collect();
            Err(RuntimeError::Ambiguous(format!(
                "ambiguous name {name:?}: found {n} entities [{}]",
                ids.join(", ")
            )))
        }
    }
}

pub(crate) async fn resolve_uuid_async(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix(token, s).await {
            Ok(Some(uuid)) => return Ok(uuid),
            Ok(None) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    resolve_name_async(s, runtime, token).await
}

/// By-ID contract (ADR-007 Rev 6): UUID resolution for get/update/delete/merge
/// is namespace-agnostic — the Gate is the authz seam, not storage-layer
/// filtering. Full-UUID inputs were already unfiltered (`resolve_by_id`); this
/// closes the gap for the *prefix* form, which previously fell through to the
/// primary-namespace-only `resolve_prefix` and was invisible for any row
/// stamped with a non-primary namespace (#391 §3). Exact copy of
/// `resolve_uuid_async` except the prefix branch.
///
/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 3) — so
/// kkernel's `--atomic` seam can resolve KG ids (full UUID / 8+ hex prefix /
/// entity-name) with the exact same semantics as the canonical handlers.
pub async fn resolve_uuid_unfiltered(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix_unfiltered(s).await {
            Ok(Some(uuid)) => return Ok(uuid),
            Ok(None) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    resolve_name_async(s, runtime, token).await
}

/// `resolve_uuid_unfiltered`, including soft-deleted rows — used by the
/// hard-delete by-ID path (#391 §3). Exact copy of
/// `resolve_uuid_including_deleted` except the prefix branch.
///
/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 3) — for
/// the same reason as `resolve_uuid_unfiltered` above; used by the atomic
/// hard-delete id-resolution path.
pub async fn resolve_uuid_unfiltered_including_deleted(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix_unfiltered_including_deleted(s).await {
            Ok(Some(uuid)) => return Ok(uuid),
            Ok(None) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    resolve_name_async(s, runtime, token).await
}

// ---- Output formatting helpers ----

pub(crate) fn format_edge_output(v: Value, _verbose: bool) -> Value {
    v
}

pub(crate) fn flatten_get_result(substrate: &str, mut inner: Value) -> Result<Value, RuntimeError> {
    if let Some(obj) = inner.as_object_mut() {
        match substrate {
            "edge" => {
                obj.entry("kind".to_string())
                    .or_insert_with(|| serde_json::Value::String("edge".to_string()));
            }
            "event" => {
                if let Some(event_kind) = obj.remove("kind") {
                    obj.insert("event_kind".to_string(), event_kind);
                }
                obj.insert(
                    "kind".to_string(),
                    serde_json::Value::String("event".to_string()),
                );
            }
            _ => {}
        }
        Ok(inner)
    } else {
        Ok(serde_json::json!({"kind": substrate, "data": inner}))
    }
}

pub(crate) fn remap_note_status(mut note_value: Value) -> Value {
    let Some(obj) = note_value.as_object_mut() else {
        return note_value;
    };
    let lifecycle_status = obj
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    if let Some(gtd_status) = lifecycle_status {
        if let Some(row_vis) = obj.remove("status") {
            obj.insert("lifecycle".to_string(), row_vis);
        }
        obj.insert("status".to_string(), Value::String(gtd_status));
    }
    note_value
}

pub(crate) fn parse_direction(s: Option<&str>) -> Result<Direction, RuntimeError> {
    match s {
        Some("in") | Some("incoming") => Ok(Direction::In),
        Some("both") | None => Ok(Direction::Both),
        Some("out") | Some("outgoing") => Ok(Direction::Out),
        Some(raw) => Err(RuntimeError::InvalidInput(format!(
            "unknown direction {raw:?}; valid: out | outgoing | in | incoming | both"
        ))),
    }
}

pub(crate) fn parse_relation(s: &str) -> Result<EdgeRelation, RuntimeError> {
    s.parse::<EdgeRelation>().map_err(|_| {
        let valid = EdgeRelation::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        RuntimeError::InvalidInput(format!("unknown relation {s:?}; valid: {valid}"))
    })
}

pub(crate) fn validate_weight(weight: Option<f64>) -> Result<f64, RuntimeError> {
    let w = weight.unwrap_or(1.0);
    if !w.is_finite() || !(0.0..=1.0).contains(&w) {
        return Err(RuntimeError::InvalidInput(format!(
            "edge weight must be a finite number in [0.0, 1.0], got {w}"
        )));
    }
    Ok(w)
}

/// Relations valid for a `(src_kind, src_entity_type, tgt_kind,
/// tgt_entity_type)` entity pair, derived from the SAME sources
/// `validate_edge_relation_endpoints` consults when accepting or rejecting a
/// link (issue #543): the base entity endpoint allowlist
/// (`khive_runtime::operations::base_entity_endpoint_rules`) plus the
/// runtime's live composed pack `EDGE_RULES`, matched through
/// `khive_runtime::operations::accepted_pack_relations_for_entities` — the
/// same `endpoint_matches` semantics `pack_rule_allows` applies internally
/// (`EntityOfKind`, `EntityOfType`, `NoteOfKind`). There is no separate
/// hand-authored table and no local re-filter of endpoint kinds here: a hint
/// can no longer diverge from what the validator itself accepts, including
/// pack rules scoped to a granular `entity_type` (e.g. `khive-pack-formal`'s
/// typed `theorem -> definition` `depends_on` rule).
///
/// Note-scoped pack rules (e.g. GTD's `task` -> `task` `depends_on`,
/// declared as `NoteOfKind`) cannot match here regardless of the shared
/// matcher, because this function is only ever reached (via
/// `enrich_allowlist_error`) after both endpoints have already been resolved
/// as entities — a note/note mismatch produces a different validation error
/// entirely ("must be an entity for relation ..."), not the base-allowlist
/// error this function enriches.
pub(crate) fn valid_relations_for_entity_pair(
    runtime: &KhiveRuntime,
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<&'static str> {
    let mut relations: Vec<&'static str> = khive_runtime::operations::base_entity_endpoint_rules()
        .iter()
        .filter(|(src, _rel, tgt)| (*src == "*" || *src == src_kind) && *tgt == tgt_kind)
        .map(|(_src, rel, _tgt)| rel.as_str())
        .collect();

    let pack_rules = runtime.pack_edge_rules();
    for rel in khive_runtime::operations::accepted_pack_relations_for_entities(
        &pack_rules,
        src_kind,
        src_entity_type,
        tgt_kind,
        tgt_entity_type,
    ) {
        relations.push(rel.as_str());
    }

    relations.sort_unstable();
    relations.dedup();
    relations
}

pub(crate) async fn enrich_allowlist_error(
    original: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
) -> String {
    let (src_kind, src_entity_type) = match runtime.get_entity(token, source_id).await {
        Ok(e) => (e.kind, e.entity_type),
        Err(_) => return original.to_string(),
    };
    let (tgt_kind, tgt_entity_type) = match runtime.get_entity(token, target_id).await {
        Ok(e) => (e.kind, e.entity_type),
        Err(_) => return original.to_string(),
    };
    let valid = valid_relations_for_entity_pair(
        runtime,
        &src_kind,
        src_entity_type.as_deref(),
        &tgt_kind,
        tgt_entity_type.as_deref(),
    );
    let mut msg = if valid.is_empty() {
        format!(
            "Invalid relation {:?} for {src_kind}\u{2192}{tgt_kind}. \
             No valid relations exist for {src_kind}\u{2192}{tgt_kind} in the current edge rules.",
            relation.as_str()
        )
    } else {
        format!(
            "Invalid relation {:?} for {src_kind}\u{2192}{tgt_kind}. \
             Valid relations: {}",
            relation.as_str(),
            valid.join(", ")
        )
    };
    // supports/refutes accept a kind-restricted entity target (ADR-055); the
    // generic valid-relations list alone reads as "wrong relation" when the
    // actual fix is a concept target, so name the requirement explicitly.
    if matches!(relation, EdgeRelation::Supports | EdgeRelation::Refutes) {
        msg.push_str(&format!(
            " (note: {} on entities requires a concept target: \
             concept|document|dataset|artifact\u{2192}concept)",
            relation.as_str()
        ));
    }
    msg
}

pub(crate) const IMMUTABLE_EVENT_MSG: &str =
    "events are immutable — create/update/delete are not permitted";

pub(crate) fn immutable_event_error() -> RuntimeError {
    RuntimeError::InvalidInput(IMMUTABLE_EVENT_MSG.into())
}

pub(crate) fn parse_event_outcome(raw: &str) -> Result<EventOutcome, RuntimeError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "success" => Ok(EventOutcome::Success),
        "denied" => Ok(EventOutcome::Denied),
        "error" => Ok(EventOutcome::Error),
        _ => Err(RuntimeError::InvalidInput(format!(
            "unknown outcome {raw:?}; valid: success | denied | error"
        ))),
    }
}

pub(crate) fn parse_event_substrate(raw: &str) -> Result<SubstrateKind, RuntimeError> {
    raw.trim()
        .to_ascii_lowercase()
        .parse::<SubstrateKind>()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "unknown substrate {raw:?}; valid: note | entity | event"
            ))
        })
}

pub(crate) fn parse_event_kind(raw: &str) -> Result<EventKind, RuntimeError> {
    raw.parse::<EventKind>()
        .map_err(|e| RuntimeError::InvalidInput(format!("unknown event_kind {raw:?}: {e}")))
}

pub(crate) fn event_filter_from_params(
    p: &ListParams,
) -> Result<(EventFilter, Option<EventOutcome>), RuntimeError> {
    let mut verbs = Vec::new();
    if let Some(verb) = &p.verb {
        verbs.push(verb.clone());
    }
    if let Some(more) = &p.verbs {
        verbs.extend(more.clone());
    }

    let substrates = match p.substrate.as_deref() {
        Some(raw) => vec![parse_event_substrate(raw)?],
        None => Vec::new(),
    };

    let outcome = p.outcome.as_deref().map(parse_event_outcome).transpose()?;

    let mut kinds: Vec<EventKind> = Vec::new();
    if let Some(k) = &p.event_kind {
        kinds.push(parse_event_kind(k)?);
    }
    if let Some(ks) = &p.event_kinds {
        for k in ks {
            kinds.push(parse_event_kind(k)?);
        }
    }

    let session_id = p
        .session_id
        .as_deref()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid session_id {s:?}: {e}")))
        })
        .transpose()?;

    let observed = p
        .observed
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid observed id {s:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let selected = p
        .selected
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid selected id {s:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((
        EventFilter {
            verbs,
            substrates,
            actors: p.actor.clone().into_iter().collect(),
            after: p.since,
            before: p.until,
            kinds,
            session_id,
            observed,
            selected,
            ..EventFilter::default()
        },
        outcome,
    ))
}

pub(crate) fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// `pub` (widened from `pub(crate)`, ADR-099 B3 fix round 5, finding 4) — so
/// kkernel's `--atomic` seam can render update/delete result payloads with
/// the same timestamp normalization as the canonical handlers.
pub fn normalize_entity_timestamps(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        for field in &["created_at", "updated_at", "deleted_at", "expires_at"] {
            if let Some(val) = obj.get_mut(*field) {
                if let Some(micros) = val.as_i64() {
                    *val = Value::String(micros_to_iso(micros));
                }
            }
        }
    }
    v
}

pub(crate) fn normalize_entity_timestamps_array(v: Value) -> Value {
    match v {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_entity_timestamps).collect())
        }
        other => normalize_entity_timestamps(other),
    }
}

const TIMESTAMP_KEYS: &[&str] = &[
    "created_at",
    "updated_at",
    "deleted_at",
    "expiry",
    "applied_at",
    "withdrawn_at",
    "reviewed_at",
    "completed_at",
    "scheduled_at",
    "expires_at",
    "due",
    "remind_at",
];

pub(crate) fn walk_timestamps(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            for (key, val) in obj.iter_mut() {
                if TIMESTAMP_KEYS.contains(&key.as_str()) {
                    let micros_opt = val
                        .as_u64()
                        .and_then(|n| i64::try_from(n).ok())
                        .or_else(|| val.as_i64());
                    if let Some(micros) = micros_opt {
                        *val = Value::String(micros_to_iso(micros));
                        continue;
                    }
                }
                walk_timestamps(val);
            }
        }
        Value::Array(arr) => {
            for elem in arr.iter_mut() {
                walk_timestamps(elem);
            }
        }
        _ => {}
    }
}

pub(crate) fn normalize_event_timestamps(mut v: Value) -> Value {
    walk_timestamps(&mut v);
    v
}

pub(crate) fn normalize_event_timestamps_array(v: Value) -> Value {
    match v {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_event_timestamps).collect())
        }
        other => normalize_event_timestamps(other),
    }
}

pub(crate) fn props_match(entity_props: Option<&Value>, filter: &Value) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true,
    };
    let actual = match entity_props.and_then(Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

pub(crate) fn tags_match_any(entity_tags: &[String], wanted: &[String]) -> bool {
    if wanted.is_empty() {
        return true;
    }
    entity_tags
        .iter()
        .any(|tag| wanted.iter().any(|w| tag.eq_ignore_ascii_case(w)))
}

/// Merge the top-level `tags` create-param into `properties["tags"]` for a
/// note. Notes have no dedicated tags column (see search.rs's `tag_filter`
/// handling) — `properties["tags"]` is the storage convention already used
/// by `memory.remember` (khive-pack-memory/src/handlers/remember.rs) and by
/// this pack's own `search`/`list` note-tag filters. Without this merge,
/// `create(kind=note, tags=[...])` silently dropped the tags (#747).
///
/// Precedence: an empty/absent `tags` param leaves `properties` untouched.
/// A non-empty `tags` param always WINS over any `properties["tags"]` the
/// caller also supplied — the top-level, typed param is the more explicit
/// signal, so it overwrites rather than merges with a same-named nested key.
pub(crate) fn merge_note_tags(
    properties: Option<Value>,
    tags: Option<Vec<String>>,
) -> Result<Option<Value>, RuntimeError> {
    let tags = match tags {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(properties),
    };
    let mut obj = match properties {
        None => serde_json::Map::new(),
        Some(Value::Object(m)) => m,
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "create: note `tags` cannot be merged into non-object `properties` (got {other})"
            )));
        }
    };
    obj.insert("tags".to_string(), json!(tags));
    Ok(Some(Value::Object(obj)))
}

// ---- Handler helpers ----

pub(crate) fn parse_entity_policy(s: &str) -> Result<EntityDedupMergePolicy, RuntimeError> {
    match s {
        "prefer_into" => Ok(EntityDedupMergePolicy::PreferInto),
        "prefer_from" => Ok(EntityDedupMergePolicy::PreferFrom),
        "union" => Ok(EntityDedupMergePolicy::Union),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown strategy {other:?}; use prefer_into | prefer_from | union"
        ))),
    }
}

pub(crate) fn parse_content_strategy(s: &str) -> Result<ContentMergeStrategy, RuntimeError> {
    match s {
        "append" => Ok(ContentMergeStrategy::Append),
        "prefer_into" => Ok(ContentMergeStrategy::PreferInto),
        "prefer_from" => Ok(ContentMergeStrategy::PreferFrom),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown content_strategy {other:?}; use append | prefer_into | prefer_from"
        ))),
    }
}

pub(crate) async fn ensure_entity_kind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    expected_kind: Option<&str>,
) -> Result<(), RuntimeError> {
    let entity = runtime.get_entity(token, id).await?;
    if let Some(k) = expected_kind {
        if entity.kind != k {
            return Err(RuntimeError::NotFound(format!("{k} {id}")));
        }
    }
    Ok(())
}

pub(crate) async fn ensure_note_kind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    expected_kind: Option<&str>,
) -> Result<(), RuntimeError> {
    use khive_runtime::Resolved;
    let note = match runtime.resolve(token, id).await? {
        Some(Resolved::Note(note)) => note,
        _ => return Err(RuntimeError::NotFound("not found in this namespace".into())),
    };
    if let Some(k) = expected_kind {
        if note.kind != k {
            return Err(RuntimeError::NotFound(format!("{k} {id}")));
        }
    }
    Ok(())
}

pub(crate) fn description_patch(v: Option<Value>) -> Result<Option<Option<String>>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "description must be null or a string, got: {other}"
        ))),
    }
}

pub(crate) fn string_value(v: Option<Value>, field: &str) -> Result<Option<String>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{field} must be a string, got: {other}"
        ))),
    }
}

pub(crate) fn optional_string_patch(
    v: Option<Value>,
    field: &str,
) -> Result<Option<Option<String>>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{field} must be null or a string, got: {other}"
        ))),
    }
}

// ---- Query result rendering ----

pub(crate) fn sql_value_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Bool(v) => json!(v),
        SqlValue::Integer(v) => json!(v),
        SqlValue::Float(v) => json!(v),
        SqlValue::Text(v) => json!(v),
        SqlValue::Blob(v) => json!(v),
        SqlValue::Json(v) => v,
        SqlValue::Uuid(v) => json!(v.to_string()),
        SqlValue::Timestamp(v) => json!(v.to_rfc3339()),
    }
}

pub(crate) fn render_query_result(result: QueryResult) -> Value {
    let rows = result
        .rows
        .into_iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for col in row.columns {
                obj.insert(col.name, sql_value_to_json(col.value));
            }
            Value::Object(obj)
        })
        .collect::<Vec<_>>();

    let mut out = serde_json::Map::new();
    out.insert("rows".to_string(), Value::Array(rows));
    if !result.warnings.is_empty() {
        out.insert("warnings".to_string(), json!(result.warnings));
    }
    Value::Object(out)
}

// ---- list() row-cap truncation signal (issue #894) ----
//
// Every `list` branch enforces its own server-side row cap (entities: 500,
// notes: 200, edges: `KhiveRuntime::EDGE_LIST_MAX_LIMIT`, events: 1000).
// Before this fix a `limit` above the cap was silently clamped with no
// signal, so a caller striding a pagination loop by its own requested
// `limit` would silently skip rows between pages. The fix mirrors the
// `query()` verb's fix for the same defect class (#777): fetch one row past
// the cap (a "sentinel") only when the request could exceed it, and let the
// sentinel's presence in the real result set — not the requested `limit`
// alone — decide whether a warning is honest. A `limit` above the cap that
// only matches a handful of rows must not warn; a `limit` at or under the
// cap never probes and never warns, even if more rows exist past the page.

/// Rows to ask the store for, given the caller's `requested` limit and this
/// branch's `cap`. When `requested` is within the cap, fetch exactly that —
/// no probe, no possible truncation. When it exceeds the cap, fetch `cap + 1`
/// (the sentinel row) so [`cap_truncation_warning`] can tell real truncation
/// apart from a harmless over-cap request.
pub(crate) fn cap_fetch_limit(requested: u32, cap: u32) -> u32 {
    if requested > cap {
        cap.saturating_add(1)
    } else {
        requested
    }
}

/// Given how many rows actually came back for a [`cap_fetch_limit`] fetch,
/// decide whether the cap was the binding constraint. Returns `None` when
/// `fetched_len` is at or under `cap` — nothing was dropped, so no warning.
/// Returns `Some(message)` when the sentinel row is present, i.e. more than
/// `cap` rows genuinely matched; the caller must then truncate the result
/// set to `cap` before returning it (the sentinel itself is not real data).
pub(crate) fn cap_truncation_warning(fetched_len: usize, requested: u32, cap: u32) -> Option<String> {
    if fetched_len as u64 > cap as u64 {
        Some(format!(
            "result set capped at {cap} rows; requested limit {requested} exceeds the cap — \
             use offset to page through the remaining results"
        ))
    } else {
        None
    }
}

/// Wrap a `list()` result array in the stable `{"items": [...], "warnings": \
/// [...]}` envelope (`warnings` omitted when empty). All `list` branches use
/// this shape now, matching the object envelope `query()` already returns
/// for the same truncation-signal defect class (#777) — see the module
/// comment above.
pub(crate) fn wrap_list_items(items: Value, warnings: Vec<String>) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("items".to_string(), items);
    if !warnings.is_empty() {
        out.insert("warnings".to_string(), json!(warnings));
    }
    Value::Object(out)
}

#[cfg(test)]
mod cap_signal_tests {
    use super::*;

    #[test]
    fn fetch_limit_under_cap_is_unchanged() {
        assert_eq!(cap_fetch_limit(50, 500), 50);
    }

    #[test]
    fn fetch_limit_at_cap_is_unchanged_no_probe() {
        // requested == cap: never probes, since the cap cannot bind any
        // harder than the caller already asked for.
        assert_eq!(cap_fetch_limit(500, 500), 500);
    }

    #[test]
    fn fetch_limit_over_cap_probes_cap_plus_one() {
        assert_eq!(cap_fetch_limit(1000, 500), 501);
    }

    #[test]
    fn fetch_limit_over_cap_saturates_on_overflow() {
        assert_eq!(cap_fetch_limit(u32::MAX, u32::MAX - 1), u32::MAX);
    }

    #[test]
    fn no_warning_when_fetched_at_or_under_cap() {
        // Mirrors #777's "LIMIT above cap, few real matches" case: the
        // caller asked for more than the cap, but only 40 rows exist —
        // nothing was actually dropped, so no warning.
        assert_eq!(cap_truncation_warning(40, 1000, 500), None);
        assert_eq!(cap_truncation_warning(500, 1000, 500), None);
    }

    #[test]
    fn no_warning_when_requested_never_exceeded_cap() {
        // requested <= cap: cap_fetch_limit never probes, so fetched_len can
        // never exceed cap here in practice, but the predicate alone must
        // still not warn if it somehow did (defense in depth).
        assert_eq!(cap_truncation_warning(500, 500, 500), None);
    }

    #[test]
    fn warns_when_sentinel_row_present() {
        let warning =
            cap_truncation_warning(501, 1000, 500).expect("sentinel row must trigger a warning");
        assert!(warning.contains("500"), "{warning}");
        assert!(warning.contains("1000"), "{warning}");
    }

    #[test]
    fn wrap_list_items_omits_warnings_key_when_empty() {
        let wrapped = wrap_list_items(json!([1, 2, 3]), Vec::new());
        assert_eq!(wrapped, json!({"items": [1, 2, 3]}));
        assert!(wrapped.get("warnings").is_none());
    }

    #[test]
    fn wrap_list_items_includes_warnings_key_when_present() {
        let wrapped = wrap_list_items(json!([1]), vec!["capped".to_string()]);
        assert_eq!(wrapped, json!({"items": [1], "warnings": ["capped"]}));
    }
}
