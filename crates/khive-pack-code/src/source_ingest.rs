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
//! (ADR-085 D6 #4) via the guarded entity-mutation seam. A credential refusal quarantines that one
//! item — it is recorded in [`CodeSourceIngestReport::blocked`] and skipped —
//! rather than aborting the rest of the sweep, the same
//! per-record posture `git.digest` already uses for its own write refusals.
//! The runtime-owned top-level secret-gate property is separately reserved:
//! its presence in a candidate, including retained existing properties,
//! refuses that entity mutation with the shared invalid-input error.
//!
//! Identity (B4): every entity this pipeline creates has a `uuid5`-derived
//! id, so re-ingesting the same path needs no dedup lookup. Edge ids are
//! likewise `uuid5`-derived from their endpoints. Re-ingest applies each
//! semantic delta through a conditional-insert/guarded-replacement loop, so
//! two sweeps targeting the same map cannot replace one another's unrelated
//! properties or evidence. B6 cross-repo resolution and B5 staleness
//! stamping are both driven off this determinism: an unresolved specifier
//! records only the information needed to recompute its target's id later,
//! and the synchronous re-resolve pass (`reresolve_pass`) does exactly that.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::hash::Hash;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

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
use crate::safe_source::{self, SourceReadError};

mod file_pending;
use file_pending::{stamp_l2_declarations, FileReference};
#[path = "source_ingest/l2_edge_refresh.rs"]
mod l2_edge_refresh;
use l2_edge_refresh::refresh_unchanged_l2_edges;
mod l2_declaration_refresh;
use l2_declaration_refresh::refresh_l2_declarations;
mod l2_observation;
use l2_observation::{
    completed_l2_observation, observation_matches, upsert_l2_depends_on, upsert_l2_implements,
    valid_l2_sweep_entry, L2Observation, PreviousL2SweepStamps,
};

const RUST_L2_SCANNER_IDENTITY_VERSION: u64 = 2;
const RUST_L2_MAX_SOURCE_BYTES: usize = safe_source::MAX_INGEST_FILE_BYTES as usize;
const RUST_L2_MAX_DELIMITER_DEPTH: usize = 64;
const RUST_L2_MAX_ANGLE_DEPTH: usize = 64;
const RUST_L2_MAX_SEGMENT_TOKENS: usize = 2048;
const RUST_L2_MAX_SEGMENT_OPERATORS: usize = 128;
const RUST_L2_SCANNER_STACK_BYTES: usize = 16 * 1024 * 1024;
const RUST_L2_SCANNER_WORKERS: usize = 2;

enum L2Source {
    Ready { content: String, hash: String },
    Refused { hash: String, reason: String },
}

impl L2Source {
    fn hash(&self) -> &str {
        match self {
            Self::Ready { hash, .. } | Self::Refused { hash, .. } => hash,
        }
    }
}

#[cfg(test)]
mod race_seam {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tokio::sync::Barrier;

    pub(super) struct OneShotPause {
        barrier: Arc<Barrier>,
        used: AtomicBool,
    }

    impl OneShotPause {
        pub(super) fn new(barrier: Arc<Barrier>) -> Self {
            Self {
                barrier,
                used: AtomicBool::new(false),
            }
        }
    }

    tokio::task_local! {
        pub(super) static AFTER_ROW_READ: Arc<OneShotPause>;
    }

    pub(super) async fn pause_after_row_read() {
        let Ok(pause) = AFTER_ROW_READ.try_with(Arc::clone) else {
            return;
        };
        if !pause.used.swap(true, Ordering::AcqRel) {
            pause.barrier.wait().await;
        }
    }
}

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
    /// L1.5 source reads refused because the candidate disappeared, was not
    /// regular, escaped the ingest root, or exceeded the 2 MiB ceiling.
    #[serde(skip_serializing_if = "count_is_zero")]
    pub source_files_refused: u64,
    /// Manifest reads refused for the same file, containment, and size checks.
    #[serde(skip_serializing_if = "count_is_zero")]
    pub manifest_files_refused: u64,
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

fn count_is_zero(count: &u64) -> bool {
    *count == 0
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

fn record_manifest_failures(
    report: &mut CodeSourceIngestReport,
    failures: Vec<manifest::ManifestReadFailure>,
) {
    for failure in failures {
        let refused = matches!(&failure.error, SourceReadError::Refused(_));
        let warning = if refused {
            format!(
                "refused manifest {}: {}",
                failure.path.display(),
                failure.error
            )
        } else {
            format!(
                "reading manifest {}: {}",
                failure.path.display(),
                failure.error
            )
        };
        if !report.warnings.contains(&warning) {
            if refused {
                report.manifest_files_refused += 1;
            }
            report.warnings.push(warning);
        }
    }
}

fn record_source_read_failure(
    report: &mut CodeSourceIngestReport,
    tier: &str,
    path: &Path,
    error: SourceReadError,
) {
    if matches!(&error, SourceReadError::Refused(_)) {
        report.source_files_refused += 1;
        report
            .warnings
            .push(format!("{tier} refused source {}: {error}", path.display()));
    } else {
        report
            .warnings
            .push(format!("reading {}: {error}", path.display()));
    }
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

async fn blocking_io<T, F>(work: F) -> Result<T, CodeSourceIngestError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        CodeSourceIngestError::Storage(format!("code ingest worker failed: {error}"))
    })
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
    let fallback_ingest_root = ingest_root.to_path_buf();
    let ingest_root = ingest_root.to_path_buf();
    let git_result = tokio::task::spawn_blocking(move || {
        let fallback_root = ingest_root
            .canonicalize()
            .unwrap_or_else(|_| ingest_root.clone());
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
        (root, revision, fallback_root)
    })
    .await
    .ok();

    let (git_root, git_revision, fallback_root) =
        git_result.unwrap_or((None, None, fallback_ingest_root));
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
struct UnresolvedSpec {
    specifier: String,
    target_kind: String,
    dependency_kind: String,
    #[serde(default)]
    dependency_scope: String,
    language: String,
}

struct PendingUnresolved {
    spec: UnresolvedSpec,
    file: String,
}

type PendingUnresolvedByOwner = BTreeMap<Uuid, Vec<PendingUnresolved>>;

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
        .range(
            (
                source_project.to_owned(),
                language.to_owned(),
                String::new(),
            )..,
        )
        .inspect(|_| {
            #[cfg(test)]
            l2_batch_tests::observe_manifest_visit();
        })
        .take_while(|((source, declared_language, _), _)| {
            source == source_project && declared_language == language
        })
        .filter(|((_, _, declared_target), _)| {
            #[cfg(test)]
            l2_batch_tests::observe_manifest_normalization();
            declared_target.replace('-', "_") == normalized_target
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
    secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
    secret_gate::check_at(&entity.name, "entity", "name")?;
    if let Some(description) = &entity.description {
        secret_gate::check_at(description, "entity", "description")?;
    }
    if let Some(properties) = &entity.properties {
        secret_gate::check_json_at(properties, "entity", "properties")?;
    }
    Ok(())
}

const MAX_ROW_REBASE_ATTEMPTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowMutationOutcome {
    Created,
    Updated,
    Unchanged,
    Blocked,
}

impl RowMutationOutcome {
    fn wrote(self) -> bool {
        matches!(self, Self::Created | Self::Updated)
    }
}

fn gate_allows_entity(
    entity: &Entity,
    file: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    if let Err(err) = gate_check(entity) {
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
    Ok(true)
}

async fn index_entity(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity: &Entity,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    rt.text(token)?
        .upsert_document(entity_fts_document(entity))
        .await
        .map_err(|e| CodeSourceIngestError::Storage(format!("entity FTS indexing: {e}")))?;
    #[cfg(test)]
    l2_batch_tests::observe_fts_write(entity.id);
    #[cfg(test)]
    l2_recovery_tests::observe_fts_write();
    report.fts_indexed += 1;
    Ok(())
}

fn advancing_entity_revision(requested: i64, current: i64) -> Result<i64, CodeSourceIngestError> {
    let minimum = current.checked_add(1).ok_or_else(|| {
        CodeSourceIngestError::Storage(format!(
            "entity revision {current} cannot advance past i64::MAX"
        ))
    })?;
    Ok(requested.max(minimum))
}

fn advancing_edge_revision(
    requested: DateTime<Utc>,
    current: DateTime<Utc>,
) -> Result<DateTime<Utc>, CodeSourceIngestError> {
    let current_micros = current.timestamp_micros();
    let minimum = current_micros.checked_add(1).ok_or_else(|| {
        CodeSourceIngestError::Storage(format!(
            "edge revision {current_micros} cannot advance past i64::MAX"
        ))
    })?;
    let micros = requested.timestamp_micros().max(minimum);
    DateTime::from_timestamp_micros(micros).ok_or_else(|| {
        CodeSourceIngestError::Storage(format!("edge revision {micros} is out of range"))
    })
}

async fn mutate_entity<F>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    file: &str,
    report: &mut CodeSourceIngestReport,
    mut apply: F,
) -> Result<RowMutationOutcome, CodeSourceIngestError>
where
    F: FnMut(Option<&Entity>) -> Option<Entity>,
{
    let store = rt.entities(token)?;
    for _ in 0..MAX_ROW_REBASE_ATTEMPTS {
        let current = store
            .get_entity_including_deleted(id)
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
        #[cfg(test)]
        l2_batch_tests::observe_row_read(id);
        #[cfg(test)]
        race_seam::pause_after_row_read().await;
        #[cfg(test)]
        l2_recovery_tests::after_entity_read(id).await;
        let Some(mut replacement) = apply(current.as_ref()) else {
            return Ok(RowMutationOutcome::Unchanged);
        };
        if replacement.id != id {
            return Err(CodeSourceIngestError::Storage(format!(
                "entity mutation for {id} produced replacement {}",
                replacement.id
            )));
        }
        replacement.deleted_at = None;
        secret_gate::reject_reserved_secret_gate_property(replacement.properties.as_ref())?;

        let outcome = if let Some(snapshot) = current.as_ref() {
            replacement.created_at = snapshot.created_at;
            replacement.version = snapshot.version;
            replacement.updated_at =
                advancing_entity_revision(replacement.updated_at, snapshot.updated_at)?;
            if !gate_allows_entity(&replacement, file, report)? {
                return Ok(RowMutationOutcome::Blocked);
            }
            store
                .replace_entity_if_unchanged(
                    replacement.clone(),
                    snapshot.updated_at,
                    snapshot.deleted_at,
                )
                .await
                .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
                .then_some(RowMutationOutcome::Updated)
        } else {
            if !gate_allows_entity(&replacement, file, report)? {
                return Ok(RowMutationOutcome::Blocked);
            }
            store
                .insert_entity_if_absent(replacement.clone())
                .await
                .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
                .then_some(RowMutationOutcome::Created)
        };

        if let Some(outcome) = outcome {
            #[cfg(test)]
            l2_batch_tests::observe_row_write(id);
            #[cfg(test)]
            l2_recovery_tests::after_entity_commit(&replacement);
            index_entity(rt, token, &replacement, report).await?;
            return Ok(outcome);
        }
    }
    Err(CodeSourceIngestError::Storage(format!(
        "entity {id} changed during all {MAX_ROW_REBASE_ATTEMPTS} code-map rebase attempts"
    )))
}

async fn mutate_edge<F>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    mut apply: F,
) -> Result<RowMutationOutcome, CodeSourceIngestError>
where
    F: FnMut(Option<&Edge>) -> Option<Edge>,
{
    let store = rt.graph(token)?;
    let link_id = LinkId::from(id);
    for _ in 0..MAX_ROW_REBASE_ATTEMPTS {
        let current = store
            .get_edge_including_deleted(link_id)
            .await
            .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
        #[cfg(test)]
        race_seam::pause_after_row_read().await;
        let Some(mut replacement) = apply(current.as_ref()) else {
            return Ok(RowMutationOutcome::Unchanged);
        };
        if Uuid::from(replacement.id) != id {
            return Err(CodeSourceIngestError::Storage(format!(
                "edge mutation for {id} produced replacement {}",
                Uuid::from(replacement.id)
            )));
        }
        replacement.deleted_at = None;

        let outcome = if let Some(snapshot) = current.as_ref() {
            replacement.created_at = snapshot.created_at;
            replacement.updated_at =
                advancing_edge_revision(replacement.updated_at, snapshot.updated_at)?;
            store
                .replace_edge_if_unchanged(replacement, snapshot.updated_at, snapshot.deleted_at)
                .await
                .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
                .then_some(RowMutationOutcome::Updated)
        } else {
            store
                .insert_edge_if_absent(replacement)
                .await
                .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?
                .then_some(RowMutationOutcome::Created)
        };
        if let Some(outcome) = outcome {
            #[cfg(test)]
            l2_recovery_tests::after_edge_commit(id).await;
            return Ok(outcome);
        }
    }
    Err(CodeSourceIngestError::Storage(format!(
        "edge {id} changed during all {MAX_ROW_REBASE_ATTEMPTS} code-map rebase attempts"
    )))
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
    let outcome = mutate_edge(rt, token, id, |current| {
        let mut edge = current.cloned().unwrap_or(Edge {
            id: LinkId::from(id),
            namespace: token.namespace().as_str().to_string(),
            source_id,
            target_id,
            relation,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        });
        edge.namespace = token.namespace().as_str().to_string();
        edge.source_id = source_id;
        edge.target_id = target_id;
        edge.relation = relation;
        edge.weight = 1.0;
        edge.updated_at = now;
        edge.metadata = Some(metadata.clone());
        Some(edge)
    })
    .await?;
    Ok(outcome == RowMutationOutcome::Created)
}

