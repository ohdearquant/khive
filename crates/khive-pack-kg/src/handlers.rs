//! Verb handlers for the KG pack.
//!
//! Each handler: deserialize params from Value → validate → call runtime → serialize result.

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    ContentMergeStrategy, EdgeListFilter, EdgePatch, EntityDedupMergePolicy, EntityPatch,
    KhiveRuntime, LinkSpec, MergeSummary, NamespaceToken, NotePatch, RuntimeError, VerbRegistry,
};
use khive_storage::types::{
    Direction, NeighborQuery, PageRequest, TraversalOptions, TraversalRequest,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{EdgeRelation, EntityFilter, EventFilter, EventOutcome, SubstrateKind};

use khive_types::{
    EntityKind, EventKind, ProposalChangeset, ProposalCreatedPayload, ProposalDecision,
    ProposalReviewedPayload, ProposalWithdrawnPayload,
};

use crate::vocab::NoteKind;
use crate::KgPack;

// ---- Kind canonicalization (ADR-030) ----
//
// kg's vocab (EntityKind / NoteKind) provides alias normalization for kg-owned
// kinds ("paper" → "document", "obs" → "observation", etc.). Other packs
// (gtd, future) register kinds with no aliases — those are matched against the
// merged registry vocabulary literally. The hybrid resolver tries kg's enum
// first, then falls back to registry membership.

fn canonical_entity_kind(raw: &str, registry: &VerbRegistry) -> Result<String, RuntimeError> {
    if let Ok(k) = EntityKind::from_str(raw) {
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

fn canonical_note_kind(raw: &str, registry: &VerbRegistry) -> Result<String, RuntimeError> {
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

// ---- Granular `kind` discriminator (CRUD verbs) ----
//
// The wire-level `kind` param accepts either a substrate-level name
// (`"entity"`, `"note"`, `"edge"`) or any pack-registered granular kind
// (`"concept"`, `"task"`, …). The granular form infers the substrate from the
// registry and lets the call site skip the legacy `entity_kind` /
// `note_kind` subfield.
//
// Substrate-level names are reserved; they're matched first so that a future
// pack accidentally registering `"entity"` as a kind name doesn't shadow the
// substrate-wide form.

/// Resolved shape of a `kind` discriminator string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KindSpec {
    /// `kind="entity"` — substrate-wide, no implicit kind filter.
    Entity { specific: Option<String> },
    /// `kind="note"` — substrate-wide, no implicit kind filter.
    Note { specific: Option<String> },
    /// `kind="edge"` — only valid for `list`.
    Edge,
    /// `kind="event"` — only valid for `list`; `get` resolves events by UUID.
    Event,
    /// `kind="proposal"` — queries the `proposals_open` projection table (ADR-046).
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
/// Order:
/// 1. Substrate-level reserved names (`entity` / `note` / `edge`).
/// 2. kg's typed enums (alias-tolerant — `"paper"` → `"document"`).
/// 3. Pack-registered entity kinds in `registry.all_entity_kinds()`.
/// 4. Pack-registered note kinds in `registry.all_note_kinds()`.
/// 5. Unknown → `InvalidInput` listing every legal value.
pub(crate) fn resolve_kind_spec(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<KindSpec, RuntimeError> {
    let normalized = raw.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "entity" => return Ok(KindSpec::Entity { specific: None }),
        "note" => return Ok(KindSpec::Note { specific: None }),
        "edge" => return Ok(KindSpec::Edge),
        "event" => return Ok(KindSpec::Event),
        "proposal" => return Ok(KindSpec::Proposal),
        _ => {}
    }

    // kg-typed enums first so aliases like "paper" → "document" still work.
    if let Ok(k) = EntityKind::from_str(raw) {
        return Ok(KindSpec::Entity {
            specific: Some(k.name().to_string()),
        });
    }
    if let Ok(k) = NoteKind::from_str(raw) {
        return Ok(KindSpec::Note {
            specific: Some(k.name().to_string()),
        });
    }

    // Pack-registered granular kinds.
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
/// If both are supplied, they must canonicalize to the same value.
fn reconcile_specific(
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

// ---- Param structs (serde-only, no rmcp dependency) ----

#[derive(Deserialize)]
struct CreateParams {
    kind: String,
    entity_type: Option<String>,
    name: Option<String>,
    description: Option<String>,
    content: Option<String>,
    salience: Option<f64>,
    annotates: Option<Vec<String>>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct GetParams {
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    kind: String,
    limit: Option<u32>,
    offset: Option<u32>,
    entity_kind: Option<String>,
    entity_type: Option<String>,
    source_id: Option<String>,
    target_id: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    max_weight: Option<f64>,
    note_kind: Option<String>,
    // event-specific filters
    verb: Option<String>,
    verbs: Option<Vec<String>>,
    outcome: Option<String>,
    actor: Option<String>,
    substrate: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    event_kind: Option<String>,
    event_kinds: Option<Vec<String>>,
    session_id: Option<String>,
    observed: Option<Vec<String>>,
    selected: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct UpdateParams {
    id: String,
    kind: String,
    name: Option<Value>,
    description: Option<Value>,
    content: Option<String>,
    #[serde(default, deserialize_with = "tri_f64")]
    salience: Option<Option<f64>>,
    #[serde(default, deserialize_with = "tri_f64")]
    decay_factor: Option<Option<f64>>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    relation: Option<String>,
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct DeleteParams {
    id: String,
    kind: String,
    hard: Option<bool>,
}

#[derive(Deserialize)]
struct MergeParams {
    into_id: String,
    from_id: String,
    kind: Option<String>,
    strategy: Option<String>,
    content_strategy: Option<String>,
    dry_run: Option<bool>,
    #[allow(dead_code)]
    verbose: Option<bool>,
}

#[derive(Deserialize)]
struct SearchParams {
    kind: String,
    query: String,
    limit: Option<u32>,
    entity_kind: Option<String>,
    entity_type: Option<String>,
    note_kind: Option<String>,
    include_superseded: Option<bool>,
    properties: Option<Value>,
}

/// One entry in a bulk-link request (F205 / ADR-038).
#[derive(Deserialize)]
struct BulkLinkEntry {
    source_id: String,
    target_id: String,
    relation: String,
    weight: Option<f64>,
    metadata: Option<Value>,
    dependency_kind: Option<String>,
}

#[derive(Deserialize)]
struct LinkParams {
    // Singleton fields (required unless `links` is provided).
    source_id: Option<String>,
    target_id: Option<String>,
    relation: Option<String>,
    weight: Option<f64>,
    /// Edge metadata (open JSON; governed keys validated by runtime).
    metadata: Option<Value>,
    /// Shortcut for `metadata.dependency_kind` on `depends_on` edges.
    dependency_kind: Option<String>,
    /// When `true`, output uses full UUIDs and ISO 8601 timestamps instead of
    /// the default 8-char short IDs and YYYY/MM/DD date format.
    verbose: Option<bool>,
    // Bulk link fields (ADR-038).
    /// Multiple edges to create in one call.
    links: Option<Vec<BulkLinkEntry>>,
    /// When `true` (default), the entire batch is atomic — any failure rolls
    /// back all writes. When `false`, errors are collected and returned as
    /// warnings while successful entries are committed individually.
    atomic: Option<bool>,
}

#[derive(Deserialize)]
struct NeighborsParams {
    /// Accepts either `id` (canonical, ADR-148 normalized) or `node_id` (legacy).
    #[serde(alias = "node_id")]
    id: String,
    direction: Option<String>,
    limit: Option<u32>,
    min_weight: Option<f64>,
    relations: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TraverseParams {
    /// Accepts either `roots` (legacy) or `ids` (normalized). Each entry may
    /// be a full UUID or an 8-char prefix; resolved via `resolve_uuid_async`.
    #[serde(alias = "ids")]
    roots: Vec<String>,
    max_depth: Option<usize>,
    direction: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    limit: Option<u32>,
    include_roots: Option<bool>,
}

#[derive(Deserialize)]
struct QueryParams {
    query: String,
}

// ---- Proposal param structs (ADR-046) ----

#[derive(Deserialize)]
struct ProposeParams {
    title: String,
    description: String,
    changeset: Value,
    #[serde(default)]
    reviewers: Vec<String>,
    expiry: Option<i64>,
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct ReviewParams {
    proposal_id: String,
    decision: String,
    comment: Option<String>,
}

#[derive(Deserialize)]
struct WithdrawParams {
    proposal_id: String,
    rationale: Option<String>,
}

#[derive(Deserialize)]
struct ListProposalsParams {
    status: Option<String>,
    proposer: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

// ---- Helpers ----

/// Resolve an entity name to its UUID.
///
/// Strategy (for issue #65):
/// 1. Exact case-insensitive name match — returns the entity's UUID.
/// 2. If 0 matches: `NotFound("entity not found: '{name}'")`
/// 3. If multiple matches: `Ambiguous("ambiguous name '{name}': found N entities [id1, id2, ...]")`
///
/// Only searches `entity` substrate (notes and edges don't have meaningful
/// user-facing names in the same sense).
async fn resolve_name_async(
    name: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    // Use EntityFilter.name_prefix with the full name to do an exact match.
    // The DB implements `name LIKE '?%'` so we get back all names that start
    // with `name`. We then filter to exact (case-insensitive) matches.
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

/// Resolve a string to a UUID for use as an edge endpoint (source or target).
///
/// Resolution order (issue #65):
/// 1. Full UUID string — parse directly.
/// 2. 8+ hex-character prefix — delegate to `runtime.resolve_prefix`.
/// 3. Everything else — treat as an entity name and call `resolve_name_async`.
async fn resolve_uuid_async(
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
    // Fall back to name-based resolution (issue #65).
    resolve_name_async(s, runtime, token).await
}

// ---- Output formatting helpers (issue #66) ----

/// Truncate a UUID string to 8 characters for compact display.
/// Post-process a serialized edge JSON for display.
///
/// Display formatting (short IDs, compact dates) belongs in the CLI/UI layer,
/// not in the MCP response. Returns the value unchanged.
fn format_edge_output(v: Value, _verbose: bool) -> Value {
    v
}

fn parse_direction(s: Option<&str>) -> Direction {
    match s {
        Some("in") => Direction::In,
        Some("both") => Direction::Both,
        _ => Direction::Out,
    }
}

/// Merge `dependency_kind` shortcut into `metadata` for `depends_on` edges.
///
/// When `dependency_kind` is provided separately and `metadata` does not already
/// carry the key, the value is injected into the metadata object. This allows
/// callers to write `dependency_kind: "build"` instead of the full
/// `metadata: { "dependency_kind": "build" }` form.
fn merge_entry_metadata(
    metadata: Option<Value>,
    dependency_kind: Option<String>,
) -> Result<Option<Value>, RuntimeError> {
    let Some(dk) = dependency_kind else {
        return Ok(metadata);
    };
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    let map = obj
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("metadata must be a JSON object".into()))?;
    map.entry("dependency_kind".to_string())
        .or_insert_with(|| serde_json::json!(dk));
    Ok(Some(obj))
}

fn parse_relation(s: &str) -> Result<EdgeRelation, RuntimeError> {
    s.parse::<EdgeRelation>().map_err(|_| {
        let valid = EdgeRelation::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        RuntimeError::InvalidInput(format!("unknown relation {s:?}; valid: {valid}"))
    })
}

const IMMUTABLE_EVENT_MSG: &str = "events are immutable — create/update/delete are not permitted";

fn immutable_event_error() -> RuntimeError {
    RuntimeError::InvalidInput(IMMUTABLE_EVENT_MSG.into())
}

fn parse_event_outcome(raw: &str) -> Result<EventOutcome, RuntimeError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "success" => Ok(EventOutcome::Success),
        "denied" => Ok(EventOutcome::Denied),
        "error" => Ok(EventOutcome::Error),
        _ => Err(RuntimeError::InvalidInput(format!(
            "unknown outcome {raw:?}; valid: success | denied | error"
        ))),
    }
}

fn parse_event_substrate(raw: &str) -> Result<SubstrateKind, RuntimeError> {
    raw.trim()
        .to_ascii_lowercase()
        .parse::<SubstrateKind>()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "unknown substrate {raw:?}; valid: note | entity | event"
            ))
        })
}

fn parse_event_kind(raw: &str) -> Result<EventKind, RuntimeError> {
    raw.parse::<EventKind>()
        .map_err(|e| RuntimeError::InvalidInput(format!("unknown event_kind {raw:?}: {e}")))
}

fn event_filter_from_params(
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

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Returns true if `entity_props` contains all key=value pairs from `filter`.
///
/// Both the filter object and the entity's `properties` field must be JSON
/// objects. If the entity has no properties, returns false when the filter is
/// non-empty. String comparisons are case-sensitive.
fn props_match(entity_props: Option<&Value>, filter: &Value) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true, // empty or non-object filter matches everything
    };
    let actual = match entity_props.and_then(Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

// ---- Handler helpers ----

fn parse_entity_policy(s: &str) -> Result<EntityDedupMergePolicy, RuntimeError> {
    match s {
        "prefer_into" => Ok(EntityDedupMergePolicy::PreferInto),
        "prefer_from" => Ok(EntityDedupMergePolicy::PreferFrom),
        "union" => Ok(EntityDedupMergePolicy::Union),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown strategy {other:?}; use prefer_into | prefer_from | union"
        ))),
    }
}

fn parse_content_strategy(s: &str) -> Result<ContentMergeStrategy, RuntimeError> {
    match s {
        "append" => Ok(ContentMergeStrategy::Append),
        "prefer_into" => Ok(ContentMergeStrategy::PreferInto),
        "prefer_from" => Ok(ContentMergeStrategy::PreferFrom),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown content_strategy {other:?}; use append | prefer_into | prefer_from"
        ))),
    }
}

async fn ensure_entity_kind(
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

async fn ensure_note_kind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    expected_kind: Option<&str>,
) -> Result<(), RuntimeError> {
    let note = runtime
        .notes(token)?
        .get_note(id)
        .await
        .map_err(RuntimeError::Storage)?
        .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
    if let Some(k) = expected_kind {
        if note.kind != k {
            return Err(RuntimeError::NotFound(format!("{k} {id}")));
        }
    }
    Ok(())
}

fn description_patch(v: Option<Value>) -> Result<Option<Option<String>>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "description must be null or a string, got: {other}"
        ))),
    }
}

