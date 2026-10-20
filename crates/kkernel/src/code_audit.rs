//! `kkernel code-audit`: a read-only derived-report pipeline over a
//! dedicated code-map database (docs/adr/ADR-114-code-audit-derived-report.md).
//!
//! Phase 1 scope: structural signals computed with deterministic SQL over the
//! `code.ingest` L1/L1.5 facts already present in the map (modules now carry
//! path/current-revision provenance, but no per-file history timeline). This
//! command opens the map database read-only, never
//! writes to any graph, never creates `finding` notes, and never calls
//! `memory.remember`. Interpreting a signal as a defect is a separate,
//! human-approved step outside this pipeline.

use crate::sql::sql;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use khive_db::StorageBackend;
use khive_storage::{SqlReader, SqlStatement, SqlValue};

/// Arguments for `kkernel code-audit`.
#[derive(Parser, Debug)]
pub struct CodeAuditArgs {
    /// Path to the dedicated code-map database. Defaults to
    /// `<cwd>/.khive/code-map.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Path to a versioned audit policy TOML file (required — see
    /// `crates/kkernel/policy/code-audit-khive.toml` for the shape).
    #[arg(long)]
    pub policy: PathBuf,

    /// RFC3339 timestamp the report is generated as-of. Required for
    /// reproducible windows; never taken from the wall clock.
    #[arg(long = "as-of")]
    pub as_of: String,

    /// History lookback window in days. Accepted but unused in phase 1 —
    /// no file/commit history facts exist in the map yet, so every
    /// history-dependent signal reports `status: unavailable` regardless of
    /// this value.
    #[arg(long = "history-window-days", default_value_t = 180)]
    pub history_window_days: u32,

    /// Also evaluate dev-scope-only project edges in the layering signal
    /// (normal/build edges are always evaluated). Dependency-cycle
    /// detection always reports production, dev, and module-import graphs
    /// separately regardless of this flag. Policy completeness reporting
    /// (POLICY_INCOMPLETE) is unaffected by this flag — it evaluates the
    /// full in-scope project set, not just evaluated edges.
    #[arg(long = "include-dev-dependencies", default_value_t = false)]
    pub include_dev_dependencies: bool,

    /// Output directory. Defaults to `<cwd>/.khive/audits/code-audit/`.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Output format(s) to write.
    #[arg(long, value_enum, default_value = "both")]
    pub format: OutputFormat,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Md,
    Both,
}

/// Resolved, internal request — `CodeAuditArgs` after path defaulting and
/// `as_of` parsing (ADR-Q1 typed contract's `AuditRequest`).
#[derive(Debug, Clone)]
struct AuditRequest {
    map_db: PathBuf,
    policy_path: PathBuf,
    as_of: DateTime<Utc>,
    history_window_days: u32,
    include_dev_dependencies: bool,
}

/// The only `policy_version` this build understands. A policy file carrying
/// any other version (or none at all) is rejected outright rather than
/// silently interpreted under an assumed schema.
const SUPPORTED_POLICY_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    policy_version: u32,
    crate_ranks: BTreeMap<String, i64>,
    #[serde(default)]
    denied_pairs: Vec<(String, String)>,
    /// Minimum ingest-coverage ratio (`module_count / (module_count +
    /// unresolved_specifier_count)`, see `ingest_coverage_signals`) a project
    /// must meet before absence-based candidate signals (today:
    /// `zero_in_edge_module`)
    /// are reported for its modules. Below the floor, the candidate degrades to
    /// `unavailable` rather than being silently emitted or dropped.
    #[serde(default)]
    coverage_floor: f64,
}

impl Policy {
    fn rank(&self, crate_name: &str) -> Option<i64> {
        self.crate_ranks.get(crate_name).copied()
    }

    fn is_denied_same_band(&self, a: &str, b: &str) -> bool {
        self.denied_pairs
            .iter()
            .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    }
}

