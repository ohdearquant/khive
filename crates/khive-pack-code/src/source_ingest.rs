//! `code.ingest` L1 manifest edges, L1.5 import-scan edges, and L2 symbol
//! persistence (ADR-085 Amendment 2 B3-B6 and Amendment 5 F1-F3).
//!
//! L2 uses the language-neutral extractor shape to persist deterministic
//! UUID5 symbols, current module ownership stamps, and same-project edges.
//! Rust syntax errors retain source metadata, clear current declaration
//! ownership, increment `symbol_parse_failures`, and allow the sweep to
//! continue.
//!
//! Every entity write in this pipeline runs through the runtime secret gate
//! (ADR-085 D6 #4) via `upsert_entity`. A gate refusal quarantines that one
//! item — it is recorded in [`CodeSourceIngestReport::blocked`] and skipped —
//! rather than aborting the rest of the sweep, the same
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
use khive_runtime::{entity_fts_document, secret_gate, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::SqlStatement;
use khive_storage::{Direction, Edge, Entity, LinkId, NeighborQuery};
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extractor::{DeclKind, ExtractedDeclaration, ExtractedFile};
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

/// L2-only outcome counters, flattened into
/// [`CodeSourceIngestReport`] only when L2 was requested — an `l2: None`
/// report serializes with none of these five keys present, so the default
/// L1+L1.5 wire shape is byte-identical to the pre-L2 report.
#[derive(Debug, Default, serde::Serialize)]
pub struct CodeSourceIngestL2Report {
    /// Entity-plus-FTS symbol writes that created a new concept row.
    pub symbols_created: u64,
    /// Entity-plus-FTS symbol writes that refreshed an existing concept row.
    pub symbols_updated: u64,
    /// Unique unresolved call/type/impl references after the synchronous
    /// same-project resolution pass (nonfatal; not a complete call graph —
    /// see the module doc comment's `ExprCall` coverage-floor note).
    pub symbol_dependencies_unresolved: u64,
    /// Current L2 `depends_on`/`implements` edges written this sweep.
    pub symbol_edges_stamped: u64,
    /// Rust files whose L2 parse failed this sweep — the file keeps its
    /// source metadata but no `declaration_ids` ownership stamp, and its
    /// prior symbol rows (if any) are left untouched as history rather than
    /// exported as current.
    pub symbol_parse_failures: u64,
}

/// Outcome counters for one `code.ingest` call, mirroring `git.digest`'s
/// `IngestReport` shape (ADR-088 Amendment 1 precedent).
#[derive(Debug, Default, serde::Serialize)]
pub struct CodeSourceIngestReport {
    pub projects_created: u64,
    pub projects_updated: u64,
    pub modules_created: u64,
    pub modules_updated: u64,
    /// `None` unless L2 was requested (`enable_l2`); present with all-zero
    /// counters for a valid L2 pass over zero Rust files or zero
    /// declarations, so "L2 requested but nothing found" stays
    /// distinguishable from "L2 not requested".
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub l2: Option<CodeSourceIngestL2Report>,
    pub edges_created: u64,
    pub edges_updated: u64,
    pub unresolved_recorded: u64,
    pub unresolved_resolved: u64,
    /// Modules from this sweep's scan map whose entity was missing at stamp
    /// time — an F2 contract violation (every scanned module must carry
    /// coverage stamps), counted separately so it is machine-visible.
    pub coverage_stamps_missed: u64,
    /// Files dropped from the sweep because no source path could be derived
    /// for them at all (see the `source_path` fallback arm) — counted so a
    /// vanished module is visible instead of silent.
    pub files_dropped_without_source_path: u64,
    /// Files returned by the walk for which no language-specific module path
    /// could be derived — counted instead of silently skipping them.
    #[serde(default)]
    pub files_skipped_without_module_path: u64,
    /// Entity documents successfully written to the map database's FTS index.
    /// A successful ingest indexes every non-blocked entity upsert, so generic
    /// KG `search` and query-anchored `context` can read the resulting map.
    pub fts_indexed: u64,
    /// Sorted, deduplicated languages observed in manifests or source files
    /// accepted by at least one selected tier during this pass.
    pub languages: Vec<String>,
    /// Per-manifest / per-file failures that did not abort the pass (fail
    /// loud without silently dropping the rest of the run).
    pub warnings: Vec<String>,
    /// Count of per-item content writes refused by the runtime secret gate
    /// during this pass, independent of unrelated `warnings` (mirrors
    /// `git.digest`'s `writes_refused`).
    pub blocked_count: u64,
    /// Safe structured detail for every entry counted by `blocked_count`
    /// A gate-refused write is quarantined and skipped; it
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
    /// L1 manifest-dependency-edge tier. The wire default is `true`.
    pub enable_l1: bool,
    /// L1.5 regex import-scan tier. Wire default `true`.
    pub enable_l1_5: bool,
    /// L2 symbol/call-edge persistence tier (this module). Wire default
    /// `false` — opt-in only.
    pub enable_l2: bool,
}

fn record_observed_language(report: &mut CodeSourceIngestReport, language: &str) {
    if !report.languages.iter().any(|observed| observed == language) {
        report.languages.push(language.to_string());
        report.languages.sort();
    }
}

const IMPORT_DEPENDENCY_KIND: &str = "import";
const IMPORT_DEPENDENCY_SCOPE: &str = "build";
const UNVERSIONED_REVISION: &str = "unversioned";

#[derive(Debug)]
struct SourceSnapshot {
    root: PathBuf,
    revision: String,
    git_metadata_available: bool,
}

#[derive(Debug)]
struct ModuleScan {
    source_project: String,
    imports: Vec<UnresolvedSpec>,
}

type ManifestScopeIndex = BTreeMap<(String, String, String), BTreeSet<String>>;

async fn source_snapshot(ingest_root: &Path) -> SourceSnapshot {
    let fallback_root = ingest_root
        .canonicalize()
        .unwrap_or_else(|_| ingest_root.to_path_buf());
    let ingest_root = ingest_root.to_path_buf();
    let git_result = tokio::task::spawn_blocking(move || {
        let git_output = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&ingest_root)
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
            .map(PathBuf::from);
        let revision =
            git_output(&["rev-parse", "--verify", "HEAD"]).filter(|revision| !revision.is_empty());
        (root, revision)
    })
    .await
    .ok();

    let (git_root, git_revision) = git_result.unwrap_or((None, None));
    let git_metadata_available = git_root.is_some() && git_revision.is_some();
    SourceSnapshot {
        root: git_root.unwrap_or(fallback_root),
        revision: git_revision.unwrap_or_else(|| UNVERSIONED_REVISION.to_string()),
        git_metadata_available,
    }
}

fn source_path(file: &Path, source_root: &Path) -> Option<String> {
    // Canonicalization can fail on a racy or dangling walk entry; fall back
    // to the path as walked so the module still ingests with a
    // best-effort repository-relative path rather than vanishing from the
    // sweep. `source_path` is provenance metadata only — module identity
    // stays the uuid5 `(source_project, language, module_path)` triple — so
    // the fallback poisons no dedup invariant.
    let canonical_file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let canonical_root = source_root
        .canonicalize()
        .unwrap_or_else(|_| source_root.to_path_buf());
    let relative = canonical_file.strip_prefix(canonical_root).ok()?;
    let components: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            // to_string_lossy is deliberate: `source_path` is provenance
            // metadata only (module identity is the uuid5 triple), so a
            // replacement character in a non-UTF-8 component is acceptable.
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    (!components.is_empty()).then(|| components.join("/"))
}

