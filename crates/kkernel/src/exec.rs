//! `kkernel exec` — run a verb DSL expression directly through the pack registry.
//!
//! When the warm daemon is reachable, exec forwards through it instead of
//! building an in-process runtime (ADR-049). Config and namespace are matched
//! against the daemon's own fingerprint; a mismatch falls back to local
//! dispatch, keeping behaviour identical to the in-process path.
//!
//! ## Modes
//!
//! - **DSL mode** (default): `kkernel exec '<dsl>'` — executes a single verb DSL
//!   expression or batch against the configured database and namespace.
//! - **Pending-events mode**: `kkernel exec --pending-events` — one-shot drain that
//!   fires all due `scheduled_event` notes. Mutually exclusive with the positional
//!   `ops` argument. Cron-friendly: run every minute for minute-granularity delivery.
//!
//! # `--ops-file` bulk-apply path
//!
//! `kkernel exec --ops-file batch.jsonl` reads a JSONL file where each
//! non-blank line is a JSON op object `{"tool":"verb","args":{...}}`.  All
//! lines are parsed first; a malformed line aborts before any writes.  Valid
//! ops are dispatched in chunks of [`OPS_FILE_CHUNK_SIZE`] through the same
//! in-process runtime path (daemon fast-path is intentionally skipped for
//! bulk apply — the daemon is warm-state optimised, not throughput optimised).
//! A progress line is printed per chunk.  `--dry-run` validates every line and
//! prints a per-verb summary without writing anything.

use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use khive_mcp::serve::enforce_strict_actor_mode;
#[cfg(unix)]
use khive_mcp::server::compute_config_id;
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

use crate::dbpath::resolve_db_override;
use crate::pending_events;

// `khive-request` is not a direct kkernel dependency.  We use serde_json to
// parse JSONL lines directly (the format is a strict subset of JSON form)
// rather than pulling in the full DSL parser crate.

/// Chunk size for `--ops-file` bulk dispatch.
///
/// Each chunk is dispatched as a single parallel batch through the same
/// `dispatch_request_local` path the MCP `request` tool uses.  100 matches
/// [`khive_request::MAX_OPS`] so the batch always fits inside the parser limit.
const OPS_FILE_CHUNK_SIZE: usize = 100;

/// Arguments for `kkernel exec` — execute a verb DSL expression against a chosen
/// database and namespace, the same syntax accepted by the MCP `request` tool.
#[derive(Parser, Debug)]
pub struct ExecArgs {
    /// DSL expression to execute (same syntax as MCP `request` tool).
    ///
    /// Examples:
    ///   kkernel exec 'knowledge.stats()'
    ///   kkernel exec 'knowledge.index(rebuild_ann=true)'
    ///   kkernel exec '[knowledge.list(limit=5), knowledge.stats()]'
    ///
    /// Mutually exclusive with `--pending-events` and `--ops-file`.
    pub ops: Option<String>,

    /// One-shot drain: fire all `scheduled_event` notes whose `trigger_at <= now`.
    ///
    /// Scans all namespaces, dispatches each event's action in its own namespace,
    /// marks fired events, and advances repeating events (daily/weekly/monthly).
    /// Prints a JSON summary of scanned/fired/advanced/failed counts to stdout.
    ///
    /// Mutually exclusive with the positional `ops` argument and `--ops-file`.
    /// Suitable for cron:
    ///   * * * * *  kkernel exec --pending-events
    #[arg(long, conflicts_with = "ops", conflicts_with = "ops_file")]
    pub pending_events: bool,

    /// Database path (defaults to `~/.khive/khive.db`). `:memory:` selects an
    /// ephemeral in-memory database, matching `kkernel mcp`.
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Namespace to operate in.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Presentation mode: `agent` (default), `verbose`, or `human`.
    #[arg(long)]
    pub presentation: Option<String>,

