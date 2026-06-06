// FILE SIZE JUSTIFICATION: kg.rs implements seven KG subcommands (validate, init, fetch, export,
// import, status, hook) that all share private helpers (NDJSON I/O, rule evaluation, archive
// building) and private test fixtures. Splitting across files would require making those helpers
// pub(crate), increasing the API surface and breaking the invariant that each helper is only used
// by the one command it supports. The inline test module requires pub(crate) access to private
// validation functions (check_no_duplicate_uuids, validate_rule_pass, etc.) that are not
// exposed even as pub(crate) — tests must be co-located to call them without relaxing visibility.

//! `kkernel kg` — KG validation, init, hook management, fetch, export, import,
//! and status (ADR-034, ADR-035, ADR-037, ADR-010, ADR-020, ADR-036).
//!
//! Implements:
//! - `kkernel kg validate` — structural + rule-pass validation
//! - `kkernel kg init`     — initialize `.khive/kg/` directory and `khive.toml`
//! - `kkernel kg hook`     — install / uninstall / status of the pre-commit hook
//! - `kkernel kg fetch`    — fetch a remote KG archive (alias: `sync`)
//! - `kkernel kg export`   — export namespace-scoped archive from SQLite DB
//! - `kkernel kg import`   — import archive or adapter records into SQLite DB
//! - `kkernel kg status`   — compare DB state against on-disk NDJSON files

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Subcommand;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::EdgeRelation;
use khive_types::EntityKind;
use khive_vcs_adapters::{EdgeRecord, EntityRecord, FormatAdapter, JsonFormatAdapter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Subcommand tree ────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum KgCommand {
    /// Validate the KG in `.khive/kg/` against structural and rule-pass checks.
    Validate(ValidateArgs),

    /// Initialize `.khive/kg/` and write `.khive/khive.toml` with defaults.
    Init(InitArgs),

    /// Fetch a remote KG archive into `.khive/kg/remotes/<remote>/`.
    ///
    /// `sync` is a visible alias so ADR-037's `kkernel kg sync --repin <remote>`
    /// reaches the same implementation.
    #[command(visible_alias = "sync")]
    Fetch(FetchArgs),

    /// Export a namespace-scoped KG archive from a SQLite DB.
    Export(ExportArgs),

    /// Import a KG archive or flat adapter records into a SQLite DB.
    Import(ImportArgs),

    /// Compare DB state against `.khive/kg/{entities,edges}.ndjson`.
    Status(StatusArgs),

    /// Manage the pre-commit hook for KG validation.
    #[command(subcommand)]
    Hook(HookCommand),
}

#[derive(clap::Parser, Debug)]
pub struct ValidateArgs {
    /// Repository root containing `.khive/kg/`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Apply fixable rules and report what changed.
    #[arg(long)]
    pub fix: bool,

    /// Treat warnings as errors; exit 1 when warnings > 0.
    #[arg(long)]
    pub strict: bool,

    /// Output format.
    #[arg(long, default_value = "text")]
    pub format: OutputFormat,

    /// Show all violations (default: cap at 2 then `+ N more`).
    #[arg(long)]
    pub verbose: bool,

    /// Print summary line only.
    #[arg(long)]
    pub quiet: bool,

    /// Override the default `.khive/kg/rules.toml` path.
    #[arg(long)]
    pub rules: Option<PathBuf>,

    /// Run ADR-020 built-in structural checks only; skip `rules.yaml`.
    #[arg(long)]
    pub no_rules: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
    Github,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Text => write!(f, "text"),
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Github => write!(f, "github"),
        }
    }
}

#[derive(clap::Parser, Debug)]
pub struct InitArgs {
    /// Repository root to initialize.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Also generate `.github/workflows/kg-validate.yml`.
    #[arg(long)]
    pub ci: bool,

    /// Install the pre-commit hook without reinitializing.
    #[arg(long)]
    pub add_hooks: bool,
}

#[derive(clap::Parser, Debug)]
pub struct FetchArgs {
    /// Remote name, used for cache path `.khive/kg/remotes/<remote>`.
    pub remote: String,

    /// Repository root that owns `.khive/`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Git remote URL.
    #[arg(long)]
    pub url: String,

    /// Git ref to fetch.
    #[arg(long = "ref", default_value = "main")]
    pub git_ref: String,

    /// Namespace to assign to fetched records.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Optional content hash pin: sha256:<64 lowercase hex chars>.
    #[arg(long)]
    pub pin: Option<String>,

    /// Accept the fetched content hash and return it for schema.yaml repinning.
    #[arg(long)]
    pub repin: bool,
}

#[derive(clap::Parser, Debug)]
pub struct ExportArgs {
    /// Output archive JSON path.
    pub output: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace to export.
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

#[derive(clap::Parser, Debug)]
pub struct ImportArgs {
    /// Source archive or adapter input file.
    pub source: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace for imported records.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Import format. Default is the ADR-010 KgArchive JSON envelope.
    #[arg(long, value_enum, default_value_t = ImportFormat::Archive)]
    pub format: ImportFormat,

    /// Print adapter warnings to stderr.
    #[arg(long)]
    pub verbose: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Archive,
    Json,
    Ndjson,
}

#[derive(clap::Parser, Debug)]
pub struct StatusArgs {
    /// Repository root containing `.khive/kg/{entities,edges}.ndjson`.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// SQLite database path. Required so this command never defaults to ~/.khive.
    #[arg(long)]
    pub db: PathBuf,