fn string_value(v: Option<Value>, field: &str) -> Result<Option<String>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{field} must be a string, got: {other}"
        ))),
    }
}

fn optional_string_patch(
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

/// Serde deserializer for tri-state nullable f64:
/// field absent → outer None, field = null → Some(None), field = number → Some(Some(v)).
fn tri_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

// ---- Handler implementations ----

impl KgPack {
    pub(crate) async fn handle_create(
        &self,
        token: &NamespaceToken,
        mut params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Read the discriminator without consuming params (the hook may mutate).
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("create requires 'kind'".into()))?
            .to_string();

        // Resolve the granular form (`kind="concept"`, `kind="task"`, …) or the
        // legacy substrate-level form (`kind="entity"` + `entity_kind=…`).
        let spec = resolve_kind_spec(&raw_kind, registry)?;

        // Canonicalize the sub-discriminator + look up the kind hook (ADR-030).
        // For entities the hook is rarely used; for notes it's how gtd's `task`
        // kind layers defaults + edges over the shared CRUD path.
        let (sub_kind, hook) = match &spec {
            KindSpec::Entity { specific } => {
                let legacy = params
                    .get("entity_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "kind=entity requires a specific kind: either kind=<concept|document|dataset|project|person|org|artifact|service> directly, or kind=entity + entity_kind=<…>".into(),
                    )
                })?;
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Note { specific } => {
                let legacy = params
                    .get("note_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty());
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?
                .unwrap_or_else(|| "observation".to_string());
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Event => {
                return Err(immutable_event_error());
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "kind=edge is not creatable via `create` — use `link` for edges".into(),
                ));
            }
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "kind=proposal is not creatable via `create` — use `propose` to create a proposal".into(),
                ));
            }
        };

        // Rewrite `kind` to the substrate label so downstream `CreateParams`
        // matching stays substrate-discriminated; granular form is now absorbed.
        if let Some(obj) = params.as_object_mut() {
            obj.insert("kind".into(), json!(spec.substrate_label()));
            // Also normalize the legacy subfield so the hook sees the canonical
            // value when the user passed only the granular form.
            if let Some(ref canonical) = sub_kind {
                match spec {
                    KindSpec::Entity { .. } => {
                        obj.insert("entity_kind".into(), json!(canonical));
                    }
                    KindSpec::Note { .. } => {
                        obj.insert("note_kind".into(), json!(canonical));
                    }
                    KindSpec::Edge | KindSpec::Event | KindSpec::Proposal => {}
                }
            }
        }

        // Propagate the authorized namespace into params so KindHooks can build
        // their own NamespaceToken (hooks don't receive a token directly).
        if let Some(obj) = params.as_object_mut() {
            obj.entry("namespace")
                .or_insert_with(|| json!(token.namespace().as_str()));
        }

        if let Some(ref h) = hook {
            h.prepare_create(&self.runtime, &mut params).await?;
        }

        let p: CreateParams = deser(params.clone())?;

        let (response, new_id) = match p.kind.as_str() {
            "entity" => {
                let canonical = sub_kind.clone().expect("entity_kind canonicalized above");
                let name = p.name.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=entity requires 'name'".into())
                })?;
                let tags = p.tags.unwrap_or_default();
                let entity = self
                    .runtime
                    .create_entity(
                        token,
                        &canonical,
                        p.entity_type.as_deref(),
                        &name,
                        p.description.as_deref(),
                        p.properties,
                        tags,
                    )
                    .await?;
                let id = entity.id;
                (to_json(&entity)?, id)
            }
            "note" => {
                let canonical = sub_kind
                    .clone()
                    .unwrap_or_else(|| "observation".to_string());
                let content = p.content.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=note requires 'content'".into())
                })?;
                let mut annotates = Vec::new();
                for s in p.annotates.unwrap_or_default() {
                    annotates.push(resolve_uuid_async(&s, &self.runtime, token).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        token,
                        &canonical,
                        p.name.as_deref(),
                        &content,
                        p.salience,
                        p.properties,
                        annotates,
                    )
                    .await?;
                let id = note.id;
                (to_json(&note)?, id)
            }
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown kind {other:?}; valid: entity | note"
                )))
            }
        };

        if let Some(ref h) = hook {
            if let Err(e) = h.after_create(&self.runtime, new_id, &params).await {
                tracing::warn!(
                    kind = %sub_kind.as_deref().unwrap_or(""),
                    id = %new_id,
                    error = %e,
                    "kind hook after_create failed (storage write already committed)"
                );
            }
        }

        Ok(response)
    }

    pub(crate) async fn handle_get(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;

        if let Ok(entity) = self.runtime.get_entity(token, id).await {
            return to_json(&serde_json::json!({"kind": "entity", "data": entity}));
        }

        if let Some(note) = self
            .runtime
            .notes(token)?
            .get_note(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if note.namespace == token.namespace().as_str() {
                return to_json(&serde_json::json!({"kind": "note", "data": note}));
            }
        }

        if let Some(edge) = self.runtime.get_edge(token, id).await? {
            return to_json(&serde_json::json!({"kind": "edge", "data": edge}));
        }

        if let Some(event) = self
            .runtime
            .events(token)?
            .get_event(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if event.namespace == token.namespace().as_str() {
                return to_json(&serde_json::json!({"kind": "event", "data": event}));
            }
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_list(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Fast-path: kind=proposal dispatches to the proposals_open projection
        // before deserializing into ListParams, so proposal-specific fields
        // (status, proposer) are handled without polluting ListParams.
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if raw_kind == "proposal" {
            return self.handle_list_proposals(token, params).await;
        }

        let p: ListParams = deser(params)?;
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                let limit = p.limit.unwrap_or(50).min(500);
                let offset = p.offset.unwrap_or(0);
                let entities = self
                    .runtime
                    .list_entities(
                        token,
                        kind_filter.as_deref(),
                        p.entity_type.as_deref(),
                        limit,
                        offset,
                    )
                    .await?;
                to_json(&entities)
            }
            KindSpec::Edge => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let relations: Vec<EdgeRelation> = p
                    .relations
                    .unwrap_or_default()
                    .iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let filter = EdgeListFilter {
                    source_id,
                    target_id,
                    relations,
                    min_weight: p.min_weight,
                    max_weight: p.max_weight,
                };
                let limit = p.limit.unwrap_or(100);
                let edges = self.runtime.list_edges(token, filter, limit).await?;
                to_json(&edges)
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let limit = p.limit.unwrap_or(20).min(200);
                let offset = p.offset.unwrap_or(0);
                let notes = self
                    .runtime
                    .list_notes(token, kind_filter.as_deref(), limit, offset)
                    .await?;
                to_json(&notes)
            }
            KindSpec::Proposal => unreachable!("kind=proposal fast-pathed before deser"),
            KindSpec::Event => {
                let limit = p.limit.unwrap_or(100).clamp(1, 1000);
                let offset = p.offset.unwrap_or(0);
                let (filter, outcome) = event_filter_from_params(&p)?;

                if let Some(wanted_outcome) = outcome {
                    let mut items = Vec::new();
                    let mut skipped = 0u32;
                    let mut raw_offset = 0u32;
                    let scan_ceiling = offset.saturating_add(limit).saturating_mul(20);

                    while (items.len() as u32) < limit {
                        let remaining = scan_ceiling.saturating_sub(raw_offset);
                        if remaining == 0 {
                            break;
                        }
                        let batch_size = 100u32.min(remaining);
                        let page = self
                            .runtime
                            .list_events(
                                token,
                                filter.clone(),
                                PageRequest {
                                    limit: batch_size,
                                    offset: raw_offset.into(),
                                },
                            )
                            .await?;
                        let batch_len = page.items.len() as u32;
                        if batch_len == 0 {
                            break;
                        }
                        raw_offset = raw_offset.saturating_add(batch_len);
                        let eof = batch_len < batch_size;

                        for event in page.items {
                            if event.outcome != wanted_outcome {
                                continue;
                            }
                            if skipped < offset {
                                skipped += 1;
                                continue;
                            }
                            items.push(event);
                            if (items.len() as u32) >= limit {
                                break;
                            }
                        }

                        if eof {
                            break;
                        }
                    }
                    to_json(&items)
                } else {
                    let page = self
                        .runtime
                        .list_events(
                            token,
                            filter,
                            PageRequest {
                                limit,
                                offset: offset.into(),
                            },
                        )
                        .await?;
                    to_json(&page.items)
                }
            }
        }
    }

    pub(crate) async fn handle_update(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let spec = resolve_kind_spec(&p.kind, registry)?;

        match spec {
            KindSpec::Entity { specific } => {
                let entity = self.runtime.get_entity(token, id).await?;
                if specific.as_ref().is_some_and(|k| entity.kind != *k) {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                let patch = EntityPatch {
                    name: string_value(p.name, "name")?,
                    description: description_patch(p.description)?,
                    properties: p.properties,
                    tags: p.tags,
                };
                to_json(&self.runtime.update_entity(token, id, patch).await?)
            }
            KindSpec::Edge => {
                let relation = p.relation.as_deref().map(parse_relation).transpose()?;
                let patch = EdgePatch {
                    relation,
                    weight: p.weight,
                    properties: p.properties,
                };
                to_json(&self.runtime.update_edge(token, id, patch).await?)
            }
            KindSpec::Note { specific } => {
                let note = self
                    .runtime
                    .notes(token)?
                    .get_note(id)
                    .await
                    .map_err(RuntimeError::Storage)?;
                if note
                    .as_ref()
                    .is_none_or(|n| specific.as_ref().is_some_and(|k| n.kind != *k))
                {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                let patch = NotePatch::new(
                    optional_string_patch(p.name, "name")?,
                    p.content,
                    p.salience,
                    p.decay_factor,
                    p.properties,
                );
                to_json(&self.runtime.update_note(token, id, patch).await?)
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }

    pub(crate) async fn handle_delete(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let spec = resolve_kind_spec(&p.kind, registry)?;

        match spec {
            KindSpec::Entity { specific } => {
                if let Some(ref expected) = specific {
                    let entity = self.runtime.get_entity(token, id).await?;
                    if entity.kind != *expected {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self
                    .runtime
                    .delete_entity(token, id, p.hard.unwrap_or(false))
                    .await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Note { specific } => {
                if let Some(ref expected) = specific {
                    let note = self
                        .runtime
                        .notes(token)?
                        .get_note(id)
                        .await
                        .map_err(RuntimeError::Storage)?;
                    if note.as_ref().is_none_or(|n| n.kind != *expected) {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self
                    .runtime
                    .delete_note(token, id, p.hard.unwrap_or(false))
                    .await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Edge => {
                let deleted = self
                    .runtime
                    .delete_edge(token, id, p.hard.unwrap_or(false))
                    .await?;
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": "edge" }))
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }

    pub(crate) async fn handle_merge(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        let into_id = resolve_uuid_async(&p.into_id, &self.runtime, token).await?;
        let from_id = resolve_uuid_async(&p.from_id, &self.runtime, token).await?;
        let raw_kind = p.kind.as_deref().unwrap_or("entity");
        let spec = resolve_kind_spec(raw_kind, registry)?;
        let policy = parse_entity_policy(p.strategy.as_deref().unwrap_or("prefer_into"))?;
        let content_strategy =
            parse_content_strategy(p.content_strategy.as_deref().unwrap_or("append"))?;
        let dry_run = p.dry_run.unwrap_or(false);

        let summary: MergeSummary = match spec {
            KindSpec::Entity { specific } => {
                ensure_entity_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_entity_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                self.runtime
                    .merge_entity(token, into_id, from_id, policy, dry_run)
                    .await?
            }
            KindSpec::Note { specific } => {
                ensure_note_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_note_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                self.runtime
                    .merge_note(token, into_id, from_id, policy, content_strategy, dry_run)
                    .await?
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "merge(kind=\"edge\") is unsupported".into(),
                ))
            }
            KindSpec::Event => return Err(immutable_event_error()),
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "proposal events are immutable and cannot be merged".into(),
                ))
            }
        };
        to_json(&summary)
    }

    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).min(100);
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                // When a properties filter is active, pull extra candidates so
                // that filtering doesn't leave fewer results than `limit`.
                let props_filter = p.properties.as_ref().and_then(|v| {
                    if v.as_object().is_some_and(|m| !m.is_empty()) {
                        Some(v)
                    } else {
                        None
                    }
                });
                let search_limit = if props_filter.is_some() {
                    (limit * 4).min(100)
                } else {
                    limit
                };
                let hits = self
                    .runtime
                    .hybrid_search(
                        token,
                        &p.query,
                        None,
                        search_limit,
                        kind_filter.as_deref(),
                        p.entity_type.as_deref(),
                    )
                    .await?;

                // Fetch entity metadata for every hit so the response can carry
                // `entity_kind` per #160. When a properties filter is also active
                // (#163), reuse the same fetch for filtering.
                let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();
                let entity_meta: HashMap<Uuid, (String, Option<Value>)> = if candidate_ids
                    .is_empty()
                {
                    HashMap::new()
                } else {
                    let entities_page = self
                        .runtime
                        .entities(token)?
                        .query_entities(
                            token.namespace().as_str(),
                            EntityFilter {
                                ids: candidate_ids,
                                ..EntityFilter::default()
                            },
                            PageRequest {
                                offset: 0u64,
                                limit: hits.len() as u32,
                            },
                        )
                        .await
                        .map_err(RuntimeError::Storage)?;
                    entities_page
                        .items
                        .into_iter()
                        .map(|e| (e.id, (e.kind, e.properties)))
                        .collect()
                };

                // Apply properties post-filter if requested.
                let filtered_hits = if let Some(pf) = props_filter {
                    hits.into_iter()
                        .filter(|h| {
                            entity_meta
                                .get(&h.entity_id)
                                .is_some_and(|(_, props)| props_match(props.as_ref(), pf))
                        })
                        .take(limit as usize)
                        .collect::<Vec<_>>()
                } else {
                    hits
                };

                let result: Vec<Value> = filtered_hits
                    .iter()
                    .map(|h| {
                        // #160: include entity_kind so agents can distinguish hit
                        // kinds without an extra get() call.
                        let entity_kind = entity_meta.get(&h.entity_id).map(|(k, _)| k.as_str());
                        serde_json::json!({
                            "id": h.entity_id.to_string(),
                            "entity_kind": entity_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let hits = self
                    .runtime
                    .search_notes(
                        token,
                        &p.query,
                        None,
                        limit,
                        kind_filter.as_deref(),
                        p.include_superseded.unwrap_or(false),
                    )
                    .await?;

                // #160 (note half): fetch note records so the response can
                // carry note_kind. NoteSearchHit doesn't expose it directly.
                let note_kinds: HashMap<Uuid, String> = if hits.is_empty() {
                    HashMap::new()
                } else {
                    let note_store = self.runtime.notes(token)?;
                    let mut map = HashMap::new();
                    for h in &hits {
                        if let Ok(Some(n)) = note_store.get_note(h.note_id).await {
                            map.insert(h.note_id, n.kind);
                        }
                    }
                    map
                };

                let result: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "id": h.note_id.to_string(),
                            "note_kind": note_kinds.get(&h.note_id),
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Edge => Err(RuntimeError::InvalidInput(
                "search does not support kind=edge — use `list(kind=\"edge\", ...)` for edge browsing".into(),
            )),
            KindSpec::Event => Err(RuntimeError::InvalidInput(
                "search does not support kind=event — use `list(kind=\"event\", ...)` for event browsing".into(),
            )),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "search does not support kind=proposal — use `list(kind=\"proposal\", ...)` for proposal browsing".into(),
            )),
        }
    }

    pub(crate) async fn handle_link(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let verbose = p.verbose.unwrap_or(false);

        if let Some(entries) = p.links {
            let attempted = entries.len();
            if attempted > 1000 {
                return Err(RuntimeError::InvalidInput(
                    "bulk link limited to 1000 entries per request".into(),
                ));
            }
            let atomic = p.atomic.unwrap_or(true);
            if atomic {
                let mut specs = Vec::with_capacity(attempted);
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                for entry in entries {
                    let source = resolve_uuid_async(&entry.source_id, &self.runtime, token).await?;
                    let target = resolve_uuid_async(&entry.target_id, &self.runtime, token).await?;
                    let relation = parse_relation(&entry.relation)?;
                    let (source, target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{source}::{target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = entry.weight.unwrap_or(1.0).clamp(0.0, 1.0);
                    let metadata = merge_entry_metadata(entry.metadata, entry.dependency_kind)?;
                    specs.push(LinkSpec {
                        namespace: Some(token.namespace().as_str().to_owned()),
                        source_id: source,
                        target_id: target,
                        relation,
                        weight,
                        metadata,
                    });
                }
                let edges = self.runtime.link_many(specs).await?;
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": edges.len(),
                    "skipped": skipped,
                    "failed": 0,
                });
                if verbose {
                    resp["edges"] = serde_json::to_value(&edges)
                        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                }
                return to_json(&resp);
            } else {
                let mut results: Vec<Value> = Vec::new();
                let mut error_list: Vec<Value> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                for (idx, entry) in entries.into_iter().enumerate() {
                    let source =
                        match resolve_uuid_async(&entry.source_id, &self.runtime, token).await {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let target =
                        match resolve_uuid_async(&entry.target_id, &self.runtime, token).await {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let relation = match parse_relation(&entry.relation) {
                        Ok(r) => r,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    let (source, target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{source}::{target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = entry.weight.unwrap_or(1.0).clamp(0.0, 1.0);
                    let metadata = match merge_entry_metadata(entry.metadata, entry.dependency_kind)
                    {
                        Ok(m) => m,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    match self
                        .runtime
                        .link(token, source, target, relation, weight, metadata)
                        .await
                    {
                        Ok(edge) => results.push(to_json(&edge)?),
                        Err(e) => error_list.push(json!({"index": idx, "error": format!("{e}")})),
                    }
                }
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": results.len(),
                    "skipped": skipped,
                    "failed": error_list.len(),
                    "errors": error_list,
                });
                if verbose {
                    resp["edges"] = serde_json::Value::Array(results);
                }
                return to_json(&resp);
            }
        }

        // Singleton path.
        let source_id_str = p.source_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires source_id (or links for bulk)".into())
        })?;
        let target_id_str = p.target_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires target_id (or links for bulk)".into())
        })?;
        let relation_str = p.relation.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires relation (or links for bulk)".into())
        })?;
        let source = resolve_uuid_async(&source_id_str, &self.runtime, token).await?;
        let target = resolve_uuid_async(&target_id_str, &self.runtime, token).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = parse_relation(&relation_str)?;
        let metadata = merge_entry_metadata(p.metadata, p.dependency_kind)?;
        let edge = self
            .runtime
            .link(token, source, target, relation, weight, metadata)
            .await?;
        let raw = to_json(&edge)?;
        Ok(format_edge_output(raw, verbose))
    }

    pub(crate) async fn handle_neighbors(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let hits = self
            .runtime
            .neighbors_with_query(
                token,
                node_id,
                NeighborQuery {
                    direction,
                    relations,
                    limit: p.limit,
                    min_weight: p.min_weight,
                },
            )
            .await?;
        to_json(&hits)
    }

    pub(crate) async fn handle_traverse(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(resolve_uuid_async(s, &self.runtime, token).await?);
        }
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let options = TraversalOptions {
            max_depth: p.max_depth.unwrap_or(3),
            direction,
            relations,
            min_weight: p.min_weight,
            limit: p.limit,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
        };
        let paths = self.runtime.traverse(token, request).await?;
        to_json(&paths)
    }

    pub(crate) async fn handle_query(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: QueryParams = deser(params)?;
        let result = self.runtime.query_with_metadata(token, &p.query).await?;
        to_json(&result)
    }

    // ---- Proposal verbs (ADR-046) ----

    /// `propose` — commissive verb. Emits a `ProposalCreated` event and inserts
    /// a row into the `proposals_open` projection table.
    pub(crate) async fn handle_propose(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ProposeParams = deser(params)?;
        if p.title.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'title'".into(),
            ));
        }
        if p.description.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'description'".into(),
            ));
        }

        let _changeset: ProposalChangeset = serde_json::from_value(p.changeset.clone())
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid changeset: {e}")))?;

        let proposal_id = Uuid::new_v4();
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();
        let now = chrono::Utc::now().timestamp_micros();

        let payload = ProposalCreatedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            proposer: actor.clone(),
            title: p.title.clone(),
            description: p.description.clone(),
            changeset: _changeset,
            reviewers: p.reviewers.clone(),
            expiry: p
                .expiry
                .map(|v| khive_types::Timestamp::from_micros(v as u64)),
            parent_id: p
                .parent_id
                .as_deref()
                .map(|s| {
                    Uuid::from_str(s)
                        .map(|u| khive_types::Id128::from_u128(u.as_u128()))
                        .map_err(|e| {
                            RuntimeError::InvalidInput(format!("invalid parent_id {s:?}: {e}"))
                        })
                })
                .transpose()?,
        };

        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize proposal payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "propose",
            EventKind::ProposalCreated,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let event_store = self.runtime.events(token)?;
        event_store
            .append_event(event)
            .await
            .map_err(RuntimeError::Storage)?;

        let expiry_val = p.expiry;
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "\
                    INSERT INTO proposals_open \
                        (proposal_id, namespace, proposer, title, status, \
                         created_at, updated_at, expiry) \
                    VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                    SqlValue::Text(actor.clone()),
                    SqlValue::Text(p.title.clone()),
                    SqlValue::Integer(now),
                    match expiry_val {
                        Some(v) => SqlValue::Integer(v),
                        None => SqlValue::Null,
                    },
                ],
                label: Some("proposals_open.insert".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "open",
            "proposer": actor,
            "title": p.title,
        }))
    }

    /// `review` — declaration verb. Emits a `ProposalReviewed` event and updates
    /// the `proposals_open` projection table (counts, status, last_decision).
    pub(crate) async fn handle_review(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ReviewParams = deser(params)?;
        let proposal_id = Uuid::from_str(&p.proposal_id).map_err(|e| {
            RuntimeError::InvalidInput(format!("invalid proposal_id {:?}: {e}", p.proposal_id))
        })?;
        // Actor is always the authenticated token identity — client cannot override.
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();
        let now = chrono::Utc::now().timestamp_micros();

        let decision: ProposalDecision = match p.decision.trim().to_ascii_lowercase().as_str() {
            "approve" => ProposalDecision::Approve,
            "reject" => ProposalDecision::Reject,
            "comment" => ProposalDecision::Comment,
            "request_changes" | "requestchanges" => ProposalDecision::RequestChanges,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown decision {other:?}; valid: approve | reject | comment | request_changes"
                )));
            }
        };

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        if matches!(current_status, "applied" | "withdrawn" | "rejected") {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status} and cannot be reviewed",
                p.proposal_id
            )));
        }

        // Self-approval guard: the proposer cannot approve their own proposal.
        // Exception: OSS local mode (`actor == "local"`) operates as a single-user
        // system where every operation runs under the same anonymous identity, so
        // the guard would unconditionally block all approvals. Skip it in that case.
        // Multi-actor deployments (where distinct actor IDs are assigned) enforce
        // the guard normally.
        if decision == ProposalDecision::Approve && actor == proposer && actor != "local" {
            return Err(RuntimeError::InvalidInput(format!(
                "self-approval is forbidden: proposer {actor:?} cannot approve their own proposal"
            )));
        }

        let payload = ProposalReviewedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            reviewer: actor.clone(),
            decision,
            comment: p.comment.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize review payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "review",
            EventKind::ProposalReviewed,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let event_store = self.runtime.events(token)?;
        event_store
            .append_event(event)
            .await
            .map_err(RuntimeError::Storage)?;

        let (new_status, approve_delta, reject_delta) = match decision {
            ProposalDecision::Approve => ("approved", 1i64, 0i64),
            ProposalDecision::Reject => ("rejected", 0, 1),
            ProposalDecision::Comment => (current_status, 0, 0),
            ProposalDecision::RequestChanges => ("changes_requested", 0, 0),
        };

        let last_decision_json = serde_json::to_string(&decision)
            .map_err(|e| RuntimeError::Internal(format!("serialize decision: {e}")))?;

        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = ?1, updated_at = ?2, last_decision = ?3, \
                          review_count = review_count + 1, \
                          approve_count = approve_count + ?4, \
                          reject_count = reject_count + ?5 \
                      WHERE proposal_id = ?6 AND namespace = ?7"
                    .to_string(),
                params: vec![
                    SqlValue::Text(new_status.to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(last_decision_json),
                    SqlValue::Integer(approve_delta),
                    SqlValue::Integer(reject_delta),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("proposals_open.update_review".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "reviewer": actor,
            "decision": p.decision,
            "status": new_status,
        }))
    }

    /// `withdraw` — commissive verb. Emits a `ProposalWithdrawn` event and updates
    /// the `proposals_open` projection table to status='withdrawn'.
    pub(crate) async fn handle_withdraw(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: WithdrawParams = deser(params)?;
        let proposal_id = Uuid::from_str(&p.proposal_id).map_err(|e| {
            RuntimeError::InvalidInput(format!("invalid proposal_id {:?}: {e}", p.proposal_id))
        })?;
        // Actor is always the authenticated token identity — client cannot override.
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();
        let now = chrono::Utc::now().timestamp_micros();

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get_for_withdraw".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if actor != proposer {
            return Err(RuntimeError::InvalidInput(format!(
                "only the original proposer {proposer:?} may withdraw this proposal"
            )));
        }

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        if matches!(current_status, "applied" | "withdrawn") {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status}",
                p.proposal_id
            )));
        }

        let payload = ProposalWithdrawnPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            by: actor.clone(),
            reason: p.rationale.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize withdraw payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "withdraw",
            EventKind::ProposalWithdrawn,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let event_store = self.runtime.events(token)?;
        event_store
            .append_event(event)
            .await
            .map_err(RuntimeError::Storage)?;

        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'withdrawn', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("proposals_open.withdraw".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "withdrawn",
            "by": actor,
        }))
    }

    /// `list(kind=proposal)` — assertive verb. Queries the `proposals_open`
    /// projection table with optional status / proposer filters.
    pub(crate) async fn handle_list_proposals(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ListProposalsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))?;
        let ns = token.namespace().as_str().to_owned();
        let limit = p.limit.unwrap_or(50).min(500) as i64;
        let offset = p.offset.unwrap_or(0) as i64;

        let mut sql_str = "\
            SELECT proposal_id, proposer, title, status, created_at, updated_at, \
                   expiry, last_decision, review_count, approve_count, reject_count \
            FROM proposals_open \
            WHERE namespace = ?1"
            .to_string();
        let mut sql_params: Vec<SqlValue> = vec![SqlValue::Text(ns)];
        let mut param_idx = 2usize;

        if let Some(status) = &p.status {
            sql_str.push_str(&format!(" AND status = ?{param_idx}"));
            sql_params.push(SqlValue::Text(status.clone()));
            param_idx += 1;
        }
        if let Some(proposer) = &p.proposer {
            sql_str.push_str(&format!(" AND proposer = ?{param_idx}"));
            sql_params.push(SqlValue::Text(proposer.clone()));
            param_idx += 1;
        }

        sql_str.push_str(&format!(
            " ORDER BY updated_at DESC LIMIT ?{param_idx} OFFSET ?{}",
            param_idx + 1
        ));
        sql_params.push(SqlValue::Integer(limit));
        sql_params.push(SqlValue::Integer(offset));

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let rows = reader
            .query_all(SqlStatement {
                sql: sql_str,
                params: sql_params,
                label: Some("proposals_open.list".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        let items: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let get_text = |name: &str| -> String {
                    row.get(name)
                        .and_then(|v| {
                            if let SqlValue::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                };
                let get_int = |name: &str| -> Option<i64> {
                    row.get(name).and_then(|v| {
                        if let SqlValue::Integer(i) = v {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                };
                serde_json::json!({
                    "proposal_id": get_text("proposal_id"),
                    "proposer": get_text("proposer"),
                    "title": get_text("title"),
                    "status": get_text("status"),
                    "created_at": get_int("created_at"),
                    "updated_at": get_int("updated_at"),
                    "expiry": get_int("expiry"),
                    "last_decision": get_text("last_decision"),
                    "review_count": get_int("review_count").unwrap_or(0),
                    "approve_count": get_int("approve_count").unwrap_or(0),
                    "reject_count": get_int("reject_count").unwrap_or(0),
                })
            })
            .collect();

        to_json(&items)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_relation, UpdateParams};
    use serde_json::json;

    // F009 (CRIT): error text must be derived from EdgeRelation::ALL, not a hardcoded list.
    // ADR-002 mandates 15 relations; error text must include derived_from and precedes.
    #[test]
    fn parse_relation_error_lists_all_relations() {
        let err = parse_relation("not_a_relation").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("derived_from"),
            "F009: parse_relation error must list derived_from (ADR-002); got: {msg}"
        );
        assert!(
            msg.contains("precedes"),
            "F009: parse_relation error must list precedes (ADR-002); got: {msg}"
        );
    }

    // ADR-014: wire-level tri-state nullable f64 for `update`.
    //   absent  → outer None (preserve existing value)
    //   null    → Some(None) (clear the value)
    //   number  → Some(Some(v)) (set to v)
    //
    // Regression for round-3 finding: the previous `Option<Value>` representation
    // collapsed absent and null into the same `None`, so JSON null could not
    // distinguish "clear" from "preserve" through the MCP wire surface.
    #[test]
    fn update_params_tri_state_salience() {
        let absent: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
        assert_eq!(
            absent.salience, None,
            "absent salience key must deserialize to outer None (preserve)"
        );

        let cleared: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "salience": null})).unwrap();
        assert_eq!(
            cleared.salience,
            Some(None),
            "salience=null must deserialize to Some(None) (clear)"
        );

        let set: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "salience": 0.5})).unwrap();
        assert_eq!(
            set.salience,
            Some(Some(0.5)),
            "salience=0.5 must deserialize to Some(Some(0.5)) (set)"
        );
    }

    #[test]
    fn update_params_tri_state_decay_factor() {
        let absent: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
        assert_eq!(
            absent.decay_factor, None,
            "absent decay_factor key must deserialize to outer None (preserve)"
        );

        let cleared: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": null}))
                .unwrap();
        assert_eq!(
            cleared.decay_factor,
            Some(None),
            "decay_factor=null must deserialize to Some(None) (clear)"
        );

        let set: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": 0.6}))
                .unwrap();
        assert_eq!(
            set.decay_factor,
            Some(Some(0.6)),
            "decay_factor=0.6 must deserialize to Some(Some(0.6)) (set)"
        );
    }

    // ADR-046: resolve_kind_spec must recognise "proposal" as KindSpec::Proposal
    #[test]
    fn resolve_kind_spec_proposal() {
        use super::{resolve_kind_spec, KindSpec};
        use crate::KgPack;
        use khive_runtime::VerbRegistryBuilder;

        let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let spec = resolve_kind_spec("proposal", &registry).expect("should resolve proposal");
        assert_eq!(
            spec,
            KindSpec::Proposal,
            "kind=proposal must resolve to KindSpec::Proposal"
        );

        let spec_upper =
            resolve_kind_spec("Proposal", &registry).expect("should be case-insensitive");
        assert_eq!(
            spec_upper,
            KindSpec::Proposal,
            "kind=Proposal (mixed case) must resolve"
        );
    }

    // ADR-046: propose param deserialization
    #[test]
    fn propose_params_deserialization() {
        use super::ProposeParams;
        let p: ProposeParams = serde_json::from_value(json!({
            "title": "Add RoPE",
            "description": "Add RoPE entity to the graph",
            "changeset": {
                "kind": "add_entity",
                "entity": "{\"kind\":\"concept\",\"name\":\"RoPE\"}"
            },
            "reviewers": ["alice"],
        }))
        .expect("ProposeParams must deserialize");
        assert_eq!(p.title, "Add RoPE");
        assert_eq!(p.reviewers, vec!["alice"]);
        assert!(p.parent_id.is_none());
        assert!(p.expiry.is_none());
    }

    // ADR-046: review param deserialization with all valid decisions
    #[test]
    fn review_params_decisions() {
        use super::ReviewParams;
        for decision in ["approve", "reject", "comment", "request_changes"] {
            let p: ReviewParams = serde_json::from_value(json!({
                "proposal_id": "00000000-0000-0000-0000-000000000001",
                "decision": decision,
            }))
            .expect("ReviewParams must deserialize");
            assert_eq!(p.decision, decision);
        }
    }

    // CRIT-2 regression: ReviewParams must not accept an `actor` field.
    // The actor is always derived from the NamespaceToken at dispatch time.
    // If a client passes actor=<other_id>, the field is ignored (unknown fields
    // are allowed by serde default, so the struct simply lacks the field).
    #[test]
    fn review_params_no_actor_field() {
        use super::ReviewParams;
        // Baseline: ReviewParams works without actor.
        let p: ReviewParams = serde_json::from_value(json!({
            "proposal_id": "00000000-0000-0000-0000-000000000001",
            "decision": "approve",
        }))
        .expect("ReviewParams must deserialize without actor");
        assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(p.decision, "approve");
    }

    // CRIT-2 regression: WithdrawParams must not accept an `actor` field.
    #[test]
    fn withdraw_params_no_actor_field() {
        use super::WithdrawParams;
        let p: WithdrawParams = serde_json::from_value(json!({
            "proposal_id": "00000000-0000-0000-0000-000000000002",
        }))
        .expect("WithdrawParams must deserialize without actor");
        assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000002");
        assert!(p.rationale.is_none());
    }

    // CRIT-2 regression: ProposeParams must not accept an `actor` field.
    #[test]
    fn propose_params_no_actor_field() {
        use super::ProposeParams;
        let p: ProposeParams = serde_json::from_value(json!({
            "title": "Fix RoPE",
            "description": "Fix RoPE entity",
            "changeset": {"kind": "add_entity", "entity": "{}"},
        }))
        .expect("ProposeParams must deserialize without actor");
        assert_eq!(p.title, "Fix RoPE");
    }

    // ADR-046: KG pack must expose exactly 14 handlers including propose/review/withdraw
    #[test]
    fn kg_pack_exposes_14_handlers() {
        use crate::KgPack;
        use khive_types::Pack;
        let handlers = KgPack::HANDLERS;
        assert_eq!(
            handlers.len(),
            14,
            "ADR-046: kg pack must expose 14 handlers (was 11, +3 for propose/review/withdraw)"
        );
        let names: Vec<&str> = handlers.iter().map(|h| h.name).collect();
        assert!(names.contains(&"propose"), "propose must be in KG_HANDLERS");
        assert!(names.contains(&"review"), "review must be in KG_HANDLERS");
        assert!(
            names.contains(&"withdraw"),
            "withdraw must be in KG_HANDLERS"
        );
    }
}
