//! Verb handlers for the KG pack.
//!
//! Each handler: deserialize params from Value → validate → call runtime → serialize result.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    EdgeListFilter, EntityPatch, KhiveRuntime, MergeStrategy, RuntimeError, VerbRegistry,
};
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};
use khive_storage::{EdgeRelation, EntityFilter};

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
}

impl KindSpec {
    pub(crate) fn substrate_label(&self) -> &'static str {
        match self {
            KindSpec::Entity { .. } => "entity",
            KindSpec::Note { .. } => "note",
            KindSpec::Edge => "edge",
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

    let mut all: Vec<String> = vec!["entity".into(), "note".into(), "edge".into()];
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
    namespace: Option<String>,
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
    namespace: Option<String>,
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    kind: String,
    namespace: Option<String>,
    limit: Option<u32>,
    entity_kind: Option<String>,
    source_id: Option<String>,
    target_id: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    max_weight: Option<f64>,
    note_kind: Option<String>,
}

#[derive(Deserialize)]
struct UpdateParams {
    namespace: Option<String>,
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
    namespace: Option<String>,
    id: String,
    hard: Option<bool>,
}

#[derive(Deserialize)]
struct MergeParams {
    namespace: Option<String>,
    into_id: String,
    from_id: String,
    strategy: Option<String>,
}

#[derive(Deserialize)]
struct SearchParams {
    kind: String,
    namespace: Option<String>,
    query: String,
    limit: Option<u32>,
    entity_kind: Option<String>,
    note_kind: Option<String>,
}

#[derive(Deserialize)]
struct LinkParams {
    namespace: Option<String>,
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
    namespace: Option<String>,
    node_id: String,
    direction: Option<String>,
    limit: Option<u32>,
    relations: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TraverseParams {
    namespace: Option<String>,
    roots: Vec<String>,
    max_depth: Option<usize>,
    direction: Option<String>,
    relations: Option<Vec<String>>,
    include_roots: Option<bool>,
}

#[derive(Deserialize)]
struct QueryParams {
    namespace: Option<String>,
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
    namespace: Option<&str>,
) -> Result<Uuid, RuntimeError> {
    // Use EntityFilter.name_prefix with the full name to do an exact match.
    // The DB implements `name LIKE '?%'` so we get back all names that start
    // with `name`. We then filter to exact (case-insensitive) matches.
    let filter = EntityFilter {
        name_prefix: Some(name.to_string()),
        ..Default::default()
    };
    let page = runtime
        .entities(namespace)?
        .query_entities(
            runtime.ns(namespace),
            filter,
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
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
    namespace: Option<&str>,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix(namespace, s).await {
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
    resolve_name_async(s, runtime, namespace).await
}

// ---- Output formatting helpers (issue #66) ----

/// Truncate a UUID string to 8 characters for compact display.
fn short_id(full_uuid: &str) -> &str {
    if full_uuid.len() >= 8 {
        &full_uuid[..8]
    } else {
        full_uuid
    }
}

/// Format a `DateTime<Utc>` as YYYY/MM/DD for compact display.
fn format_datetime(dt: &DateTime<Utc>) -> String {
    dt.format("%Y/%m/%d").to_string()
}

/// Post-process a serialized edge JSON to use compact IDs and dates by default.
///
/// When `verbose = false` (default):
/// - UUID fields (`id`, `source_id`, `target_id`) → 8-char short IDs.
/// - `created_at` (ISO 8601 string from `DateTime<Utc>`) → YYYY/MM/DD.
///
/// When `verbose = true`: returns the value unchanged.
fn format_edge_output(mut v: Value, verbose: bool) -> Value {
    if verbose {
        return v;
    }
    if let Some(obj) = v.as_object_mut() {
        for key in &["id", "source_id", "target_id"] {
            if let Some(val) = obj.get_mut(*key) {
                if let Some(s) = val.as_str() {
                    *val = json!(short_id(s));
                }
            }
        }
        if let Some(created_at) = obj.get_mut("created_at") {
            if let Some(s) = created_at.as_str() {
                // Edge.created_at serializes as ISO 8601 via serde.
                if let Ok(dt) = s.parse::<DateTime<Utc>>() {
                    *created_at = json!(format_datetime(&dt));
                }
            }
        }
    }
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

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ---- Handler implementations ----

impl KgPack {
    pub(crate) async fn handle_create(
        &self,
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
                    KindSpec::Edge => {}
                }
            }
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
                        p.namespace.as_deref(),
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
                    annotates
                        .push(resolve_uuid_async(&s, &self.runtime, p.namespace.as_deref()).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        p.namespace.as_deref(),
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

    pub(crate) async fn handle_get(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if let Some(entity) = self.runtime.get_entity(ns, id).await? {
            return to_json(&serde_json::json!({"kind": "entity", "data": entity}));
        }

        if let Some(note) = self
            .runtime
            .notes(ns)?
            .get_note(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if note.namespace == self.runtime.ns(ns) {
                return to_json(&serde_json::json!({"kind": "note", "data": note}));
            }
        }

        if let Some(edge) = self.runtime.get_edge(ns, id).await? {
            return to_json(&serde_json::json!({"kind": "edge", "data": edge}));
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_list(
        &self,
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
                let entities = self
                    .runtime
                    .list_entities(p.namespace.as_deref(), kind_filter.as_deref(), limit)
                    .await?;
                to_json(&entities)
            }
            KindSpec::Edge => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => {
                        Some(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?)
                    }
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => {
                        Some(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?)
                    }
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
                let edges = self
                    .runtime
                    .list_edges(p.namespace.as_deref(), filter, limit)
                    .await?;
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
                let notes = self
                    .runtime
                    .list_notes(p.namespace.as_deref(), kind_filter.as_deref(), limit)
                    .await?;
                to_json(&notes)
            }
        }
    }

    pub(crate) async fn handle_update(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if self.runtime.get_entity(ns, id).await?.is_some() {
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
            let entity = self.runtime.update_entity(ns, id, patch).await?;
            return to_json(&entity);
        }

        if self.runtime.get_edge(ns, id).await?.is_some() {
            let relation = p.relation.as_deref().map(parse_relation).transpose()?;
            let edge = self.runtime.update_edge(ns, id, relation, p.weight).await?;
            return to_json(&edge);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_delete(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if self.runtime.get_entity(ns, id).await?.is_some() {
            let deleted = self
                .runtime
                .delete_entity(ns, id, p.hard.unwrap_or(false))
                .await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        if self.runtime.get_edge(ns, id).await?.is_some() {
            let deleted = self.runtime.delete_edge(ns, id).await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        let deleted_note = self
            .runtime
            .delete_note(ns, id, p.hard.unwrap_or(false))
            .await?;
        if deleted_note {
            return to_json(&serde_json::json!({ "deleted": true, "id": p.id }));
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_merge(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        let into_id = resolve_uuid_async(&p.into_id, &self.runtime, p.namespace.as_deref()).await?;
        let from_id = resolve_uuid_async(&p.from_id, &self.runtime, p.namespace.as_deref()).await?;
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
            .merge_entity(p.namespace.as_deref(), into_id, from_id, strategy)
            .await?;
        to_json(&summary)
    }

    pub(crate) async fn handle_search(
        &self,
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
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(self.runtime.embed(&p.query).await?)
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .hybrid_search(
                        p.namespace.as_deref(),
                        &p.query,
                        query_vector,
                        limit,
                        kind_filter.as_deref(),
                    )
                    .await?;
                let result: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "entity_id": h.entity_id.to_string(),
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
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(self.runtime.embed(&p.query).await?)
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .search_notes(
                        p.namespace.as_deref(),
                        &p.query,
                        query_vector,
                        limit,
                        kind_filter.as_deref(),
                    )
                    .await?;
                let result: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "note_id": h.note_id.to_string(),
                            "score": h.score.to_f64(),
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Edge => Err(RuntimeError::InvalidInput(
                "search does not support kind=edge — use `list(kind=\"edge\", ...)` for edge browsing".into(),
            )),
        }
    }

    pub(crate) async fn handle_link(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let verbose = p.verbose.unwrap_or(false);
        let source =
            resolve_uuid_async(&p.source_id, &self.runtime, p.namespace.as_deref()).await?;
        let target =
            resolve_uuid_async(&p.target_id, &self.runtime, p.namespace.as_deref()).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = parse_relation(&p.relation)?;
        let edge = self
            .runtime
            .link(p.namespace.as_deref(), source, target, relation, weight)
            .await?;
        let raw = to_json(&edge)?;
        Ok(format_edge_output(raw, verbose))
    }

    pub(crate) async fn handle_neighbors(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.node_id, &self.runtime, p.namespace.as_deref()).await?;
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
            .neighbors(
                p.namespace.as_deref(),
                node_id,
                direction,
                p.limit,
                relations,
            )
            .await?;
        to_json(&hits)
    }

    pub(crate) async fn handle_traverse(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?);
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
            min_weight: None,
            limit: None,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
        };
        let paths = self
            .runtime
            .traverse(p.namespace.as_deref(), request)
            .await?;
        to_json(&paths)
    }

    pub(crate) async fn handle_query(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: QueryParams = deser(params)?;
        let rows = self.runtime.query(p.namespace.as_deref(), &p.query).await?;
        to_json(&rows)
    }
}
