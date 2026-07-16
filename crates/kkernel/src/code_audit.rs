//! `kkernel code-audit`: a read-only derived-report pipeline over a
//! dedicated code-map database (docs/adr/ADR-114-code-audit-derived-report.md,
//! ADR-Q1/Q2 in `.khive/workspaces/20260716/code-quality-graph/DESIGN.md`).
//!
//! Phase 1 scope: structural signals computed with deterministic SQL over the
//! `code.ingest` L1/L1.5 facts already present in the map (no file/history
//! facts exist yet). This command opens the map database read-only, never
//! writes to any graph, never creates `finding` notes, and never calls
//! `memory.remember`. Interpreting a signal as a defect is a separate,
//! human-approved step outside this pipeline.

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

    /// Also evaluate `dev-dependencies`-only project edges in the layering
    /// signal (production edges are always evaluated). Dependency-cycle
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
/// any other version (or none at all) is rejected outright — see M1 in
/// `.khive/codex_reviews/codex_review_pr1052.md`.
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
    /// `unavailable` rather than being silently emitted or dropped (M1).
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

// Every SQL query template this command runs against the read-only map
// database. Kept as Rust string constants rather than `.sql` files: the
// workspace's `lint-sql.sh` treats every `.sql` file outside the
// `khive-db` migration chain as a standalone DDL fragment it replays into a
// fresh in-memory database, which rejects query-only SQL that assumes an
// existing `entities`/`graph_edges` schema.

const SQL_PROJECT_NAMES: &str = "
SELECT id, name FROM entities
WHERE kind = 'project' AND deleted_at IS NULL
ORDER BY name;
";

const SQL_INGEST_COVERAGE: &str = "
SELECT id, name, properties FROM entities
WHERE kind = 'project' AND deleted_at IS NULL
ORDER BY name;
";

const SQL_MODULES_FOR_PROJECT: &str = "
SELECT id, properties FROM entities
WHERE entity_type = 'module' AND deleted_at IS NULL
  AND json_extract(properties,'$.source_project') = ?1
ORDER BY id;
";

const SQL_FAN_IN: &str = "
SELECT e.source_id AS source_id, e.target_id AS target_id
FROM graph_edges e
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL AND t.entity_type = 'module'
ORDER BY e.target_id, e.source_id;
";

const SQL_MODULE_NAMES: &str = "
SELECT id, name FROM entities WHERE entity_type = 'module' AND deleted_at IS NULL;
";

const SQL_PROJECT_EDGES: &str = "
SELECT e.id AS id, e.source_id AS source_id, e.target_id AS target_id,
       s.name AS source_name, t.name AS target_name, e.metadata AS metadata
FROM graph_edges e
JOIN entities s ON s.id = e.source_id
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL
  AND s.kind = 'project' AND t.kind = 'project'
ORDER BY e.source_id, e.target_id, e.id;
";

const SQL_MANIFEST_IMPORT_MISMATCH: &str = "
SELECT e.id AS id, s.name AS source_name, t.name AS target_name
FROM graph_edges e
JOIN entities s ON s.id = e.source_id
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL
  AND EXISTS (
    SELECT 1 FROM json_each(e.metadata, '$.dependency_kinds') WHERE value = 'import'
  )
  AND NOT EXISTS (
    SELECT 1 FROM json_each(e.metadata, '$.dependency_kinds')
    WHERE value IN ('dependencies', 'build-dependencies', 'dev-dependencies')
  )
ORDER BY e.source_id, e.target_id, e.id;
";

const SQL_MODULE_EDGES: &str = "
SELECT e.id AS id, e.source_id AS source_id, e.target_id AS target_id,
       s.name AS source_name, t.name AS target_name
FROM graph_edges e
JOIN entities s ON s.id = e.source_id
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL
  AND s.entity_type = 'module' AND t.entity_type = 'module'
ORDER BY e.source_id, e.target_id, e.id;
";

const SQL_ZERO_IN_EDGE: &str = "
SELECT m.id AS id, m.name AS name,
       json_extract(m.properties,'$.source_project') AS source_project