    /// Namespace to compare.
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

#[derive(Subcommand, Debug)]
pub enum HookCommand {
    /// Create `.git/hooks/pre-commit` symlink pointing to the tracked hook.
    Install {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove the `.git/hooks/pre-commit` symlink.
    Uninstall {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Show whether the hook symlink exists and points to a valid target.
    Status {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

// ── Output types ───────────────────────────────────────────────────────────────

/// Hash-based comparison result between the DB state and on-disk NDJSON files.
#[derive(Debug, Serialize)]
pub struct KgStatusReport {
    pub db_hash: String,
    pub ndjson_hash: String,
    pub clean: bool,
}

#[derive(Debug, Serialize)]
pub struct ValidationReport {
    pub rules: Vec<RuleResult>,
    pub summary: ValidationSummary,
}

#[derive(Debug, Serialize)]
pub struct RuleResult {
    pub id: String,
    pub severity: &'static str,
    pub passed: bool,
    pub violations: Vec<Violation>,
}

#[derive(Debug, Serialize)]
pub struct Violation {
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub entity_kind: Option<String>,
    pub rule_id: String,
    pub severity: &'static str,
    pub message: String,
    pub fixable: bool,
}

#[derive(Debug, Serialize)]
pub struct ValidationSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub entities: usize,
    pub edges: usize,
    pub passed: bool,
}

// ── Entry points ───────────────────────────────────────────────────────────────

/// Dispatch `kkernel kg` subcommands to their implementations.
pub async fn run_kg(cmd: KgCommand) -> Result<()> {
    match cmd {
        KgCommand::Validate(args) => cmd_validate(args),
        KgCommand::Init(args) => cmd_init(args),
        KgCommand::Fetch(args) => cmd_fetch(args).await,
        KgCommand::Export(args) => cmd_export(args).await,
        KgCommand::Import(args) => cmd_import(args).await,
        KgCommand::Status(args) => cmd_status(args).await,
        KgCommand::Hook(h) => cmd_hook(h),
    }
}

// ── fetch ─────────────────────────────────────────────────────────────────────

fn is_safe_remote_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

async fn cmd_fetch(args: FetchArgs) -> Result<()> {
    if !is_safe_remote_name(&args.remote) {
        bail!(
            "invalid remote name {:?}: must be [A-Za-z0-9._-]+ and not . or ..",
            args.remote
        );
    }

    let pin = args
        .pin
        .as_deref()
        .map(khive_vcs::SnapshotId::from_prefixed)
        .transpose()
        .context("invalid --pin")?;

    let remote = crate::sync::RemoteConfig {
        name: args.remote,
        url: args.url,
        git_ref: args.git_ref,
        namespace: args.namespace,
        pin,
    };

    let report = crate::sync::run_sync_remote(&args.repo, &remote, args.repin)
        .await
        .with_context(|| format!("fetch remote {:?}", remote.name))?;
    let json = serde_json::to_string(&report).expect("serialize RemoteSyncReport");
    println!("{json}");
    Ok(())
}

// ── export ────────────────────────────────────────────────────────────────────

async fn cmd_export(args: ExportArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;

    // Refuse to clobber the source database with the JSON export (codex #529).
    // Resolve the output's real identity: canonicalize it directly when it
    // already exists (this follows an existing symlink to its target), else
    // canonicalize the parent and rejoin the file name. Compare literally too so
    // `./x.db` vs `x.db` can't slip through.
    let db_canon = std::fs::canonicalize(&args.db).ok();
    let out_canon = std::fs::canonicalize(&args.output).ok().or_else(|| {
        args.output
            .parent()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .map(|p| p.join(args.output.file_name().unwrap_or_default()))
    });
    if args.output == args.db || (db_canon.is_some() && db_canon == out_canon) {
        anyhow::bail!(
            "refusing to export: --output {} resolves to the --db path {} (would overwrite the database)",
            args.output.display(),
            args.db.display(),
        );
    }

    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let token = runtime.authorize(ns)?;

    let json = runtime
        .export_kg_json(&token)
        .await
        .with_context(|| format!("export namespace {:?}", args.namespace))?;
    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    // Write through a temp sibling + atomic rename so a symlinked --output is
    // replaced rather than followed into the source DB. The temp is created with
    // O_EXCL (create_new): a pre-existing temp path — including a planted symlink
    // to the DB — fails the create rather than being followed, closing the whole
    // symlink-overwrite class, not just --output itself (codex #529).
    use std::io::Write as _;
    let mut tmp_name = args.output.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.inprogress", std::process::id()));
    let tmp = args.output.with_file_name(tmp_name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("create temp {}", tmp.display()))?;
    f.write_all(json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, &args.output)
        .with_context(|| format!("finalize {}", args.output.display()))?;
    Ok(())
}

// ── import ────────────────────────────────────────────────────────────────────

async fn cmd_import(args: ImportArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;
    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let token = runtime.authorize(ns)?;

    let source = std::fs::read_to_string(&args.source)
        .with_context(|| format!("read {}", args.source.display()))?;

    let summary = match args.format {
        ImportFormat::Archive => {
            let archive: KgArchive = serde_json::from_str(&source)
                .with_context(|| format!("parse archive {}", args.source.display()))?;
            validate_archive_entity_kinds(&archive)?;
            runtime
                .import_kg(&archive, &token)
                .await
                .with_context(|| format!("import archive {}", args.source.display()))?
        }
        ImportFormat::Json | ImportFormat::Ndjson => {
            let input = match args.format {
                ImportFormat::Json => source,
                ImportFormat::Ndjson => ndjson_to_json_array(&source)?,
                ImportFormat::Archive => unreachable!(),
            };
            let mut adapter = JsonFormatAdapter::new(&input)
                .with_context(|| format!("parse adapter input {}", args.source.display()))?;
            if args.verbose {
                for warning in adapter.warnings() {
                    eprintln!("warning: {warning}");
                }
            }
            let entities: Vec<EntityRecord> = adapter
                .entities()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let edges: Vec<EdgeRecord> = adapter
                .edges()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let archive = adapter_records_to_archive(&args.namespace, entities, edges)?;
            runtime
                .import_kg(&archive, &token)
                .await
                .with_context(|| format!("import adapter records {}", args.source.display()))?
        }
    };

    let json = serde_json::to_string(&summary).expect("serialize ImportSummary");
    println!("{json}");
    Ok(())
}

fn ndjson_to_json_array(source: &str) -> Result<String> {
    let mut values = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse NDJSON line {}", idx + 1))?;
        values.push(value);
    }
    serde_json::to_string(&values).context("serialize NDJSON records as JSON array")
}

fn adapter_records_to_archive(
    namespace: &str,
    entities: Vec<EntityRecord>,
    edges: Vec<EdgeRecord>,
) -> Result<KgArchive> {
    let now = Utc::now();
    let entity_ids: HashSet<Uuid> = entities.iter().map(|e| e.id).collect();

    let exported_entities: Vec<ExportedEntity> = entities
        .into_iter()
        .map(|e| {
            let _: EntityKind = e.kind.parse().map_err(|_| {
                anyhow::anyhow!("unknown entity kind {:?} on entity {}", e.kind, e.id)
            })?;
            Ok(ExportedEntity {
                id: e.id,
                kind: e.kind,
                entity_type: None,
                name: e.name,
                description: e.description,
                properties: if e.properties.is_null() {
                    None
                } else {
                    Some(e.properties)
                },
                tags: e.tags,
                created_at: now,
                updated_at: now,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let exported_edges = edges
        .into_iter()
        .map(|edge| adapter_edge_to_exported(edge, &entity_ids))
        .collect::<Result<Vec<_>>>()?;

    Ok(KgArchive {
        format: "khive-kg".to_string(),
        version: "0.1".to_string(),
        namespace: namespace.to_string(),
        exported_at: now,
        entities: exported_entities,
        edges: exported_edges,
    })
}

/// Validate that a deserialized edge weight is finite and within [0.0, 1.0].
///
/// Rejects NaN, positive infinity, negative infinity, and values outside the
/// accepted edge-weight domain defined in ADR-002.
fn validate_edge_weight(weight: f64, edge_id: impl std::fmt::Display) -> Result<()> {
    if !weight.is_finite() {
        bail!(
            "edge {} weight {weight} is not finite (NaN or infinity not allowed)",
            edge_id
        );
    }
    if !(0.0..=1.0).contains(&weight) {
        bail!(
            "edge {} weight {weight} is outside the valid range [0.0, 1.0] (ADR-002)",
            edge_id
        );
    }
    Ok(())
}

fn validate_archive_entity_kinds(archive: &KgArchive) -> Result<()> {
    for e in &archive.entities {
        let _: EntityKind = e
            .kind
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown entity kind {:?} on entity {}", e.kind, e.id))?;
    }
    for edge in &archive.edges {
        validate_edge_weight(edge.weight, edge.edge_id)?;
    }
    Ok(())
}

fn adapter_edge_to_exported(edge: EdgeRecord, entity_ids: &HashSet<Uuid>) -> Result<ExportedEdge> {
    let source = edge
        .source
        .parse::<Uuid>()
        .with_context(|| format!("edge {} source must be a UUID", edge.edge_id))?;
    let target = edge
        .target
        .parse::<Uuid>()
        .with_context(|| format!("edge {} target must be a UUID", edge.edge_id))?;
    if !entity_ids.contains(&source) {
        bail!(
            "edge {} source {} is not present in adapter entities",
            edge.edge_id,
            source
        );
    }
    if !entity_ids.contains(&target) {
        bail!(
            "edge {} target {} is not present in adapter entities",
            edge.edge_id,
            target
        );
    }
    let relation: EdgeRelation = edge
        .relation
        .parse()
        .with_context(|| format!("edge {} invalid relation {:?}", edge.edge_id, edge.relation))?;

    validate_edge_weight(edge.weight, edge.edge_id)?;
    Ok(ExportedEdge {
        edge_id: edge.edge_id,
        source,
        target,
        relation,
        weight: edge.weight,
    })
}

// ── status ────────────────────────────────────────────────────────────────────

async fn cmd_status(args: StatusArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;
    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let token = runtime.authorize(ns)?;

    let db_archive = runtime.export_kg(&token).await?;
    let db_hash = khive_vcs::hash::snapshot_id_for_archive(&db_archive)
        .context("hash DB archive")?
        .as_str()
        .to_string();

    let ndjson_archive = archive_from_ndjson_repo(&args.repo, &args.namespace)?;
    let ndjson_hash = khive_vcs::hash::snapshot_id_for_archive(&ndjson_archive)
        .context("hash NDJSON archive")?
        .as_str()
        .to_string();

    let report = KgStatusReport {
        clean: db_hash == ndjson_hash,
        db_hash,
        ndjson_hash,
    };
    let json = serde_json::to_string(&report).expect("serialize KgStatusReport");
    println!("{json}");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StatusNdjsonEntity {
    id: Uuid,
    kind: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    properties: Option<serde_json::Value>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusNdjsonEdge {
    edge_id: Uuid,
    source: Uuid,
    target: Uuid,
    relation: String,
    #[serde(default = "default_status_edge_weight")]
    weight: f64,
}

fn default_status_edge_weight() -> f64 {
    1.0
}

fn archive_from_ndjson_repo(repo: &Path, namespace: &str) -> Result<KgArchive> {
    let kg_dir = repo.join(".khive/kg");
    let entities =
        read_ndjson_records::<StatusNdjsonEntity>(&kg_dir.join("entities.ndjson"), "entity")?;
    let edges = read_ndjson_records::<StatusNdjsonEdge>(&kg_dir.join("edges.ndjson"), "edge")?;
    let now = Utc::now();

    let exported_entities = entities
        .into_iter()
        .map(|e| ExportedEntity {
            id: e.id,
            kind: e.kind,
            entity_type: None,
            name: e.name,
            description: e.description,
            properties: e.properties,
            tags: e.tags,
            created_at: parse_status_dt(e.created_at.as_deref(), now),
            updated_at: parse_status_dt(e.updated_at.as_deref(), now),
        })
        .collect();

    let exported_edges = edges
        .into_iter()
        .map(|edge| {
            let relation: EdgeRelation = edge
                .relation
                .parse()
                .with_context(|| format!("invalid relation {:?}", edge.relation))?;
            validate_edge_weight(edge.weight, edge.edge_id)?;
            Ok(ExportedEdge {
                edge_id: edge.edge_id,
                source: edge.source,
                target: edge.target,
                relation,
                weight: edge.weight,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(KgArchive {
        format: "khive-kg".to_string(),
        version: "0.1".to_string(),
        namespace: namespace.to_string(),
        exported_at: now,
        entities: exported_entities,
        edges: exported_edges,
    })
}

fn read_ndjson_records<T>(path: &Path, label: &str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut records = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record = serde_json::from_str(trimmed)
            .with_context(|| format!("parse {label} at {}:{}", path.display(), idx + 1))?;
        records.push(record);
    }
    Ok(records)
}

fn parse_status_dt(value: Option<&str>, fallback: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    value
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(fallback)
}

// ── validate ──────────────────────────────────────────────────────────────────

fn cmd_validate(args: ValidateArgs) -> Result<()> {
    let kg_dir = args.repo.join(".khive/kg");
    if !kg_dir.exists() {
        bail!(
            "KG directory not found: {}. Run `kkernel kg init` first.",
            kg_dir.display()
        );
    }

    let entities_path = kg_dir.join("entities.ndjson");
    let edges_path = kg_dir.join("edges.ndjson");

    let entities = count_ndjson_lines(&entities_path).unwrap_or(0);
    let edges = count_ndjson_lines(&edges_path).unwrap_or(0);

    let rules_path = args.rules.unwrap_or_else(|| kg_dir.join("rules.toml"));

    // Run structural checks (ADR-020 built-ins).
    let mut rule_results: Vec<RuleResult> = structural_checks(&entities_path, &edges_path);

    // Run configurable rule pass unless --no-rules.
    if !args.no_rules && rules_path.exists() {
        let configurable = configurable_rule_checks(&entities_path, &edges_path, &rules_path)?;
        rule_results.extend(configurable);
    }

    let errors: usize = rule_results
        .iter()
        .filter(|r| r.severity == "error" && !r.passed)
        .count();
    let warnings: usize = rule_results
        .iter()
        .filter(|r| r.severity == "warning" && !r.passed)
        .count();
    let info: usize = rule_results
        .iter()
        .filter(|r| r.severity == "info" && !r.passed)
        .count();

    let passed = if args.strict {
        errors == 0 && warnings == 0
    } else {
        errors == 0
    };

    let summary = ValidationSummary {
        errors,
        warnings,
        info,
        entities,
        edges,
        passed,
    };

    let report = ValidationReport {
        rules: rule_results,
        summary,
    };

    match args.format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&report).expect("serialize ValidationReport");
            println!("{json}");
        }
        OutputFormat::Github => print_github_format(&report),
        OutputFormat::Text => print_text_format(&report, args.verbose, args.quiet),
    }

    if args.fix {
        apply_fixes(&args.repo)?;
    }

    if !report.summary.passed {
        std::process::exit(1);
    }
    Ok(())
}

fn count_ndjson_lines(path: &Path) -> Option<usize> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count())
}

fn structural_checks(entities_path: &Path, edges_path: &Path) -> Vec<RuleResult> {
    vec![
        check_no_duplicate_uuids(entities_path),
        check_sort_order(entities_path, edges_path),
        check_referential_integrity(entities_path, edges_path),
    ]
}

fn check_no_duplicate_uuids(entities_path: &Path) -> RuleResult {
    let mut seen = std::collections::HashSet::new();
    let mut violations = Vec::new();

    if let Ok(content) = std::fs::read_to_string(entities_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                    if !seen.insert(id.to_string()) {
                        violations.push(Violation {
                            entity_id: Some(id.to_string()),
                            entity_name: v.get("name").and_then(|n| n.as_str()).map(str::to_string),
                            entity_kind: v.get("kind").and_then(|k| k.as_str()).map(str::to_string),
                            rule_id: "no-duplicate-uuids".into(),
                            severity: "error",
                            message: format!("Duplicate UUID: {id}"),
                            fixable: false,
                        });
                    }
                }
            }
        }
    }

    RuleResult {
        id: "no-duplicate-uuids".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

fn check_sort_order(entities_path: &Path, edges_path: &Path) -> RuleResult {
    let mut violations = Vec::new();

    // Check entities.ndjson sorted by UUID.
    if let Ok(content) = std::fs::read_to_string(entities_path) {
        let ids: Vec<String> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("id")?.as_str().map(str::to_string))
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        if ids != sorted {
            violations.push(Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: "sort-order".into(),
                severity: "warning",
                message: "entities.ndjson is not sorted by UUID; run `kkernel kg validate --fix`"
                    .into(),
                fixable: true,
            });
        }
    }

    // Check edges.ndjson sorted by (source, target, relation).
    if let Ok(content) = std::fs::read_to_string(edges_path) {
        let keys: Vec<(String, String, String)> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                let s = v.get("source_id")?.as_str()?.to_string();
                let t = v.get("target_id")?.as_str()?.to_string();
                let r = v.get("relation")?.as_str()?.to_string();
                Some((s, t, r))
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        if keys != sorted {
            violations.push(Violation {
                entity_id: None,
                entity_name: None,
                entity_kind: None,
                rule_id: "sort-order".into(),
                severity: "warning",
                message:
                    "edges.ndjson is not sorted by (source, target, relation); run `kkernel kg validate --fix`"
                        .into(),
                fixable: true,
            });
        }
    }

    RuleResult {
        id: "sort-order".into(),
        severity: "warning",
        passed: violations.is_empty(),
        violations,
    }
}

fn check_referential_integrity(entities_path: &Path, edges_path: &Path) -> RuleResult {
    let mut violations = Vec::new();

    let entity_ids: std::collections::HashSet<String> =
        if let Ok(content) = std::fs::read_to_string(entities_path) {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .ok()
                        .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                })
                .collect()
        } else {
            std::collections::HashSet::new()
        };

    if let Ok(content) = std::fs::read_to_string(edges_path) {
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                for field in &["source_id", "target_id"] {
                    if let Some(id) = v.get(field).and_then(|i| i.as_str()) {
                        if !entity_ids.contains(id) {
                            violations.push(Violation {
                                entity_id: Some(id.to_string()),
                                entity_name: None,
                                entity_kind: None,
                                rule_id: "referential-integrity".into(),
                                severity: "error",
                                message: format!(
                                    "Edge {} references unknown entity: {id}",
                                    if *field == "source_id" {
                                        "source"
                                    } else {
                                        "target"
                                    }
                                ),
                                fixable: false,
                            });
                        }
                    }
                }
            }
        }
    }

