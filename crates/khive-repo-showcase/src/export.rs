use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use schemars::schema_for;
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::aggregate::{build_aggregates, AggregateInput};
use crate::join::{
    changed_paths_fallback, derive_rust_module_keys, head_committed_at, head_sha, natural_id,
    release_tags, tracked_paths,
};
use crate::read::{read_history, read_map, HistoryData, MapData};
use crate::*;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("database does not exist: {0}")]
    MissingDatabase(PathBuf),
    #[error("opening or reading SQLite database {path}: {source}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("record {record} in {path} has invalid JSON properties: {source}")]
    InvalidJson {
        path: PathBuf,
        record: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("history project {repo_slug:?} not found in {path}")]
    HistoryProjectNotFound { repo_slug: String, path: PathBuf },
    #[error("repository identity {repo_slug:?} matched {count} history projects in {path}")]
    AmbiguousHistoryProject {
        repo_slug: String,
        count: usize,
        path: PathBuf,
    },
    #[error("spawning git {args}: {source}")]
    GitSpawn {
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing Cargo manifest {path}: {source}")]
    CargoManifest {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid repository data: {0}")]
    InvalidData(String),
    #[error("serializing khive.repo.v1: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("persisting canonical bundle to {path}: {source}")]
    Persist {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ExportRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo_path: PathBuf,
        history_db: PathBuf,
        map_db: PathBuf,
        generated_at: String,
        repository_url: String,
        provenance: PipelineProvenance,
    ) -> Self {
        Self {
            repo_path,
            history_db,
            map_db,
            generated_at,
            repository_url,
            bounds: ExportBounds::default(),
            provenance,
            default_branch: Availability::unavailable(
                "default branch metadata was not explicitly supplied",
            ),
        }
    }

    pub fn with_bounds(mut self, bounds: ExportBounds) -> Self {
        self.bounds = bounds;
        self
    }

    pub fn with_default_branch(mut self, default_branch: Availability<String>) -> Self {
        self.default_branch = default_branch;
        self
    }
}

#[derive(Debug)]
pub(crate) struct CommitWork {
    pub(crate) node: CommitNode,
    pub(crate) paths: Vec<(String, DerivationSource)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DerivationSource {
    Digest,
    CloneFallback,
}

#[derive(Debug)]
struct PreparedGraph {
    graph: RepoGraph,
    commits: Vec<CommitWork>,
    full_modules: Vec<ModuleNode>,
    full_structure_edges: Vec<GraphEdge>,
    full_commit_module_edges: Vec<GraphEdge>,
    full_issues: Vec<IssueNode>,
    full_pull_requests: Vec<PullRequestNode>,
    tags: Page<ReleaseTag>,
}

pub fn export(request: &ExportRequest) -> Result<RepoBundle, ExportError> {
    validate_bounds(&request.bounds)?;
    let repository =
        parse_repository_identity(&request.repository_url, request.default_branch.clone())?;
    let head = head_sha(&request.repo_path)?;
    let generated_at = normalize_generated_at(&request.generated_at, &request.repo_path)?;
    validate_provenance(request, &head)?;
    let repo_slug = format!(
        "{}/{}/{}",
        repository.host, repository.owner, repository.name
    );
    let history = read_history(&request.history_db, &repo_slug, &repository.canonical_url)?;
    let map = read_map(&request.map_db, &head)?;
    let prepared = prepare_graph(request, &repository, &head, history, map)?;
    let aggregates = build_aggregates(AggregateInput {
        generated_at: &generated_at,
        graph: &prepared.graph,
        commits: &prepared.commits,
        modules: &prepared.full_modules,
        structure_edges: &prepared.full_structure_edges,
        commit_module_edges: &prepared.full_commit_module_edges,
        issues: &prepared.full_issues,
        pull_requests: &prepared.full_pull_requests,
        release_tags: prepared.tags,
        bounds: &request.bounds,
        provenance: &request.provenance,
    })?;
    let capability = capability(&prepared.graph, &aggregates);
    Ok(RepoBundle {
        schema_version: SchemaVersion::KhiveRepoV1,
        meta: BundleMeta {
            repository,
            snapshot: SnapshotIdentity {
                head_sha: head,
                ingested_at: Timestamp::parse(generated_at)
                    .expect("normalized generated_at is RFC3339"),
            },
            producer: ProducerIdentity {
                exporter: "khive-repo-showcase".into(),
                kkernel_version: env!("CARGO_PKG_VERSION").into(),
                khive_pack_git_version: env!("CARGO_PKG_VERSION").into(),
                khive_pack_code_version: env!("CARGO_PKG_VERSION").into(),
            },
            ingest: request.provenance.clone(),
        },
        graph: prepared.graph,
        aggregates,
        capability,
    })
}

fn validate_bounds(bounds: &ExportBounds) -> Result<(), ExportError> {
    let limits = [
        ("packages", bounds.packages, 2_048),
        ("modules", bounds.modules, 10_000),
        ("commits", bounds.commits, 5_000),
        ("issues", bounds.issues, 2_000),
        ("pull_requests", bounds.pull_requests, 2_000),
        ("structure_edges", bounds.structure_edges, 50_000),
        ("history_edges", bounds.history_edges, 50_000),
        ("commit_module_edges", bounds.commit_module_edges, 50_000),
        ("residuals", bounds.residuals, 5_000),
        ("aggregate_rows", bounds.aggregate_rows, 5_000),
        ("navigation_entities", bounds.navigation_entities, 10_000),
        ("navigation_per_entity", bounds.navigation_per_entity, 50),
        ("authors_per_scope", bounds.authors_per_scope, 100),
    ];
    if let Some((name, value, maximum)) = limits
        .into_iter()
        .find(|(_, value, maximum)| value > maximum)
    {
        return Err(ExportError::InvalidData(format!(
            "export bound {name}={value} exceeds the khive.repo.v1 maximum {maximum}"
        )));
    }
    Ok(())
}

pub fn export_canonical_bytes(request: &ExportRequest) -> Result<Vec<u8>, ExportError> {
    canonical_bytes(&export(request)?)
}

pub fn canonical_bytes(bundle: &RepoBundle) -> Result<Vec<u8>, ExportError> {
    let mut bytes = serde_json::to_vec(bundle)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn write_canonical_atomic(bundle: &RepoBundle, output: &Path) -> Result<(), ExportError> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ExportError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| ExportError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(&canonical_bytes(bundle)?)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| ExportError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(output)
        .map_err(|error| ExportError::Persist {
            path: output.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

pub fn json_schema() -> Value {
    serde_json::to_value(schema_for!(RepoBundle)).expect("JSON Schema always serializes")
}

fn normalize_generated_at(value: &str, repo: &Path) -> Result<String, ExportError> {
    let generated = DateTime::parse_from_rfc3339(value)
        .map_err(|error| {
            ExportError::InvalidData(format!("generated_at must be RFC3339: {error}"))
        })?
        .with_timezone(&Utc);
    let committed = DateTime::parse_from_rfc3339(&head_committed_at(repo)?)
        .map_err(|error| ExportError::InvalidData(format!("HEAD commit time is invalid: {error}")))?
        .with_timezone(&Utc);
    if generated < committed {
        return Err(ExportError::InvalidData(format!(
            "generated_at {} precedes HEAD commit time {}",
            generated.to_rfc3339(),
            committed.to_rfc3339()
        )));
    }
    Ok(generated.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn parse_repository_identity(
    url: &str,
    default_branch: Availability<String>,
) -> Result<RepositoryIdentity, ExportError> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let without_scheme = trimmed.strip_prefix("https://").ok_or_else(|| {
        ExportError::InvalidData("repository_url must be a public HTTPS URL".into())
    })?;
    let mut segments = without_scheme.split('/');
    let host = segments.next().unwrap_or_default();
    let owner = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    if host.is_empty() || owner.is_empty() || name.is_empty() || segments.next().is_some() {
        return Err(ExportError::InvalidData(format!(
            "repository_url must have host/owner/name shape, got {url:?}"
        )));
    }
    if let Availability::Available { value } = &default_branch {
        if value.trim().is_empty() || value != value.trim() {
            return Err(ExportError::InvalidData(
                "explicit default branch must be a non-empty, whitespace-trimmed name".into(),
            ));
        }
    }
    Ok(RepositoryIdentity {
        host: host.to_ascii_lowercase(),
        owner: owner.to_string(),
        name: name.to_string(),
        canonical_url: format!("https://{host}/{owner}/{name}"),
        default_branch,
    })
}

fn validate_provenance(request: &ExportRequest, head: &str) -> Result<(), ExportError> {
    if let Availability::Available { value } = &request.provenance.code_ingest {
        if value.source_revision != head {
            return Err(ExportError::InvalidData(format!(
                "code.ingest source_revision {} does not equal HEAD {head}",
                value.source_revision
            )));
        }
    }
    if let Availability::Available { value } = &request.provenance.git_digest {
        if value.sources.commits.completed() {
            if value.cursor_stalled {
                return Err(ExportError::InvalidData(
                    "completed commit coverage cannot have cursor_stalled=true".into(),
                ));
            }
            if value.writes_refused > 0 {
                return Err(ExportError::InvalidData(format!(
                    "completed commit coverage is incomplete: {} git.digest write(s) were refused",
                    value.writes_refused
                )));
            }
            if value.changed_paths_filtered_noncanonical > 0 {
                return Err(ExportError::InvalidData(format!(
                    "commit-module join is incomplete: {} noncanonical changed path(s) were filtered",
                    value.changed_paths_filtered_noncanonical
                )));
            }
        }
    }
    Ok(())
}

fn prop_str(properties: &Value, field: &str, record: &str) -> Result<String, ExportError> {
    properties
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ExportError::InvalidData(format!("{record} requires string {field}")))
}

fn prop_optional_string(properties: &Value, field: &str, reason: &str) -> Availability<String> {
    properties
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| Availability::available(value.to_string()))
        .unwrap_or_else(|| Availability::unavailable(reason))
}

fn prop_u64(properties: &Value, field: &str, record: &str) -> Result<u64, ExportError> {
    properties
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| ExportError::InvalidData(format!("{record} requires integer {field}")))
}

fn normalize_record_time(value: &str, record: &str, field: &str) -> Result<Timestamp, ExportError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|error| {
            ExportError::InvalidData(format!("{record} has invalid RFC3339 {field}: {error}"))
        })
        .map(|value| {
            Timestamp::parse(
                value
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            )
            .expect("chrono produced RFC3339")
        })
}

fn prop_optional_timestamp(
    properties: &Value,
    field: &str,
    record: &str,
    reason: &str,
) -> Result<Availability<Timestamp>, ExportError> {
    let value = match properties.get(field) {
        None | Some(Value::Null) => return Ok(Availability::unavailable(reason)),
        Some(Value::String(value)) if value.is_empty() => {
            return Ok(Availability::unavailable(reason));
        }
        Some(Value::String(value)) => value,
        Some(_) => {
            return Err(ExportError::InvalidData(format!(
                "{record} {field} must be an RFC3339 string or null"
            )));
        }
    };
    normalize_record_time(value, record, field).map(Availability::available)
}

fn prop_string_array(properties: &Value, field: &str) -> Result<Option<Vec<String>>, ExportError> {
    let Some(value) = properties.get(field) else {
        return Ok(None);
    };
    let values = value.as_array().ok_or_else(|| {
        ExportError::InvalidData(format!("property {field} must be an array when present"))
    })?;
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        let value = value.as_str().ok_or_else(|| {
            ExportError::InvalidData(format!("property {field} entries must be strings"))
        })?;
        output.push(value.to_string());
    }
    output.sort();
    output.dedup();
    Ok(Some(output))
}

