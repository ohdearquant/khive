//! `code.ingest` L1 (manifest edges) + L1.5 (import-scan edges) core pipeline
//! (ADR-085 Amendment 2 B3-B6 and Amendment 5 F1-F3). L2
//! Scanner/Extractor is out of scope (PR-2).
//!
//! Every entity write in this pipeline runs through the runtime secret gate
//! (ADR-085 D6 #4) via `upsert_entity`. A gate refusal quarantines that one
//! item — it is recorded in [`CodeSourceIngestReport::blocked`] and skipped —
//! rather than aborting the rest of the sweep (issue #1594), the same
//! per-record posture `git.digest` already uses for its own write refusals.
//!
//! Identity (B4): every entity this pipeline creates has a `uuid5`-derived
//! id, so re-ingesting the same path is a pure upsert — no dedup lookups are
//! needed to avoid duplicate rows. Edge ids are likewise `uuid5`-derived from
//! their endpoints, so re-creating the same edge is also an idempotent
//! upsert. B6 cross-repo resolution and B5 staleness stamping are both
//! driven off this determinism: an unresolved specifier records only the
//! information needed to recompute its target's id later, and the
//! synchronous re-resolve pass (`reresolve_pass`) does exactly that.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use khive_runtime::{secret_gate, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Edge, Entity, LinkId};
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::imports::{self, Resolved};
use crate::ingest::CODE_INGEST_NAMESPACE;
use crate::manifest;

/// One content write the runtime secret gate refused during this pass.
///
/// The record's own identity (the manifest/source file it came from) is
/// kept; the secret itself is represented only by the detector name and a
/// masked excerpt (`SecretMatch`'s `first6...N` shape) — the rejected
/// content is never copied into the report. Mirrors `git.digest`'s
/// `IngestWriteRefusal` (ADR-088 Amendment 1 precedent).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockedWrite {
    pub file: String,
    pub detector: String,
    pub masked_excerpt: String,
}

/// Outcome counters for one `code.ingest` call, mirroring `git.digest`'s
/// `IngestReport` shape (ADR-088 Amendment 1 precedent).
#[derive(Debug, Default, serde::Serialize)]
pub struct CodeSourceIngestReport {
    pub projects_created: u64,
    pub projects_updated: u64,
    pub modules_created: u64,
    pub modules_updated: u64,
    pub edges_created: u64,
    pub edges_updated: u64,
    pub unresolved_recorded: u64,
    pub unresolved_resolved: u64,
    pub languages: Vec<String>,
    /// Per-manifest / per-file failures that did not abort the pass (fail
    /// loud without silently dropping the rest of the run).
    pub warnings: Vec<String>,
    /// Count of per-item content writes refused by the runtime secret gate
    /// during this pass, independent of unrelated `warnings` (mirrors
    /// `git.digest`'s `writes_refused`).
    pub blocked_count: u64,
    /// Safe structured detail for every entry counted by `blocked_count`
    /// (issue #1594): a gate-refused write is quarantined and skipped, it
    /// never aborts the rest of the ingest.
    pub blocked: Vec<BlockedWrite>,
    pub db_path: String,
    /// Git `HEAD` observed for the source tree, or `unversioned`.
    pub source_revision: String,
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
}

const IMPORT_DEPENDENCY_KIND: &str = "import";
const IMPORT_DEPENDENCY_SCOPE: &str = "build";
const UNVERSIONED_REVISION: &str = "unversioned";

#[derive(Debug)]
struct SourceSnapshot {
    root: PathBuf,
    revision: String,
}

#[derive(Debug)]
struct ModuleScan {
    source_project: String,
    imports: Vec<UnresolvedSpec>,
}

type ManifestScopeIndex = BTreeMap<(String, String, String), BTreeSet<String>>;

fn source_snapshot(ingest_root: &Path) -> SourceSnapshot {
    let fallback_root = ingest_root
        .canonicalize()
        .unwrap_or_else(|_| ingest_root.to_path_buf());
    let git_output = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(ingest_root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|stdout| stdout.trim().to_string())
    };
    let root = git_output(&["rev-parse", "--show-toplevel"])
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback_root);
    let revision = git_output(&["rev-parse", "--verify", "HEAD"])
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| UNVERSIONED_REVISION.to_string());
    SourceSnapshot { root, revision }
}

