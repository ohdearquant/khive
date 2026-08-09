//! Offline repository-showcase orchestration (ADR-147).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Subcommand, ValueEnum};
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_git::source::{parse_source, remote_url_to_slug, DigestSource};
use khive_repo_showcase::{
    export, write_canonical_atomic, Availability, CodeIngestProvenance, ExportRequest,
    GitDigestProvenance, HistorySourceCoverage, PipelineProvenance, SourceCoverage,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DIGEST_MAX_ITEMS: u64 = 2_000;
const DEFAULT_MAX_DIGEST_PASSES: usize = 10_000;

/// Maximum bytes read from one tracked Rust source during showcase ingestion.
const MAX_RUST_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum bytes read from one tracked Cargo manifest during join derivation.
const MAX_CARGO_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Maximum combined bytes across tracked Rust sources and Cargo manifests.
const MAX_RELEVANT_INPUT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// Export a bundle from an existing clone, history database, and code map.
    Export(RepoExportArgs),

    /// Clone/resolve, ingest both stores, and export one showcase bundle.
    #[command(alias = "showcase")]
    Build(RepoBuildArgs),
}

#[derive(Args, Debug)]
pub struct RepoExportArgs {
    /// Absolute path to the clean repository clone represented by the stores.
    #[arg(long)]
    pub repo: PathBuf,

    /// Dedicated graph database populated by git.digest.
    #[arg(long)]
    pub history_db: PathBuf,

    /// Dedicated code-map database populated by code.ingest.
    #[arg(long)]
    pub map_db: PathBuf,

    /// Public HTTPS repository URL represented by the bundle.
    #[arg(long)]
    pub repository_url: String,

    /// Explicit RFC3339 bundle-generation timestamp.
    #[arg(long)]
    pub generated_at: String,

    /// Explicit default-branch label. Omit when it was not independently pinned.
    #[arg(long)]
    pub default_branch: Option<String>,

    /// Destination for canonical khive.repo.v1 JSON bytes.
    #[arg(long, alias = "output")]
    pub out: PathBuf,
}

#[derive(Args, Debug)]
pub struct RepoBuildArgs {
    /// Absolute local clone path or public HTTPS repository URL.
    #[arg(long)]
    pub source: String,

    /// Public HTTPS identity override. Required for a local clone without a usable origin.
    #[arg(long)]
    pub repository_url: Option<String>,

    /// Exact 40-hex commit to showcase. Defaults to the source's current HEAD.
    #[arg(long)]
    pub revision: Option<String>,

    /// Scratch directory for the isolated checkout and two dedicated databases.
    #[arg(long)]
    pub work_dir: PathBuf,

    /// Override the dedicated history database path.
    #[arg(long)]
    pub history_db: Option<PathBuf>,

    /// Override the dedicated code-map database path.
    #[arg(long)]
    pub map_db: Option<PathBuf>,

    /// History sources to ingest. Commits are mandatory in v1.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "commits,issues,pull-requests"
    )]
    pub include: Vec<HistorySource>,

    /// Tag-ref policy. `none` is reproducible from a commit; `current` observes the forge now.
    #[arg(long, value_enum, default_value = "none")]
    pub tags: CloneTagsMode,

    /// Explicit default-branch label. Omit to encode it as unavailable.
    #[arg(long)]
    pub default_branch: Option<String>,

    /// Maximum git.digest passes before the pipeline refuses to loop further.
    #[arg(long, default_value_t = DEFAULT_MAX_DIGEST_PASSES)]
    pub max_digest_passes: usize,

    /// Explicit RFC3339 bundle-generation timestamp.
    #[arg(long)]
    pub generated_at: String,

    /// Destination for canonical khive.repo.v1 JSON bytes.
    #[arg(long, alias = "output")]
    pub out: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
pub enum HistorySource {
    Commits,
    Issues,
    PullRequests,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CloneTagsMode {
    None,
    Current,
}

impl CloneTagsMode {
    fn wire_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Current => "current",
        }
    }
}