    RuleResult {
        id: "referential-integrity".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

// ── Configurable rule loader (issue #382, ADR-034) ────────────────────────────

/// A single configurable lint rule loaded from `rules.toml` (ADR-034).
///
/// Each rule has `id`, `severity` ("error"|"warning"|"info"), `kind` ("entity"|"edge"),
/// an optional `condition` predicate, an optional `require_field`, and a `message`.
/// See `crates/kkernel/docs/kg-rules.md` for the full TOML format and examples.
#[derive(Debug, Deserialize)]
struct RuleConfig {
    /// Unique rule ID.
    id: String,
    /// Severity: "error", "warning", or "info".
    #[serde(default = "default_severity")]
    severity: String,
    /// Substrate: "entity" or "edge".
    kind: String,
    /// `field=value` filter: only records where `record[field] == value` are checked.
    /// Use `source_id=target_id` (literal string) for the special self-loop sentinel.
    condition: Option<String>,
    /// When set, the rule fails for any record (passing the condition filter)
    /// that has an empty or absent value for this field.
    require_field: Option<String>,
    /// Human-readable violation message template. `{id}` is replaced with the
    /// record's `id` field when present.
    #[serde(default)]
    message: String,
}

fn default_severity() -> String {
    "warning".to_owned()
}

/// Top-level structure of a `rules.toml` file.
#[derive(Debug, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

fn severity_static(s: &str) -> &'static str {
    match s {
        "error" => "error",
        "info" => "info",
        _ => "warning",
    }
}

/// Load and evaluate configurable rules from a TOML rules file (issue #382).
///
/// Supports `.toml` files directly. For `.yaml` / `.yml` files the function
/// returns an error directing the user to use TOML format, as `serde_yaml` is
/// not a workspace dependency.
///
/// Rule evaluation:
/// 1. Parse the TOML file into a `RulesFile` containing a `Vec<RuleConfig>`.
/// 2. For each rule, load the appropriate NDJSON file (entities or edges).
/// 3. Apply the `condition` filter (field=value equality or self-loop sentinel).
/// 4. For `require_field` rules, collect violations where the field is absent/empty.
/// 5. Return one `RuleResult` per rule.
fn configurable_rule_checks(
    entities_path: &Path,
    edges_path: &Path,
    rules_path: &Path,
) -> Result<Vec<RuleResult>> {
    // Extension-based format routing.
    let ext = rules_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if matches!(ext, "yaml" | "yml") {
        bail!(
            "rules file {:?} uses YAML format which is not supported in this build. \
             Rename it to {}.toml and use TOML format instead.",
            rules_path,
            rules_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rules")
        );
    }

    let content = std::fs::read_to_string(rules_path)
        .with_context(|| format!("read rules file {}", rules_path.display()))?;

    let rules_file: RulesFile = toml::from_str(&content)
        .with_context(|| format!("parse rules TOML {}", rules_path.display()))?;

    let mut results = Vec::with_capacity(rules_file.rules.len());
    for rule in &rules_file.rules {
        let path = match rule.kind.as_str() {
            "entity" => entities_path,
            "edge" => edges_path,
            other => {
                // Unknown kind — emit an error rule result.
                results.push(RuleResult {
                    id: rule.id.clone(),
                    severity: "error",
                    passed: false,
                    violations: vec![Violation {
                        entity_id: None,
                        entity_name: None,
                        entity_kind: None,
                        rule_id: rule.id.clone(),
                        severity: "error",
                        message: format!(
                            "Rule {:?}: unknown kind {other:?}; must be \"entity\" or \"edge\"",
                            rule.id
                        ),
                        fixable: false,
                    }],
                });
                continue;
            }
        };

        let violations = evaluate_rule(rule, path);
        let sev = severity_static(&rule.severity);
        results.push(RuleResult {
            id: rule.id.clone(),
            severity: sev,
            passed: violations.is_empty(),
            violations,
        });
    }

    Ok(results)
}

/// Evaluate a single `RuleConfig` against an NDJSON file.
fn evaluate_rule(rule: &RuleConfig, path: &Path) -> Vec<Violation> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    // Parse the condition: "field=value" or the sentinel "source_id=target_id".
    let condition: Option<(&str, &str)> = rule.condition.as_deref().and_then(|c| c.split_once('='));

