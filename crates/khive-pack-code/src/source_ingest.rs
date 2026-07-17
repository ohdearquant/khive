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
    /// Run the L2 Scanner/Extractor symbol tier (ADR-085 Amendment 2 B2-B3)
    /// in addition to L1/L1.5. Defaults to `false` at the verb boundary
    /// (`handlers.rs`'s `tiers` param) so existing callers see no change.
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

async fn upsert_entity(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity: Entity,
) -> Result<(), CodeSourceIngestError> {
    rt.entities(token)?
        .upsert_entity(entity)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))
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
) -> Result<Uuid, CodeSourceIngestError> {
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
    Ok(id)
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

    let manifests = manifest::discover_manifests(opts.path, &opts.languages)
        .map_err(|e| CodeSourceIngestError::InvalidPath(opts.path.join(e.to_string())))?;

    let mut project_ids: HashMap<String, Uuid> = HashMap::new();
    for m in &manifests {
        let id =
            upsert_project(rt, token, &m.name, m.language, opts.sweep_time, &mut report).await?;
        project_ids.insert(m.name.clone(), id);
    }

    // L1: manifest dependency edges (project depends_on project).
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

    // L1.5: regex import scan (module + project depends_on edges). Driven by
    // per-language file discovery across the whole ingest root — independent
    // of manifest discovery — so a manifestless source folder still yields
    // module/project entities and import edges under the basename-fallback
    // identity rule (ADR-085 Amendment 2 B4), rather than being silently
    // skipped for lack of a governing manifest.
    for language in opts.languages.iter().copied() {
        run_import_scan(
            rt,
            token,
            language,
            opts.path,
            opts.sweep_time,
            opts.enable_l2,
            &mut project_ids,
            &mut report,
        )
        .await?;
    }

    reresolve_pass(rt, token, opts.sweep_time, &mut report).await?;

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

/// A call-target or impl-target name pending resolution against the
/// project-wide L2 symbol index, accumulated across every file of one
/// `run_import_scan` language pass and resolved once all files are scanned
/// (mirrors L1.5's within-pass ordering independence: a caller earlier than
/// its callee in file-walk order still resolves).
struct PendingCall {
    source_id: Uuid,
    project_name: String,
    callee_name: String,
}

struct PendingImpl {
    project_name: String,
    type_name: String,
    trait_name: String,
}

#[allow(clippy::too_many_arguments)]
async fn run_import_scan(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    language: &'static str,
    ingest_root: &Path,
    sweep_time: DateTime<Utc>,
    enable_l2: bool,
    project_ids: &mut HashMap<String, Uuid>,
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

    // L2 project-wide symbol index (name+kind -> entity id) and deferred
    // call/impl resolution, populated as files are scanned below and
    // resolved once after the whole file loop completes. Rust-only in this
    // slice (B2's Scanner delivery order); a no-op set for every other
    // language.
    let mut symbol_index: HashMap<(String, String, DeclKind), Uuid> = HashMap::new();
    let mut pending_calls: Vec<PendingCall> = Vec::new();
    let mut pending_impls: Vec<PendingImpl> = Vec::new();

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
        let hash = content_hash(&content);
        let module_id = upsert_module(
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

        if enable_l2 && language == "rust" {
            scan_rust_l2(
                rt,
                token,
                &proj_name,
                language,
                &module_path,
                module_id,
                &content,
                &file,
                sweep_time,
                &mut symbol_index,
                &mut pending_calls,
                &mut pending_impls,
                report,
            )
            .await?;
        }

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

    // L2 same-project symbol resolution: every declaration across every
    // file of this language pass is now in `symbol_index`, so calls/impls
    // recorded against a declaration that appeared later in file-walk order
    // than its caller still resolve here.
    for call in pending_calls {
        let key = (
            call.project_name.clone(),
            call.callee_name.clone(),
            DeclKind::Function,
        );
        match symbol_index.get(&key) {
            Some(target_id) => {
                let edge_id = edge_uuid(EdgeRelation::DependsOn, call.source_id, *target_id);
                let created = upsert_edge(
                    rt,
                    token,
                    edge_id,
                    call.source_id,
                    *target_id,
                    EdgeRelation::DependsOn,
                    json!({ "dependency_kind": "build" }),
                    sweep_time,
                )
                .await?;
                if created {
                    report.edges_created += 1;
                } else {
                    report.edges_updated += 1;
                }
            }
            None => report.symbol_dependencies_unresolved += 1,
        }
    }
    for imp in pending_impls {
        let type_key = (
            imp.project_name.clone(),
            imp.type_name.clone(),
            DeclKind::Datatype,
        );
        let trait_key = (
            imp.project_name.clone(),
            imp.trait_name.clone(),
            DeclKind::Interface,
        );
        match (symbol_index.get(&type_key), symbol_index.get(&trait_key)) {
            (Some(type_id), Some(trait_id)) => {
                let edge_id = edge_uuid(EdgeRelation::Implements, *type_id, *trait_id);
                let created = upsert_edge(
                    rt,
                    token,
                    edge_id,
                    *type_id,
                    *trait_id,
                    EdgeRelation::Implements,
                    json!({}),
                    sweep_time,
                )
                .await?;
                if created {
                    report.edges_created += 1;
                } else {
                    report.edges_updated += 1;
                }
            }
            _ => report.symbol_dependencies_unresolved += 1,
        }
    }

    Ok(())
}

/// L2 declaration scan for one already-read Rust file (ADR-085 Amendment 2
/// B2-B4): upserts a subtype `concept` entity per top-level declaration,
/// links it to its containing module via `contains`, and queues its
/// call/impl targets for same-project resolution once the whole language
/// pass's `symbol_index` is complete.
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
    symbol_index: &mut HashMap<(String, String, DeclKind), Uuid>,
    pending_calls: &mut Vec<PendingCall>,
    pending_impls: &mut Vec<PendingImpl>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let scan = match scanner_rust::scan_rust_source(content) {
        Ok(scan) => scan,
        Err(e) => {
            report
                .warnings
                .push(format!("L2 scanning {}: {e}", file.display()));
            return Ok(());
        }
    };
    let extracted = extractor::from_rust_scan(scan);

    for decl in &extracted.declarations {
        let decl_id = symbol_uuid(
            proj_name,
            language,
            module_path,
            &decl.name,
            decl.kind.code_token(),
        );
        let existing = get_entity_opt(rt, token, decl_id).await?;
        let is_new = existing.is_none();

        let mut props = serde_json::Map::new();
        props.insert("source_project".into(), json!(proj_name));
        props.insert("language".into(), json!(language));
        props.insert("module_path".into(), json!(module_path));
        props.insert("content_hash".into(), json!(decl.content_hash));
        props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));

        let mut entity = Entity::new(token.namespace().as_str(), "concept", decl.name.clone())
            .with_entity_type(Some(decl.kind.code_token()));
        entity.id = decl_id;
        if let Some(desc) = &decl.description {
            entity.description = Some(desc.clone());
        }
        entity.properties = Some(Value::Object(props));
        let now = ts(sweep_time);
        entity.created_at = existing.as_ref().map(|e| e.created_at).unwrap_or(now);
        entity.updated_at = now;
        upsert_entity(rt, token, entity).await?;

        if is_new {
            report.symbols_created += 1;
        } else {
            report.symbols_updated += 1;
        }

        symbol_index.insert(
            (proj_name.to_string(), decl.name.clone(), decl.kind),
            decl_id,
        );

        let contains_edge_id = edge_uuid(EdgeRelation::Contains, module_id, decl_id);
        let created = upsert_edge(
            rt,
            token,
            contains_edge_id,
            module_id,
            decl_id,
            EdgeRelation::Contains,
            json!({}),
            sweep_time,
        )
        .await?;
        if created {
            report.edges_created += 1;
        } else {
            report.edges_updated += 1;
        }

        if decl.kind == DeclKind::Function {
            for callee in &decl.calls {
                pending_calls.push(PendingCall {
                    source_id: decl_id,
                    project_name: proj_name.to_string(),
                    callee_name: callee.clone(),
                });
            }
        }
    }

    for imp in &extracted.impls {
        pending_impls.push(PendingImpl {
            project_name: proj_name.to_string(),
            type_name: imp.type_name.clone(),
            trait_name: imp.trait_name.clone(),
        });
    }

    Ok(())
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
