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
    SearchParams, StatsParams, TraverseParams, UpdateParams, WhoamiParams, WithdrawParams,
    HARD_CAP,
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
    registry: &VerbRegistry,
) -> Result<Option<String>, RuntimeError> {
    let Some(raw) = entity_type else {
        return Ok(None);
    };
    let kind = kind_name
        .parse::<khive_types::EntityKind>()
        .map_err(|_| RuntimeError::InvalidInput(format!("unknown entity kind {kind_name:?}")))?;
    // ADR-017 additive composition (not `EntityTypeRegistry::global()`); see
    // docs/api/entity-kind-validation.md#validate_entity_type.
    let composed = EntityTypeRegistry::with_extra(registry.all_entity_types());
    let resolved = composed.resolve(kind, Some(raw))?;
    Ok(resolved.entity_type)
}

// ---- Granular `kind` discriminator ----

/// Resolved shape of a `kind` discriminator string: which substrate (entity, note,
/// edge, event, proposal) it names, plus the specific granular kind if any.
/// See `docs/api/entity-kind-validation.md#adr-099-b3-pub-widening-rationale` for why this is `pub` rather than `pub(crate)`.
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

/// Every wire-level `kind` value accepted anywhere in the KG pack: the five
/// fixed substrate names plus every granular entity/note kind `registry` has
/// merged in from the loaded pack set. Sourced entirely from the registry so
/// a pack that registers a new kind is reflected here without a code change.
fn all_valid_kind_names(registry: &VerbRegistry) -> Vec<String> {
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
    all
}

/// Build the error for a required `kind`-shaped param that the caller omitted
/// entirely, enumerating every value the registry currently accepts (same
/// list [`resolve_kind_spec`] uses for an unrecognized value).
pub fn missing_kind_error(param_name: &str, registry: &VerbRegistry) -> RuntimeError {
    RuntimeError::InvalidInput(format!(
        "missing required param {param_name:?}; valid kinds: {}",
        all_valid_kind_names(registry).join(" | ")
    ))
}

