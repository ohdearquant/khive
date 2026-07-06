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
//! ops are dispatched in chunks of 100 (`OPS_FILE_CHUNK_SIZE`) through the same
//! in-process runtime path (daemon fast-path is intentionally skipped for
//! bulk apply — the daemon is warm-state optimised, not throughput optimised).
//! A progress line is printed per chunk.  `--dry-run` validates every line and
//! prints a per-verb summary without writing anything.

use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use khive_mcp::serve::{
    apply_env_output_format, enforce_strict_actor_mode, resolve_runtime_config, RuntimeConfigInputs,
};
#[cfg(unix)]
use khive_mcp::server::compute_config_id;
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveConfig, KhiveRuntime, Namespace, RuntimeConfig};

// ── daemon-forward seam (Unix only) ─────────────────────────────────────────
//
// `run_exec_inline_with_forward` takes a `ForwardFnPtr` so that tests can
// inject a spy instead of the real `forward_or_spawn`.  This lets us assert
// that `enforce_strict_actor_mode` fires BEFORE any forwarding attempt, without
// spawning a subprocess or depending on a live daemon socket.
//
// On non-Unix platforms the seam parameter is absent and the daemon block is
// compiled out entirely.
/// Boxed future returned by a forward function.
#[cfg(unix)]
type ForwardFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<Result<String, rmcp::ErrorData>>> + Send + 'a>,
>;

/// Function pointer type for the daemon-forwarding seam.
#[cfg(unix)]
type ForwardFnPtr = for<'a> fn(&'a DaemonRequestFrame) -> ForwardFuture<'a>;

/// Adapts the real `forward_or_spawn` to the `ForwardFnPtr` signature.
#[cfg(unix)]
fn forward_or_spawn_boxed(frame: &DaemonRequestFrame) -> ForwardFuture<'_> {
    Box::pin(khive_mcp::daemon::forward_or_spawn(frame))
}

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

    /// Presentation mode: `verbose` (default), `agent`, or `human`.
    ///
    /// ADR-045 §2 selection rules: the `kkernel exec` CLI surface (a trusted
    /// operator / scripted-caller path) defaults to `Verbose` — the full
    /// canonical shape — unlike the MCP `request` tool, which defaults to
    /// `Agent` for token efficiency. Pass `--presentation agent` to opt into
    /// the trimmed shape, or `--presentation human` for pretty terminal output.
    #[arg(long, default_value = "verbose")]
    pub presentation: Option<String>,

    /// Output format for verb results (ADR-078 §2 precedence: this flag >
    /// `KHIVE_OUTPUT_FORMAT` env var > `[runtime] default_output_format` in
    /// `khive.toml` > builtin `json`).
    ///
    /// Valid values: `json` (compact, lossless — default), `auto` (shape-aware:
    /// markdown table for record arrays, key-value block for single records),
    /// `table` (force markdown table).
    #[arg(long, value_name = "FORMAT")]
    pub output_format: Option<String>,

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
            format: None,
            format_per_op: None,
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

    // Resolve through the SAME TOML-aware path `kkernel mcp` and `kkernel reindex`
    // use (`resolve_runtime_config`), so `kkernel exec`'s config_id and actor
    // identity agree with the daemon's. Previously this built `cfg` from
    // `RuntimeConfig::default()` (env-only) plus an env-only db override and
    // never called `KhiveConfig::load_with_home_fallback` at all, so a project's
    // tier-3 `.khive/config.toml` (`[actor] id`, `[[engines]]`) was invisible to
    // `kkernel exec`. That drift made `compute_config_id(&cfg, None)` diverge
    // from the daemon's TOML-resolved fingerprint, so the daemon rejected the
    // forwarded frame as a `ConfigMismatch` and `exec` silently fell back to an
    // in-process, TOML-blind, effectively-anonymous dispatch (issue #581).
    let namespace = Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None, // `kkernel exec` has no `--config` flag today
        namespace,
        // `--namespace` has a clap `default_value = "local"`, so it is always
        // present — there is no way to distinguish "operator typed --namespace
        // local" from "operator didn't pass --namespace at all". `true` is the
        // conservative, behavior-preserving choice: it keeps exec's pre-existing
        // semantics (the CLI/default value always becomes `default_namespace`,
        // matching what `resolve_runtime_config`'s embed path already did
        // unconditionally). It is also empirically inert for config_id parity:
        // in the embed path (`no_embed: false`, exec's only mode), this flag
        // gates only the actor_id fill-when-None guard in `resolve_runtime_config`
        // — and `compute_config_id` never reads `actor_id` (namespace is
        // "carried separately" per its own doc comment). See the
        // `namespace_explicit_changes_actor_id_fill_but_not_config_id` and
        // `exec_config_id_matches_serve_config_id_for_project_toml_actor` tests
        // below, which construct both arms and assert this directly rather than
        // assuming it.
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;

    match mode {
        ExecMode::Inline(ops) => {
            run_exec_inline(
                ops,
                cfg,
                args.presentation,
                args.output_format,
                args.save_file,
            )
            .await
        }
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
    output_format: Option<String>,
    save_file: Option<String>,
) -> Result<()> {
    #[cfg(unix)]
    return run_exec_inline_with_forward(
        ops,
        cfg,
        presentation,
        output_format,
        save_file,
        forward_or_spawn_boxed,
    )
    .await;
    #[cfg(not(unix))]
    return run_exec_inline_with_forward(ops, cfg, presentation, output_format, save_file).await;
}