fn prop_ordered_string_array(
    properties: &Value,
    field: &str,
) -> Result<Option<Vec<String>>, ExportError> {
    let Some(value) = properties.get(field) else {
        return Ok(None);
    };
    let values = value.as_array().ok_or_else(|| {
        ExportError::InvalidData(format!("property {field} must be an array when present"))
    })?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                ExportError::InvalidData(format!("property {field} entries must be strings"))
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn validate_edge_weight(edge_id: &str, weight: f64) -> Result<(), ExportError> {
    if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return Err(ExportError::InvalidData(format!(
            "edge {edge_id} has weight {weight}; khive.repo.v1 requires a finite value in [0,1]"
        )));
    }
    Ok(())
}

fn validate_full_sha(value: &str, record: &str, field: &str) -> Result<(), ExportError> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExportError::InvalidData(format!(
            "{record} {field} must be a 40-character lowercase hexadecimal commit id, got {value:?}"
        )));
    }
    Ok(())
}

fn validate_short_sha(short: &str, sha: &str, record: &str) -> Result<(), ExportError> {
    if !(7..=40).contains(&short.len())
        || !short
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !sha.starts_with(short)
    {
        return Err(ExportError::InvalidData(format!(
            "{record} short_sha {short:?} must be a 7-40 character lowercase prefix of sha {sha}"
        )));
    }
    Ok(())
}