impl HistorySource {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Commits => "commits",
            Self::Issues => "issues",
            Self::PullRequests => "pull_requests",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum SourceState {
    Completed,
    StoppedEarly(String),
    Skipped(String),
}

impl SourceState {
    fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DigestSources {
    pub commits: Option<SourceState>,
    pub issues: Option<SourceState>,
    pub pull_requests: Option<SourceState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DigestReport {
    pub done: bool,
    pub cursor_stalled: bool,
    pub writes_refused: u64,
    #[serde(default)]
    pub write_refusals: Vec<Value>,
    pub commits_ingested: u64,
    pub commits_skipped_existing: u64,
    pub commits_total_in_db: u64,
    pub issues_ingested: u64,
    pub issues_skipped_existing: u64,
    pub prs_ingested: u64,
    pub prs_skipped_existing: u64,
    pub changed_paths_filtered_noncanonical: u64,
    pub code_module_ambiguous_path_skips: u64,
    pub history_exhausted: bool,
    pub gh_available: Option<bool>,
    pub project_id: Option<String>,
    pub sources: DigestSources,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodeIngestReport {
    pub source_revision: String,
    pub db_path: String,
    pub projects_created: u64,
    pub projects_updated: u64,
    pub modules_created: u64,
    pub modules_updated: u64,
    pub edges_created: u64,
    pub edges_updated: u64,
    pub blocked_count: u64,
    #[serde(default)]
    pub blocked: Vec<Value>,
    pub coverage_stamps_missed: u64,
    pub files_dropped_without_source_path: u64,
    #[serde(default)]
    pub files_skipped_without_module_path: u64,
    pub languages: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BuildResult {
    schema_version: &'static str,
    repository_url: String,
    head_sha: String,
    output: String,
    history_db: String,
    map_db: String,
    generated_at: String,
    digest_passes: usize,
    requested_sources: Vec<&'static str>,
    tags: &'static str,
    default_branch: Option<String>,
    digest: DigestReport,
    code: CodeIngestReport,
}

#[derive(Debug, Serialize)]
struct ExportResult {
    schema_version: &'static str,
    repository_url: String,
    head_sha: String,
    output: String,
    generated_at: String,
    ingest_provenance: &'static str,
    default_branch: Option<String>,
}

struct ResolvedSource {
    repo: PathBuf,
    repository_url: String,
    remote: bool,
}

pub async fn run_repo(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Export(args) => run_export(args),
        RepoCommand::Build(args) => run_build(args).await,
    }
}

fn run_export(args: RepoExportArgs) -> Result<()> {
    let repo = canonical_repo(&args.repo)?;
    ensure_clean_snapshot(&repo)?;
    preflight_tracked_inputs(&repo)?;
    let generated_at = canonical_timestamp(&args.generated_at)?;
    let repository_url = canonical_repository_url(&args.repository_url)?;
    let head_sha = git_output(&repo, &["rev-parse", "--verify", "HEAD"])?;
    let default_branch = args
        .default_branch
        .as_deref()
        .map(|branch| canonical_branch(&repo, branch))
        .transpose()?;
    ensure_generated_at_not_before_head(&repo, &generated_at)?;
    let request = ExportRequest::new(
        repo,
        args.history_db,
        args.map_db,
        generated_at.clone(),
        repository_url.clone(),
        PipelineProvenance::unknown(head_sha.clone()),
    )
    .with_default_branch(default_branch_coverage(default_branch.clone()));
    export_to_path(request, &args.out)?;
    print_json(&ExportResult {
        schema_version: "khive.repo.export.v1",
        repository_url,
        head_sha,
        output: args.out.display().to_string(),
        generated_at,
        ingest_provenance: "unknown (pipeline reports were not supplied)",
        default_branch,
    })
}

async fn run_build(args: RepoBuildArgs) -> Result<()> {
    if args.max_digest_passes == 0 {
        bail!("--max-digest-passes must be greater than zero");
    }
    let requested: BTreeSet<HistorySource> = args.include.into_iter().collect();
    if !requested.contains(&HistorySource::Commits) {
        bail!("repository showcase v1 requires commits in --include");
    }

    let revision = args
        .revision
        .as_deref()
        .map(canonical_revision)
        .transpose()?;
    let generated_at = canonical_timestamp(&args.generated_at)?;
    let resolved_source = resolve_source(&args.source, args.repository_url.as_deref())?;
    ensure_clean_snapshot(&resolved_source.repo)?;
    let default_branch = args
        .default_branch
        .as_deref()
        .map(|branch| canonical_branch(&resolved_source.repo, branch))
        .transpose()?;

    let requested_work_dir = absolute_path(args.work_dir.clone())?;
    if normalized_future_path(&requested_work_dir).starts_with(&resolved_source.repo) {
        bail!(
            "--work-dir {} must be outside the source repository",
            args.work_dir.display()
        );
    }
    std::fs::create_dir_all(&requested_work_dir)
        .with_context(|| format!("create work directory {}", requested_work_dir.display()))?;
    let work_dir = requested_work_dir
        .canonicalize()
        .with_context(|| format!("canonicalize work directory {}", args.work_dir.display()))?;
    if work_dir.starts_with(&resolved_source.repo) {
        bail!(
            "--work-dir {} must be outside the source repository",
            args.work_dir.display()
        );
    }
    let input_repo = resolved_source.repo.clone();
    let expected_source_path = work_dir.join("source");
    let history_db = absolute_path(
        args.history_db
            .unwrap_or_else(|| work_dir.join("history.db")),
    )?;
    let map_db = absolute_path(args.map_db.unwrap_or_else(|| work_dir.join("code-map.db")))?;
    if normalized_future_path(&history_db) == normalized_future_path(&map_db) {
        bail!("history and code-map databases must be distinct files");
    }
    for (label, path) in [("history", &history_db), ("code-map", &map_db)] {
        if path.exists() || sqlite_sidecars(path).iter().any(|sidecar| sidecar.exists()) {
            bail!(
                "{label} database {} or one of its SQLite sidecars already exists; repo build requires fresh dedicated stores (use repo export to read existing stores)",
                path.display()
            );
        }
        let normalized_path = normalized_future_path(path);
        if normalized_path.starts_with(&input_repo)
            || normalized_path.starts_with(normalized_future_path(&expected_source_path))
        {
            bail!(
                "{label} database {} must be outside the source repository",
                path.display()
            );
        }
    }
    create_parent(&history_db)?;
    create_parent(&map_db)?;

    let source = materialize_source(resolved_source, revision.as_deref(), &work_dir)?;
    ensure_clean_snapshot(&source.repo)?;
    preflight_tracked_inputs(&source.repo)?;
    let initial_head = git_output(&source.repo, &["rev-parse", "--verify", "HEAD"])?;
    ensure_generated_at_not_before_head(&source.repo, &generated_at)?;

    let packs = vec!["kg".to_string(), "git".to_string(), "code".to_string()];
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(history_db.clone()),
        default_namespace: Namespace::local(),
        packs: packs.clone(),
        actor_id: None,
        ..RuntimeConfig::no_embeddings()
    })
    .map_err(|error| anyhow::anyhow!(error))
    .with_context(|| format!("open dedicated history database {}", history_db.display()))?;
    let server = KhiveMcpServer::with_packs(runtime, &packs)
        .map_err(|error| anyhow::anyhow!(error))
        .context("build repository-showcase ingest registry")?;

    let wire_sources: Vec<&str> = requested.iter().map(|source| source.wire_name()).collect();
    let (digest, digest_passes) =
        digest_to_completion(&server, &source.repo, &wire_sources, args.max_digest_passes).await?;
    verify_digest(&digest, &requested)?;
    let commit_count = git_output(&source.repo, &["rev-list", "--count", "HEAD"])?
        .parse::<u64>()
        .context("parse git rev-list --count HEAD")?;
    if digest.commits_total_in_db != commit_count {
        bail!(
            "history coverage mismatch: git.digest reports {} commits in its project, but git rev-list reports {commit_count}",
            digest.commits_total_in_db
        );
    }
    require_unchanged_head(&source.repo, &initial_head, "git.digest")?;

    // `code.ingest(db=...)` treats an explicit path as an operator-selected,
    // already-current map. Repo build is itself the owner-authorized creation
    // workflow for this fresh dedicated output, so initialize it explicitly
    // before handing the path across the public verb boundary.
    drop(
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(map_db.clone()),
            default_namespace: Namespace::local(),
            packs: vec!["kg".to_string(), "code".to_string()],
            actor_id: None,
            ..RuntimeConfig::no_embeddings()
        })
        .map_err(|error| anyhow::anyhow!(error))
        .with_context(|| format!("initialize dedicated code map {}", map_db.display()))?,
    );

