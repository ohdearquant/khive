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

#[cfg(test)]
use khive_mcp::serve::resolve_runtime_config;
use khive_mcp::serve::{
    apply_env_output_format, build_server_multi_backend_with_db_anchor, config_discovery_db_anchor,
    enforce_strict_actor_mode, install_resolved_blob_store, RuntimeConfigInputs,
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

// The scheduled-event drain now lives in `khive-mcp` (ADR-106: the
// daemon-resident tick needs to call it from `khive-mcp::serve`, which
// cannot depend back on `kkernel`).
use khive_mcp::pending_events;

// ── guarded local construction (cold-boot FTS race, #667/#645) ─────────────
//
// `kkernel mcp --daemon` acquires `khive_runtime::daemon::acquire_daemon_boot_guard()`
// before constructing its runtime/server, holding it across migrations + pack
// schema plans (FTS DDL included) — see `khive-mcp/src/serve.rs::run`. Every
// `kkernel exec` local-dispatch path (the daemon-unreachable/mismatch
// fallback, `--save-file`, `KHIVE_NO_DAEMON=1`, `--ops-file`, and
// `--ops-file --atomic`) also constructs a `KhiveRuntime`/`KhiveMcpServer`
// against the same on-disk database, so it must acquire the SAME guard
// before construction or a concurrent guarded daemon boot can race it.

/// Guard type returned by [`acquire_local_construction_guard`].
#[cfg(unix)]
type LocalConstructionGuard = Option<khive_runtime::daemon::DaemonBootGuard>;
#[cfg(not(unix))]
type LocalConstructionGuard = Option<std::fs::File>;

/// Acquire the daemon boot/recovery guard for a local (non-daemon)
/// `kkernel exec` construction path, fatally — an unavailable lock is a hard
/// error rather than proceeding unguarded, which would reopen the cold-boot
/// FTS race this guard exists to close (#667).
///
/// In-memory databases (`cfg.db_path.is_none()`) need no guard: there is no
/// shared file another process could be racing to initialize. See the
/// `#[cfg(not(unix))]` arm below for the non-unix equivalent.
#[cfg(unix)]
pub(crate) fn acquire_local_construction_guard(
    cfg: &RuntimeConfig,
) -> Result<LocalConstructionGuard> {
    if cfg.db_path.is_none() {
        return Ok(None);
    }
    Ok(Some(
        khive_runtime::daemon::acquire_daemon_boot_guard().context(
            "acquire daemon boot/recovery guard for local kkernel exec construction \
             (another process may be cold-booting the same database)",
        )?,
    ))
}

/// Non-unix mirror of the `#[cfg(unix)]` arm above: no daemon ever boots on
/// this target (`khive_runtime::daemon::run_daemon` is unix-only), so this
/// guard exists purely to serialize *concurrent local-construction* callers
/// against each other (e.g. two overlapping `kkernel exec` invocations, or
/// `--ops-file`/`KHIVE_NO_DAEMON=1` racing a fallback dispatch) — the same
/// cold-boot FTS race #667 closes on unix, just without a daemon on the
/// other end of it.
///
/// Uses `std::fs::File::lock()` (stabilized 1.89, workspace MSRV 1.93) on the
/// SAME lock file path the unix guard uses
/// ([`khive_runtime::daemon::lock_path`]) — a blocking exclusive advisory
/// lock, released when the returned `File` is dropped. On unix this API is
/// documented to correspond exactly to `flock(..., LOCK_EX)`, i.e. the same
/// primitive the unix arm uses directly; here it is the platform-appropriate
/// equivalent (`LockFileEx` w/ `LOCKFILE_EXCLUSIVE_LOCK` on Windows).
#[cfg(not(unix))]
pub(crate) fn acquire_local_construction_guard(
    cfg: &RuntimeConfig,
) -> Result<LocalConstructionGuard> {
    if cfg.db_path.is_none() {
        return Ok(None);
    }
    let path = khive_runtime::daemon::lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create parent directory for construction guard lock file {path:?}")
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open construction guard lock file {path:?}"))?;
    file.lock().context(
        "acquire local construction guard lock for kkernel exec construction \
         (another process may be cold-booting the same database)",
    )?;
    Ok(Some(file))
}

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

    /// Path to a khive TOML config file.
    ///
    /// Overrides the standard cwd/database/home config discovery order and
    /// matches `kkernel mcp` and `kkernel reindex` (`KHIVE_CONFIG`).
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

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

    /// Run the whole ops-file as ONE cross-op atomic unit (ADR-099): every op
    /// commits or the whole file rolls back, with zero partial state either
    /// way. Only valid with `--ops-file`. Only the v1 admissible verb set
    /// (`update`, `delete`, `link`, `merge`, `gtd.transition`, `gtd.complete`)
    /// may appear in the file — an embedding-bearing verb (`create`, ...), a
    /// read verb, or an unlisted verb is rejected before any write. Without
    /// this flag, `--ops-file` behavior is unchanged (chunked, best-effort,
    /// per-op success/failure).
    #[arg(long, requires = "ops_file")]
    pub atomic: bool,

    /// Maximum op count admitted into one `--atomic` unit (ADR-099 D2 defers
    /// the exact threshold to harness measurement; see
    /// `khive_types::pack::ATOMIC_MAX_OPS_DEFAULT` for the interim default
    /// and its rationale). Rejected before any write when exceeded. Only
    /// meaningful with `--atomic`.
    #[arg(long, requires = "atomic")]
    pub atomic_max_ops: Option<usize>,

    /// Exit non-zero when any op in the batch fails (or, for `--ops-file`,
    /// when any applied op fails). Without this flag the process exits 0
    /// whenever the request itself was dispatched, even if every op inside
    /// it failed — the per-op `results` entries and the `summary`/`status`
    /// fields in the printed output are the only signal (#1220). Not
    /// meaningful with `--atomic`, which already fails the whole file on
    /// any rejected op.
    #[arg(long)]
    pub strict: bool,
}

/// A single parsed op entry from an ops-file line.
#[derive(Debug, Clone)]
pub(crate) struct OpsFileEntry {
    pub(crate) tool: String,
    pub(crate) args: serde_json::Value,
}

/// Parse a JSONL ops-file.
///
/// Returns the ordered list of ops, or an error naming the first malformed
/// line.  Blank lines are skipped.
///
/// Each line must be a JSON object `{"tool":"verb","args":{...}}`.  `"args"`
/// is optional and defaults to an empty object.  Any other top-level keys are
/// silently ignored so the format is forward-compatible.
pub(crate) fn parse_ops_file(path: &PathBuf) -> Result<Vec<OpsFileEntry>> {
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

/// Extract the failed entries of one dispatched chunk as `{op_index, tool,
/// error}` objects, with `op_index` global across chunks. A failure summary
/// without the per-op reason strings is unactionable: a gate rejection, a
/// schema error, and a transient failure all look identical, and pipelines
/// that trust the counts alone lose records silently.
fn collect_op_failures(
    parsed: &serde_json::Value,
    applied_before: usize,
) -> Vec<serde_json::Value> {
    let Some(results) = parsed["results"].as_array() else {
        return Vec::new();
    };
    results
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry["ok"].as_bool() == Some(false))
        .map(|(i, entry)| {
            let error = match &entry["error"] {
                serde_json::Value::Null => serde_json::Value::from("unknown error"),
                other => other.clone(),
            };
            serde_json::json!({
                "op_index": applied_before + i,
                "tool": entry["tool"].as_str().unwrap_or("?"),
                "error": error,
            })
        })
        .collect()
}

/// Apply a parsed ops-file against the given server, printing progress to
/// stderr and the final summary to stdout.
async fn apply_ops_file(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    strict: bool,
) -> Result<()> {
    let total = ops.len();
    let mut total_succeeded: usize = 0;
    let mut total_failed: usize = 0;
    let mut failures: Vec<serde_json::Value> = Vec::new();

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
            request_id: None,
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

        for failure in collect_op_failures(&parsed, applied_before) {
            let reason = match &failure["error"] {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            eprintln!(
                "op {} ({}) failed: {reason}",
                failure["op_index"],
                failure["tool"].as_str().unwrap_or("?"),
            );
            failures.push(failure);
        }

        let applied_now = applied_before + chunk.len();
        eprintln!("applied {applied_now}/{total} (ok={total_succeeded}, failed={total_failed})");
    }

    let mut summary = serde_json::json!({
        "total": total,
        "succeeded": total_succeeded,
        "failed": total_failed,
    });
    if !failures.is_empty() {
        summary["failures"] = serde_json::Value::Array(failures);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).expect("serialize summary")
    );
    if strict && total_failed > 0 {
        anyhow::bail!(
            "--strict: {total_failed} op(s) failed out of {total} (see printed summary above)"
        );
    }
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
        let summary = pending_events::run_pending_events_with_config(
            args.db.as_deref(),
            args.config.as_deref(),
            &args.namespace,
            args.verbose,
        )
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
    let (cfg, db_anchor) =
        khive_mcp::serve::resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
            db: args.db.as_deref(),
            config: args.config.as_deref(),
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
            // — and `compute_config_id` never reads identity fields (`actor_id` or
            // `visible_namespaces`; namespace is carried separately per its own doc
            // comment). See the
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

    // Regression fence: `cfg.db_path` must agree with the canonical anchor for
    // this same `--db`/`KHIVE_DB` input, or `compute_config_id` would silently
    // desynchronize `kkernel exec` from the daemon it is trying to reach.
    khive_runtime::assert_captured_db_anchor_consistent(
        cfg.db_path.as_deref(),
        db_anchor.as_deref(),
    )?;
    let db_context = ExecDbContext {
        raw: args.db,
        anchor: db_anchor,
        config: args.config,
    };

    match mode {
        ExecMode::Inline(ops) => {
            run_exec_inline(
                ops,
                cfg,
                args.presentation,
                args.output_format,
                args.save_file,
                db_context,
                args.strict,
            )
            .await
        }
        ExecMode::OpsFile(path) => {
            run_exec_ops_file(
                path,
                cfg,
                args.presentation,
                args.dry_run,
                db_context,
                args.atomic,
                args.atomic_max_ops,
                args.strict,
            )
            .await
        }
    }
}