    let sev = severity_static(&rule.severity);
    let mut violations = Vec::new();

    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(val) => val,
            Err(_) => continue,
        };

        // Apply condition filter.
        if let Some((field, expected)) = condition {
            // Special sentinel: "source_id=target_id" means check for self-loops.
            if field == "source_id" && expected == "target_id" {
                let src = v.get("source_id").and_then(|s| s.as_str()).unwrap_or("");
                let tgt = v.get("target_id").and_then(|s| s.as_str()).unwrap_or("");
                if src == tgt {
                    // Self-loop detected — always a violation for this rule type.
                    violations.push(Violation {
                        entity_id: Some(src.to_owned()),
                        entity_name: None,
                        entity_kind: v
                            .get("relation")
                            .and_then(|r| r.as_str())
                            .map(str::to_owned),
                        rule_id: rule.id.clone(),
                        severity: sev,
                        message: rule.message.replace("{id}", src),
                        fixable: false,
                    });
                }
                continue;
            }

            // Normal field=value filter: only proceed for records where field matches.
            let actual = v.get(field).and_then(|f| f.as_str()).unwrap_or("");
            if actual != expected {
                continue;
            }
        }

        // require_field check: the record must have a non-empty value for this field.
        if let Some(req) = rule.require_field.as_deref() {
            let val = v.get(req).and_then(|f| f.as_str()).unwrap_or("");
            if val.is_empty() {
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                violations.push(Violation {
                    entity_id: if id.is_empty() {
                        None
                    } else {
                        Some(id.to_owned())
                    },
                    entity_name: v.get("name").and_then(|n| n.as_str()).map(str::to_owned),
                    entity_kind: v.get("kind").and_then(|k| k.as_str()).map(str::to_owned),
                    rule_id: rule.id.clone(),
                    severity: sev,
                    message: rule.message.replace("{id}", id),
                    fixable: false,
                });
            }
        }
    }

    violations
}