/// Resolve a wire-level `kind` value into a [`KindSpec`]. Accepts a bare substrate
/// name (`entity`, `note`, `edge`, `event`, `proposal`) or a granular entity/note kind
/// registered on `registry`.
///
/// # Errors
///
/// Returns [`RuntimeError::InvalidInput`] listing every valid value if `raw` matches
/// neither a substrate name nor a registered granular kind.
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

    Err(RuntimeError::InvalidInput(format!(
        "unknown kind {raw:?}; valid: {}",
        all_valid_kind_names(registry).join(" | ")
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

/// Resolve `s` (a full UUID, an 8+ hex-digit UUID prefix, or an entity name) to a
/// [`Uuid`], namespace-agnostic (ADR-007 Rev 6 by-ID contract — no namespace filtering
/// is applied to the prefix or full-UUID forms; name resolution still scopes to
/// `token`'s namespace).
///
/// # Errors
///
/// [`RuntimeError::InvalidInput`] if a hex-prefix matches no record;
/// [`RuntimeError::NotFound`]/[`RuntimeError::Ambiguous`] from name resolution.
/// See `docs/api/entity-kind-validation.md#adr-099-b3-pub-widening-rationale`.
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

/// Same as [`resolve_uuid_unfiltered`], but the prefix-resolution branch also matches
/// soft-deleted rows. Used by the hard-delete by-ID path (#391 §3), which must be able
/// to target a record regardless of its `deleted_at` state.
///
/// # Errors
///
/// Same error conditions as [`resolve_uuid_unfiltered`].
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

/// Max `annotates` targets accepted per note create (singleton or bulk item).
/// Shared between the singleton and bulk note-create paths so the contract
/// stays uniform: neither path may be looser than the other.
pub(crate) const ANNOTATES_CAP: usize = 100;

/// Max total `annotates` targets (after per-item dedup) accepted across an
/// entire bulk create request. The per-item cap alone still lets a 1000-item
/// batch drive up to 100,000 target resolutions and edge writes; this budget
/// keeps a request's total annotates work in the same order as the items cap
/// itself (1000 items, ~1 annotation each) rather than 100x it.
pub(crate) const ANNOTATES_BULK_BUDGET: usize = 1000;

/// Dedup key for an `annotates` target: targets that parse as a UUID dedup on
/// the parsed value (so case variants and other equivalent encodings of the
/// same UUID collapse), matching the equivalence `resolve_uuid_unfiltered`
/// itself applies via `Uuid::from_str` before falling back to prefix/name
/// resolution. Non-UUID references (hex prefixes, names) dedup on the raw
/// string only — the resolver does not treat two different prefixes/names as
/// equivalent ahead of a storage lookup, so dedup must not invent that either.
#[derive(PartialEq, Eq, Hash)]
enum AnnotateKey {
    Uuid(Uuid),
    Raw(String),
}

fn annotate_dedup_key(target: &str) -> AnnotateKey {
    match Uuid::from_str(target) {
        Ok(uuid) => AnnotateKey::Uuid(uuid),
        Err(_) => AnnotateKey::Raw(target.to_string()),
    }
}

/// Deduplicate a raw `annotates` list using [`annotate_dedup_key`], preserving
/// first-occurrence order.
fn dedup_annotates_raw(raw: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(raw.len());
    raw.into_iter()
        .filter(|target| seen.insert(annotate_dedup_key(target)))
        .collect()
}

/// Reject an oversized `annotates` list, then deduplicate targets before
/// resolving each to a UUID — a duplicated target must not cost an extra
/// lookup or produce an extra edge.
pub(crate) async fn resolve_annotates_targets(
    raw: Vec<String>,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Vec<Uuid>, RuntimeError> {
    if raw.len() > ANNOTATES_CAP {
        return Err(RuntimeError::InvalidInput(format!(
            "annotates accepts at most {ANNOTATES_CAP} targets per note; got {}",
            raw.len()
        )));
    }
    let deduped = dedup_annotates_raw(raw);
    let mut resolved = Vec::with_capacity(deduped.len());
    for target in deduped {
        resolved.push(resolve_uuid_unfiltered(&target, runtime, token).await?);
    }
    Ok(resolved)
}

/// Pre-scan a bulk create request's raw items and reject it if the total
/// `annotates` target count (summed per-item, each item deduped the same way
/// [`resolve_annotates_targets`] would) exceeds [`ANNOTATES_BULK_BUDGET`].
/// Runs before any per-item spec building or target resolution — a request
/// that would blow the budget must not cost a single lookup.
pub(crate) fn check_bulk_annotates_budget(entries: &[Value]) -> Result<(), RuntimeError> {
    let mut total = 0usize;
    for entry in entries {
        let Some(raw) = entry.get("annotates") else {
            continue;
        };
        let Ok(targets) = serde_json::from_value::<Vec<String>>(raw.clone()) else {
            continue;
        };
        total += dedup_annotates_raw(targets).len();
    }
    if total > ANNOTATES_BULK_BUDGET {
        return Err(RuntimeError::InvalidInput(format!(
            "bulk create annotates budget exceeded: at most {ANNOTATES_BULK_BUDGET} total annotates targets per request (after per-item dedup); got {total}"
        )));
    }
    Ok(())
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

pub(crate) fn remap_note_status_array(v: Value) -> Value {
    match v {
        Value::Array(arr) => Value::Array(arr.into_iter().map(remap_note_status).collect()),
        other => remap_note_status(other),
    }
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

/// Relations valid for an entity-kind pair, derived from the same allowlist +
/// pack `EDGE_RULES` sources the real link validator consults (issue #543).
/// See `docs/api/entity-kind-validation.md#valid_relations_for_entity_pair`.
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

/// Convert `created_at`/`updated_at`/`deleted_at`/`expires_at` fields on a JSON entity
/// object from epoch-micros integers to ISO-8601 strings, in place. Fields that are
/// absent or already non-integer are left untouched.
/// See `docs/api/entity-kind-validation.md#adr-099-b3-pub-widening-rationale`.
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

/// Merge the top-level `tags` create-param into `properties["tags"]` for a note
/// (#747). A non-empty `tags` param always wins over any `properties["tags"]` the
/// caller also supplied. See `docs/api/note-crud-fields.md#merge_note_tags`.
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
    // Always present (not gated on the cap having fired) so a caller can
    // check it unconditionally rather than inferring "not truncated" from
    // the field's absence (#1168, #1247).
    out.insert("truncated".to_string(), json!(result.truncated));
    Value::Object(out)
}