/// `source_path` plus its ingest-relative fallback and drop-with-warning
/// arm, shared by every per-file walker in this pipeline (L1.5 import scan
/// and the L2 sweep) so both tiers record identical provenance for the same
/// file. Returns `None` only when no path is derivable at all (the file
/// module is dropped from the sweep this call — `report.warnings` and
/// `report.files_dropped_without_source_path` already reflect why).
fn derive_source_path(
    file: &Path,
    ingest_root: &Path,
    snapshot_root: &Path,
    report: &mut CodeSourceIngestReport,
) -> Option<String> {
    if let Some(path) = source_path(file, snapshot_root) {
        return Some(path);
    }
    // Best-effort provenance fallback: keep the module in the sweep under
    // its ingest-root-relative path (with a warning) instead of dropping it
    // — see `source_path`. Reachable even with a resolved git root:
    // `source_path` canonicalizes both ends independently, so a walked path
    // whose canonical form does not extend the canonical repository root (a
    // symlinked ingest path, or one side's canonicalize racing and failing)
    // makes `strip_prefix` fail and lands here.
    let fallback = match file.strip_prefix(ingest_root) {
        Ok(path) => {
            report.warnings.push(format!(
                "canonical repository-relative path unavailable for {}; \
                 falling back to the ingest-relative path",
                file.display()
            ));
            path
        }
        Err(_) => {
            report.warnings.push(format!(
                "canonical repository-relative path unavailable for {}; path is \
                 outside the ingest root and is recorded as-is",
                file.display()
            ));
            file
        }
    };
    let components: Vec<String> = fallback
        .components()
        .filter_map(|component| match component {
            // to_string_lossy is deliberate: provenance metadata only,
            // never part of module identity.
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    if components.is_empty() {
        // Practically unreachable — `file` was walked under `ingest_root`,
        // so the relative path always carries at least the file name — but
        // if it ever fires, a real module would vanish; count it and warn.
        report.warnings.push(format!(
            "no derivable source path for {}; module dropped from the sweep",
            file.display()
        ));
        report.files_dropped_without_source_path += 1;
        return None;
    }
    Some(components.join("/"))
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
    symbol_uuid(source_project, language, module_path, module_path, "module")
}

/// Deterministic L2 symbol identity:
/// `uuid5(CODE_INGEST_NAMESPACE, source_project | language | module_path |
/// name | canonical_kind)`, realized here the same way every other identity
/// in this module is (a stable JSON object into `uuid5_json`, not a literal
/// pipe-joined string — matches the existing `project_uuid`/`module_uuid`
/// convention). File-module anchors use `module_uuid`, whose identity names
/// the full file module path. Inline modules remain declarations: their
/// identity uses the containing module path and the declared module name, so
/// readers can distinguish them from file-module ownership anchors.
///
/// `canonical_kind` is one of `function | datatype | interface | module`
/// (`DeclKind::code_token`) — never a raw Rust syntax name, so storage
/// identity is stable across scanner refactors that only change how a
/// declaration's Rust-specific kind maps to these four buckets.
fn symbol_uuid(
    source_project: &str,
    language: &str,
    module_path: &str,
    name: &str,
    canonical_kind: &str,
) -> Uuid {
    uuid5_json(&json!({
        "kind": "code-source-symbol",
        "source_project": source_project,
        "language": language,
        "module_path": module_path,
        "name": name,
        "symbol_kind": canonical_kind,
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

/// Import-only scope for a given project pair is a deterministic function
/// of the current manifest index: within one run every spec for the same
/// pair derives its scope from the same `manifest_scopes` snapshot (or the
/// constant `build` fallback), and `reresolve_pass` repairs stored legacy
/// import scopes against the current index before any edge upsert — so the
/// "last writer" for a pair always writes the same scope, and two distinct
/// import-only scopes never merge onto one edge (no union needed; ADR-085
/// Amendment 5 F1's multi-scope arm is only reachable via manifest-declared
/// kinds).
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

/// `(source_project, language, alias)` -> `package` for renamed Cargo
/// dependencies. Rust source imports a renamed dependency under its alias —
/// the real crate name never appears in source — so an alias-form import
/// must resolve to the package's project identity: that is the entity the
/// dependency's own manifest ingest creates.
type ProjectRenames = HashMap<(String, String, String), String>;

/// Rewrite an import's target name alias->package when the governing
/// manifest renamed that dependency (see [`ProjectRenames`]); every other
/// name passes through unchanged.
fn canonical_project_target(
    project_renames: &ProjectRenames,
    source_project: &str,
    language: &str,
    target_project: &str,
) -> String {
    project_renames
        .get(&(
            source_project.to_string(),
            language.to_string(),
            target_project.to_string(),
        ))
        .cloned()
        .unwrap_or_else(|| target_project.to_string())
}

#[derive(Debug)]
struct DeclaredProjectImport {
    target: String,
    scope: &'static str,
    /// All declared targets that shared the normalized Rust identifier.
    /// An empty vector means there was no collision.
    normalization_matches: Vec<String>,
}

/// Resolve a declared import target and scope. On a Rust dash/underscore
/// normalization collision the lexicographically first declared target wins.
fn declared_project_import_target_and_scope(
    manifest_scopes: &ManifestScopeIndex,
    source_project: &str,
    language: &str,
    target_project: &str,
) -> Option<DeclaredProjectImport> {
    let exact_key = (
        source_project.to_string(),
        language.to_string(),
        target_project.to_string(),
    );
    if language != "rust" {
        return manifest_scopes
            .get(&exact_key)
            .map(|scopes| DeclaredProjectImport {
                target: target_project.to_string(),
                scope: preferred_import_scope(scopes),
                normalization_matches: Vec::new(),
            });
    }
    let normalized_target = target_project.replace('-', "_");
    let matches: Vec<_> = manifest_scopes
        .iter()
        .filter(|((source, declared_language, declared_target), _)| {
            source == source_project
                && declared_language == language
                && declared_target.replace('-', "_") == normalized_target
        })
        .collect();

    if matches.len() > 1 {
        let (key, scopes) = matches[0];
        return Some(DeclaredProjectImport {
            target: key.2.clone(),
            scope: preferred_import_scope(scopes),
            normalization_matches: matches.iter().map(|(key, _)| key.2.clone()).collect(),
        });
    }

    if let Some(scopes) = manifest_scopes.get(&exact_key) {
        return Some(DeclaredProjectImport {
            target: target_project.to_string(),
            scope: preferred_import_scope(scopes),
            normalization_matches: Vec::new(),
        });
    }

    matches.first().map(|(key, scopes)| DeclaredProjectImport {
        target: key.2.clone(),
        scope: preferred_import_scope(scopes),
        normalization_matches: Vec::new(),
    })
}

fn project_import_target_and_scope(
    manifest_scopes: &ManifestScopeIndex,
    project_renames: &ProjectRenames,
    source_project: &str,
    language: &str,
    target_project: &str,
) -> DeclaredProjectImport {
    let canonical =
        canonical_project_target(project_renames, source_project, language, target_project);
    declared_project_import_target_and_scope(manifest_scopes, source_project, language, &canonical)
        .unwrap_or(DeclaredProjectImport {
            target: canonical,
            scope: IMPORT_DEPENDENCY_SCOPE,
            normalization_matches: Vec::new(),
        })
}

fn report_normalization_collision(
    report: &mut CodeSourceIngestReport,
    source_project: &str,
    target_project: &str,
    resolution: &DeclaredProjectImport,
) {
    if resolution.normalization_matches.len() <= 1 {
        return;
    }
    let warning = format!(
        "Rust import target {target_project:?} in project {source_project:?} has a \
         dash/underscore normalization collision among declared targets {:?}; \
         lexicographically first declared target {:?} wins",
        resolution.normalization_matches, resolution.target
    );
    if !report.warnings.contains(&warning) {
        report.warnings.push(warning);
    }
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

/// Runs the runtime secret gate over `entity`'s name, description, and
/// properties, the same content the gate checks for every other write
/// (ADR-085 D6 #4). The direct storage-layer call `upsert_entity` wraps does
/// not run this check on its own path, so callers of this pipeline get no
/// gate coverage unless it happens here.
///
/// `description` is checked because L2 symbols store their exact
/// documentation text there — L1/L1.5 entities
/// never set `description`, so this is additive and does not change their
/// gate coverage.
fn gate_check(entity: &Entity) -> Result<(), RuntimeError> {
    secret_gate::check(&entity.name)?;
    if let Some(description) = &entity.description {
        secret_gate::check(description)?;
    }
    if let Some(properties) = &entity.properties {
        secret_gate::check_json(properties)?;
    }
    Ok(())
}

/// Upserts `entity` after running it through [`gate_check`]. Returns
/// `Ok(false)` without writing anything when the gate refuses the entity:
/// the refusal is recorded in `report.blocked` keyed by `file`, and the
/// caller moves on to the next item rather than aborting the whole ingest
/// (quarantine, don't abort, mirroring `git.digest`'s
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
    let document = entity_fts_document(&entity);
    rt.entities(token)?
        .upsert_entity(entity)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    // This direct ingest path intentionally bypasses the runtime create/update
    // verbs, so it must compose the same FTS write itself. Fail the call if the
    // index write fails: returning an apparently healthy but unsearchable map
    // makes an empty search result indistinguishable from a correct answer.
    rt.text(token)?
        .upsert_document(document)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(format!("entity FTS indexing: {e}")))?;
    report.fts_indexed += 1;
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

/// Get-or-create a project id from the shared per-sweep `project_ids` cache,
/// shared by every per-file walker in this pipeline (L1.5 import scan and
/// the L2 sweep) so both tiers resolve the same fallback project identity
/// for a manifestless source tree instead of re-deriving it independently.
/// Default L1/L1.5 calls retain the legacy name-only cache key so their
/// counters, FTS writes, and sweep clocks are unchanged. L2-selected calls
/// opt into a language component because L2 currentness is explicitly
/// per `(source_project, language)`.
/// Returns `Ok(None)` when the fallback project write is gate-refused (the
/// refusal is recorded in `report.blocked`, keyed by `file_label` — a real
/// on-disk location, never the content-derived project name);
/// callers must skip this file rather than indexing it.
#[allow(clippy::too_many_arguments)]
async fn ensure_project_id(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    project_ids: &mut HashMap<(String, String), Uuid>,
    proj_name: &str,
    file_label: &str,
    language: &str,
    per_language_project_stamps: bool,
    sweep_time: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Uuid>, CodeSourceIngestError> {
    let key = project_cache_key(proj_name, language, per_language_project_stamps);
    if let Some(id) = project_ids.get(&key) {
        return Ok(Some(*id));
    }
    let Some(id) = upsert_project(
        rt, token, proj_name, file_label, language, sweep_time, report,
    )
    .await?
    else {
        return Ok(None);
    };
    project_ids.insert(key, id);
    Ok(Some(id))
}

fn project_cache_key(
    project_name: &str,
    language: &str,
    per_language_project_stamps: bool,
) -> (String, String) {
    (
        project_name.to_string(),
        if per_language_project_stamps {
            language.to_string()
        } else {
            String::new()
        },
    )
}

/// Returns `Ok(None)` when the runtime secret gate refuses the write (the
/// refusal is recorded in `report.blocked`, keyed by `file`) — callers must
/// treat that module as absent from this sweep rather than indexing it.
///
/// Shared by the L1.5 import scan and the L2 sweep (both tiers upsert the
/// same file-module entity, keyed by the same `module_uuid`), so every
/// property NOT owned by the calling tier is preserved from the existing
/// row rather than reset: an L2-only pass must not erase L1.5's
/// `import_scan_status`/`import_specifier_count`/`unresolved_import_count`,
/// and an L1.5-only pass must not erase L2's `declaration_ids` ownership
/// stamp. When an L2 pass has detected changed content, `preserve_l2_state`
/// is false so the module update publishes the new source metadata and the
/// absence of current L2 ownership atomically. `import_scan_status` therefore
/// initializes to `"unscanned"` only when the row is new; an L1.5 pass
/// always overwrites it correctly afterward via `stamp_import_scan_coverage`
/// regardless of this initial value.
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
    preserve_l2_state: bool,
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
    let existing_props = existing.as_ref().and_then(|e| e.properties.as_ref());

    let mut props = serde_json::Map::new();
    props.insert("source_project".into(), json!(source_project));
    props.insert("language".into(), json!(language));
    props.insert("module_path".into(), json!(module_path));
    props.insert("source_path".into(), json!(source_path));
    props.insert("source_revision".into(), json!(source_revision));
    props.insert("content_hash".into(), json!(content_hash));
    props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
    // L1.5-owned fields: preserve verbatim, default only for a new row —
    // `stamp_import_scan_coverage` is the sole writer of the accurate value.
    for key in [
        "import_scan_status",
        "import_specifier_count",
        "unresolved_import_count",
    ] {
        if let Some(value) = existing_props.and_then(|p| p.get(key)) {
            props.insert(key.to_string(), value.clone());
        }
    }
    if !props.contains_key("import_scan_status") {
        props.insert("import_scan_status".into(), json!("unscanned"));
    }
    // L2-owned scan state is preserved by L1/L1.5 passes and unchanged L2
    // refreshes. A changed L2 input omits it in this same module upsert, so
    // readers cannot observe stale declaration ownership under new bytes.
    if preserve_l2_state {
        for key in ["declaration_ids", "l2_pending_impls", "l2_content_hash"] {
            if let Some(value) = existing_props.and_then(|p| p.get(key)) {
                props.insert(key.to_string(), value.clone());
            }
        }
    }
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
///
/// The three rebuilt fields — `dependency_kinds`, `dependency_scopes`, and
/// `language` — are the COMPLETE metadata set these edges carry: this
/// function (and its Amendment-2 predecessor `merge_dependency_kinds`,
/// which wrote the subset `{dependency_kinds, language}`) is the only
/// writer of `depends_on` edge metadata in this pipeline, and B7 dedicates
/// the map database to it, so no unknown fields exist to preserve.
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
#[derive(Clone, Copy)]
struct ReresolveTiers {
    l1: bool,
    l1_5: bool,
}

async fn reresolve_pass(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    manifest_scopes: &ManifestScopeIndex,
    project_renames: &ProjectRenames,
    tiers: ReresolveTiers,
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
            let selected = if spec.dependency_kind == IMPORT_DEPENDENCY_KIND {
                tiers.l1_5
            } else {
                tiers.l1
            };
            if !selected {
                still_unresolved.push(spec);
                continue;
            }
            if spec.target_kind == "project" {
                // Alias-form imports (and legacy alias-row manifest specs)
                // resolve to the package identity, never the alias.
                let canonical = canonical_project_target(
                    project_renames,
                    &source_project,
                    &spec.language,
                    &spec.specifier,
                );
                if canonical != spec.specifier {
                    spec.specifier = canonical;
                    changed = true;
                }
                if spec.dependency_kind == IMPORT_DEPENDENCY_KIND {
                    if let Some(target) = declared_project_import_target_and_scope(
                        manifest_scopes,
                        &source_project,
                        &spec.language,
                        &spec.specifier,
                    ) {
                        report_normalization_collision(
                            report,
                            &source_project,
                            &spec.specifier,
                            &target,
                        );
                        changed |= spec.specifier != target.target
                            || spec.dependency_scope != target.scope;
                        spec.specifier = target.target;
                        spec.dependency_scope = target.scope.to_string();
                    }
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
            // F2 contract: every module from a completed scan must carry
            // coverage stamps. A scan-map module missing here is a contract
            // violation — report it instead of silently skipping.
            report.warnings.push(format!(
                "module {module_id} from this sweep's scan map was missing at stamp time; \
                 coverage stamps skipped (F2 contract violation)"
            ));
            report.coverage_stamps_missed += 1;
            continue;
        };
        let source_label = module
            .properties
            .as_ref()
            .and_then(|properties| properties.get("source_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module_id.to_string());
        let mut props = match module.properties.clone() {
            Some(Value::Object(map)) => map,
            _ => {
                // `upsert_module` normally writes an object; anything else
                // means the row drifted outside this pipeline. Never rebuild
                // from nothing because that would destroy unrelated module
                // provenance properties.
                report.warnings.push(format!(
                    "module {module_id} has missing or non-object properties at stamp time; \
                     coverage stamp skipped (F2 contract violation)"
                ));
                report.coverage_stamps_missed += 1;
                continue;
            }
        };
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
        if !upsert_entity(rt, token, module, &source_label, report).await? {
            report.coverage_stamps_missed += 1;
        }
    }
    Ok(())
}

const SOURCE_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
];

fn collect_source_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SOURCE_SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            collect_source_files(&path, ext, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    Ok(())
}

/// L2's source walk resolves symlinks but never crosses the canonical ingest
/// root. Canonical directory de-duplication also prevents symlink cycles.
fn collect_l2_source_files(
    root: &Path,
    ext: &str,
    out: &mut Vec<PathBuf>,
    skipped_outside_root: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    fn visit(
        path: &Path,
        canonical_root: &Path,
        ext: &str,
        visited_dirs: &mut BTreeSet<PathBuf>,
        out: &mut Vec<PathBuf>,
        skipped: &mut Vec<PathBuf>,
    ) -> std::io::Result<()> {
        let canonical = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(_) => {
                skipped.push(path.to_path_buf());
                return Ok(());
            }
        };
        if !canonical.starts_with(canonical_root) {
            skipped.push(path.to_path_buf());
            return Ok(());
        }
        if canonical.is_dir() {
            if !visited_dirs.insert(canonical.clone()) {
                return Ok(());
            }
            for entry in fs::read_dir(&canonical)? {
                let entry = entry?;
                let entry_path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if SOURCE_SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                    continue;
                }
                visit(&entry_path, canonical_root, ext, visited_dirs, out, skipped)?;
            }
        } else if canonical.extension().and_then(|value| value.to_str()) == Some(ext) {
            out.push(canonical);
        }
        Ok(())
    }

    let canonical_root = fs::canonicalize(root)?;
    visit(
        &canonical_root,
        &canonical_root,
        ext,
        &mut BTreeSet::new(),
        out,
        skipped_outside_root,
    )?;
    out.sort();
    out.dedup();
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

/// Run one selected-tier ingest pass over `opts.path` into the runtime `rt`
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

    let snapshot = source_snapshot(opts.path).await;
    let mut report = CodeSourceIngestReport {
        source_revision: snapshot.revision.clone(),
        l2: opts.enable_l2.then(CodeSourceIngestL2Report::default),
        ..Default::default()
    };
    if !snapshot.git_metadata_available {
        report.warnings.push(format!(
            "git metadata unavailable for {}; source revision degraded to {UNVERSIONED_REVISION}",
            opts.path.display()
        ));
    }

    let mut manifest_scopes = ManifestScopeIndex::new();
    let mut project_renames = ProjectRenames::new();
    // The tuple supports L2's per-language project stamps while
    // `project_cache_key` collapses its language component for default
    // L1/L1.5 calls to preserve their established write/counter behavior.
    let mut project_ids: HashMap<(String, String), Uuid> = HashMap::new();

    // Manifest discovery supplies bounded identity, alias, and scope context
    // to L1.5 without implying L1 output. No selected L1/L1.5 tier means no
    // manifest walk, preserving the zero-write and L2-only boundaries.
    let manifests = if opts.enable_l1 || opts.enable_l1_5 {
        manifest::discover_manifests(opts.path, &opts.languages)
            .map_err(|e| CodeSourceIngestError::InvalidPath(opts.path.join(e.to_string())))?
    } else {
        Vec::new()
    };
    for manifest in &manifests {
        record_observed_language(&mut report, manifest.language);
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
        for (alias, package) in &manifest.renames {
            project_renames.insert(
                (
                    manifest.name.clone(),
                    manifest.language.to_string(),
                    alias.clone(),
                ),
                package.clone(),
            );
        }
    }

    // L1 writes project entities and manifest dependency edges.
    if opts.enable_l1 {
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
                // Gate-refused write, already recorded in report.blocked —
                // this project is absent from the sweep, skip it and keep
                // going.
                continue;
            };
            project_ids.insert(project_cache_key(&m.name, m.language, opts.enable_l2), id);
        }

        for m in &manifests {
            let Some(&source_id) =
                project_ids.get(&project_cache_key(&m.name, m.language, opts.enable_l2))
            else {
                // This manifest's own project write was gate-refused above;
                // nothing to hang dependency edges off of this sweep.
                continue;
            };
            let file_label = m.root.display().to_string();
            for (dep_name, dep_kind, dep_scope) in &m.dependencies {
                // A renamed dependency's alias row and package row both
                // index the same declared fact; canonicalizing the alias to
                // the package at record time makes the two rows produce one
                // identical spec (deduped by `record_unresolved`) targeting
                // the package's project identity — never a phantom alias
                // project.
                let specifier =
                    canonical_project_target(&project_renames, &m.name, m.language, dep_name);
                let spec = UnresolvedSpec {
                    specifier,
                    target_kind: "project".to_string(),
                    dependency_kind: dep_kind.clone(),
                    dependency_scope: dep_scope.clone(),
                    language: m.language.to_string(),
                };
                record_unresolved(rt, token, source_id, spec, &file_label, &mut report).await?;
            }
        }
    }

    // L1.5: regex import scan (module + project depends_on edges). Driven by
    // per-language file discovery across the whole ingest root — independent
    // of manifest discovery — so a manifestless source folder still yields
    // module/project entities and import edges under the basename-fallback
    // identity rule (ADR-085 Amendment 2 B4), rather than being silently
    // skipped for lack of a governing manifest.
    let mut module_scans = HashMap::new();
    if opts.enable_l1_5 {
        for language in opts.languages.iter().copied() {
            run_import_scan(
                rt,
                token,
                language,
                opts.path,
                &snapshot,
                &manifest_scopes,
                &project_renames,
                opts.enable_l2,
                opts.sweep_time,
                &mut project_ids,
                &mut module_scans,
                &mut report,
            )
            .await?;
        }
    }

    if opts.enable_l1 || opts.enable_l1_5 {
        reresolve_pass(
            rt,
            token,
            &manifest_scopes,
            &project_renames,
            ReresolveTiers {
                l1: opts.enable_l1,
                l1_5: opts.enable_l1_5,
            },
            opts.sweep_time,
            &mut report,
        )
        .await?;
    }
    if opts.enable_l1_5 {
        stamp_import_scan_coverage(rt, token, module_scans, &mut report).await?;
    }

    // L2: symbol/call-edge persistence (Rust-only; see the module doc
    // comment for scanner availability). A `languages` selection that
    // excludes "rust" must scan zero Rust symbols even with `enable_l2`.
    if opts.enable_l2 && opts.languages.contains("rust") {
        let mut state = run_l2_sweep(
            rt,
            token,
            opts.path,
            &snapshot,
            opts.sweep_time,
            &mut project_ids,
            &mut report,
        )
        .await?;
        l2_reresolve_pass(rt, token, opts.sweep_time, &mut state, &mut report).await?;
        refresh_unchanged_l2_edges(rt, token, opts.sweep_time, &mut state, &mut report).await?;
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_edges_stamped = state.stamped_edge_ids.len() as u64;
        }
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

#[allow(clippy::too_many_arguments)]
async fn run_import_scan(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    language: &'static str,
    ingest_root: &Path,
    snapshot: &SourceSnapshot,
    manifest_scopes: &ManifestScopeIndex,
    project_renames: &ProjectRenames,
    per_language_project_stamps: bool,
    sweep_time: DateTime<Utc>,
    project_ids: &mut HashMap<(String, String), Uuid>,
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
    if !files.is_empty() {
        record_observed_language(report, language);
    }

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
            report.files_skipped_without_module_path += 1;
            continue;
        };
        let Some(source_path) = derive_source_path(&file, ingest_root, &snapshot.root, report)
        else {
            continue;
        };

        let file_label = file.display().to_string();

        let Some(proj_id) = ensure_project_id(
            rt,
            token,
            project_ids,
            &proj_name,
            &file_label,
            language,
            per_language_project_stamps,
            sweep_time,
            report,
        )
        .await?
        else {
            // Gate-refused write, already recorded in report.blocked — move
            // on to the next file.
            continue;
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
            true,
            sweep_time,
            &file_label,
            report,
        )
        .await?
        else {
            // Gate-refused write, already recorded in report.blocked — move
            // on to the next file.
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
        let is_package = file.file_name().is_some_and(|name| name == "__init__.py");
        for raw in imports::extract_raw_imports(language, &content) {
            let resolved = if language == "typescript" && raw.starts_with('.') {
                let rel_dir = file_dir.strip_prefix(&proj_root).unwrap_or(Path::new(""));
                Resolved::IntraModule(imports::resolve_relative_ts_module(rel_dir, &raw))
            } else {
                imports::classify_import(language, &raw, &module_path, &proj_name, is_package)
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
                    let resolution = project_import_target_and_scope(
                        manifest_scopes,
                        project_renames,
                        &proj_name,
                        language,
                        &target_name,
                    );
                    report_normalization_collision(report, &proj_name, &target_name, &resolution);
                    let spec = UnresolvedSpec {
                        specifier: resolution.target,
                        target_kind: "project".to_string(),
                        dependency_kind: IMPORT_DEPENDENCY_KIND.to_string(),
                        dependency_scope: resolution.scope.to_string(),
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

// ===== L2: symbol/call-edge persistence =====
//
// Declaration/impl shapes (`DeclKind`, `CallRef`, `TypeRef`,
// `ExtractedDeclaration`, `ExtractedImpl`, `ExtractedFile`) live in
// `crate::extractor`; this module consumes that language-neutral contract
// rather than defining a parallel copy.

#[derive(Debug, Clone, PartialEq, Eq)]
struct L2OwnerKey {
    source_project: String,
    language: String,
}

#[derive(Debug, Default)]
struct L2SweepState {
    /// Declarations proven current by this L2 invocation, never ambient
    /// ownership left by a prior sweep or an earlier tier in this call.
    current_declarations: HashMap<Uuid, L2OwnerKey>,
    /// File modules whose L2 ownership was successfully refreshed/stamped.
    current_modules: HashMap<Uuid, L2OwnerKey>,
    /// Current declarations reused without parsing. Their outgoing edges are
    /// refreshed only after every target file's ownership is finalized.
    unchanged_declarations: BTreeSet<Uuid>,
    /// Natural L2 dependency/implementation edges successfully stamped this
    /// sweep; this set is the authority for `symbol_edges_stamped`.
    stamped_edge_ids: BTreeSet<Uuid>,
}

impl L2SweepState {
    fn mark_current_declarations(&mut self, ids: &[Uuid], source_project: &str, language: &str) {
        let owner = L2OwnerKey {
            source_project: source_project.to_string(),
            language: language.to_string(),
        };
        for id in ids {
            self.current_declarations.insert(*id, owner.clone());
        }
    }

    fn mark_current_module(&mut self, id: Uuid, source_project: &str, language: &str) {
        self.current_modules.insert(
            id,
            L2OwnerKey {
                source_project: source_project.to_string(),
                language: language.to_string(),
            },
        );
    }

    fn is_current_declaration(
        &self,
        id: Uuid,
        source_project: &str,
        language: &str,
        current_file_ids: &BTreeSet<Uuid>,
    ) -> bool {
        current_file_ids.contains(&id)
            || self.current_declarations.get(&id).is_some_and(|owner| {
                owner.source_project == source_project && owner.language == language
            })
    }
}

/// L2 Rust source parsing: the real syn-based scan (`scanner_rust`) adapted
/// into the language-neutral extractor shape (`extractor::from_rust_scan`).
/// A `syn::Error` (i.e. content that does not parse as a Rust file) surfaces
/// through the parse-failure channel: retain source
/// metadata, no `declaration_ids` stamp, increment `symbol_parse_failures`,
/// warn, retry next sweep) instead of aborting the sweep.
fn parse_rust_file(content: &str) -> Result<ExtractedFile, String> {
    crate::scanner_rust::scan_rust_source(content)
        .map(crate::extractor::from_rust_scan)
        .map_err(|e| e.to_string())
}

/// Join a file module's own path with a declaration's in-file nesting
/// segments into the absolute module path it lives in. Rust-only, so the
/// separator is always `::` (`module_path_separator("rust")`).
fn resolve_module_path(file_module_path: &str, module_segments: &[String]) -> String {
    if module_segments.is_empty() {
        file_module_path.to_string()
    } else {
        format!("{file_module_path}::{}", module_segments.join("::"))
    }
}

/// Resolve the immediate containment owner for an extracted declaration.
/// Top-level declarations belong to the file module; nested declarations
/// belong to the inline-module declaration named by the last segment.
fn declaration_owner_id(
    source_project: &str,
    language: &str,
    file_module_path: &str,
    file_module_id: Uuid,
    module_segments: &[String],
) -> Uuid {
    match module_segments.split_last() {
        None => file_module_id,
        Some((name, parent_segments)) => symbol_uuid(
            source_project,
            language,
            &resolve_module_path(file_module_path, parent_segments),
            name,
            "module",
        ),
    }
}

/// Decide whether an L2-selected file needs (re)parsing this sweep
/// unchanged content with a valid
/// existing ownership stamp reuses that stamp without reparsing; anything
/// else — changed content, or no stamp yet even with unchanged content —
/// parses. Pure and independent of storage so the boundary is directly
/// testable.
fn l2_needs_reparse(
    existing_content_hash: Option<&str>,
    existing_declaration_ids: Option<&Value>,
    new_content_hash: &str,
) -> bool {
    existing_content_hash != Some(new_content_hash)
        || existing_declaration_ids
            .and_then(read_declaration_ids)
            .is_none()
}

fn read_declaration_ids(value: &Value) -> Option<Vec<Uuid>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().and_then(|id| Uuid::parse_str(id).ok()))
        .collect()
}

/// Candidate target symbol ids for a call/type-reference path, tried in
/// priority order: same-declaring-module bare name first (a sibling
/// function/type reached without a path prefix), then the path's own
/// module-prefix-as-declared. There is no real name resolver here — L2 is a
/// documented syntax coverage floor — so this is a
/// best-effort heuristic, not exhaustive Rust name resolution. Because every
/// candidate is built from the caller's own `source_project`/`language`,
/// resolution can never cross a project or language boundary: a reference
/// that would only resolve elsewhere simply stays unresolved rather than
/// producing a cross-project edge (same-source-project enforcement is
/// structural here, not a separate rejection check).
fn symbol_candidate_ids(
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    segments: &[String],
    evidence: &str,
) -> Vec<Uuid> {
    let kinds: &[&str] = match evidence {
        "call" => &["function", "datatype", "interface"],
        "type_reference" => &["datatype", "interface"],
        _ => return Vec::new(),
    };
    symbol_candidate_ids_for_kinds(
        source_project,
        language,
        declaring_module_path,
        segments,
        kinds,
    )
}

fn symbol_candidate_ids_for_kinds(
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    segments: &[String],
    kinds: &[&str],
) -> Vec<Uuid> {
    let Some((name, prefix_segments)) = segments.split_last() else {
        return Vec::new();
    };
    let module_paths = candidate_module_paths(declaring_module_path, prefix_segments);
    let canonical_kinds: Vec<&str> = kinds
        .iter()
        .filter_map(|kind| DeclKind::from_code_token(kind))
        .map(DeclKind::code_token)
        .collect();
    let mut candidates = Vec::with_capacity(canonical_kinds.len() * module_paths.len());
    for module_path in module_paths {
        for kind in &canonical_kinds {
            candidates.push(symbol_uuid(
                source_project,
                language,
                &module_path,
                name,
                kind,
            ));
        }
    }
    candidates
}

fn candidate_module_paths(declaring_module_path: &str, prefix: &[String]) -> Vec<String> {
    if prefix.is_empty() {
        return vec![declaring_module_path.to_string()];
    }

    let mut paths = Vec::new();
    match prefix[0].as_str() {
        "crate" => push_module_path_variants(&mut paths, prefix.join("::")),
        "self" => {
            let suffix = prefix[1..].join("::");
            let path = if suffix.is_empty() {
                declaring_module_path.to_string()
            } else {
                format!("{declaring_module_path}::{suffix}")
            };
            push_module_path_variants(&mut paths, path);
        }
        "super" => {
            let mut base: Vec<String> = declaring_module_path
                .split("::")
                .map(str::to_string)
                .collect();
            let count = prefix
                .iter()
                .take_while(|segment| segment.as_str() == "super")
                .count();
            let max_ascents = if base.first().is_some_and(|segment| segment == "crate") {
                base.len().saturating_sub(1)
            } else {
                base.len()
            };
            if count > max_ascents {
                return Vec::new();
            }
            for _ in 0..count {
                base.pop();
            }
            if base.is_empty() {
                base.push("crate".to_string());
            }
            let suffix = prefix[count..].join("::");
            let base = base.join("::");
            let path = if suffix.is_empty() {
                base
            } else {
                format!("{base}::{suffix}")
            };
            push_module_path_variants(&mut paths, path);
        }
        _ => return Vec::new(),
    }
    paths
}

fn push_module_path_variants(paths: &mut Vec<String>, path: String) {
    let mut variants = vec![path.clone()];
    if let Some(stripped) = path.strip_prefix("crate::") {
        variants.push(stripped.to_string());
    }
    for variant in variants {
        if !variant.is_empty() && !paths.contains(&variant) {
            paths.push(variant);
        }
    }
}

/// A `uuid5`-recomputable unresolved call/type reference recorded on the
/// *declaring* symbol entity (mirrors L1.5's `UnresolvedSpec` on
/// project/module entities). Kept content-hash-free by the same design: only
/// the fields needed to retry resolution are stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct L2UnresolvedRef {
    segments: Vec<String>,
    evidence: String,
}

fn read_l2_unresolved(properties: &Value) -> Vec<L2UnresolvedRef> {
    properties
        .get("l2_unresolved_references")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Records `reference` on `entity_id`'s pending list (deduped). Returns
/// `true` only when the reference was newly recorded — callers use this to
/// count *unique* unresolved references, matching an already-pending
/// reference re-observed on a later sweep costing nothing extra.
async fn record_l2_unresolved(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
    reference: L2UnresolvedRef,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let Some(mut entity) = get_entity_opt(rt, token, entity_id).await? else {
        return Ok(false);
    };
    let mut list = entity
        .properties
        .as_ref()
        .map(read_l2_unresolved)
        .unwrap_or_default();
    if list.contains(&reference) {
        return Ok(false);
    }
    list.push(reference);
    let mut props = entity
        .properties
        .clone()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    props.insert(
        "l2_unresolved_references".into(),
        serde_json::to_value(&list).expect("serializes"),
    );
    entity.properties = Some(Value::Object(props));
    upsert_entity(rt, token, entity, file_label, report).await
}

/// Attempt immediate same-project resolution of one call/type reference
/// declared by `declaring_id`; on success upserts (or refreshes) a
/// `depends_on` edge with the given evidence, on failure records a pending
/// reference for the reresolve pass. Nonfatal either way.
#[allow(clippy::too_many_arguments)]
async fn resolve_l2_reference(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    declaring_id: Uuid,
    current_file_ids: &BTreeSet<Uuid>,
    segments: &[String],
    evidence: &str,
    file_label: &str,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    if segments.is_empty() {
        return Ok(());
    }
    let mut target = None;
    let mut suppressed_self_type = false;
    for candidate in symbol_candidate_ids(
        source_project,
        language,
        declaring_module_path,
        segments,
        evidence,
    ) {
        if candidate == declaring_id && evidence == "type_reference" {
            suppressed_self_type = true;
            break;
        }
        if state.is_current_declaration(candidate, source_project, language, current_file_ids) {
            target = Some(candidate);
            break;
        }
    }
    match target {
        Some(target_id) => {
            upsert_l2_depends_on(
                rt,
                token,
                declaring_id,
                target_id,
                evidence,
                language,
                sweep_time,
                state,
                report,
            )
            .await?;
        }
        None if !suppressed_self_type => {
            let reference = L2UnresolvedRef {
                segments: segments.to_vec(),
                evidence: evidence.to_string(),
            };
            record_l2_unresolved(rt, token, declaring_id, reference, file_label, report).await?;
        }
        None => {}
    }
    Ok(())
}

/// Sorted-set-union evidence merge for one L2 `depends_on` edge — repeated
/// evidence (e.g. the same call observed on re-ingest) folds onto the
/// existing array rather than duplicating it, mirroring
/// `merge_dependency_metadata`'s established pattern for L1 edges.
fn merge_l2_evidence(
    existing_metadata: Option<&Value>,
    new_evidence: &str,
    language: &str,
    now: DateTime<Utc>,
) -> Value {
    let mut evidence: BTreeSet<String> = existing_metadata
        .and_then(|m| m.get("l2_evidence"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    evidence.insert(new_evidence.to_string());
    json!({
        "l2_derived": true,
        "l2_evidence": evidence.into_iter().collect::<Vec<_>>(),
        "language": language,
        "last_seen_at": now.to_rfc3339(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn upsert_l2_depends_on(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
    evidence: &str,
    language: &str,
    now: DateTime<Utc>,
    state: &mut L2SweepState,
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
    let metadata = merge_l2_evidence(
        existing.as_ref().and_then(|e| e.metadata.as_ref()),
        evidence,
        language,
        now,
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
    state.stamped_edge_ids.insert(edge_id);
    if existed {
        report.edges_updated += 1;
    } else {
        report.edges_created += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn upsert_l2_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    type_id: Uuid,
    trait_id: Uuid,
    language: &str,
    now: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::Implements, type_id, trait_id);
    let link_id = LinkId::from(edge_id);
    let graph = rt.graph(token)?;
    let existing = graph
        .get_edge(link_id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let existed = existing.is_some();
    graph
        .upsert_edge(Edge {
            id: link_id,
            namespace: token.namespace().as_str().to_string(),
            source_id: type_id,
            target_id: trait_id,
            relation: EdgeRelation::Implements,
            weight: 1.0,
            created_at: existing.as_ref().map(|edge| edge.created_at).unwrap_or(now),
            updated_at: now,
            deleted_at: None,
            metadata: Some(json!({
                "l2_derived": true,
                "language": language,
                "last_seen_at": now.to_rfc3339(),
            })),
            target_backend: None,
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    state.stamped_edge_ids.insert(edge_id);
    if existed {
        report.edges_updated += 1;
    } else {
        report.edges_created += 1;
    }
    Ok(())
}

/// Attempt immediate same-project resolution of one positive `impl Trait for
/// Type`. Unlike a call/type reference, an impl has no declaring storage
/// entity of its own, so a failed
/// resolution is recorded as a pending impl on the *file module* instead,
/// for the reresolve pass to retry.
#[allow(clippy::too_many_arguments)]
async fn resolve_l2_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    module_id: Uuid,
    containing_module_path: &str,
    current_file_ids: &BTreeSet<Uuid>,
    type_path: &[String],
    trait_path: &[String],
    file_label: &str,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    if type_path.is_empty() || trait_path.is_empty() {
        return Ok(());
    }
    let type_id = find_first_current(
        state,
        source_project,
        language,
        symbol_candidate_ids_for_kinds(
            source_project,
            language,
            containing_module_path,
            type_path,
            &["datatype"],
        ),
        current_file_ids,
    );
    let trait_id = find_first_current(
        state,
        source_project,
        language,
        symbol_candidate_ids_for_kinds(
            source_project,
            language,
            containing_module_path,
            trait_path,
            &["interface"],
        ),
        current_file_ids,
    );
    match (type_id, trait_id) {
        (Some(type_id), Some(trait_id)) => {
            upsert_l2_implements(
                rt, token, type_id, trait_id, language, sweep_time, state, report,
            )
            .await?;
        }
        _ => {
            record_l2_pending_impl(
                rt,
                token,
                module_id,
                type_path.to_vec(),
                trait_path.to_vec(),
                file_label,
                report,
            )
            .await?;
        }
    }
    Ok(())
}

fn find_first_current(
    state: &L2SweepState,
    source_project: &str,
    language: &str,
    candidates: Vec<Uuid>,
    current_file_ids: &BTreeSet<Uuid>,
) -> Option<Uuid> {
    candidates.into_iter().find(|candidate| {
        state.is_current_declaration(*candidate, source_project, language, current_file_ids)
    })
}

/// A `uuid5`-recomputable unresolved positive impl, recorded on the *file
/// module* entity that declared it (mirrors [`L2UnresolvedRef`] on symbol
/// entities — see [`resolve_l2_implements`]'s doc comment for why the
/// attachment point differs).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct L2PendingImpl {
    type_path: Vec<String>,
    trait_path: Vec<String>,
}

fn read_l2_pending_impls(properties: &Value) -> Vec<L2PendingImpl> {
    properties
        .get("l2_pending_impls")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

async fn record_l2_pending_impl(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    type_path: Vec<String>,
    trait_path: Vec<String>,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let Some(mut module) = get_entity_opt(rt, token, module_id).await? else {
        return Ok(false);
    };
    let entry = L2PendingImpl {
        type_path,
        trait_path,
    };
    let mut list = module
        .properties
        .as_ref()
        .map(read_l2_pending_impls)
        .unwrap_or_default();
    if list.contains(&entry) {
        return Ok(false);
    }
    list.push(entry);
    let mut props = module
        .properties
        .clone()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    props.insert(
        "l2_pending_impls".into(),
        serde_json::to_value(&list).expect("serializes"),
    );
    module.properties = Some(Value::Object(props));
    upsert_entity(rt, token, module, file_label, report).await
}

/// Upsert (create or refresh) the `contains` edge from a declaration's
/// owning module to the declaration itself.
#[derive(Clone, Copy)]
struct L2ContainmentStamp<'a> {
    language: &'a str,
    sweep_time: DateTime<Utc>,
}

async fn stamp_containment_edge(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    owner_id: Uuid,
    child_id: Uuid,
    stamp: L2ContainmentStamp<'_>,
    preserve_non_l2_metadata: bool,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::Contains, owner_id, child_id);
    let link_id = LinkId::from(edge_id);
    let graph = rt.graph(token)?;
    let existing = graph
        .get_edge(link_id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let metadata = match existing.as_ref().and_then(|edge| edge.metadata.as_ref()) {
        Some(metadata)
            if preserve_non_l2_metadata
                && !metadata
                    .get("l2_derived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
        {
            metadata.clone()
        }
        _ => json!({
            "l2_derived": true,
            "language": stamp.language,
            "last_seen_at": stamp.sweep_time.to_rfc3339(),
        }),
    };
    graph
        .upsert_edge(Edge {
            id: link_id,
            namespace: token.namespace().as_str().to_string(),
            source_id: owner_id,
            target_id: child_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            created_at: existing
                .as_ref()
                .map(|edge| edge.created_at)
                .unwrap_or(stamp.sweep_time),
            updated_at: stamp.sweep_time,
            deleted_at: None,
            metadata: Some(metadata),
            target_backend: None,
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    if existing.is_some() {
        report.edges_updated += 1;
    } else {
        report.edges_created += 1;
    }
    Ok(())
}

/// Upsert one declaration's `concept` entity. Returns `Ok(None)` when the
/// runtime secret gate refuses the write (recorded in `report.blocked`,
/// keyed by `file_label`) — callers must treat this declaration as absent
/// from this sweep. Also returns the declaration's own absolute module path
/// (identical to `containing_module_path` for non-`Module` kinds; the
/// nested path for an inline `Module` declaration).
#[allow(clippy::too_many_arguments)]
async fn upsert_declaration(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    containing_module_path: &str,
    decl: &ExtractedDeclaration,
    source_path: &str,
    source_revision: &str,
    sweep_time: DateTime<Utc>,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<(Uuid, String)>, CodeSourceIngestError> {
    let canonical_kind = decl.kind.code_token();
    let id = symbol_uuid(
        source_project,
        language,
        containing_module_path,
        &decl.name,
        canonical_kind,
    );
    let own_module_path = if decl.kind == DeclKind::Module {
        format!("{containing_module_path}::{}", decl.name)
    } else {
        containing_module_path.to_string()
    };
    let existing = get_entity_opt(rt, token, id).await?;
    let is_new = existing.is_none();

    let mut props = serde_json::Map::new();
    props.insert("source_project".into(), json!(source_project));
    props.insert("language".into(), json!(language));
    // `module_path` is the containing module used in the UUID preimage for
    // every declaration, including inline-module declarations. The returned
    // `own_module_path` is only traversal context for that module's children.
    props.insert("module_path".into(), json!(containing_module_path));
    props.insert("source_path".into(), json!(source_path));
    props.insert("source_revision".into(), json!(source_revision));
    props.insert("content_hash".into(), json!(decl.content_hash));
    props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));

    let mut entity = Entity::new(token.namespace().as_str(), "concept", decl.name.clone())
        .with_entity_type(Some(canonical_kind));
    entity.id = id;
    entity.description = decl.description.clone();
    entity.properties = Some(Value::Object(props));
    let now = ts(sweep_time);
    entity.created_at = existing.as_ref().map(|e| e.created_at).unwrap_or(now);
    entity.updated_at = now;

    if !upsert_entity(rt, token, entity, file_label, report).await? {
        return Ok(None);
    }
    if let Some(l2) = report.l2.as_mut() {
        if is_new {
            l2.symbols_created += 1;
        } else {
            l2.symbols_updated += 1;
        }
    }
    Ok(Some((id, own_module_path)))
}

/// Remove the module's `declaration_ids` ownership stamp (a failed parse
/// leaves the module with source metadata but no current coverage stamp)
/// without touching any other property, including prior symbol
/// rows, which remain as history rather than being exported as current.
async fn clear_l2_ownership(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let Some(mut module) = get_entity_opt(rt, token, module_id).await? else {
        return Ok(());
    };
    let Some(mut props) = module
        .properties
        .clone()
        .and_then(|v| v.as_object().cloned())
    else {
        return Ok(());
    };
    let removed_ownership = props.remove("declaration_ids").is_some();
    let removed_pending_impls = props.remove("l2_pending_impls").is_some();
    let removed_content_hash = props.remove("l2_content_hash").is_some();
    if !removed_ownership && !removed_pending_impls && !removed_content_hash {
        return Ok(()); // already un-stamped, nothing to clear
    }
    module.properties = Some(Value::Object(props));
    upsert_entity(rt, token, module, file_label, report).await?;
    Ok(())
}

/// Stamp the module's current-coverage `declaration_ids` (sorted, deduped)
/// after a successful parse — the authoritative "this module's declarations
/// as of `content_hash`/`source_revision`" marker.
async fn stamp_l2_declarations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    declaration_ids: &[Uuid],
    content_hash: &str,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let Some(mut module) = get_entity_opt(rt, token, module_id).await? else {
        report.warnings.push(format!(
            "L2 module {module_id} from this sweep was missing at stamp time; \
             declaration_ids not recorded"
        ));
        return Ok(());
    };
    let mut props = match module.properties.clone() {
        Some(Value::Object(map)) => map,
        _ => {
            report.warnings.push(format!(
                "L2 module {module_id} has missing or non-object properties at stamp time; \
                 declaration_ids not recorded"
            ));
            return Ok(());
        }
    };
    props.insert(
        "declaration_ids".into(),
        json!(declaration_ids
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()),
    );
    props.insert("l2_content_hash".into(), json!(content_hash));
    module.properties = Some(Value::Object(props));
    upsert_entity(rt, token, module, file_label, report).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn refresh_l2_declarations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    source_path: &str,
    source_revision: &str,
    sweep_time: DateTime<Utc>,
    file_label: &str,
    declaration_ids: &[Uuid],
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let mut declarations = Vec::with_capacity(declaration_ids.len());
    for id in declaration_ids {
        let Some(entity) = get_entity_opt(rt, token, *id).await? else {
            return Ok(false);
        };
        let canonical_kind = entity
            .entity_type
            .as_deref()
            .and_then(DeclKind::from_code_token);
        let properties = entity.properties.as_ref();
        let matches_owner = properties
            .and_then(|value| value.get("source_project"))
            .and_then(Value::as_str)
            == Some(source_project)
            && properties
                .and_then(|value| value.get("language"))
                .and_then(Value::as_str)
                == Some(language);
        if canonical_kind.is_none() || !matches_owner {
            return Ok(false);
        }
        declarations.push(entity);
    }

    for mut declaration in declarations {
        let mut properties = declaration
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        properties.insert("source_path".into(), json!(source_path));
        properties.insert("source_revision".into(), json!(source_revision));
        properties.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        declaration.properties = Some(Value::Object(properties));
        declaration.updated_at = ts(sweep_time);
        if !upsert_entity(rt, token, declaration, file_label, report).await? {
            return Ok(false);
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbols_updated += 1;
        }
    }
    Ok(true)
}

/// Persist one L2-selected Rust file's parse outcome. On failure, invalidate
/// current ownership without touching history; on success, upsert every declaration, its
/// containment edge, its same-project call/type-reference resolution, every
/// positive impl, then stamp the module's `declaration_ids` coverage.
#[allow(clippy::too_many_arguments)]
async fn persist_l2_file(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    module_id: Uuid,
    module_path: &str,
    source_path: &str,
    source_revision: &str,
    content_hash: &str,
    parse: Result<&ExtractedFile, &str>,
    sweep_time: DateTime<Utc>,
    file_label: &str,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Vec<Uuid>>, CodeSourceIngestError> {
    let parsed = match parse {
        Err(message) => {
            if let Some(l2) = report.l2.as_mut() {
                l2.symbol_parse_failures += 1;
            }
            report
                .warnings
                .push(format!("L2 parse failed for {file_label}: {message}"));
            clear_l2_ownership(rt, token, module_id, file_label, report).await?;
            return Ok(None);
        }
        Ok(parsed) => parsed,
    };

    // Phase A: upsert every declaration's entity first, independent of
    // resolution order, so phase B's containment/dependency edges can
    // target any declaration in this file regardless of list position.
    let mut declaration_ids: Vec<Uuid> = Vec::new();
    let mut declared: Vec<(Uuid, String, &ExtractedDeclaration)> = Vec::new();
    let mut refused_inline_modules = BTreeSet::new();
    for decl in &parsed.declarations {
        let containing_module_path = resolve_module_path(module_path, &decl.module_segments);
        if refused_inline_modules.iter().any(|refused: &String| {
            containing_module_path == *refused
                || containing_module_path.starts_with(&format!("{refused}::"))
        }) {
            continue;
        }
        let Some((id, _own_path)) = upsert_declaration(
            rt,
            token,
            source_project,
            language,
            &containing_module_path,
            decl,
            source_path,
            source_revision,
            sweep_time,
            file_label,
            report,
        )
        .await?
        else {
            if decl.kind == DeclKind::Module {
                refused_inline_modules.insert(format!("{containing_module_path}::{}", decl.name));
            }
            continue; // gate-refused, already recorded in report.blocked
        };
        declaration_ids.push(id);
        declared.push((id, containing_module_path, decl));
    }
    let current_file_ids: BTreeSet<Uuid> = declaration_ids.iter().copied().collect();

    // Phase B: containment + same-project call/type-reference resolution.
    for (id, containing_module_path, decl) in &declared {
        let owner_id = declaration_owner_id(
            source_project,
            language,
            module_path,
            module_id,
            &decl.module_segments,
        );
        stamp_containment_edge(
            rt,
            token,
            owner_id,
            *id,
            L2ContainmentStamp {
                language,
                sweep_time,
            },
            false,
            report,
        )
        .await?;

        for call in &decl.calls {
            resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &call.segments,
                "call",
                file_label,
                sweep_time,
                state,
                report,
            )
            .await?;
        }
        for type_ref in &decl.type_refs {
            resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &type_ref.segments,
                "type_reference",
                file_label,
                sweep_time,
                state,
                report,
            )
            .await?;
        }
    }

    // Phase C: positive trait implementations.
    for imp in &parsed.impls {
        let containing_module_path = resolve_module_path(module_path, &imp.module_segments);
        resolve_l2_implements(
            rt,
            token,
            source_project,
            language,
            module_id,
            &containing_module_path,
            &current_file_ids,
            &imp.type_path,
            &imp.trait_path,
            file_label,
            sweep_time,
            state,
            report,
        )
        .await?;
    }

    declaration_ids.sort();
    declaration_ids.dedup();
    stamp_l2_declarations(
        rt,
        token,
        module_id,
        &declaration_ids,
        content_hash,
        file_label,
        report,
    )
    .await?;
    Ok(Some(declaration_ids))
}

/// Walk every `.rs` file under `ingest_root`, ensure its project/module L2
/// ownership scaffolding exists, and (re)parse it when needed
/// Rust-only; other-language selections
/// never reach this function (`run_code_ingest` only calls it when L2 is
/// enabled, and L2 itself scans Rust exclusively).
async fn run_l2_sweep(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ingest_root: &Path,
    snapshot: &SourceSnapshot,
    sweep_time: DateTime<Utc>,
    project_ids: &mut HashMap<(String, String), Uuid>,
    report: &mut CodeSourceIngestReport,
) -> Result<L2SweepState, CodeSourceIngestError> {
    const LANGUAGE: &str = "rust";
    let mut state = L2SweepState::default();
    let Some(ext) = imports::extension_for_language(LANGUAGE) else {
        return Ok(state);
    };
    let canonical_ingest_root = match fs::canonicalize(ingest_root) {
        Ok(path) => path,
        Err(error) => {
            report.warnings.push(format!(
                "canonicalizing L2 ingest root {}: {error}",
                ingest_root.display()
            ));
            return Ok(state);
        }
    };
    let mut files = Vec::new();
    let mut skipped_outside_root = Vec::new();
    if let Err(e) = collect_l2_source_files(
        &canonical_ingest_root,
        ext,
        &mut files,
        &mut skipped_outside_root,
    ) {
        report
            .warnings
            .push(format!("walking {}: {e}", ingest_root.display()));
        return Ok(state);
    }
    for skipped in skipped_outside_root {
        report.warnings.push(format!(
            "L2 skipped source outside the canonical ingest root: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    if !files.is_empty() {
        record_observed_language(report, LANGUAGE);
    }

    for file in files {
        let Some(file_dir) = file.parent() else {
            continue;
        };
        let (proj_root, proj_name) =
            manifest::find_governing_manifest(file_dir, &canonical_ingest_root, LANGUAGE)
                .unwrap_or_else(|| {
                    (
                        canonical_ingest_root.clone(),
                        basename_project_name(ingest_root),
                    )
                });
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, LANGUAGE) else {
            report.files_skipped_without_module_path += 1;
            continue;
        };
        let Some(source_path) =
            derive_source_path(&file, &canonical_ingest_root, &snapshot.root, report)
        else {
            continue;
        };
        let file_label = file.display().to_string();

        let Some(proj_id) = ensure_project_id(
            rt,
            token,
            project_ids,
            &proj_name,
            &file_label,
            LANGUAGE,
            true,
            sweep_time,
            report,
        )
        .await?
        else {
            continue;
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

        let precomputed_module_id = module_uuid(&proj_name, LANGUAGE, &module_path);
        let existing_module = get_entity_opt(rt, token, precomputed_module_id).await?;
        let needs_reparse = l2_needs_reparse(
            existing_module
                .as_ref()
                .and_then(|e| e.properties.as_ref())
                .and_then(|p| p.get("l2_content_hash"))
                .and_then(Value::as_str),
            existing_module
                .as_ref()
                .and_then(|e| e.properties.as_ref())
                .and_then(|p| p.get("declaration_ids")),
            &hash,
        );

        let Some(module_id) = upsert_module(
            rt,
            token,
            &proj_name,
            LANGUAGE,
            &module_path,
            &source_path,
            &snapshot.revision,
            &hash,
            !needs_reparse,
            sweep_time,
            &file_label,
            report,
        )
        .await?
        else {
            continue;
        };

        stamp_containment_edge(
            rt,
            token,
            proj_id,
            module_id,
            L2ContainmentStamp {
                language: LANGUAGE,
                sweep_time,
            },
            true,
            report,
        )
        .await?;

        if !needs_reparse {
            let declaration_ids = existing_module
                .as_ref()
                .and_then(|entity| entity.properties.as_ref())
                .and_then(|properties| properties.get("declaration_ids"))
                .and_then(read_declaration_ids)
                .unwrap_or_default();
            if refresh_l2_declarations(
                rt,
                token,
                &proj_name,
                LANGUAGE,
                &source_path,
                &snapshot.revision,
                sweep_time,
                &file_label,
                &declaration_ids,
                report,
            )
            .await?
            {
                state.mark_current_module(module_id, &proj_name, LANGUAGE);
                state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
                state
                    .unchanged_declarations
                    .extend(declaration_ids.iter().copied());
                continue;
            }
        }

        clear_l2_ownership(rt, token, module_id, &file_label, report).await?;
        let parse_result = parse_rust_file(&content);
        if let Some(declaration_ids) = persist_l2_file(
            rt,
            token,
            &proj_name,
            LANGUAGE,
            module_id,
            &module_path,
            &source_path,
            &snapshot.revision,
            &hash,
            parse_result.as_ref().map_err(String::as_str),
            sweep_time,
            &file_label,
            &mut state,
            report,
        )
        .await?
        {
            state.mark_current_module(module_id, &proj_name, LANGUAGE);
            state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
        }
    }
    Ok(state)
}

/// Refresh outgoing dependency/implementation edges for declarations whose
/// files were reused without parsing. This runs only after the complete L2
/// walk and re-resolution, so a target is refreshed only when this invocation
/// proved both endpoints current. Changed sources restamp only references
/// observed by their new parse, leaving removed edges at their prior stamp.
async fn refresh_unchanged_l2_edges(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    if state.unchanged_declarations.is_empty() {
        return Ok(());
    }
    let graph = rt.graph(token)?;
    let sources: Vec<Uuid> = state.unchanged_declarations.iter().copied().collect();
    let hits = graph
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::Out,
                relations: Some(vec![EdgeRelation::DependsOn, EdgeRelation::Implements]),
                limit: None,
                min_weight: None,
            },
        )
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let edge_ids: BTreeSet<Uuid> = hits.into_iter().map(|(_, hit)| hit.edge_id).collect();
    let edges = graph
        .get_edges(&edge_ids.into_iter().map(LinkId::from).collect::<Vec<_>>())
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for mut edge in edges {
        let Some(source_owner) = state.current_declarations.get(&edge.source_id).cloned() else {
            continue;
        };
        if !state.unchanged_declarations.contains(&edge.source_id)
            || state.current_declarations.get(&edge.target_id) != Some(&source_owner)
        {
            continue;
        }
        let mut metadata = edge
            .metadata
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if !metadata
            .get("l2_derived")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        metadata.insert("language".into(), json!(source_owner.language));
        metadata.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        edge.updated_at = sweep_time;
        edge.metadata = Some(Value::Object(metadata));
        let edge_id = Uuid::from(edge.id);
        graph
            .upsert_edge(edge)
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
        state.stamped_edge_ids.insert(edge_id);
        report.edges_updated += 1;
    }

    let hits = graph
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::In,
                relations: Some(vec![EdgeRelation::Contains]),
                limit: None,
                min_weight: None,
            },
        )
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let edge_ids: BTreeSet<Uuid> = hits.into_iter().map(|(_, hit)| hit.edge_id).collect();
    let edges = graph
        .get_edges(&edge_ids.into_iter().map(LinkId::from).collect::<Vec<_>>())
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for mut edge in edges {
        let Some(target_owner) = state.current_declarations.get(&edge.target_id).cloned() else {
            continue;
        };
        let source_owner = state
            .current_declarations
            .get(&edge.source_id)
            .or_else(|| state.current_modules.get(&edge.source_id));
        if !state.unchanged_declarations.contains(&edge.target_id)
            || source_owner != Some(&target_owner)
        {
            continue;
        }
        let mut metadata = edge
            .metadata
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if !metadata
            .get("l2_derived")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        metadata.insert("language".into(), json!(target_owner.language));
        metadata.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        edge.updated_at = sweep_time;
        edge.metadata = Some(Value::Object(metadata));
        graph
            .upsert_edge(edge)
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
        report.edges_updated += 1;
    }
    Ok(())
}

/// L2 synchronous re-resolve pass, run once after the whole L2 file walk
/// completes (mirrors L1.5's `reresolve_pass`): revisits every symbol
/// carrying pending call/type references and every module carrying pending
/// impls, and retries resolution against the now-fully-populated set. This
/// is what makes edge convergence independent of file-visit order within one
/// sweep, and lets a later sweep pick up targets that did not exist yet.
async fn l2_reresolve_pass(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let sql = rt.sql();
    let no_current_file_ids = BTreeSet::new();
    if let Some(l2) = report.l2.as_mut() {
        l2.symbol_dependencies_unresolved = 0;
    }

    // Pass 1: pending call/type references on symbol entities.
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE deleted_at IS NULL \
                  AND json_extract(properties,'$.l2_unresolved_references') IS NOT NULL"
                .into(),
            params: vec![],
            label: Some("code_ingest_l2_reresolve_refs".into()),
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for row in rows {
        let Some(id) = row_uuid(&row) else { continue };
        let Some(entity) = get_entity_opt(rt, token, id).await? else {
            continue;
        };
        let Some(source_project) = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("source_project"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(language) = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("language"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(declaring_module_path) = entity
            .properties
            .as_ref()
            .and_then(|p| p.get("module_path"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let source_project = source_project.to_string();
        let language = language.to_string();
        let declaring_module_path = declaring_module_path.to_string();
        if !state.is_current_declaration(id, &source_project, &language, &no_current_file_ids) {
            continue;
        }
        let pending = entity
            .properties
            .as_ref()
            .map(read_l2_unresolved)
            .unwrap_or_default();
        if pending.is_empty() {
            continue;
        }
        let mut still_pending = Vec::new();
        let mut pending_changed = false;
        for reference in pending {
            let mut target = None;
            let mut suppressed_self_type = false;
            for candidate in symbol_candidate_ids(
                &source_project,
                &language,
                &declaring_module_path,
                &reference.segments,
                &reference.evidence,
            ) {
                if candidate == id && reference.evidence == "type_reference" {
                    suppressed_self_type = true;
                    break;
                }
                if state.is_current_declaration(
                    candidate,
                    &source_project,
                    &language,
                    &no_current_file_ids,
                ) {
                    target = Some(candidate);
                    break;
                }
            }
            match target {
                Some(target_id) => {
                    upsert_l2_depends_on(
                        rt,
                        token,
                        id,
                        target_id,
                        &reference.evidence,
                        &language,
                        sweep_time,
                        state,
                        report,
                    )
                    .await?;
                    pending_changed = true;
                }
                None if suppressed_self_type => pending_changed = true,
                None => still_pending.push(reference),
            }
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_dependencies_unresolved += still_pending.len() as u64;
        }
        if pending_changed {
            let label = id.to_string();
            let mut props = entity
                .properties
                .clone()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if still_pending.is_empty() {
                props.remove("l2_unresolved_references");
            } else {
                props.insert(
                    "l2_unresolved_references".into(),
                    serde_json::to_value(&still_pending).expect("serializes"),
                );
            }
            let mut entity = entity;
            entity.properties = Some(Value::Object(props));
            upsert_entity(rt, token, entity, &label, report).await?;
        }
    }

    // Pass 2: pending positive impls on module entities.
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE deleted_at IS NULL \
                  AND json_extract(properties,'$.l2_pending_impls') IS NOT NULL"
                .into(),
            params: vec![],
            label: Some("code_ingest_l2_reresolve_impls".into()),
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for row in rows {
        let Some(module_id) = row_uuid(&row) else {
            continue;
        };
        if !state.current_modules.contains_key(&module_id) {
            continue;
        }
        let Some(module) = get_entity_opt(rt, token, module_id).await? else {
            continue;
        };
        let Some(source_project) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("source_project"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(language) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("language"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(module_path) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("module_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let pending = module
            .properties
            .as_ref()
            .map(read_l2_pending_impls)
            .unwrap_or_default();
        if pending.is_empty() {
            continue;
        }
        let mut still_pending = Vec::new();
        let mut resolved_any = false;
        for entry in pending {
            let type_id = find_first_current(
                state,
                &source_project,
                &language,
                symbol_candidate_ids_for_kinds(
                    &source_project,
                    &language,
                    &module_path,
                    &entry.type_path,
                    &["datatype"],
                ),
                &no_current_file_ids,
            );
            let trait_id = find_first_current(
                state,
                &source_project,
                &language,
                symbol_candidate_ids_for_kinds(
                    &source_project,
                    &language,
                    &module_path,
                    &entry.trait_path,
                    &["interface"],
                ),
                &no_current_file_ids,
            );
            match (type_id, trait_id) {
                (Some(type_id), Some(trait_id)) => {
                    upsert_l2_implements(
                        rt, token, type_id, trait_id, &language, sweep_time, state, report,
                    )
                    .await?;
                    resolved_any = true;
                }
                _ => still_pending.push(entry),
            }
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_dependencies_unresolved += still_pending.len() as u64;
        }
        if resolved_any {
            let label = module_id.to_string();
            let mut props = module
                .properties
                .clone()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if still_pending.is_empty() {
                props.remove("l2_pending_impls");
            } else {
                props.insert(
                    "l2_pending_impls".into(),
                    serde_json::to_value(&still_pending).expect("serializes"),
                );
            }
            let mut module = module;
            module.properties = Some(Value::Object(props));
            upsert_entity(rt, token, module, &label, report).await?;
        }
    }

    Ok(())
}

fn row_uuid(row: &khive_storage::types::SqlRow) -> Option<Uuid> {
    use khive_storage::types::SqlValue;
    match row.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{Namespace, RuntimeConfig};
    use tempfile::TempDir;

    #[test]
    fn invalid_declaration_ids_force_reparse() {
        let hash = "0123456789abcdef";

        assert!(l2_needs_reparse(Some(hash), None, hash));
        assert!(l2_needs_reparse(Some(hash), Some(&Value::Null), hash));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&json!("not-an-array")),
            hash
        ));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&json!(["not-a-uuid"])),
            hash
        ));
        assert!(!l2_needs_reparse(Some(hash), Some(&json!([])), hash));
        assert!(!l2_needs_reparse(
            Some(hash),
            Some(&json!([Uuid::nil().to_string()])),
            hash
        ));
    }

    #[tokio::test]
    async fn stamp_skips_non_object_properties_without_rebuilding() {
        let root = TempDir::new().expect("temporary database directory");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(root.path().join("stamp.db")),
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("target runtime opens");
        let token = rt.authorize(Namespace::local()).expect("token");
        let module_id = module_uuid("fixture", "rust", "crate");
        let mut module = Entity::new(token.namespace().as_str(), "concept", "crate")
            .with_entity_type(Some("module"));
        module.id = module_id;
        module.properties = Some(json!("corrupt"));
        let original_properties = module.properties.clone();
        rt.entities(&token)
            .expect("entity store")
            .upsert_entity(module)
            .await
            .expect("direct entity write");

        let mut module_scans = HashMap::new();
        module_scans.insert(
            module_id,
            ModuleScan {
                source_project: "fixture".to_string(),
                imports: Vec::new(),
            },
        );
        let mut report = CodeSourceIngestReport::default();
        stamp_import_scan_coverage(&rt, &token, module_scans, &mut report)
            .await
            .expect("stamp path completes");

        let stored = rt
            .entities(&token)
            .expect("entity store")
            .get_entity(module_id)
            .await
            .expect("fetch stamped module")
            .expect("module remains present");
        assert_eq!(stored.properties, original_properties);
        assert_eq!(report.coverage_stamps_missed, 1);
        assert!(report.warnings.iter().any(|warning| {
            warning.contains("F2 contract violation") && warning.contains("coverage stamp skipped")
        }));
    }

    #[tokio::test]
    async fn symbol_fts_failure_aborts_without_incrementing_success_counters() {
        let root = TempDir::new().expect("temporary database directory");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(root.path().join("fts-failure.db")),
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("target runtime opens");
        let token = rt.authorize(Namespace::local()).expect("token");
        rt.sql()
            .writer()
            .await
            .expect("writer")
            .execute_script(
                "DROP TABLE fts_entities; \
                 CREATE TABLE fts_entities (broken_column TEXT);"
                    .to_string(),
            )
            .await
            .expect("replace temporary FTS table with an incompatible schema");

        let declaration = ExtractedDeclaration {
            kind: DeclKind::Function,
            name: "symbol".to_string(),
            description: None,
            content_hash: "0123456789abcdef".to_string(),
            calls: Vec::new(),
            module_segments: Vec::new(),
            type_refs: Vec::new(),
        };
        let mut report = CodeSourceIngestReport {
            l2: Some(CodeSourceIngestL2Report::default()),
            ..Default::default()
        };
        let error = upsert_declaration(
            &rt,
            &token,
            "fixture",
            "rust",
            "crate",
            &declaration,
            "src/lib.rs",
            "unversioned",
            Utc::now(),
            "src/lib.rs",
            &mut report,
        )
        .await
        .expect_err("symbol FTS failure must abort");
        assert!(error.to_string().contains("entity FTS indexing"));
        assert_eq!(report.fts_indexed, 0);
        assert_eq!(report.l2.expect("L2 report").symbols_created, 0);
    }
}