fn source_path(file: &Path, source_root: &Path) -> Option<String> {
    let canonical_file = file.canonicalize().ok()?;
    let canonical_root = source_root
        .canonicalize()
        .unwrap_or_else(|_| source_root.to_path_buf());
    let relative = canonical_file.strip_prefix(canonical_root).ok()?;
    let components: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    (!components.is_empty()).then(|| components.join("/"))
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

/// `graph_edges` carries a `UNIQUE(namespace, source_id, target_id, relation)`
/// natural key independent of the row's `id` (khive-db schema.sql), so at
/// most one edge of a given relation can ever exist between an ordered pair
/// regardless of what `id` an upsert names — a second `id` for the "same"
/// pair collapses onto the first row's natural-key conflict arm instead of
/// creating a second row. Edge identity here matches that invariant exactly:
/// no disambiguator. Distinct provenance for the same `depends_on` pair
/// (e.g. a manifest-declared dependency and an import-scan-detected one)
/// is folded into that single edge's dependency metadata (see
/// `merge_dependency_metadata`), not encoded into a second id.
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
    #[serde(default)]
    dependency_scope: String,
    language: String,
}

fn read_unresolved(properties: &Value) -> Vec<UnresolvedSpec> {
    let mut specs: Vec<UnresolvedSpec> = properties
        .get("unresolved_specifiers")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    for spec in &mut specs {
        if !is_dependency_scope(&spec.dependency_scope) {
            spec.dependency_scope = dependency_scope_for_kind(&spec.dependency_kind).to_string();
        }
    }
    specs
}

fn dependency_scope_for_kind(kind: &str) -> &'static str {
    match kind {
        "dev-dependencies" | "devDependencies" => "dev",
        "build-dependencies" | "import" => "build",
        _ => "normal",
    }
}

fn is_dependency_scope(scope: &str) -> bool {
    matches!(scope, "normal" | "dev" | "build")
}

fn scopes_for_dependency_kinds(kinds: &BTreeSet<String>, import_scope: &str) -> BTreeSet<String> {
    let declared: BTreeSet<String> = kinds
        .iter()
        .filter(|kind| kind.as_str() != IMPORT_DEPENDENCY_KIND)
        .map(|kind| dependency_scope_for_kind(kind).to_string())
        .collect();
    if !declared.is_empty() {
        declared
    } else {
        [import_scope.to_string()].into_iter().collect()
    }
}

fn preferred_import_scope(scopes: &BTreeSet<String>) -> &'static str {
    if scopes.contains("normal") {
        "normal"
    } else if scopes.contains("build") {
        "build"
    } else if scopes.contains("dev") {
        "dev"
    } else {
        IMPORT_DEPENDENCY_SCOPE
    }
}

fn declared_project_import_target_and_scope(
    manifest_scopes: &ManifestScopeIndex,
    source_project: &str,
    language: &str,
    target_project: &str,
) -> Option<(String, &'static str)> {
    let exact_key = (
        source_project.to_string(),
        language.to_string(),
        target_project.to_string(),
    );
    if let Some(scopes) = manifest_scopes.get(&exact_key) {
        return Some((target_project.to_string(), preferred_import_scope(scopes)));
    }
    if language != "rust" {
        return None;
    }
    let normalized_target = target_project.replace('-', "_");
    manifest_scopes
        .iter()
        .find(|((source, declared_language, declared_target), _)| {
            source == source_project
                && declared_language == language
                && declared_target.replace('-', "_") == normalized_target
        })
        .map(|((_, _, declared_target), scopes)| {
            (declared_target.clone(), preferred_import_scope(scopes))
        })
}