/// Inner implementation of `run_exec_inline`, parameterised over the daemon
/// forwarding function.  On Unix the real caller passes `forward_or_spawn_boxed`;
/// tests pass a spy to assert that the strict-actor gate fires BEFORE any
/// forwarding attempt is made.
///
/// # Why this seam exists
///
/// The daemon bypass bug (fixed in the commit preceding this one) could only be
/// regression-tested by either spawning a real daemon subprocess (fragile) or
/// injecting a spy at the forwarding boundary (deterministic).  This function
/// enables the latter: tests pass a spy `forward_fn` and assert it is never
/// called when the gate should have rejected.
#[cfg_attr(not(unix), allow(unused_variables))]
async fn run_exec_inline_with_forward(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    #[cfg(unix)] forward_fn: ForwardFnPtr,
) -> Result<()> {
    // ── strict-actor gate (before any forwarding) ─────────────────────────────
    // Must run BEFORE the daemon fast-path so that a comm-capable anonymous daemon
    // already running cannot be used to bypass KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1.
    // The daemon receives requests over a socket and dispatches comm verbs — the
    // same tenant-isolation risk as in-process dispatch.  Checking only in the
    // in-process fallback (as was the case before this fix) allowed a strict-mode
    // client to silently forward through a pre-existing anonymous daemon and exit 0.
    enforce_strict_actor_mode(cfg.actor_id.as_deref(), &cfg.packs)?;

    // ── daemon fast-path (Unix only) ─────────────────────────────────────────
    // The daemon path does not support --save-file (the daemon returns a string;
    // we would need to parse it back to apply the sink).  Skip daemon forwarding
    // when --save-file is set so the in-process path handles everything.
    //
    // The --output-format CLI flag (ADR-078 tier-1) is forwarded to the daemon as
    // the per-request `format` field so the daemon applies it at its seam.
    #[cfg(unix)]
    if save_file.is_none() {
        let frame = DaemonRequestFrame {
            ops: ops.clone(),
            presentation: presentation.clone(),
            presentation_per_op: None,
            namespace: cfg.default_namespace.as_str().to_string(),
            actor_id: cfg.actor_id.clone(),
            visible_namespaces: cfg
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            config_id: compute_config_id(&cfg, None),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            format: output_format.clone(),
            format_per_op: None,
            // `kkernel exec` is a trusted operator surface: subhandler verbs are
            // allowed. Only the agent-facing MCP `request` tool sets this true.
            from_wire: false,
        };
        if let Some(res) = forward_fn(&frame).await {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            println!("{output}");
            return Ok(());
        }
    }

    // ── in-process fallback ───────────────────────────────────────────────────
    // Resolve the full ADR-078 §2 precedence chain before building the server,
    // mirroring the serve path: env-var (tier-2: KHIVE_OUTPUT_FORMAT) over
    // TOML `[runtime] default_output_format` (tier-3) over builtin json. The CLI
    // flag is then passed as the per-request `format` field (tier-1), which
    // overrides the server default at dispatch time. This block only runs in the
    // in-process fallback — the common daemon-forward path returns above and the
    // daemon applies its own serve-time TOML resolution — so the config load here
    // is off the hot path.
    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Note: enforce_strict_actor_mode was called above before the daemon fast-path;
    // it is not repeated here — the single early check covers both paths.
    //
    // `rt.config().db_path` anchors tier-3 project-local config discovery to the
    // database's own directory rather than the process cwd, matching the serve path.
    let toml_default = KhiveConfig::load_with_home_fallback(None, rt.config().db_path.as_deref())
        .ok()
        .flatten()
        .and_then(|c| c.runtime.default_output_format);
    let env_fmt = apply_env_output_format(toml_default);
    let server = KhiveMcpServer::new(rt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .with_default_output_format(env_fmt);

    let params = RequestParams {
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
        // Tier-1: CLI --output-format overrides the server default (env/builtin).
        format: output_format,
        format_per_op: None,
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

    // ── ADR-045 §2: `kkernel exec` CLI surface defaults to Verbose ────────────

    #[test]
    fn presentation_defaults_to_verbose_when_flag_omitted() {
        // ADR-045 §2 selection rules: `kkernel exec` (a scripted/operator
        // surface) defaults to Verbose, unlike the MCP `request` tool (which
        // defaults to Agent at the envelope layer — see
        // `khive_mcp::server::parse_presentation_mode`, unchanged by this test).
        let args = ExecArgs::parse_from(["exec", "stats()"]);
        assert_eq!(args.presentation.as_deref(), Some("verbose"));
    }

    #[test]
    fn presentation_agent_flag_still_selects_agent() {
        let args = ExecArgs::parse_from(["exec", "stats()", "--presentation", "agent"]);
        assert_eq!(args.presentation.as_deref(), Some("agent"));
    }

    #[test]
    fn presentation_human_flag_still_selects_human() {
        let args = ExecArgs::parse_from(["exec", "stats()", "--presentation", "human"]);
        assert_eq!(args.presentation.as_deref(), Some("human"));
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
            embedding_model: None,
            additional_embedding_models: vec![],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime on temp db");
        KhiveMcpServer::new(rt).expect("server on temp db")
    }

    // ── exec-path / serve-path config_id parity (#581) ────────────────────────
    //
    // `run_exec`'s cfg construction (above) and `kkernel mcp`'s `build_server`
    // both call `resolve_runtime_config`. These tests prove the two call shapes
    // agree on `compute_config_id` for the same database — the acceptance gate
    // for the #581 fix — and settle the `namespace_explicit` design question
    // empirically rather than by convention.

    /// Direct regression guard for #581: a project's tier-3 `.khive/config.toml`
    /// `[actor] id` must be visible to `kkernel exec` exactly as it is to
    /// `kkernel mcp`, and the two paths' `config_id` must be byte-identical so
    /// the daemon accepts exec's forwarded frame instead of rejecting it as a
    /// `ConfigMismatch` (which silently falls back to an anonymous in-process
    /// dispatch — the reported symptom: `comm.inbox` returning `count=0`).
    #[test]
    #[serial]
    fn exec_config_id_matches_serve_config_id_for_project_toml_actor() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");

        let dir = tempfile::tempdir().expect("tempdir");
        let khive_dir = dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
        std::fs::write(
            khive_dir.join("config.toml"),
            r#"
[actor]
id = "lambda:test-actor"

[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
        )
        .expect("write config.toml");

        // A db path anchored INSIDE the same `.khive` dir — this is what makes
        // tier-3 discovery agree between a client and a daemon serving the same
        // database, regardless of process cwd (see `project_config_anchor_dir`).
        let db_path = khive_dir.join("exec-parity-test.db");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        let ns = Namespace::parse("local").expect("ns");

        // exec-shaped inputs: `config: None` (kkernel exec has no `--config`
        // flag today), `namespace_explicit: true` (the choice made in `run_exec`
        // above).
        let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        // serve-shaped inputs: mirrors `build_server` when the operator starts
        // `kkernel mcp --daemon` with no explicit --actor/--namespace flag,
        // relying on the config file's `[actor] id` — the common daemon-startup
        // shape (`resolve_cli_namespace` returns `explicit=false` in that case).
        let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: ns,
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve serve-shaped config");

        // The TOML must actually have reached both constructions — the direct
        // regression proxy for #581, verified without a live daemon socket.
        assert_eq!(exec_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
        assert_eq!(serve_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
        assert!(
            exec_cfg
                .visible_namespaces
                .contains(&Namespace::parse("lambda:test-actor").expect("ns")),
            "actor.id must fold into visible_namespaces (ADR-007 Rev 4 Rule 3b)"
        );
        assert!(
            exec_cfg.embedding_model.is_some(),
            "config-file [[engines]] must resolve an embedding model, not env/default"
        );
        assert_eq!(
            format!("{:?}", exec_cfg.embedding_model),
            format!("{:?}", serve_cfg.embedding_model),
        );

        // The acceptance gate: byte-identical config_id, so the daemon accepts
        // exec's forwarded frame instead of rejecting it as a ConfigMismatch.
        assert_eq!(
            compute_config_id(&exec_cfg, None),
            compute_config_id(&serve_cfg, None),
            "exec-path config_id must match the serve/daemon-path config_id for the same db"
        );
    }

    /// Settles the `namespace_explicit` design question by constructing both
    /// arms and comparing `compute_config_id` directly, per the decision
    /// criterion: does either arm break config_id parity with the daemon?
    ///
    /// No `[actor] id` is present (an explicit nonexistent config path makes
    /// this fully deterministic — no dependency on cwd or `$HOME`), and the
    /// namespace is a non-"local" value so the actor_id fill-when-None guard in
    /// `resolve_runtime_config` (the ONLY place `namespace_explicit` has any
    /// effect in the embed path, i.e. `no_embed: false`, which `kkernel exec`
    /// always uses) actually fires for one arm and not the other.
    #[test]
    #[serial]
    fn namespace_explicit_changes_actor_id_fill_but_not_config_id() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");

        let missing_config =
            std::path::PathBuf::from("/nonexistent/khive-exec-parity-test/config.toml");
        let ns = Namespace::parse("lambda:custom-ns").expect("ns");

        let with_explicit_true = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve with namespace_explicit=true");

        let with_explicit_false = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns,
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve with namespace_explicit=false");

        // The fill-when-None guard DOES fire differently between the two arms...
        assert_eq!(
            with_explicit_true.actor_id.as_deref(),
            Some("lambda:custom-ns"),
            "namespace_explicit=true + non-local namespace + no config actor.id \
             must fill actor_id from the namespace (ADR-057)"
        );
        assert_eq!(
            with_explicit_false.actor_id, None,
            "namespace_explicit=false must NOT fill actor_id"
        );

        // ...but `compute_config_id` never reads `actor_id` (namespace is
        // "carried separately" per its own doc comment), so the two configs —
        // which differ ONLY in actor_id — must still produce a byte-identical
        // fingerprint. This is the empirical basis for `run_exec` picking
        // `namespace_explicit: true`: it is the conservative, behavior-
        // preserving choice, and it provably does not affect config_id parity
        // with the daemon either way.
        assert_eq!(
            compute_config_id(&with_explicit_true, None),
            compute_config_id(&with_explicit_false, None),
            "namespace_explicit must not affect the daemon-forwarded config_id"
        );
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
            format: None,
            format_per_op: None,
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
            format: None,
            format_per_op: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let count = resp["results"][0]["result"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(count, 0, "dry-run must not write any entities");
    }

    // ── strict-actor mode: daemon bypass regression ───────────────────────────

    /// Regression: `run_exec_inline` must enforce the strict-actor gate BEFORE
    /// forwarding to the daemon, so a comm-capable anonymous daemon already running
    /// cannot be used to bypass `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`.
    ///
    /// Prior to this fix, `enforce_strict_actor_mode` was only called in the
    /// in-process fallback path (after the daemon fast-path returned).  An attacker
    /// or misconfigured operator could start a no-actor daemon, then run strict-mode
    /// `kkernel exec` which would forward through it and exit 0.
    ///
    /// The fix moves the check to before the daemon block.  This test drives
    /// `run_exec_inline` directly with a config that has `comm` in the pack list
    /// and no actor identity.  It must return an `Err` whose message names
    /// `KHIVE_REQUIRE_ATTRIBUTED_ACTOR` regardless of whether a daemon is reachable
    /// (KHIVE_NO_DAEMON=1 is set to keep the test isolated from any running daemon,
    /// but the error should fire before any forwarding attempt anyway).
    #[tokio::test]
    #[serial]
    async fn strict_mode_rejects_before_daemon_forward_when_comm_and_no_actor() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();

        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        // Belt-and-suspenders: ensure no daemon is contacted even if one happens
        // to be running.  The error should fire before forwarding, but we make the
        // test deterministic by also suppressing the daemon path.
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let cfg = RuntimeConfig {
            db_path: None, // in-memory
            packs: vec!["kg".to_string(), "comm".to_string()],
            actor_id: None, // no actor — triggers the strict-mode gate
            ..RuntimeConfig::default()
        };

        let result = run_exec_inline("stats()".to_string(), cfg, None, None, None).await;

        // Restore env.
        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }

        assert!(
            result.is_err(),
            "run_exec_inline must return Err under strict mode + comm + no actor; got Ok"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
            "error must name the strict-mode env var; got: {msg}"
        );
        assert!(
            msg.contains("KHIVE_ACTOR"),
            "error must name the remedy (KHIVE_ACTOR); got: {msg}"
        );
    }

    /// Complement: strict mode must NOT reject when comm is loaded and an actor
    /// IS configured — the daemon fast-path must remain available in that case.
    #[tokio::test]
    #[serial]
    async fn strict_mode_allows_exec_when_comm_and_actor_configured() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();

        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        std::env::set_var("KHIVE_NO_DAEMON", "1"); // force in-process to avoid daemon dep

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            actor_id: Some("lambda:tenant-x".to_string()), // actor configured → no gate
            ..RuntimeConfig::default()
        };

        // The strict gate must pass; the actual dispatch will succeed (stats() is safe).
        let result = run_exec_inline("stats()".to_string(), cfg, None, None, None).await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }

        assert!(
            result.is_ok(),
            "run_exec_inline must succeed under strict mode when actor IS configured; got: {result:?}"
        );
    }

    /// Default-off regression: when KHIVE_REQUIRE_ATTRIBUTED_ACTOR is unset,
    /// run_exec_inline must NOT reject even with comm + no actor (OSS default path).
    #[tokio::test]
    #[serial]
    async fn strict_mode_off_exec_inline_passes_with_comm_no_actor() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();

        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"); // default OFF
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            actor_id: None,
            ..RuntimeConfig::default()
        };

        let result = run_exec_inline("stats()".to_string(), cfg, None, None, None).await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }

        assert!(
            result.is_ok(),
            "run_exec_inline must NOT reject when strict mode is OFF (OSS default); got: {result:?}"
        );
    }

    // ── spy-based isomorphism guard (Unix only) ───────────────────────────────
    //
    // The three tests above use KHIVE_NO_DAEMON=1, which disables the daemon
    // fast-path at the `forward_or_spawn` level.  That makes them correct checks
    // of the strict gate in isolation, but tautological w.r.t. the daemon-bypass
    // bug: moving `enforce_strict_actor_mode` back to BELOW the daemon block would
    // NOT cause those tests to fail because the daemon path is suppressed.
    //
    // These tests use `run_exec_inline_with_forward` directly, passing a spy
    // function pointer.  KHIVE_NO_DAEMON is NOT set in the rejection test.
    // The spy can therefore be reached if — and only if — `enforce_strict_actor_mode`
    // is called AFTER the forwarding attempt.  Under the correct implementation
    // (enforce first) the gate rejects before the spy is invoked, so the spy
    // thread-local remains false.
    //
    // ISOMORPHISM PROOF (performed during review, result recorded here):
    //   Temporarily moved `enforce_strict_actor_mode` to below the daemon block in
    //   `run_exec_inline_with_forward`.  `strict_mode_spy_confirms_enforce_fires_before_forward`
    //   failed with: "spy forward_fn was called — enforce fired after forwarding"
    //   Restoring the early check made the test pass again.
    //   This confirms the test is NOT tautological w.r.t. the bug it guards.

    // Thread-local spy flag shared between the outer test body and the spy fn pointer.
    // Using a module-level thread_local! avoids the "two separate statics" trap that
    // arises when thread_local! is declared inside a function body.
    #[cfg(unix)]
    std::thread_local! {
        static SPY_WAS_CALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(unix)]
    fn spy_forward_records_call(_frame: &DaemonRequestFrame) -> super::ForwardFuture<'_> {
        SPY_WAS_CALLED.with(|c| c.set(true));
        Box::pin(async { None })
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn strict_mode_spy_confirms_enforce_fires_before_forward() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        // Deliberately do NOT set KHIVE_NO_DAEMON — the spy must be reachable
        // if the enforce call is in the wrong place.
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        SPY_WAS_CALLED.with(|c| c.set(false));

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            actor_id: None, // no actor — should trigger the strict gate
            ..RuntimeConfig::default()
        };

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg,
            None,
            None, // output_format
            None,
            spy_forward_records_call,
        )
        .await;

        let spy_was_called = SPY_WAS_CALLED.with(|c| c.get());
        SPY_WAS_CALLED.with(|c| c.set(false)); // clean up

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }

        assert!(
            result.is_err(),
            "strict mode + comm + no actor must return Err; got Ok"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
            "error must name the strict-mode env var; got: {msg}"
        );
        assert!(
            !spy_was_called,
            "spy forward_fn was called — enforce_strict_actor_mode fired AFTER forwarding, not before"
        );
    }

    /// Complement: when an actor IS configured, the spy fn is reached because
    /// the gate passes and forwarding is attempted.  We use KHIVE_NO_DAEMON=1 so
    /// the spy returns None and in-process dispatch handles the request normally.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn strict_mode_spy_forward_reached_when_actor_configured() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        // Suppress real daemon; spy still records the call before returning None.
        std::env::set_var("KHIVE_NO_DAEMON", "1");
        SPY_WAS_CALLED.with(|c| c.set(false));

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            actor_id: Some("lambda:tenant-x".to_string()), // gate should pass
            ..RuntimeConfig::default()
        };

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg,
            None,
            None, // output_format
            None,
            spy_forward_records_call,
        )
        .await;

        let spy_was_called = SPY_WAS_CALLED.with(|c| c.get());
        SPY_WAS_CALLED.with(|c| c.set(false));

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }

        assert!(
            result.is_ok(),
            "gate must pass when actor is configured; got: {result:?}"
        );
        assert!(
            spy_was_called,
            "spy forward_fn must be called when gate passes (KHIVE_NO_DAEMON=1 causes in-process fallback)"
        );
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
            format: None,
            format_per_op: None,
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