    let code_value = dispatch_single(
        &server,
        "code.ingest",
        json!({
            "path": path_for_request(&source.repo, "repository")?,
            "db": path_for_request(&map_db, "code-map database")?,
            "languages": ["rust"],
        }),
    )
    .await?;
    let code: CodeIngestReport = serde_json::from_value(code_value)
        .context("decode code.ingest repository-showcase report")?;
    verify_code_ingest(&code, &initial_head, &map_db)?;
    require_unchanged_head(&source.repo, &initial_head, "code.ingest")?;
    let tag_coverage = establish_tag_coverage(&source.repo, args.tags)?;
    require_unchanged_head(&source.repo, &initial_head, "tag observation")?;
    let provenance = pipeline_provenance(&digest, digest_passes, &requested, &code, tag_coverage)?;

    let request = ExportRequest::new(
        source.repo.clone(),
        history_db.clone(),
        map_db.clone(),
        generated_at.clone(),
        source.repository_url.clone(),
        provenance,
    )
    .with_default_branch(default_branch_coverage(default_branch.clone()));
    export_to_path(request, &args.out)?;

    print_json(&BuildResult {
        schema_version: "khive.repo.build.v1",
        repository_url: source.repository_url,
        head_sha: initial_head,
        output: args.out.display().to_string(),
        history_db: history_db.display().to_string(),
        map_db: map_db.display().to_string(),
        generated_at,
        digest_passes,
        requested_sources: wire_sources,
        tags: args.tags.wire_name(),
        default_branch,
        digest,
        code,
    })
}