pub(crate) fn bounded_page<T>(mut items: Vec<T>, limit: u32, order: impl Into<String>) -> Page<T> {
    let total = items.len() as u64;
    let truncated = total > u64::from(limit);
    items.truncate(limit as usize);
    Page {
        items,
        total_count: Availability::available(total),
        bound: PageBound {
            kind: if truncated {
                BoundKind::TopN
            } else {
                BoundKind::All
            },
            max_items: limit,
            order: order.into(),
        },
        next_cursor: truncated.then(|| format!("offset:{limit}")),
        truncated,
        disclosure: Disclosure {
            status: if truncated {
                DisclosureStatus::Truncated
            } else {
                DisclosureStatus::Complete
            },
            reason: truncated.then(|| format!("section limited to {limit} items")),
        },
    }
}

fn covered_page<T>(
    items: Vec<T>,
    limit: u32,
    order: impl Into<String>,
    coverage: &SourceCoverage,
    label: &str,
) -> Page<T> {
    if coverage.completed() {
        bounded_page(items, limit, order)
    } else {
        Page::unavailable(
            limit,
            order,
            coverage
                .unavailable_reason(label)
                .expect("non-completed coverage has a reason"),
        )
    }
}

fn covered_page_reason<T>(
    items: Vec<T>,
    limit: u32,
    order: impl Into<String>,
    unavailable_reason: Option<&str>,
) -> Page<T> {
    let order = order.into();
    match unavailable_reason {
        Some(reason) => Page::unavailable(limit, order, reason),
        None => bounded_page(items, limit, order),
    }
}

fn source_coverage(
    provenance: &PipelineProvenance,
    pick: impl FnOnce(&HistorySourceCoverage) -> &SourceCoverage,
    label: &str,
) -> SourceCoverage {
    match &provenance.git_digest {
        Availability::Available { value } => pick(&value.sources).clone(),
        Availability::Unavailable { reason } => SourceCoverage::Unknown {
            reason: format!("{label}: {reason}"),
        },
    }
}

