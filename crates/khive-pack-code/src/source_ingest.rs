//! `code.ingest` L1 (manifest edges) + L1.5 (import-scan edges) + L2
//! (Scanner/Extractor symbol tier, Rust only) core pipeline (ADR-085
//! Amendment 2 B3-B6).
//!
//! Identity (B4): every entity this pipeline creates has a `uuid5`-derived
//! id, so re-ingesting the same path is a pure upsert — no dedup lookups are
//! needed to avoid duplicate rows. Edge ids are likewise `uuid5`-derived from
//! their endpoints, so re-creating the same edge is also an idempotent
//! upsert. B6 cross-repo resolution and B5 staleness stamping are both
//! driven off this determinism: an unresolved specifier records only the
//! information needed to recompute its target's id later, and the
//! synchronous re-resolve pass (`reresolve_pass`) does exactly that.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Edge, Entity, LinkId};
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extractor::{self, DeclKind};
use crate::imports::{self, Resolved};
use crate::ingest::CODE_INGEST_NAMESPACE;
use crate::manifest;
use crate::scanner_rust;

/// SQLite `SQLITE_MAX_VARIABLE_NUMBER` defaults to 999; chunk at 900 to stay
/// safe (precedent: `khive-db/src/stores/graph.rs`).
const SQL_ID_CHUNK: usize = 900;

/// Identity of one L2 declaration within a single language pass's
/// project-wide symbol index: the project it belongs to, its full module
/// path, its declared name, and its declaration kind -- together unique
/// enough that two same-named declarations in different modules (or of
/// different kinds) never collapse onto one entry. Named to replace the
/// previous anonymous `(String, String, String, DeclKind)` tuple, whose
/// three indistinguishable `String` positions made every construction site
/// order-dependent and unreadable at a glance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolKey {
    source_project: String,
    module_path: String,
    name: String,
    kind: DeclKind,
}

/// Outcome counters for one `code.ingest` call, mirroring `git.digest`'s
/// `IngestReport` shape (ADR-088 Amendment 1 precedent).
#[derive(Debug, Default, serde::Serialize)]
pub struct CodeSourceIngestReport {
    pub projects_created: u64,
    pub projects_updated: u64,
    pub modules_created: u64,
    pub modules_updated: u64,
    /// L2 symbol-tier declaration entities (ADR-085 Amendment 2 B2-B3),
    /// zero when `enable_l2` is false.
    pub symbols_created: u64,
    pub symbols_updated: u64,
    pub edges_created: u64,
    pub edges_updated: u64,
    pub unresolved_recorded: u64,
    pub unresolved_resolved: u64,
    /// L2 call-target / impl-target names that did not resolve against the
    /// same `source_project`'s declaration set (a same-project coverage
    /// floor, analogous to L1.5's unresolved-specifier queue but without a
    /// deferred re-resolve pass in this slice — see ADR-085 PR body).
    pub symbol_dependencies_unresolved: u64,
    /// L2-derived `depends_on`/`implements` edges whose `last_seen_at` was
    /// stamped to this sweep's time because the current scan re-resolved
    /// them (B5 extended to L2 edges — see `resolved_edges` below). An edge
    /// whose source declaration was re-scanned but which no longer resolves
    /// is never counted here, never deleted, and never otherwise mutated:
    /// its `last_seen_at` simply keeps its last-resolved value, exactly like
    /// B5's entity staleness rule.
    pub symbol_edges_stamped: u64,
    pub languages: Vec<String>,
    /// Per-manifest / per-file failures that did not abort the pass (fail
    /// loud without silently dropping the rest of the run).
    pub warnings: Vec<String>,
    pub db_path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CodeSourceIngestError {
    #[error("path {0:?} does not exist or is not a directory")]
    InvalidPath(PathBuf),
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("storage error: {0}")]
    Storage(String),
}

pub struct CodeSourceIngestOptions<'a> {
    pub path: &'a Path,
    pub languages: BTreeSet<&'static str>,
    pub sweep_time: DateTime<Utc>,
    /// Run the L1 manifest-dependency tier: project entities from discovered
    /// manifests plus their declared-dependency edges.
    pub enable_l1: bool,
    /// Run the L1.5 regex import-scan tier: per-file module entities and
    /// import-derived `depends_on` edges. Independent of `enable_l1` and
    /// `enable_l2` (ADR-085 Amendment 2 B3) — each tier runs in isolation
    /// when only it is requested, and tiers compose when several are.
    pub enable_l1_5: bool,
    /// Run the L2 Scanner/Extractor symbol tier (ADR-085 Amendment 2 B2-B3).
    /// Rust (`syn`) is the only Scanner this slice ships; other languages
    /// are silently unaffected when this is `true`.
    pub enable_l2: bool,
}

fn uuid5_json(value: &Value) -> Uuid {
    let bytes = serde_json::to_vec(value).expect("Value always serializes");
    Uuid::new_v5(&CODE_INGEST_NAMESPACE, &bytes)
}

fn project_uuid(source_project: &str) -> Uuid {
    uuid5_json(&json!({
        "kind": "code-source-project",
        "source_project": source_project,
    }))
}

fn module_uuid(source_project: &str, language: &str, module_path: &str) -> Uuid {
    uuid5_json(&json!({
        "kind": "code-source-symbol",
        "source_project": source_project,
        "language": language,
        "module_path": module_path,
        "name": module_path,
        "symbol_kind": "module",
    }))
}

/// B4 identity for an L2 declaration entity: `uuid5` over `(source_project,
/// language, module_path, name, kind)`, `kind` being one of the four
/// canonical D2 tokens — the same `CODE_INGEST_NAMESPACE` seed and shape
/// `module_uuid` uses, with `name` the declaration's own name rather than
/// the module path.
fn symbol_uuid(
    source_project: &str,
    language: &str,
    module_path: &str,
    name: &str,
    code_token: &str,
) -> Uuid {
    uuid5_json(&json!({
        "kind": "code-source-symbol",
        "source_project": source_project,
        "language": language,
        "module_path": module_path,
        "name": name,
        "symbol_kind": code_token,
    }))
}

/// `graph_edges` carries a `UNIQUE(namespace, source_id, target_id, relation)`
/// natural key independent of the row's `id` (khive-db schema.sql), so at
/// most one edge of a given relation can ever exist between an ordered pair
/// regardless of what `id` an upsert names — a second `id` for the "same"
/// pair collapses onto the first row's natural-key conflict arm instead of
/// creating a second row. Edge identity here matches that invariant exactly:
/// no disambiguator. Distinct provenance for the same `depends_on` pair
/// (e.g. a manifest-declared dependency and an import-scan-detected one)
/// is folded into that single edge's `dependency_kinds` metadata (see
/// `merge_dependency_kinds`), not encoded into a second id.
fn edge_uuid(relation: EdgeRelation, source_id: Uuid, target_id: Uuid) -> Uuid {
    uuid5_json(&json!({
        "kind": "code-source-edge",
        "relation": relation.as_str(),
        "source_id": source_id.to_string(),
        "target_id": target_id.to_string(),
    }))
}