/// Returns `Err` describing the failed/aborted op counts when `strict` is set
/// and the parsed response envelope's `summary` reports either as non-zero
/// (#1220). `raw` must be the exact envelope string already printed to
/// stdout — the caller prints first, unconditionally, then this decides the
/// exit code; a caller piping the output still sees the full result either way.
fn enforce_strict_batch_result(raw: &str, strict: bool) -> Result<()> {
    if !strict {
        return Ok(());
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        // Non-JSON output (e.g. --output-format table/auto): nothing to
        // inspect here. `--strict` only applies to the default JSON shape.
        return Ok(());
    };
    let failed = parsed["summary"]["failed"].as_u64().unwrap_or(0);
    let aborted = parsed["summary"]["aborted"].as_u64().unwrap_or(0);
    if failed > 0 || aborted > 0 {
        anyhow::bail!(
            "--strict: {failed} op(s) failed, {aborted} op(s) aborted (see printed output above)"
        );
    }
    Ok(())
}

enum ExecMode {
    Inline(String),
    OpsFile(PathBuf),
}

#[derive(Default)]
struct ExecDbContext {
    raw: Option<String>,
    anchor: Option<PathBuf>,
    config: Option<PathBuf>,
}

async fn run_exec_inline(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    db_context: ExecDbContext,
    strict: bool,
) -> Result<()> {
    #[cfg(unix)]
    return run_exec_inline_with_forward(
        ops,
        cfg,
        presentation,
        output_format,
        save_file,
        db_context,
        strict,
        forward_or_spawn_boxed,
    )
    .await;
    #[cfg(not(unix))]
    return run_exec_inline_with_forward(
        ops,
        cfg,
        presentation,
        output_format,
        save_file,
        db_context,
        strict,
    )
    .await;
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
#[allow(clippy::too_many_arguments)]
async fn run_exec_inline_with_forward(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    db_context: ExecDbContext,
    strict: bool,
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

    // Load the resolved `KhiveConfig` ONCE, up front, so both the daemon
    // forward-frame `config_id` below and the in-process fallback's backend
    // topology (further below) resolve from the identical TOML file the
    // daemon's own boot path loads (`serve.rs`'s `build_server`:
    // `KhiveConfig::load_with_home_fallback(args.config.as_deref(),
    // config_discovery_db_anchor(args.db.as_deref()).as_deref())` —
    // The first argument is the same explicit `--config`/`KHIVE_CONFIG`
    // selection used during initial resolution. The second argument is the raw
    // `--db`/`KHIVE_DB` discovery anchor (`None` unless `--db` was set) rather
    // than `cfg.db_path` — `cfg.db_path` materializes the `$HOME/.khive`
    // default when `--db` is unset (#689), which would incorrectly re-anchor
    // tier-3 discovery away from the process cwd.
    //
    // Fixes the config_id topology-drift bug: the forward frame below used to
    // always fold `None` here, while the daemon folds `Some(&khive_cfg)`
    // (`serve.rs`, `compute_config_id(default_runtime.config(),
    // Some(khive_cfg))`). On a config declaring a non-empty `[[backends]]`
    // topology (e.g. a separate `sessions` backend) the two fingerprints
    // diverged, so a correctly-configured client was rejected as a
    // `ConfigMismatch` and silently fell back to the cold in-process path on
    // every call.
    let db_path_for_config = config_discovery_db_anchor(db_context.raw.as_deref());
    let khive_cfg = KhiveConfig::load_with_home_fallback(
        db_context.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    .unwrap_or_default();

    // #1226: apply the same --db/[[backends]] conflict guard the in-process
    // fallback below applies, BEFORE the daemon fast-path — otherwise a warm
    // daemon answers this request without the override ever being checked at
    // all, while the identical override on `--ops-file` (always in-process)
    // correctly rejects it. See `validate_db_override_against_backends`.
    if !khive_cfg.backends.is_empty() {
        khive_mcp::serve::validate_db_override_against_backends(
            db_context.raw.as_deref(),
            khive_cfg.backends.len(),
        )?;
    }

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
            // Fold the SAME backends topology the daemon folds (`Some(&khive_cfg)`)
            // instead of `None` — see the `khive_cfg` load above.
            config_id: compute_config_id(&cfg, Some(&khive_cfg)),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: output_format.clone(),
            format_per_op: None,
            // `kkernel exec` is a trusted operator surface: subhandler verbs are
            // allowed. Only the agent-facing MCP `request` tool sets this true.
            from_wire: false,
            request_id: None,
        };
        if let Some(res) = forward_fn(&frame).await {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            println!("{output}");
            enforce_strict_batch_result(&output, strict)?;
            return Ok(());
        }
    }

    // ── in-process fallback ───────────────────────────────────────────────────
    // Note: enforce_strict_actor_mode was called above before the daemon fast-path;
    // it is not repeated here — the single early check covers both paths.
    //
    // `build_local_fallback_server` resolves the ADR-078 §2 output-format
    // precedence chain (env var over TOML `[runtime] default_output_format`
    // over builtin json) AND honors `[[backends]]` multi-backend topology —
    // see its doc comment.
    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )?;

    let params = RequestParams {
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
        // Tier-1: CLI --output-format overrides the server default (env/builtin).
        format: output_format,
        format_per_op: None,
        request_id: None,
    };

    let output = server
        .dispatch_request_local(params)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{output}");
    enforce_strict_batch_result(&output, strict)?;
    Ok(())
}