fn apply_fixes(repo: &Path) -> Result<()> {
    let kg_dir = repo.join(".khive/kg");
    fix_sort_order(&kg_dir.join("entities.ndjson"), "id")?;
    fix_sort_order_edges(&kg_dir.join("edges.ndjson"))?;
    eprintln!("~ sort-order: applied fix to entities.ndjson and edges.ndjson");
    Ok(())
}

fn fix_sort_order(path: &Path, sort_key: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    lines.sort_by(|a, b| {
        let ak = a.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        let bk = b.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        ak.cmp(bk)
    });
    let out: String = lines
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, out + "\n").with_context(|| format!("write {}", path.display()))
}

fn fix_sort_order_edges(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    lines.sort_by(|a, b| {
        let ak = (
            a.get("source_id").and_then(|v| v.as_str()).unwrap_or(""),
            a.get("target_id").and_then(|v| v.as_str()).unwrap_or(""),
            a.get("relation").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let bk = (
            b.get("source_id").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("target_id").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("relation").and_then(|v| v.as_str()).unwrap_or(""),
        );
        ak.cmp(&bk)
    });
    let out: String = lines
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, out + "\n").with_context(|| format!("write {}", path.display()))
}

fn print_text_format(report: &ValidationReport, verbose: bool, quiet: bool) {
    if !quiet {
        for r in &report.rules {
            let symbol = if r.passed {
                "\u{2713}"
            } else if r.severity == "error" {
                "\u{2717}"
            } else {
                "\u{26a0}"
            };
            if r.violations.is_empty() {
                println!("  {symbol} {}", r.id);
            } else {
                println!("  {symbol} {}: {} violation(s)", r.id, r.violations.len());
                let shown = if verbose {
                    r.violations.len()
                } else {
                    2.min(r.violations.len())
                };
                for v in &r.violations[..shown] {
                    println!("    - {}", v.message);
                }
                if !verbose && r.violations.len() > 2 {
                    println!("    + {} more (run with --verbose)", r.violations.len() - 2);
                }
            }
        }
    }
    let s = &report.summary;
    println!(
        "\nSummary: {} error(s), {} warning(s), {} entities, {} edges",
        s.errors, s.warnings, s.entities, s.edges
    );
}

