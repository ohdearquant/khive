//! Verb handlers for the KG pack.
//!
//! Each handler: deserialize params from Value → validate → call runtime → serialize result.

use std::collections::HashMap;
use std::str::FromStr;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    EdgeListFilter, EntityPatch, KhiveRuntime, MergeStrategy, NamespaceToken, RuntimeError,
    VerbRegistry,
};
use khive_storage::types::{
    Direction, NeighborQuery, PageRequest, TraversalOptions, TraversalRequest,
};
use khive_storage::{EdgeRelation, EntityFilter, EventFilter, EventOutcome, SubstrateKind};

use crate::vocab::{EntityKind, NoteKind};
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
}

impl KindSpec {
    pub(crate) fn substrate_label(&self) -> &'static str {
        match self {
            KindSpec::Entity { .. } => "entity",
            KindSpec::Note { .. } => "note",
            KindSpec::Edge => "edge",
            KindSpec::Event => "event",
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
}

#[derive(Deserialize)]
struct UpdateParams {
    id: String,
    name: Option<String>,
    description: Option<Value>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    relation: Option<String>,
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct DeleteParams {
    id: String,
    hard: Option<bool>,
}

#[derive(Deserialize)]
struct MergeParams {
    into_id: String,
    from_id: String,
    strategy: Option<String>,
}

#[derive(Deserialize)]
struct SearchParams {
    kind: String,
    query: String,
    limit: Option<u32>,
    entity_kind: Option<String>,
    note_kind: Option<String>,
    properties: Option<Value>,
}

#[derive(Deserialize)]
struct LinkParams {
    source_id: String,
    target_id: String,
    relation: String,
    weight: Option<f64>,
    /// When `true`, output uses full UUIDs and ISO 8601 timestamps instead of
    /// the default 8-char short IDs and YYYY/MM/DD date format.
    verbose: Option<bool>,
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

fn parse_relation(s: &str) -> Result<EdgeRelation, RuntimeError> {
    s.parse::<EdgeRelation>().map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "unknown relation {s:?}; valid: contains | part_of | instance_of | extends | \
             variant_of | introduced_by | supersedes | depends_on | enables | implements | \
             competes_with | composed_with | annotates"
        ))
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

    Ok((
        EventFilter {
            verbs,
            substrates,
            actors: p.actor.clone().into_iter().collect(),
            after: p.since,
            before: p.until,
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
                        "kind=entity requires a specific kind: either kind=<concept|document|dataset|project|person|org> directly, or kind=entity + entity_kind=<…>".into(),
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
                    KindSpec::Edge | KindSpec::Event => {}
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
                let salience = p.salience.unwrap_or(0.5);
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
                        salience,
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
                    .list_entities(token, kind_filter.as_deref(), limit, offset)
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
                            .list_events(token, filter.clone(), batch_size, raw_offset)
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
                        .list_events(token, filter, limit, offset)
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
    ) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;

        if self
            .runtime
            .events(token)?
            .get_event(id)
            .await
            .map_err(RuntimeError::Storage)?
            .is_some()
        {
            return Err(immutable_event_error());
        }

        if self.runtime.get_entity(token, id).await.is_ok() {
            let description = match p.description {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s)),
                Some(other) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "description must be null or a string, got: {other}"
                    )))
                }
            };
            let patch = EntityPatch {
                name: p.name,
                description,
                properties: p.properties,
                tags: p.tags,
            };
            let entity = self.runtime.update_entity(token, id, patch).await?;
            return to_json(&entity);
        }

        if self.runtime.get_edge(token, id).await?.is_some() {
            let relation = p.relation.as_deref().map(parse_relation).transpose()?;
            let edge = self
                .runtime
                .update_edge(token, id, relation, p.weight)
                .await?;
            return to_json(&edge);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_delete(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;

        if self
            .runtime
            .events(token)?
            .get_event(id)
            .await
            .map_err(RuntimeError::Storage)?
            .is_some()
        {
            return Err(immutable_event_error());
        }

        if self.runtime.get_entity(token, id).await.is_ok() {
            let deleted = self
                .runtime
                .delete_entity(token, id, p.hard.unwrap_or(false))
                .await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        if self.runtime.get_edge(token, id).await?.is_some() {
            let deleted = self.runtime.delete_edge(token, id).await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        let deleted_note = self
            .runtime
            .delete_note(token, id, p.hard.unwrap_or(false))
            .await?;
        if deleted_note {
            return to_json(&serde_json::json!({ "deleted": true, "id": p.id }));
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_merge(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        let into_id = resolve_uuid_async(&p.into_id, &self.runtime, token).await?;
        let from_id = resolve_uuid_async(&p.from_id, &self.runtime, token).await?;
        let strategy = match p.strategy.as_deref().unwrap_or("prefer_into") {
            "prefer_into" => MergeStrategy::PreferInto,
            "prefer_from" => MergeStrategy::PreferFrom,
            "union" => MergeStrategy::Union,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown strategy {other:?}; use prefer_into | prefer_from | union"
                )))
            }
        };
        let summary = self
            .runtime
            .merge_entity(token, into_id, from_id, strategy)
            .await?;
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
        }
    }

    pub(crate) async fn handle_link(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let verbose = p.verbose.unwrap_or(false);
        let source = resolve_uuid_async(&p.source_id, &self.runtime, token).await?;
        let target = resolve_uuid_async(&p.target_id, &self.runtime, token).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = parse_relation(&p.relation)?;
        let edge = self
            .runtime
            .link(token, source, target, relation, weight)
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
}