fn export_to_path(request: ExportRequest, output: &Path) -> Result<()> {
    let history_path = normalized_future_path(&request.history_db);
    let map_path = normalized_future_path(&request.map_db);
    if history_path == map_path {
        bail!("history and code-map databases must be distinct files");
    }
    let output_path = normalized_future_path(output);
    if output_path == history_path || output_path == map_path {
        bail!(
            "bundle output {} must not overwrite an input database",
            output.display()
        );
    }
    for (label, path) in [
        ("history", request.history_db.as_path()),
        ("code-map", request.map_db.as_path()),
    ] {
        if !path.is_file() {
            bail!("{label} database {} does not exist", path.display());
        }
    }
    let bundle = export(&request).map_err(|error| anyhow::anyhow!(error))?;
    write_canonical_atomic(&bundle, output).map_err(|error| anyhow::anyhow!(error))?;
    Ok(())
}

fn pipeline_provenance(
    digest: &DigestReport,
    digest_passes: usize,
    requested: &BTreeSet<HistorySource>,
    code: &CodeIngestReport,
    clone_tags: SourceCoverage,
) -> Result<PipelineProvenance> {
    let calls = u32::try_from(digest_passes)
        .context("git.digest pass count does not fit the bundle provenance model")?;
    let warnings_count = u64::try_from(code.warnings.len())
        .context("code.ingest warning count does not fit the bundle provenance model")?;
    Ok(PipelineProvenance {
        git_digest: Availability::available(GitDigestProvenance {
            calls,
            history_exhausted: digest.history_exhausted,
            cursor_stalled: digest.cursor_stalled,
            writes_refused: digest.writes_refused,
            changed_paths_filtered_noncanonical: digest.changed_paths_filtered_noncanonical,
            sources: HistorySourceCoverage {
                commits: source_coverage(
                    digest.sources.commits.as_ref(),
                    requested.contains(&HistorySource::Commits),
                    "commits",
                ),
                issues: source_coverage(
                    digest.sources.issues.as_ref(),
                    requested.contains(&HistorySource::Issues),
                    "issues",
                ),
                pull_requests: source_coverage(
                    digest.sources.pull_requests.as_ref(),
                    requested.contains(&HistorySource::PullRequests),
                    "pull requests",
                ),
            },
        }),
        code_ingest: Availability::available(CodeIngestProvenance {
            source_revision: code.source_revision.clone(),
            languages: code.languages.clone(),
            blocked_count: code.blocked_count,
            files_dropped_without_source_path: code.files_dropped_without_source_path,
            files_skipped_without_module_path: code.files_skipped_without_module_path,
            coverage_stamps_missed: code.coverage_stamps_missed,
            warnings_count,
        }),
        clone_tags,
    })
}

fn source_coverage(state: Option<&SourceState>, requested: bool, label: &str) -> SourceCoverage {
    if !requested {
        return SourceCoverage::Unrequested;
    }
    match state {
        Some(SourceState::Completed) => SourceCoverage::Completed,
        Some(SourceState::StoppedEarly(reason)) => SourceCoverage::StoppedEarly {
            reason: reason.clone(),
        },
        Some(SourceState::Skipped(reason)) => SourceCoverage::Skipped {
            reason: reason.clone(),
        },
        None => SourceCoverage::Unknown {
            reason: format!("git.digest omitted requested {label} coverage"),
        },
    }
}