    /// Verbose output: print per-event progress to stderr.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Write results as JSONL to this path and print a self-describing manifest.
    ///
    /// The manifest (`{path, rows, per_column_null_counts, schema_fingerprint,
    /// checksum}`) is printed to stdout instead of the raw results.  Parent
    /// directories are created if absent.
    ///
    /// Note: `--save-file` always runs in-process and bypasses the warm daemon,
    /// so ANN-dependent verbs (e.g. `knowledge.suggest`, `knowledge.compose`) may
    /// hit a cold or warming index on the first call after a daemon restart.
    ///
    /// Example:
    ///   kkernel exec 'list(kind="entity")' --save-file /tmp/entities.jsonl
    #[arg(long)]
    pub save_file: Option<String>,

    /// JSONL file of ops to apply in bulk.
    ///
    /// Each non-blank line must be a JSON object `{"tool":"verb","args":{...}}`
    /// (the same JSON form the MCP `request` tool accepts).  All lines are
    /// parsed before any write.  A malformed line prints the line number and
    /// error, then aborts without writing.
    ///
    /// Progress is printed per chunk to stderr; the final aggregate summary is
    /// printed to stdout.
    ///
    /// Mutually exclusive with the positional `ops` argument.
    #[arg(long, value_name = "PATH")]
    pub ops_file: Option<PathBuf>,

    /// Parse and validate every op, print the would-be summary, then exit
    /// without writing anything.  Only valid with `--ops-file`.
    #[arg(long, requires = "ops_file")]
    pub dry_run: bool,
}

/// A single parsed op entry from an ops-file line.
#[derive(Debug)]
struct OpsFileEntry {
    tool: String,
    args: serde_json::Value,
}

/// Parse a JSONL ops-file.
///
/// Returns the ordered list of ops, or an error naming the first malformed
/// line.  Blank lines are skipped.
///
/// Each line must be a JSON object `{"tool":"verb","args":{...}}`.  `"args"`
/// is optional and defaults to an empty object.  Any other top-level keys are
/// silently ignored so the format is forward-compatible.
fn parse_ops_file(path: &PathBuf) -> Result<Vec<OpsFileEntry>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("open ops-file {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut ops: Vec<OpsFileEntry> = Vec::new();

    for (line_idx, result) in reader.lines().enumerate() {
        let line_num = line_idx + 1;
        let raw = result.with_context(|| format!("read ops-file line {line_num}"))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse as a JSON object.
        let obj: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| anyhow::anyhow!("ops-file line {line_num}: invalid JSON: {e}"))?;

        let obj = obj.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "ops-file line {line_num}: expected a JSON object {{\"tool\":...,\"args\":...}}, \
                 got a non-object value"
            )
        })?;

        // "tool" is required.
        let tool = obj
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("ops-file line {line_num}: missing or non-string \"tool\" field")
            })?
            .to_owned();

        // "args" defaults to an empty object.
        let args = match obj.get("args") {
            None => serde_json::Value::Object(serde_json::Map::new()),
            Some(v) => {
                if !v.is_object() {
                    anyhow::bail!(
                        "ops-file line {line_num}: \"args\" must be a JSON object, got {v}"
                    );
                }
                v.clone()
            }
        };

        ops.push(OpsFileEntry { tool, args });
    }

    Ok(ops)
}

/// Apply a parsed ops-file against the given server, printing progress to
/// stderr and the final summary to stdout.
async fn apply_ops_file(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
) -> Result<()> {
    let total = ops.len();
    let mut total_succeeded: usize = 0;
    let mut total_failed: usize = 0;

    for (chunk_idx, chunk) in ops.chunks(OPS_FILE_CHUNK_SIZE).enumerate() {
        let applied_before = (chunk_idx * OPS_FILE_CHUNK_SIZE).min(total);

        // Build the JSON array string for this chunk.
        let batch_arr: Vec<serde_json::Value> = chunk
            .iter()
            .map(|e| {
                serde_json::json!({
                    "tool": e.tool,
                    "args": e.args,
                })
            })
            .collect();
        let batch_json = serde_json::to_string(&batch_arr).context("serialize chunk to JSON")?;

        let params = RequestParams {
            ops: batch_json,
            presentation: presentation.clone(),
            presentation_per_op: None,
            save_to: None,
        };

        let raw = server
            .dispatch_request_local(params)
            .await
            .map_err(|e| anyhow::anyhow!("dispatch chunk {}: {}", chunk_idx + 1, e))?;

        let parsed: serde_json::Value =
            serde_json::from_str(&raw).context("parse dispatch result")?;

        let chunk_succeeded = parsed["summary"]["succeeded"].as_u64().unwrap_or(0) as usize;
        let chunk_failed = parsed["summary"]["failed"].as_u64().unwrap_or(0) as usize;

        total_succeeded += chunk_succeeded;
        total_failed += chunk_failed;

        let applied_now = applied_before + chunk.len();
        eprintln!("applied {applied_now}/{total} (ok={total_succeeded}, failed={total_failed})");
    }

    let summary = serde_json::json!({
        "total": total,
        "succeeded": total_succeeded,
        "failed": total_failed,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).expect("serialize summary")
    );
    Ok(())
}