/// A `uuid5`-recomputable unresolved reference recorded on a source entity
/// (B6). Content-hash-free by design: only the fields needed to recompute
/// the target's identity and the edge's metadata are kept.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct UnresolvedSpec {
    specifier: String,
    target_kind: String,
    dependency_kind: String,
    language: String,
}

fn read_unresolved(properties: &Value) -> Vec<UnresolvedSpec> {
    properties
        .get("unresolved_specifiers")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

async fn get_entity_opt(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Option<Entity>, CodeSourceIngestError> {
    rt.entities(token)?
        .get_entity(id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))
}

/// Every entity write in this pipeline is a raw `EntityStore::upsert_entity`
/// call rather than `KhiveRuntime::create_entity` (this pack needs
/// `uuid5`-derived, caller-chosen ids for idempotent re-ingest — B4 — which
/// `create_entity` does not support). `create_entity` runs doc-comment text
/// and structured properties through the secret gate before writing (ADR-085
/// D6); a raw store write bypasses that, so this helper re-applies the same
/// checks the runtime's own gated path uses (mirrors
/// `khive-pack-kg`'s `proposal.rs`, which does the same for its own
/// deterministic-id writes).
fn gate_entity(entity: &Entity) -> Result<(), CodeSourceIngestError> {
    khive_runtime::secret_gate::check(&entity.name)?;
    if let Some(d) = entity.description.as_deref() {
        khive_runtime::secret_gate::check(d)?;
    }
    if let Some(p) = entity.properties.as_ref() {
        khive_runtime::secret_gate::check_json(p)?;
    }
    Ok(())
}

async fn upsert_entity(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity: Entity,
) -> Result<(), CodeSourceIngestError> {
    gate_entity(&entity)?;
    rt.entities(token)?
        .upsert_entity(entity)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))
}

/// Batched entity upsert (ADR-085 B5): every declaration-tier write in one
/// call per file instead of one awaited `upsert_entity` per declaration.
async fn upsert_entities_batch(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entities: Vec<Entity>,
) -> Result<(), CodeSourceIngestError> {
    if entities.is_empty() {
        return Ok(());
    }
    for entity in &entities {
        gate_entity(entity)?;
    }
    rt.entities(token)?
        .upsert_entities(entities)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    Ok(())
}

/// Batched edge upsert counterpart to `upsert_entities_batch`.
async fn upsert_edges_batch(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    edges: Vec<Edge>,
) -> Result<(), CodeSourceIngestError> {
    if edges.is_empty() {
        return Ok(());
    }
    rt.graph(token)?
        .upsert_edges(edges)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    Ok(())
}

/// Batched last-seen-at stamp for declarations whose `content_hash` matched
/// the prior sweep (ADR-085 B5): a direct `UPDATE ... WHERE id IN (...)`
/// against only the `last_seen_at` property, so an unchanged declaration
/// gets no entity rewrite (no reindex, no FTS/vector churn) while still
/// satisfying B5's every-sweep staleness stamp.
async fn touch_last_seen_at(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ids: &[Uuid],
    sweep_time: DateTime<Utc>,
) -> Result<(), CodeSourceIngestError> {
    if ids.is_empty() {
        return Ok(());
    }
    use khive_storage::types::{SqlStatement, SqlValue};

    for chunk in ids.chunks(SQL_ID_CHUNK) {
        let placeholders: Vec<String> = (0..chunk.len()).map(|i| format!("?{}", i + 4)).collect();
        let sql = format!(
            "UPDATE entities SET properties = json_set(COALESCE(properties, '{{}}'), \
             '$.last_seen_at', ?1), updated_at = ?2 WHERE namespace = ?3 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut params = vec![
            SqlValue::Text(sweep_time.to_rfc3339()),
            SqlValue::Integer(ts(sweep_time)),
            SqlValue::Text(token.namespace().as_str().to_string()),
        ];
        params.extend(chunk.iter().map(|id| SqlValue::Uuid(*id)));

        let mut writer = rt
            .sql()
            .writer()
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
        writer
            .execute(SqlStatement {
                sql,
                params,
                label: Some("code_ingest_touch_last_seen".into()),
            })
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    }
    Ok(())
}