FROM entities m
WHERE m.entity_type = 'module' AND m.deleted_at IS NULL
  AND NOT EXISTS (
    SELECT 1 FROM graph_edges e
    WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL AND e.target_id = m.id
  )
ORDER BY m.name, m.id;
";

const SQL_DUPLICATE_CONTENT_HASH: &str = "
SELECT json_extract(properties, '$.content_hash') AS content_hash,
       group_concat(id) AS ids,
       count(*) AS c
FROM entities
WHERE entity_type = 'module' AND deleted_at IS NULL
  AND json_extract(properties, '$.content_hash') IS NOT NULL
GROUP BY content_hash
HAVING c > 1
ORDER BY content_hash;
";

/// Every SQL query template this command runs, in a fixed order — hashed
/// into `query_bundle_sha256` so a report can be tied back to the exact
/// derivation logic that produced it.
const QUERY_BUNDLE: &[&str] = &[
    SQL_PROJECT_NAMES,
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
    let query_bundle_sha256 = hex_sha256(QUERY_BUNDLE.concat().as_bytes());

    let backend = StorageBackend::sqlite_read_only(&request.map_db).map_err(|e| {
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
    // `source_id`/`target_id`/`relation`/`deleted_at` — never `metadata` —
    // so they must stay available on a map missing only the metadata column
    // (codex round-2 Medium, `.khive/codex_reviews/codex_review_pr1052_round2.md`).
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
    // `dependency_cycle_summary` all read `graph_edges.metadata`
    // (dependency-kind classification), so they additionally require the
    // metadata column on top of the base edge columns.
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
/// or partial map is missing a table or column (H1,
/// `.khive/codex_reviews/codex_review_pr1052.md`).
///
/// `edges_ok` was originally a single table-wide boolean, but that over-
/// suppressed signals whose SQL never reads `graph_edges.metadata` on a map
/// missing only that column (round-2 Medium,
/// `.khive/codex_reviews/codex_review_pr1052_round2.md`). Edge capability is
/// therefore split per the columns each signal group actually reads:
/// `edges_base_ok` covers `id`/`source_id`/`target_id`/`relation`/
/// `deleted_at` (read by every edge-derived signal), `edges_metadata_ok`
/// additionally covers `metadata` (read only by the dependency-kind
/// classification used in layering, manifest/import mismatch, and the
/// project-level dependency-cycle graphs).
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
const EDGES_BASE_REQUIRED_COLUMNS: &[&str] =
    &["id", "source_id", "target_id", "relation", "deleted_at"];
const EDGES_METADATA_REQUIRED_COLUMNS: &[&str] = &["metadata"];

async fn inspect_schema(reader: &mut dyn SqlReader) -> Result<SchemaCaps> {
    let table_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name FROM sqlite_master WHERE type='table'".to_string(),
            params: vec![],
            label: Some("code-audit table inventory".to_string()),
        })
        .await
        .map_err(map_storage_busy)?;
    let tables: BTreeSet<String> = table_rows
        .iter()
        .filter_map(|r| text_col(r, "name"))
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
    let col_rows = reader
        .query_all(SqlStatement {
            sql: format!("PRAGMA table_info({table})"),
            params: vec![],
            label: Some(format!("code-audit {table} column inventory")),
        })
        .await
        .map_err(map_storage_busy)?;
    Ok(col_rows
        .iter()
        .filter_map(|r| text_col(r, "name"))
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

fn text_col(row: &khive_storage::SqlRow, name: &str) -> Option<String> {
    match row.get(name) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn json_col(row: &khive_storage::SqlRow, name: &str) -> Option<Value> {
    text_col(row, name).and_then(|s| serde_json::from_str(&s).ok())
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
        let project_id = text_col(row, "id").unwrap_or_default();
        let project_name = text_col(row, "name").unwrap_or_default();
        let properties = json_col(row, "properties").unwrap_or(Value::Null);
        all_projects.insert(project_name.clone());

        // L1.5 records an unresolved intra-module import against the MODULE
        // entity, not the project (source_ingest.rs:776-784) — the project's
        // own `unresolved_specifiers` (manifest-level, if any) is only part
        // of the picture. Aggregate both so a project with unresolved module
        // imports is never reported as a false zero (H2).
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
            let module_id = text_col(module_row, "id").unwrap_or_default();
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
                "total observed import count is not reconstructable in the phase-1 map \
                 schema: resolved specifiers are folded into depends_on edge metadata rather \
                 than counted individually, so only unresolved-specifier and module counts \
                 are available"
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
        let source = text_col(row, "source_id").unwrap_or_default();
        let target = text_col(row, "target_id").unwrap_or_default();
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
        .filter_map(|r| Some((text_col(r, "id")?, text_col(r, "name")?)))
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
    // still degrade rather than silently vanish (H4).
    let unmapped: BTreeSet<String> = all_projects
        .iter()
        .filter(|name| policy.rank(name).is_none())
        .cloned()
        .collect();

    for row in &rows {
        let edge_id = text_col(row, "id").unwrap_or_default();
        let source_name = text_col(row, "source_name").unwrap_or_default();
        let target_name = text_col(row, "target_name").unwrap_or_default();
        let metadata = json_col(row, "metadata").unwrap_or(Value::Null);
        let kinds: BTreeSet<String> = metadata
            .get("dependency_kinds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let is_dev_only = kinds.contains("dev-dependencies")
            && !kinds.contains("dependencies")
            && !kinds.contains("build-dependencies");
        // include_dev_dependencies gates VIOLATION EVALUATION only; policy
        // completeness (above) is independent of it (H4).
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
        let edge_id = text_col(row, "id").unwrap_or_default();
        let source_name = text_col(row, "source_name").unwrap_or_default();
        let target_name = text_col(row, "target_name").unwrap_or_default();
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
/// merge two distinct module graphs into one false SCC (H3). Field order
/// matches the required tie-break sort `(source_id, target_id, edge_id)`.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct DepEdge {
    source_id: String,
    target_id: String,
    id: String,
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
        let edge_id = text_col(row, "id").unwrap_or_default();
        let source_id = text_col(row, "source_id").unwrap_or_default();
        let target_id = text_col(row, "target_id").unwrap_or_default();
        let source_name = text_col(row, "source_name").unwrap_or_default();
        let target_name = text_col(row, "target_name").unwrap_or_default();
        node_names.insert(source_id.clone(), source_name);
        node_names.insert(target_id.clone(), target_name);
        let metadata = json_col(row, "metadata").unwrap_or(Value::Null);
        let kinds: BTreeSet<String> = metadata
            .get("dependency_kinds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let edge = DepEdge {
            id: edge_id,
            source_id,
            target_id,
        };
        let is_dev_only = kinds.contains("dev-dependencies")
            && !kinds.contains("dependencies")
            && !kinds.contains("build-dependencies");
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
            let source_id = text_col(row, "source_id").unwrap_or_default();
            let target_id = text_col(row, "target_id").unwrap_or_default();
            node_names.insert(
                source_id.clone(),
                text_col(row, "source_name").unwrap_or_default(),
            );
            node_names.insert(
                target_id.clone(),
                text_col(row, "target_name").unwrap_or_default(),
            );
            DepEdge {
                id: text_col(row, "id").unwrap_or_default(),
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
        let module_id = text_col(row, "id").unwrap_or_default();
        let module_name = text_col(row, "name").unwrap_or_default();
        let source_project = text_col(row, "source_project");
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
        // different languages can share a display name (H3).
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
        let hash = text_col(row, "content_hash").unwrap_or_default();
        let ids_csv = text_col(row, "ids").unwrap_or_default();
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
            "requires durable file identity (FileIdentity/PathBinding) to distinguish a \
             file from a module-path projection; not available until an ADR-085/088 \
             history-join amendment lands",
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
mod tests {
    use super::*;

    use std::path::Path;

    use khive_storage::{SqlWriter, StorageResult};

    async fn seed(
        writer: &mut dyn SqlWriter,
        id: &str,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        properties: Value,
    ) -> StorageResult<()> {
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO entities (id, namespace, kind, name, properties, tags, \
                      created_at, updated_at, entity_type) \
                      VALUES (?1, 'local', ?2, ?3, ?4, '[]', 0, 0, ?5)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(kind.to_string()),
                    SqlValue::Text(name.to_string()),
                    SqlValue::Text(properties.to_string()),
                    match entity_type {
                        Some(t) => SqlValue::Text(t.to_string()),
                        None => SqlValue::Null,
                    },
                ],
                label: None,
            })
            .await?;
        Ok(())
    }

    async fn seed_edge(
        writer: &mut dyn SqlWriter,
        id: &str,
        source: &str,
        target: &str,
        metadata: Value,
    ) -> StorageResult<()> {
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO graph_edges (namespace, id, source_id, target_id, relation, \
                      created_at, updated_at, metadata) \
                      VALUES ('local', ?1, ?2, ?3, 'depends_on', 0, 0, ?4)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(source.to_string()),
                    SqlValue::Text(target.to_string()),
                    SqlValue::Text(metadata.to_string()),
                ],
                label: None,
            })
            .await?;
        Ok(())
    }

    /// Builds a fixture map database at `path` with a small, deterministic
    /// project/module graph: three projects (`crate-a` depends_on `crate-b`,
    /// declared + observed; a policy-unmapped `crate-z` reachable only by a
    /// dev edge; a policy-unmapped `crate-orphan` reachable by NO edge at
    /// all), four modules (a self-import cycle between `crate_a::lib`/
    /// `crate_a::util`, a zero-in-edge/duplicate-hash `crate_a::orphan`
    /// carrying an unresolved import, and a same-named-but-different-
    /// language `crate_a::lib` in python with no edges at all).
    async fn build_fixture(path: &Path, edge_insert_order: [usize; 2]) {
        let backend = StorageBackend::sqlite(path).expect("open fixture backend");
        {
            let mut writer = backend.pool().writer().expect("writer guard");
            khive_db::run_migrations(writer.conn_mut()).expect("run core migrations");
        }
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("sql writer");

        seed(
            writer.as_mut(),
            "11111111-1111-1111-1111-111111111111",
            "project",
            None,
            "crate-a",
            json!({"source_project": "crate-a", "last_seen_at": "2026-07-16T00:00:00Z"}),
        )
        .await
        .unwrap();
        seed(
            writer.as_mut(),
            "22222222-2222-2222-2222-222222222222",
            "project",
            None,
            "crate-b",
            json!({"source_project": "crate-b", "last_seen_at": "2026-07-16T00:00:00Z"}),
        )
        .await
        .unwrap();
        seed(
            writer.as_mut(),
            "33333333-3333-3333-3333-333333333333",
            "project",
            None,
            "crate-z",
            json!({"source_project": "crate-z", "last_seen_at": "2026-07-16T00:00:00Z"}),
        )
        .await
        .unwrap();
        // H4: a project reachable by NO edge at all — must still degrade to
        // unavailable + POLICY_INCOMPLETE, not silently vanish.
        seed(
            writer.as_mut(),
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "project",
            None,
            "crate-orphan",
            json!({"source_project": "crate-orphan", "last_seen_at": "2026-07-16T00:00:00Z"}),
        )
        .await
        .unwrap();

        seed_edge(
            writer.as_mut(),
            "44444444-4444-4444-4444-444444444444",
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
            json!({"dependency_kinds": ["dependencies"]}),
        )
        .await
        .unwrap();
        // crate-b -> crate-z (dev-only): crate-z absent from policy.
        seed_edge(
            writer.as_mut(),
            "55555555-5555-5555-5555-555555555555",
            "22222222-2222-2222-2222-222222222222",
            "33333333-3333-3333-3333-333333333333",
            json!({"dependency_kinds": ["dev-dependencies"]}),
        )
        .await
        .unwrap();
        // crate-b -> crate-a (production): the correct top-down direction
        // (rank 1 -> rank 0), so layering does NOT flag it — but together
        // with the first edge (crate-a -> crate-b, the violating direction)
        // it forms a project-level production cycle.
        seed_edge(
            writer.as_mut(),
            "66666666-6666-6666-6666-666666666666",
            "22222222-2222-2222-2222-222222222222",
            "11111111-1111-1111-1111-111111111111",
            json!({"dependency_kinds": ["dependencies"]}),
        )
        .await
        .unwrap();

        seed(
            writer.as_mut(),
            "77777777-7777-7777-7777-777777777777",
            "concept",
            Some("module"),
            "crate_a::lib",
            json!({
                "source_project": "crate-a",
                "language": "rust",
                "module_path": "crate_a::lib",
                "content_hash": "deadbeef",
            }),
        )
        .await
        .unwrap();
        seed(
            writer.as_mut(),
            "88888888-8888-8888-8888-888888888888",
            "concept",
            Some("module"),
            "crate_a::util",
            json!({
                "source_project": "crate-a",
                "language": "rust",
                "module_path": "crate_a::util",
                "content_hash": "deadbeef",
            }),
        )
        .await
        .unwrap();
        seed(
            writer.as_mut(),
            "99999999-9999-9999-9999-999999999999",
            "concept",
            Some("module"),
            "crate_a::orphan",
            json!({
                "source_project": "crate-a",
                "language": "rust",
                "module_path": "crate_a::orphan",
                "content_hash": "cafef00d",
                "unresolved_specifiers": ["crate_a::missing"],
            }),
        )
        .await
        .unwrap();
        // H3: a SECOND module named identically to `crate_a::lib`, but a
        // distinct language/entity — must not be collapsed into the rust
        // module's Tarjan node or zero-in-edge subject by name collision.
        seed(
            writer.as_mut(),
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
            "concept",
            Some("module"),
            "crate_a::lib",
            json!({
                "source_project": "crate-a",
                "language": "python",
                "module_path": "crate_a::lib",
                "content_hash": "beefcafe",
            }),
        )
        .await
        .unwrap();

        let module_edges = [
            (
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "77777777-7777-7777-7777-777777777777",
                "88888888-8888-8888-8888-888888888888",
            ),
            (
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "88888888-8888-8888-8888-888888888888",
                "77777777-7777-7777-7777-777777777777",
            ),
        ];
        for idx in edge_insert_order {
            let (id, source, target) = module_edges[idx];
            seed_edge(
                writer.as_mut(),
                id,
                source,
                target,
                json!({"dependency_kinds": ["import"]}),
            )
            .await
            .unwrap();
        }
    }

    fn test_policy(dir: &Path) -> PathBuf {
        write_policy(
            dir,
            "policy.toml",
            r#"
policy_version = 1
coverage_floor = 0.0
denied_pairs = []

[crate_ranks]
crate-a = 0
crate-b = 1
"#,
        )
    }

    fn write_policy(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn base_request(db: PathBuf, policy: PathBuf) -> AuditRequest {
        AuditRequest {
            map_db: db,
            policy_path: policy,
            as_of: DateTime::parse_from_rfc3339("2026-07-16T18:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            history_window_days: 180,
            include_dev_dependencies: false,
        }
    }

    #[tokio::test]
    async fn layering_violation_flags_upward_dependency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        // crate-a (rank 0) depends_on crate-b (rank 1): a lower-ranked,
        // more-foundational crate depending on a higher-ranked crate is the
        // violating direction. The reverse edge in the fixture (crate-b ->
        // crate-a, rank 1 -> rank 0) is the correct direction and must NOT
        // be flagged, even though together the two edges form a cycle
        // (asserted separately by the dependency_cycle signal test).
        let violation = report
            .signals
            .iter()
            .find(|s| s.id == "layering_violation" && s.subject_id == "crate-a->crate-b")
            .expect("expected crate-a->crate-b layering violation");
        assert_eq!(violation.status, SignalStatus::Observed);
        assert!(
            !report
                .signals
                .iter()
                .any(|s| s.id == "layering_violation" && s.subject_id == "crate-b->crate-a"),
            "crate-b->crate-a is the correct top-down direction and must not be flagged"
        );

        // crate-z is only reachable via a dev-only edge and crate-orphan has
        // NO edge at all — both are unmapped in the policy, so BOTH must
        // degrade to unavailable regardless of include_dev_dependencies (H4):
        // POLICY_INCOMPLETE is derived from the full project set, not from
        // which edges happened to be selected.
        for unmapped in ["crate-z", "crate-orphan"] {
            let signal = report
                .signals
                .iter()
                .find(|s| s.id == "layering_violation" && s.subject_id == unmapped)
                .unwrap_or_else(|| panic!("expected {unmapped} reported as policy-incomplete"));
            assert_eq!(signal.status, SignalStatus::Unavailable);
        }
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::PolicyIncomplete)));

        let mut with_dev = base_request(tmp.path().join("map.db"), test_policy(tmp.path()));
        with_dev.include_dev_dependencies = true;
        let report_with_dev = generate_report(&with_dev).await.unwrap();
        let unmapped = report_with_dev
            .signals
            .iter()
            .find(|s| s.id == "layering_violation" && s.subject_id == "crate-z")
            .expect("expected crate-z reported as policy-incomplete once dev edges are included");
        assert_eq!(unmapped.status, SignalStatus::Unavailable);
    }

    #[tokio::test]
    async fn dependency_cycle_detects_project_and_module_cycles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        let project_cycle = report.signals.iter().any(|s| {
            s.id == "dependency_cycle"
                && s.subject_id.starts_with("project_production:")
                && s.subject_id
                    .contains("11111111-1111-1111-1111-111111111111")
                && s.subject_id
                    .contains("22222222-2222-2222-2222-222222222222")
        });
        assert!(project_cycle, "expected a project_production cycle");

        // H3: the module_import cycle must be exactly the rust lib/util
        // pair by ID — the same-named python `crate_a::lib` module (which
        // has no edges at all) must NOT appear in any cycle membership.
        let module_cycle = report
            .signals
            .iter()
            .find(|s| s.id == "dependency_cycle" && s.subject_id.starts_with("module_import:"))
            .expect("expected a module_import cycle");
        assert_eq!(
            module_cycle.subject_id,
            "module_import:77777777-7777-7777-7777-777777777777,\
             88888888-8888-8888-8888-888888888888"
        );
        assert!(
            !module_cycle
                .subject_id
                .contains("dddddddd-dddd-dddd-dddd-dddddddddddd"),
            "the edge-less python crate_a::lib must not be pulled into the rust SCC by name"
        );
    }

    #[tokio::test]
    async fn zero_in_edge_and_duplicate_hash_are_candidates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();

        // Subject is now the module's own ID (not its display name), so the
        // two same-named `crate_a::lib` modules each get an independent,
        // unambiguous zero_in_edge_module signal.
        let orphan = report
            .signals
            .iter()
            .find(|s| {
                s.id == "zero_in_edge_module"
                    && s.subject_id == "99999999-9999-9999-9999-999999999999"
            })
            .expect("orphan module has no incoming depends_on edge");
        assert_eq!(orphan.status, SignalStatus::Candidate);

        let python_lib = report
            .signals
            .iter()
            .find(|s| {
                s.id == "zero_in_edge_module"
                    && s.subject_id == "dddddddd-dddd-dddd-dddd-dddddddddddd"
            })
            .expect("edge-less python crate_a::lib is independently a zero-in-edge candidate");
        assert_eq!(python_lib.status, SignalStatus::Candidate);

        let dup = report
            .signals
            .iter()
            .find(|s| s.id == "duplicate_content_hash" && s.subject_id == "deadbeef")
            .expect("crate_a::lib and crate_a::util share a content hash");
        assert_eq!(dup.status, SignalStatus::Candidate);
        assert_eq!(dup.evidence_ids.len(), 2);
    }

    #[tokio::test]
    async fn ingest_coverage_aggregates_module_level_unresolved_imports() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        // crate-a itself carries no `unresolved_specifiers` property; the
        // one unresolved import lives on its `crate_a::orphan` MODULE
        // (source_ingest.rs:776-784). The pre-fix code read only the
        // project's own property and reported 0 (H2).
        let coverage = report
            .signals
            .iter()
            .find(|s| s.id == "ingest_coverage" && s.subject_id == "crate-a")
            .expect("expected an ingest_coverage signal for crate-a");
        assert_eq!(
            coverage
                .value
                .get("unresolved_specifier_count")
                .and_then(Value::as_u64),
            Some(1),
            "unresolved specifier recorded on a module must roll up to its project"
        );
        assert!(coverage
            .evidence_ids
            .contains(&"99999999-9999-9999-9999-999999999999".to_string()));
    }

    #[tokio::test]
    async fn coverage_floor_gates_zero_in_edge_to_unavailable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        // crate-a has 4 modules (lib, util, orphan, python-lib) and 1
        // unresolved specifier => coverage_ratio = 4/(4+1) = 0.8. A floor of
        // 0.9 must suppress crate-a's zero_in_edge candidates.
        let policy = write_policy(
            tmp.path(),
            "strict-policy.toml",
            r#"
policy_version = 1
coverage_floor = 0.9
denied_pairs = []

[crate_ranks]
crate-a = 0
crate-b = 1
"#,
        );

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        let orphan = report
            .signals
            .iter()
            .find(|s| {
                s.id == "zero_in_edge_module"
                    && s.subject_id == "99999999-9999-9999-9999-999999999999"
            })
            .unwrap();
        assert_eq!(orphan.status, SignalStatus::Unavailable);
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::CoverageInsufficient)));
    }

    #[tokio::test]
    async fn every_catalog_signal_declares_status() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        for id in ["churn_hotspot", "dead_file", "orphan_test_file"] {
            assert!(
                report
                    .signals
                    .iter()
                    .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
                "expected an unavailable {id} signal"
            );
        }
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::HistoryAbsent)));
    }

    #[tokio::test]
    async fn report_is_byte_identical_across_runs_and_insertion_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = test_policy(tmp.path());

        let db_a = tmp.path().join("map_a.db");
        build_fixture(&db_a, [0, 1]).await;
        let report_a = generate_report(&base_request(db_a, policy.clone()))
            .await
            .unwrap();
        let json_a = serde_json::to_string_pretty(&report_a).unwrap();

        let db_b = tmp.path().join("map_b.db");
        build_fixture(&db_b, [1, 0]).await;
        let report_b = generate_report(&base_request(db_b, policy.clone()))
            .await
            .unwrap();
        let json_b = serde_json::to_string_pretty(&report_b).unwrap();

        assert_eq!(
            json_a, json_b,
            "report JSON must be byte-identical regardless of edge insertion order"
        );

        let db_c = tmp.path().join("map_c.db");
        build_fixture(&db_c, [0, 1]).await;
        let report_c = generate_report(&base_request(db_c, policy)).await.unwrap();
        let json_c = serde_json::to_string_pretty(&report_c).unwrap();
        assert_eq!(
            json_a, json_c,
            "two runs over the same fixture must produce byte-identical JSON"
        );
    }

    #[tokio::test]
    async fn missing_map_database_fails_loud() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("does-not-exist.db");
        let policy = test_policy(tmp.path());
        let err = generate_report(&base_request(db, policy))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn missing_table_degrades_affected_signals_instead_of_aborting() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        {
            let backend = StorageBackend::sqlite(&db).expect("open fixture backend");
            let mut writer = backend.pool().writer().expect("writer guard");
            writer
                .conn_mut()
                .execute_batch("DROP TABLE graph_edges;")
                .expect("drop graph_edges to simulate a partial/older map");
        }
        let policy = test_policy(tmp.path());

        // H1: a partial map missing an entire table must still produce a
        // report — not abort the whole command.
        let report = generate_report(&base_request(db, policy)).await.unwrap();
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
                && e.message.contains("graph_edges")));
        for id in [
            "module_fan_in",
            "layering_violation",
            "manifest_import_mismatch",
            "dependency_cycle_summary",
            "zero_in_edge_module",
        ] {
            assert!(
                report
                    .signals
                    .iter()
                    .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
                "expected {id} to degrade to unavailable when graph_edges is missing"
            );
        }
        // entities-only signals are unaffected by a missing graph_edges table.
        assert!(report
            .signals
            .iter()
            .any(|s| s.id == "ingest_coverage" && s.status == SignalStatus::Observed));
    }

    #[tokio::test]
    async fn missing_column_degrades_affected_signals_instead_of_aborting() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        {
            let backend = StorageBackend::sqlite(&db).expect("open fixture backend");
            let mut writer = backend.pool().writer().expect("writer guard");
            writer
                .conn_mut()
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_entities_kind_entity_type; \
                     ALTER TABLE entities DROP COLUMN entity_type;",
                )
                .expect("drop entity_type to simulate an older map schema");
        }
        let policy = test_policy(tmp.path());

        let report = generate_report(&base_request(db, policy)).await.unwrap();
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
                && e.message.contains("entity_type")));
        assert!(report
            .signals
            .iter()
            .any(|s| s.id == "ingest_coverage" && s.status == SignalStatus::Unavailable));
        assert!(report
            .signals
            .iter()
            .any(|s| s.id == "module_fan_in" && s.status == SignalStatus::Unavailable));
    }

    #[tokio::test]
    async fn missing_edge_metadata_column_degrades_only_metadata_dependent_signals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("map.db");
        build_fixture(&db, [0, 1]).await;
        {
            let backend = StorageBackend::sqlite(&db).expect("open fixture backend");
            let mut writer = backend.pool().writer().expect("writer guard");
            writer
                .conn_mut()
                .execute_batch("ALTER TABLE graph_edges DROP COLUMN metadata;")
                .expect(
                    "drop metadata to simulate a map missing dependency-kind classification \
                     while every other edge column is intact",
                );
        }
        let policy = test_policy(tmp.path());

        // Round-2 Medium: `module_fan_in` and `zero_in_edge_module` never
        // read `graph_edges.metadata`, so a map missing ONLY that column
        // must not suppress them — only the dependency-kind-classification
        // signals (layering, manifest/import mismatch, dependency cycles)
        // may degrade.
        let report = generate_report(&base_request(db, policy)).await.unwrap();
        assert!(report
            .errors
            .iter()
            .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
                && e.message.contains("metadata")));

        for id in ["module_fan_in", "zero_in_edge_module"] {
            assert!(
                report
                    .signals
                    .iter()
                    .any(|s| s.id == id && s.status != SignalStatus::Unavailable),
                "expected {id} to remain available when only graph_edges.metadata is missing"
            );
        }
        for id in [
            "layering_violation",
            "manifest_import_mismatch",
            "dependency_cycle_summary",
        ] {
            assert!(
                report
                    .signals
                    .iter()
                    .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
                "expected {id} to degrade to unavailable when graph_edges.metadata is missing"
            );
        }
    }

    #[tokio::test]
    async fn policy_rejects_unknown_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = write_policy(
            tmp.path(),
            "bad.toml",
            r#"
policy_version = 1
typo_field = true

[crate_ranks]
crate-a = 0
"#,
        );
        let err = load_policy(&policy).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("typo_field")
                || format!("{err:#}").to_lowercase().contains("unknown field")
        );
    }

    #[tokio::test]
    async fn policy_rejects_missing_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = write_policy(
            tmp.path(),
            "no-version.toml",
            r#"
[crate_ranks]
crate-a = 0
"#,
        );
        assert!(load_policy(&policy).is_err());
    }

    #[tokio::test]
    async fn policy_rejects_unsupported_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = write_policy(
            tmp.path(),
            "future-version.toml",
            r#"
policy_version = 99

[crate_ranks]
crate-a = 0
"#,
        );
        let err = load_policy(&policy).unwrap_err();
        assert!(err.to_string().contains("policy_version"));
    }
}