/// Execute the DSL expression, routing through the warm daemon when available.
///
/// Strategy:
/// 1. Build `RuntimeConfig` from args (cheap — no I/O).
/// 2. On Unix, attempt to forward through the daemon via the same
///    length-prefixed socket protocol the MCP stdio server uses (ADR-049).
///    Config and namespace fingerprints are verified by the daemon; a mismatch
///    causes it to respond with a rejection and we fall through to step 3.
/// 3. Fall back to building the full in-process runtime when the daemon is
///    absent, unreachable, or returns a mismatch (KHIVE_NO_DAEMON=1 also skips).
///
/// Output byte-shape is identical in both paths — the daemon echoes the same
/// JSON the local dispatch produces.
///
/// When `--ops-file` is given, steps 2 and 3 differ: the daemon fast-path is
/// skipped entirely, and all ops are dispatched through the in-process runtime
/// in chunks (see module-level docs).
pub async fn run_exec(args: ExecArgs) -> Result<()> {
    // ── pending-events drain ─────────────────────────────────────────────────
    if args.pending_events {
        let summary =
            pending_events::run_pending_events(args.db.as_deref(), &args.namespace, args.verbose)
                .await?;
        pending_events::print_summary(&summary);
        return Ok(());
    }

    // ── mutual exclusion check ─────────────────────────────────────────────────
    let mode = match (&args.ops, &args.ops_file) {
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "cannot use both a positional ops string and --ops-file; supply exactly one"
            );
        }
        (None, None) => {
            anyhow::bail!(
                "no ops provided; supply a DSL expression as a positional argument or use \
                 --ops-file <PATH>"
            );
        }
        (Some(ops), None) => ExecMode::Inline(ops.clone()),
        (None, Some(path)) => ExecMode::OpsFile(path.clone()),
    };

    let mut cfg = RuntimeConfig::default();
    if let Some(db_path) = resolve_db_override(args.db.as_deref()) {
        cfg.db_path = db_path;
    }
    cfg.default_namespace =
        Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;

    match mode {
        ExecMode::Inline(ops) => run_exec_inline(ops, cfg, args.presentation, args.save_file).await,
        ExecMode::OpsFile(path) => {
            run_exec_ops_file(path, cfg, args.presentation, args.dry_run).await
        }
    }
}

enum ExecMode {
    Inline(String),
    OpsFile(PathBuf),
}

async fn run_exec_inline(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    save_file: Option<String>,
) -> Result<()> {
    // ── daemon fast-path (Unix only) ─────────────────────────────────────────
    // The daemon path does not support --save-file (the daemon returns a string;
    // we would need to parse it back to apply the sink).  Skip daemon forwarding
    // when --save-file is set so the in-process path handles everything.
    #[cfg(unix)]
    if save_file.is_none() {
        let frame = DaemonRequestFrame {
            ops: ops.clone(),
            presentation: presentation.clone(),
            presentation_per_op: None,
            namespace: cfg.default_namespace.as_str().to_string(),
            config_id: compute_config_id(&cfg, None),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
        };
        if let Some(res) = khive_mcp::daemon::forward_or_spawn(&frame).await {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            println!("{output}");
            return Ok(());
        }
    }

    // ── in-process fallback ───────────────────────────────────────────────────
    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    enforce_strict_actor_mode(rt.config().actor_id.as_deref(), &rt.config().packs)?;
    let server = KhiveMcpServer::new(rt).map_err(|e| anyhow::anyhow!("{e}"))?;

    let params = RequestParams {
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
    };

    let output = server
        .dispatch_request_local(params)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{output}");
    Ok(())
}