fn prepare_graph(
    request: &ExportRequest,
    repository: &RepositoryIdentity,
    head: &str,
    history: HistoryData,
    map: MapData,
) -> Result<PreparedGraph, ExportError> {
    let repository_id = natural_id("repository", &[&repository.canonical_url]);

    let mut module_rows = Vec::new();
    for raw in &map.modules {
        debug_assert_eq!(raw.kind, "concept");
        debug_assert_eq!(raw.entity_type.as_deref(), Some("module"));
        let source_project = prop_str(&raw.properties, "source_project", &raw.id)?;
        let language = prop_str(&raw.properties, "language", &raw.id)?;
        let module_path = prop_str(&raw.properties, "module_path", &raw.id)?;
        let source_path = prop_str(&raw.properties, "source_path", &raw.id)?;
        let source_revision = prop_str(&raw.properties, "source_revision", &raw.id)?;
        let content_hash = prop_str(&raw.properties, "content_hash", &raw.id)?;
        let import_scan_status = prop_str(&raw.properties, "import_scan_status", &raw.id)?;
        let package_id = natural_id("package", &[&repository.canonical_url, &source_project]);
        let id = natural_id(
            "module",
            &[
                &repository.canonical_url,
                &source_project,
                &language,
                &module_path,
            ],
        );
        module_rows.push((
            raw.id.clone(),
            source_project,
            ModuleNode {
                id,
                package_id,
                name: raw.name.clone(),
                language,
                module_path,
                source_path,
                source_revision,
                content_hash,
                import_scan_status,
            },
        ));
    }
    module_rows.sort_by(|left, right| left.2.id.cmp(&right.2.id));
    let mut seen_source_paths = BTreeMap::<&str, &str>::new();
    let mut seen_module_keys = BTreeMap::<(&str, &str, &str), &str>::new();
    for (raw_id, source_project, module) in &module_rows {
        if let Some(existing) = seen_source_paths.insert(&module.source_path, raw_id) {
            return Err(ExportError::InvalidData(format!(
                "code map rows {existing} and {raw_id} duplicate source_path {:?}",
                module.source_path
            )));
        }
        let key = (
            source_project.as_str(),
            module.language.as_str(),
            module.module_path.as_str(),
        );
        if let Some(existing) = seen_module_keys.insert(key, raw_id) {
            return Err(ExportError::InvalidData(format!(
                "code map rows {existing} and {raw_id} duplicate module key ({source_project:?}, {:?}, {:?})",
                module.language, module.module_path
            )));
        }
    }

    let mut package_languages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, source_project, module) in &module_rows {
        package_languages
            .entry(source_project.clone())
            .or_default()
            .insert(module.language.clone());
    }
    let mut packages = Vec::new();
    let mut raw_to_wire = BTreeMap::new();
    let mut source_project_to_id = BTreeMap::new();
    for raw in &map.projects {
        debug_assert_eq!(raw.kind, "project");
        let source_project = raw
            .properties
            .get("source_project")
            .and_then(Value::as_str)
            .unwrap_or(&raw.name)
            .to_string();
        let id = natural_id("package", &[&repository.canonical_url, &source_project]);
        if source_project_to_id
            .insert(source_project.clone(), id.clone())
            .is_some()
        {
            return Err(ExportError::InvalidData(format!(
                "duplicate code-map project identity {source_project:?}"
            )));
        }
        raw_to_wire.insert(raw.id.clone(), id.clone());
        packages.push(PackageNode {
            id,
            name: source_project.clone(),
            languages: package_languages
                .get(&source_project)
                .map(|values| values.iter().cloned().collect())
                .unwrap_or_default(),
        });
    }
    packages.sort_by(|left, right| left.id.cmp(&right.id));
    for (raw_id, source_project, module) in &module_rows {
        if !source_project_to_id.contains_key(source_project) {
            return Err(ExportError::InvalidData(format!(
                "module {} names missing source_project {source_project:?}",
                module.source_path
            )));
        }
        raw_to_wire.insert(raw_id.clone(), module.id.clone());
    }
    let modules = module_rows
        .iter()
        .map(|(_, _, module)| module.clone())
        .collect::<Vec<_>>();

    let mut structure_edges = packages
        .iter()
        .map(|package| GraphEdge {
            id: natural_id(
                "edge",
                &[&repository_id, "contains", &package.id, "derived"],
            ),
            source: repository_id.clone(),
            target: package.id.clone(),
            relation: "contains".into(),
            weight: 1.0,
            origin: EdgeOrigin::Derived,
            derivation: Some(EdgeDerivation::RepositoryPackageNormalization {
                source_project: package.name.clone(),
            }),
        })
        .collect::<Vec<_>>();
    for edge in &map.edges {
        validate_edge_weight(&edge.id, edge.weight)?;
        let Some(source) = raw_to_wire.get(&edge.source_id) else {
            continue;
        };
        let Some(target) = raw_to_wire.get(&edge.target_id) else {
            continue;
        };
        structure_edges.push(GraphEdge {
            id: natural_id("edge", &[source, &edge.relation, target, "ingested"]),
            source: source.clone(),
            target: target.clone(),
            relation: edge.relation.clone(),
            weight: edge.weight,
            origin: EdgeOrigin::Ingested,
            derivation: None,
        });
    }
    structure_edges.sort_by(|left, right| left.id.cmp(&right.id));
    structure_edges.dedup_by(|left, right| left.id == right.id);

    let commit_coverage = source_coverage(
        &request.provenance,
        |sources| &sources.commits,
        "commit history",
    );
    let issue_coverage = source_coverage(
        &request.provenance,
        |sources| &sources.issues,
        "issue history",
    );
    let pull_request_coverage = source_coverage(
        &request.provenance,
        |sources| &sources.pull_requests,
        "pull-request history",
    );
    let code_ingest_reason = match &request.provenance.code_ingest {
        Availability::Available { value }
            if value.blocked_count > 0
                || value.files_dropped_without_source_path > 0
                || value.coverage_stamps_missed > 0 =>
        {
            Some(format!(
                "code structure coverage is incomplete: blocked={}, dropped_without_source_path={}, coverage_stamps_missed={}",
                value.blocked_count,
                value.files_dropped_without_source_path,
                value.coverage_stamps_missed
            ))
        }
        Availability::Available { .. } => None,
        Availability::Unavailable { reason } => Some(format!(
            "code structure coverage is unavailable: {reason}"
        )),
    };
    let join_unavailable_reason = commit_coverage
        .unavailable_reason("commit-to-module join")
        .or_else(|| code_ingest_reason.clone());
    let history_edges_unavailable_reason = [
        (&commit_coverage, "commit history edges"),
        (&issue_coverage, "issue history edges"),
        (&pull_request_coverage, "pull-request history edges"),
    ]
    .into_iter()
    .find_map(|(coverage, label)| coverage.unavailable_reason(label));

    let mut commits = Vec::new();
    let mut issues = Vec::new();
    let mut pull_requests = Vec::new();
    let mut history_raw_to_wire = BTreeMap::new();
    let mut seen_commit_shas = BTreeSet::new();
    let mut seen_issue_numbers = BTreeSet::new();
    let mut seen_pull_request_numbers = BTreeSet::new();
    if commit_coverage.completed() {
        for note in history.notes.iter().filter(|note| note.kind == "commit") {
            let sha = prop_str(&note.properties, "sha", &note.id)?;
            validate_full_sha(&sha, &note.id, "sha")?;
            if !seen_commit_shas.insert(sha.clone()) {
                return Err(ExportError::InvalidData(format!(
                    "duplicate commit SHA {sha} in history project"
                )));
            }
            let short_sha = note
                .properties
                .get("short_sha")
                .and_then(Value::as_str)
                .unwrap_or_else(|| sha.get(..7).expect("validated SHA has seven characters"))
                .to_string();
            validate_short_sha(&short_sha, &sha, &note.id)?;
            let parents =
                prop_ordered_string_array(&note.properties, "parents")?.unwrap_or_default();
            for parent in &parents {
                validate_full_sha(parent, &note.id, "parent SHA")?;
            }
            let committed_at = normalize_record_time(
                &prop_str(&note.properties, "committed_at", &note.id)?,
                &note.id,
                "committed_at",
            )?;
            let paths = match prop_string_array(&note.properties, "changed_paths")? {
                Some(paths) => paths
                    .into_iter()
                    .map(|path| (path, DerivationSource::Digest))
                    .collect(),
                None => changed_paths_fallback(&request.repo_path, &sha)?
                    .into_iter()
                    .map(|path| (path, DerivationSource::CloneFallback))
                    .collect(),
            };
            let id = natural_id("commit", &[&repository.canonical_url, &sha]);
            history_raw_to_wire.insert(note.id.clone(), id.clone());
            let subject = note
                .content
                .lines()
                .next()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(&note.name)
                .to_string();
            commits.push(CommitWork {
                node: CommitNode {
                    id,
                    sha: sha.clone(),
                    short_sha,
                    author: prop_str(&note.properties, "author", &note.id)?,
                    committed_at,
                    parents,
                    subject,
                },
                paths,
            });
        }
        commits.sort_by(|left, right| {
            left.node
                .committed_at
                .cmp(&right.node.committed_at)
                .then_with(|| left.node.sha.cmp(&right.node.sha))
        });
    }

    if issue_coverage.completed() {
        for note in history.notes.iter().filter(|note| note.kind == "issue") {
            let number = prop_u64(&note.properties, "number", &note.id)?;
            if !seen_issue_numbers.insert(number) {
                return Err(ExportError::InvalidData(format!(
                    "duplicate issue number #{number} in history project"
                )));
            }
            let id = natural_id("issue", &[&repository.canonical_url, &number.to_string()]);
            history_raw_to_wire.insert(note.id.clone(), id.clone());
            issues.push(IssueNode {
                id,
                number,
                title: note
                    .properties
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(&note.name)
                    .to_string(),
                author: prop_optional_string(
                    &note.properties,
                    "author",
                    "issue author unavailable",
                ),
                created_at: prop_optional_timestamp(
                    &note.properties,
                    "created_at",
                    &note.id,
                    "issue creation time unavailable",
                )?,
                closed_at: prop_optional_timestamp(
                    &note.properties,
                    "closed_at",
                    &note.id,
                    "issue is open",
                )?,
                labels: prop_string_array(&note.properties, "labels")?.unwrap_or_default(),
            });
        }
        issues.sort_by_key(|issue| Reverse(issue.number));
    }

    if pull_request_coverage.completed() {
        for note in history
            .notes
            .iter()
            .filter(|note| note.kind == "pull_request")
        {
            let number = prop_u64(&note.properties, "number", &note.id)?;
            if !seen_pull_request_numbers.insert(number) {
                return Err(ExportError::InvalidData(format!(
                    "duplicate pull-request number #{number} in history project"
                )));
            }
            let id = natural_id(
                "pull_request",
                &[&repository.canonical_url, &number.to_string()],
            );
            history_raw_to_wire.insert(note.id.clone(), id.clone());
            pull_requests.push(PullRequestNode {
                id,
                number,
                title: note
                    .properties
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(&note.name)
                    .to_string(),
                author: prop_optional_string(
                    &note.properties,
                    "author",
                    "pull-request author unavailable",
                ),
                created_at: prop_optional_timestamp(
                    &note.properties,
                    "created_at",
                    &note.id,
                    "pull-request creation time unavailable",
                )?,
                merged_at: prop_optional_timestamp(
                    &note.properties,
                    "merged_at",
                    &note.id,
                    "pull request is unmerged",
                )?,
                closed_at: prop_optional_timestamp(
                    &note.properties,
                    "closed_at",
                    &note.id,
                    "pull request is open",
                )?,
                base_ref: prop_optional_string(
                    &note.properties,
                    "base_ref",
                    "pull-request base ref unavailable",
                ),
                head_ref: prop_optional_string(
                    &note.properties,
                    "head_ref",
                    "pull-request head ref unavailable",
                ),
            });
        }
        pull_requests.sort_by_key(|pull_request| Reverse(pull_request.number));
    }

    let mut history_edges = Vec::new();
    for edge in &history.edges {
        validate_edge_weight(&edge.id, edge.weight)?;
        let Some(source) = history_raw_to_wire.get(&edge.source_id) else {
            continue;
        };
        let target = if edge.target_id == history.project_id {
            Some(repository_id.clone())
        } else {
            history_raw_to_wire.get(&edge.target_id).cloned()
        };
        let Some(target) = target else {
            continue;
        };
        history_edges.push(GraphEdge {
            id: natural_id("edge", &[source, &edge.relation, &target, "ingested"]),
            source: source.clone(),
            target,
            relation: edge.relation.clone(),
            weight: edge.weight,
            origin: EdgeOrigin::Ingested,
            derivation: None,
        });
    }
    history_edges.sort_by(|left, right| left.id.cmp(&right.id));
    history_edges.dedup_by(|left, right| left.id == right.id);

    let mut modules_by_path: BTreeMap<String, Vec<&ModuleNode>> = BTreeMap::new();
    for module in modules.iter().filter(|module| module.language == "rust") {
        modules_by_path
            .entry(module.source_path.clone())
            .or_default()
            .push(module);
    }
    let mut commit_module_edges = Vec::new();
    let mut historical_residuals = Vec::new();
    let mut total_changed_paths = 0_u64;
    let mut rust_in_scope_paths = 0_u64;
    let mut matched_rust_paths = 0_u64;
    let mut out_of_scope_paths = 0_u64;
    for commit in &commits {
        for (path, source) in &commit.paths {
            total_changed_paths += 1;
            if !path.ends_with(".rs") {
                out_of_scope_paths += 1;
                continue;
            }
            rust_in_scope_paths += 1;
            match modules_by_path.get(path).map(Vec::as_slice) {
                Some([module]) => {
                    matched_rust_paths += 1;
                    commit_module_edges.push(GraphEdge {
                        id: natural_id(
                            "edge",
                            &[&commit.node.id, "annotates", &module.id, "derived"],
                        ),
                        source: commit.node.id.clone(),
                        target: module.id.clone(),
                        relation: "annotates".into(),
                        weight: 1.0,
                        origin: EdgeOrigin::Derived,
                        derivation: Some(match source {
                            DerivationSource::Digest => {
                                EdgeDerivation::ChangedPathSourcePathExact {
                                    source_revision: head.to_string(),
                                    source_path: path.clone(),
                                }
                            }
                            DerivationSource::CloneFallback => EdgeDerivation::ClonePathFallback {
                                source_revision: head.to_string(),
                                source_path: path.clone(),
                            },
                        }),
                    });
                }
                Some(_) => historical_residuals.push(HistoricalPathResidual {
                    commit_sha: commit.node.sha.clone(),
                    source_path: path.clone(),
                    reason: "multiple current-snapshot modules claim this source_path".into(),
                }),
                None => historical_residuals.push(HistoricalPathResidual {
                    commit_sha: commit.node.sha.clone(),
                    source_path: path.clone(),
                    reason: "no current-snapshot module has this source_path (deleted, renamed, or unscanned)"
                        .into(),
                }),
            }
        }
    }
    commit_module_edges.sort_by(|left, right| left.id.cmp(&right.id));
    commit_module_edges.dedup_by(|left, right| left.id == right.id);
    historical_residuals.sort_by(|left, right| {
        left.commit_sha
            .cmp(&right.commit_sha)
            .then_with(|| left.source_path.cmp(&right.source_path))
    });

    let tracked = tracked_paths(&request.repo_path)?;
    let derived = derive_rust_module_keys(&request.repo_path, &tracked)?;
    let files = derived.len() as u64;
    let mut derived_keys = BTreeSet::new();
    let mut current_residuals = Vec::new();
    for (source_path, key, reason) in &derived {
        match key {
            Some(key) => {
                derived_keys.insert((key.source_project.clone(), key.module_path.clone()));
            }
            None => current_residuals.push(JoinResidual {
                side: ResidualSide::Path,
                source_project: String::new(),
                module_path: String::new(),
                source_path: source_path.clone(),
                reason: reason.clone(),
            }),
        }
    }
    let mut entity_keys = BTreeSet::new();
    let mut entity_by_key = BTreeMap::new();
    for (_, source_project, module) in module_rows
        .iter()
        .filter(|(_, _, module)| module.language == "rust")
    {
        let key = (source_project.clone(), module.module_path.clone());
        entity_keys.insert(key.clone());
        entity_by_key.insert(key, module);
    }
    for (source_project, module_path) in derived_keys.difference(&entity_keys) {
        let source_path = derived
            .iter()
            .find_map(|(path, key, _)| {
                key.as_ref()
                    .filter(|key| {
                        &key.source_project == source_project && &key.module_path == module_path
                    })
                    .map(|_| path.clone())
            })
            .unwrap_or_default();
        current_residuals.push(JoinResidual {
            side: ResidualSide::Path,
            source_project: source_project.clone(),
            module_path: module_path.clone(),
            source_path,
            reason: "derived Rust module key has no current code-map entity".into(),
        });
    }
    for key in entity_keys.difference(&derived_keys) {
        let module = entity_by_key.get(key).expect("entity key was indexed");
        current_residuals.push(JoinResidual {
            side: ResidualSide::Entity,
            source_project: key.0.clone(),
            module_path: key.1.clone(),
            source_path: module.source_path.clone(),
            reason: "current code-map entity is not reached by a tracked Rust source path".into(),
        });
    }
    current_residuals.sort_by(|left, right| {
        left.source_path
            .cmp(&right.source_path)
            .then_with(|| left.module_path.cmp(&right.module_path))
            .then_with(|| format!("{:?}", left.side).cmp(&format!("{:?}", right.side)))
    });
    let matched = derived_keys.intersection(&entity_keys).count() as u64;
    let derived_key_count = derived_keys.len() as u64;

    let history_navigation = history_navigation(
        &modules,
        &commits,
        &commit_module_edges,
        &request.bounds,
        join_unavailable_reason.as_deref(),
    );
    let tags = if request.provenance.clone_tags.completed() {
        let mut tags = Vec::new();
        for (name, target_sha, committed_at) in release_tags(&request.repo_path)? {
            let committed_at = match committed_at {
                Some(value) => Availability::available(normalize_record_time(
                    &value,
                    &format!("tag {name}"),
                    "committed_at",
                )?),
                None => Availability::unavailable("tag timestamp unavailable"),
            };
            tags.push(ReleaseTag {
                name,
                target_sha,
                committed_at,
            });
        }
        bounded_page(tags, request.bounds.aggregate_rows, "tag_name")
    } else {
        Page::unavailable(
            request.bounds.aggregate_rows,
            "tag_name",
            request
                .provenance
                .clone_tags
                .unavailable_reason("clone tags")
                .expect("non-completed clone tag coverage has a reason"),
        )
    };

    let mut public_commits = commits
        .iter()
        .map(|commit| commit.node.clone())
        .collect::<Vec<_>>();
    public_commits.sort_by(|left, right| {
        right
            .committed_at
            .cmp(&left.committed_at)
            .then_with(|| left.sha.cmp(&right.sha))
    });

    let graph = RepoGraph {
        repository: RepositoryNode {
            id: repository_id,
            label: format!("{}/{}", repository.owner, repository.name),
        },
        packages: covered_page_reason(
            packages,
            request.bounds.packages,
            "package_id",
            code_ingest_reason.as_deref(),
        ),
        modules: covered_page_reason(
            modules.clone(),
            request.bounds.modules,
            "module_id",
            code_ingest_reason.as_deref(),
        ),
        functions: SymbolPage::empty(),
        datatypes: SymbolPage::empty(),
        interfaces: SymbolPage::empty(),
        commits: covered_page(
            public_commits,
            request.bounds.commits,
            "committed_at_desc,sha",
            &commit_coverage,
            "commit history",
        ),
        issues: covered_page(
            issues.clone(),
            request.bounds.issues,
            "issue_number_desc",
            &issue_coverage,
            "issue history",
        ),
        pull_requests: covered_page(
            pull_requests.clone(),
            request.bounds.pull_requests,
            "pull_request_number_desc",
            &pull_request_coverage,
            "pull-request history",
        ),
        structure_edges: covered_page_reason(
            structure_edges.clone(),
            request.bounds.structure_edges,
            "edge_id",
            code_ingest_reason.as_deref(),
        ),
        history_edges: covered_page_reason(
            history_edges,
            request.bounds.history_edges,
            "edge_id",
            history_edges_unavailable_reason.as_deref(),
        ),
        commit_module_edges: covered_page_reason(
            commit_module_edges.clone(),
            request.bounds.commit_module_edges,
            "edge_id",
            join_unavailable_reason.as_deref(),
        ),
        history_navigation,
        join_resolution: JoinResolution {
            scope: JoinScope {
                languages: vec!["rust".into()],
                python: Availability::unavailable(
                    "Python path-to-module resolution is unmeasured in khive.repo.v1",
                ),
                typescript: Availability::unavailable(
                    "TypeScript path-to-module resolution is unmeasured in khive.repo.v1",
                ),
            },
            repositories: match &code_ingest_reason {
                Some(reason) => Availability::unavailable(reason.clone()),
                None => Availability::available(vec![RepositoryResolution {
                    repository: repository.canonical_url.clone(),
                    language: "rust".into(),
                    files,
                    derived_keys: derived_key_count,
                    entity_keys: entity_keys.len() as u64,
                    matched,
                    resolution_rate: if derived_key_count == 0 {
                        Availability::unavailable(
                            "resolution rate is undefined because no Rust module keys were derived",
                        )
                    } else {
                        Availability::available(
                            Ratio::new(matched as f64 / derived_key_count as f64)
                                .expect("matched keys cannot exceed derived keys"),
                        )
                    },
                    residuals: bounded_page(
                        current_residuals,
                        request.bounds.residuals,
                        "side,source_path,module_path",
                    ),
                }]),
            },
            historical: match &join_unavailable_reason {
                Some(reason) => Availability::unavailable(reason.clone()),
                None => Availability::available(vec![HistoricalJoinCoverage {
                    repository: repository.canonical_url.clone(),
                    language: "rust".into(),
                    total_changed_paths,
                    rust_in_scope_paths,
                    matched_rust_paths,
                    out_of_scope_paths,
                    unresolved_rust_paths: bounded_page(
                        historical_residuals,
                        request.bounds.residuals,
                        "commit_sha,source_path",
                    ),
                }]),
            },
        },
    };
    Ok(PreparedGraph {
        graph,
        commits,
        full_modules: modules,
        full_structure_edges: structure_edges,
        full_commit_module_edges: commit_module_edges,
        full_issues: issues,
        full_pull_requests: pull_requests,
        tags,
    })
}