/// Upserts the edge and returns `true` when it did not previously exist
/// (created) or `false` when an existing row with this id was refreshed
/// (updated) — callers fold this into the report's created/updated counters.
#[allow(clippy::too_many_arguments)]
async fn upsert_edge(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    metadata: Value,
    now: DateTime<Utc>,
) -> Result<bool, CodeSourceIngestError> {
    let link_id = LinkId::from(id);
    let graph = rt.graph(token)?;
    let existed = graph
        .get_edge(link_id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
        .is_some();
    let edge = Edge {
        id: link_id,
        namespace: token.namespace().as_str().to_string(),
        source_id,
        target_id,
        relation,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(metadata),
        target_backend: None,
    };
    graph
        .upsert_edge(edge)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    Ok(!existed)
}

fn ts(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_micros()
}

/// Upsert (create or refresh) the `project` entity for `name`, merging the
/// per-`(source_project, language)` sweep clock (B5) with any prior sweeps
/// for a different language recorded on the same entity.
async fn upsert_project(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    language: &str,
    sweep_time: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<Uuid, CodeSourceIngestError> {
    let id = project_uuid(name);
    let existing = get_entity_opt(rt, token, id).await?;
    let is_new = existing.is_none();

    let mut sweep_clock = existing
        .as_ref()
        .and_then(|e| e.properties.as_ref())
        .and_then(|p| p.get("sweep_clock"))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    sweep_clock.insert(language.to_string(), json!(sweep_time.to_rfc3339()));

    let unresolved = existing
        .as_ref()
        .and_then(|e| e.properties.as_ref())
        .map(read_unresolved)
        .unwrap_or_default();

    let mut props = serde_json::Map::new();
    props.insert("source_project".into(), json!(name));
    props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
    props.insert("sweep_clock".into(), Value::Object(sweep_clock));
    if !unresolved.is_empty() {
        props.insert(
            "unresolved_specifiers".into(),
            serde_json::to_value(&unresolved).expect("serializes"),
        );
    }

    let mut entity = Entity::new(token.namespace().as_str(), "project", name);
    entity.id = id;
    entity.properties = Some(Value::Object(props));
    let now = ts(sweep_time);
    entity.created_at = existing.as_ref().map(|e| e.created_at).unwrap_or(now);
    entity.updated_at = now;
    upsert_entity(rt, token, entity).await?;

    if is_new {
        report.projects_created += 1;
    } else {
        report.projects_updated += 1;
    }
    Ok(id)
}

/// Upserts (or, on an unchanged `content_hash`, no-ops) the module entity for
/// `module_path` and returns `(id, changed)`. `changed = false` means this
/// file's content matched the prior sweep exactly — the caller records `id`
/// for a batched `last_seen_at`-only touch (ADR-085 B5) instead of
/// rewriting the row.
#[allow(clippy::too_many_arguments)]
async fn upsert_module(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    module_path: &str,
    content_hash: &str,
    sweep_time: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<(Uuid, bool, Option<Entity>), CodeSourceIngestError> {
    let id = module_uuid(source_project, language, module_path);
    let existing = get_entity_opt(rt, token, id).await?;
    let is_new = existing.is_none();

    let existing_hash = existing
        .as_ref()
        .and_then(|e| e.properties.as_ref())
        .and_then(|p| p.get("content_hash"))
        .and_then(Value::as_str);
    if !is_new && existing_hash == Some(content_hash) {
        // The caller uses `existing` to load this module's already-known
        // declarations into the resolution symbol index WITHOUT re-parsing —
        // the whole point of the hash-before-parse fast path.
        return Ok((id, false, existing));
    }

    let unresolved = existing
        .as_ref()
        .and_then(|e| e.properties.as_ref())
        .map(read_unresolved)
        .unwrap_or_default();

    let mut props = serde_json::Map::new();
    props.insert("source_project".into(), json!(source_project));
    props.insert("language".into(), json!(language));
    props.insert("module_path".into(), json!(module_path));
    props.insert("content_hash".into(), json!(content_hash));
    props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
    if !unresolved.is_empty() {
        props.insert(
            "unresolved_specifiers".into(),
            serde_json::to_value(&unresolved).expect("serializes"),
        );
    }

    let mut entity = Entity::new(token.namespace().as_str(), "concept", module_path)
        .with_entity_type(Some("module"));
    entity.id = id;
    entity.properties = Some(Value::Object(props));
    let now = ts(sweep_time);
    entity.created_at = existing.as_ref().map(|e| e.created_at).unwrap_or(now);
    entity.updated_at = now;
    upsert_entity(rt, token, entity).await?;

    if is_new {
        report.modules_created += 1;
    } else {
        report.modules_updated += 1;
    }
    Ok((id, true, existing))
}

/// Read back `declaration_ids` previously stamped on a module entity — the
/// ids of every L2 declaration this module produced the last time it was
/// actually parsed.
fn read_declaration_ids(properties: &Value) -> Vec<Uuid> {
    properties
        .get("declaration_ids")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Stamp `decl_ids` onto a just-(re)parsed module entity's
/// `declaration_ids` property so a future sweep that finds this
/// module's content unchanged can seed the resolution symbol index from
/// these ids directly, without re-parsing.
async fn stamp_module_declaration_ids(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    decl_ids: &[Uuid],
) -> Result<(), CodeSourceIngestError> {
    let Some(mut entity) = get_entity_opt(rt, token, module_id).await? else {
        return Ok(());
    };
    let mut props = entity
        .properties
        .clone()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    props.insert(
        "declaration_ids".into(),
        json!(decl_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()),
    );
    entity.properties = Some(Value::Object(props));
    upsert_entity(rt, token, entity).await
}

/// Batched load of previously-known declarations for an unchanged module:
/// one `get_entities_by_ids` call (already 900-chunked
/// internally, matching the repository's SQL parameter-limit precedent)
/// rather than a per-declaration fetch, seeding `symbol_index` so a changed
/// module elsewhere in the same pass can still resolve a call/impl into any
/// of these declarations. Every loaded id is also queued onto `touch_ids`:
/// the module's file WAS read and hashed this sweep (that is how "unchanged"
/// was established), so its declarations count as observed and get the same
/// last_seen_at bump B5 gives the module itself.
async fn load_unchanged_declarations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    decl_ids: &[Uuid],
    symbol_index: &mut HashMap<SymbolKey, Uuid>,
    touch_ids: &mut Vec<Uuid>,
) -> Result<(), CodeSourceIngestError> {
    if decl_ids.is_empty() {
        return Ok(());
    }
    let entities = rt.get_entities_by_ids(token, decl_ids).await?;
    for entity in entities {
        let Some(props) = entity.properties.as_ref() else {
            continue;
        };
        let Some(module_path) = props.get("module_path").and_then(Value::as_str) else {
            continue;
        };
        let Some(kind) = entity
            .entity_type
            .as_deref()
            .and_then(DeclKind::from_code_token)
        else {
            continue;
        };
        let key = SymbolKey {
            source_project: props
                .get("source_project")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            module_path: module_path.to_string(),
            name: entity.name.clone(),
            kind,
        };
        symbol_index.insert(key, entity.id);
        touch_ids.push(entity.id);
    }
    Ok(())
}

/// Append `spec` to `entity_id`'s `unresolved_specifiers` (deduped), without
/// disturbing any other property already stamped this sweep (project/module
/// upsert already ran first, so this always reads back the row this pass
/// just wrote).
async fn record_unresolved(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
    spec: UnresolvedSpec,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let Some(mut entity) = get_entity_opt(rt, token, entity_id).await? else {
        return Ok(());
    };
    let mut list = entity
        .properties
        .as_ref()
        .map(read_unresolved)
        .unwrap_or_default();
    if list.contains(&spec) {
        return Ok(());
    }
    list.push(spec);
    report.unresolved_recorded += 1;
    let mut props = entity
        .properties
        .clone()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    props.insert(
        "unresolved_specifiers".into(),
        serde_json::to_value(&list).expect("serializes"),
    );
    entity.properties = Some(Value::Object(props));
    upsert_entity(rt, token, entity).await
}

/// The path separator a module path uses in each language's native form
/// (`imports::module_path_for_file`'s output shape).
fn module_path_separator(language: &str) -> &'static str {
    match language {
        "python" => ".",
        "typescript" => "/",
        _ => "::",
    }
}

/// Candidate module-path prefixes for `specifier`, longest first, then each
/// shorter prefix down to the single leading segment.
///
/// A `use crate::foo::Thing` item import classifies to the intra-module
/// target `foo::Thing`, but module identity is the *declaring file's* module
/// path (`foo`, not `foo::Thing` — `Thing` names an item inside that module,
/// not a nested module). Trying progressively shorter prefixes against the
/// known module set picks the longest one that actually exists, so an item
/// import resolves to its containing module instead of staying unresolved
/// forever.
fn module_candidate_specifiers(language: &str, specifier: &str) -> Vec<String> {
    let sep = module_path_separator(language);
    let segments: Vec<&str> = specifier.split(sep).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return vec![specifier.to_string()];
    }
    (1..=segments.len())
        .rev()
        .map(|n| segments[..n].join(sep))
        .collect()
}

/// Candidate target ids for `spec`, in resolution-priority order — the
/// caller tries each in turn and takes the first that resolves to an
/// existing entity (see `module_candidate_specifiers`).
fn target_ids_for(source_project: &str, spec: &UnresolvedSpec) -> Vec<Uuid> {
    match spec.target_kind.as_str() {
        "module" => module_candidate_specifiers(&spec.language, &spec.specifier)
            .into_iter()
            .map(|path| module_uuid(source_project, &spec.language, &path))
            .collect(),
        _ => vec![project_uuid(&spec.specifier)],
    }
}

/// Merges `new_kind` into the `dependency_kinds` array already recorded on
/// `existing_metadata` (if any), sorted and deduped so repeated ingests of
/// the same provenance stay a no-op change.
fn merge_dependency_kinds(
    existing_metadata: Option<&Value>,
    new_kind: &str,
    language: &str,
) -> Value {
    let mut kinds: BTreeSet<String> = existing_metadata
        .and_then(|m| m.get("dependency_kinds"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    kinds.insert(new_kind.to_string());
    json!({
        "dependency_kinds": kinds.into_iter().collect::<Vec<_>>(),
        "language": language,
    })
}

/// Upserts a `depends_on` edge, merging `dependency_kind` into the edge's
/// `dependency_kinds` list rather than overwriting it — `graph_edges`'s
/// `(namespace, source_id, target_id, relation)` natural key means only one
/// `depends_on` edge can ever exist between a given ordered pair, so a
/// manifest-declared dependency and an import-scan-detected one between the
/// same two projects are two provenance facts folded onto one row, not two
/// rows.
#[allow(clippy::too_many_arguments)]
async fn upsert_dependency_edge(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
    dependency_kind: &str,
    language: &str,
    now: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::DependsOn, source_id, target_id);
    let link_id = LinkId::from(edge_id);
    let graph = rt.graph(token)?;
    let existing = graph
        .get_edge(link_id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let existed = existing.is_some();
    let metadata = merge_dependency_kinds(
        existing.as_ref().and_then(|e| e.metadata.as_ref()),
        dependency_kind,
        language,
    );
    let edge = Edge {
        id: link_id,
        namespace: token.namespace().as_str().to_string(),
        source_id,
        target_id,
        relation: EdgeRelation::DependsOn,
        weight: 1.0,
        created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
        updated_at: now,
        deleted_at: None,
        metadata: Some(metadata),
        target_backend: None,
    };
    graph
        .upsert_edge(edge)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    if existed {
        report.edges_updated += 1;
    } else {
        report.edges_created += 1;
    }
    Ok(())
}

/// B6 synchronous re-resolve pass: scan every entity in the target database
/// carrying unresolved specifiers (from this call or any prior one) and
/// replay each against the now-known entity set, materializing edges for
/// anything that now resolves.
async fn reresolve_pass(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    now: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id, kind, properties FROM entities WHERE namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.unresolved_specifiers') IS NOT NULL"
                .into(),
            params: vec![SqlValue::Text(token.namespace().as_str().to_string())],
            label: Some("code_ingest_reresolve_scan".into()),
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;

    for row in rows {
        let id = match row.get("id") {
            Some(SqlValue::Uuid(u)) => *u,
            Some(SqlValue::Text(s)) => match Uuid::parse_str(s) {
                Ok(u) => u,
                Err(_) => continue,
            },
            _ => continue,
        };
        let Some(mut entity) = get_entity_opt(rt, token, id).await? else {
            continue;
        };
        let source_project = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("source_project"))
            .and_then(Value::as_str)
            .unwrap_or(entity.name.as_str())
            .to_string();
        let mut list = entity
            .properties
            .as_ref()
            .map(read_unresolved)
            .unwrap_or_default();
        if list.is_empty() {
            continue;
        }
        let mut still_unresolved = Vec::new();
        let mut changed = false;
        for spec in list.drain(..) {
            let mut resolved_target = None;
            for target_id in target_ids_for(&source_project, &spec) {
                if get_entity_opt(rt, token, target_id).await?.is_some() {
                    resolved_target = Some(target_id);
                    break;
                }
            }
            match resolved_target {
                Some(target_id) => {
                    upsert_dependency_edge(
                        rt,
                        token,
                        entity.id,
                        target_id,
                        &spec.dependency_kind,
                        &spec.language,
                        now,
                        report,
                    )
                    .await?;
                    report.unresolved_resolved += 1;
                    changed = true;
                }
                None => still_unresolved.push(spec),
            }
        }
        if changed {
            let mut props = entity
                .properties
                .clone()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if still_unresolved.is_empty() {
                props.remove("unresolved_specifiers");
            } else {
                props.insert(
                    "unresolved_specifiers".into(),
                    serde_json::to_value(&still_unresolved).expect("serializes"),
                );
            }
            entity.properties = Some(Value::Object(props));
            upsert_entity(rt, token, entity).await?;
        }
    }
    Ok(())
}

fn collect_source_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        "dist",
        "build",
    ];
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            collect_source_files(&path, ext, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    Ok(())
}

/// Run one L1 + L1.5 (+ L2 when `opts.enable_l2`) ingest pass over
/// `opts.path` into the runtime `rt` (already bound to the caller-selected
/// target database — B7 target selection happens in the verb handler, not
/// here).
pub async fn run_code_ingest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    opts: CodeSourceIngestOptions<'_>,
) -> Result<CodeSourceIngestReport, CodeSourceIngestError> {
    if !opts.path.is_dir() {
        return Err(CodeSourceIngestError::InvalidPath(opts.path.to_path_buf()));
    }

    let mut report = CodeSourceIngestReport {
        languages: opts.languages.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };

    let mut project_ids: HashMap<String, Uuid> = HashMap::new();

    // L1: manifest discovery, project entities, and manifest-declared
    // dependency edges (project depends_on project). Independently
    // switchable (ADR-085 Amendment 2 B3) — with `enable_l1: false` no
    // manifest is even read, so a caller selecting only l1.5/l2 sees no
    // manifest-derived project or dependency-edge writes at all.
    if opts.enable_l1 {
        let manifests = manifest::discover_manifests(opts.path, &opts.languages)
            .map_err(|e| CodeSourceIngestError::InvalidPath(opts.path.join(e.to_string())))?;

        for m in &manifests {
            let id = upsert_project(rt, token, &m.name, m.language, opts.sweep_time, &mut report)
                .await?;
            project_ids.insert(m.name.clone(), id);
        }

        for m in &manifests {
            let source_id = project_ids[&m.name];
            for (dep_name, dep_kind) in &m.dependencies {
                let spec = UnresolvedSpec {
                    specifier: dep_name.clone(),
                    target_kind: "project".to_string(),
                    dependency_kind: dep_kind.clone(),
                    language: m.language.to_string(),
                };
                record_unresolved(rt, token, source_id, spec, &mut report).await?;
            }
        }
    }

    // L1.5: regex import scan (module + project depends_on edges) and/or L2:
    // Scanner/Extractor symbol tier. Both are driven by the same per-language
    // file walk, so they share one pass over the tree; `run_import_scan`
    // internally gates which entities/edges each tier contributes. Driven by
    // per-language file discovery across the whole ingest root — independent
    // of manifest discovery — so a manifestless source folder still yields
    // module/project entities and import edges under the basename-fallback
    // identity rule (ADR-085 Amendment 2 B4), rather than being silently
    // skipped for lack of a governing manifest.
    if opts.enable_l1_5 || opts.enable_l2 {
        for language in opts.languages.iter().copied() {
            run_import_scan(
                rt,
                token,
                language,
                opts.path,
                opts.sweep_time,
                opts.enable_l1_5,
                opts.enable_l2,
                &mut project_ids,
                &mut report,
            )
            .await?;
        }
    }

    // Only L1 (project-level manifest deps) and L1.5 (module/project import
    // scan) ever record `unresolved_specifiers`; L2 resolves synchronously
    // against its own in-pass `symbol_index` and never uses this queue.
    // Gating on their disjunction means an L2-only call never
    // replays and materializes an L1.5 import edge left pending by some
    // earlier, differently-tiered ingest of the same database, while an
    // L1-only call still resolves its own manifest-declared dependency
    // edges exactly as before.
    if opts.enable_l1 || opts.enable_l1_5 {
        reresolve_pass(rt, token, opts.sweep_time, &mut report).await?;
    }

    Ok(report)
}