fn project_import_target_and_scope(
    manifest_scopes: &ManifestScopeIndex,
    source_project: &str,
    language: &str,
    target_project: &str,
) -> (String, &'static str) {
    declared_project_import_target_and_scope(
        manifest_scopes,
        source_project,
        language,
        target_project,
    )
    .unwrap_or_else(|| (target_project.to_string(), IMPORT_DEPENDENCY_SCOPE))
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

/// Runs the runtime secret gate over `entity`'s name and properties, the
/// same content the gate checks for every other write (ADR-085 D6 #4). The
/// direct storage-layer call `upsert_entity` wraps does not run this check
/// on its own path, so callers of this pipeline get no gate coverage unless
/// it happens here.
fn gate_check(entity: &Entity) -> Result<(), RuntimeError> {
    secret_gate::check(&entity.name)?;
    if let Some(properties) = &entity.properties {
        secret_gate::check_json(properties)?;
    }
    Ok(())
}

/// Upserts `entity` after running it through [`gate_check`]. Returns
/// `Ok(false)` without writing anything when the gate refuses the entity:
/// the refusal is recorded in `report.blocked` keyed by `file`, and the
/// caller moves on to the next item rather than aborting the whole ingest
/// (issue #1594 — quarantine, don't abort, mirroring `git.digest`'s
/// per-record refusal handling). Any other `RuntimeError` — not a gate
/// refusal — still propagates as an error, since only a gate refusal is
/// safe to treat as "skip this one item and continue".
async fn upsert_entity(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity: Entity,
    file: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    if let Err(err) = gate_check(&entity) {
        return match err {
            RuntimeError::SecretDetected(secret) => {
                report.blocked_count += 1;
                report.blocked.push(BlockedWrite {
                    file: file.to_string(),
                    detector: secret.detector.to_string(),
                    masked_excerpt: secret.masked,
                });
                Ok(false)
            }
            other => Err(other.into()),
        };
    }
    rt.entities(token)?
        .upsert_entity(entity)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    Ok(true)
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
///
/// Returns `Ok(None)` when the runtime secret gate refuses the write (the
/// refusal is recorded in `report.blocked`, keyed by `source_label` — never
/// by `name`, since `name` is content-derived from the manifest and may
/// itself be what the gate refused) — callers must treat that project as
/// absent from this sweep rather than indexing it.
#[allow(clippy::too_many_arguments)]
async fn upsert_project(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    source_label: &str,
    language: &str,
    sweep_time: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Uuid>, CodeSourceIngestError> {
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
    if !upsert_entity(rt, token, entity, source_label, report).await? {
        return Ok(None);
    }

    if is_new {
        report.projects_created += 1;
    } else {
        report.projects_updated += 1;
    }
    Ok(Some(id))
}

/// Returns `Ok(None)` when the runtime secret gate refuses the write (the
/// refusal is recorded in `report.blocked`, keyed by `file`) — callers must
/// treat that module as absent from this sweep rather than indexing it.
#[allow(clippy::too_many_arguments)]
async fn upsert_module(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    module_path: &str,
    source_path: &str,
    source_revision: &str,
    content_hash: &str,
    sweep_time: DateTime<Utc>,
    file: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Uuid>, CodeSourceIngestError> {
    let id = module_uuid(source_project, language, module_path);
    let existing = get_entity_opt(rt, token, id).await?;
    let is_new = existing.is_none();

    let unresolved = existing
        .as_ref()
        .and_then(|e| e.properties.as_ref())
        .map(read_unresolved)
        .unwrap_or_default();

    let mut props = serde_json::Map::new();
    props.insert("source_project".into(), json!(source_project));
    props.insert("language".into(), json!(language));
    props.insert("module_path".into(), json!(module_path));
    props.insert("source_path".into(), json!(source_path));
    props.insert("source_revision".into(), json!(source_revision));
    props.insert("content_hash".into(), json!(content_hash));
    props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
    props.insert("import_scan_status".into(), json!("unscanned"));
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
    if !upsert_entity(rt, token, entity, file, report).await? {
        return Ok(None);
    }

    if is_new {
        report.modules_created += 1;
    } else {
        report.modules_updated += 1;
    }
    Ok(Some(id))
}

/// Append `spec` to `entity_id`'s `unresolved_specifiers` (deduped), without
/// disturbing any other property already stamped this sweep (project/module
/// upsert already ran first, so this always reads back the row this pass
/// just wrote).
///
/// When the gate refuses the updated properties (e.g. `spec.specifier` is
/// itself secret-shaped), the refusal is recorded in `report.blocked` keyed
/// by `file` and the specifier is simply not recorded this sweep — the
/// entity itself is untouched, since `upsert_entity` blocks before writing.
async fn record_unresolved(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
    spec: UnresolvedSpec,
    file: &str,
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
    if upsert_entity(rt, token, entity, file, report).await? {
        report.unresolved_recorded += 1;
    }
    Ok(())
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

/// Merges producer evidence and derives the normalized scope array from the
/// complete evidence set. Manifest evidence is authoritative over `import`,
/// so re-ingest can repair an older import-default scope without retaining a
/// false production scope.
fn merge_dependency_metadata(
    existing_metadata: Option<&Value>,
    new_kind: &str,
    new_scope: &str,
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
    let scopes = scopes_for_dependency_kinds(&kinds, new_scope);
    json!({
        "dependency_kinds": kinds.into_iter().collect::<Vec<_>>(),
        "dependency_scopes": scopes.into_iter().collect::<Vec<_>>(),
        "language": language,
    })
}

/// Upserts a `depends_on` edge, merging its evidence kind and normalized
/// scope rather than overwriting either — `graph_edges`'s
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
    dependency_scope: &str,
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
    let metadata = merge_dependency_metadata(
        existing.as_ref().and_then(|e| e.metadata.as_ref()),
        dependency_kind,
        dependency_scope,
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
    manifest_scopes: &ManifestScopeIndex,
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
        for mut spec in list.drain(..) {
            if spec.target_kind == "project" && spec.dependency_kind == IMPORT_DEPENDENCY_KIND {
                if let Some((target, scope)) = declared_project_import_target_and_scope(
                    manifest_scopes,
                    &source_project,
                    &spec.language,
                    &spec.specifier,
                ) {
                    changed |= spec.specifier != target || spec.dependency_scope != scope;
                    spec.specifier = target;
                    spec.dependency_scope = scope.to_string();
                }
            }
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
                        &spec.dependency_scope,
                        &spec.language,
                        now,
                        report,
                    )
                    .await?;
                    report.unresolved_resolved += 1;
                    changed = true;
                }
                None => {
                    // A legacy import without `dependency_scope` can
                    // normalize to the same specifier as the freshly
                    // scanned form above. Keep the durable queue deduped
                    // after that repair as well as before it.
                    if still_unresolved.contains(&spec) {
                        changed = true;
                    } else {
                        still_unresolved.push(spec);
                    }
                }
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
            let entity_label = entity.id.to_string();
            upsert_entity(rt, token, entity, &entity_label, report).await?;
        }
    }
    Ok(())
}

async fn stamp_import_scan_coverage(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_scans: HashMap<Uuid, ModuleScan>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    for (module_id, scan) in module_scans {
        let mut unresolved_count = 0_u64;
        for spec in &scan.imports {
            let mut resolved = false;
            for target_id in target_ids_for(&scan.source_project, spec) {
                if get_entity_opt(rt, token, target_id).await?.is_some() {
                    resolved = true;
                    break;
                }
            }
            if !resolved {
                unresolved_count += 1;
            }
        }

        let Some(mut module) = get_entity_opt(rt, token, module_id).await? else {
            continue;
        };
        let source_label = module
            .properties
            .as_ref()
            .and_then(|properties| properties.get("source_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module_id.to_string());
        let mut props = module
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        props.insert(
            "import_scan_status".into(),
            json!(if unresolved_count == 0 {
                "scanned"
            } else {
                "partially_resolved"
            }),
        );
        props.insert("import_specifier_count".into(), json!(scan.imports.len()));
        props.insert("unresolved_import_count".into(), json!(unresolved_count));
        module.properties = Some(Value::Object(props));
        upsert_entity(rt, token, module, &source_label, report).await?;
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

fn content_hash(content: &str) -> String {
    // FNV-1a: fast, dependency-free, sufficient for change-detection (not a
    // security boundary).
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in content.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Run one L1 + L1.5 ingest pass over `opts.path` into the runtime `rt`
/// (already bound to the caller-selected target database — B7 target
/// selection happens in the verb handler, not here).
pub async fn run_code_ingest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    opts: CodeSourceIngestOptions<'_>,
) -> Result<CodeSourceIngestReport, CodeSourceIngestError> {
    if !opts.path.is_dir() {
        return Err(CodeSourceIngestError::InvalidPath(opts.path.to_path_buf()));
    }

    let snapshot = source_snapshot(opts.path);
    let mut report = CodeSourceIngestReport {
        languages: opts.languages.iter().map(|s| s.to_string()).collect(),
        source_revision: snapshot.revision.clone(),
        ..Default::default()
    };

    let manifests = manifest::discover_manifests(opts.path, &opts.languages)
        .map_err(|e| CodeSourceIngestError::InvalidPath(opts.path.join(e.to_string())))?;

    let mut manifest_scopes = ManifestScopeIndex::new();
    for manifest in &manifests {
        for (dependency, _kind, scope) in &manifest.dependencies {
            manifest_scopes
                .entry((
                    manifest.name.clone(),
                    manifest.language.to_string(),
                    dependency.clone(),
                ))
                .or_default()
                .insert(scope.clone());
        }
    }

    let mut project_ids: HashMap<String, Uuid> = HashMap::new();
    for m in &manifests {
        let file_label = m.manifest_path.display().to_string();
        let Some(id) = upsert_project(
            rt,
            token,
            &m.name,
            &file_label,
            m.language,
            opts.sweep_time,
            &mut report,
        )
        .await?
        else {
            // Gate-refused write, already recorded in report.blocked — this
            // project is absent from the sweep, skip it and keep going
            // (issue #1594).
            continue;
        };
        project_ids.insert(m.name.clone(), id);
    }

    // L1: manifest dependency edges (project depends_on project).
    for m in &manifests {
        let Some(&source_id) = project_ids.get(&m.name) else {
            // This manifest's own project write was gate-refused above;
            // nothing to hang dependency edges off of this sweep.
            continue;
        };
        let file_label = m.root.display().to_string();
        for (dep_name, dep_kind, dep_scope) in &m.dependencies {
            let spec = UnresolvedSpec {
                specifier: dep_name.clone(),
                target_kind: "project".to_string(),
                dependency_kind: dep_kind.clone(),
                dependency_scope: dep_scope.clone(),
                language: m.language.to_string(),
            };
            record_unresolved(rt, token, source_id, spec, &file_label, &mut report).await?;
        }
    }

    // L1.5: regex import scan (module + project depends_on edges). Driven by
    // per-language file discovery across the whole ingest root — independent
    // of manifest discovery — so a manifestless source folder still yields
    // module/project entities and import edges under the basename-fallback
    // identity rule (ADR-085 Amendment 2 B4), rather than being silently
    // skipped for lack of a governing manifest.
    let mut module_scans = HashMap::new();
    for language in opts.languages.iter().copied() {
        run_import_scan(
            rt,
            token,
            language,
            opts.path,
            &snapshot,
            &manifest_scopes,
            opts.sweep_time,
            &mut project_ids,
            &mut module_scans,
            &mut report,
        )
        .await?;
    }

    reresolve_pass(rt, token, &manifest_scopes, opts.sweep_time, &mut report).await?;
    stamp_import_scan_coverage(rt, token, module_scans, &mut report).await?;

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

#[allow(clippy::too_many_arguments)]
async fn run_import_scan(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    language: &'static str,
    ingest_root: &Path,
    snapshot: &SourceSnapshot,
    manifest_scopes: &ManifestScopeIndex,
    sweep_time: DateTime<Utc>,
    project_ids: &mut HashMap<String, Uuid>,
    module_scans: &mut HashMap<Uuid, ModuleScan>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
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
    files.sort();

    for file in files {
        let Some(file_dir) = file.parent() else {
            continue;
        };
        let (proj_root, proj_name) =
            manifest::find_governing_manifest(file_dir, ingest_root, language).unwrap_or_else(
                || {
                    (
                        ingest_root.to_path_buf(),
                        basename_project_name(ingest_root),
                    )
                },
            );
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, language) else {
            continue;
        };
        let Some(source_path) = source_path(&file, &snapshot.root) else {
            report.warnings.push(format!(
                "deriving repository-relative path for {}",
                file.display()
            ));
            continue;
        };

        let file_label = file.display().to_string();

        let proj_id = match project_ids.get(&proj_name) {
            Some(id) => *id,
            None => {
                // Label a refused fallback-project write by the source file
                // whose scan triggered it — a real on-disk location, never
                // the content-derived project name (#1594).
                let proj_label = file_label.clone();
                let Some(id) = upsert_project(
                    rt,
                    token,
                    &proj_name,
                    &proj_label,
                    language,
                    sweep_time,
                    report,
                )
                .await?
                else {
                    // Gate-refused write, already recorded in report.blocked
                    // — move on to the next file (issue #1594).
                    continue;
                };
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
        let hash = content_hash(&content);
        let Some(module_id) = upsert_module(
            rt,
            token,
            &proj_name,
            language,
            &module_path,
            &source_path,
            &snapshot.revision,
            &hash,
            sweep_time,
            &file_label,
            report,
        )
        .await?
        else {
            // Gate-refused write, already recorded in report.blocked — move
            // on to the next file (issue #1594).
            continue;
        };

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

        let mut scan_imports = Vec::new();
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
                        dependency_kind: IMPORT_DEPENDENCY_KIND.to_string(),
                        dependency_scope: IMPORT_DEPENDENCY_SCOPE.to_string(),
                        language: language.to_string(),
                    };
                    scan_imports.push(spec.clone());
                    record_unresolved(rt, token, module_id, spec, &file_label, report).await?;
                }
                Resolved::ExternalProject(target_name) => {
                    let (target_name, dependency_scope) = project_import_target_and_scope(
                        manifest_scopes,
                        &proj_name,
                        language,
                        &target_name,
                    );
                    let spec = UnresolvedSpec {
                        specifier: target_name,
                        target_kind: "project".to_string(),
                        dependency_kind: IMPORT_DEPENDENCY_KIND.to_string(),
                        dependency_scope: dependency_scope.to_string(),
                        language: language.to_string(),
                    };
                    scan_imports.push(spec.clone());
                    record_unresolved(rt, token, proj_id, spec, &file_label, report).await?;
                }
            }
        }
        let scan = module_scans.entry(module_id).or_insert_with(|| ModuleScan {
            source_project: proj_name.clone(),
            imports: Vec::new(),
        });
        scan.imports.extend(scan_imports);
    }
    Ok(())
}