fn default_branch_coverage(branch: Option<String>) -> Availability<String> {
    branch.map(Availability::available).unwrap_or_else(|| {
        Availability::unavailable(
            "default branch was not explicitly supplied; mutable origin/HEAD was not inferred",
        )
    })
}

fn establish_tag_coverage(repo: &Path, mode: CloneTagsMode) -> Result<SourceCoverage> {
    match mode {
        CloneTagsMode::None => Ok(SourceCoverage::Unrequested),
        CloneTagsMode::Current => {
            git_output(
                repo,
                &[
                    "fetch",
                    "--force",
                    "--prune",
                    "--prune-tags",
                    "origin",
                    "+refs/tags/*:refs/tags/*",
                ],
            )?;
            // A successful forced, pruning tag-ref fetch is the evidence for
            // Completed. The generated timestamp records when this mutable
            // clone-derived observation was taken.
            Ok(SourceCoverage::Completed)
        }
    }
}

async fn digest_to_completion(
    server: &KhiveMcpServer,
    repo: &Path,
    include: &[&str],
    max_passes: usize,
) -> Result<(DigestReport, usize)> {
    let source = path_for_request(repo, "repository")?;
    for pass in 1..=max_passes {
        let value = dispatch_single(
            server,
            "git.digest",
            json!({
                "source": source,
                "max_items": DIGEST_MAX_ITEMS,
                "include": include,
            }),
        )
        .await?;
        let report: DigestReport =
            serde_json::from_value(value).context("decode git.digest report")?;
        eprintln!(
            "git.digest pass {pass}/{max_passes}: commits +{}/{}, issues +{}/{}, prs +{}/{}, done={}",
            report.commits_ingested,
            report.commits_total_in_db,
            report.issues_ingested,
            report.issues_skipped_existing,
            report.prs_ingested,
            report.prs_skipped_existing,
            report.done,
        );
        if report.cursor_stalled || report.writes_refused > 0 {
            bail!(
                "git.digest pass {pass} is not clean: cursor_stalled={}, writes_refused={}",
                report.cursor_stalled,
                report.writes_refused
            );
        }
        if report.changed_paths_filtered_noncanonical > 0 {
            bail!(
                "git.digest pass {pass} filtered {} noncanonical changed path(s); refusing an incomplete showcase bundle",
                report.changed_paths_filtered_noncanonical
            );
        }
        if report.done {
            return Ok((report, pass));
        }
    }
    bail!(
        "git.digest did not reach done=true within {max_passes} passes; increase --max-digest-passes after inspecting source progress"
    )
}

fn verify_digest(report: &DigestReport, requested: &BTreeSet<HistorySource>) -> Result<()> {
    let commits = report
        .sources
        .commits
        .as_ref()
        .context("git.digest omitted commits source coverage")?;
    if !commits.is_completed() {
        bail!("git.digest did not exhaust commit history: {commits:?}");
    }
    for (source, state) in [
        (HistorySource::Issues, report.sources.issues.as_ref()),
        (
            HistorySource::PullRequests,
            report.sources.pull_requests.as_ref(),
        ),
    ] {
        if !requested.contains(&source) {
            if state.is_some() {
                bail!("git.digest reported unrequested source {source:?}");
            }
            continue;
        }
        // Optional forge sources may be completed, skipped before the walk,
        // or stopped after a partial walk. Every state is retained verbatim
        // in bundle provenance; the exporter renders non-completed series as
        // unavailable rather than presenting partial data as complete.
        let _state = state.with_context(|| format!("git.digest omitted {source:?} coverage"))?;
    }
    Ok(())
}