/// The `source_project` for a file with no governing manifest anywhere above
/// it: the basename of the ingested folder (ADR-085 Amendment 2 B4).
fn basename_project_name(ingest_root: &Path) -> String {
    ingest_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ingest_root.display().to_string())
}

/// A call-target path pending resolution against the project-wide L2 symbol
/// index, accumulated across every file of one `run_import_scan` language
/// pass and resolved once all files are scanned (mirrors L1.5's within-pass
/// ordering independence: a caller earlier than its callee in file-walk
/// order still resolves).
struct PendingCall {
    source_id: Uuid,
    project_name: String,
    /// The calling declaration's own module — the base a bare name or a
    /// `self`/`super`-qualified path resolves against.
    caller_module_path: String,
    segments: Vec<String>,
}

struct PendingImpl {
    project_name: String,
    /// The `impl` block's own module — the base a bare name or a
    /// `crate`/`self`/`super`-qualified path resolves against (the same
    /// resolver `resolve_call_target` uses for call targets).
    module_path: String,
    /// Full path as written at the impl site (e.g. `["crate", "types",
    /// "S"]`), not just the last segment — a qualified path can name a type
    /// declared outside the impl block's own module.
    type_segments: Vec<String>,
    trait_segments: Vec<String>,
}

/// The parent module path of `module_path` (one level up), or the crate
/// root's `"crate"` token when no parent segment remains (`module_path_for_file`'s
/// convention for a crate-root file).
fn parent_module_path(module_path: &str) -> String {
    let mut parts: Vec<&str> = module_path.split("::").collect();
    parts.pop();
    if parts.is_empty() {
        "crate".to_string()
    } else {
        parts.join("::")
    }
}