fn load_policy(path: &std::path::Path) -> Result<(Policy, Vec<u8>)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read policy file {}", path.display()))?;
    let policy: Policy =
        toml::from_str(std::str::from_utf8(&bytes).context("policy file is not valid UTF-8")?)
            .with_context(|| format!("parse policy file {}", path.display()))?;
    if policy.policy_version != SUPPORTED_POLICY_VERSION {
        anyhow::bail!(
            "policy file {} declares policy_version {}, but this build only supports \
             policy_version {SUPPORTED_POLICY_VERSION}",
            path.display(),
            policy.policy_version
        );
    }
    Ok((policy, bytes))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SignalStatus {
    Observed,
    Candidate,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
struct Signal {
    id: &'static str,
    subject_id: String,
    status: SignalStatus,
    value: Value,
    evidence_ids: Vec<String>,
    limitations: Vec<String>,
}

fn unavailable_signal(id: &'static str, reason: String) -> Signal {
    Signal {
        id,
        subject_id: "*".to_string(),
        status: SignalStatus::Unavailable,
        value: Value::Null,
        evidence_ids: vec![],
        limitations: vec![reason],
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum AuditErrorCode {
    SchemaUnsupported,
    PolicyIncomplete,
    HistoryAbsent,
    CoverageInsufficient,
}

#[derive(Debug, Clone, Serialize)]
struct AuditErrorEntry {
    code: AuditErrorCode,
    message: String,
}

/// The `AuditReport` typed contract (ADR-Q1).
#[derive(Debug, Serialize)]
pub struct AuditReport {
    schema_version: u32,
    policy_sha256: String,
    query_bundle_sha256: String,
    code_sweep_clocks: BTreeMap<String, BTreeMap<String, String>>,
    git_head_sha: Option<String>,
    git_history_complete: Option<bool>,
    as_of: String,
    signals: Vec<Signal>,
    errors: Vec<AuditErrorEntry>,
}

const SCHEMA_VERSION: u32 = 1;

const SQL_INGEST_COVERAGE: &str = sql!("code_audit_projects_ingest_coverage");
const SQL_MODULES_FOR_PROJECT: &str = sql!("code_audit_modules_for_project");
const SQL_FAN_IN: &str = sql!("code_audit_module_edges_fan_in");
const SQL_MODULE_NAMES: &str = sql!("code_audit_modules_list");
const SQL_PROJECT_EDGES: &str = sql!("code_audit_project_edges");

// `import` is the only evidence kind the import scanner emits; every other
// `dependency_kinds` value is a manifest declaration section name (Cargo
// sections, npm `devDependencies`/`peerDependencies`/`optionalDependencies`,
// Python `optional-dependencies:<group>` — the group suffix is open-ended,
// so declarations cannot be enumerated in a fixed allowlist).
const SQL_MANIFEST_IMPORT_MISMATCH: &str = sql!("code_audit_manifest_import_mismatches");
const SQL_MODULE_EDGES: &str = sql!("code_audit_module_edges");
const SQL_ZERO_IN_EDGE: &str = sql!("code_audit_modules_without_in_edges");
const SQL_DUPLICATE_CONTENT_HASH: &str = sql!("code_audit_modules_duplicate_content_hash");

/// Every SQL query template this command runs, in a fixed order — hashed
/// into `query_bundle_sha256` so a report can be tied back to the exact
/// derivation logic that produced it.
const QUERY_BUNDLE: &[&str] = &[
    SQL_INGEST_COVERAGE,
    SQL_MODULES_FOR_PROJECT,
    SQL_FAN_IN,
    SQL_MODULE_NAMES,
    SQL_PROJECT_EDGES,
    SQL_MANIFEST_IMPORT_MISMATCH,
    SQL_MODULE_EDGES,
    SQL_ZERO_IN_EDGE,
    SQL_DUPLICATE_CONTENT_HASH,
];

/// Entry point for `kkernel code-audit`.
pub async fn run_code_audit(args: CodeAuditArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let map_db = args
        .db
        .clone()
        .unwrap_or_else(|| cwd.join(".khive/code-map.db"));
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| cwd.join(".khive/audits/code-audit"));
    let as_of = DateTime::parse_from_rfc3339(&args.as_of)
        .with_context(|| format!("--as-of {:?} is not a valid RFC3339 timestamp", args.as_of))?
        .with_timezone(&Utc);

    let request = AuditRequest {
        map_db,
        policy_path: args.policy.clone(),
        as_of,
        history_window_days: args.history_window_days,
        include_dev_dependencies: args.include_dev_dependencies,
    };

    let report = generate_report(&request).await?;

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create output directory {}", out_dir.display()))?;
    if matches!(args.format, OutputFormat::Json | OutputFormat::Both) {
        let json_path = out_dir.join("report.json");
        std::fs::write(
            &json_path,
            serde_json::to_string_pretty(&report).context("serialize report as JSON")?,
        )
        .with_context(|| format!("write {}", json_path.display()))?;
        println!("wrote {}", json_path.display());
    }
    if matches!(args.format, OutputFormat::Md | OutputFormat::Both) {
        let md_path = out_dir.join("report.md");
        std::fs::write(&md_path, render_markdown(&report))
            .with_context(|| format!("write {}", md_path.display()))?;
        println!("wrote {}", md_path.display());
    }
    Ok(())
}

/// Core report generation, split out so tests can assert on the returned
/// [`AuditReport`] directly.
async fn generate_report(request: &AuditRequest) -> Result<AuditReport> {
    if !request.map_db.exists() {
        anyhow::bail!(
            "map database {} does not exist — run `code.ingest` first",
            request.map_db.display()
        );
    }
    let (policy, policy_bytes) = load_policy(&request.policy_path)?;
    let policy_sha256 = hex_sha256(&policy_bytes);
    let query_bundle_sha256 = query_bundle_sha256();

    let mut config = khive_runtime::RuntimeConfig {
        db_path: Some(request.map_db.clone()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let wal_ceiling = config.resolve_wal_ceiling_policy(true)?;
    let backend = StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
        &request.map_db,
        None,
        wal_ceiling,
    )
    .map_err(|e| {
        let msg = e.to_string();
        if msg.to_lowercase().contains("busy") || msg.to_lowercase().contains("locked") {
            anyhow::anyhow!("DB_BUSY: {msg}")
        } else {
            anyhow::anyhow!("{msg}")
        }
    })?;
    let sql = backend.sql();
    // A single reader connection is held for the entire report: every query
    // below runs sequentially against this one connection rather than a
    // fresh connection per query, so the report reflects one consistent
    // snapshot of the read-only map rather than interleaving with any
    // concurrent writer the map database might otherwise have.
    let mut reader = sql.reader().await.map_err(map_storage_busy)?;

    let caps = inspect_schema(reader.as_mut()).await?;

    let mut errors = Vec::new();
    for msg in &caps.schema_errors {
        errors.push(AuditErrorEntry {
            code: AuditErrorCode::SchemaUnsupported,
            message: msg.clone(),
        });
    }
    errors.push(AuditErrorEntry {
        code: AuditErrorCode::HistoryAbsent,
        message: format!(
            "no git/file-history facts exist in the phase-1 map schema; churn, dead-file, and \
             orphan-test signals report status=unavailable regardless of the requested \
             {}-day history window",
            request.history_window_days
        ),
    });

    let mut signals = Vec::new();
    let mut code_sweep_clocks = BTreeMap::new();
    let mut all_projects: BTreeSet<String> = BTreeSet::new();
    let mut coverage_by_project: BTreeMap<String, f64> = BTreeMap::new();

    if caps.entities_ok {
        let coverage = ingest_coverage_signals(
            reader.as_mut(),
            &mut signals,
            &mut code_sweep_clocks,
            &mut all_projects,
        )
        .await?;
        coverage_by_project = coverage;
        duplicate_content_hash_signals(reader.as_mut(), &mut signals).await?;
    } else {
        let reason = format!(
            "entities table capability unavailable: {}",
            caps.schema_errors.join("; ")
        );
        signals.push(unavailable_signal("ingest_coverage", reason.clone()));
        signals.push(unavailable_signal("duplicate_content_hash", reason));
    }

    // Base edge signals (`module_fan_in`, `zero_in_edge_module`) only read
    // `source_id`/`target_id`/`relation`/`deleted_at` — never `id` or
    // `metadata` — so they must stay available on a map missing only the
    // metadata column, or only the `id` column: both are real degraded-schema
    // scenarios seen in practice, and neither field is needed by these two
    // signals.
    if caps.entities_ok && caps.edges_base_ok {
        fan_in_signals(reader.as_mut(), &mut signals).await?;
        zero_in_edge_signals(
            reader.as_mut(),
            &policy,
            &coverage_by_project,
            &mut signals,
            &mut errors,
        )
        .await?;
    } else {
        let reason = format!(
            "graph_edges/entities base capability unavailable: {}",
            caps.schema_errors.join("; ")
        );
        for id in ["module_fan_in", "zero_in_edge_module"] {
            signals.push(unavailable_signal(id, reason.clone()));
        }
    }

    // `layering_violation`, `manifest_import_mismatch`, and
    // `dependency_cycle_summary` all read `graph_edges.id` and/or
    // `graph_edges.metadata` (dependency-kind classification), so they
    // additionally require both columns on top of the base edge columns.
    if caps.entities_ok && caps.edges_base_ok && caps.edges_metadata_ok {
        layering_signals(
            reader.as_mut(),
            &policy,
            request.include_dev_dependencies,
            &all_projects,
            &mut signals,
            &mut errors,
        )
        .await?;
        manifest_import_mismatch_signals(reader.as_mut(), &mut signals).await?;
        dependency_cycle_signals(reader.as_mut(), &mut signals).await?;
    } else {
        let reason = format!(
            "graph_edges.metadata capability unavailable: {}",
            caps.schema_errors.join("; ")
        );
        for id in [
            "layering_violation",
            "manifest_import_mismatch",
            "dependency_cycle_summary",
        ] {
            signals.push(unavailable_signal(id, reason.clone()));
        }
    }

    unavailable_history_signals(&mut signals);

    signals.sort_by(|a, b| (a.id, &a.subject_id).cmp(&(b.id, &b.subject_id)));
    errors.sort_by(|a, b| {
        (format!("{:?}", a.code), &a.message).cmp(&(format!("{:?}", b.code), &b.message))
    });

    Ok(AuditReport {
        schema_version: SCHEMA_VERSION,
        policy_sha256,
        query_bundle_sha256,
        code_sweep_clocks,
        git_head_sha: None,
        git_history_complete: None,
        as_of: request.as_of.to_rfc3339(),
        signals,
        errors,
    })
}

fn map_storage_busy(e: khive_storage::StorageError) -> anyhow::Error {
    let msg = e.to_string();
    if msg.to_lowercase().contains("busy") || msg.to_lowercase().contains("locked") {
        anyhow::anyhow!("DB_BUSY: {msg}")
    } else {
        anyhow::anyhow!("{msg}")
    }
}

/// What the open map database can support. Built once per report via
/// `PRAGMA table_info` so every derivation can be gated on real column
/// presence rather than aborting the whole report the first time an older
/// or partial map is missing a table or column.
///
/// `edges_ok` was originally a single table-wide boolean, but that over-
/// suppressed signals whose SQL never reads `graph_edges.metadata` on a map
/// missing only that column. Edge capability is
/// therefore split per the columns each signal group actually reads:
/// `edges_base_ok` covers exactly `source_id`/`target_id`/`relation`/
/// `deleted_at` — the columns `SQL_FAN_IN` and `SQL_ZERO_IN_EDGE` read, and
/// nothing more. `edges_metadata_ok` additionally covers `id` and
/// `metadata`, read only by the dependency-kind classification used in
/// layering, manifest/import mismatch, and the project-level dependency-
/// cycle graphs; `id` moved out of the base tier here because neither
/// `SQL_FAN_IN` nor `SQL_ZERO_IN_EDGE` reads `graph_edges.id`, so a map
/// missing only that column must not suppress them.
struct SchemaCaps {
    entities_ok: bool,
    edges_base_ok: bool,
    edges_metadata_ok: bool,
    schema_errors: Vec<String>,
}

const ENTITIES_REQUIRED_COLUMNS: &[&str] = &[
    "id",
    "name",
    "kind",
    "entity_type",
    "properties",
    "deleted_at",
];
const EDGES_BASE_REQUIRED_COLUMNS: &[&str] = &["source_id", "target_id", "relation", "deleted_at"];
const EDGES_METADATA_REQUIRED_COLUMNS: &[&str] = &["id", "metadata"];

async fn inspect_schema(reader: &mut dyn SqlReader) -> Result<SchemaCaps> {
    let table_rows = reader
        .query_all(SqlStatement {
            sql: sql!("schema_tables_list").to_string(),
            params: vec![],
            label: Some("code-audit table inventory".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    let tables: BTreeSet<String> = table_rows
        .iter()
        .filter_map(|r| r.text_or_none("name").map(str::to_owned))
        .collect();

    let mut schema_errors = Vec::new();
    let (entities_ok, entities_err) =
        check_table_capability(reader, &tables, "entities", ENTITIES_REQUIRED_COLUMNS).await?;
    if let Some(e) = entities_err {
        schema_errors.push(e);
    }

    let (edges_base_ok, edges_metadata_ok) = if !tables.contains("graph_edges") {
        schema_errors.push(
            "SCHEMA_UNSUPPORTED: table `graph_edges` is missing from the map database; every \
             signal requiring it reports status=unavailable"
                .to_string(),
        );
        (false, false)
    } else {
        let cols = table_columns(reader, "graph_edges").await?;
        let (base_ok, base_err) =
            missing_columns_error("graph_edges", &cols, EDGES_BASE_REQUIRED_COLUMNS);
        if let Some(e) = base_err {
            schema_errors.push(e);
        }
        let (metadata_ok, metadata_err) =
            missing_columns_error("graph_edges", &cols, EDGES_METADATA_REQUIRED_COLUMNS);
        if let Some(e) = metadata_err {
            schema_errors.push(e);
        }
        (base_ok, base_ok && metadata_ok)
    };

    Ok(SchemaCaps {
        entities_ok,
        edges_base_ok,
        edges_metadata_ok,
        schema_errors,
    })
}

async fn table_columns(reader: &mut dyn SqlReader, table: &str) -> Result<BTreeSet<String>> {
    let statement = match table {
        "entities" => sql!("schema_entities_columns"),
        "graph_edges" => sql!("schema_graph_edges_columns"),
        _ => anyhow::bail!("unsupported code-audit schema table {table:?}"),
    };
    let col_rows = reader
        .query_all(SqlStatement {
            sql: statement.to_string(),
            params: vec![],
            label: Some(format!("code-audit {table} column inventory")),
        })
        .await
        .map_err(map_storage_busy)?;
    Ok(col_rows
        .iter()
        .filter_map(|r| r.text_or_none("name").map(str::to_owned))
        .collect())
}

fn missing_columns_error(
    table: &str,
    cols: &BTreeSet<String>,
    required_cols: &[&str],
) -> (bool, Option<String>) {
    let missing: Vec<&str> = required_cols
        .iter()
        .filter(|c| !cols.contains(**c))
        .copied()
        .collect();
    if missing.is_empty() {
        (true, None)
    } else {
        (
            false,
            Some(format!(
                "SCHEMA_UNSUPPORTED: table `{table}` is missing required column(s) [{}]; every \
                 signal requiring them reports status=unavailable",
                missing.join(", ")
            )),
        )
    }
}

async fn check_table_capability(
    reader: &mut dyn SqlReader,
    tables: &BTreeSet<String>,
    table: &str,
    required_cols: &[&str],
) -> Result<(bool, Option<String>)> {
    if !tables.contains(table) {
        return Ok((
            false,
            Some(format!(
                "SCHEMA_UNSUPPORTED: table `{table}` is missing from the map database; every \
                 signal requiring it reports status=unavailable"
            )),
        ));
    }
    let cols = table_columns(reader, table).await?;
    Ok(missing_columns_error(table, &cols, required_cols))
}

fn json_col(row: &khive_storage::SqlRow, name: &str) -> Option<Value> {
    row.text_or_none(name)
        .and_then(|s| serde_json::from_str(s).ok())
}

fn unresolved_count(properties: &Value) -> usize {
    properties
        .get("unresolved_specifiers")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

async fn ingest_coverage_signals(
    reader: &mut dyn SqlReader,
    signals: &mut Vec<Signal>,
    code_sweep_clocks: &mut BTreeMap<String, BTreeMap<String, String>>,
    all_projects: &mut BTreeSet<String>,
) -> Result<BTreeMap<String, f64>> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_INGEST_COVERAGE.to_string(),
            params: vec![],
            label: Some("code-audit ingest coverage".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;

    let mut coverage_by_project: BTreeMap<String, f64> = BTreeMap::new();

    for row in &rows {
        let project_id = row.text_or_none("id").unwrap_or_default().to_owned();
        let project_name = row.text_or_none("name").unwrap_or_default().to_owned();
        let properties = json_col(row, "properties").unwrap_or(Value::Null);
        all_projects.insert(project_name.clone());

        // L1.5 records an unresolved intra-module import against the MODULE
        // entity, not the project (source_ingest.rs:776-784) — the project's
        // own `unresolved_specifiers` (manifest-level, if any) is only part
        // of the picture. Aggregate both so a project with unresolved module
        // imports is never reported as a false zero.
        let mut own_unresolved = unresolved_count(&properties);
        let mut unresolved_evidence: BTreeSet<String> = BTreeSet::new();

        let sweep_clock = properties
            .get("sweep_clock")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut languages: BTreeMap<String, String> = BTreeMap::new();
        for (lang, ts) in &sweep_clock {
            if let Some(ts) = ts.as_str() {
                languages.insert(lang.clone(), ts.to_string());
            }
        }
        if !languages.is_empty() {
            code_sweep_clocks.insert(project_name.clone(), languages);
        }

        let module_rows = reader
            .query_all(SqlStatement {
                sql: SQL_MODULES_FOR_PROJECT.to_string(),
                params: vec![SqlValue::Text(project_name.clone())],
                label: Some("code-audit modules for project".to_string()),
            })
            .await
            .map_err(map_storage_busy)?;
        let module_count = module_rows.len();
        for module_row in &module_rows {
            let module_id = module_row.text_or_none("id").unwrap_or_default().to_owned();
            let module_properties = json_col(module_row, "properties").unwrap_or(Value::Null);
            let n = unresolved_count(&module_properties);
            if n > 0 {
                own_unresolved += n;
                unresolved_evidence.insert(module_id);
            }
        }

        let denom = module_count + own_unresolved;
        let coverage_ratio = if denom == 0 {
            1.0
        } else {
            module_count as f64 / denom as f64
        };
        coverage_by_project.insert(project_name.clone(), coverage_ratio);

        let mut evidence_ids = vec![project_id];
        evidence_ids.extend(unresolved_evidence);

        signals.push(Signal {
            id: "ingest_coverage",
            subject_id: project_name,
            status: SignalStatus::Observed,
            value: json!({
                "module_count": module_count,
                "unresolved_specifier_count": own_unresolved,
                "coverage_ratio": coverage_ratio,
            }),
            evidence_ids,
            limitations: vec![
                "this schema_version=1 coverage ratio remains the legacy module-count / \
                 unresolved-reference proxy; ADR-085 Amendment 5's per-module \
                 import_specifier_count and unresolved_import_count remain available on the \
                 source module entities for consumers that need exact scan coverage"
                    .to_string(),
            ],
        });
    }
    Ok(coverage_by_project)
}

async fn fan_in_signals(reader: &mut dyn SqlReader, signals: &mut Vec<Signal>) -> Result<()> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_FAN_IN.to_string(),
            params: vec![],
            label: Some("code-audit fan-in edges".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;

    let mut importers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &rows {
        let source = row.text_or_none("source_id").unwrap_or_default().to_owned();
        let target = row.text_or_none("target_id").unwrap_or_default().to_owned();
        importers.entry(target).or_default().insert(source);
    }

    let module_rows = reader
        .query_all(SqlStatement {
            sql: SQL_MODULE_NAMES.to_string(),
            params: vec![],
            label: Some("code-audit module names".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    let names: BTreeMap<String, String> = module_rows
        .iter()
        .filter_map(|r| {
            Some((
                r.text_or_none("id")?.to_owned(),
                r.text_or_none("name")?.to_owned(),
            ))
        })
        .collect();

    for (target, sources) in &importers {
        let subject = names.get(target).cloned().unwrap_or_else(|| target.clone());
        signals.push(Signal {
            id: "module_fan_in",
            subject_id: subject,
            status: SignalStatus::Observed,
            value: json!({ "fan_in": sources.len() }),
            evidence_ids: sources.iter().cloned().collect(),
            limitations: vec![],
        });
    }
    Ok(())
}

async fn layering_signals(
    reader: &mut dyn SqlReader,
    policy: &Policy,
    include_dev: bool,
    all_projects: &BTreeSet<String>,
    signals: &mut Vec<Signal>,
    errors: &mut Vec<AuditErrorEntry>,
) -> Result<()> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_PROJECT_EDGES.to_string(),
            params: vec![],
            label: Some("code-audit layering edges".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;

    // POLICY_INCOMPLETE must be derived from the full in-scope project set,
    // not just the crates that happen to appear in a selected edge — a
    // project with no evaluated edge at all (e.g. no edges, or only a
    // dev-only edge with dev evaluation disabled) is still unmapped and must
    // still degrade rather than silently vanish.
    let unmapped: BTreeSet<String> = all_projects
        .iter()
        .filter(|name| policy.rank(name).is_none())
        .cloned()
        .collect();

    for row in &rows {
        let edge_id = row.text_or_none("id").unwrap_or_default().to_owned();
        let source_name = row
            .text_or_none("source_name")
            .unwrap_or_default()
            .to_owned();
        let target_name = row
            .text_or_none("target_name")
            .unwrap_or_default()
            .to_owned();
        let metadata = json_col(row, "metadata").unwrap_or(Value::Null);
        let scopes = dependency_scopes(&metadata);
        let is_dev_only = scopes.len() == 1 && scopes.contains("dev");
        // include_dev_dependencies gates VIOLATION EVALUATION only; policy
        // completeness (above) is independent of it.
        if is_dev_only && !include_dev {
            continue;
        }

        let (Some(sr), Some(tr)) = (policy.rank(&source_name), policy.rank(&target_name)) else {
            continue;
        };

        let violated =
            tr > sr || (tr == sr && policy.is_denied_same_band(&source_name, &target_name));
        if violated {
            signals.push(Signal {
                id: "layering_violation",
                subject_id: format!("{source_name}->{target_name}"),
                status: SignalStatus::Observed,
                value: json!({
                    "source": source_name,
                    "target": target_name,
                    "source_rank": sr,
                    "target_rank": tr,
                    "dependency_kind": if is_dev_only { "dev" } else { "production" },
                }),
                evidence_ids: vec![edge_id],
                limitations: vec![],
            });
        }
    }

    for crate_name in &unmapped {
        signals.push(Signal {
            id: "layering_violation",
            subject_id: crate_name.clone(),
            status: SignalStatus::Unavailable,
            value: Value::Null,
            evidence_ids: vec![],
            limitations: vec![format!(
                "crate {crate_name:?} is absent from the policy crate_ranks table \
                 (POLICY_INCOMPLETE); layering cannot be evaluated for its edges"
            )],
        });
    }
    if !unmapped.is_empty() {
        errors.push(AuditErrorEntry {
            code: AuditErrorCode::PolicyIncomplete,
            message: format!(
                "{} crate(s) absent from policy crate_ranks: {}",
                unmapped.len(),
                unmapped.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    Ok(())
}

async fn manifest_import_mismatch_signals(
    reader: &mut dyn SqlReader,
    signals: &mut Vec<Signal>,
) -> Result<()> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_MANIFEST_IMPORT_MISMATCH.to_string(),
            params: vec![],
            label: Some("code-audit manifest/import mismatch".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    for row in &rows {
        let edge_id = row.text_or_none("id").unwrap_or_default().to_owned();
        let source_name = row
            .text_or_none("source_name")
            .unwrap_or_default()
            .to_owned();
        let target_name = row
            .text_or_none("target_name")
            .unwrap_or_default()
            .to_owned();
        signals.push(Signal {
            id: "manifest_import_mismatch",
            subject_id: format!("{source_name}->{target_name}"),
            status: SignalStatus::Observed,
            value: json!({ "source": source_name, "target": target_name }),
            evidence_ids: vec![edge_id],
            limitations: vec![],
        });
    }
    Ok(())
}

/// One directed edge in a dependency graph, used by the Tarjan SCC pass.
/// Node identity is the map's `source_id`/`target_id` (entity UUIDs) rather
/// than display names — module names are not unique across languages
/// (ADR-085 §"module identity"), so keying the graph by name can silently
/// merge two distinct module graphs into one false SCC. Field order
/// matches the required tie-break sort `(source_id, target_id, edge_id)`.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct DepEdge {
    source_id: String,
    target_id: String,
    id: String,
}

fn dependency_scopes(metadata: &Value) -> BTreeSet<String> {
    let scopes: BTreeSet<String> = metadata
        .get("dependency_scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|scope| matches!(*scope, "normal" | "dev" | "build"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !scopes.is_empty() {
        return scopes;
    }

    let kinds: Vec<&str> = metadata
        .get("dependency_kinds")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let declared: BTreeSet<String> = kinds
        .iter()
        .filter(|kind| **kind != "import")
        .map(|kind| match *kind {
            "dev-dependencies" | "devDependencies" => "dev",
            "build-dependencies" => "build",
            _ => "normal",
        })
        .map(str::to_string)
        .collect();
    if !declared.is_empty() {
        declared
    } else if kinds.contains(&"import") {
        ["build".to_string()].into_iter().collect()
    } else {
        BTreeSet::new()
    }
}

async fn dependency_cycle_signals(
    reader: &mut dyn SqlReader,
    signals: &mut Vec<Signal>,
) -> Result<()> {
    let project_rows = reader
        .query_all(SqlStatement {
            sql: SQL_PROJECT_EDGES.to_string(),
            params: vec![],
            label: Some("code-audit project depends_on edges".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;

    let mut node_names: BTreeMap<String, String> = BTreeMap::new();
    let mut prod_edges = Vec::new();
    let mut dev_edges = Vec::new();
    for row in &project_rows {
        let edge_id = row.text_or_none("id").unwrap_or_default().to_owned();
        let source_id = row.text_or_none("source_id").unwrap_or_default().to_owned();
        let target_id = row.text_or_none("target_id").unwrap_or_default().to_owned();
        let source_name = row
            .text_or_none("source_name")
            .unwrap_or_default()
            .to_owned();
        let target_name = row
            .text_or_none("target_name")
            .unwrap_or_default()
            .to_owned();
        node_names.insert(source_id.clone(), source_name);
        node_names.insert(target_id.clone(), target_name);
        let metadata = json_col(row, "metadata").unwrap_or(Value::Null);
        let scopes = dependency_scopes(&metadata);
        let edge = DepEdge {
            id: edge_id,
            source_id,
            target_id,
        };
        let is_dev_only = scopes.len() == 1 && scopes.contains("dev");
        if is_dev_only {
            dev_edges.push(edge);
        } else {
            prod_edges.push(edge);
        }
    }

    let module_rows = reader
        .query_all(SqlStatement {
            sql: SQL_MODULE_EDGES.to_string(),
            params: vec![],
            label: Some("code-audit module depends_on edges".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    let module_edges: Vec<DepEdge> = module_rows
        .iter()
        .map(|row| {
            let source_id = row.text_or_none("source_id").unwrap_or_default().to_owned();
            let target_id = row.text_or_none("target_id").unwrap_or_default().to_owned();
            node_names.insert(
                source_id.clone(),
                row.text_or_none("source_name")
                    .unwrap_or_default()
                    .to_owned(),
            );
            node_names.insert(
                target_id.clone(),
                row.text_or_none("target_name")
                    .unwrap_or_default()
                    .to_owned(),
            );
            DepEdge {
                id: row.text_or_none("id").unwrap_or_default().to_owned(),
                source_id,
                target_id,
            }
        })
        .collect();

    for (graph_label, edges) in [
        ("project_production", prod_edges),
        ("project_dev", dev_edges),
        ("module_import", module_edges),
    ] {
        let sccs = tarjan_sccs(&edges);
        signals.push(Signal {
            id: "dependency_cycle_summary",
            subject_id: graph_label.to_string(),
            status: SignalStatus::Observed,
            value: json!({ "graph": graph_label, "cycle_count": sccs.len() }),
            evidence_ids: vec![],
            limitations: vec![],
        });
        for members in sccs {
            let evidence_ids: Vec<String> = edges
                .iter()
                .filter(|e| members.contains(&e.source_id) && members.contains(&e.target_id))
                .map(|e| e.id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let member_details: Vec<Value> = members
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "name": node_names.get(id).cloned().unwrap_or_default(),
                    })
                })
                .collect();
            signals.push(Signal {
                id: "dependency_cycle",
                subject_id: format!("{graph_label}:{}", members.join(",")),
                status: SignalStatus::Observed,
                value: json!({ "graph": graph_label, "members": member_details }),
                evidence_ids,
                limitations: vec![],
            });
        }
    }
    Ok(())
}

/// Deterministic Tarjan strongly-connected-components pass over `edges`,
/// sorted by `(source_id, target_id, edge_id)` first (ADR-Q1 §"Import
/// cycles"). Node identity is the map entity ID (see [`DepEdge`]), never a
/// display name. Returns each SCC of size > 1, plus single-node self-loops,
/// as a sorted member-ID list; SCCs themselves are returned in sorted order
/// of their first member.
fn tarjan_sccs(edges: &[DepEdge]) -> Vec<Vec<String>> {
    let mut sorted_edges = edges.to_vec();
    sorted_edges.sort();

    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut nodes: BTreeSet<String> = BTreeSet::new();
    for e in &sorted_edges {
        nodes.insert(e.source_id.clone());
        nodes.insert(e.target_id.clone());
        adjacency
            .entry(e.source_id.clone())
            .or_default()
            .insert(e.target_id.clone());
    }

    let mut index_counter = 0usize;
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: BTreeSet<String> = BTreeSet::new();
    let mut indices: BTreeMap<String, usize> = BTreeMap::new();
    let mut lowlink: BTreeMap<String, usize> = BTreeMap::new();
    let mut result: Vec<Vec<String>> = Vec::new();

    // Iterative Tarjan to avoid recursion depth concerns on larger graphs.
    for start in nodes.iter().cloned().collect::<Vec<_>>() {
        if indices.contains_key(&start) {
            continue;
        }
        let mut work: Vec<(String, usize)> = vec![(start, 0)];
        while let Some((node, child_idx)) = work.pop() {
            if child_idx == 0 {
                indices.insert(node.clone(), index_counter);
                lowlink.insert(node.clone(), index_counter);
                index_counter += 1;
                stack.push(node.clone());
                on_stack.insert(node.clone());
            }
            let neighbors: Vec<String> = adjacency
                .get(&node)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            if child_idx < neighbors.len() {
                work.push((node.clone(), child_idx + 1));
                let next = neighbors[child_idx].clone();
                if !indices.contains_key(&next) {
                    work.push((next, 0));
                } else if on_stack.contains(&next) {
                    let next_idx = indices[&next];
                    let cur_low = lowlink[&node];
                    lowlink.insert(node.clone(), cur_low.min(next_idx));
                }
                continue;
            }
            // Finished exploring `node`'s children: propagate lowlink to
            // whichever frame pushed it, then possibly pop an SCC.
            if let Some((parent, _)) = work.last() {
                let node_low = lowlink[&node];
                let parent_low = lowlink[parent];
                lowlink.insert(parent.clone(), parent_low.min(node_low));
            }
            if lowlink[&node] == indices[&node] {
                let mut members = Vec::new();
                loop {
                    let w = stack.pop().expect("stack non-empty while popping SCC");
                    on_stack.remove(&w);
                    members.push(w.clone());
                    if w == node {
                        break;
                    }
                }
                let is_self_loop = members.len() == 1
                    && adjacency
                        .get(&members[0])
                        .is_some_and(|s| s.contains(&members[0]));
                if members.len() > 1 || is_self_loop {
                    members.sort();
                    result.push(members);
                }
            }
        }
    }
    result.sort();
    result
}

async fn zero_in_edge_signals(
    reader: &mut dyn SqlReader,
    policy: &Policy,
    coverage_by_project: &BTreeMap<String, f64>,
    signals: &mut Vec<Signal>,
    errors: &mut Vec<AuditErrorEntry>,
) -> Result<()> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_ZERO_IN_EDGE.to_string(),
            params: vec![],
            label: Some("code-audit zero-in-edge modules".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    let mut coverage_gated: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        let module_id = row.text_or_none("id").unwrap_or_default().to_owned();
        let module_name = row.text_or_none("name").unwrap_or_default().to_owned();
        let source_project = row.text_or_none("source_project").map(str::to_owned);
        let coverage = source_project
            .as_deref()
            .and_then(|p| coverage_by_project.get(p))
            .copied();
        let below_floor = coverage.map(|c| c < policy.coverage_floor).unwrap_or(false);

        let mut limitations = vec![
            "absence of an observed import in-edge is not proof of dead code; L1.5 \
             regex-based import resolution has known false-negative gaps"
                .to_string(),
        ];
        let status = if below_floor {
            if let Some(p) = &source_project {
                coverage_gated.insert(p.clone());
            }
            limitations.push(format!(
                "ingest coverage for project {:?} ({:.3}) is below the policy coverage_floor \
                 ({:.3}); this candidate is suppressed to unavailable rather than reported \
                 (COVERAGE_INSUFFICIENT)",
                source_project.clone().unwrap_or_default(),
                coverage.unwrap_or(0.0),
                policy.coverage_floor,
            ));
            SignalStatus::Unavailable
        } else {
            SignalStatus::Candidate
        };

        // Module identity, not display name, is the subject: two modules in
        // different languages can share a display name.
        signals.push(Signal {
            id: "zero_in_edge_module",
            subject_id: module_id.clone(),
            status,
            value: json!({ "module_id": module_id, "name": module_name }),
            evidence_ids: vec![module_id],
            limitations,
        });
    }
    if !coverage_gated.is_empty() {
        errors.push(AuditErrorEntry {
            code: AuditErrorCode::CoverageInsufficient,
            message: format!(
                "{} project(s) below policy coverage_floor ({:.3}); zero_in_edge candidates \
                 suppressed to unavailable: {}",
                coverage_gated.len(),
                policy.coverage_floor,
                coverage_gated
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(())
}

async fn duplicate_content_hash_signals(
    reader: &mut dyn SqlReader,
    signals: &mut Vec<Signal>,
) -> Result<()> {
    let rows = reader
        .query_all(SqlStatement {
            sql: SQL_DUPLICATE_CONTENT_HASH.to_string(),
            params: vec![],
            label: Some("code-audit duplicate content hash".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    for row in &rows {
        let hash = row
            .text_or_none("content_hash")
            .unwrap_or_default()
            .to_owned();
        let ids_csv = row.text_or_none("ids").unwrap_or_default().to_owned();
        let mut module_ids: Vec<String> = ids_csv.split(',').map(str::to_string).collect();
        module_ids.sort();
        signals.push(Signal {
            id: "duplicate_content_hash",
            subject_id: hash.clone(),
            status: SignalStatus::Candidate,
            value: json!({ "content_hash": hash, "module_ids": module_ids.clone() }),
            evidence_ids: module_ids,
            limitations: vec![
                "FNV-1a content hash is not collision-proof and does not distinguish \
                 generated, fixture, or vendored code from originals in phase 1"
                    .to_string(),
            ],
        });
    }
    Ok(())
}

fn unavailable_history_signals(signals: &mut Vec<Signal>) {
    for (id, reason) in [
        (
            "churn_hotspot",
            "requires per-file revision facts (additions/deletions, commit time) that the \
             phase-1 map schema does not persist",
        ),
        (
            "dead_file",
            "requires durable file identity and path bindings across revisions; the current \
             module source_path/source_revision observation does not encode file lifecycle \
             history",
        ),
        (
            "orphan_test_file",
            "requires test-entrypoint/collector classification and per-file production \
             dependency edges that the phase-1 map schema does not persist",
        ),
    ] {
        signals.push(Signal {
            id,
            subject_id: "*".to_string(),
            status: SignalStatus::Unavailable,
            value: Value::Null,
            evidence_ids: vec![],
            limitations: vec![reason.to_string()],
        });
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn query_bundle_sha256() -> String {
    let bundle = format!("\n{}\n", QUERY_BUNDLE.join("\n\n"));
    hex_sha256(bundle.as_bytes())
}

fn render_markdown(report: &AuditReport) -> String {
    let mut out = String::new();
    out.push_str("# kkernel code-audit report\n\n");
    out.push_str(&format!("- schema_version: {}\n", report.schema_version));
    out.push_str(&format!("- as_of: {}\n", report.as_of));
    out.push_str(&format!("- policy_sha256: {}\n", report.policy_sha256));
    out.push_str(&format!(
        "- query_bundle_sha256: {}\n\n",
        report.query_bundle_sha256
    ));

    if !report.errors.is_empty() {
        out.push_str("## Errors\n\n");
        for err in &report.errors {
            out.push_str(&format!("- **{:?}**: {}\n", err.code, err.message));
        }
        out.push('\n');
    }

    out.push_str("## Signals\n\n");
    out.push_str("| id | subject | status | evidence |\n");
    out.push_str("|---|---|---|---|\n");
    for signal in &report.signals {
        out.push_str(&format!(
            "| {} | {} | {:?} | {} |\n",
            signal.id,
            signal.subject_id,
            signal.status,
            signal.evidence_ids.len()
        ));
    }
    out
}

#[cfg(test)]
#[path = "code_audit_tests.rs"]
mod tests;