fn print_github_format(report: &ValidationReport) {
    for r in &report.rules {
        for v in &r.violations {
            let level = if r.severity == "error" {
                "error"
            } else {
                "warning"
            };
            println!("::{level} ::{}", v.message);
        }
    }
}

// ── init ──────────────────────────────────────────────────────────────────────

const DEFAULT_KHIVE_TOML: &str = r#"# .khive/khive.toml — project KG configuration (ADR-035)
# Committed to git. All collaborators use these settings.

[[backends]]
name = "main"
path = "~/.khive/khive.db"
cache_mb = 256
journal_mode = "wal"

[[engines]]
name = "mE5-small"
dim = 384
weight = 1.0

[packs.kg]
backend = "main"
engines = ["mE5-small"]

[packs.memory]
backend = "main"
engines = ["mE5-small"]

[packs.gtd]
backend = "main"
engines = []

[embed]
model = "mE5-small"
dimensions = 384
auto_embed = true
batch_size = 64

[embed.fields]
include = ["name", "description"]

[schema]
strict = true
"#;

const GITIGNORE_CONTENT: &str = "*\n!.gitignore\n!kg/\n!kg/**\n!khive.toml\n";

const PRE_COMMIT_HOOK: &str = r#"#!/usr/bin/env bash
# .khive/kg/hooks/pre-commit
# Generated by kkernel kg init.
# Runs KG validation on staged NDJSON files.
# Bypass with: git commit --no-verify

set -euo pipefail

staged=$(git diff --cached --name-only \
  | grep -E '^\.khive/kg/(entities|edges)\.ndjson$' || true)
if [ -z "$staged" ]; then
  exit 0
fi

kkernel kg validate
"#;

const CI_WORKFLOW: &str = r#"name: KG Validate
on:
  push:
    paths: [".khive/kg/**"]
  pull_request:
    paths: [".khive/kg/**"]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Validate KG
        run: kkernel kg validate --format github
"#;

fn cmd_init(args: InitArgs) -> Result<()> {
    if args.add_hooks {
        return hook_install(&args.repo);
    }

    let khive_dir = args.repo.join(".khive");
    let kg_dir = khive_dir.join("kg");
    let hooks_dir = kg_dir.join("hooks");

    std::fs::create_dir_all(&kg_dir).with_context(|| format!("create {}", kg_dir.display()))?;
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("create {}", hooks_dir.display()))?;

    // Write entities.ndjson and edges.ndjson if absent.
    for name in &["entities.ndjson", "edges.ndjson"] {
        let path = kg_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, "").with_context(|| format!("create {}", path.display()))?;
        }
    }

    // Write .khive/.gitignore.
    let gitignore = khive_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE_CONTENT)
            .with_context(|| format!("write {}", gitignore.display()))?;
    }

    // Write .khive/khive.toml (do not overwrite).
    let toml_path = khive_dir.join("khive.toml");
    if !toml_path.exists() {
        std::fs::write(&toml_path, DEFAULT_KHIVE_TOML)
            .with_context(|| format!("write {}", toml_path.display()))?;
        println!("  Initialized {}", toml_path.display());
    } else {
        println!("  Skipped {} (already exists)", toml_path.display());
    }

    // Write pre-commit hook script.
    let hook_script = hooks_dir.join("pre-commit");
    if !hook_script.exists() {
        std::fs::write(&hook_script, PRE_COMMIT_HOOK)
            .with_context(|| format!("write {}", hook_script.display()))?;
        // Make hook script executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_script, perms)?;
        }
    }

    println!("  Initialized .khive/kg/ (entities.ndjson, edges.ndjson, hooks/pre-commit)");

    if args.ci {
        let workflow_dir = args.repo.join(".github/workflows");
        std::fs::create_dir_all(&workflow_dir)
            .with_context(|| format!("create {}", workflow_dir.display()))?;
        let workflow_path = workflow_dir.join("kg-validate.yml");
        if !workflow_path.exists() {
            std::fs::write(&workflow_path, CI_WORKFLOW)
                .with_context(|| format!("write {}", workflow_path.display()))?;
            println!("  Generated {}", workflow_path.display());
        }
    }

    Ok(())
}

// ── hook ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HookStatus {
    pub symlink_exists: bool,
    pub symlink_target: Option<String>,
    pub target_valid: bool,
}

fn cmd_hook(cmd: HookCommand) -> Result<()> {
    match cmd {
        HookCommand::Install { repo } => hook_install(&repo),
        HookCommand::Uninstall { repo } => hook_uninstall(&repo),
        HookCommand::Status { repo } => hook_status(&repo),
    }
}