/// Append `extra` path segments onto `base`, respecting the `"crate"`
/// root-module convention (`module_path_for_file`): crate root plus one
/// segment is just that segment, not `"crate::segment"`.
fn join_module_path(base: &str, extra: &[String]) -> String {
    if extra.is_empty() {
        base.to_string()
    } else if base == "crate" {
        extra.join("::")
    } else {
        format!("{base}::{}", extra.join("::"))
    }
}

/// Resolve a Rust call-site path (`CallRef::segments`) into a `(module_path,
/// name)` candidate against the same-project symbol index, using only the
/// syntactic context available at the call site: `crate`/`self`/`super`
/// qualifiers plus the calling declaration's own module path.
///
/// A bare name resolves within the caller's own module only — this tier
/// does not build a per-file `use`-alias map, so a bare name absent from the
/// caller's own module, or a path qualified by anything other than
/// `crate`/`self`/`super` (an external module name, a re-export, a type's
/// associated function), is left unresolved rather than guessed at. Two
/// same-named declarations in different modules therefore never collapse
/// onto the wrong one: each resolves only against its own caller's module
/// (or an explicitly qualified target).
fn resolve_call_target(caller_module_path: &str, segments: &[String]) -> Option<(String, String)> {
    let (name, prefix) = segments.split_last()?;
    if prefix.is_empty() {
        return Some((caller_module_path.to_string(), name.clone()));
    }
    match prefix[0].as_str() {
        "crate" => Some((join_module_path("crate", &prefix[1..]), name.clone())),
        "self" => Some((
            join_module_path(caller_module_path, &prefix[1..]),
            name.clone(),
        )),
        "super" => {
            // Consume ALL consecutive leading `super` segments:
            // `super::super::helper` from `a::b::c` must resolve against
            // `a`, not `a::super::helper` (the old first-only bug). Walking
            // above the crate root returns `None` — unresolved is always
            // safer than a wrong path.
            let mut base = caller_module_path.to_string();
            let mut rest = prefix;
            while rest.first().map(String::as_str) == Some("super") {
                if base == "crate" {
                    return None;
                }
                base = parent_module_path(&base);
                rest = &rest[1..];
            }
            Some((join_module_path(&base, rest), name.clone()))
        }
        _ => None,
    }
}

/// L2 language adapter table: the single place mapping a language to
/// EVERYTHING its L2 support requires — whether it is
/// supported at all, its Scanner+Extractor dispatch, and its call/impl
/// target resolver. One `L2Language` variant carries all three (an
/// `l2_scanner_supported`-style check that only gated the scanner dispatch,
/// separately from whatever resolver got applied to its calls regardless of
/// source language, could add a language's scan support and silently keep
/// resolving its calls with another language's rules, or add "support" that
/// dispatches to no scanner at all — a match on this enum forces both to
/// move together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum L2Language {
    Rust,
}

impl L2Language {
    fn for_language(language: &str) -> Option<Self> {
        match language {
            "rust" => Some(Self::Rust),
            _ => None,
        }
    }

    /// The `crate`/`self`/`super`-aware call/impl-target resolver for this
    /// language. Rust-only in this slice.
    fn resolve_target(
        self,
        caller_module_path: &str,
        segments: &[String],
    ) -> Option<(String, String)> {
        match self {
            Self::Rust => resolve_call_target(caller_module_path, segments),
        }
    }
}