/// Build the server used whenever `kkernel exec` dispatches a request locally
/// instead of through the warm daemon (both the fallback and `--ops-file`
/// bulk-apply paths). See
/// `crates/kkernel/docs/design.md#exec-local-dispatch-fallback-server-adr-067-adr-028-8`
/// for why this must agree with the daemon's own multi-backend boot logic.
fn build_local_fallback_server(
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> Result<KhiveMcpServer> {
    // Held across construction below (`KhiveRuntime::new` / `KhiveMcpServer::new`
    // / `build_server_multi_backend`, both of which run migrations and apply
    // pack schema plans synchronously) and dropped when this function returns.
    let _boot_guard = acquire_local_construction_guard(&cfg)?;
    if khive_cfg.backends.is_empty() {
        let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
        // Mirror the `serve` boot path's single-backend branch (ADR-111
        // Amendment 2): without this, `exec`'s in-process fallback server
        // never installs a `BlobStore`, so `blob.put`/`blob.get`/`blob.stat`
        // fail as "unconfigured" here even when `serve` resolves one from
        // the same config and backend (khive#1209).
        install_resolved_blob_store(&rt, khive_cfg, rt.backend())?;
        let env_fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        Ok(KhiveMcpServer::new(rt)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .with_default_output_format(env_fmt))
    } else {
        build_server_multi_backend_with_db_anchor(cfg, khive_cfg, cli_db_override, db_anchor)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_exec_ops_file(
    path: PathBuf,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    dry_run: bool,
    db_context: ExecDbContext,
    atomic: bool,
    atomic_max_ops: Option<usize>,
    strict: bool,
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
    // the round-trip overhead of socket forwarding per chunk). Honors
    // `[[backends]]` multi-backend topology exactly like the daemon-fallback
    // path — see `build_local_fallback_server`.
    enforce_strict_actor_mode(cfg.actor_id.as_deref(), &cfg.packs)?;
    let db_path_for_config = config_discovery_db_anchor(db_context.raw.as_deref());
    let khive_cfg = KhiveConfig::load_with_home_fallback(
        db_context.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    .unwrap_or_default();

    if atomic {
        let max_ops = atomic_max_ops.unwrap_or(khive_types::pack::ATOMIC_MAX_OPS_DEFAULT);
        let envelope = crate::atomic_apply::execute_atomic_ops_file(ops, cfg, &khive_cfg, max_ops)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).expect("serialize atomic envelope")
        );
        return Ok(());
    }

    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )?;

    apply_ops_file(&server, ops, presentation, strict).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serial_test::serial;
    use tempfile::NamedTempFile;

    // ── collect_op_failures: per-op error surfacing (#1228) ───────────────────

    #[test]
    fn collect_op_failures_extracts_reason_with_global_index() {
        let parsed = serde_json::json!({
            "results": [
                {"ok": true, "tool": "create", "result": {}},
                {"ok": false, "tool": "create", "error": "content rejected: suspected credential material"},
                {"ok": false, "tool": "link"},
            ],
            "summary": {"total": 3, "succeeded": 1, "failed": 2}
        });
        let failures = collect_op_failures(&parsed, 500);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["op_index"], 501);
        assert_eq!(failures[0]["tool"], "create");
        assert_eq!(
            failures[0]["error"],
            "content rejected: suspected credential material"
        );
        assert_eq!(failures[1]["op_index"], 502);
        assert_eq!(
            failures[1]["error"], "unknown error",
            "a failed entry with no error value still surfaces a placeholder"
        );
    }

    #[test]
    fn collect_op_failures_preserves_structured_error_payloads() {
        let parsed = serde_json::json!({
            "results": [
                {"ok": false, "tool": "create",
                 "error": {"kind": "invalid_input", "message": "content rejected"}},
            ],
            "summary": {"total": 1, "succeeded": 0, "failed": 1}
        });
        let failures = collect_op_failures(&parsed, 0);
        assert_eq!(
            failures[0]["error"],
            serde_json::json!({"kind": "invalid_input", "message": "content rejected"}),
            "structured KhiveError payloads pass through as JSON, not a placeholder"
        );
    }

    #[test]
    fn collect_op_failures_empty_on_all_ok_or_missing_results() {
        let all_ok = serde_json::json!({
            "results": [{"ok": true, "tool": "stats", "result": {}}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0}
        });
        assert!(collect_op_failures(&all_ok, 0).is_empty());
        assert!(collect_op_failures(&serde_json::json!({}), 0).is_empty());
    }

    // ── HOME isolation for local-fallback tests ───────────────────────────────
    //
    // `build_local_fallback_server` (via `run_exec_inline_with_forward` /
    // `run_exec_ops_file`) now loads `KhiveConfig::load_with_home_fallback`
    // unconditionally, which falls through to `~/.khive/config.toml` (tier 4)
    // when no project-local config is found. Any test that builds a
    // `RuntimeConfig` directly (bypassing `resolve_runtime_config`) with
    // `db_path: None` would otherwise pick up whatever REAL config a
    // developer/CI machine happens to have at `$HOME/.khive/config.toml` —
    // including a genuinely multi-backend one — and silently exercise the
    // multi-backend arm (or open real backend files) instead of the isolated
    // single-backend path the test assumes. Point `HOME` at an empty tempdir
    // for the duration of any such test so `khive_cfg` resolves to
    // `KhiveConfig::default()` deterministically, regardless of the host.
    fn isolate_home_for_test() -> (Option<std::ffi::OsString>, tempfile::TempDir) {
        let prev = std::env::var_os("HOME");
        let dir = tempfile::tempdir().expect("tempdir for isolated HOME");
        std::env::set_var("HOME", dir.path());
        (prev, dir)
    }

    fn restore_home(prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // ── acquire_local_construction_guard: in-memory dbs skip the guard ────────

    #[test]
    #[serial(local_exec_boot_guard)]
    fn acquire_local_construction_guard_is_noop_for_in_memory_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

        let cfg = RuntimeConfig {
            db_path: None,
            ..RuntimeConfig::default()
        };
        let guard = acquire_local_construction_guard(&cfg).expect("in-memory db needs no guard");
        assert!(
            guard.is_none(),
            "an in-memory database has no shared file to serialize construction against"
        );

        std::env::remove_var("KHIVE_LOCK");
    }

    // ── acquire_local_construction_guard: file-backed dbs serialize ──────────
    //
    // Two threads race to acquire the guard for the same file-backed db.
    // Both must succeed (the guard is a blocking exclusive lock, not a
    // try-and-fail check), but their guarded critical sections must never
    // overlap — proven the same way
    // `khive_runtime::daemon::tests::recovery_lock_serializes_two_concurrent_boot_sequences`
    // proves it for the raw primitive: two threads increment/decrement a
    // shared "inside the critical section" counter around a sleep, and a
    // third-thread-visible max-observed-concurrency of 1 is the guarantee.

    #[cfg(unix)]
    #[test]
    #[serial(local_exec_boot_guard)]
    fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers() {
        acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
    }

    #[cfg(not(unix))]
    #[test]
    #[serial(local_exec_boot_guard)]
    fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_nonunix() {
        acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
    }

    fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        let db_path = dir.path().join("cold.db3");

        let concurrent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let spawn_one = |label: &'static str| {
            let db_path = db_path.clone();
            let concurrent = concurrent.clone();
            let max_observed = max_observed.clone();
            std::thread::spawn(move || {
                let cfg = RuntimeConfig {
                    db_path: Some(db_path),
                    ..RuntimeConfig::default()
                };
                let guard = acquire_local_construction_guard(&cfg)
                    .unwrap_or_else(|e| panic!("{label} must acquire the guard: {e}"));

                let now = concurrent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                max_observed.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                concurrent.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

                drop(guard);
            })
        };

        let t_a = spawn_one("writer-a");
        let t_b = spawn_one("writer-b");
        t_a.join().expect("writer-a thread must not panic");
        t_b.join().expect("writer-b thread must not panic");

        assert_eq!(
            max_observed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the two guarded critical sections must never overlap — the guard \
             failed to serialize concurrent local-construction callers"
        );

        std::env::remove_var("KHIVE_LOCK");
    }

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
            // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
            // from the ambient environment (#1276) — every caller of this
            // helper only dispatches `kg` verbs, so the behavior under test
            // shouldn't depend on a wider pack set a developer's shell exports.
            packs: vec!["kg".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime on temp db");
        KhiveMcpServer::new(rt).expect("server on temp db")
    }

    // ── isolated_server ignores ambient KHIVE_PACKS (#1276) ───────────────────
    //
    // `cargo test -p kkernel` failed ~20 exec tests whenever a developer's
    // shell exported `KHIVE_PACKS` naming a pack not compiled into this
    // workspace (e.g. `kg,gtd`): every `RuntimeConfig` built by this test
    // module's shared helpers fell through to `RuntimeConfig::default()`'s
    // `packs` field, which reads that env var, so construction panicked with
    // `PackRegError { unknown: "gtd", .. }`. A unit test's outcome must not
    // depend on ambient shell configuration.
    #[test]
    #[serial(khive_packs_env)]
    fn isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack() {
        let prev = std::env::var("KHIVE_PACKS").ok();
        std::env::set_var("KHIVE_PACKS", "kg,gtd");

        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        // Before the fix, this panicked inside `KhiveMcpServer::new` — the
        // helper inherited the ambient list above instead of pinning its own.
        let _server = isolated_server(&db_path);

        match prev {
            Some(v) => std::env::set_var("KHIVE_PACKS", v),
            None => std::env::remove_var("KHIVE_PACKS"),
        }
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

        // ...but `compute_config_id` never reads identity fields (`actor_id` or
        // `visible_namespaces`; namespace is carried separately per its own doc
        // comment), so the two configs — which differ ONLY in actor_id — must
        // still produce a byte-identical fingerprint. This is the empirical
        // basis for `run_exec` picking `namespace_explicit: true`: it is the
        // conservative, behavior-preserving choice, and it provably does not
        // affect config_id parity with the daemon either way.
        assert_eq!(
            compute_config_id(&with_explicit_true, None),
            compute_config_id(&with_explicit_false, None),
            "namespace_explicit must not affect the daemon-forwarded config_id"
        );
    }

    /// D1-R3: the two tests above are inert to the config_id topology-drift
    /// bug because they always call `compute_config_id(_, None)` on BOTH
    /// sides — omitting the backends topology can never diverge from itself.
    /// This test constructs a genuinely multi-backend `KhiveConfig` (mirroring
    /// the real hosted shape: a `main` backend plus a separate `sessions`
    /// backend, with the `session` pack pinned to it) and proves both that the
    /// pre-fix computation diverges and that the post-fix computation is
    /// byte-identical.
    #[test]
    #[serial]
    fn exec_config_id_matches_serve_config_id_for_multi_backend_topology() {
        use khive_runtime::{BackendConfig, BackendKind, PackConfig};

        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");

        // An explicit nonexistent config path keeps this fully deterministic
        // regardless of host state (same rationale as the sibling test above).
        let missing_config = std::path::PathBuf::from(
            "/nonexistent/khive-exec-parity-test/multi-backend-config.toml",
        );
        let ns = Namespace::parse("local").expect("ns");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(std::path::PathBuf::from("/tmp/khive-parity-main.db")),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "sessions".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(std::path::PathBuf::from("/tmp/khive-parity-sessions.db")),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "session".to_string(),
                    PackConfig {
                        backend: "sessions".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // exec-shaped inputs (namespace_explicit: true — the choice `run_exec` makes).
        let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        // serve-shaped inputs (namespace_explicit: false — the daemon-startup shape).
        let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns,
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve serve-shaped config");

        // Pre-fix proof: the OLD exec-path computation (`compute_config_id(_, None)`,
        // exec.rs:490 before this fix) diverges from the daemon/serve-path
        // computation (`Some(&khive_cfg)`, serve.rs:916) the instant the backends
        // topology is non-empty. This is the exact bug: a legitimately-matching
        // client was rejected as a `ConfigMismatch` and silently fell back to the
        // cold in-process path on every call.
        assert_ne!(
            compute_config_id(&exec_cfg, None),
            compute_config_id(&serve_cfg, Some(&khive_cfg)),
            "pre-fix exec computation (None) must diverge from the daemon computation \
             (Some) for a non-empty backends topology — proves this test catches the \
             real divergence, not a tautology"
        );

        // Post-fix proof: both sides fold the SAME backends topology and produce
        // a byte-identical fingerprint, so the daemon accepts the forwarded frame
        // instead of rejecting it as a ConfigMismatch.
        assert_eq!(
            compute_config_id(&exec_cfg, Some(&khive_cfg)),
            compute_config_id(&serve_cfg, Some(&khive_cfg)),
            "exec-path config_id must match the daemon-path config_id for the same \
             multi-backend topology (D1 fix acceptance gate)"
        );
    }

    // ── build_local_fallback_server multi-backend routing (D1-R2) ────────────
    //
    // Before this fix, both of exec's local-dispatch call sites always built a
    // single-backend runtime pointed at `cfg.db_path`, regardless of any
    // `[[backends]]` declaration in `khive_cfg`. A config pinning a pack (e.g.
    // `comm`) to a separate backend would have that pack's writes silently
    // land in whatever single file `cfg.db_path` pointed at instead of the
    // declared backend file. This test pins `comm` to a second, file-backed
    // `secondary` backend and proves the write lands there — not in `main` —
    // by re-opening each backend file independently afterward.

    /// D1-R2 regression proof: `build_local_fallback_server` must delegate to
    /// `build_server_multi_backend` (not the single-backend `KhiveMcpServer::new`)
    /// whenever `khive_cfg.backends` is non-empty, and pack routing must actually
    /// take effect end to end.
    #[tokio::test]
    #[serial]
    async fn build_local_fallback_server_routes_through_multi_backend_when_backends_declared() {
        use khive_runtime::{BackendConfig, BackendKind, PackConfig};

        let main_db = NamedTempFile::new().expect("main db tempfile");
        let secondary_db = NamedTempFile::new().expect("secondary db tempfile");
        let main_path = main_db.path().to_path_buf();
        let secondary_path = secondary_db.path().to_path_buf();

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "kg".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // `db_path` here is NOT the actual storage location when `[[backends]]`
        // is declared — `build_server_multi_backend` opens each backend's own
        // declared path (the tempfiles above) independently. It is only the
        // identity/fingerprint value `assert_captured_db_anchor_consistent` checks
        // against `resolve_db_anchor(cli_db_override)`, exactly mirroring what
        // a real `kkernel exec` invocation with NO explicit `--db` flag would
        // resolve to (the realistic shape when `[[backends]]` fully governs
        // storage) — see `base_runtime_config_for_multi_backend` in serve.rs's
        // own multi-backend test suite for the identical pattern.
        let cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(None),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            actor_id: Some("actor-routing-test".to_string()),
            ..RuntimeConfig::default()
        };

        // No explicit `--db` override — `[[backends]]` alone governs storage,
        // matching the `cfg.db_path` shape above. An explicit override here
        // would be rejected as ambiguous by `build_registry_for_multi_backend`
        // (ADR-028 §8) since 2 backends are already declared.
        let db_anchor = cfg.db_path.clone();
        let server = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref())
            .expect("multi-backend local fallback must build");

        let create = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="entity", entity_kind="concept", name="routed-via-secondary")"#
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("create must dispatch");
        let create_resp: serde_json::Value = serde_json::from_str(&create).expect("valid JSON");
        assert_eq!(
            create_resp["results"][0]["ok"].as_bool(),
            Some(true),
            "create must succeed through the multi-backend fallback server: {create_resp}"
        );

        // Re-open EACH backend file independently (fresh KhiveMcpServer, no
        // shared state) and list `concept` entities directly against it.
        async fn count_concepts(db_path: &std::path::Path) -> usize {
            let cfg = RuntimeConfig {
                db_path: Some(db_path.to_path_buf()),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let rt = KhiveRuntime::new(cfg).expect("runtime on backend file");
            let probe = KhiveMcpServer::new(rt).expect("server on backend file");
            let raw = probe
                .dispatch_request_local(RequestParams {
                    ops: r#"list(kind="concept")"#.to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                })
                .await
                .expect("list must dispatch");
            let resp: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
            resp["results"][0]["result"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0)
        }

        let main_count = count_concepts(&main_path).await;
        let secondary_count = count_concepts(&secondary_path).await;

        assert_eq!(
            main_count, 0,
            "kg pack must NOT write into the `main` backend file when pinned to \
             `secondary` (D1-R2: a silent single-backend fallback would have written \
             it here instead)"
        );
        assert_eq!(
            secondary_count, 1,
            "kg pack write must land in its declared `secondary` backend file"
        );
    }

    #[test]
    #[serial]
    fn build_local_fallback_server_uses_captured_anchor_after_home_changes() {
        let (previous_home, _first_home) = isolate_home_for_test();
        let cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(None),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let db_anchor = cfg.db_path.clone();
        let khive_cfg = KhiveConfig {
            backends: vec![khive_runtime::BackendConfig {
                name: "main".to_string(),
                kind: khive_runtime::BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };
        let second_home = tempfile::tempdir().expect("second HOME");
        std::env::set_var("HOME", second_home.path());

        let result = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref());
        restore_home(previous_home);

        assert!(
            result.is_ok(),
            "exec fallback must use the anchor captured with RuntimeConfig after HOME changes: {}",
            result.err().unwrap()
        );
    }

    // ── single-backend fallback installs a BlobStore (khive#1209) ────────────
    //
    // Before this fix, `build_local_fallback_server`'s single-backend branch
    // constructed `KhiveRuntime`/`KhiveMcpServer` without ever calling
    // `install_resolved_blob_store`, so `blob.*` verbs dispatched through
    // `kkernel exec`'s in-process fallback always saw an unconfigured
    // `BlobStore` even when the same config/backend combination resolves one
    // for the `serve` daemon boot path. `KhiveMcpServer` does not expose its
    // wrapped runtime, so this asserts the same *observable* side effect the
    // `serve` path's own tests rely on: `FsBlobStore::new` (khive-db
    // `stores/blob.rs`) creates its root directory eagerly. With no
    // `[storage.blob]` config and no `KHIVE_BLOB_ROOT`, resolution falls
    // back to `<db_dir>/blobs` — that directory existing after construction
    // is proof the install call ran.
    #[test]
    fn build_local_fallback_server_installs_blob_store_single_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("exec_blob.db");
        let cfg = RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        };
        let khive_cfg = KhiveConfig::default();

        let _server = build_local_fallback_server(cfg, &khive_cfg, None, None)
            .expect("single-backend local-exec construction must succeed");

        assert!(
            dir.path().join("blobs").is_dir(),
            "default <db_dir>/blobs root must exist after construction, proving \
             install_resolved_blob_store ran for the single-backend fallback path"
        );
    }

    // ── guarded local construction races a guarded boot (#667/#645) ──────────
    //
    // Mirrors `khive-runtime/tests/cold_boot_fts_race.rs`'s deterministic
    // two-thread pattern, but races a `kkernel mcp --daemon`-style guarded
    // boot against `build_local_fallback_server` itself — the exact local
    // path that, before this fix, constructed `KhiveRuntime`/`KhiveMcpServer`
    // without acquiring the boot guard at all. Both "boots" target the SAME
    // fresh (cold) db file; if either side ran unguarded, migrations/FTS DDL
    // could interleave and corrupt (or lose rows from) the `fts_notes` index.

    #[cfg(unix)]
    fn run_one_guarded_daemon_boot(
        db_path: std::path::PathBuf,
        writer_label: &'static str,
        count: usize,
    ) {
        let guard =
            khive_runtime::daemon::acquire_recovery_lock().expect("acquire daemon boot guard");

        let rt_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build per-thread tokio runtime");

        rt_handle.block_on(async {
            let rt = KhiveRuntime::new(RuntimeConfig {
                db_path: Some(db_path),
                embedding_model: None,
                additional_embedding_models: vec![],
                ..RuntimeConfig::default()
            })
            .expect("cold-boot migrations succeed");
            let token = rt.authorize(Namespace::local()).expect("authorize local");

            for i in 0..count {
                rt.create_note(
                    &token,
                    "memo",
                    None,
                    &format!("{writer_label} note {i} — boot race marker"),
                    None,
                    None,
                    vec![],
                )
                .await
                .expect("note write must succeed inside the guarded boot window");
            }
        });

        drop(guard);
    }

    #[cfg(unix)]
    fn run_one_local_exec_construction(
        db_path: std::path::PathBuf,
        writer_label: &'static str,
        count: usize,
    ) {
        let rt_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build per-thread tokio runtime");

        rt_handle.block_on(async {
            let cfg = RuntimeConfig {
                db_path: Some(db_path),
                embedding_model: None,
                additional_embedding_models: vec![],
                // Pin the pack list explicitly rather than inheriting
                // `KHIVE_PACKS` from the ambient environment (#1276) — this
                // race only exercises `kg` writes.
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let khive_cfg = KhiveConfig::default();
            // The exact call site under test: before this fix, this function
            // built `KhiveRuntime`/`KhiveMcpServer` without acquiring any
            // guard, so it could run migrations/FTS DDL concurrently with
            // the guarded boot above against the same file.
            let server = build_local_fallback_server(cfg, &khive_cfg, None, None)
                .expect("guarded local-exec construction must succeed");

            for i in 0..count {
                let params = RequestParams {
                    ops: format!(
                        r#"create(kind="observation", content="{writer_label} note {i} — boot race marker")"#
                    ),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                };
                let raw = server
                    .dispatch_request_local(params)
                    .await
                    .expect("dispatch must succeed inside the guarded construction window");
                let resp: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
                assert_eq!(
                    resp["results"][0]["ok"],
                    serde_json::json!(true),
                    "write must succeed: {resp}"
                );
            }
        });
    }

    // ── deterministic lock-blocking oracle ────────────────────────────────────
    //
    // The end-to-end race test below proves no corruption results when both
    // sides respect the guard, but a mutation-testing pass showed its
    // final-row-count oracle does NOT fail if the guard at
    // `build_local_fallback_server`'s call site is removed entirely: with no
    // second real lock-holder racing it, the row count comes out right either
    // way, so the test cannot tell "guarded" from "unguarded". This test
    // closes that gap: it holds the SAME recovery lock the guard acquires
    // from the test thread itself, then asserts `build_local_fallback_server`
    // cannot complete construction while that lock is held (bounded wait) —
    // an assertion that is trivially true when the guard is unguarded (it
    // never acquires anything, so it isn't blocked by our held lock).
    #[cfg(unix)]
    #[test]
    #[serial(local_exec_boot_guard)]
    fn build_local_fallback_server_blocks_while_recovery_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_file = dir.path().join("khived.recovery.lock");
        std::env::set_var("KHIVE_LOCK", &lock_file);

        let db_path = dir.path().join("guard_block_test.db3");

        // A separate file descriptor to the SAME lock path — flock's
        // blocking semantics apply per open-file-description, so this
        // blocks a second acquirer even from another thread in this same
        // process (the same pattern `daemon.rs`'s own
        // `recovery_lock_serializes_two_concurrent_boot_sequences` and
        // `cold_boot_fts_race.rs` rely on).
        let held_guard =
            khive_runtime::daemon::acquire_recovery_lock().expect("acquire recovery lock in test");

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt_handle = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build per-thread tokio runtime");
            let cfg = RuntimeConfig {
                db_path: Some(db_path),
                embedding_model: None,
                additional_embedding_models: vec![],
                // Pin the pack list explicitly rather than inheriting
                // `KHIVE_PACKS` from the ambient environment (#1276).
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let khive_cfg = KhiveConfig::default();
            // The exact call under test: every non-atomic local-exec path
            // (daemon-unreachable fallback, --save-file, KHIVE_NO_DAEMON=1,
            // non-atomic --ops-file) funnels through this one function.
            let result = rt_handle
                .block_on(async { build_local_fallback_server(cfg, &khive_cfg, None, None) });
            // Sent only AFTER construction returns — the test observes
            // whether this arrives before or after the lock is released.
            let _ = tx.send(());
            result
        });

        // Bounded wait: construction must NOT complete while the lock is
        // held. If the production guard at `build_local_fallback_server`'s
        // call site is ever removed or no-op'd, nothing blocks this thread
        // and the signal arrives well inside this window — this is the
        // mutation-killing assertion.
        let completed_while_locked = rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok();
        assert!(
            !completed_while_locked,
            "build_local_fallback_server must NOT complete while the boot/recovery \
             lock is held by another holder — if this fires, the guard at its \
             production call site has been removed or stopped acquiring the shared lock"
        );

        drop(held_guard);

        handle
            .join()
            .expect("construction thread must not panic")
            .expect("construction must succeed once the lock is released");

        std::env::remove_var("KHIVE_LOCK");
    }

    // Named serial key (not the bare `#[serial]` default): this test only
    // touches `KHIVE_LOCK`, not the `KHIVE_REQUIRE_ATTRIBUTED_ACTOR` /
    // `KHIVE_NO_DAEMON` / `HOME` vars the default-keyed `#[serial]` tests
    // above guard. Sharing their queue would only add wall-clock delay
    // (this test spawns two real OS threads doing real `flock` + migrations)
    // without protecting anything — and empirically DOES perturb unrelated
    // non-serial tests elsewhere in this binary (`pending_events`) that race
    // on those other env vars.
    #[cfg(unix)]
    #[test]
    #[serial(local_exec_boot_guard)]
    fn local_exec_construction_races_guarded_daemon_boot_without_fts_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_file = dir.path().join("khived.recovery.lock");
        std::env::set_var("KHIVE_LOCK", &lock_file);

        // Fresh (cold) database file — neither side has run migrations on it yet.
        let db_path = dir.path().join("local_exec_boot_race.db3");

        const PER_WRITER: usize = 10;
        let path_a = db_path.clone();
        let path_b = db_path.clone();

        let t_a = std::thread::spawn(move || {
            run_one_guarded_daemon_boot(path_a, "daemon-boot", PER_WRITER)
        });
        let t_b = std::thread::spawn(move || {
            run_one_local_exec_construction(path_b, "local-exec", PER_WRITER)
        });
        t_a.join().expect("daemon-boot thread must not panic");
        t_b.join().expect("local-exec thread must not panic");

        let rt_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build verification tokio runtime");
        rt_handle.block_on(async {
            let verify_rt = KhiveRuntime::new(RuntimeConfig {
                db_path: Some(db_path.clone()),
                embedding_model: None,
                additional_embedding_models: vec![],
                ..RuntimeConfig::default()
            })
            .expect("post-race runtime opens cleanly");
            let token = verify_rt
                .authorize(Namespace::local())
                .expect("authorize local");

            let hits = verify_rt
                .search_notes(
                    &token,
                    "boot race marker",
                    None,
                    100,
                    None,
                    false,
                    &[],
                    None,
                )
                .await
                .expect("FTS search over notes must succeed, not error on a corrupted index");
            assert_eq!(
                hits.len(),
                PER_WRITER * 2,
                "every planted note from both writers must be present and \
                 FTS-searchable — a corrupted/partial index would drop or \
                 duplicate rows: {hits:?}"
            );
        });

        std::env::remove_var("KHIVE_LOCK");
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
        apply_ops_file(&server, ops, None, false).await.unwrap();

        // Verify all 3 entities are present.
        let params = RequestParams {
            ops: r#"list(kind="concept")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
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

    // ── #1220: --strict exit-code signal for partially-failed batches ─────────

    #[test]
    fn enforce_strict_batch_result_ok_when_strict_off_regardless_of_failures() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 1, "succeeded": 0, "failed": 1, "aborted": 0},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, false).is_ok());
    }

    #[test]
    fn enforce_strict_batch_result_ok_when_strict_on_and_nothing_failed() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 2, "succeeded": 2, "failed": 0, "aborted": 0},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, true).is_ok());
    }

    #[test]
    fn enforce_strict_batch_result_errs_when_strict_on_and_a_failure_present() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
        })
        .to_string();
        let err = enforce_strict_batch_result(&raw, true).unwrap_err();
        assert!(format!("{err}").contains("1 op(s) failed"));
    }

    #[test]
    fn enforce_strict_batch_result_errs_when_strict_on_and_chain_aborted() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 2, "succeeded": 0, "failed": 1, "aborted": 1},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, true).is_err());
    }

    #[test]
    fn enforce_strict_batch_result_errs_on_save_manifest_with_failures() {
        // The save-file path prints a manifest, not the raw envelope; the
        // manifest carries the envelope's summary through (khive-mcp
        // save_sink) precisely so --strict works on this path too.
        let raw = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":1,"failed":1,"aborted":0}}"#;
        assert!(enforce_strict_batch_result(raw, true).is_err());
        let clean = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":2,"failed":0,"aborted":0}}"#;
        assert!(enforce_strict_batch_result(clean, true).is_ok());
    }

    #[test]
    fn enforce_strict_batch_result_ok_on_non_json_output() {
        // --output-format table/auto renders a non-JSON string; --strict has
        // nothing to inspect and must not itself error out on that shape.
        assert!(enforce_strict_batch_result("| a | b |\n", true).is_ok());
    }

    #[tokio::test]
    async fn apply_ops_file_strict_errs_when_an_op_fails() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);

        // First op succeeds; second targets an unknown kind and fails.
        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write as _;
        f.write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"StrictOne\"}}\n",
        )
        .unwrap();
        f.write_all(
            b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n",
        )
        .unwrap();

        let ops = parse_ops_file(&f.path().to_path_buf()).unwrap();
        assert_eq!(ops.len(), 2);

        let err = apply_ops_file(&server, ops, None, true)
            .await
            .expect_err("strict mode must surface the per-op failure as a process error");
        assert!(format!("{err}").contains("1 op(s) failed"));
    }

    // ── ADR-099 B1 inertness (golden shape) ────────────────────────────────────
    //
    // B1 adds only new, unconsumed types (khive-types atomic admissibility
    // metadata, khive-runtime atomic-plan data, khive-request's parse-time
    // check). None of them are wired into `dispatch_request_local` or
    // `apply_ops_file` — this test pins the non-atomic response envelope's
    // shape so a later slice that DOES wire `--atomic` in cannot silently
    // change today's default (non-atomic) output. The op sequence below
    // (create → update → link → get) is the representative mix named in the
    // task: a create, a mutation, a graph edge, and a read, run back-to-back
    // through the same in-process dispatch path bulk apply uses.
    #[tokio::test]
    async fn non_atomic_dispatch_envelope_shape_is_unchanged_by_adr099_b1() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);

        async fn dispatch(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
            let params = RequestParams {
                ops: ops.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            };
            let raw = server
                .dispatch_request_local(params)
                .await
                .unwrap_or_else(|e| panic!("dispatch {ops:?} failed: {e}"));
            serde_json::from_str(&raw).expect("valid JSON")
        }

        // create
        let created = dispatch(
            &server,
            r#"create(kind="concept", name="ADR-099-B1-inertness")"#,
        )
        .await;
        assert_golden_envelope_shape(&created, "create");
        let entity_id = created["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return an id")
            .to_string();

        // update
        let updated = dispatch(
            &server,
            &format!(r#"update(id="{entity_id}", description="updated by inertness test")"#),
        )
        .await;
        assert_golden_envelope_shape(&updated, "update");

        // link (self-referential edge is rejected by endpoint validation for
        // most relations, so create a second entity as the link target)
        let target = dispatch(&server, r#"create(kind="concept", name="link-target")"#).await;
        let target_id = target["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return an id")
            .to_string();
        let linked = dispatch(
            &server,
            &format!(
                r#"link(source_id="{entity_id}", target_id="{target_id}", relation="extends")"#
            ),
        )
        .await;
        assert_golden_envelope_shape(&linked, "link");

        // get (read)
        let got = dispatch(&server, &format!(r#"get(id="{entity_id}")"#)).await;
        assert_golden_envelope_shape(&got, "get");

        // Every op above succeeded end-to-end with zero surprises in the
        // envelope shape — this is the inertness pin: no `atomic` key
        // appeared anywhere, `summary` kept exactly its 4 pre-existing
        // fields on every response, and every op's own result still nests
        // under `results[0].result` as before.
    }

    /// Asserts a `dispatch_request_local` response matches the pre-ADR-099-B1
    /// golden shape: exactly the top-level keys `results` and `summary` (no
    /// additive `atomic` block — that is a future, opt-in-only slice), a
    /// `summary` with exactly `total`/`succeeded`/`failed`/`aborted`, and a
    /// successful single-op `results[0]` carrying `ok`/`tool`/`result`.
    fn assert_golden_envelope_shape(resp: &serde_json::Value, expected_tool: &str) {
        let top_level_keys: std::collections::BTreeSet<&str> = resp
            .as_object()
            .expect("response must be a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top_level_keys,
            std::collections::BTreeSet::from(["results", "summary", "status"]),
            "non-atomic envelope must carry exactly results+summary+status, no `atomic` block (#1220 added `status`): {resp}"
        );

        let summary_keys: std::collections::BTreeSet<&str> = resp["summary"]
            .as_object()
            .expect("summary must be an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            summary_keys,
            std::collections::BTreeSet::from(["total", "succeeded", "failed", "aborted"]),
            "summary shape must be unchanged: {resp}"
        );
        assert_eq!(resp["summary"]["total"], serde_json::json!(1));
        assert_eq!(resp["summary"]["succeeded"], serde_json::json!(1));
        assert_eq!(resp["summary"]["failed"], serde_json::json!(0));

        assert_eq!(resp["results"][0]["ok"], serde_json::json!(true));
        assert_eq!(resp["results"][0]["tool"], serde_json::json!(expected_tool));
        assert!(
            resp["results"][0].get("result").is_some(),
            "results[0] must carry a `result` field: {resp}"
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
        run_exec_ops_file(
            path.clone(),
            cfg.clone(),
            None,
            true,
            ExecDbContext::default(),
            false,
            None,
            false,
        )
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
            request_id: None,
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

    /// Complement: strict mode must NOT reject when a pack is loaded and an actor
    /// IS configured — the daemon fast-path must remain available in that case.
    #[tokio::test]
    #[serial]
    async fn strict_mode_allows_exec_when_actor_configured() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
        let (prev_home, _home_dir) = isolate_home_for_test();

        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        std::env::set_var("KHIVE_NO_DAEMON", "1"); // force in-process to avoid daemon dep

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            actor_id: Some("lambda:tenant-x".to_string()), // actor configured → no gate
            ..RuntimeConfig::default()
        };

        // The strict gate must pass; the actual dispatch will succeed (stats() is safe).
        let result = run_exec_inline(
            "stats()".to_string(),
            cfg,
            None,
            None,
            None,
            ExecDbContext::default(),
            false,
        )
        .await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }
        restore_home(prev_home);

        assert!(
            result.is_ok(),
            "run_exec_inline must succeed under strict mode when actor IS configured; got: {result:?}"
        );
    }

    /// Default-off regression: when KHIVE_REQUIRE_ATTRIBUTED_ACTOR is unset,
    /// run_exec_inline must NOT reject even with no actor (OSS default path).
    #[tokio::test]
    #[serial]
    async fn strict_mode_off_exec_inline_passes_with_no_actor() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
        let (prev_home, _home_dir) = isolate_home_for_test();

        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"); // default OFF
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            actor_id: None,
            ..RuntimeConfig::default()
        };

        let result = run_exec_inline(
            "stats()".to_string(),
            cfg,
            None,
            None,
            None,
            ExecDbContext::default(),
            false,
        )
        .await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        match prev_no_daemon {
            Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }
        restore_home(prev_home);

        assert!(
            result.is_ok(),
            "run_exec_inline must NOT reject when strict mode is OFF (OSS default); got: {result:?}"
        );
    }

    // ── D1-R3 (end-to-end): exec frame config_id vs. daemon config_id ────────
    //
    // `exec_config_id_matches_serve_config_id_for_multi_backend_topology` above
    // proves `compute_config_id` folds the topology identically for exec-shaped
    // and serve-shaped `RuntimeConfig`s — but it constructs both arms manually
    // and never calls `run_exec_inline_with_forward` itself, so it would not
    // notice a revert of the actual `compute_config_id(&cfg, Some(&khive_cfg))`
    // call at the real call site above. This test closes that gap: it drives
    // `run_exec_inline_with_forward` for real, against a project-local
    // `.khive/config.toml` that declares a genuine multi-backend topology, and
    // captures the DAEMON REQUEST FRAME's actual `config_id` via a spy — the
    // exact value that would be sent over the wire to a real daemon.

    #[cfg(unix)]
    std::thread_local! {
        static SPY_CAPTURED_CONFIG_ID: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    fn spy_capture_config_id(frame: &DaemonRequestFrame) -> super::ForwardFuture<'_> {
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
        Box::pin(async { None })
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn exec_frame_config_id_matches_daemon_config_id_for_multi_backend_project_toml() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let (prev_home, home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

        // No explicit `--db` anywhere below — this mirrors the real multi-tenant
        // deployment shape the bug affects: `~/.khive/config.toml` declares
        // `[[backends]]` and `kkernel exec` relies on default discovery. An
        // explicit `--db` would itself be rejected as ambiguous once backends
        // are declared (ADR-028 §8, `build_registry_for_multi_backend`), so it
        // is not a legitimate way to reach this scenario — default discovery is.
        let khive_dir = home_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
        let main_backend_path = khive_dir.join("main-backend.db");
        let sessions_backend_path = khive_dir.join("sessions-backend.db");
        std::fs::write(
            khive_dir.join("config.toml"),
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"

[packs.session]
backend = "sessions"
"#,
                main_backend_path.display(),
                sessions_backend_path.display(),
            ),
        )
        .expect("write multi-backend config.toml");

        // `no_embed: true` keeps this test fast and network-independent — it is
        // scoped to the backends-topology fold, not embedding-model resolution
        // (a separate, already-covered concern in the sibling project-toml test).
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
            // from the ambient environment (#1276) — this test's assertion is
            // about config_id parity, not about pack resolution.
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg,
            None,
            None,
            None,
            ExecDbContext::default(),
            false,
            spy_capture_config_id,
        )
        .await;
        assert!(result.is_ok(), "exec dispatch must succeed: {result:?}");

        let captured = SPY_CAPTURED_CONFIG_ID
            .with(|c| c.borrow_mut().take())
            .expect("spy must have captured a forwarded frame");

        // Independently compute what the DAEMON would compute for the exact
        // same on-disk config.toml + database, mirroring serve.rs's own boot
        // path (`build_server`): resolve_runtime_config with
        // namespace_explicit=false (the daemon-startup shape), load the same
        // KhiveConfig, and fold it with Some(&khive_cfg) exactly like
        // serve.rs:916 does.
        let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            // Same pin as `cfg` above (#1276) — both sides of the parity
            // comparison must resolve identically regardless of ambient
            // `KHIVE_PACKS`.
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve serve-shaped config");
        let khive_cfg = KhiveConfig::load_with_home_fallback(None, serve_cfg.db_path.as_deref())
            .expect("load multi-backend config.toml")
            .expect("config.toml must be found at tier 3");
        assert!(
            !khive_cfg.backends.is_empty(),
            "sanity: the written config.toml must actually resolve with a non-empty \
             backends list, or this test proves nothing"
        );
        let daemon_config_id = compute_config_id(&serve_cfg, Some(&khive_cfg));
        restore_home(prev_home);

        assert_eq!(
            captured, daemon_config_id,
            "the config_id in the ACTUAL frame run_exec_inline_with_forward sends to the \
             daemon must be byte-identical to what the daemon computes for the same \
             multi-backend config.toml (D1 acceptance gate, exercised end-to-end through \
             the real call site rather than a standalone compute_config_id comparison)"
        );
    }

    // ── #1226: inline --db/[[backends]] guard must fire before daemon-forward ──

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn inline_db_override_conflicting_with_backends_is_rejected_before_daemon_forward() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let (prev_home, home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

        let khive_dir = home_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
        let main_backend_path = khive_dir.join("main-backend.db");
        let sessions_backend_path = khive_dir.join("sessions-backend.db");
        std::fs::write(
            khive_dir.join("config.toml"),
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"

[packs.session]
backend = "sessions"
"#,
                main_backend_path.display(),
                sessions_backend_path.display(),
            ),
        )
        .expect("write multi-backend config.toml");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        let conflicting_override = khive_dir.join("override.db");
        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg,
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(conflicting_override.display().to_string()),
                anchor: None,
                config: None,
            },
            false,
            spy_capture_config_id,
        )
        .await;
        restore_home(prev_home);

        assert!(
            result.is_err(),
            "a --db/KHIVE_DB override that conflicts with a declared [[backends]] topology \
             must be rejected on the inline path too, not only on --ops-file; got: {result:?}"
        );
        assert!(
            SPY_CAPTURED_CONFIG_ID.with(|c| c.borrow().is_none()),
            "the conflict must be caught BEFORE any daemon-forward attempt — the spy must \
             never have been called"
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
            request_id: None,
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

    // ── ADR-099 B3: `--atomic` CLI surface acceptance tests ───────────────────

    fn atomic_op(tool: &str, args: serde_json::Value) -> OpsFileEntry {
        OpsFileEntry {
            tool: tool.to_string(),
            args,
        }
    }

    async fn dispatch_json(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
        // Verbose presentation: the default Agent mode truncates entity ids
        // to an 8-char short form for readability, which the atomic prepare
        // path (and every KG verb) rejects as "not a full UUID". Tests here
        // need the real id back out so it can feed straight into `update`/
        // `delete`/`link` args.
        let params = RequestParams {
            ops: ops.to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn atomic_cfg(db_path: &str) -> RuntimeConfig {
        RuntimeConfig {
            db_path: Some(PathBuf::from(db_path)),
            embedding_model: None,
            additional_embedding_models: vec![],
            // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
            // from the ambient environment (#1276) — every atomic-apply test
            // using this helper only dispatches `kg` verbs.
            packs: vec!["kg".to_string()],
            ..Default::default()
        }
    }

    /// Acceptance test 1a: an all-success atomic ops-file run commits every
    /// op as one unit and the results are visible afterward.
    #[tokio::test]
    async fn atomic_ops_file_success_commits_all_ops() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (x_id, y_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="AtomicX"), create(kind="concept", name="AtomicY")]"#,
            )
            .await;
            let x_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("x id")
                .to_string();
            let y_id = resp["results"][1]["result"]["id"]
                .as_str()
                .expect("y id")
                .to_string();
            (x_id, y_id)
        };

        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": x_id, "name": "AtomicX-renamed"}),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": y_id, "name": "AtomicY-renamed"}),
            ),
        ];

        let khive_cfg = KhiveConfig::default();
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("atomic run must succeed");

        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let server = isolated_server(&db_path);
        let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
        let y_resp = dispatch_json(&server, &format!(r#"get(id="{y_id}")"#)).await;
        assert_eq!(x_resp["results"][0]["result"]["name"], "AtomicX-renamed");
        assert_eq!(y_resp["results"][0]["result"]["name"], "AtomicY-renamed");
    }

    /// Acceptance test 1b: a mid-unit failure rolls the WHOLE unit back —
    /// zero partial state, including the op that "succeeded" before the
    /// failing one.
    ///
    /// Shape: `x` and `y` both exist. Op 0 hard-deletes `x`. Op 1 links `y`
    /// to `x`. At PREPARE time (before either op runs) `x` still exists, so
    /// both plans build successfully. At COMMIT time op 0 removes `x` first,
    /// then op 1's guarded `INSERT ... WHERE EXISTS` affects zero rows (the
    /// dangling-edge guard, ADR-099 D1 rule 1) — the whole unit rolls back,
    /// so `x`'s deletion is undone too.
    #[tokio::test]
    async fn atomic_ops_file_mid_unit_failure_rolls_back_whole_unit() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (x_id, y_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="RollbackX"), create(kind="concept", name="RollbackY")]"#,
            )
            .await;
            let x_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("x id")
                .to_string();
            let y_id = resp["results"][1]["result"]["id"]
                .as_str()
                .expect("y id")
                .to_string();
            (x_id, y_id)
        };

        let ops = vec![
            atomic_op("delete", serde_json::json!({"id": x_id, "hard": true})),
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": y_id,
                    "target_id": x_id,
                    "relation": "extends",
                }),
            ),
        ];

        let khive_cfg = KhiveConfig::default();
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("the seam call itself must not error — the unit rolls back cleanly");

        assert_eq!(
            envelope["atomic"]["rolled_back"], true,
            "envelope: {envelope}"
        );
        assert_eq!(
            envelope["atomic"]["failed_op_index"], 1,
            "envelope: {envelope}"
        );

        let server = isolated_server(&db_path);
        let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
        assert!(
            x_resp["results"][0]["result"]["deleted_at"].is_null(),
            "x must NOT be deleted — the whole unit must have rolled back: {x_resp}"
        );
    }

    /// ADR-099 B3 (second half): the inverse
    /// same-unit race — `[link(A, B, competes_with), update(X
    /// extends A-B -> competes_with)]`, where the CANONICAL row the update
    /// conflict-absorbs into is created by an EARLIER op in the SAME
    /// atomic unit (so it does not exist at either op's prepare time). The
    /// commit must both write correctly (X deleted, the just-linked row
    /// carries X's patch) and RENDER the correct surviving id — not X's
    /// prepare-time-advisory id, which this fix removed reliance on
    /// entirely (`build_op_result` now derives it from a post-commit
    /// natural-key lookup).
    #[tokio::test]
    async fn atomic_symmetric_update_absorbs_into_same_unit_link_and_renders_correct_id() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (a_id, b_id, x_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="LinkRaceA"), create(kind="concept", name="LinkRaceB")]"#,
            )
            .await;
            let a_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("a id")
                .to_string();
            let b_id = resp["results"][1]["result"]["id"]
                .as_str()
                .expect("b id")
                .to_string();

            let link_resp = dispatch_json(
                &server,
                &format!(
                    r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
                ),
            )
            .await;
            let x_id = link_resp["results"][0]["result"]["id"]
                .as_str()
                .expect("x id")
                .to_string();
            (a_id, b_id, x_id)
        };

        let ops = vec![
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": a_id,
                    "target_id": b_id,
                    "relation": "competes_with",
                    "weight": 0.6,
                }),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
            ),
        ];

        let khive_cfg = KhiveConfig::default();
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("atomic run must succeed");

        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let linked_id = envelope["results"][0]["result"]["id"]
            .as_str()
            .expect("link result id")
            .to_string();
        let rendered_update_id = envelope["results"][1]["result"]["id"]
            .as_str()
            .expect("update result id")
            .to_string();
        assert_ne!(
            rendered_update_id, x_id,
            "the update's rendered result must NOT be X's stale requested id: {envelope}"
        );
        assert_eq!(
            rendered_update_id, linked_id,
            "the update's rendered result must be the surviving (just-linked) row: {envelope}"
        );
        assert_eq!(
            envelope["results"][1]["result"]["weight"], 0.9,
            "the surviving row must carry the update's patch: {envelope}"
        );

        let server = isolated_server(&db_path);
        let surviving_resp = dispatch_json(&server, &format!(r#"get(id="{linked_id}")"#)).await;
        assert_eq!(
            surviving_resp["results"][0]["result"]["weight"], 0.9,
            "the committed row itself must carry the patch: {surviving_resp}"
        );
    }

    /// Acceptance test 2: every CLI-boundary rejection fires BEFORE any
    /// write — each sub-case asserts both the error and that the db stays
    /// empty (zero entities created).
    #[tokio::test]
    async fn atomic_cli_boundary_rejections_happen_before_any_write() {
        let khive_cfg = KhiveConfig::default();

        // (a) embedding-bearing verb.
        {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![atomic_op(
                "create",
                serde_json::json!({"kind": "concept", "name": "ShouldNotLand"}),
            )];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .expect_err("embedding-bearing verb must be rejected");
            assert!(
                format!("{err:#}").contains("embedding-bearing"),
                "error: {err:#}"
            );
            let server = isolated_server(&db_path);
            let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
            assert_eq!(resp["results"][0]["result"].as_array().unwrap().len(), 0);
        }

        // (b) read verb.
        {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![atomic_op("search", serde_json::json!({"query": "x"}))];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .expect_err("read verbs must be rejected");
            assert!(format!("{err:#}").contains("read"), "error: {err:#}");
        }

        // (c) unlisted verb.
        {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![atomic_op("not_a_real_verb", serde_json::json!({}))];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .expect_err("unlisted verbs must be rejected");
            assert!(
                format!("{err:#}").contains("not on the v1 atomic-admissible"),
                "error: {err:#}"
            );
        }

        // (d) op-count guard.
        {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![
                atomic_op(
                    "update",
                    serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
                ),
                atomic_op(
                    "update",
                    serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
                ),
                atomic_op(
                    "update",
                    serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
                ),
            ];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                2,
            )
            .await
            .expect_err("exceeding max_ops must be rejected");
            assert!(
                format!("{err:#}").contains("exceeds the configured maximum"),
                "error: {err:#}"
            );
        }

        // (e) governance verbs (`propose`/`review`/`withdraw`) — ADR-099 B3:
        // these are on the v1 admissible list
        // (ADR-099 D3 intends them to gain a seam) but have no prepare/apply
        // implementation in this slice yet. They must be rejected at this
        // SAME pre-runtime static guard — never reaching `KhiveRuntime::new`
        // or any write — not deferred to fail later inside `prepare_op`.
        for verb in ["propose", "review", "withdraw"] {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![atomic_op(
                verb,
                serde_json::json!({"title": "x", "description": "y", "changeset": {}}),
            )];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .expect_err(&format!("{verb:?} must be rejected before any write"));
            assert!(
                format!("{err:#}").contains("no --atomic prepare/apply seam"),
                "error for {verb:?}: {err:#}"
            );
            // No runtime/db file activity: the db stays empty (nothing else
            // touched it, so a plain re-open with the same path must show a
            // fresh, unwritten store).
            let server = isolated_server(&db_path);
            let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
            assert_eq!(
                resp["results"][0]["result"].as_array().unwrap().len(),
                0,
                "no write must have landed for {verb:?}"
            );
        }

        // (f) `merge` — ADR-099 B3: deferred at this SAME pre-runtime static guard rather than shipped
        // with partial parity. Must name the non-atomic merge verb as the
        // supported route, and must not reach `KhiveRuntime::new`/any write.
        {
            let db_file = NamedTempFile::new().expect("temp db");
            let db_path = db_file.path().to_str().expect("utf8").to_string();
            let ops = vec![atomic_op(
                "merge",
                serde_json::json!({
                    "into_id": uuid::Uuid::new_v4().to_string(),
                    "from_id": uuid::Uuid::new_v4().to_string(),
                }),
            )];
            let err = crate::atomic_apply::execute_atomic_ops_file(
                ops,
                atomic_cfg(&db_path),
                &khive_cfg,
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .expect_err("merge must be rejected before any write");
            assert!(
                format!("{err:#}").contains("use the non-atomic merge verb instead"),
                "error: {err:#}"
            );
            let server = isolated_server(&db_path);
            let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
            assert_eq!(
                resp["results"][0]["result"].as_array().unwrap().len(),
                0,
                "no write must have landed for merge"
            );
        }
    }

    // ── ADR-099 B3 fix: `--atomic` deny_unknown_fields parity ────────────────
    //
    // Canonical `update`/`delete`/`link`/`gtd.transition`/`gtd.complete`
    // reject unknown/typo'd arg keys via `#[serde(deny_unknown_fields)]` on
    // their param structs. Pre-fix, `--atomic` silently dropped unrecognized
    // keys instead of rejecting the op — a typo like `conten` (for
    // `content`) would report `ok:true` while quietly discarding the
    // caller's intended change. These tests exercise the fix at the same
    // `execute_atomic_ops_file` seam as the acceptance tests above, and are
    // the end-to-end counterpart to the syntactic-only unit coverage in
    // `atomic_apply::validate_atomic_args_tests`.

    /// Sharp case called out explicitly: atomic `update(id=X,
    /// conten="hello")` (typo of `content`) must be rejected AND must not
    /// mutate the row — no `content` change, no `updated_at` bump. Pre-fix,
    /// this silently discarded `conten`, reset every other field to its
    /// current value, bumped `updated_at`, and reported `ok:true`.
    #[tokio::test]
    async fn atomic_update_entity_unknown_field_is_rejected_and_does_not_mutate_row() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (entity_id, updated_at_before) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"create(kind="concept", name="TypoGuardX", description="original")"#,
            )
            .await;
            let id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string();
            let get_resp = dispatch_json(&server, &format!(r#"get(id="{id}")"#)).await;
            let updated_at = get_resp["results"][0]["result"]["updated_at"].clone();
            (id, updated_at)
        };

        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": entity_id, "conten": "hello"}),
        )];
        let khive_cfg = KhiveConfig::default();
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `conten` must be rejected, not silently dropped");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );

        let server = isolated_server(&db_path);
        let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
        assert_eq!(
            get_resp["results"][0]["result"]["description"], "original",
            "a rejected op must not have mutated description: {get_resp}"
        );
        assert_eq!(
            get_resp["results"][0]["result"]["updated_at"], updated_at_before,
            "a rejected op must not bump updated_at (no write happened): {get_resp}"
        );
    }

    /// update-note variant of the same parity fix: a typo'd key on a note
    /// update must be rejected, and a well-formed note update still
    /// succeeds (parity boundary — don't over-reject).
    #[tokio::test]
    async fn atomic_update_note_unknown_field_rejected_well_formed_succeeds() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let note_id = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"create(kind="observation", content="original note")"#,
            )
            .await;
            resp["results"][0]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };

        // (a) unknown field rejected.
        let khive_cfg = KhiveConfig::default();
        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": note_id, "conten": "typo'd"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `conten` on a note update must be rejected");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );

        // (b) well-formed update still succeeds.
        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": note_id, "content": "updated note"}),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("a well-formed note update must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let server = isolated_server(&db_path);
        let get_resp = dispatch_json(&server, &format!(r#"get(id="{note_id}")"#)).await;
        assert_eq!(
            get_resp["results"][0]["result"]["content"], "updated note",
            "the well-formed update must have landed: {get_resp}"
        );
    }

    /// `delete`: a typo'd key (`hardd` for `hard`) must be rejected before
    /// any write; a well-formed delete still succeeds.
    #[tokio::test]
    async fn atomic_delete_unknown_field_rejected_well_formed_succeeds() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let entity_id = {
            let server = isolated_server(&db_path);
            let resp =
                dispatch_json(&server, r#"create(kind="concept", name="DeleteTypoGuard")"#).await;
            resp["results"][0]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };

        // (a) unknown field rejected — entity must survive.
        let khive_cfg = KhiveConfig::default();
        let ops = vec![atomic_op(
            "delete",
            serde_json::json!({"id": entity_id, "hardd": true}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `hardd` must be rejected");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
        assert!(
            get_resp["results"][0]["result"]["deleted_at"].is_null(),
            "a rejected delete must not have deleted the entity: {get_resp}"
        );

        // (b) well-formed delete still succeeds.
        let ops = vec![atomic_op("delete", serde_json::json!({"id": entity_id}))];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("a well-formed delete must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );
    }

    /// `link`: a typo'd key (`relatoin` for `relation`) must be rejected
    /// before any write; a well-formed link still succeeds. (Distinct from
    /// the Leo-accepted `target_backend` conflict-arm deferral — out of
    /// scope here.)
    #[tokio::test]
    async fn atomic_link_unknown_field_rejected_well_formed_succeeds() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (a_id, b_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="LinkTypoA"), create(kind="concept", name="LinkTypoB")]"#,
            )
            .await;
            let a_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("a id")
                .to_string();
            let b_id = resp["results"][1]["result"]["id"]
                .as_str()
                .expect("b id")
                .to_string();
            (a_id, b_id)
        };

        // (a) unknown field rejected.
        let khive_cfg = KhiveConfig::default();
        let ops = vec![atomic_op(
            "link",
            serde_json::json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "extends",
                "relatoin": "extends",
            }),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `relatoin` must be rejected");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );

        // (b) well-formed link still succeeds.
        let ops = vec![atomic_op(
            "link",
            serde_json::json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("a well-formed link must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );
    }

    // ── ADR-099 B3: delete kind parity, update
    // null/type validation, canonical id resolution, per-op result payloads ──

    /// Atomic `delete(id=<entity>, kind="note")` must be
    /// REJECTED (no row deleted) — pre-fix, atomic ignored `kind` entirely
    /// and deleted the entity anyway (a destructive wrong-substrate action).
    /// `delete(id=<entity>, kind="entity")` and `kind` omitted must both
    /// still succeed.
    #[tokio::test]
    async fn atomic_delete_rejects_kind_mismatch_and_accepts_matching_or_omitted_kind() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let (mismatch_id, matching_id, omitted_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="KindMismatch"), create(kind="concept", name="KindMatching"), create(kind="concept", name="KindOmitted")]"#,
            )
            .await;
            let id = |i: usize| {
                resp["results"][i]["result"]["id"]
                    .as_str()
                    .expect("id")
                    .to_string()
            };
            (id(0), id(1), id(2))
        };

        // (a) kind mismatch: entity, caller says "note" — must be rejected,
        // entity must still be present afterward.
        let ops = vec![atomic_op(
            "delete",
            serde_json::json!({"id": mismatch_id, "kind": "note"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("delete(kind=\"note\") on an entity must be rejected");
        assert!(
            format!("{err:#}").contains("not found"),
            "expected a NotFound-shaped rejection, error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, &format!(r#"get(id="{mismatch_id}")"#)).await;
        assert!(
            resp["results"][0]["result"]["deleted_at"].is_null(),
            "entity must NOT be deleted after a kind-mismatch rejection: {resp}"
        );

        // (b) matching kind: succeeds.
        let ops = vec![atomic_op(
            "delete",
            serde_json::json!({"id": matching_id, "kind": "entity"}),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("delete(kind=\"entity\") on an entity must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        // (c) omitted kind: succeeds.
        let ops = vec![atomic_op("delete", serde_json::json!({"id": omitted_id}))];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("delete with kind omitted must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );
    }

    /// Atomic `update` null/type semantics must match canonical's actually-reachable
    /// behavior. See `crates/kkernel/docs/design.md#execrs-regression-test-notes`.
    #[tokio::test]
    async fn atomic_update_null_and_type_semantics_match_canonical_no_op_behavior() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let entity_id = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"create(kind="concept", name="NullSemantics", description="orig-desc", properties={"k": "v"}, tags=["a", "b"])"#,
            )
            .await;
            resp["results"][0]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };

        // (a) name: a non-null, non-string value must be REJECTED — the
        // actual violation (pre-fix: silently treated as
        // absent, reporting success).
        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": entity_id, "name": 123}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("name: 123 (non-null, non-string) must be rejected");
        assert!(
            format!("{err:#}").contains("name must be a string"),
            "error: {err:#}"
        );

        // (b) name=null, description=null, properties=null, tags=null in one
        // update: all four are canonical no-ops — the update must succeed
        // and every field must be UNCHANGED afterward.
        let ops = vec![atomic_op(
            "update",
            serde_json::json!({
                "id": entity_id,
                "name": null,
                "description": null,
                "properties": null,
                "tags": null,
            }),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("an all-null update must be a no-op success, not a rejection");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
        let row = &resp["results"][0]["result"];
        assert_eq!(
            row["name"], "NullSemantics",
            "name must be unchanged: {row}"
        );
        assert_eq!(
            row["description"], "orig-desc",
            "description must be unchanged: {row}"
        );
        assert_eq!(
            row["properties"]["k"], "v",
            "properties must be unchanged: {row}"
        );
        assert_eq!(
            row["tags"],
            serde_json::json!(["a", "b"]),
            "tags must be unchanged: {row}"
        );
    }

    /// An atomic ops-file using an 8-hex-prefix id for
    /// `update` AND `delete` must succeed identically to canonical
    /// (which accepts full UUID or an 8+ hex prefix); a non-existent prefix
    /// must error with canonical's error shape ("no record matches
    /// prefix"). Pre-fix, atomic did a bare `Uuid::parse_str` and rejected
    /// any short id outright — the same ops-file that succeeds non-atomically
    /// (e.g. against `create`'s own short `id` output) would fail before
    /// prepare under `--atomic`.
    #[tokio::test]
    async fn atomic_update_and_delete_accept_8_hex_prefix_ids() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let (entity_full_id, doomed_full_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="PrefixEntity"), create(kind="concept", name="PrefixDoomed")]"#,
            )
            .await;
            let entity_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("entity id")
                .to_string();
            let doomed_id = resp["results"][1]["result"]["id"]
                .as_str()
                .expect("doomed id")
                .to_string();
            (entity_id, doomed_id)
        };
        let entity_prefix = &entity_full_id[..8];
        let doomed_prefix = &doomed_full_id[..8];

        // (a) 8-hex-prefix update and delete in the SAME atomic unit
        // both succeed.
        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": entity_prefix, "name": "PrefixEntity-renamed"}),
            ),
            atomic_op("delete", serde_json::json!({"id": doomed_prefix})),
        ];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("8-hex-prefix ids must resolve identically to canonical");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, &format!(r#"get(id="{entity_full_id}")"#)).await;
        assert_eq!(
            resp["results"][0]["result"]["name"], "PrefixEntity-renamed",
            "prefix-addressed update must have landed: {resp}"
        );

        // (b) a non-existent 8-hex prefix errors with canonical's error
        // shape.
        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": "deadbeef", "name": "should not resolve"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("a non-existent prefix must be rejected");
        assert!(
            format!("{err:#}").contains("no record matches prefix"),
            "error: {err:#}"
        );
    }

    /// A committed atomic unit's success output must
    /// carry a canonical-shaped `result` per op (ADR-099 D4), not just
    /// `{ok, tool, op_index}`. Exercises three v1-admissible verbs in one
    /// unit and asserts the relevant field for each:
    /// updated name for `update`, the deleted marker for `delete`, and edge
    /// fields for `link`.
    #[tokio::test]
    async fn atomic_success_results_carry_canonical_shaped_result_per_op() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let (entity_id, doomed_id, source_id, target_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="ResultUpdate"), create(kind="concept", name="ResultDelete"), create(kind="concept", name="ResultLinkSource"), create(kind="concept", name="ResultLinkTarget")]"#,
            )
            .await;
            let id = |i: usize| {
                resp["results"][i]["result"]["id"]
                    .as_str()
                    .expect("id")
                    .to_string()
            };
            (id(0), id(1), id(2), id(3))
        };

        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": entity_id, "name": "ResultUpdate-renamed"}),
            ),
            atomic_op("delete", serde_json::json!({"id": doomed_id})),
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": source_id,
                    "target_id": target_id,
                    "relation": "extends",
                }),
            ),
        ];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("three v1-admissible verbs must commit as one unit");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let results = envelope["results"].as_array().expect("results array");
        assert_eq!(results.len(), 3, "envelope: {envelope}");

        assert_eq!(
            results[0]["result"]["name"], "ResultUpdate-renamed",
            "update result must carry the updated name: {envelope}"
        );

        assert_eq!(
            results[1]["result"]["deleted"], true,
            "delete result: {envelope}"
        );
        assert_eq!(
            results[1]["result"]["id"], doomed_id,
            "delete result must echo the caller's id: {envelope}"
        );

        assert_eq!(
            results[2]["result"]["relation"], "extends",
            "link result must carry the edge's relation: {envelope}"
        );
        assert_eq!(
            results[2]["result"]["source_id"], source_id,
            "link result must carry source_id: {envelope}"
        );
        assert_eq!(
            results[2]["result"]["target_id"], target_id,
            "link result must carry target_id: {envelope}"
        );
    }
}