fn verify_code_ingest(
    report: &CodeIngestReport,
    expected_head: &str,
    expected_db: &Path,
) -> Result<()> {
    if report.source_revision != expected_head {
        bail!(
            "code.ingest revision mismatch: expected {expected_head}, got {}",
            report.source_revision
        );
    }
    if report.blocked_count > 0 {
        bail!(
            "code.ingest secret gate blocked {} write(s); refusing a partial map",
            report.blocked_count
        );
    }
    if report.coverage_stamps_missed > 0 || report.files_dropped_without_source_path > 0 {
        bail!(
            "code.ingest coverage is incomplete: coverage_stamps_missed={}, files_dropped_without_source_path={}",
            report.coverage_stamps_missed,
            report.files_dropped_without_source_path
        );
    }
    let reported_db = PathBuf::from(&report.db_path);
    let reported_db = reported_db.canonicalize().with_context(|| {
        format!(
            "canonicalize code.ingest database {}",
            reported_db.display()
        )
    })?;
    let expected_db = expected_db.canonicalize().with_context(|| {
        format!(
            "canonicalize expected code-map database {}",
            expected_db.display()
        )
    })?;
    if reported_db != expected_db {
        bail!(
            "code.ingest wrote {}, but the showcase requested {}",
            reported_db.display(),
            expected_db.display()
        );
    }
    Ok(())
}

async fn dispatch_single(server: &KhiveMcpServer, tool: &str, args: Value) -> Result<Value> {
    let ops = serde_json::to_string(&json!({"tool": tool, "args": args}))
        .context("serialize repository-showcase operation")?;
    let raw = server
        .dispatch_request_local(RequestParams {
            ops,
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let response: Value =
        serde_json::from_str(&raw).with_context(|| format!("decode {tool} dispatch response"))?;
    let item = response
        .get("results")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .with_context(|| format!("{tool} response omitted its result entry"))?;
    if item.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = item
            .get("error")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown error".to_string()));
        bail!("{tool} failed: {error}");
    }
    item.get("result")
        .cloned()
        .with_context(|| format!("{tool} success omitted result"))
}

fn resolve_source(raw: &str, override_url: Option<&str>) -> Result<ResolvedSource> {
    if raw.starts_with("https://") {
        validate_https_url_components(raw)?;
    }
    let parsed = parse_source(raw).map_err(anyhow::Error::msg)?;
    match parsed {
        DigestSource::Local(path) => {
            let repo = canonical_repo(&path)?;
            let repository_url = match override_url {
                Some(url) => canonical_repository_url(url)?,
                None => repository_url_from_origin(&repo)?,
            };
            Ok(ResolvedSource {
                repo,
                repository_url,
                remote: false,
            })
        }
        DigestSource::Remote { .. } => {
            let repository_url = canonical_repository_url(raw)?;
            if let Some(url) = override_url {
                let explicit = canonical_repository_url(url)?;
                if explicit != repository_url {
                    bail!(
                        "--repository-url {explicit:?} does not match remote source {repository_url:?}"
                    );
                }
            }
            // Only the validated, credential-free canonical URL reaches Git
            // argv, origin configuration, or the shared clone cache.
            let repo = khive_pack_git::cache::ensure_clone(&repository_url)
                .map_err(|error| anyhow::anyhow!(error))
                .context("resolve repository through the bounded git.digest clone cache")?;
            Ok(ResolvedSource {
                repo: canonical_repo(&repo)?,
                repository_url,
                remote: true,
            })
        }
    }
}

fn materialize_source(
    source: ResolvedSource,
    revision: Option<&str>,
    work_dir: &Path,
) -> Result<ResolvedSource> {
    let selected_revision = match revision {
        Some(revision) => revision.to_string(),
        None => git_output(&source.repo, &["rev-parse", "--verify", "HEAD"])?,
    };
    let resolved_revision = git_output(
        &source.repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{selected_revision}^{{commit}}"),
        ],
    )?;
    if resolved_revision != selected_revision {
        bail!(
            "requested revision {selected_revision} did not resolve to that exact commit (resolved {resolved_revision})"
        );
    }

    if !source.remote {
        let head = git_output(&source.repo, &["rev-parse", "--verify", "HEAD"])?;
        if head != selected_revision {
            bail!(
                "local source HEAD is {head}, but --revision requested {selected_revision}; check out the revision in a clean clone first"
            );
        }
    }

    let checkout = work_dir.join("source");
    if checkout.exists() {
        bail!(
            "isolated source checkout {} already exists; repo build requires a fresh work directory",
            checkout.display()
        );
    }
    let output = hardened_git_command()
        .arg("clone")
        .arg("--no-checkout")
        .arg(&source.repo)
        .arg(&checkout)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .context("spawn git clone for isolated showcase checkout")?;
    if !output.status.success() {
        bail!(
            "git clone from the bounded source cache into {} failed",
            checkout.display()
        );
    }
    git_output(
        &checkout,
        &["remote", "set-url", "origin", &source.repository_url],
    )?;
    git_output(&checkout, &["checkout", "--detach", &selected_revision])?;
    Ok(ResolvedSource {
        repo: canonical_repo(&checkout)?,
        repository_url: source.repository_url,
        remote: false,
    })
}