fn hook_install(repo: &Path) -> Result<()> {
    let hook_script = repo.join(".khive/kg/hooks/pre-commit");
    let git_hook = repo.join(".git/hooks/pre-commit");

    if !hook_script.exists() {
        bail!(
            "Hook script not found: {}. Run `kkernel kg init` first.",
            hook_script.display()
        );
    }

    if let Some(parent) = git_hook.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    if git_hook.exists() || git_hook.is_symlink() {
        std::fs::remove_file(&git_hook)
            .with_context(|| format!("remove existing {}", git_hook.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Use the absolute path for the symlink target.
        let absolute_script = hook_script
            .canonicalize()
            .unwrap_or_else(|_| hook_script.clone());
        symlink(&absolute_script, &git_hook)
            .with_context(|| format!("create symlink {}", git_hook.display()))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::copy(&hook_script, &git_hook)
            .with_context(|| format!("copy hook to {}", git_hook.display()))?;
    }

    println!(
        "  Installed: {} -> {}",
        git_hook.display(),
        hook_script.display()
    );
    Ok(())
}

fn hook_uninstall(repo: &Path) -> Result<()> {
    let git_hook = repo.join(".git/hooks/pre-commit");
    if git_hook.exists() || git_hook.is_symlink() {
        std::fs::remove_file(&git_hook)
            .with_context(|| format!("remove {}", git_hook.display()))?;
        println!("  Uninstalled: {}", git_hook.display());
    } else {
        println!("  No hook installed at {}", git_hook.display());
    }
    Ok(())
}

fn hook_status(repo: &Path) -> Result<()> {
    let git_hook = repo.join(".git/hooks/pre-commit");
    let symlink_exists = git_hook.exists() || git_hook.is_symlink();
    let symlink_target = if symlink_exists {
        std::fs::read_link(&git_hook)
            .ok()
            .map(|p| p.display().to_string())
    } else {
        None
    };
    let target_valid = symlink_target
        .as_deref()
        .map(|t| Path::new(t).exists())
        .unwrap_or(false);

    let status = HookStatus {
        symlink_exists,
        symlink_target,
        target_valid,
    };
    let json = serde_json::to_string(&status).expect("serialize HookStatus");
    println!("{json}");
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// INLINE TEST JUSTIFICATION: Tests call private validation functions
// (check_no_duplicate_uuids, validate_rule_pass, is_safe_remote_name, etc.) and private
// helpers that require module-private access. Moving them to crates/kkernel/tests/ would
// require exposing those functions as pub(crate), which widens the API surface beyond what
// the subcommand architecture intends.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_kg_dir(tmp: &TempDir) -> PathBuf {
        let kg_dir = tmp.path().join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        kg_dir
    }

    fn write_entities(kg_dir: &Path, entities: &[(&str, &str, &str)]) {
        let content: String = entities
            .iter()
            .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
    }

    fn write_edges(kg_dir: &Path, edges: &[(&str, &str, &str)]) {
        let content: String = edges
            .iter()
            .map(|(src, tgt, rel)| {
                format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
    }

    #[test]
    fn duplicate_uuid_detected() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A-dup"),
            ],
        );
        let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
        assert!(!result.passed, "duplicate UUID should fail");
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn no_duplicates_passes() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
        assert!(result.passed);
    }

    #[test]
    fn referential_integrity_catches_missing_target() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        write_edges(
            &kg_dir,
            &[(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            )],
        );
        let result = check_referential_integrity(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn init_creates_expected_files() {
        let tmp = TempDir::new().unwrap();
        let args = InitArgs {
            repo: tmp.path().to_path_buf(),
            ci: false,
            add_hooks: false,
        };
        cmd_init(args).unwrap();

        assert!(tmp.path().join(".khive/kg/entities.ndjson").exists());
        assert!(tmp.path().join(".khive/kg/edges.ndjson").exists());
        assert!(tmp.path().join(".khive/khive.toml").exists());
        assert!(tmp.path().join(".khive/kg/hooks/pre-commit").exists());
    }

    #[test]
    fn init_does_not_overwrite_existing_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".khive")).unwrap();
        let toml_path = tmp.path().join(".khive/khive.toml");
        std::fs::write(&toml_path, "# custom\n").unwrap();

        let args = InitArgs {
            repo: tmp.path().to_path_buf(),
            ci: false,
            add_hooks: false,
        };
        cmd_init(args).unwrap();

        let content = std::fs::read_to_string(&toml_path).unwrap();
        assert_eq!(content, "# custom\n", "should not overwrite existing toml");
    }

    // ── configurable_rule_checks (issue #382) ─────────────────────────────────

    #[test]
    fn configurable_rule_checks_empty_rules_file_returns_no_results() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_path = tmp.path().join("rules.toml");
        // Valid TOML with empty rules array.
        std::fs::write(&rules_path, "rules = []\n").unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert!(results.is_empty(), "no rules → no results");
    }

    #[test]
    fn configurable_rule_checks_require_field_detects_missing_description() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);

        // One entity with description, one without.
        let entities = r#"{"id":"aaa1","kind":"concept","name":"A","description":"has one"}
{"id":"aaa2","kind":"concept","name":"B"}
"#;
        std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_toml = r#"