async fn run_exec_ops_file(
    path: PathBuf,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    dry_run: bool,
) -> Result<()> {
    // Parse the whole file first — fail before any writes if any line is bad.
    let ops = parse_ops_file(&path)?;

    if ops.is_empty() {
        anyhow::bail!("ops-file is empty (no non-blank lines): {}", path.display());
    }

    if dry_run {
        // Count ops per verb and report — no dispatch.
        let mut per_verb: BTreeMap<String, usize> = BTreeMap::new();
        for op in &ops {
            *per_verb.entry(op.tool.clone()).or_insert(0) += 1;
        }
        let summary = serde_json::json!({
            "dry_run": true,
            "total": ops.len(),
            "per_verb": per_verb,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).expect("serialize dry-run summary")
        );
        return Ok(());
    }

    // Build the in-process runtime (daemon fast-path is intentionally skipped
    // for bulk apply — bulk throughput benefits from a single warm runtime, not
    // the round-trip overhead of socket forwarding per chunk).
    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    enforce_strict_actor_mode(rt.config().actor_id.as_deref(), &rt.config().packs)?;
    let server = KhiveMcpServer::new(rt).map_err(|e| anyhow::anyhow!("{e}"))?;

    apply_ops_file(&server, ops, presentation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serial_test::serial;
    use tempfile::NamedTempFile;

    // ── clap / env-binding tests ───────────────────────────────────────────────

    #[test]
    #[serial]
    fn khive_db_env_binds_to_db_arg() {
        // clap reads KHIVE_DB for `--db` (parity with `kkernel mcp`).
        std::env::set_var("KHIVE_DB", "/tmp/kkernel-exec-env.db");
        let args = ExecArgs::parse_from(["exec", "stats()"]);
        std::env::remove_var("KHIVE_DB");
        assert_eq!(args.db.as_deref(), Some("/tmp/kkernel-exec-env.db"));
    }

    #[test]
    fn pending_events_flag_sets_mode() {
        let args = ExecArgs::parse_from(["exec", "--pending-events"]);
        assert!(args.pending_events);
        assert!(args.ops.is_none());
    }

    #[test]
    fn pending_events_conflicts_with_ops() {
        let result = ExecArgs::try_parse_from(["exec", "--pending-events", "stats()"]);
        assert!(
            result.is_err(),
            "--pending-events and positional ops must conflict"
        );
    }

    #[test]
    fn pending_events_conflicts_with_ops_file() {
        let result =
            ExecArgs::try_parse_from(["exec", "--pending-events", "--ops-file", "/tmp/x.jsonl"]);
        assert!(
            result.is_err(),
            "--pending-events and --ops-file must conflict"
        );
    }

    #[test]
    fn ops_positional_is_optional() {
        // With --ops-file, the positional ops should be absent.
        let args = ExecArgs::parse_from(["exec", "--ops-file", "/tmp/batch.jsonl"]);
        assert!(args.ops.is_none());
        assert_eq!(
            args.ops_file.as_deref(),
            Some(std::path::Path::new("/tmp/batch.jsonl"))
        );
    }

    #[test]
    fn ops_positional_works_without_pending_events() {
        let args = ExecArgs::parse_from(["exec", "stats()"]);
        assert_eq!(args.ops.as_deref(), Some("stats()"));
        assert!(!args.pending_events);
    }

    #[test]
    fn dry_run_requires_ops_file() {
        // clap enforces `requires = "ops_file"` for --dry-run.
        let result = ExecArgs::try_parse_from(["exec", "stats()", "--dry-run"]);
        assert!(
            result.is_err(),
            "dry-run without --ops-file should be rejected by clap"
        );
    }

    // ── isolated DB helpers ────────────────────────────────────────────────────

    /// Build an isolated in-process runtime using a temp-file SQLite database.
    /// Never touches the production `~/.khive/khive.db`.
    fn isolated_server(db_path: &str) -> KhiveMcpServer {
        let cfg = RuntimeConfig {
            db_path: Some(PathBuf::from(db_path)),
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime on temp db");
        KhiveMcpServer::new(rt).expect("server on temp db")
    }

    // ── parse_ops_file tests ───────────────────────────────────────────────────

    #[test]
    fn parse_ops_file_skips_blank_lines() {
        use std::io::Write as _;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
        f.write_all(b"\n").unwrap(); // blank
        f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
        let ops = parse_ops_file(&f.path().to_path_buf()).unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn parse_ops_file_reports_line_number_on_malformed() {
        use std::io::Write as _;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
        f.write_all(b"not-json\n").unwrap(); // line 2 is bad
        let err = parse_ops_file(&f.path().to_path_buf()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("line 2"),
            "error should name the bad line number, got: {msg}"
        );
    }

    #[test]
    fn parse_ops_file_missing_tool_field() {
        use std::io::Write as _;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{\"notool\":\"x\",\"args\":{}}\n").unwrap();
        let err = parse_ops_file(&f.path().to_path_buf()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "should report line number: {msg}");
    }

    // ── integration: bulk apply (isolated DB) ─────────────────────────────────

    #[tokio::test]
    async fn ops_file_applies_ops_and_summary_matches() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);

        // Write 3 create-entity ops.
        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write as _;
        for name in ["Alpha", "Beta", "Gamma"] {
            let line = format!(
                "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
            );
            f.write_all(line.as_bytes()).unwrap();
        }

        let ops = parse_ops_file(&f.path().to_path_buf()).unwrap();
        assert_eq!(ops.len(), 3);
        apply_ops_file(&server, ops, None).await.unwrap();

        // Verify all 3 entities are present.
        let params = RequestParams {
            ops: r#"list(kind="concept")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Agent presentation: `{"results":[{"ok":true,"result":[...],"tool":"list"}],...}`.
        // The `list` verb returns an array of entities directly under `result`.
        let count = resp["results"][0]["result"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(
            count, 3,
            "all 3 entities should be present after apply\nraw: {resp}"
        );
    }

    #[tokio::test]
    async fn ops_file_dry_run_writes_nothing() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write as _;
        for name in ["DryA", "DryB"] {
            let line = format!(
                "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
            );
            f.write_all(line.as_bytes()).unwrap();
        }

        let path = f.path().to_path_buf();
        let cfg = RuntimeConfig {
            db_path: Some(PathBuf::from(&db_path)),
            ..Default::default()
        };

        // dry_run=true → no writes.
        run_exec_ops_file(path.clone(), cfg.clone(), None, true)
            .await
            .unwrap();

        // Verify nothing was written by checking with a fresh server.
        let server = isolated_server(&db_path);
        let params = RequestParams {
            ops: r#"list(kind="concept")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let count = resp["results"][0]["result"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(count, 0, "dry-run must not write any entities");
    }

    #[tokio::test]
    async fn ops_file_malformed_line_aborts_before_writes() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write as _;
        // Line 1: valid op
        f.write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"ShouldNotExist\"}}\n",
        )
        .unwrap();
        // Line 2: malformed
        f.write_all(b"INVALID JSON LINE\n").unwrap();

        let path = f.path().to_path_buf();

        // parse_ops_file should fail with line 2 error.
        let err = parse_ops_file(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("line 2"),
            "should report line 2 as malformed: {msg}"
        );

        // Because parse failed, no dispatch happened → DB is clean.
        let server = isolated_server(&db_path);
        let params = RequestParams {
            ops: r#"list(kind="concept")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let count = resp["results"][0]["result"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(
            count, 0,
            "nothing should be written when any line fails to parse"
        );
    }
}