fn canonical_repository_url(raw: &str) -> Result<String> {
    validate_https_url_components(raw)?;
    let parsed = parse_source(raw).map_err(anyhow::Error::msg)?;
    let DigestSource::Remote { canonical, .. } = parsed else {
        bail!("repository URL must be a public https:// URL");
    };
    let slug = remote_url_to_slug(&canonical)
        .with_context(|| format!("repository URL {raw:?} does not identify host/owner/repo"))?;
    let mut segments = slug.split('/');
    let (Some(_host), Some(_owner), Some(_name), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        bail!("repository URL {raw:?} must have exact host/owner/repository shape");
    };
    Ok(format!("https://{slug}"))
}

fn validate_https_url_components(raw: &str) -> Result<()> {
    let Some(remainder) = raw.strip_prefix("https://") else {
        bail!("repository URL must be a public https:// URL");
    };
    if raw.chars().any(char::is_control) {
        bail!("repository URL must not contain control characters");
    }
    let authority = remainder.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        bail!("repository URL must include a host");
    }
    if authority.contains('@') {
        bail!("repository URL must not contain userinfo or credentials");
    }
    if remainder.contains('?') {
        bail!("repository URL must not contain a query component");
    }
    if remainder.contains('#') {
        bail!("repository URL must not contain a fragment component");
    }
    Ok(())
}

fn canonical_revision(raw: &str) -> Result<String> {
    let revision = raw.trim();
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("--revision must be an exact 40-hex commit id");
    }
    Ok(revision.to_ascii_lowercase())
}

fn canonical_branch(repo: &Path, raw: &str) -> Result<String> {
    let branch = raw.trim();
    if branch.is_empty() || branch != raw || branch.contains("@{") {
        bail!("--default-branch must be a normalized Git branch name");
    }
    let checked = git_output(repo, &["check-ref-format", "--branch", branch])
        .with_context(|| format!("--default-branch {branch:?} is not a valid Git branch"))?;
    if checked != branch {
        bail!("--default-branch must be a normalized Git branch name");
    }
    Ok(branch.to_string())
}

fn repository_url_from_origin(repo: &Path) -> Result<String> {
    let origin = git_output(repo, &["remote", "get-url", "origin"]).with_context(|| {
        "local source has no usable origin; pass --repository-url with its public HTTPS identity"
    })?;
    if origin.starts_with("https://") {
        canonical_repository_url(&origin)
    } else {
        let slug = remote_url_to_slug(&origin).with_context(|| {
            "local source origin is not a recognizable public repository URL; pass --repository-url"
        })?;
        canonical_repository_url(&format!("https://{slug}"))
    }
}

fn canonical_repo(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("repository path {} must be absolute", path.display());
    }
    let repo = path
        .canonicalize()
        .with_context(|| format!("canonicalize repository {}", path.display()))?;
    if !repo.join(".git").exists() {
        bail!(
            "repository {} does not contain a .git entry",
            repo.display()
        );
    }
    Ok(repo)
}