fn l2_scanner_supported(language: &str) -> bool {
    L2Language::for_language(language).is_some()
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_l2_scan(
    language: &str,
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    proj_name: &str,
    module_path: &str,
    module_id: Uuid,
    content: &str,
    file: &Path,
    sweep_time: DateTime<Utc>,
    symbol_index: &mut HashMap<SymbolKey, Uuid>,
    pending_calls: &mut Vec<PendingCall>,
    pending_impls: &mut Vec<PendingImpl>,
    touch_ids: &mut Vec<Uuid>,
    report: &mut CodeSourceIngestReport,
) -> Result<Vec<Uuid>, CodeSourceIngestError> {
    match L2Language::for_language(language) {
        Some(L2Language::Rust) => {
            scan_rust_l2(
                rt,
                token,
                proj_name,
                language,
                module_path,
                module_id,
                content,
                file,
                sweep_time,
                symbol_index,
                pending_calls,
                pending_impls,
                touch_ids,
                report,
            )
            .await
        }
        None => Ok(Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_import_scan(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    language: &'static str,
    ingest_root: &Path,
    sweep_time: DateTime<Utc>,
    enable_l1_5: bool,
    enable_l2: bool,
    project_ids: &mut HashMap<String, Uuid>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    // A language with no L2 scanner and no L1.5 request does zero work for
    // this call — no file walk, no read, no hash, no module upsert. Must be
    // checked before `collect_source_files` runs at all.
    let l2_here = enable_l2 && l2_scanner_supported(language);
    if !enable_l1_5 && !l2_here {
        return Ok(());
    }

    let Some(ext) = imports::extension_for_language(language) else {
        return Ok(());
    };
    let mut files = Vec::new();
    if let Err(e) = collect_source_files(ingest_root, ext, &mut files) {
        report
            .warnings
            .push(format!("walking {}: {e}", ingest_root.display()));
        return Ok(());
    }

    // L2 project-wide symbol index ((project, module_path, name, kind) ->
    // entity id — module_path is part of the key so two
    // same-named declarations in different modules never collapse onto one
    // entry) and deferred call/impl resolution, populated as files are
    // scanned below and resolved once after the whole file loop completes.
    // Rust-only in this slice (B2's Scanner delivery order); a no-op set for
    // every other language.
    let mut symbol_index: HashMap<SymbolKey, Uuid> = HashMap::new();
    let mut pending_calls: Vec<PendingCall> = Vec::new();
    let mut pending_impls: Vec<PendingImpl> = Vec::new();
    // Declaration/module ids whose content_hash matched the prior sweep
    // exactly: stamped with a single batched last_seen_at-only
    // UPDATE at the end of this language pass instead of a full rewrite.
    let mut touch_ids: Vec<Uuid> = Vec::new();
    // Memoized across every file of this language pass — a
    // directory with many files (or many descendants sharing an ancestor
    // manifest) probes and parses that manifest chain once, not once per file.
    let mut manifest_memo = manifest::ManifestMemo::new();

    for file in files {
        let Some(file_dir) = file.parent() else {
            continue;
        };
        let (proj_root, proj_name) =
            manifest::find_governing_manifest_memoized(file_dir, language, &mut manifest_memo)
                .unwrap_or_else(|| {
                    (
                        ingest_root.to_path_buf(),
                        basename_project_name(ingest_root),
                    )
                });
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, language) else {
            continue;
        };

        let proj_id = match project_ids.get(&proj_name) {
            Some(id) => *id,
            None => {
                let id =
                    upsert_project(rt, token, &proj_name, language, sweep_time, report).await?;
                project_ids.insert(proj_name.clone(), id);
                id
            }
        };

        let content = match fs::read_to_string(&file) {
            Ok(c) => c,
            Err(e) => {
                report
                    .warnings
                    .push(format!("reading {}: {e}", file.display()));
                continue;
            }
        };
        let hash = extractor::fnv1a(&content);
        let (module_id, module_changed, existing_module) = upsert_module(
            rt,
            token,
            &proj_name,
            language,
            &module_path,
            &hash,
            sweep_time,
            report,
        )
        .await?;

        if module_changed {
            let contains_edge_id = edge_uuid(EdgeRelation::Contains, proj_id, module_id);
            let contains_created = upsert_edge(
                rt,
                token,
                contains_edge_id,
                proj_id,
                module_id,
                EdgeRelation::Contains,
                json!({}),
                sweep_time,
            )
            .await?;
            if contains_created {
                report.edges_created += 1;
            } else {
                report.edges_updated += 1;
            }
        } else {
            touch_ids.push(module_id);
        }

        if l2_here {
            // `declaration_ids` is stamped onto a module entity only after
            // it has actually gone through an L2 scan (`stamp_module_declaration_ids`);
            // its absence means this module has never been L2-scanned -- e.g.
            // an L1.5-only prior ingest of this exact file, followed by
            // enabling L2 on an unchanged sweep. Without this check, an
            // unchanged content_hash would route straight to
            // `load_unchanged_declarations` and silently load an empty
            // list, leaving the module's L2 symbols permanently unscanned.
            let l2_never_scanned = existing_module
                .as_ref()
                .and_then(|e| e.properties.as_ref())
                .is_none_or(|p| p.get("declaration_ids").is_none());
            if module_changed || l2_never_scanned {
                // Hash-before-parse: this file's whole-content hash
                // (computed above, before any syn parsing) already told us
                // it changed, so only a changed-or-new module, or one that
                // has never had an L2 pass, ever reaches the actual
                // parse+extract path.
                let decl_ids = dispatch_l2_scan(
                    language,
                    rt,
                    token,
                    &proj_name,
                    &module_path,
                    module_id,
                    &content,
                    &file,
                    sweep_time,
                    &mut symbol_index,
                    &mut pending_calls,
                    &mut pending_impls,
                    &mut touch_ids,
                    report,
                )
                .await?;
                stamp_module_declaration_ids(rt, token, module_id, &decl_ids).await?;
            } else {
                // No parse, no extraction, no edge writes — just
                // load this module's already-known declarations (from its
                // own `declaration_ids` property, stamped the last time it
                // actually changed) into the resolution symbol index, so a
                // changed module elsewhere in this pass can still resolve a
                // call/impl into one of them.
                let decl_ids = existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .map(read_declaration_ids)
                    .unwrap_or_default();
                load_unchanged_declarations(
                    rt,
                    token,
                    &decl_ids,
                    &mut symbol_index,
                    &mut touch_ids,
                )
                .await?;
            }
        }

        if enable_l1_5 {
            for raw in imports::extract_raw_imports(language, &content) {
                let resolved = if language == "typescript" && raw.starts_with('.') {
                    let rel_dir = file_dir.strip_prefix(&proj_root).unwrap_or(Path::new(""));
                    Resolved::IntraModule(imports::resolve_relative_ts_module(rel_dir, &raw))
                } else {
                    imports::classify_import(language, &raw, &module_path, &proj_name)
                };
                match resolved {
                    Resolved::Skip => {}
                    Resolved::IntraModule(target_module_path) => {
                        let spec = UnresolvedSpec {
                            specifier: target_module_path,
                            target_kind: "module".to_string(),
                            dependency_kind: "import".to_string(),
                            language: language.to_string(),
                        };
                        record_unresolved(rt, token, module_id, spec, report).await?;
                    }
                    Resolved::ExternalProject(target_name) => {
                        let spec = UnresolvedSpec {
                            specifier: target_name,
                            target_kind: "project".to_string(),
                            dependency_kind: "import".to_string(),
                            language: language.to_string(),
                        };
                        record_unresolved(rt, token, proj_id, spec, report).await?;
                    }
                }
            }
        }
    }

    // L2 same-project symbol resolution: every declaration across every
    // file of this language pass is now in `symbol_index`, so calls/impls
    // recorded against a declaration that appeared later in file-walk order
    // than its caller still resolve here. Resolved targets are deduped per
    // caller (N calls to one helper yield one edge) and batched
    // into a single `upsert_edges` call. Both loops resolve
    // through the language's own adapter rather than calling
    // the Rust resolver directly — `pending_calls`/`pending_impls` are only
    // ever populated when `L2Language::for_language(language)` is `Some`
    // (`dispatch_l2_scan`'s gate), so `adapter` is always `Some` in practice
    // here; the `and_then` is defensive, not load-bearing.
    let adapter = L2Language::for_language(language);
    let mut call_targets: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
    for call in &pending_calls {
        let Some((module_path, name)) =
            adapter.and_then(|a| a.resolve_target(&call.caller_module_path, &call.segments))
        else {
            report.symbol_dependencies_unresolved += 1;
            continue;
        };
        let key = SymbolKey {
            source_project: call.project_name.clone(),
            module_path,
            name,
            kind: DeclKind::Function,
        };
        match symbol_index.get(&key) {
            Some(target_id) => {
                call_targets
                    .entry(call.source_id)
                    .or_default()
                    .insert(*target_id);
            }
            None => report.symbol_dependencies_unresolved += 1,
        }
    }
    // Both sides resolve through the same `crate`/`self`/`super`
    // -aware module resolution call targets use, rather than the previous
    // bare-last-segment lookup scoped only to the impl's own module — a
    // qualifier that resolver doesn't understand (an external module name)
    // leaves the relation unresolved rather than guessing the impl's own
    // module is right.
    let mut impl_targets: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
    for imp in &pending_impls {
        let resolved = adapter
            .and_then(|a| a.resolve_target(&imp.module_path, &imp.type_segments))
            .zip(adapter.and_then(|a| a.resolve_target(&imp.module_path, &imp.trait_segments)));
        let Some(((type_module, type_name), (trait_module, trait_name))) = resolved else {
            report.symbol_dependencies_unresolved += 1;
            continue;
        };
        let type_key = SymbolKey {
            source_project: imp.project_name.clone(),
            module_path: type_module,
            name: type_name,
            kind: DeclKind::Datatype,
        };
        let trait_key = SymbolKey {
            source_project: imp.project_name.clone(),
            module_path: trait_module,
            name: trait_name,
            kind: DeclKind::Interface,
        };
        match (symbol_index.get(&type_key), symbol_index.get(&trait_key)) {
            (Some(type_id), Some(trait_id)) => {
                impl_targets.entry(*type_id).or_default().insert(*trait_id);
            }
            _ => report.symbol_dependencies_unresolved += 1,
        }
    }

    let mut desired: Vec<(Uuid, Uuid, EdgeRelation)> = Vec::new();
    for (source_id, targets) in &call_targets {
        for target_id in targets {
            desired.push((*source_id, *target_id, EdgeRelation::DependsOn));
        }
    }
    for (source_id, targets) in &impl_targets {
        for target_id in targets {
            desired.push((*source_id, *target_id, EdgeRelation::Implements));
        }
    }

    // One batched existence lookup instead of N serial awaited
    // `get_edge` reads — `get_edges` already chunks internally at 900
    // (khive-db `graph.rs` precedent), so this stays safe at any scale.
    let desired_ids: Vec<LinkId> = desired
        .iter()
        .map(|(source_id, target_id, relation)| {
            LinkId::from(edge_uuid(*relation, *source_id, *target_id))
        })
        .collect();
    let existing_ids: std::collections::HashSet<LinkId> = rt
        .graph(token)?
        .get_edges(&desired_ids)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
        .into_iter()
        .map(|e| e.id)
        .collect();

    // B5 extended to L2 edges: every edge this scan re-resolves gets its
    // `last_seen_at` stamped to this sweep's time, the same way declaration
    // entities are stamped. An edge whose source declaration was re-scanned
    // but no longer resolves to that target simply is not in `desired` this
    // pass, so it is never touched here — it keeps its previous
    // `last_seen_at` untouched, never deleted or mutated. Currency is a
    // view-layer decision: an L2 edge is current iff its `last_seen_at`
    // equals the latest sweep time for its source declaration's
    // `(source_project, language)` pair (the project entity's
    // `sweep_clock`), never a data-layer deletion.
    let stamp = sweep_time.to_rfc3339();
    let mut resolved_edges: Vec<Edge> = Vec::with_capacity(desired.len());
    for ((source_id, target_id, relation), edge_id) in
        desired.iter().copied().zip(desired_ids.iter().copied())
    {
        let existed = existing_ids.contains(&edge_id);
        let metadata = match relation {
            EdgeRelation::DependsOn => {
                json!({ "dependency_kind": "build", "l2_derived": true, "last_seen_at": stamp })
            }
            _ => json!({ "l2_derived": true, "last_seen_at": stamp }),
        };
        resolved_edges.push(Edge {
            id: edge_id,
            namespace: token.namespace().as_str().to_string(),
            source_id,
            target_id,
            relation,
            weight: 1.0,
            created_at: sweep_time,
            updated_at: sweep_time,
            deleted_at: None,
            metadata: Some(metadata),
            target_backend: None,
        });
        if existed {
            report.edges_updated += 1;
        } else {
            report.edges_created += 1;
        }
        report.symbol_edges_stamped += 1;
    }
    upsert_edges_batch(rt, token, resolved_edges).await?;

    touch_last_seen_at(rt, token, &touch_ids, sweep_time).await?;

    Ok(())
}

/// The immediate containing entity for a declaration at `module_segments`
/// relative to the file's own `module_path`: the file-level
/// `module_id` when the declaration is at top level, or the nested inline
/// module's own `symbol_uuid` (D2 `module` token) otherwise — the same id
/// that declaration's own `RustDeclKind::Module` entry resolves to.
fn contains_parent_id(
    proj_name: &str,
    language: &str,
    module_path: &str,
    module_id: Uuid,
    module_segments: &[String],
) -> Uuid {
    match module_segments.split_last() {
        None => module_id,
        Some((name, parent_segments)) => symbol_uuid(
            proj_name,
            language,
            &join_module_path(module_path, parent_segments),
            name,
            "module",
        ),
    }
}

/// L2 declaration scan for one already-read Rust file (ADR-085 Amendment 2
/// B2-B4): upserts a subtype `concept` entity per declaration (including
/// inline-module, method, and trait-default-method declarations)
/// links it to its containing module via `contains`, and queues its
/// call/impl targets for same-project resolution once the whole language
/// pass's `symbol_index` is complete.
///
/// Batched and incremental: every declaration's prior
/// content_hash is fetched in one `get_entities_by_ids` call, unchanged
/// declarations are stamped into `touch_ids` for a last_seen_at-only sweep
/// and never rewritten, and every changed-or-new declaration's entity and
/// `contains` edge go through one `upsert_entities`/`upsert_edges` batch call
/// each instead of two awaited singles per declaration.
#[allow(clippy::too_many_arguments)]
async fn scan_rust_l2(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    proj_name: &str,
    language: &str,
    module_path: &str,
    module_id: Uuid,
    content: &str,
    file: &Path,
    sweep_time: DateTime<Utc>,
    symbol_index: &mut HashMap<SymbolKey, Uuid>,
    pending_calls: &mut Vec<PendingCall>,
    pending_impls: &mut Vec<PendingImpl>,
    touch_ids: &mut Vec<Uuid>,
    report: &mut CodeSourceIngestReport,
) -> Result<Vec<Uuid>, CodeSourceIngestError> {
    let scan = match scanner_rust::scan_rust_source(content) {
        Ok(scan) => scan,
        Err(e) => {
            report
                .warnings
                .push(format!("L2 scanning {}: {e}", file.display()));
            return Ok(Vec::new());
        }
    };
    let extracted = extractor::from_rust_scan(scan);

    // Each declaration's own full module path: a top-level item
    // uses the file's `module_path` unchanged; an item nested inside `mod
    // inner { .. }` (or deeper) uses `module_path` joined with its
    // `module_segments`. This is the path stored on the entity, used as the
    // symbol identity input, and used as the caller module for its own call
    // resolution.
    let decl_module_paths: Vec<String> = extracted
        .declarations
        .iter()
        .map(|decl| join_module_path(module_path, &decl.module_segments))
        .collect();
    let decl_ids: Vec<Uuid> = extracted
        .declarations
        .iter()
        .zip(decl_module_paths.iter())
        .map(|(decl, decl_module_path)| {
            symbol_uuid(
                proj_name,
                language,
                decl_module_path,
                &decl.name,
                decl.kind.code_token(),
            )
        })
        .collect();
    let existing_by_id: HashMap<Uuid, Entity> = rt
        .get_entities_by_ids(token, &decl_ids)
        .await?
        .into_iter()
        .map(|e| (e.id, e))
        .collect();

    let now = ts(sweep_time);
    let mut changed_entities: Vec<Entity> = Vec::new();
    let mut changed_edges: Vec<Edge> = Vec::new();

    for ((decl, decl_module_path), decl_id) in extracted
        .declarations
        .iter()
        .zip(decl_module_paths.iter())
        .zip(decl_ids.iter().copied())
    {
        symbol_index.insert(
            SymbolKey {
                source_project: proj_name.to_string(),
                module_path: decl_module_path.clone(),
                name: decl.name.clone(),
                kind: decl.kind,
            },
            decl_id,
        );

        if decl.kind == DeclKind::Function {
            for callee in &decl.calls {
                pending_calls.push(PendingCall {
                    source_id: decl_id,
                    project_name: proj_name.to_string(),
                    caller_module_path: decl_module_path.clone(),
                    segments: callee.segments.clone(),
                });
            }
        }

        let existing = existing_by_id.get(&decl_id);
        let existing_hash = existing
            .and_then(|e| e.properties.as_ref())
            .and_then(|p| p.get("content_hash"))
            .and_then(Value::as_str);
        if existing.is_some() && existing_hash == Some(decl.content_hash.as_str()) {
            touch_ids.push(decl_id);
            continue;
        }
        let is_new = existing.is_none();

        let mut props = serde_json::Map::new();
        props.insert("source_project".into(), json!(proj_name));
        props.insert("language".into(), json!(language));
        props.insert("module_path".into(), json!(decl_module_path));
        props.insert("content_hash".into(), json!(decl.content_hash));
        props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));

        let mut entity = Entity::new(token.namespace().as_str(), "concept", decl.name.clone())
            .with_entity_type(Some(decl.kind.code_token()));
        entity.id = decl_id;
        if let Some(desc) = &decl.description {
            entity.description = Some(desc.clone());
        }
        entity.properties = Some(Value::Object(props));
        entity.created_at = existing.map(|e| e.created_at).unwrap_or(now);
        entity.updated_at = now;
        changed_entities.push(entity);

        if is_new {
            report.symbols_created += 1;
        } else {
            report.symbols_updated += 1;
        }

        let parent_id = contains_parent_id(
            proj_name,
            language,
            module_path,
            module_id,
            &decl.module_segments,
        );
        let contains_edge_id = edge_uuid(EdgeRelation::Contains, parent_id, decl_id);
        changed_edges.push(Edge {
            id: LinkId::from(contains_edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id: parent_id,
            target_id: decl_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            created_at: sweep_time,
            updated_at: sweep_time,
            deleted_at: None,
            metadata: Some(json!({ "l2_derived": true })),
            target_backend: None,
        });
        if is_new {
            report.edges_created += 1;
        } else {
            report.edges_updated += 1;
        }
    }

    upsert_entities_batch(rt, token, changed_entities).await?;
    upsert_edges_batch(rt, token, changed_edges).await?;

    for imp in &extracted.impls {
        pending_impls.push(PendingImpl {
            project_name: proj_name.to_string(),
            module_path: join_module_path(module_path, &imp.module_segments),
            type_segments: imp.type_path.clone(),
            trait_segments: imp.trait_path.clone(),
        });
    }

    Ok(decl_ids)
}

#[cfg(test)]
mod l2_identity_tests {
    use super::symbol_uuid;

    /// ADR-085 Amendment 2 B8 property 3 (cross-language identity
    /// disjointness): `language` is part of the identity tuple specifically
    /// so two same-named, same-kind declarations whose module paths
    /// coincide across languages never collapse onto one entity.
    #[test]
    fn symbol_uuid_differs_by_language() {
        let rust_id = symbol_uuid("proj", "rust", "crate", "helper", "function");
        let py_id = symbol_uuid("proj", "python", "crate", "helper", "function");
        assert_ne!(rust_id, py_id);
    }

    #[test]
    fn symbol_uuid_differs_by_kind() {
        let as_function = symbol_uuid("proj", "rust", "crate", "Thing", "function");
        let as_datatype = symbol_uuid("proj", "rust", "crate", "Thing", "datatype");
        assert_ne!(as_function, as_datatype);
    }

    #[test]
    fn symbol_uuid_is_stable_across_calls() {
        let a = symbol_uuid("proj", "rust", "crate::foo", "helper", "function");
        let b = symbol_uuid("proj", "rust", "crate::foo", "helper", "function");
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod resolve_call_target_tests {
    use super::resolve_call_target;

    fn segs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// Every consecutive leading `super` is consumed, not just
    /// the first — from `a::b::c`, `super::super::helper` resolves to `a`.
    #[test]
    fn multi_super_walks_up_one_level_per_segment() {
        let (module_path, name) =
            resolve_call_target("a::b::c", &segs(&["super", "super", "helper"])).unwrap();
        assert_eq!(module_path, "a");
        assert_eq!(name, "helper");
    }

    #[test]
    fn single_super_still_works() {
        let (module_path, name) =
            resolve_call_target("a::b::c", &segs(&["super", "helper"])).unwrap();
        assert_eq!(module_path, "a::b");
        assert_eq!(name, "helper");
    }

    /// An underflowing `super` chain (walking above the crate root) must
    /// return unresolved, never a wrong path.
    #[test]
    fn underflowing_super_chain_is_unresolved() {
        assert!(resolve_call_target("a", &segs(&["super", "super", "helper"])).is_none());
        assert!(resolve_call_target("crate", &segs(&["super", "helper"])).is_none());
    }
}