[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.id, "concept-must-have-description");
        assert!(
            !r.passed,
            "rule should fail when a concept lacks description"
        );
        assert_eq!(r.violations.len(), 1);
        assert_eq!(r.violations[0].entity_id.as_deref(), Some("aaa2"));
    }

    #[test]
    fn configurable_rule_checks_self_loop_sentinel_detects_loop() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);

        write_entities(
            &kg_dir,
            &[
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        // One self-loop edge, one valid edge.
        let edges = r#"{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"aaaaaaaa-0000-0000-0000-000000000001","relation":"extends"}
{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
"#;
        std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();

        let rules_toml = r#"
[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
condition = "source_id=target_id"
message = "Self-loop detected on {id}"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(!r.passed);
        assert_eq!(r.violations.len(), 1, "exactly one self-loop");
    }

    #[test]
    fn configurable_rule_checks_yaml_extension_returns_error() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_path = tmp.path().join("rules.yaml");
        std::fs::write(&rules_path, "rules: []\n").unwrap();

        let result = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        );
        assert!(result.is_err(), "YAML extension must return an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("YAML") || msg.contains("toml"),
            "error message should mention TOML: {msg}"
        );
    }

    #[test]
    fn configurable_rule_checks_unknown_kind_produces_error_result() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        write_entities(
            &kg_dir,
            &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let rules_toml = r#"
[[rules]]
id = "bad-kind"
severity = "error"
kind = "note"
condition = "kind=concept"
require_field = "description"
message = "bad"
"#;
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, rules_toml).unwrap();

        let results = configurable_rule_checks(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
            &rules_path,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert_eq!(results[0].severity, "error");
    }

    #[test]
    fn sort_order_fix_sorts_entities() {
        let tmp = TempDir::new().unwrap();
        let kg_dir = make_kg_dir(&tmp);
        // Write out-of-order entities.
        write_entities(
            &kg_dir,
            &[
                ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
                ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
                ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ],
        );
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        fix_sort_order(&kg_dir.join("entities.ndjson"), "id").unwrap();
        let result = check_sort_order(
            &kg_dir.join("entities.ndjson"),
            &kg_dir.join("edges.ndjson"),
        );
        assert!(result.passed, "sort-order should pass after fix");
    }

    // ── fetch / sync alias ────────────────────────────────────────────────────

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {}",
            args.join(" "),
            status
        );
    }

    fn make_git_remote_for_kg(dir: &std::path::Path) -> String {
        let kg_dir = dir.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        let entity_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let entities = format!(
            r#"{{"id":"{entity_id}","kind":"concept","name":"RemoteEntity","properties":{{}},"tags":[]}}"#
        );
        std::fs::write(kg_dir.join("entities.ndjson"), &entities).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-m", "init"]);
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn fetch_populates_temp_remote_cache() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let remote_url = make_git_remote_for_kg(remote_dir.path());

        let args = FetchArgs {
            remote: "upstream".to_string(),
            repo: repo_dir.path().to_path_buf(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: None,
            repin: false,
        };

        cmd_fetch(args).await.unwrap();

        let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
        assert!(
            cache.join("entities.ndjson").exists(),
            "entities.ndjson in cache"
        );
        assert!(cache.join("edges.ndjson").exists(), "edges.ndjson in cache");
        assert!(cache.join("meta.json").exists(), "meta.json in cache");
    }

    // ── export ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn export_creates_archive_json() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let output_path = tmp.path().join("archive.json");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "TestEntity", None, None, vec![])
            .await
            .unwrap();

        let args = ExportArgs {
            output: output_path.clone(),
            db: db_path,
            namespace: "test-ns".to_string(),
        };
        cmd_export(args).await.unwrap();

        assert!(output_path.exists(), "output archive must exist");
        let content = std::fs::read_to_string(&output_path).unwrap();
        let archive: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(archive["format"].as_str().unwrap(), "khive-kg");
        let entities = archive["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1, "one entity exported");
        assert_eq!(entities[0]["name"].as_str().unwrap(), "TestEntity");
    }

    // codex #529: a symlinked --output pointing at the DB must be refused, and
    // the source DB must remain byte-for-byte intact.
    #[tokio::test]
    #[cfg(unix)]
    async fn export_refuses_symlinked_output_to_db() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("working.db");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "Keep", None, None, vec![])
            .await
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&db_path).unwrap();

        // --output is a symlink pointing straight at the DB.
        let link = tmp.path().join("archive.json");
        std::os::unix::fs::symlink(&db_path, &link).unwrap();

        let args = ExportArgs {
            output: link,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
        };
        assert!(
            cmd_export(args).await.is_err(),
            "export through a symlink to the DB must be refused"
        );

        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
    }

    // codex #529 round 3: a planted symlink at the temp write path must not be
    // followed into the DB either (create_new / O_EXCL refuses it).
    #[tokio::test]
    #[cfg(unix)]
    async fn export_refuses_symlinked_temp_to_db() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("working.db");

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();
        runtime
            .create_entity(&token, "concept", None, "Keep", None, None, vec![])
            .await
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&db_path).unwrap();

        // Plant a symlink at the exact temp path cmd_export will try to create
        // (same process => same pid suffix).
        let out = tmp.path().join("archive.json");
        let mut tmp_name = out.file_name().unwrap().to_os_string();
        tmp_name.push(format!(".{}.inprogress", std::process::id()));
        let temp_path = out.with_file_name(tmp_name);
        std::os::unix::fs::symlink(&db_path, &temp_path).unwrap();

        let args = ExportArgs {
            output: out,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
        };
        assert!(
            cmd_export(args).await.is_err(),
            "export must refuse when the temp path is a symlink to the DB"
        );
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
    }

    // ── import archive ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn import_archive_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("import-test.db");
        let entity_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let archive_json = format!(
            r#"{{"format":"khive-kg","version":"0.1","namespace":"test-ns","exported_at":"2026-01-01T00:00:00Z","entities":[{{"id":"{entity_id}","kind":"concept","name":"Imported","tags":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}],"edges":[]}}"#
        );
        let source_path = tmp.path().join("archive.json");
        std::fs::write(&source_path, &archive_json).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Archive,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.name, "Imported");
    }

    // ── import --format json ──────────────────────────────────────────────────

    #[tokio::test]
    async fn import_json_adapter_imports_entities() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-json.db");
        let e1_id = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let e2_id = "dddddddd-dddd-dddd-dddd-dddddddddddd";

        let json_input = format!(
            r#"[{{"id":"{e1_id}","kind":"concept","name":"Entity1"}},{{"id":"{e2_id}","kind":"concept","name":"Entity2"}}]"#
        );
        let source_path = tmp.path().join("records.json");
        std::fs::write(&source_path, &json_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Json,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let e1_uuid: Uuid = e1_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, e1_uuid).await.unwrap();
        assert_eq!(entity.name, "Entity1");
    }

    // ── import --format ndjson ────────────────────────────────────────────────

    #[tokio::test]
    async fn import_ndjson_adapter_imports_entity() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("adapter-ndjson.db");
        let entity_id = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

        let ndjson_input =
            format!(r#"{{"id":"{entity_id}","kind":"concept","name":"NdjsonEntity"}}"#);
        let source_path = tmp.path().join("records.ndjson");
        std::fs::write(&source_path, &ndjson_input).unwrap();

        let args = ImportArgs {
            source: source_path,
            db: db_path.clone(),
            namespace: "test-ns".to_string(),
            format: ImportFormat::Ndjson,
            verbose: false,
        };
        cmd_import(args).await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let rt2 = KhiveRuntime::new(config).unwrap();
        let tok2 = rt2.authorize(ns).unwrap();
        let entity_uuid: Uuid = entity_id.parse().unwrap();
        let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
        assert_eq!(entity.name, "NdjsonEntity");
    }

    // ── status ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn status_hashes_clean_after_sync() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let entity_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let entity_ndjson = format!(
            r#"{{"id":"{entity_id}","kind":"concept","name":"StatusEntity","properties":{{}},"tags":[]}}"#
        );
        let kg_dir = repo.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        std::fs::write(kg_dir.join("entities.ndjson"), &entity_ndjson).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let db = repo.join(".khive/state/working.db");
        crate::sync::run_sync(repo, &db, "test-ns").await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();

        let db_archive = runtime.export_kg(&token).await.unwrap();
        let ndjson_archive = archive_from_ndjson_repo(repo, "test-ns").unwrap();

        let db_hash = khive_vcs::hash::snapshot_id_for_archive(&db_archive).unwrap();
        let ndjson_hash = khive_vcs::hash::snapshot_id_for_archive(&ndjson_archive).unwrap();
        assert_eq!(db_hash, ndjson_hash, "hashes must match after sync");
    }

    // ── edge weight validation ────────────────────────────────────────────────

    #[test]
    fn validate_edge_weight_valid_boundaries() {
        assert!(validate_edge_weight(0.0, "edge-a").is_ok());
        assert!(validate_edge_weight(1.0, "edge-a").is_ok());
        assert!(validate_edge_weight(0.5, "edge-a").is_ok());
    }

    #[test]
    fn validate_edge_weight_nan_is_rejected() {
        let err = validate_edge_weight(f64::NAN, "edge-x").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
    }

    #[test]
    fn validate_edge_weight_infinity_is_rejected() {
        let err = validate_edge_weight(f64::INFINITY, "edge-y").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
        let err = validate_edge_weight(f64::NEG_INFINITY, "edge-y").unwrap_err();
        assert!(
            err.to_string().contains("not finite"),
            "expected 'not finite' in error: {err}"
        );
    }

    #[test]
    fn validate_edge_weight_out_of_range_is_rejected() {
        let err = validate_edge_weight(1.5, "edge-z").unwrap_err();
        assert!(
            err.to_string().contains("outside the valid range"),
            "expected range error: {err}"
        );
        let err = validate_edge_weight(-0.1, "edge-z").unwrap_err();
        assert!(
            err.to_string().contains("outside the valid range"),
            "expected range error: {err}"
        );
    }
}