fn ensure_clean_snapshot(repo: &Path) -> Result<()> {
    let status = git_output(repo, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let relevant: Vec<&str> = status
        .lines()
        .filter(|line| line.trim() != "?? .khive-last-used")
        .collect();
    if !relevant.is_empty() {
        bail!(
            "repository {} is not a clean HEAD snapshot (first change: {})",
            repo.display(),
            relevant[0]
        );
    }
    Ok(())
}

fn preflight_tracked_inputs(repo: &Path) -> Result<()> {
    let root = repo
        .canonicalize()
        .with_context(|| format!("canonicalize repository {}", repo.display()))?;
    let output = hardened_git_command()
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "--cached", "--stage", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .context("enumerate tracked repository inputs")?;
    if !output.status.success() {
        bail!("git ls-files failed while preflighting repository inputs");
    }
    let records = String::from_utf8(output.stdout)
        .context("tracked repository paths must be valid UTF-8 for code ingestion")?;
    let mut relevant_bytes = 0_u64;

    for record in records.split_terminator('\0') {
        let (header, raw_path) = record
            .split_once('\t')
            .context("git ls-files emitted a malformed tracked-file record")?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields
            .next()
            .context("tracked-file record omitted its Git mode")?;
        let _object_id = fields
            .next()
            .context("tracked-file record omitted its object id")?;
        let stage = fields
            .next()
            .context("tracked-file record omitted its index stage")?;
        if fields.next().is_some() || stage != "0" {
            bail!("tracked-file index contains a non-stage-zero entry");
        }

        let relative = Path::new(raw_path);
        if raw_path.is_empty()
            || relative.is_absolute()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            bail!("tracked path {raw_path:?} is not a safe repository-relative path");
        }
        match mode {
            "100644" | "100755" => {}
            "120000" => bail!("tracked symlink {raw_path:?} is not accepted for showcase ingest"),
            _ => bail!(
                "tracked special file {raw_path:?} with Git mode {mode} is not accepted for showcase ingest"
            ),
        }

        let candidate = root.join(relative);
        let metadata = std::fs::symlink_metadata(&candidate)
            .with_context(|| format!("inspect tracked input {}", candidate.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("tracked symlink {raw_path:?} is not accepted for showcase ingest");
        }
        if !metadata.file_type().is_file() {
            bail!("tracked special file {raw_path:?} is not accepted for showcase ingest");
        }
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("canonicalize tracked input {}", candidate.display()))?;
        if !canonical.starts_with(&root) {
            bail!("tracked input {raw_path:?} resolves outside the repository root");
        }

        let limit = if relative.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
            Some((MAX_CARGO_MANIFEST_BYTES, "Cargo.toml"))
        } else if relative
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("rs")
        {
            Some((MAX_RUST_SOURCE_BYTES, "Rust source"))
        } else {
            None
        };
        let Some((per_file_limit, kind)) = limit else {
            continue;
        };
        let bytes = metadata.len();
        if bytes > per_file_limit {
            bail!(
                "tracked {kind} {raw_path:?} is {bytes} bytes, exceeding the {per_file_limit}-byte per-file showcase limit"
            );
        }
        relevant_bytes = relevant_bytes
            .checked_add(bytes)
            .context("tracked relevant-input byte count overflowed")?;
        if relevant_bytes > MAX_RELEVANT_INPUT_BYTES {
            bail!(
                "tracked Rust sources and Cargo manifests total {relevant_bytes} bytes, exceeding the {MAX_RELEVANT_INPUT_BYTES}-byte showcase limit"
            );
        }
    }
    Ok(())
}

fn require_unchanged_head(repo: &Path, expected: &str, stage: &str) -> Result<()> {
    let actual = git_output(repo, &["rev-parse", "--verify", "HEAD"])?;
    if actual != expected {
        bail!("repository HEAD changed during {stage}: expected {expected}, got {actual}");
    }
    ensure_clean_snapshot(repo)
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = hardened_git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed for {}", args.join(" "), repo.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn hardened_git_command() -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "gc.auto=0"])
        .args(["-c", "maintenance.auto=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT");
    command
}

fn canonical_timestamp(raw: &str) -> Result<String> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("--generated-at {raw:?} is not RFC3339"))?;
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn ensure_generated_at_not_before_head(repo: &Path, generated_at: &str) -> Result<()> {
    let generated = DateTime::parse_from_rfc3339(generated_at)
        .context("parse canonical --generated-at timestamp")?;
    let head_raw = git_output(repo, &["show", "-s", "--format=%cI", "HEAD"])?;
    let head = DateTime::parse_from_rfc3339(&head_raw)
        .with_context(|| format!("parse HEAD committer timestamp {head_raw:?}"))?;
    if generated < head {
        bail!(
            "--generated-at {generated_at} predates HEAD commit time {}; repository bundle provenance cannot precede its snapshot",
            head.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        );
    }
    Ok(())
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("resolve current directory")?
            .join(path))
    }
}

fn normalized_future_path(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }
    let mut normalized = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for part in suffix.into_iter().rev() {
        normalized.push(part);
    }
    normalized
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create database directory {}", parent.display()))
}

fn path_for_request<'a>(path: &'a Path, label: &str) -> Result<&'a str> {
    path.to_str()
        .with_context(|| format!("{label} path {} is not valid UTF-8", path.display()))
}

fn sqlite_sidecars(path: &Path) -> [PathBuf; 3] {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    let mut journal = path.as_os_str().to_os_string();
    journal.push("-journal");
    [
        PathBuf::from(wal),
        PathBuf::from(shm),
        PathBuf::from(journal),
    ]
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
