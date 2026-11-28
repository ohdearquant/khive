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
#[cfg(test)]
use khive_runtime::entity_fts_document;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
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

mod direct_entity;
#[cfg(test)]
use direct_entity::gate_check;
use direct_entity::{
    advancing_entity_revision, candidate_context, gate_check_with_context, mutate_entity,
    mutate_entity_with_context,
};

mod l2;
use l2::*;

mod file_pending;
use file_pending::{stamp_l2_declarations, FileReference};
mod source_reads;
pub use source_reads::CodeSourceReadReport;
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
#[path = "source_ingest/race_seam_tests.rs"]
mod race_seam;

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
    /// Literal source-input observations from L1.5. These are lexical
    /// evidence, not a complete read graph or Cargo target classification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reads: Option<CodeSourceReadReport>,
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
    source_reads: Vec<source_reads::SourceReadInput>,
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
        "build-dependencies" | "import" | "source_read" => "build",
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
    let context = candidate_context(token.namespace().as_str());
    if let Err(error) = gate_check_with_context(&context, &current) {
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
        match context.check_properties(Some(&candidate)) {
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
    let outcome = mutate_entity_with_context(
        rt,
        token,
        &context,
        entity_id,
        &first.file,
        report,
        |current| {
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
        },
    )
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
/// Source-reader observations share the natural edge with imports. Retain
/// their revision-qualified evidence when another producer refreshes it.
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
    let mut metadata = json!({
        "dependency_kinds": kinds.into_iter().collect::<Vec<_>>(),
        "dependency_scopes": scopes.into_iter().collect::<Vec<_>>(),
        "language": language,
    });
    if let Some(observations) = existing_metadata.and_then(|m| m.get("source_read_observations")) {
        metadata["source_read_observations"] = observations.clone();
    }
    metadata
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
    source_read_plans: &BTreeMap<Uuid, source_reads::SourceReadPlan>,
    report: &mut CodeSourceIngestReport,
) -> Result<BTreeSet<Uuid>, CodeSourceIngestError> {
    let mut source_reads_stamped = BTreeSet::new();
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
        let mut source_read_mismatch = false;
        let outcome = mutate_entity(rt, token, module_id, &source_label, report, |current| {
            row_missing = current.is_none();
            source_read_mismatch = false;
            let mut module = current?.clone();
            let Some(Value::Object(mut props)) = module.properties.clone() else {
                invalid_properties = true;
                return None;
            };
            if let Some(plan) = source_read_plans.get(&module_id) {
                if !plan.matches(&props) {
                    source_read_mismatch = true;
                    return None;
                }
                props.insert("source_read_scan".into(), plan.observation.clone());
            }
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
        if outcome.wrote() && source_read_plans.contains_key(&module_id) {
            source_reads_stamped.insert(module_id);
        }
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
        } else if source_read_mismatch {
            report.warnings.push(format!(
                "module {module_id} changed since this invocation's source scan; coverage stamps skipped"
            ));
            report.coverage_stamps_missed += 1;
        } else if outcome == RowMutationOutcome::Blocked {
            report.coverage_stamps_missed += 1;
        }
    }
    Ok(source_reads_stamped)
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
) -> Result<(String, Vec<String>, crate::source_reads::SourceReadScan), SourceReadError> {
    let content = safe_source::read_contained_to_string(canonical_root, path)?;
    Ok((
        content_hash(&content),
        imports::extract_raw_imports(language, &content),
        source_reads::extract_on_worker(language, content),
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
        let plans = source_reads::prepare(&module_scans);
        let stamped =
            stamp_import_scan_coverage(rt, token, module_scans, &plans, &mut report).await?;
        source_reads::persist_edges(rt, token, &plans, &stamped, opts.sweep_time, &mut report)
            .await?;
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
        let (hash, raw_imports, source_read_scan) =
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
            source_reads: Vec::new(),
        });
        scan.imports.extend(scan_imports);
        scan.source_reads.push(source_reads::SourceReadInput {
            source_path,
            source_revision: snapshot.revision.clone(),
            content_hash: hash,
            language: language.to_string(),
            sweep_time: sweep_time.to_rfc3339(),
            scan: source_read_scan,
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "source_ingest/owner_alias_tests.rs"]
mod owner_alias_tests;

#[cfg(test)]
mod direct_entity_tests;

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
#[path = "source_ingest/source_ingest_tests.rs"]
mod tests;

#[cfg(test)]
include!("source_ingest/entity_mutation_tests.rs");

#[cfg(test)]
#[path = "source_ingest/workspace_parity_tests.rs"]
mod workspace_parity_tests;