fn ts(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_micros()
}

/// Capture completed predecessor authority before any selected tier advances
/// the visible project clock (ADR-085 Amendments 14 and 15).
async fn capture_previous_l2_sweep_stamp(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    language: &str,
    previous_stamps: &mut PreviousL2SweepStamps,
) -> Result<(), CodeSourceIngestError> {
    let owner = L2OwnerKey {
        source_project: name.to_string(),
        language: language.to_string(),
    };
    if previous_stamps.stamps.contains_key(&owner) {
        return Ok(());
    }
    let stamp = get_entity_opt(rt, token, project_uuid(name))
        .await?
        .and_then(|project| {
            project
                .properties
                .as_ref()
                .and_then(|properties| completed_l2_observation(properties, language))
        });
    previous_stamps.stamps.insert(owner, stamp);
    Ok(())
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
    capture_previous_l2_sweep: bool,
    previous_l2_sweep_stamps: &mut PreviousL2SweepStamps,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Uuid>, CodeSourceIngestError> {
    let id = project_uuid(name);
    if capture_previous_l2_sweep {
        capture_previous_l2_sweep_stamp(rt, token, name, language, previous_l2_sweep_stamps)
            .await?;
    }
    let now = ts(sweep_time);
    let outcome = mutate_entity(rt, token, id, source_label, report, |current| {
        let mut entity = current
            .cloned()
            .unwrap_or_else(|| Entity::new(token.namespace().as_str(), "project", name));
        let mut props = entity
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let mut sweep_clock = props
            .get("sweep_clock")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        sweep_clock.insert(language.to_string(), json!(sweep_time.to_rfc3339()));
        props.insert("source_project".into(), json!(name));
        props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        props.insert("sweep_clock".into(), Value::Object(sweep_clock));
        if capture_previous_l2_sweep {
            let mut runs = props
                .get("l2_sweep_runs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let completed = runs
                .get(language)
                .filter(|entry| valid_l2_sweep_entry(entry))
                .and_then(|entry| entry.get("completed"))
                .cloned()
                .unwrap_or(Value::Null);
            runs.insert(
                language.to_string(),
                json!({
                    "version": 1,
                    "attempted": {
                        "run_id": previous_l2_sweep_stamps.run_id.to_string(),
                        "sweep_time": sweep_time.to_rfc3339(),
                    },
                    "completed": completed,
                }),
            );
            props.insert("l2_sweep_runs".into(), Value::Object(runs));
        }
        entity.id = id;
        entity.namespace = token.namespace().as_str().to_string();
        entity.kind = "project".to_string();
        entity.name = name.to_string();
        entity.properties = Some(Value::Object(props));
        entity.updated_at = now;
        Some(entity)
    })
    .await?;

    match outcome {
        RowMutationOutcome::Blocked => return Ok(None),
        RowMutationOutcome::Created => report.projects_created += 1,
        RowMutationOutcome::Updated => report.projects_updated += 1,
        RowMutationOutcome::Unchanged => {}
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
    previous_l2_sweep_stamps: &mut PreviousL2SweepStamps,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Uuid>, CodeSourceIngestError> {
    let key = project_cache_key(proj_name, language, per_language_project_stamps);
    if let Some(id) = project_ids.get(&key) {
        return Ok(Some(*id));
    }
    let Some(id) = upsert_project(
        rt,
        token,
        proj_name,
        file_label,
        language,
        sweep_time,
        per_language_project_stamps && language == "rust",
        previous_l2_sweep_stamps,
        report,
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
    let now = ts(sweep_time);
    let outcome = mutate_entity(rt, token, id, file, report, |current| {
        let mut entity = current.cloned().unwrap_or_else(|| {
            Entity::new(token.namespace().as_str(), "concept", module_path)
                .with_entity_type(Some("module"))
        });
        let mut props = entity
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        props.insert("source_project".into(), json!(source_project));
        props.insert("language".into(), json!(language));
        props.insert("module_path".into(), json!(module_path));
        props.insert("source_path".into(), json!(source_path));
        props.insert("source_revision".into(), json!(source_revision));
        props.insert("content_hash".into(), json!(content_hash));
        props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        props
            .entry("import_scan_status".to_string())
            .or_insert_with(|| json!("unscanned"));
        if !preserve_l2_state {
            for key in [
                "declaration_ids",
                "l2_pending_impls",
                "l2_content_hash",
                "l2_scanner_identity_version",
            ] {
                props.remove(key);
            }
        }
        entity.id = id;
        entity.namespace = token.namespace().as_str().to_string();
        entity.kind = "concept".to_string();
        entity.entity_type = Some("module".to_string());
        entity.name = module_path.to_string();
        entity.properties = Some(Value::Object(props));
        entity.updated_at = now;
        Some(entity)
    })
    .await?;

    match outcome {
        RowMutationOutcome::Blocked => return Ok(None),
        RowMutationOutcome::Created => report.modules_created += 1,
        RowMutationOutcome::Updated => report.modules_updated += 1,
        RowMutationOutcome::Unchanged => {}
    }
    Ok(Some(id))
}

/// Append one sweep's unresolved specs to an owner in encounter order with a
/// single guarded entity/FTS write. Project/module upserts have already run,
/// and the fresh-read rebase preserves their other properties.
///
/// Screen each candidate separately before batching, so a secret-shaped
/// specifier is quarantined under its own source file without discarding safe
/// siblings. The full replacement still passes `mutate_entity`'s secret gate.
async fn record_unresolved_batch(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
    pending: &[PendingUnresolved],
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    // A pre-existing spec (or an earlier safe candidate in this batch) was
    // already a no-op in the per-spec path, before its gate check. Preserve
    // that behavior and avoid screening duplicates repeatedly. The guarded
    // mutation below reads again and rebases if another sweep wrote meanwhile.
    let current = rt
        .entities(token)?
        .get_entity_including_deleted(entity_id)
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let Some(current) = current else {
        return Ok(());
    };
    let mut staged_seen: HashSet<_> = current
        .properties
        .as_ref()
        .map(read_unresolved)
        .unwrap_or_default()
        .into_iter()
        .collect();
    if pending.iter().all(|item| staged_seen.contains(&item.spec)) {
        return Ok(());
    }
    // If the owner already carries a gate-refused value, every new per-spec
    // mutation used to be refused before any of them could write. Keep that
    // per-item report behavior without rebuilding and rechecking the growing
    // list K times.
    if let Err(error) = gate_check(&current) {
        match error {
            RuntimeError::SecretDetected(secret) => {
                for item in pending {
                    if !staged_seen.contains(&item.spec) {
                        report.blocked_count += 1;
                        report.blocked.push(BlockedWrite {
                            file: item.file.clone(),
                            detector: secret.detector.to_string(),
                            masked_excerpt: secret.masked.clone(),
                        });
                    }
                }
                return Ok(());
            }
            other => return Err(other.into()),
        }
    }
    let mut allowed = Vec::with_capacity(pending.len());
    for item in pending {
        if staged_seen.contains(&item.spec) {
            continue;
        }
        let candidate = serde_json::to_value(&item.spec).expect("serializes");
        match secret_gate::check_json_at(&candidate, "entity", "properties") {
            Ok(()) => {
                staged_seen.insert(item.spec.clone());
                allowed.push(item);
            }
            Err(RuntimeError::SecretDetected(secret)) => {
                report.blocked_count += 1;
                report.blocked.push(BlockedWrite {
                    file: item.file.clone(),
                    detector: secret.detector.to_string(),
                    masked_excerpt: secret.masked,
                });
            }
            Err(other) => return Err(other.into()),
        }
    }
    let Some(first) = allowed.first() else {
        return Ok(());
    };
    let mut appended = 0usize;
    let outcome = mutate_entity(rt, token, entity_id, &first.file, report, |current| {
        let mut entity = current?.clone();
        let mut list = entity
            .properties
            .as_ref()
            .map(read_unresolved)
            .unwrap_or_default();
        let mut seen: HashSet<UnresolvedSpec> = list.iter().cloned().collect();
        appended = 0;
        for item in &allowed {
            if seen.insert(item.spec.clone()) {
                list.push(item.spec.clone());
                appended += 1;
            }
        }
        if appended == 0 {
            return None;
        }
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
        Some(entity)
    })
    .await?;
    if outcome.wrote() {
        report.unresolved_recorded += appended as u64;
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
    let outcome = mutate_edge(rt, token, edge_id, |current| {
        let metadata = merge_dependency_metadata(
            current.and_then(|edge| edge.metadata.as_ref()),
            dependency_kind,
            dependency_scope,
            language,
        );
        Some(Edge {
            id: LinkId::from(edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id,
            target_id,
            relation: EdgeRelation::DependsOn,
            weight: 1.0,
            created_at: current.map(|edge| edge.created_at).unwrap_or(now),
            updated_at: now,
            deleted_at: None,
            metadata: Some(metadata),
            target_backend: current.and_then(|edge| edge.target_backend.clone()),
        })
    })
    .await?;
    match outcome {
        RowMutationOutcome::Created => report.edges_created += 1,
        RowMutationOutcome::Updated => report.edges_updated += 1,
        RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
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
            sql: "SELECT id FROM entities WHERE namespace=?1 \
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
        let Some(entity) = get_entity_opt(rt, token, id).await? else {
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
        let original_list = list.clone();
        let mut still_unresolved = Vec::new();
        let mut still_seen = HashSet::new();
        let mut changed = false;
        for mut spec in list.drain(..) {
            let selected = if spec.dependency_kind == IMPORT_DEPENDENCY_KIND {
                tiers.l1_5
            } else {
                tiers.l1
            };
            if !selected {
                still_seen.insert(spec.clone());
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
                    if !still_seen.insert(spec.clone()) {
                        changed = true;
                    } else {
                        still_unresolved.push(spec);
                    }
                }
            }
        }
        if changed {
            let entity_label = id.to_string();
            let original_set: HashSet<_> = original_list.iter().cloned().collect();
            mutate_entity(rt, token, id, &entity_label, report, |current| {
                let mut entity = current?.clone();
                let mut rebased = entity
                    .properties
                    .as_ref()
                    .map(read_unresolved)
                    .unwrap_or_default();
                rebased.retain(|specifier| !original_set.contains(specifier));
                let mut seen: HashSet<_> = rebased.iter().cloned().collect();
                for specifier in &still_unresolved {
                    if seen.insert(specifier.clone()) {
                        rebased.push(specifier.clone());
                    }
                }
                let mut props = entity
                    .properties
                    .clone()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if rebased.is_empty() {
                    props.remove("unresolved_specifiers");
                } else {
                    props.insert(
                        "unresolved_specifiers".into(),
                        serde_json::to_value(&rebased).expect("serializes"),
                    );
                }
                entity.properties = Some(Value::Object(props));
                Some(entity)
            })
            .await?;
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

        let source_label = get_entity_opt(rt, token, module_id)
            .await?
            .as_ref()
            .and_then(|module| module.properties.as_ref())
            .and_then(|properties| properties.get("source_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module_id.to_string());
        let mut row_missing = false;
        let mut invalid_properties = false;
        let outcome = mutate_entity(rt, token, module_id, &source_label, report, |current| {
            row_missing = current.is_none();
            let mut module = current?.clone();
            let Some(Value::Object(mut props)) = module.properties.clone() else {
                invalid_properties = true;
                return None;
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
            Some(module)
        })
        .await?;
        if row_missing {
            report.warnings.push(format!(
                "module {module_id} from this sweep's scan map was missing at stamp time; \
                 coverage stamps skipped (F2 contract violation)"
            ));
            report.coverage_stamps_missed += 1;
        } else if invalid_properties {
            report.warnings.push(format!(
                "module {module_id} has missing or non-object properties at stamp time; \
                 coverage stamp skipped (F2 contract violation)"
            ));
            report.coverage_stamps_missed += 1;
        } else if outcome == RowMutationOutcome::Blocked {
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

/// Both source tiers resolve symlinks without crossing the canonical ingest
/// root. Canonical directory de-duplication also prevents symlink cycles.
fn collect_source_files(
    root: &Path,
    ext: &str,
    out: &mut Vec<PathBuf>,
    skipped_outside_root: &mut Vec<PathBuf>,
    skipped_non_regular: &mut Vec<PathBuf>,
    skipped_non_source: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    struct SourceWalk<'a> {
        canonical_root: &'a Path,
        ext: &'a str,
        visited_dirs: BTreeSet<PathBuf>,
        out: &'a mut Vec<PathBuf>,
        skipped_outside_root: &'a mut Vec<PathBuf>,
        skipped_non_regular: &'a mut Vec<PathBuf>,
        skipped_non_source: &'a mut Vec<PathBuf>,
    }

    fn visit(path: &Path, walk: &mut SourceWalk<'_>) -> std::io::Result<()> {
        let canonical = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(_) => {
                if path.extension().and_then(|value| value.to_str()) == Some(walk.ext) {
                    walk.skipped_outside_root.push(path.to_path_buf());
                } else {
                    walk.skipped_non_source.push(path.to_path_buf());
                }
                return Ok(());
            }
        };
        if !canonical.starts_with(walk.canonical_root) {
            if path.extension().and_then(|value| value.to_str()) == Some(walk.ext) {
                walk.skipped_outside_root.push(path.to_path_buf());
            } else {
                walk.skipped_non_source.push(path.to_path_buf());
            }
            return Ok(());
        }
        // An alias must not re-enter a directory excluded by its canonical
        // location. Components are relative to the explicitly chosen root,
        // so a caller may still choose an excluded-name directory as root.
        if canonical
            .strip_prefix(walk.canonical_root)
            .expect("canonical path was checked within the ingest root")
            .components()
            .any(|component| match component {
                std::path::Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    SOURCE_SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.')
                }
                _ => false,
            })
        {
            return Ok(());
        }
        if canonical.is_dir() {
            if !walk.visited_dirs.insert(canonical.clone()) {
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
                visit(&entry_path, walk)?;
            }
        } else if canonical.extension().and_then(|value| value.to_str()) == Some(walk.ext) {
            if canonical.is_file() {
                walk.out.push(canonical);
            } else {
                walk.skipped_non_regular.push(path.to_path_buf());
            }
        }
        Ok(())
    }

    let canonical_root = fs::canonicalize(root)?;
    let mut walk = SourceWalk {
        canonical_root: &canonical_root,
        ext,
        visited_dirs: BTreeSet::new(),
        out,
        skipped_outside_root,
        skipped_non_regular,
        skipped_non_source,
    };
    visit(&canonical_root, &mut walk)?;
    walk.out.sort();
    walk.out.dedup();
    Ok(())
}

struct SourceWalkResult {
    canonical_root: PathBuf,
    files: Vec<PathBuf>,
    skipped_outside_root: Vec<PathBuf>,
    skipped_non_regular: Vec<PathBuf>,
    skipped_non_source: Vec<PathBuf>,
}

async fn walk_source_files_on_worker(
    ingest_root: &Path,
    ext: &'static str,
) -> Result<io::Result<SourceWalkResult>, CodeSourceIngestError> {
    let root = ingest_root.to_path_buf();
    blocking_io(move || {
        let canonical_root = fs::canonicalize(root)?;
        let mut walk = SourceWalkResult {
            canonical_root,
            files: Vec::new(),
            skipped_outside_root: Vec::new(),
            skipped_non_regular: Vec::new(),
            skipped_non_source: Vec::new(),
        };
        collect_source_files(
            &walk.canonical_root,
            ext,
            &mut walk.files,
            &mut walk.skipped_outside_root,
            &mut walk.skipped_non_regular,
            &mut walk.skipped_non_source,
        )?;
        Ok(walk)
    })
    .await
}

async fn derive_source_path_on_worker(
    file: &Path,
    ingest_root: &Path,
    snapshot_root: &Path,
) -> Result<(Option<String>, Vec<String>, u64), CodeSourceIngestError> {
    let file = file.to_path_buf();
    let ingest_root = ingest_root.to_path_buf();
    let snapshot_root = snapshot_root.to_path_buf();
    blocking_io(move || {
        let mut path_report = CodeSourceIngestReport::default();
        let path = derive_source_path(&file, &ingest_root, &snapshot_root, &mut path_report);
        (
            path,
            path_report.warnings,
            path_report.files_dropped_without_source_path,
        )
    })
    .await
}

fn content_hash(content: &str) -> String {
    // FNV-1a: fast, dependency-free, sufficient for change-detection (not a
    // security boundary).
    let hash = khive_types::fnv1a_64(content.as_bytes());
    format!("{hash:016x}")
}

fn scan_import_source(
    canonical_root: &Path,
    path: &Path,
    language: &str,
) -> Result<(String, Vec<String>), SourceReadError> {
    let content = safe_source::read_contained_to_string(canonical_root, path)?;
    Ok((
        content_hash(&content),
        imports::extract_raw_imports(language, &content),
    ))
}

/// Read at most the L2 scanner's byte limit plus one. A refused file keeps
/// module metadata and a parse-failure row, but its `refused:` fingerprint is
/// deliberately not represented as a hash of unread source bytes.
fn read_l2_source(canonical_root: &Path, path: &Path) -> Result<L2Source, SourceReadError> {
    let source = safe_source::open_contained_file(canonical_root, path)?;
    let metadata = source.metadata()?;
    if metadata.len() > RUST_L2_MAX_SOURCE_BYTES as u64 {
        return Ok(L2Source::Refused {
            hash: format!("refused:size:{}", metadata.len()),
            reason: "scanner safety limit: Rust source is too large".to_string(),
        });
    }
    let mut reader = BufReader::new(source);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut hash: u64 = 0xcbf29ce484222325;
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let retained = count.min(RUST_L2_MAX_SOURCE_BYTES + 1 - bytes.len());
        for byte in &chunk[..retained] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        bytes.extend_from_slice(&chunk[..retained]);
        if bytes.len() > RUST_L2_MAX_SOURCE_BYTES {
            return Ok(L2Source::Refused {
                hash: format!("refused:{hash:016x}"),
                reason: "scanner safety limit: Rust source is too large".to_string(),
            });
        }
    }
    let hash = format!("{hash:016x}");
    let content = String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if let Err(reason) = check_rust_l2_nesting(&content) {
        return Ok(L2Source::Refused { hash, reason });
    }
    Ok(L2Source::Ready { content, hash })
}

/// Count delimiter nesting without constructing a recursive syntax tree. The
/// scanner consumes only valid UTF-8, and this guard runs before `syn` sees it.
fn check_rust_l2_nesting(content: &str) -> Result<(), String> {
    let bytes = content.as_bytes();
    let mut delimiters = Vec::new();
    let mut angle_at_delimiter = Vec::new();
    let mut angle_depth = 0usize;
    let mut segment_tokens = 0usize;
    let mut segment_operators = 0usize;
    let mut in_word = false;
    let mut top_level_value_item = false;
    let mut braced_item = false;
    let mut item_body_opened = false;
    let mut i = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    if bytes[i..].starts_with(b"#!") && !bytes[i..].starts_with(b"#![") {
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
    }
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            in_word = false;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            in_word = false;
            i += 2;
            let mut comment_depth = 1usize;
            while i < bytes.len() && comment_depth != 0 {
                if bytes[i..].starts_with(b"/*") {
                    comment_depth += 1;
                    if comment_depth > RUST_L2_MAX_DELIMITER_DEPTH {
                        return Err("scanner safety limit: comment nesting is too deep".into());
                    }
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    comment_depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if comment_depth != 0 {
                return Err("scanner safety limit: unterminated block comment".into());
            }
            continue;
        }
        if bytes[i] == b'r' {
            let mut marker = i + 1;
            while marker < bytes.len() && bytes[marker] == b'#' {
                marker += 1;
                if marker - i - 1 > RUST_L2_MAX_DELIMITER_DEPTH {
                    return Err("scanner safety limit: raw string delimiter is too long".into());
                }
            }
            if marker < bytes.len() && bytes[marker] == b'"' {
                scanner_budget_token(&mut segment_tokens)?;
                in_word = false;
                let hashes = marker - i - 1;
                i = marker + 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'"'
                        && bytes.get(i + 1..i + 1 + hashes) == Some(&bytes[marker - hashes..marker])
                    {
                        i += 1 + hashes;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err("scanner safety limit: unterminated raw string".into());
                }
                continue;
            }
        }
        if bytes[i] == b'"' {
            scanner_budget_token(&mut segment_tokens)?;
            in_word = false;
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i = (i + 2).min(bytes.len()),
                    b'"' => {
                        i += 1;
                        closed = true;
                        break;
                    }
                    _ => i += 1,
                }
            }
            if !closed {
                return Err("scanner safety limit: unterminated string".into());
            }
            continue;
        }
        if bytes[i] == b'\'' {
            if let Some(end) = rust_char_literal_end(content, i) {
                scanner_budget_token(&mut segment_tokens)?;
                in_word = false;
                i = end;
                continue;
            }
        }
        let byte = bytes[i];
        let word = byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80;
        if word {
            if !in_word {
                scanner_budget_token(&mut segment_tokens)?;
                if delimiters.is_empty() && angle_depth == 0 && !top_level_value_item {
                    let mut end = i + 1;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric()
                            || bytes[end] == b'_'
                            || bytes[end] >= 0x80)
                    {
                        end += 1;
                    }
                    if matches!(
                        std::str::from_utf8(&bytes[i..end]).ok(),
                        Some(
                            "fn" | "impl"
                                | "struct"
                                | "enum"
                                | "union"
                                | "trait"
                                | "mod"
                                | "extern"
                                | "macro_rules"
                                | "macro"
                        )
                    ) {
                        braced_item = true;
                    }
                }
            }
        } else if !byte.is_ascii_whitespace() {
            scanner_budget_token(&mut segment_tokens)?;
        }
        in_word = word;
        if byte == b'<' {
            angle_depth += 1;
            if angle_depth > RUST_L2_MAX_ANGLE_DEPTH {
                return Err("scanner safety limit: Rust generic nesting is too deep".into());
            }
        } else if byte == b'>' && i.checked_sub(1).and_then(|prev| bytes.get(prev)) != Some(&b'-') {
            let floor = angle_at_delimiter.last().copied().unwrap_or(0);
            angle_depth = angle_depth.saturating_sub(1).max(floor);
        }
        if matches!(byte, b'&' | b'*' | b'!' | b'+' | b'-' | b'=' | b'.' | b'?') {
            segment_operators += 1;
            if segment_operators > RUST_L2_MAX_SEGMENT_OPERATORS {
                return Err("scanner safety limit: Rust expression is too complex".into());
            }
        }
        if byte == b'=' && delimiters.is_empty() && angle_depth == 0 {
            top_level_value_item = true;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                if byte == b'{'
                    && delimiters.is_empty()
                    && angle_depth == 0
                    && braced_item
                    && !top_level_value_item
                {
                    item_body_opened = true;
                }
                delimiters.push(bytes[i]);
                angle_at_delimiter.push(angle_depth);
                if delimiters.len() > RUST_L2_MAX_DELIMITER_DEPTH {
                    return Err("scanner safety limit: Rust syntax nesting is too deep".into());
                }
            }
            b')' | b']' | b'}' => {
                let expected = match bytes[i] {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if delimiters.pop() != Some(expected) {
                    return Err("scanner safety limit: unbalanced Rust delimiters".into());
                }
                angle_depth = angle_at_delimiter
                    .pop()
                    .expect("paired delimiter angle depth");
                if byte == b'}' && delimiters.is_empty() && item_body_opened {
                    segment_tokens = 0;
                    segment_operators = 0;
                    angle_depth = 0;
                    top_level_value_item = false;
                    braced_item = false;
                    item_body_opened = false;
                }
            }
            b';' | b',' => {
                segment_tokens = 0;
                segment_operators = 0;
                if byte == b';' {
                    angle_depth = angle_at_delimiter.last().copied().unwrap_or(0);
                    if delimiters.is_empty() {
                        top_level_value_item = false;
                        braced_item = false;
                        item_body_opened = false;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !delimiters.is_empty() {
        return Err("scanner safety limit: unbalanced Rust delimiters".into());
    }
    Ok(())
}

fn scanner_budget_token(segment: &mut usize) -> Result<(), String> {
    *segment += 1;
    if *segment > RUST_L2_MAX_SEGMENT_TOKENS {
        return Err("scanner safety limit: Rust syntax is too complex".into());
    }
    Ok(())
}

/// Distinguish one-character literals from lifetimes without letting a
/// lifetime hide delimiters later on its line.
fn rust_char_literal_end(content: &str, start: usize) -> Option<usize> {
    let tail = content.get(start + 1..)?;
    let first = tail.chars().next()?;
    if first == '\\' {
        let escaped = tail.chars().nth(1)?;
        let end = if escaped == 'u' {
            let open = start + 3;
            if content.as_bytes().get(open) != Some(&b'{') {
                return None;
            }
            let limit = (open + 10).min(content.len());
            let close = content.get(open + 1..limit)?.find('}')? + open + 1;
            if close - open > 8 {
                return None;
            }
            close + 1
        } else if escaped == 'x' {
            start + 5
        } else {
            start + 3
        };
        return (content.as_bytes().get(end) == Some(&b'\'')).then_some(end + 1);
    }
    let end = start + 1 + first.len_utf8();
    (content.as_bytes().get(end) == Some(&b'\'')).then_some(end + 1)
}

/// Run one selected-tier ingest pass over `opts.path` into the runtime `rt`
/// (already bound to the caller-selected target database — B7 target
/// selection happens in the verb handler, not here).
pub async fn run_code_ingest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    opts: CodeSourceIngestOptions<'_>,
) -> Result<CodeSourceIngestReport, CodeSourceIngestError> {
    let path_for_check = opts.path.to_path_buf();
    if !blocking_io(move || path_for_check.is_dir()).await? {
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
    let mut previous_l2_sweep_stamps = PreviousL2SweepStamps::new();
    let mut pending_unresolved = PendingUnresolvedByOwner::new();

    // Manifest discovery supplies bounded identity, alias, and scope context
    // to L1.5 without implying L1 output. No selected L1/L1.5 tier means no
    // manifest walk, preserving the zero-write and L2-only boundaries.
    let manifests = if opts.enable_l1 || opts.enable_l1_5 {
        let root = opts.path.to_path_buf();
        let languages = opts.languages.clone();
        let discovery = blocking_io(move || {
            let canonical_root = fs::canonicalize(root)?;
            manifest::discover_manifests(&canonical_root, &languages)
        })
        .await?;
        let (manifests, failures) = discovery.map_err(|error| {
            CodeSourceIngestError::InvalidPath(opts.path.join(error.to_string()))
        })?;
        record_manifest_failures(&mut report, failures);
        manifests
    } else {
        Vec::new()
    };
    let manifest_index = manifest::ManifestIndex::new(&manifests);
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
                opts.enable_l2 && m.language == "rust",
                &mut previous_l2_sweep_stamps,
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
                // identical spec (deduped by the per-owner batch) targeting
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
                pending_unresolved
                    .entry(source_id)
                    .or_default()
                    .push(PendingUnresolved {
                        spec,
                        file: file_label.clone(),
                    });
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
                &manifest_index,
                opts.enable_l2,
                opts.sweep_time,
                &mut project_ids,
                &mut previous_l2_sweep_stamps,
                &mut module_scans,
                &mut pending_unresolved,
                &mut report,
            )
            .await?;
        }
    }

    // Flush after all project/module refreshes, before synchronous B6
    // re-resolution observes the unresolved queue. Each owner gets one
    // guarded write regardless of how many files/specifiers contributed.
    for (entity_id, pending) in pending_unresolved {
        record_unresolved_batch(rt, token, entity_id, &pending, &mut report).await?;
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
            L2SweepInputs {
                ingest_root: opts.path,
                snapshot: &snapshot,
                sweep_time: opts.sweep_time,
            },
            &mut project_ids,
            &mut previous_l2_sweep_stamps,
            &mut report,
        )
        .await?;
        l2_reresolve_pass(rt, token, opts.sweep_time, &mut state, &mut report).await?;
        refresh_unchanged_l2_edges(
            rt,
            token,
            opts.sweep_time,
            &previous_l2_sweep_stamps,
            &mut state,
            &mut report,
        )
        .await?;
        #[cfg(test)]
        l2_recovery_tests::before_completion().await;
        complete_l2_sweeps(
            rt,
            token,
            opts.sweep_time,
            &previous_l2_sweep_stamps,
            &state,
            &mut report,
        )
        .await?;
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_edges_stamped = state.stamped_edge_ids.len() as u64;
        }
    }

    Ok(report)
}

/// The `source_project` for a file with no governing manifest anywhere above
/// it: the basename of the ingested folder (ADR-085 Amendment 2 B4).
fn basename_project_name(ingest_root: &Path, canonical_ingest_root: &Path) -> String {
    ingest_root
        .file_name()
        .or_else(|| canonical_ingest_root.file_name())
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
    manifest_index: &manifest::ManifestIndex,
    per_language_project_stamps: bool,
    sweep_time: DateTime<Utc>,
    project_ids: &mut HashMap<(String, String), Uuid>,
    previous_l2_sweep_stamps: &mut PreviousL2SweepStamps,
    module_scans: &mut HashMap<Uuid, ModuleScan>,
    pending_unresolved: &mut PendingUnresolvedByOwner,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let Some(ext) = imports::extension_for_language(language) else {
        return Ok(());
    };
    let walk = match walk_source_files_on_worker(ingest_root, ext).await? {
        Ok(walk) => walk,
        Err(error) => {
            report
                .warnings
                .push(format!("walking {}: {error}", ingest_root.display()));
            return Ok(());
        }
    };
    let SourceWalkResult {
        canonical_root: canonical_ingest_root,
        files,
        skipped_outside_root,
        skipped_non_regular,
        skipped_non_source,
    } = walk;
    for skipped in skipped_outside_root {
        report.warnings.push(format!(
            "L1.5 skipped source outside the canonical ingest root: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_regular {
        report.warnings.push(format!(
            "L1.5 skipped non-regular source: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_source {
        let warning = format!(
            "L1.5 skipped non-source traversal entry: {}",
            skipped.display()
        );
        if !report.warnings.contains(&warning) {
            report.warnings.push(warning);
        }
    }
    if !files.is_empty() {
        record_observed_language(report, language);
    }

    for file in files {
        let Some(file_dir) = file.parent() else {
            continue;
        };
        let governing = manifest_index.governing(file_dir, &canonical_ingest_root, language);
        let (proj_root, proj_name) = governing.unwrap_or_else(|| {
            (
                canonical_ingest_root.clone(),
                basename_project_name(ingest_root, &canonical_ingest_root),
            )
        });
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, language) else {
            report.files_skipped_without_module_path += 1;
            continue;
        };
        let (source_path, warnings, dropped) =
            derive_source_path_on_worker(&file, &canonical_ingest_root, &snapshot.root).await?;
        report.warnings.extend(warnings);
        report.files_dropped_without_source_path += dropped;
        let Some(source_path) = source_path else {
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
            previous_l2_sweep_stamps,
            report,
        )
        .await?
        else {
            // Gate-refused write, already recorded in report.blocked — move
            // on to the next file.
            continue;
        };

        let file_for_read = file.clone();
        let root_for_read = canonical_ingest_root.clone();
        let (hash, raw_imports) =
            match blocking_io(move || scan_import_source(&root_for_read, &file_for_read, language))
                .await?
            {
                Ok(scan) => scan,
                Err(error) => {
                    record_source_read_failure(report, "L1.5", &file, error);
                    continue;
                }
            };
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
        for raw in raw_imports {
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
                    pending_unresolved
                        .entry(module_id)
                        .or_default()
                        .push(PendingUnresolved {
                            spec,
                            file: file_label.clone(),
                        });
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
                    pending_unresolved
                        .entry(proj_id)
                        .or_default()
                        .push(PendingUnresolved {
                            spec,
                            file: file_label.clone(),
                        });
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

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct L2OwnerKey {
    source_project: String,
    language: String,
}

#[derive(Debug)]
struct L2OwnerCoverage {
    whole: bool,
    fallback: bool,
    file_label: String,
}

#[derive(Debug)]
struct L2SweepState {
    run_id: Uuid,
    owners: HashMap<L2OwnerKey, L2OwnerCoverage>,
    /// Declarations proven current by this L2 invocation, never ambient
    /// ownership left by a prior sweep or an earlier tier in this call.
    current_declarations: HashMap<Uuid, L2OwnerKey>,
    /// File modules whose L2 ownership was successfully refreshed/stamped.
    current_modules: HashMap<Uuid, L2OwnerKey>,
    current_files: BTreeMap<(Uuid, String), String>,
    /// Current declarations reused without parsing. Their outgoing edges are
    /// refreshed only after every target file's ownership is finalized.
    unchanged_declarations: BTreeSet<Uuid>,
    /// Natural L2 dependency/implementation edges successfully stamped this
    /// sweep; this set is the authority for `symbol_edges_stamped`.
    stamped_edge_ids: BTreeSet<Uuid>,
    /// Successful natural writer observations, including repeated shared IDs.
    observed_natural_edge_ids: Vec<Uuid>,
    /// Accepted natural relations belonging to reused physical producers.
    current_natural_edge_ids: BTreeSet<Uuid>,
}

impl L2SweepState {
    fn new(run_id: Uuid) -> Self {
        Self {
            run_id,
            owners: HashMap::new(),
            current_declarations: HashMap::new(),
            current_modules: HashMap::new(),
            current_files: BTreeMap::new(),
            unchanged_declarations: BTreeSet::new(),
            stamped_edge_ids: BTreeSet::new(),
            observed_natural_edge_ids: Vec::new(),
            current_natural_edge_ids: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
impl Default for L2SweepState {
    fn default() -> Self {
        Self::new(Uuid::new_v4())
    }
}

async fn complete_l2_sweeps(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    previous_stamps: &PreviousL2SweepStamps,
    state: &L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let run_id = previous_stamps.run_id.to_string();
    for (owner, coverage) in &state.owners {
        if !coverage.whole || coverage.fallback {
            continue;
        }
        let outcome = mutate_entity(
            rt,
            token,
            project_uuid(&owner.source_project),
            &coverage.file_label,
            report,
            |current| {
                let mut entity = current?.clone();
                let props = entity.properties.as_mut()?.as_object_mut()?;
                let runs = props.get_mut("l2_sweep_runs")?.as_object_mut()?;
                let entry = runs.get_mut(&owner.language)?;
                if !valid_l2_sweep_entry(entry)
                    || entry["attempted"]["run_id"].as_str() != Some(run_id.as_str())
                {
                    return None;
                }
                entry["completed"] = json!({
                    "run_id": run_id,
                    "sweep_time": sweep_time.to_rfc3339(),
                });
                entity.updated_at = ts(sweep_time);
                Some(entity)
            },
        )
        .await?;
        if outcome.wrote() {
            report.projects_updated += 1;
        }
    }
    Ok(())
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
    #[cfg(test)]
    if content.contains("l2_worker_probe_3292") {
        if let Some(observer) = scanner_thread_observer()
            .lock()
            .expect("scanner observer lock")
            .take()
        {
            let current = std::thread::current();
            let _ = observer.send((current.id(), current.name().map(str::to_string)));
        }
    }
    crate::scanner_rust::scan_rust_source(content)
        .map(crate::extractor::from_rust_scan)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
type ScannerThreadObservation = (std::thread::ThreadId, Option<String>);

#[cfg(test)]
type ScannerThreadObserver =
    std::sync::Mutex<Option<std::sync::mpsc::Sender<ScannerThreadObservation>>>;

#[cfg(test)]
fn scanner_thread_observer() -> &'static ScannerThreadObserver {
    static OBSERVER: OnceLock<ScannerThreadObserver> = OnceLock::new();
    OBSERVER.get_or_init(|| std::sync::Mutex::new(None))
}

/// Parsing and adaptation may recurse inside syn and the scanner. Keep that
/// work off the async executor on a known stack, with a process-wide cap on
/// concurrent scanner threads. The caller has already checked source size
/// and delimiter depth before this function is reached.
async fn parse_rust_file_on_worker(content: String) -> Result<ExtractedFile, String> {
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let workers = WORKERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(RUST_L2_SCANNER_WORKERS)))
        .clone();
    let permit = workers
        .acquire_owned()
        .await
        .map_err(|_| "scanner worker pool unavailable".to_string())?;
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("khive-rust-l2-scanner".to_string())
        .stack_size(RUST_L2_SCANNER_STACK_BYTES)
        .spawn(move || {
            let _permit = permit;
            let result = std::panic::catch_unwind(|| parse_rust_file(&content))
                .unwrap_or_else(|_| Err("scanner worker panicked".to_string()));
            let _ = send.send(result);
        })
        .map_err(|error| format!("scanner worker unavailable: {error}"))?;
    receive
        .await
        .map_err(|_| "scanner worker terminated".to_string())?
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

/// Unchanged content reuses its ownership stamp only when that stamp was
/// produced by this Rust scanner identity version. Older generic method IDs
/// must be recomputed even when the source bytes have not changed.
fn l2_needs_reparse(
    existing_content_hash: Option<&str>,
    existing_declaration_ids: Option<&Value>,
    existing_identity_version: Option<u64>,
    new_content_hash: &str,
) -> bool {
    existing_content_hash != Some(new_content_hash)
        || existing_identity_version != Some(RUST_L2_SCANNER_IDENTITY_VERSION)
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

/// Unresolved call/type tuple. Declaration rows retain historical unions;
/// accepted per-file entries alone authorize late materialization.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
struct L2UnresolvedRef {
    segments: Vec<String>,
    evidence: String,
}

#[cfg(test)]
fn read_l2_unresolved(properties: &Value) -> Vec<L2UnresolvedRef> {
    properties
        .get("l2_unresolved_references")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Attempt immediate same-project resolution of one call/type reference
/// declared by `declaring_id`; on success upserts (or refreshes) a
/// `depends_on` edge with the given evidence, on failure stages a pending
/// reference for the phase-local flush and reresolve pass. Nonfatal either way.
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
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<L2UnresolvedRef>, CodeSourceIngestError> {
    if segments.is_empty() {
        return Ok(None);
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
            return Ok(Some(reference));
        }
        None => {}
    }
    Ok(None)
}

/// Attempt immediate same-project resolution of one positive `impl Trait for
/// Type`. Unlike a call/type reference, an impl has no declaring storage
/// entity of its own, so a failed
/// resolution is staged as a pending impl on the *file module* instead,
/// for the reresolve pass to retry.
#[allow(clippy::too_many_arguments)]
async fn resolve_l2_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    containing_module_path: &str,
    current_file_ids: &BTreeSet<Uuid>,
    type_path: &[String],
    trait_path: &[String],
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<L2PendingImpl>, CodeSourceIngestError> {
    if type_path.is_empty() || trait_path.is_empty() {
        return Ok(None);
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
            return Ok(Some(L2PendingImpl {
                type_path: type_path.to_vec(),
                trait_path: trait_path.to_vec(),
            }));
        }
    }
    Ok(None)
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
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

#[derive(Clone)]
struct PendingL2<T> {
    value: T,
    file: String,
}

fn append_l2_pending<T: Clone + Eq + Hash>(list: &mut Vec<T>, additions: &[T]) -> usize {
    let mut seen: HashSet<T> = list.iter().cloned().collect();
    let mut appended = 0;
    for addition in additions {
        if seen.insert(addition.clone()) {
            list.push(addition.clone());
            appended += 1;
        }
    }
    appended
}

fn rebase_l2_pending<T: Clone + Eq + Hash>(
    current: &mut Vec<T>,
    original: &HashSet<T>,
    remaining: &[T],
) {
    current.retain(|value| !original.contains(value));
    append_l2_pending(current, remaining);
}

async fn record_l2_pending_batch<T>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    key: &str,
    pending: &[PendingL2<T>],
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError>
where
    T: Clone + Eq + Hash + serde::Serialize + serde::de::DeserializeOwned,
{
    if pending.is_empty() {
        return Ok(false);
    }
    let read_list = |entity: &Entity| -> Vec<T> {
        entity
            .properties
            .as_ref()
            .and_then(|props| props.get(key))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    };
    let Some(current) = rt
        .entities(token)?
        .get_entity_including_deleted(id)
        .await
        .map_err(|error| CodeSourceIngestError::Storage(error.to_string()))?
    else {
        return Ok(false);
    };
    #[cfg(test)]
    l2_batch_tests::observe_row_read(id);
    let existing: HashSet<T> = read_list(&current).into_iter().collect();
    if pending.iter().all(|item| existing.contains(&item.value)) {
        return Ok(false);
    }
    advancing_entity_revision(current.updated_at, current.updated_at)?;
    if let Err(error) = gate_check(&current) {
        match error {
            RuntimeError::SecretDetected(secret) => {
                for item in pending
                    .iter()
                    .filter(|item| !existing.contains(&item.value))
                {
                    report.blocked_count += 1;
                    report.blocked.push(BlockedWrite {
                        file: item.file.clone(),
                        detector: secret.detector.to_string(),
                        masked_excerpt: secret.masked.clone(),
                    });
                }
                return Ok(false);
            }
            other => return Err(other.into()),
        }
    }
    let mut allowed = Vec::new();
    let mut occurrences = Vec::new();
    let mut seen = existing.clone();
    for item in pending {
        if existing.contains(&item.value) {
            continue;
        }
        if seen.contains(&item.value) {
            occurrences.push(item);
            continue;
        }
        match secret_gate::check_json_at(
            &serde_json::to_value(&item.value).expect("serializes"),
            "entity",
            "properties",
        ) {
            Ok(()) => {
                seen.insert(item.value.clone());
                allowed.push(item.value.clone());
                occurrences.push(item);
            }
            Err(RuntimeError::SecretDetected(secret)) => {
                report.blocked_count += 1;
                report.blocked.push(BlockedWrite {
                    file: item.file.clone(),
                    detector: secret.detector.to_string(),
                    masked_excerpt: secret.masked,
                });
            }
            Err(other) => return Err(other.into()),
        }
    }
    if allowed.is_empty() {
        return Ok(false);
    }
    #[cfg(test)]
    l2_batch_tests::pause_before_batch().await;
    let mut attempted_files = Vec::new();
    let outcome = mutate_entity(rt, token, id, &pending[0].file, report, |current| {
        attempted_files.clear();
        let mut entity = current?.clone();
        let mut list = read_list(&entity);
        let present: HashSet<T> = list.iter().cloned().collect();
        attempted_files.extend(
            occurrences
                .iter()
                .filter(|item| !present.contains(&item.value))
                .map(|item| item.file.clone()),
        );
        if append_l2_pending(&mut list, &allowed) == 0 {
            return None;
        }
        let mut props = entity
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        props.insert(key.into(), serde_json::to_value(list).expect("serializes"));
        entity.properties = Some(Value::Object(props));
        Some(entity)
    })
    .await?;
    if outcome == RowMutationOutcome::Blocked {
        let refusal = report
            .blocked
            .pop()
            .expect("blocked mutation records refusal");
        report.blocked_count -= 1;
        for file in attempted_files {
            report.blocked_count += 1;
            report.blocked.push(BlockedWrite {
                file,
                detector: refusal.detector.clone(),
                masked_excerpt: refusal.masked_excerpt.clone(),
            });
        }
    }
    Ok(outcome.wrote())
}

/// Upsert (create or refresh) the `contains` edge from a declaration's
/// owning module to the declaration itself.
#[derive(Clone, Copy)]
struct L2ContainmentStamp<'a> {
    language: &'a str,
    sweep_time: DateTime<Utc>,
    run_id: Uuid,
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
    let outcome = mutate_edge(rt, token, edge_id, |current| {
        let metadata = match current.and_then(|edge| edge.metadata.as_ref()) {
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
                "l2_observed_run_id": stamp.run_id.to_string(),
            }),
        };
        Some(Edge {
            id: LinkId::from(edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id: owner_id,
            target_id: child_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            created_at: current
                .map(|edge| edge.created_at)
                .unwrap_or(stamp.sweep_time),
            updated_at: stamp.sweep_time,
            deleted_at: None,
            metadata: Some(metadata),
            target_backend: current.and_then(|edge| edge.target_backend.clone()),
        })
    })
    .await?;
    match outcome {
        RowMutationOutcome::Created => report.edges_created += 1,
        RowMutationOutcome::Updated => report.edges_updated += 1,
        RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
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
    run_id: Uuid,
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
    let now = ts(sweep_time);
    let outcome = mutate_entity(rt, token, id, file_label, report, |current| {
        let mut entity = current.cloned().unwrap_or_else(|| {
            Entity::new(token.namespace().as_str(), "concept", decl.name.clone())
                .with_entity_type(Some(canonical_kind))
        });
        let mut props = entity
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
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
        props.insert("l2_observed_run_id".into(), json!(run_id.to_string()));
        entity.id = id;
        entity.namespace = token.namespace().as_str().to_string();
        entity.kind = "concept".to_string();
        entity.entity_type = Some(canonical_kind.to_string());
        entity.name = decl.name.clone();
        entity.description = decl.description.clone();
        entity.properties = Some(Value::Object(props));
        entity.updated_at = now;
        Some(entity)
    })
    .await?;
    if outcome == RowMutationOutcome::Blocked {
        return Ok(None);
    }
    if let Some(l2) = report.l2.as_mut() {
        match outcome {
            RowMutationOutcome::Created => l2.symbols_created += 1,
            RowMutationOutcome::Updated => l2.symbols_updated += 1,
            RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
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
    mutate_entity(rt, token, module_id, file_label, report, |current| {
        let mut module = current?.clone();
        let mut props = module
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())?;
        let changed = [
            "declaration_ids",
            "l2_pending_impls",
            "l2_content_hash",
            "l2_scanner_identity_version",
        ]
        .into_iter()
        .any(|key| props.remove(key).is_some());
        if !changed {
            return None;
        }
        module.properties = Some(Value::Object(props));
        Some(module)
    })
    .await?;
    Ok(())
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
            state.run_id,
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

    let observation_start = state.observed_natural_edge_ids.len();
    let mut file_references = Vec::new();
    let mut file_implementations = Vec::new();
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
                run_id: state.run_id,
            },
            false,
            report,
        )
        .await?;

        let mut pending_references = Vec::new();
        for call in &decl.calls {
            if let Some(reference) = resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &call.segments,
                "call",
                sweep_time,
                state,
                report,
            )
            .await?
            {
                pending_references.push(PendingL2 {
                    value: reference,
                    file: file_label.to_owned(),
                });
            }
        }
        for type_ref in &decl.type_refs {
            if let Some(reference) = resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &type_ref.segments,
                "type_reference",
                sweep_time,
                state,
                report,
            )
            .await?
            {
                pending_references.push(PendingL2 {
                    value: reference,
                    file: file_label.to_owned(),
                });
            }
        }
        file_references.extend(pending_references.iter().map(|pending| FileReference {
            declaration_id: *id,
            module_path: containing_module_path.clone(),
            reference: pending.value.clone(),
        }));
        // Preserve declaration history without granting it late edge authority.
        record_l2_pending_batch(
            rt,
            token,
            *id,
            "l2_unresolved_references",
            &pending_references,
            report,
        )
        .await?;
    }

    // Phase C: positive trait implementations.
    let mut pending_impls = Vec::new();
    for imp in &parsed.impls {
        let containing_module_path = resolve_module_path(module_path, &imp.module_segments);
        if !imp.type_path.is_empty() && !imp.trait_path.is_empty() {
            file_implementations.push(file_pending::FileImplementation {
                module_path: containing_module_path.clone(),
                implementation: L2PendingImpl {
                    type_path: imp.type_path.clone(),
                    trait_path: imp.trait_path.clone(),
                },
            });
        }
        if let Some(entry) = resolve_l2_implements(
            rt,
            token,
            source_project,
            language,
            &containing_module_path,
            &current_file_ids,
            &imp.type_path,
            &imp.trait_path,
            sweep_time,
            state,
            report,
        )
        .await?
        {
            pending_impls.push(PendingL2 {
                value: entry,
                file: file_label.to_owned(),
            });
        }
    }
    record_l2_pending_batch(
        rt,
        token,
        module_id,
        "l2_pending_impls",
        &pending_impls,
        report,
    )
    .await?;

    declaration_ids.sort();
    declaration_ids.dedup();
    if !stamp_l2_declarations(
        rt,
        token,
        module_id,
        &declaration_ids,
        &file_references,
        &state.observed_natural_edge_ids[observation_start..],
        &file_implementations,
        content_hash,
        file_label,
        report,
    )
    .await?
    {
        return Ok(None);
    }
    Ok(Some(declaration_ids))
}

struct L2SweepInputs<'a> {
    ingest_root: &'a Path,
    snapshot: &'a SourceSnapshot,
    sweep_time: DateTime<Utc>,
}

/// Walk every `.rs` file under `ingest_root`, ensure its project/module L2
/// ownership scaffolding exists, and (re)parse it when needed
/// Rust-only; other-language selections
/// never reach this function (`run_code_ingest` only calls it when L2 is
/// enabled, and L2 itself scans Rust exclusively).
async fn run_l2_sweep(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    inputs: L2SweepInputs<'_>,
    project_ids: &mut HashMap<(String, String), Uuid>,
    previous_l2_sweep_stamps: &mut PreviousL2SweepStamps,
    report: &mut CodeSourceIngestReport,
) -> Result<L2SweepState, CodeSourceIngestError> {
    let L2SweepInputs {
        ingest_root,
        snapshot,
        sweep_time,
    } = inputs;
    const LANGUAGE: &str = "rust";
    let mut state = L2SweepState::new(previous_l2_sweep_stamps.run_id);
    let Some(ext) = imports::extension_for_language(LANGUAGE) else {
        return Ok(state);
    };
    let walk = match walk_source_files_on_worker(ingest_root, ext).await? {
        Ok(walk) => walk,
        Err(error) => {
            report
                .warnings
                .push(format!("walking {}: {error}", ingest_root.display()));
            return Ok(state);
        }
    };
    let SourceWalkResult {
        canonical_root: canonical_ingest_root,
        files,
        skipped_outside_root,
        skipped_non_regular,
        skipped_non_source,
    } = walk;
    for skipped in skipped_outside_root {
        report.warnings.push(format!(
            "L2 skipped source outside the canonical ingest root: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_regular {
        report.warnings.push(format!(
            "L2 skipped non-regular source: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_source {
        report.warnings.push(format!(
            "L2 skipped non-source traversal entry: {}",
            skipped.display()
        ));
    }
    if !files.is_empty() {
        record_observed_language(report, LANGUAGE);
    }

    // L2-only never requests the L1 manifest walk. Read only ancestors of
    // canonical source paths and reuse that parsed snapshot for every file.
    let files_for_manifests = files.clone();
    let root_for_manifests = canonical_ingest_root.clone();
    let (manifests, failures) = blocking_io(move || {
        manifest::discover_rust_manifests_for_sources(&files_for_manifests, &root_for_manifests)
    })
    .await?;
    record_manifest_failures(report, failures);
    let manifest_index = manifest::ManifestIndex::new(&manifests);

    // Resolve every encountered owner before any unchanged-file decision.
    // A later fallback file disqualifies the owner for the whole invocation,
    // including files encountered earlier through a governing manifest.
    let files: Vec<_> = files
        .into_iter()
        .filter_map(|file| {
            let governing =
                manifest_index.governing(file.parent()?, &canonical_ingest_root, LANGUAGE);
            let fallback = governing.is_none();
            let (proj_root, proj_name) = governing.unwrap_or_else(|| {
                (
                    canonical_ingest_root.clone(),
                    basename_project_name(ingest_root, &canonical_ingest_root),
                )
            });
            let owner = L2OwnerKey {
                source_project: proj_name.clone(),
                language: LANGUAGE.to_string(),
            };
            let whole = proj_root.starts_with(&canonical_ingest_root);
            state
                .owners
                .entry(owner.clone())
                .and_modify(|coverage| {
                    coverage.whole &= whole;
                    coverage.fallback |= fallback;
                })
                .or_insert_with(|| L2OwnerCoverage {
                    whole,
                    fallback,
                    file_label: file.display().to_string(),
                });
            if fallback {
                // This also discards authority captured by an earlier L1 or
                // L1.5 upsert. Completion retention is a separate decision.
                previous_l2_sweep_stamps.stamps.insert(owner, None);
            }
            Some((file, proj_root, proj_name))
        })
        .collect();

    for (file, proj_root, proj_name) in files {
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, LANGUAGE) else {
            report.files_skipped_without_module_path += 1;
            continue;
        };
        let (source_path, warnings, dropped) =
            derive_source_path_on_worker(&file, &canonical_ingest_root, &snapshot.root).await?;
        report.warnings.extend(warnings);
        report.files_dropped_without_source_path += dropped;
        let Some(source_path) = source_path else {
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
            previous_l2_sweep_stamps,
            report,
        )
        .await?
        else {
            continue;
        };

        let file_for_read = file.clone();
        let root_for_read = canonical_ingest_root.clone();
        #[cfg(test)]
        l2_recovery_tests::before_source_read(&file).await;
        let source =
            match blocking_io(move || read_l2_source(&root_for_read, &file_for_read)).await? {
                Ok(source) => source,
                Err(error) => {
                    record_source_read_failure(report, "L2", &file, error);
                    continue;
                }
            };
        let hash = source.hash().to_string();
        let refused = matches!(&source, L2Source::Refused { .. });

        let precomputed_module_id = module_uuid(&proj_name, LANGUAGE, &module_path);
        let existing_module = get_entity_opt(rt, token, precomputed_module_id).await?;
        let owner = L2OwnerKey {
            source_project: proj_name.clone(),
            language: LANGUAGE.to_string(),
        };
        let recovering = previous_l2_sweep_stamps
            .get(&owner)
            .and_then(Option::as_ref)
            .is_none();
        let needs_reparse = recovering
            || refused
            || existing_module
                .as_ref()
                .and_then(|entity| entity.properties.as_ref())
                .and_then(|properties| file_pending::read_file(properties, &file_label))
                .filter(|entry| entry.content_hash == hash && entry.natural_edge_ids.is_some())
                .is_none()
            || l2_needs_reparse(
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("l2_content_hash"))
                    .and_then(Value::as_str),
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("declaration_ids")),
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("l2_scanner_identity_version"))
                    .and_then(Value::as_u64),
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
                run_id: state.run_id,
            },
            true,
            report,
        )
        .await?;

        if !needs_reparse {
            let accepted = existing_module
                .as_ref()
                .and_then(|entity| entity.properties.as_ref())
                .and_then(|properties| file_pending::read_file(properties, &file_label))
                .expect("reuse requires accepted producer coverage");
            let declaration_ids = accepted.declaration_ids;
            let natural_edge_ids = accepted.natural_edge_ids.expect("checked inventory");
            if refresh_l2_declarations(
                rt,
                token,
                &proj_name,
                LANGUAGE,
                &source_path,
                &snapshot.revision,
                sweep_time,
                state.run_id,
                &file_label,
                &declaration_ids,
                &natural_edge_ids,
                previous_l2_sweep_stamps
                    .get(&owner)
                    .and_then(Option::as_ref),
                report,
            )
            .await?
            {
                state
                    .current_files
                    .insert((module_id, file_label.clone()), hash.clone());
                state.mark_current_module(module_id, &proj_name, LANGUAGE);
                state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
                state
                    .unchanged_declarations
                    .extend(declaration_ids.iter().copied());
                state.current_natural_edge_ids.extend(natural_edge_ids);
                continue;
            }
        }

        clear_l2_ownership(rt, token, module_id, &file_label, report).await?;
        let parse_result = match source {
            L2Source::Ready { content, .. } => {
                let result = parse_rust_file_on_worker(content).await;
                #[cfg(test)]
                l2_recovery_tests::observe_parse(&file);
                result
            }
            L2Source::Refused { reason, .. } => Err(reason),
        };
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
            state
                .current_files
                .insert((module_id, file_label.clone()), hash.clone());
            state.mark_current_module(module_id, &proj_name, LANGUAGE);
            state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
        }
    }
    Ok(state)
}

/// L2 synchronous re-resolve pass, run once after the whole L2 file walk
/// completes (mirrors L1.5's `reresolve_pass`): revisits every symbol
/// carrying accepted per-file call/type references and every module carrying pending
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

    file_pending::reresolve(rt, token, sweep_time, state, report).await?;

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
    let mut resolved = HashMap::new();
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
        let original_pending: HashSet<_> = pending.iter().cloned().collect();
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
                    resolved.insert(
                        (module_id, entry),
                        edge_uuid(EdgeRelation::Implements, type_id, trait_id),
                    );
                }
                _ => still_pending.push(entry),
            }
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_dependencies_unresolved += still_pending.len() as u64;
        }
        if resolved_any {
            let label = module_id.to_string();
            #[cfg(test)]
            l2_batch_tests::pause_before_rebase().await;
            mutate_entity(rt, token, module_id, &label, report, |current| {
                let mut module = current?.clone();
                let mut rebased = module
                    .properties
                    .as_ref()
                    .map(read_l2_pending_impls)
                    .unwrap_or_default();
                rebase_l2_pending(&mut rebased, &original_pending, &still_pending);
                let mut props = module
                    .properties
                    .clone()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if rebased.is_empty() {
                    props.remove("l2_pending_impls");
                } else {
                    props.insert(
                        "l2_pending_impls".into(),
                        serde_json::to_value(&rebased).expect("serializes"),
                    );
                }
                module.properties = Some(Value::Object(props));
                Some(module)
            })
            .await?;
        }
    }

    file_pending::record_resolved_implementations(rt, token, state, &resolved, report).await?;
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
#[path = "source_ingest/owner_alias_tests.rs"]
mod owner_alias_tests;

#[cfg(test)]
#[path = "source_ingest/reresolve_projection_tests.rs"]
mod reresolve_projection_tests;

#[cfg(test)]
mod l2_batch_tests;

#[cfg(test)]
mod l2_recovery_tests;

#[cfg(test)]
mod basename_project_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use khive_db::StorageBackend;
    use khive_runtime::{Namespace, RuntimeConfig};
    use tempfile::TempDir;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_worker_yields_the_async_executor() {
        let worker = blocking_io(|| std::thread::sleep(std::time::Duration::from_millis(200)));
        tokio::pin!(worker);
        tokio::select! {
            biased;
            _ = &mut worker => panic!("blocking work completed before the executor timer"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        worker.await.expect("blocking worker completed");
    }

    fn runtime_on(db_path: &Path) -> (KhiveRuntime, NamespaceToken) {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path.to_path_buf()),
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("target runtime opens");
        let token = runtime.authorize(Namespace::local()).expect("token");
        (runtime, token)
    }

    #[derive(Clone, Copy)]
    pub(super) enum TestJournalMode {
        Wal,
        Delete,
    }

    impl TestJournalMode {
        fn label(self) -> &'static str {
            match self {
                Self::Wal => "wal",
                Self::Delete => "delete",
            }
        }

        fn wal_mode(self) -> bool {
            matches!(self, Self::Wal)
        }
    }

    pub(super) fn runtime_on_with_mode(
        db_path: &Path,
        mode: TestJournalMode,
    ) -> (KhiveRuntime, NamespaceToken) {
        // Lock files beside the database: the fixture's own lease slot, so a
        // test pausing an ingest inside the lease stalls only its own store.
        runtime_on_with_mode_in(db_path, mode, db_path.with_extension("volume-locks"))
    }

    pub(super) fn runtime_on_with_mode_in(
        db_path: &Path,
        mode: TestJournalMode,
        volume_lock_dir: std::path::PathBuf,
    ) -> (KhiveRuntime, NamespaceToken) {
        let backend = Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode_in(
                db_path,
                mode.wal_mode(),
                std::time::Duration::from_secs(5),
                volume_lock_dir,
            )
            .expect("target backend opens"),
        );
        backend.prepare_core_schema().expect("fresh schema");
        let runtime = KhiveRuntime::from_prepared_backend(
            backend,
            RuntimeConfig {
                db_path: Some(db_path.to_path_buf()),
                packs: vec![],
                ..RuntimeConfig::no_embeddings()
            },
        )
        .expect("target runtime opens");
        let writer = runtime.backend().pool().writer().expect("writer");
        let actual_mode: String = writer
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        let busy_timeout_ms: i64 = writer
            .conn()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy timeout");
        assert_eq!(actual_mode.to_ascii_lowercase(), mode.label());
        assert_eq!(busy_timeout_ms, 5_000);
        drop(writer);
        let token = runtime.authorize(Namespace::local()).expect("token");
        (runtime, token)
    }

    #[test]
    fn rust_l2_safety_guard_ignores_literals_and_comments() {
        let braces = "{".repeat(RUST_L2_MAX_DELIMITER_DEPTH + 1);
        let source = format!(
            "// {braces}\n/* {braces} */\nconst RAW: &str = r#\"{braces}\"#;\nconst QUOTED: &str = \"{braces}\";\nfn valid<'a>(value: &'a str) {{ let _brace = '{{'; let _ = value; }}\n"
        );
        assert!(check_rust_l2_nesting(&source).is_ok());
        assert!(check_rust_l2_nesting("#!/usr/bin/env rust-script ]\nfn valid() {}\n").is_ok());

        let nested = format!("{}0{}", "(".repeat(65), ")".repeat(65));
        assert!(check_rust_l2_nesting(&nested)
            .unwrap_err()
            .contains("scanner safety limit"));
        let generic = format!("type Deep = {}u8{};", "Vec<".repeat(65), ">".repeat(65));
        assert!(check_rust_l2_nesting(&generic)
            .unwrap_err()
            .contains("generic nesting"));
        let unary = format!("fn f() {{ let _ = {}true; }}", "!".repeat(129));
        assert!(check_rust_l2_nesting(&unary)
            .unwrap_err()
            .contains("expression is too complex"));
    }

    #[test]
    fn rust_l2_safety_guard_accepts_large_flat_item() {
        let mut source = String::from("pub fn many_statements() {\n");
        for _ in 0..1_024 {
            source.push_str("let _ = 0;\n");
        }
        source.push_str("}\n");

        assert!(check_rust_l2_nesting(&source).is_ok());
    }

    #[test]
    fn rust_l2_oversized_file_is_refused_without_reading_it() {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("oversized.rs");
        let source = " ".repeat(RUST_L2_MAX_SOURCE_BYTES + 1);
        fs::write(&path, &source).expect("source file");
        let L2Source::Refused { hash, reason } =
            read_l2_source(&root.path().canonicalize().expect("canonical root"), &path)
                .expect("bounded read")
        else {
            panic!("oversized source must not be retained for parsing");
        };
        assert_eq!(hash, format!("refused:size:{}", source.len()));
        assert!(reason.contains("scanner safety limit"));
    }

    #[cfg(unix)]
    #[test]
    fn import_scan_reader_refuses_opened_source_outside_root() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("ingest root");
        let outside = TempDir::new().expect("outside root");
        let source = outside.path().join("outside.py");
        fs::write(&source, "import must_not_ingest\n").expect("outside source");
        let link = root.path().join("source.py");
        symlink(&source, &link).expect("outside link");
        assert!(matches!(
            scan_import_source(&root.path().canonicalize().expect("canonical root"), &link, "python"),
            Err(SourceReadError::Refused(reason)) if reason.contains("escapes the canonical ingest root")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rust_l2_reader_refuses_opened_source_outside_root() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("ingest root");
        let outside = TempDir::new().expect("outside root");
        let source = outside.path().join("outside.rs");
        fs::write(&source, "pub fn must_not_ingest() {}\n").expect("outside source");
        let link = root.path().join("source.rs");
        symlink(&source, &link).expect("outside link");
        assert!(matches!(
            read_l2_source(&root.path().canonicalize().expect("canonical root"), &link),
            Err(SourceReadError::Refused(reason)) if reason.contains("escapes the canonical ingest root")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_l2_scanner_runs_off_the_ingest_thread() {
        let root = TempDir::new().expect("tempdir");
        let project = root.path().join("worker_probe");
        fs::create_dir_all(project.join("src")).expect("source directory");
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"worker_probe\"\n",
        )
        .expect("manifest");
        fs::write(
            project.join("src/lib.rs"),
            "pub fn l2_worker_probe_3292() {}\n",
        )
        .expect("source");
        let (runtime, token) = runtime_on(&root.path().join("worker.db"));
        let (send, receive) = std::sync::mpsc::channel();
        *scanner_thread_observer()
            .lock()
            .expect("scanner observer lock") = Some(send);

        let caller = std::thread::current().id();
        let report = run_code_ingest(
            &runtime,
            &token,
            CodeSourceIngestOptions {
                path: &project,
                languages: ["rust"].into_iter().collect(),
                sweep_time: Utc::now(),
                enable_l1: false,
                enable_l1_5: false,
                enable_l2: true,
            },
        )
        .await
        .expect("L2 ingest");
        let (scanner, name) = receive
            .try_recv()
            .expect("valid Rust source reaches scanner");
        assert_ne!(
            scanner, caller,
            "source parsing must leave the ingest thread"
        );
        assert_eq!(name.as_deref(), Some("khive-rust-l2-scanner"));
        assert_eq!(report.l2.expect("L2 report").symbol_parse_failures, 0);
    }

    #[test]
    fn invalid_declaration_ids_force_reparse() {
        let hash = "0123456789abcdef";

        assert!(l2_needs_reparse(
            Some(hash),
            None,
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&Value::Null),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&json!("not-an-array")),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&json!(["not-a-uuid"])),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
        assert!(!l2_needs_reparse(
            Some(hash),
            Some(&json!([])),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
        assert!(!l2_needs_reparse(
            Some(hash),
            Some(&json!([Uuid::nil().to_string()])),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
            hash
        ));
    }

    #[test]
    fn old_scanner_identity_stamp_forces_reparse_of_unchanged_file() {
        let hash = "0123456789abcdef";
        let declarations = json!([Uuid::nil().to_string()]);
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&declarations),
            None,
            hash
        ));
        assert!(l2_needs_reparse(
            Some(hash),
            Some(&declarations),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION - 1),
            hash
        ));
        assert!(!l2_needs_reparse(
            Some(hash),
            Some(&declarations),
            Some(RUST_L2_SCANNER_IDENTITY_VERSION),
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
            Uuid::new_v4(),
            "src/lib.rs",
            &mut report,
        )
        .await
        .expect_err("symbol FTS failure must abort");
        assert!(error.to_string().contains("entity FTS indexing"));
        assert_eq!(report.fts_indexed, 0);
        assert_eq!(report.l2.expect("L2 report").symbols_created, 0);
    }

    async fn concurrent_unresolved_additions_rebase_without_losing_either_specifier_in_mode(
        mode: TestJournalMode,
    ) {
        let root = TempDir::new().expect("temporary database directory");
        let db_path = root.path().join(format!("entity-race-{}.db", mode.label()));
        let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
        let entity_id = project_uuid("race-fixture");
        let mut entity = Entity::new(token_a.namespace().as_str(), "project", "race-fixture");
        entity.id = entity_id;
        entity.properties = Some(json!({"source_project": "race-fixture"}));
        let seed_revision = entity.updated_at;
        runtime_a
            .entities(&token_a)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
        let specifier_a = UnresolvedSpec {
            specifier: "alpha".to_string(),
            target_kind: "project".to_string(),
            dependency_kind: "dependencies".to_string(),
            dependency_scope: "normal".to_string(),
            language: "rust".to_string(),
        };
        let specifier_b = UnresolvedSpec {
            specifier: "beta".to_string(),
            target_kind: "project".to_string(),
            dependency_kind: "dev-dependencies".to_string(),
            dependency_scope: "dev".to_string(),
            language: "rust".to_string(),
        };
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let pause_a = std::sync::Arc::new(race_seam::OneShotPause::new(std::sync::Arc::clone(
            &barrier,
        )));
        let pause_b = std::sync::Arc::new(race_seam::OneShotPause::new(barrier));
        let mut report_a = CodeSourceIngestReport::default();
        let mut report_b = CodeSourceIngestReport::default();
        let pending_a = [PendingUnresolved {
            spec: specifier_a.clone(),
            file: "alpha.rs".to_string(),
        }];
        let pending_b = [PendingUnresolved {
            spec: specifier_b.clone(),
            file: "beta.rs".to_string(),
        }];

        let (result_a, result_b) = tokio::join!(
            race_seam::AFTER_ROW_READ.scope(
                pause_a,
                record_unresolved_batch(&runtime_a, &token_a, entity_id, &pending_a, &mut report_a),
            ),
            race_seam::AFTER_ROW_READ.scope(
                pause_b,
                record_unresolved_batch(&runtime_b, &token_b, entity_id, &pending_b, &mut report_b),
            ),
        );
        result_a.expect("writer A completes");
        result_b.expect("writer B completes");

        let stored = runtime_a
            .entities(&token_a)
            .expect("entity store")
            .get_entity(entity_id)
            .await
            .expect("read entity")
            .expect("entity remains");
        let specifiers = stored
            .properties
            .as_ref()
            .map(read_unresolved)
            .unwrap_or_default();
        assert_eq!(
            specifiers.len(),
            2,
            "two additions derived from one entity revision must both survive: {specifiers:?}"
        );
        assert!(specifiers.contains(&specifier_a));
        assert!(specifiers.contains(&specifier_b));
        assert!(stored.updated_at > seed_revision);
        assert_eq!(report_a.unresolved_recorded, 1);
        assert_eq!(report_b.unresolved_recorded, 1);
        assert_eq!(report_a.fts_indexed, 1);
        assert_eq!(report_b.fts_indexed, 1);
    }

    #[tokio::test]
    async fn concurrent_unresolved_additions_rebase_without_losing_either_specifier() {
        for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
            concurrent_unresolved_additions_rebase_without_losing_either_specifier_in_mode(mode)
                .await;
        }
    }

    #[tokio::test]
    async fn unresolved_batch_keeps_order_and_dedup_with_one_owner_write() {
        let root = TempDir::new().expect("temporary database directory");
        let (runtime, token) = runtime_on(&root.path().join("unresolved-batch.db"));
        let entity_id = project_uuid("batch-fixture");
        let existing = UnresolvedSpec {
            specifier: "existing".to_string(),
            target_kind: "project".to_string(),
            dependency_kind: "dependencies".to_string(),
            dependency_scope: "normal".to_string(),
            language: "rust".to_string(),
        };
        let mut entity = Entity::new(token.namespace().as_str(), "project", "batch-fixture");
        entity.id = entity_id;
        entity.properties = Some(json!({
            "source_project": "batch-fixture",
            "unresolved_specifiers": [existing],
        }));
        runtime
            .entities(&token)
            .expect("entity store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let spec = |name: String| UnresolvedSpec {
            specifier: name,
            target_kind: "project".to_string(),
            dependency_kind: "dependencies".to_string(),
            dependency_scope: "normal".to_string(),
            language: "rust".to_string(),
        };
        let mut pending: Vec<_> = (0..64)
            .map(|i| PendingUnresolved {
                spec: spec(format!("missing_{i:02}")),
                file: "Cargo.toml".to_string(),
            })
            .collect();
        pending.insert(
            1,
            PendingUnresolved {
                spec: spec("existing".to_string()),
                file: "Cargo.toml".to_string(),
            },
        );
        pending.push(PendingUnresolved {
            spec: spec("missing_00".to_string()),
            file: "Cargo.toml".to_string(),
        });
        pending.push(PendingUnresolved {
            spec: spec("scheme://user:pass@host".to_string()),
            file: "blocked.toml".to_string(),
        });
        let mut report = CodeSourceIngestReport::default();
        record_unresolved_batch(&runtime, &token, entity_id, &pending, &mut report)
            .await
            .expect("batch appends safe siblings");

        let stored = runtime
            .entities(&token)
            .expect("entity store")
            .get_entity(entity_id)
            .await
            .expect("read entity")
            .expect("entity remains");
        let list = read_unresolved(stored.properties.as_ref().expect("properties"));
        let expected: Vec<_> = std::iter::once(spec("existing".to_string()))
            .chain((0..64).map(|i| spec(format!("missing_{i:02}"))))
            .collect();
        assert_eq!(list, expected, "append order and dedup must be stable");
        assert_eq!(report.unresolved_recorded, 64);
        assert_eq!(report.fts_indexed, 1, "one owner gets one FTS upsert");
        assert_eq!(report.blocked_count, 1);
        assert_eq!(report.blocked[0].file, "blocked.toml");
    }

    async fn concurrent_dependency_evidence_rebases_without_losing_either_kind_in_mode(
        mode: TestJournalMode,
    ) {
        let root = TempDir::new().expect("temporary database directory");
        let db_path = root.path().join(format!("edge-race-{}.db", mode.label()));
        let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
        let source_id = project_uuid("source");
        let target_id = project_uuid("target");
        for (id, name) in [(source_id, "source"), (target_id, "target")] {
            let mut entity = Entity::new(token_a.namespace().as_str(), "project", name);
            entity.id = id;
            runtime_a
                .entities(&token_a)
                .expect("entity store")
                .upsert_entity(entity)
                .await
                .expect("seed endpoint");
        }
        let seed_time = Utc::now();
        let mut seed_report = CodeSourceIngestReport::default();
        upsert_dependency_edge(
            &runtime_a,
            &token_a,
            source_id,
            target_id,
            "dependencies",
            "normal",
            "rust",
            seed_time,
            &mut seed_report,
        )
        .await
        .expect("seed dependency edge");

        let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let pause_a = std::sync::Arc::new(race_seam::OneShotPause::new(std::sync::Arc::clone(
            &barrier,
        )));
        let pause_b = std::sync::Arc::new(race_seam::OneShotPause::new(barrier));
        let mut report_a = CodeSourceIngestReport::default();
        let mut report_b = CodeSourceIngestReport::default();
        let update_time = seed_time + chrono::Duration::seconds(1);

        let (result_a, result_b) = tokio::join!(
            race_seam::AFTER_ROW_READ.scope(
                pause_a,
                upsert_dependency_edge(
                    &runtime_a,
                    &token_a,
                    source_id,
                    target_id,
                    "dev-dependencies",
                    "dev",
                    "rust",
                    update_time,
                    &mut report_a,
                ),
            ),
            race_seam::AFTER_ROW_READ.scope(
                pause_b,
                upsert_dependency_edge(
                    &runtime_b,
                    &token_b,
                    source_id,
                    target_id,
                    "build-dependencies",
                    "build",
                    "rust",
                    update_time,
                    &mut report_b,
                ),
            ),
        );
        result_a.expect("writer A completes");
        result_b.expect("writer B completes");

        let edge = runtime_a
            .graph(&token_a)
            .expect("graph store")
            .get_edge(LinkId::from(edge_uuid(
                EdgeRelation::DependsOn,
                source_id,
                target_id,
            )))
            .await
            .expect("read dependency edge")
            .expect("dependency edge remains");
        let kinds: BTreeSet<String> = edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("dependency_kinds"))
            .and_then(Value::as_array)
            .expect("dependency kinds")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        assert_eq!(
            kinds,
            [
                "build-dependencies".to_string(),
                "dependencies".to_string(),
                "dev-dependencies".to_string(),
            ]
            .into_iter()
            .collect(),
            "two evidence additions derived from one edge revision must both survive"
        );
        assert_eq!(report_a.edges_updated, 1);
        assert_eq!(report_b.edges_updated, 1);
        assert!(edge.updated_at > update_time);
    }

    #[tokio::test]
    async fn concurrent_dependency_evidence_rebases_without_losing_either_kind() {
        for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
            concurrent_dependency_evidence_rebases_without_losing_either_kind_in_mode(mode).await;
        }
    }

    #[tokio::test]
    async fn concurrent_conditional_inserts_preserve_entity_and_edge_winners() {
        for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
            let root = TempDir::new().expect("temporary database directory");
            let db_path = root.path().join(format!("insert-race-{}.db", mode.label()));
            let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
            let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
            let entity_id = project_uuid("insert-race");
            let mut entity_a = Entity::new("local", "project", "first-candidate");
            entity_a.id = entity_id;
            entity_a.properties = Some(json!({"candidate": "a"}));
            let mut entity_b = entity_a.clone();
            entity_b.name = "second-candidate".to_string();
            entity_b.properties = Some(json!({"candidate": "b"}));

            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let entity_store_a = runtime_a.entities(&token_a).expect("entity store A");
            let entity_store_b = runtime_b.entities(&token_b).expect("entity store B");
            let (inserted_a, inserted_b) = tokio::join!(
                async {
                    barrier.wait().await;
                    entity_store_a
                        .insert_entity_if_absent(entity_a.clone())
                        .await
                },
                async {
                    barrier.wait().await;
                    entity_store_b
                        .insert_entity_if_absent(entity_b.clone())
                        .await
                },
            );
            let inserted_a = inserted_a.expect("entity insert A");
            let inserted_b = inserted_b.expect("entity insert B");
            assert_ne!(
                inserted_a,
                inserted_b,
                "exactly one entity insert wins in {} mode",
                mode.label()
            );
            let stored_entity = entity_store_a
                .get_entity(entity_id)
                .await
                .expect("read entity")
                .expect("one entity remains");
            let winner = if inserted_a { &entity_a } else { &entity_b };
            assert_eq!(stored_entity.name, winner.name, "{} mode", mode.label());
            assert_eq!(
                stored_entity.properties,
                winner.properties,
                "{} mode",
                mode.label()
            );

            let source_id = project_uuid("insert-source");
            let target_id = project_uuid("insert-target");
            for (id, name) in [(source_id, "insert-source"), (target_id, "insert-target")] {
                let mut endpoint = Entity::new("local", "project", name);
                endpoint.id = id;
                entity_store_a
                    .upsert_entity(endpoint)
                    .await
                    .expect("seed endpoint");
            }
            let now = Utc::now();
            let edge_a = Edge {
                id: LinkId::from(Uuid::new_v4()),
                namespace: "local".to_string(),
                source_id,
                target_id,
                relation: EdgeRelation::DependsOn,
                weight: 1.0,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: Some(json!({"candidate": "a"})),
                target_backend: None,
            };
            let edge_b = Edge {
                id: LinkId::from(Uuid::new_v4()),
                weight: 0.25,
                metadata: Some(json!({"candidate": "b"})),
                ..edge_a.clone()
            };
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let edge_store_a = runtime_a.graph(&token_a).expect("edge store A");
            let edge_store_b = runtime_b.graph(&token_b).expect("edge store B");
            let (inserted_a, inserted_b) = tokio::join!(
                async {
                    barrier.wait().await;
                    edge_store_a.insert_edge_if_absent(edge_a.clone()).await
                },
                async {
                    barrier.wait().await;
                    edge_store_b.insert_edge_if_absent(edge_b.clone()).await
                },
            );
            let inserted_a = inserted_a.expect("edge insert A");
            let inserted_b = inserted_b.expect("edge insert B");
            assert_ne!(
                inserted_a,
                inserted_b,
                "exactly one natural-key edge insert wins in {} mode",
                mode.label()
            );
            let (winning_edge, losing_edge) = if inserted_a {
                (&edge_a, &edge_b)
            } else {
                (&edge_b, &edge_a)
            };
            let stored_edge = edge_store_a
                .get_edge(winning_edge.id)
                .await
                .expect("read edge")
                .expect("one edge remains");
            assert_eq!(stored_edge.id, winning_edge.id, "{} mode", mode.label());
            assert_eq!(
                stored_edge.weight,
                winning_edge.weight,
                "{} mode",
                mode.label()
            );
            assert_eq!(
                stored_edge.metadata,
                winning_edge.metadata,
                "{} mode",
                mode.label()
            );
            assert!(
                edge_store_a
                    .get_edge(losing_edge.id)
                    .await
                    .expect("read loser")
                    .is_none(),
                "losing edge must be absent in {} mode",
                mode.label()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollback_journal_busy_begin_retries_after_other_runtime_releases_lock() {
        let root = TempDir::new().expect("temporary database directory");
        let db_path = root.path().join("busy-delete.db");
        // The runtimes take different lock directories, so the second one stands
        // in for a writer outside this process's lease and meets the first
        // one's reserved lock at BEGIN, as SQLITE_BUSY.
        let (runtime_a, _) = runtime_on_with_mode(&db_path, TestJournalMode::Delete);
        let (runtime_b, token_b) = runtime_on_with_mode_in(
            &db_path,
            TestJournalMode::Delete,
            root.path().join("other-writer-locks"),
        );
        let pool_b = runtime_b.backend().pool();
        let writer_task = pool_b
            .writer_task_handle()
            .expect("writer task handle")
            .expect("file-backed writer task");
        writer_task
            .send_top_level(|conn| {
                conn.busy_handler(None)
                    .map_err(|error| khive_storage::StorageError::Internal(error.to_string()))
            })
            .await
            .expect("disable SQLite wait on first BEGIN");

        let lock_holder = runtime_a.backend().pool().writer().expect("lock holder");
        lock_holder
            .conn()
            .execute_batch("BEGIN IMMEDIATE")
            .expect("reserve rollback-journal writer lock");
        let id = project_uuid("busy-retry");
        let mut report = CodeSourceIngestReport::default();
        let write = mutate_entity(&runtime_b, &token_b, id, "busy.rs", &mut report, |_| {
            let mut entity = Entity::new("local", "project", "busy-retry");
            entity.id = id;
            Some(entity)
        });
        let release = async {
            let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while pool_b.writer_acquisition_snapshot().writer_task_begin_busy == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await;
            lock_holder
                .conn()
                .execute_batch("ROLLBACK")
                .expect("release rollback-journal writer lock");
            observed.expect("the other runtime must observe a real SQLITE_BUSY refusal");
        };
        let (result, ()) = tokio::join!(write, release);
        assert_eq!(
            result.expect("busy BEGIN retries after release"),
            RowMutationOutcome::Created
        );
        assert_eq!(report.fts_indexed, 1);
        let counters = pool_b.writer_acquisition_snapshot();
        assert!(counters.writer_task_begin_busy >= 1);
        assert!(counters.writer_task_begin_busy_absorbed >= 1);
    }

    fn git_in(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("git command starts");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is UTF-8")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "manual A8 WAL/DELETE wall-clock comparison"]
    async fn measure_concurrent_code_map_ingest_a8_32_commits() {
        const COMMITS: usize = 32;
        const REPO_NAME: &str = "code-map-a8-32";
        let root = TempDir::new().expect("temporary measurement directory");
        let repo = root.path().join(REPO_NAME);
        let src = repo.join("src");
        fs::create_dir_all(&src).expect("source directory");
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"code-map-a8-32\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("stable manifest");
        fs::write(src.join("lib.rs"), "").expect("initial library");
        git_in(&repo, &["init", "-q"]);
        for i in 0..COMMITS {
            fs::write(
                src.join(format!("module_{i:02}.rs")),
                format!("pub fn value_{i:02}() -> usize {{ {i} }}\n"),
            )
            .expect("small module");
            let mut lib = fs::read_to_string(src.join("lib.rs")).expect("read library");
            lib.push_str(&format!("pub mod module_{i:02};\n"));
            fs::write(src.join("lib.rs"), lib).expect("extend library");
            git_in(&repo, &["add", "-A"]);
            let message = format!("add module {i:02}");
            git_in(
                &repo,
                &[
                    "-c",
                    "user.name=A8 Fixture",
                    "-c",
                    "user.email=a8@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    &message,
                ],
            );
        }
        assert_eq!(
            git_in(&repo, &["rev-list", "--count", "HEAD"]).trim(),
            COMMITS.to_string()
        );

        for mode in [TestJournalMode::Wal, TestJournalMode::Delete] {
            let db_path = root.path().join(format!("map-{}.db", mode.label()));
            let (runtime_a, token_a) = runtime_on_with_mode(&db_path, mode);
            let (runtime_b, token_b) = runtime_on_with_mode(&db_path, mode);
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let sweep_time = Utc::now();
            let run_a = async {
                barrier.wait().await;
                run_code_ingest(
                    &runtime_a,
                    &token_a,
                    CodeSourceIngestOptions {
                        path: &repo,
                        languages: ["rust"].into_iter().collect(),
                        sweep_time,
                        enable_l1: true,
                        enable_l1_5: true,
                        enable_l2: false,
                    },
                )
                .await
            };
            let run_b = async {
                barrier.wait().await;
                run_code_ingest(
                    &runtime_b,
                    &token_b,
                    CodeSourceIngestOptions {
                        path: &repo,
                        languages: ["rust"].into_iter().collect(),
                        sweep_time,
                        enable_l1: true,
                        enable_l1_5: true,
                        enable_l2: false,
                    },
                )
                .await
            };
            let started = std::time::Instant::now();
            let (report_a, report_b) = tokio::join!(run_a, run_b);
            let wall = started.elapsed();
            let report_a = report_a.expect("concurrent ingest A");
            let report_b = report_b.expect("concurrent ingest B");
            assert_eq!(report_a.source_revision, report_b.source_revision);
            println!(
                "A8_MEASURE mode={} repo={REPO_NAME} commits={COMMITS} wall_ms={}",
                mode.label(),
                wall.as_millis()
            );
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn issue2673_code_entity_mutation_rebases_persisted_versions() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let mut report = CodeSourceIngestReport::default();
    for version in 1..=3 {
        let name = format!("code revision {version}");
        let outcome = mutate_entity(&runtime, &token, id, "version.rs", &mut report, |_| {
            let mut entity = Entity::new("local", "concept", &name);
            entity.id = id;
            Some(entity)
        })
        .await
        .unwrap();
        assert!(outcome.wrote());
        let stored = runtime.get_entity(&token, id).await.unwrap();
        assert_eq!(stored.version, version);
        assert_eq!(stored.name, name);
    }
    assert_eq!(
        mutate_entity(&runtime, &token, id, "version.rs", &mut report, |_| None)
            .await
            .unwrap(),
        RowMutationOutcome::Unchanged
    );
    assert_eq!(runtime.get_entity(&token, id).await.unwrap().version, 3);
}

#[cfg(test)]
#[tokio::test]
async fn code_entity_mutation_refuses_reserved_candidate_and_carried_properties() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let mut report = CodeSourceIngestReport::default();
    let reserved = json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"});

    let candidate =
        Entity::new("local", "concept", "reserved candidate").with_properties(reserved.clone());
    let candidate_id = candidate.id;
    let error = mutate_entity(
        &runtime,
        &token,
        candidate_id,
        "reserved.rs",
        &mut report,
        |_| Some(candidate.clone()),
    )
    .await
    .expect_err("a reserved candidate must not be inserted");
    assert!(
        matches!(error, CodeSourceIngestError::Runtime(RuntimeError::InvalidInput(ref message)) if message.contains("khive:secret_gate")),
        "unexpected error: {error:?}"
    );
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(candidate_id)
        .await
        .unwrap()
        .is_none());

    let current = Entity::new("local", "concept", "original").with_properties(reserved);
    let current_id = current.id;
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(current)
        .await
        .unwrap();
    let before = runtime
        .entities(&token)
        .unwrap()
        .get_entity(current_id)
        .await
        .unwrap()
        .unwrap();
    let error = mutate_entity(
        &runtime,
        &token,
        current_id,
        "reserved.rs",
        &mut report,
        |current| {
            let mut replacement = current.cloned().expect("seeded row");
            replacement.name = "changed".into();
            Some(replacement)
        },
    )
    .await
    .expect_err("a carried reserved key must not be replaced");
    assert!(
        matches!(error, CodeSourceIngestError::Runtime(RuntimeError::InvalidInput(ref message)) if message.contains("khive:secret_gate")),
        "unexpected error: {error:?}"
    );
    let after = runtime
        .entities(&token)
        .unwrap()
        .get_entity(current_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}