fn history_navigation(
    modules: &[ModuleNode],
    commits: &[CommitWork],
    edges: &[GraphEdge],
    bounds: &ExportBounds,
    join_unavailable_reason: Option<&str>,
) -> HistoryNavigation {
    let commit_order = commits
        .iter()
        .map(|commit| {
            (
                commit.node.id.as_str(),
                (commit.node.committed_at.as_ref(), commit.node.sha.as_str()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut commits_by_module: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut modules_by_commit: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for edge in edges {
        commits_by_module
            .entry(&edge.target)
            .or_default()
            .push(edge.source.clone());
        modules_by_commit
            .entry(&edge.source)
            .or_default()
            .push(edge.target.clone());
    }
    let by_module = modules
        .iter()
        .map(|module| {
            let mut commit_ids = commits_by_module
                .remove(module.id.as_str())
                .unwrap_or_default();
            commit_ids.sort_by(|left, right| {
                let left_key = commit_order
                    .get(left.as_str())
                    .expect("derived edge references an indexed commit");
                let right_key = commit_order
                    .get(right.as_str())
                    .expect("derived edge references an indexed commit");
                right_key
                    .0
                    .cmp(left_key.0)
                    .then_with(|| left_key.1.cmp(right_key.1))
            });
            commit_ids.dedup();
            let commits = match join_unavailable_reason {
                None => bounded_page(
                    commit_ids,
                    bounds.navigation_per_entity,
                    "committed_at_desc,sha",
                ),
                Some(reason) => Page::unavailable(
                    bounds.navigation_per_entity,
                    "committed_at_desc,sha",
                    reason,
                ),
            };
            ModuleHistoryNavigation {
                module_id: module.id.clone(),
                commits,
                pull_requests: Availability::unavailable(
                    "git.digest does not provide a complete pull-request-to-commit set",
                ),
                issues: Availability::unavailable(
                    "git.digest issue references are not evidence that an issue touched a module",
                ),
            }
        })
        .collect();
    let mut by_commit = commits
        .iter()
        .map(|commit| {
            let mut module_ids = modules_by_commit
                .remove(commit.node.id.as_str())
                .unwrap_or_default();
            module_ids.sort();
            module_ids.dedup();
            CommitHistoryNavigation {
                commit_id: commit.node.id.clone(),
                modules: bounded_page(module_ids, bounds.navigation_per_entity, "module_id"),
            }
        })
        .collect::<Vec<_>>();
    by_commit.sort_by(|left, right| {
        let left_key = commit_order
            .get(left.commit_id.as_str())
            .expect("navigation commit is indexed");
        let right_key = commit_order
            .get(right.commit_id.as_str())
            .expect("navigation commit is indexed");
        right_key
            .0
            .cmp(left_key.0)
            .then_with(|| left_key.1.cmp(right_key.1))
    });
    let by_commit = match join_unavailable_reason {
        None => bounded_page(
            by_commit,
            bounds.navigation_entities,
            "committed_at_desc,sha",
        ),
        Some(reason) => {
            Page::unavailable(bounds.navigation_entities, "committed_at_desc,sha", reason)
        }
    };
    let by_module = match join_unavailable_reason {
        Some(reason) => Page::unavailable(bounds.navigation_entities, "module_id", reason),
        None => bounded_page(by_module, bounds.navigation_entities, "module_id"),
    };
    HistoryNavigation {
        by_module,
        by_commit,
    }
}

fn capability(graph: &RepoGraph, aggregates: &RepoAggregates) -> Capability {
    let structure_reason = [&graph.modules.disclosure, &graph.structure_edges.disclosure]
        .into_iter()
        .find(|disclosure| disclosure.status == DisclosureStatus::Unavailable)
        .and_then(|disclosure| disclosure.reason.clone());
    let structure_status = if structure_reason.is_some() {
        ViewStatus::Unavailable
    } else {
        ViewStatus::Available
    };
    let navigation_reason =
        if graph.commit_module_edges.disclosure.status == DisclosureStatus::Unavailable {
            graph
                .commit_module_edges
                .disclosure
                .reason
                .clone()
                .or_else(|| Some("commit-to-module join is unavailable".into()))
        } else {
            None
        };
    let navigation_status = if navigation_reason.is_some() {
        ViewStatus::Unavailable
    } else {
        ViewStatus::Available
    };
    let commit_module_facet = match &navigation_reason {
        Some(reason) => Availability::unavailable(reason.clone()),
        None => Availability::available(true),
    };
    let rust_measured = matches!(
        graph.join_resolution.repositories,
        Availability::Available { .. }
    );
    let rust_module_join = rust_measured && navigation_status == ViewStatus::Available;
    let rust_reason = if rust_module_join {
        None
    } else {
        navigation_reason
            .clone()
            .or_else(|| structure_reason.clone())
            .or_else(|| Some("Rust join coverage is unavailable for this bundle".into()))
    };

    Capability {
        mode: CapabilityMode::StaticShowcase,
        read_only: true,
        writes: false,
        live_queries: false,
        on_demand_ingest: false,
        languages: LanguageCapabilities {
            rust: LanguageCapability {
                label: "Rust".into(),
                module_join: rust_module_join,
                measured: rust_measured,
                reason: rust_reason,
            },
            python: LanguageCapability {
                label: "Python".into(),
                module_join: false,
                measured: false,
                reason: Some("Python path-to-module resolution is not measured in v1".into()),
            },
            typescript: LanguageCapability {
                label: "TypeScript".into(),
                module_join: false,
                measured: false,
                reason: Some("TypeScript path-to-module resolution is not measured in v1".into()),
            },
        },
        labels: CapabilityLabels {
            product: "khive Repository Showcase".into(),
            input_placeholder: "https://github.com/owner/repository".into(),
            lookup_action: "Open repository".into(),
            miss_title: "Not in the showcase set yet".into(),
            miss_body: "This static showcase only contains its curated repository set.".into(),
            unavailable: "Unavailable".into(),
            truncated: "Truncated".into(),
            derived: "Derived".into(),
            ingested: "Ingested".into(),
            node_types: NodeTypeLabels {
                repository: "Repository".into(),
                package: "Package".into(),
                module: "Module".into(),
                function: "Function".into(),
                datatype: "Datatype".into(),
                interface: "Interface".into(),
                commit: "Commit".into(),
                issue: "Issue".into(),
                pull_request: "Pull request".into(),
            },
            metrics: MetricLabels {
                change_frequency: "Change frequency".into(),
                fan_in: "Fan-in".into(),
                fan_out: "Fan-out".into(),
                cochange_count: "Co-change count".into(),
                support: "Support".into(),
                source_files: "Source files".into(),
                recent_activity: "Recent activity".into(),
                week: "Week".into(),
                commits: "Commits".into(),
                issues_opened: "Issues opened".into(),
                issues_closed: "Issues closed".into(),
                pull_requests_opened: "Pull requests opened".into(),
                pull_requests_merged: "Pull requests merged".into(),
                lead_time: "Lead time".into(),
                p50: "50th percentile".into(),
                p90: "90th percentile".into(),
                p95: "95th percentile".into(),
                author_concentration: "Author concentration".into(),
                bus_factor: "Bus factor".into(),
                dependent_count: "Dependent count".into(),
                cycle_count: "Dependency cycles".into(),
                resolution: "Resolution".into(),
                repository_age: "Repository age".into(),
                package_count: "Packages".into(),
                module_count: "Modules".into(),
                symbol_count: "Symbols".into(),
                activity_trend: "Activity trend".into(),
                top_hotspots: "Top hotspots".into(),
                ownership_warnings: "Ownership warnings".into(),
            },
            hotspot_quadrants: HotspotQuadrantLabels {
                high_churn_high_fan_in: "High churn · high fan-in".into(),
                high_churn_low_fan_in: "High churn · low fan-in".into(),
                low_churn_high_fan_in: "Low churn · high fan-in".into(),
                low_churn_low_fan_in: "Low churn · low fan-in".into(),
            },
        },
        views: ViewCatalog {
            structure_graph: view_capability(
                "Structure graph",
                Granularity::ModuleSymbolDeferred,
                JoinTag::StructureOnly,
                structure_status,
                structure_reason,
            ),
            history_structure_navigation: HistoryStructureViewCapability {
                label: "History-structure navigation".into(),
                granularity: Granularity::Module,
                join: JoinTag::Join,
                status: navigation_status,
                unavailable_reason: navigation_reason,
                commit_module_facet,
                pull_request_module_facet: Availability::unavailable(
                    "no explicit pull-request-to-linked-commit evidence is ingested",
                ),
                issue_module_facet: Availability::unavailable(
                    "issue references do not prove that an issue touched a module",
                ),
            },
            dependency_topology: view_from_meta(
                "Dependency topology",
                &aggregates.dependency_topology.meta,
            ),
            hotspot_quadrant: view_from_meta("Hotspot quadrant", &aggregates.hotspot_quadrant.meta),
            hidden_coupling: view_from_meta("Hidden coupling", &aggregates.hidden_coupling.meta),
            structure_treemap: view_from_meta(
                "Structure treemap",
                &aggregates.structure_treemap.meta,
            ),
            cadence_timeline: view_from_meta("Cadence timeline", &aggregates.cadence_timeline.meta),
            ownership: view_from_meta("Ownership", &aggregates.ownership.meta),
            api_surface: view_from_meta("De-facto API surface", &aggregates.api_surface.meta),
            scorecard: view_from_meta("Scorecard", &aggregates.scorecard.meta),
        },
    }
}

fn view_from_meta(label: &str, meta: &AnalysisMeta) -> ViewCapability {
    view_capability(
        label,
        meta.granularity,
        meta.join,
        meta.status,
        meta.unavailable_reason.clone(),
    )
}

fn view_capability(
    label: &str,
    granularity: Granularity,
    join: JoinTag,
    status: ViewStatus,
    unavailable_reason: Option<String>,
) -> ViewCapability {
    ViewCapability {
        label: label.into(),
        granularity,
        join,
        status,
        unavailable_reason,
    }
}
